//! The WebSockets of a Durable Object: the runner holds them, so the object
//! sends its frames and its accepts as ops, gets the frames of the client as
//! events, and a new instance finds the hibernatable sockets again.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectSocket, ObjectSocketOp,
    ObjectStorageOp, ObjectStorageResult, OpFuture, OperationsHandler, RequestBody, ResponseBody,
    RuntimeLimits, Script, WebSocketConnection, WebSocketIncoming, WebSocketOutgoing,
    WebSocketUpgrade,
};
use openworkers_runtime_v8::{ObjectEvent, ObjectSpec, SocketEvent};
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const WRITE_DELAY: Duration = Duration::from_millis(200);

/// The runner side: what the object asked of its sockets and storage, in
/// order, with the socket ids as `#1`, `#2`... in order of appearance.
#[derive(Default)]
struct Host {
    log: Mutex<Vec<String>>,
    ids: Mutex<Vec<u64>>,
    upgrades: Mutex<Vec<WebSocketUpgrade>>,
    attachments: Mutex<Vec<Vec<u8>>>,
    /// What a new instance finds
    sockets: Mutex<Vec<ObjectSocket>>,
}

impl Host {
    fn name(&self, id: u64) -> String {
        let mut ids = self.ids.lock().unwrap();

        let index = match ids.iter().position(|known| *known == id) {
            Some(index) => index,
            None => {
                ids.push(id);
                ids.len() - 1
            }
        };

        format!("#{}", index + 1)
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn id(&self, name: usize) -> u64 {
        self.ids.lock().unwrap()[name - 1]
    }
}

impl OperationsHandler for Host {
    fn handle_durable_object_storage(
        &self,
        op: ObjectStorageOp,
    ) -> OpFuture<'_, ObjectStorageResult> {
        Box::pin(async move {
            match op {
                ObjectStorageOp::Write { .. } => {
                    tokio::time::sleep(WRITE_DELAY).await;
                    self.log.lock().unwrap().push("write".to_string());

                    ObjectStorageResult::Written { deleted: 0 }
                }
                _ => ObjectStorageResult::Error("not in these tests".to_string()),
            }
        })
    }

    fn handle_durable_object_socket(&self, op: ObjectSocketOp) {
        let line = match op {
            ObjectSocketOp::Accept {
                socket,
                tags,
                hibernatable,
            } => format!("accept {} {tags:?} {hibernatable}", self.name(socket)),
            ObjectSocketOp::Attach { socket, upgrade } => {
                self.upgrades.lock().unwrap().push(upgrade);
                format!("attach {}", self.name(socket))
            }
            ObjectSocketOp::Send { socket, message } => {
                let message = match message {
                    WebSocketOutgoing::Text(text) => format!("text {text}"),
                    WebSocketOutgoing::Binary(bytes) => format!("binary {bytes:?}"),
                    WebSocketOutgoing::Close { code, reason } => format!("close {code} {reason}"),
                };

                format!("send {} {message}", self.name(socket))
            }
            ObjectSocketOp::Attachment { socket, value } => {
                let line = format!("attachment {} {}", self.name(socket), value.is_some());

                if let Some(value) = value {
                    self.attachments.lock().unwrap().push(value);
                }

                line
            }
        };

        self.log.lock().unwrap().push(line);
    }

    fn durable_object_sockets(&self) -> Vec<ObjectSocket> {
        self.sockets.lock().unwrap().clone()
    }

    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            self.log.lock().unwrap().push("fetch".to_string());

            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: ResponseBody::None,
            })
        })
    }
}

fn spec(host: Arc<Host>, class: &str, idle: Duration) -> ObjectSpec {
    let code = openworkers_transform::parse_worker_code(class.as_bytes(), CodeLanguage::JavaScript)
        .unwrap();

    ObjectSpec {
        script: Script::new(code),
        class_name: "Room".to_string(),
        id: "object".to_string(),
        limits: RuntimeLimits::default(),
        ops: host,
        idle,
        drain: Duration::from_secs(30),
    }
}

fn object(host: Arc<Host>, class: &str) -> mpsc::UnboundedSender<ObjectEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let spec = spec(host, class, Duration::from_secs(60));

    tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

    tx
}

fn request(path: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://object{path}"),
        headers: HashMap::from([("upgrade".to_string(), "websocket".to_string())]),
        body: RequestBody::None,
    }
}

