//! A WebSocketPair hands its server end to the socket of an upgraded request.
//!
//! The tests play the runner: they read the 101, then send the socket on the
//! upgrade channel, or drop it.

mod common;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::FetchInit;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::HttpResponse;
use openworkers_core::RequestBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_core::TerminationReason;
use openworkers_core::WebSocketConnection;
use openworkers_core::WebSocketIncoming;
use openworkers_core::WebSocketOutgoing;
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const ECHO: &str = r#"
    addEventListener('fetch', (event) => {
        const [client, server] = Object.values(new WebSocketPair());

        server.accept();
        server.addEventListener('message', (e) => server.send(e.data));

        event.respondWith(new Response(null, { status: 101, webSocket: client }));
    });
"#;

/// The peer side of a socket, as the runner holds it.
struct Peer {
    to_worker: mpsc::UnboundedSender<WebSocketIncoming>,
    from_worker: mpsc::UnboundedReceiver<WebSocketOutgoing>,
}

impl Peer {
    fn connect(socket_tx: oneshot::Sender<WebSocketConnection>) -> Self {
        let (send_tx, from_worker) = mpsc::unbounded_channel();
        let (to_worker, recv_rx) = mpsc::unbounded_channel();

        socket_tx
            .send(WebSocketConnection { send_tx, recv_rx })
            .expect("the worker holds the upgrade");

        Self {
            to_worker,
            from_worker,
        }
    }

    fn send(&self, text: &str) {
        self.to_worker
            .send(WebSocketIncoming::Text(text.into()))
            .unwrap();
    }

    async fn recv(&mut self) -> WebSocketOutgoing {
        timeout(Duration::from_secs(5), self.from_worker.recv())
            .await
            .expect("the worker answers")
            .expect("the worker keeps the socket")
    }
}

fn request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/socket".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

fn limits(wall_clock_ms: u64) -> RuntimeLimits {
    RuntimeLimits {
        max_wall_clock_time_ms: wall_clock_ms,
        ..Default::default()
    }
}

/// Runs one event on its own task, as the pool does, so the test can talk to
/// the socket while the event lasts.
type Running = JoinHandle<(Worker, Result<(), TerminationReason>)>;

async fn start(code: &str, limits: RuntimeLimits, init: FetchInit) -> Running {
    let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();

    tokio::task::spawn_local(async move {
        let outcome = worker.exec(Event::Fetch(Some(init))).await;
        (worker, outcome)
    })
}

/// The worker comes back alive, as the pool keeps a context after its event.
async fn finish(event: Running) -> (Worker, Result<(), TerminationReason>) {
    timeout(Duration::from_secs(5), event)
        .await
        .expect("the event ends")
        .unwrap()
}

fn text(message: WebSocketOutgoing) -> String {
    match message {
        WebSocketOutgoing::Text(text) => text,
        other => panic!("not a text frame: {other:?}"),
    }
}

async fn upgraded(res_rx: oneshot::Receiver<HttpResponse>) -> HttpResponse {
    let response = timeout(Duration::from_secs(5), res_rx)
        .await
        .expect("the worker answers")
        .expect("the worker sends a response");
    assert_eq!(response.status, 101);

    response
}

#[tokio::test(flavor = "current_thread")]
async fn the_server_end_echoes_through_the_upgrade() {
    run_in_local(|| async {
        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(ECHO, limits(5_000), init).await;

        upgraded(res_rx).await;
        let mut peer = Peer::connect(socket_tx);

        peer.send("one");
        assert_eq!(text(peer.recv().await), "one");
        peer.send("two");
        assert_eq!(text(peer.recv().await), "two");

        peer.to_worker
            .send(WebSocketIncoming::Closed {
                code: 1000,
                reason: "bye".into(),
            })
            .unwrap();

        finish(event).await.1.unwrap();
    })
    .await;
}

/// The wall clock bounds a run of JS, not the life of the socket.
#[tokio::test(flavor = "current_thread")]
async fn an_idle_socket_outlives_the_wall_clock() {
    run_in_local(|| async {
        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(ECHO, limits(200), init).await;

        upgraded(res_rx).await;
        let mut peer = Peer::connect(socket_tx);

        tokio::time::sleep(Duration::from_millis(600)).await;
        peer.send("still here");
        assert_eq!(text(peer.recv().await), "still here");

        drop(peer.to_worker);
        finish(event).await.1.unwrap();
    })
    .await;
}