async fn answer(res_rx: oneshot::Receiver<HttpResponse>) -> (u16, String) {
    let response = timeout(Duration::from_secs(5), res_rx)
        .await
        .expect("the object answers in time")
        .expect("the object answers");

    let body = match response.body {
        ResponseBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
        _ => String::new(),
    };

    (response.status, body)
}

/// Sends an upgradable request; answers the status, the body, and the
/// sender of the socket of the client.
async fn upgrade(
    object: &mpsc::UnboundedSender<ObjectEvent>,
    path: &str,
) -> (u16, String, oneshot::Sender<WebSocketConnection>) {
    let (res_tx, res_rx) = oneshot::channel();
    let (init, socket_tx) = FetchInit::upgradable(request(path), res_tx);

    object.send(ObjectEvent::Fetch(init)).unwrap();

    let (status, body) = answer(res_rx).await;

    (status, body, socket_tx)
}

async fn fetch(object: &mpsc::UnboundedSender<ObjectEvent>, path: &str) -> (u16, String) {
    let (res_tx, res_rx) = oneshot::channel();

    object
        .send(ObjectEvent::Fetch(FetchInit::new(request(path), res_tx)))
        .unwrap();

    answer(res_rx).await
}

fn event(object: &mpsc::UnboundedSender<ObjectEvent>, socket: u64, incoming: WebSocketIncoming) {
    object
        .send(ObjectEvent::Socket(SocketEvent {
            socket,
            incoming,
            permit: None,
        }))
        .unwrap();
}

/// Waits until the log has `count` lines.
async fn settled(host: &Host, count: usize) -> Vec<String> {
    for _ in 0..100 {
        if host.log().len() >= count {
            break;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    host.log()
}

const ROOM: &str = r#"
    export class Room {
        constructor(state) {
            this.state = state;
            this.found = state.getWebSockets('a').length;
        }

        async fetch(request) {
            const path = new URL(request.url).pathname;

            if (path === '/state') {
                return new Response(JSON.stringify({ found: this.found, closed: this.closed }));
            }

            const [client, server] = Object.values(new WebSocketPair());

            if (path === '/block') {
                this.state.blockConcurrencyWhile(() => new Promise((ok) => setTimeout(ok, 300)));
                return new Response('blocked');
            }

            if (path === '/early') {
                return new Response(String(await Promise.resolve().then(() => server.send('x')).catch((e) => e.name)));
            }

            this.state.acceptWebSocket(server, ['a', 'b', 'a']);
            server.serializeAttachment({ name: 'ada' });
            server.send('hello');

            return new Response(null, { status: 101, webSocket: client });
        }

        webSocketMessage(ws, message) {
            const tags = this.state.getTags(ws).join(',');
            const count = this.state.getWebSockets('b').length;
            ws.send(`${message} ${tags} ${count} ${JSON.stringify(ws.deserializeAttachment())}`);
        }

        webSocketClose(ws, code, reason, wasClean) {
            this.closed = [code, reason, wasClean, this.state.getWebSockets().length];
        }
    }
"#;

#[tokio::test(flavor = "current_thread")]
async fn a_hibernatable_socket_goes_to_the_runner() {
    run_in_local(|| async {
        let host = Arc::new(Host::default());
        let object = object(host.clone(), ROOM);

        let (status, _, socket_tx) = upgrade(&object, "/").await;
        assert_eq!(status, 101);

        assert_eq!(
            settled(&host, 4).await,
            [
                r#"accept #1 ["a", "b"] true"#,
                "attachment #1 true",
                "send #1 text hello",
                "attach #1",
            ]
        );

        // The upgrade the runner gets is the one of the request
        let (send_tx, _) = mpsc::unbounded_channel();
        let (_, recv_rx) = mpsc::unbounded_channel();
        socket_tx
            .send(WebSocketConnection { send_tx, recv_rx })
            .unwrap();

        let upgrade = host.upgrades.lock().unwrap().pop().unwrap();
        assert!(upgrade.await.is_ok());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_socket_is_accepted_before_it_sends() {
    run_in_local(|| async {
        let host = Arc::new(Host::default());
        let object = object(host.clone(), ROOM);

        assert_eq!(
            fetch(&object, "/early").await,
            (200, "InvalidStateError".to_string())
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_response_hands_over_a_socket_only_on_an_upgrade() {
    run_in_local(|| async {
        let host = Arc::new(Host::default());
        let object = object(host.clone(), ROOM);

        assert_eq!(
            fetch(&object, "/").await,
            (
                500,
                "The response carries a WebSocket, but the request is not a WebSocket upgrade"
                    .to_string()
            )
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_socket_event_runs_the_handler_of_the_class() {
    run_in_local(|| async {
        let host = Arc::new(Host::default());
        let object = object(host.clone(), ROOM);

        upgrade(&object, "/").await;
        settled(&host, 4).await;
        let socket = host.id(1);

        event(&object, socket, WebSocketIncoming::Text("hi".to_string()));

        assert_eq!(
            settled(&host, 5).await[4],
            r#"send #1 text hi a,b 1 {"name":"ada"}"#
        );

        event(
            &object,
            socket,
            WebSocketIncoming::Closed {
                code: 4000,
                reason: "bye".to_string(),
            },
        );

        assert_eq!(
            fetch(&object, "/state").await.1,
            r#"{"found":0,"closed":[4000,"bye",true,0]}"#
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_instance_finds_the_hibernatable_sockets() {
    run_in_local(|| async {
        // The first instance makes the attachment, as V8 serializes it
        let first = Arc::new(Host::default());
        let object = object(first.clone(), ROOM);
        upgrade(&object, "/").await;
        settled(&first, 4).await;
        let attachment = first.attachments.lock().unwrap().pop().unwrap();

        let host = Arc::new(Host::default());
        *host.sockets.lock().unwrap() = vec![ObjectSocket {
            id: 7,
            tags: vec!["a".to_string(), "b".to_string()],
            attachment: Some(attachment),
        }];
        let object = self::object(host.clone(), ROOM);

        event(&object, 7, WebSocketIncoming::Text("ping".to_string()));

        assert_eq!(
            settled(&host, 1).await,
            [r#"send #1 text ping a,b 1 {"name":"ada"}"#]
        );
        assert_eq!(host.id(1), 7);
        assert_eq!(fetch(&object, "/state").await.1, r#"{"found":1}"#);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_socket_with_listeners_keeps_the_object() {
    run_in_local(|| async {
        let class = r#"
            export class Room {
                async fetch() {
                    const [client, server] = Object.values(new WebSocketPair());
                    server.accept();
                    server.addEventListener('message', (event) => server.send(`got ${event.data}`));

                    return new Response(null, { status: 101, webSocket: client });
                }
            }
        "#;
        let host = Arc::new(Host::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let spec = spec(host.clone(), class, Duration::from_millis(100));
        let run = tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

        upgrade(&tx, "/").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!run.is_finished(), "the open socket keeps the object");

        let socket = host.id(1);
        event(&tx, socket, WebSocketIncoming::Text("x".to_string()));
        assert_eq!(
            settled(&host, 3).await,
            ["accept #1 [] false", "attach #1", "send #1 text got x"]
        );

        event(
            &tx,
            socket,
            WebSocketIncoming::Closed {
                code: 1000,
                reason: String::new(),
            },
        );
        drop(tx);

        timeout(Duration::from_secs(5), run)
            .await
            .expect("a closed socket lets the object go")
            .unwrap();
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn outbound_calls_and_frames_wait_for_the_writes_before_them() {
    run_in_local(|| async {
        let class = r#"
            export class Room {
                constructor(state) {
                    this.state = state;
                }

                async fetch() {
                    const [client, server] = Object.values(new WebSocketPair());
                    this.state.acceptWebSocket(server);
                    this.state.storage.put('k', 1);
                    server.send('after the write');
                    await fetch('http://example.com/');

                    return new Response(null, { status: 101, webSocket: client });
                }
            }
        "#;
        let host = Arc::new(Host::default());
        let object = object(host.clone(), class);

        upgrade(&object, "/").await;

        assert_eq!(
            settled(&host, 5).await,
            [
                "accept #1 [] true",
                "write",
                "send #1 text after the write",
                "fetch",
                "attach #1",
            ]
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_socket_event_keeps_its_permit_until_the_object_takes_it() {
    run_in_local(|| async {
        let host = Arc::new(Host::default());
        let object = object(host.clone(), ROOM);

        upgrade(&object, "/").await;
        settled(&host, 4).await;
        let socket = host.id(1);

        assert_eq!(fetch(&object, "/block").await.1, "blocked");

        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        object
            .send(ObjectEvent::Socket(SocketEvent {
                socket,
                incoming: WebSocketIncoming::Text("hi".to_string()),
                permit: Some(permits.clone().try_acquire_owned().unwrap()),
            }))
            .unwrap();

        // The object holds its events while blockConcurrencyWhile runs
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(permits.available_permits(), 0);

        settled(&host, 5).await;
        assert_eq!(permits.available_permits(), 1);
    })
    .await;
}