/// A turn that runs when the deadline of the event passes is not cut: the
/// socket stopped that deadline at the 101.
#[tokio::test(flavor = "current_thread")]
async fn the_deadline_of_the_event_does_not_cut_a_turn_of_the_socket() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [client, server] = Object.values(new WebSocketPair());

                server.accept();
                server.addEventListener('message', () => {
                    const end = Date.now() + 800;
                    while (Date.now() < end) {}
                    server.send('done');
                });

                event.respondWith(new Response(null, { status: 101, webSocket: client }));
            });
        "#;

        let limits = RuntimeLimits {
            max_cpu_time_ms: 0,
            ..limits(1_000)
        };

        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits, init).await;

        upgraded(res_rx).await;
        let mut peer = Peer::connect(socket_tx);

        // The turn runs from about 500 ms to 1300 ms after the event starts
        tokio::time::sleep(Duration::from_millis(500)).await;
        peer.send("slow");
        assert_eq!(text(peer.recv().await), "done");

        drop(peer.to_worker);
        finish(event).await.1.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_turn_on_the_socket_that_never_yields_is_cut() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [client, server] = Object.values(new WebSocketPair());

                server.accept();
                server.addEventListener('message', () => { while (true) {} });

                event.respondWith(new Response(null, { status: 101, webSocket: client }));
            });
        "#;

        // No CPU limit, so the wall clock is what cuts the loop
        let limits = RuntimeLimits {
            max_cpu_time_ms: 0,
            ..limits(200)
        };

        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits, init).await;

        upgraded(res_rx).await;
        let peer = Peer::connect(socket_tx);
        peer.send("spin");

        let (_worker, outcome) = finish(event).await;
        assert!(
            matches!(outcome, Err(TerminationReason::WallClockTimeout)),
            "{outcome:?}"
        );
    })
    .await;
}

/// What the guest sends before the client arrives goes out first.
#[tokio::test(flavor = "current_thread")]
async fn a_send_before_the_upgrade_is_kept() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [client, server] = Object.values(new WebSocketPair());

                server.accept();
                server.send('hello');

                event.respondWith(new Response(null, { status: 101, webSocket: client }));
            });
        "#;

        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits(5_000), init).await;

        upgraded(res_rx).await;
        let mut peer = Peer::connect(socket_tx);
        assert_eq!(text(peer.recv().await), "hello");

        drop(peer.to_worker);
        finish(event).await.1.unwrap();
    })
    .await;
}

/// A runner that does not upgrade drops the sender, and the guest sees its
/// socket close instead of waiting on it forever.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_upgrade_closes_the_server_end() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [client, server] = Object.values(new WebSocketPair());

                server.accept();
                event.waitUntil(new Promise((resolve) => {
                    server.addEventListener('close', (e) => {
                        console.log('closed ' + e.code);
                        resolve();
                    });
                }));

                event.respondWith(new Response(null, { status: 101, webSocket: client }));
            });
        "#;

        let (res_tx, res_rx) = oneshot::channel();
        let (init, socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits(30_000), init).await;

        upgraded(res_rx).await;
        drop(socket_tx);

        finish(event).await.1.unwrap();
    })
    .await;
}

/// A pair the response does not carry closes when the response is read.
#[tokio::test(flavor = "current_thread")]
async fn a_pair_left_out_of_the_response_closes() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [, server] = Object.values(new WebSocketPair());

                server.accept();
                event.waitUntil(new Promise((resolve) => {
                    server.addEventListener('close', resolve);
                }));

                event.respondWith(new Response('no socket'));
            });
        "#;

        let (res_tx, res_rx) = oneshot::channel();
        let (init, _socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits(30_000), init).await;

        assert_eq!(res_rx.await.unwrap().status, 200);
        finish(event).await.1.unwrap();
    })
    .await;
}

/// A socket the guest never accepts has no reader, so it closes with the event.
#[tokio::test(flavor = "current_thread")]
async fn a_server_end_never_accepted_closes_with_the_event() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const [client] = Object.values(new WebSocketPair());

                event.respondWith(new Response(null, { status: 101, webSocket: client }));
            });
        "#;

        let (res_tx, res_rx) = oneshot::channel();
        let (init, mut socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits(5_000), init).await;

        upgraded(res_rx).await;
        let (_worker, outcome) = finish(event).await;
        outcome.unwrap();

        timeout(Duration::from_secs(5), socket_tx.closed())
            .await
            .expect("the worker lets the socket go");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_socket_for_a_request_that_is_not_an_upgrade_is_refused() {
    run_in_local(|| async {
        let (res_tx, res_rx) = oneshot::channel();
        let event = start(ECHO, limits(5_000), FetchInit::new(request(), res_tx)).await;

        match finish(event).await.1 {
            Err(TerminationReason::Exception(message)) => {
                assert!(message.contains("not a WebSocket upgrade"), "{message}");
            }
            other => panic!("the response went out: {other:?}"),
        }

        assert!(res_rx.await.is_err());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_101_without_a_pair_is_refused() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                event.respondWith((async () => {
                    const upstream = WebSocket.__adopt(99);
                    return new Response(null, { status: 101, webSocket: upstream });
                })());
            });
        "#;

        let (res_tx, res_rx) = oneshot::channel();
        let (init, _socket_tx) = FetchInit::upgradable(request(), res_tx);
        let event = start(code, limits(5_000), init).await;

        match finish(event).await.1 {
            Err(TerminationReason::Exception(message)) => {
                assert!(message.contains("WebSocketPair"), "{message}");
            }
            other => panic!("the response went out: {other:?}"),
        }

        assert!(res_rx.await.is_err());
    })
    .await;
}
