//! A Durable Object serves its events in one context that keeps its state,
//! lets them interleave at their awaits, and holds them back while a
//! blockConcurrencyWhile callback runs.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, RequestBody, ResponseBody, RuntimeLimits,
    Script,
};
use openworkers_runtime_v8::ObjectSpec;
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

const COUNTER: &str = r#"
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

    export class Counter {
        constructor(state, env) {
            this.state = state;
            this.count = 0;
        }

        async fetch(request) {
            const url = new URL(request.url);

            switch (url.pathname) {
                case '/id':
                    return new Response(this.state.id.toString());
                case '/slow':
                    await sleep(200);
                    return new Response('slow ' + this.count);
                case '/spin':
                    while (true) {}
                case '/tick':
                    setInterval(() => {}, 10);
                    return new Response('ticking');
                case '/forever':
                    await new Promise(() => {});
                default:
                    this.count += 1;
                    return new Response(String(this.count));
            }
        }
    }
"#;

/// The classic script the runner makes of a module.
fn module(code: &str) -> String {
    openworkers_transform::parse_worker_code(code.as_bytes(), CodeLanguage::JavaScript).unwrap()
}

/// A short wall clock, so a turn that never yields stops soon.
const WALL_CLOCK_MS: u64 = 300;

fn spec(code: &str, idle: Duration) -> ObjectSpec {
    ObjectSpec {
        script: Script::new(module(code)),
        class_name: "Counter".to_string(),
        id: "0123abcd".to_string(),
        limits: RuntimeLimits {
            max_wall_clock_time_ms: WALL_CLOCK_MS,
            ..Default::default()
        },
        ops: Arc::new(openworkers_core::DefaultOps),
        idle,
    }
}

/// Starts the object on this thread and answers its mailbox.
fn start(spec: ObjectSpec) -> mpsc::UnboundedSender<FetchInit> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

    tx
}

fn send(object: &mpsc::UnboundedSender<FetchInit>, path: &str) -> oneshot::Receiver<HttpResponse> {
    let (res_tx, res_rx) = oneshot::channel();
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://object{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    object.send(FetchInit::new(request, res_tx)).unwrap();

    res_rx
}

async fn body(response: HttpResponse) -> String {
    match response.body {
        ResponseBody::None => String::new(),
        ResponseBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
        ResponseBody::Stream(mut rx) => {
            let mut text = Vec::new();

            while let Some(chunk) = rx.recv().await {
                text.extend_from_slice(&chunk.unwrap());
            }

            String::from_utf8(text).unwrap()
        }
    }
}

async fn text(res_rx: oneshot::Receiver<HttpResponse>) -> String {
    let response = timeout(Duration::from_secs(5), res_rx)
        .await
        .expect("the object answers in time")
        .expect("the object answers");

    body(response).await
}

/// Whether the event failed soon after a turn that never yields: a wall
/// clock that does not cut the turn blocks the thread, which no timeout of
/// the test can see.
async fn cut(res_rx: oneshot::Receiver<HttpResponse>) -> bool {
    let started = std::time::Instant::now();
    let failed = failed(res_rx).await;

    assert!(started.elapsed() < Duration::from_millis(WALL_CLOCK_MS * 5));

    failed
}

/// Whether the event failed: the object dropped its response.
async fn failed(res_rx: oneshot::Receiver<HttpResponse>) -> bool {
    timeout(Duration::from_secs(5), res_rx)
        .await
        .expect("the event ends in time")
        .is_err()
}

#[tokio::test(flavor = "current_thread")]
async fn the_object_keeps_its_state_across_events() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_secs(60)));

        assert_eq!(text(send(&object, "/")).await, "1");
        assert_eq!(text(send(&object, "/")).await, "2");
        assert_eq!(text(send(&object, "/id")).await, "0123abcd");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn events_interleave_at_their_awaits() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_secs(60)));

        let slow = send(&object, "/slow");
        let fast = send(&object, "/");

        // The fast event runs while the slow one waits, and the slow one
        // sees its change
        assert_eq!(text(fast).await, "1");
        assert_eq!(text(slow).await, "slow 1");
    })
    .await;
}

const GATED: &str = r#"
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

    export class Counter {
        constructor(state) {
            this.state = state;
            this.log = [];

            state.blockConcurrencyWhile(async () => {
                await sleep(100);
                this.log.push('built');
            });
        }

        async fetch(request) {
            if (new URL(request.url).pathname === '/block') {
                await this.state.blockConcurrencyWhile(async () => {
                    await sleep(100);
                    this.log.push('blocked');
                });
            }

            this.log.push(new URL(request.url).pathname);
            return new Response(this.log.join(' '));
        }
    }
"#;

#[tokio::test(flavor = "current_thread")]
async fn block_concurrency_while_holds_back_the_next_events() {
    run_in_local(|| async {
        let object = start(spec(GATED, Duration::from_secs(60)));

        // The first event waits for the constructor's callback
        let first = send(&object, "/block");
        let second = send(&object, "/next");

        assert_eq!(text(first).await, "built blocked /block");
        assert_eq!(text(second).await, "built blocked /block /next");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_constructor_runs_again_after_the_idle_time() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_millis(100)));

        assert_eq!(text(send(&object, "/")).await, "1");
        assert_eq!(text(send(&object, "/")).await, "2");

        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(text(send(&object, "/")).await, "1");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_event_in_flight_keeps_the_object() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_millis(50)));

        assert_eq!(text(send(&object, "/")).await, "1");

        // Longer than the idle time, but the object has an event in flight
        assert_eq!(text(send(&object, "/slow")).await, "slow 1");
        assert_eq!(text(send(&object, "/")).await, "2");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_timer_does_not_keep_an_idle_object() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_millis(100)));

        assert_eq!(text(send(&object, "/")).await, "1");
        assert_eq!(text(send(&object, "/tick")).await, "ticking");

        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(text(send(&object, "/")).await, "1");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_event_whose_caller_left_does_not_keep_the_object() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_millis(100)));

        assert_eq!(text(send(&object, "/")).await, "1");
        drop(send(&object, "/forever"));

        tokio::time::sleep(Duration::from_millis(400)).await;

        assert_eq!(text(send(&object, "/")).await, "1");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_turn_that_never_yields_stops_only_its_object() {
    run_in_local(|| async {
        let spinning = start(spec(COUNTER, Duration::from_secs(60)));
        let other = start(spec(COUNTER, Duration::from_secs(60)));

        assert_eq!(text(send(&spinning, "/")).await, "1");
        assert_eq!(text(send(&other, "/")).await, "1");

        assert!(cut(send(&spinning, "/spin")).await);

        // The next event builds the object again; the other one kept its state
        assert_eq!(text(send(&spinning, "/")).await, "1");
        assert_eq!(text(send(&other, "/")).await, "2");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_stopped_object_fails_its_events_in_flight() {
    run_in_local(|| async {
        let object = start(spec(COUNTER, Duration::from_secs(60)));

        let slow = send(&object, "/slow");
        let spin = send(&object, "/spin");

        assert!(cut(spin).await);
        assert!(failed(slow).await);
    })
    .await;
}

const BROKEN: &str = r#"
    globalThis.built = 0;

    export class Counter {
        constructor(state) {
            globalThis.built += 1;
            state.blockConcurrencyWhile(async () => {
                throw new Error('no storage');
            });
        }

        fetch() {
            return new Response('served');
        }
    }
"#;

#[tokio::test(flavor = "current_thread")]
async fn a_failed_block_concurrency_while_stops_the_object() {
    run_in_local(|| async {
        let object = start(spec(BROKEN, Duration::from_secs(60)));

        assert!(failed(send(&object, "/")).await);
        assert!(failed(send(&object, "/")).await);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_object_of_a_class_the_worker_lacks_fails_its_events() {
    run_in_local(|| async {
        let mut spec = spec(COUNTER, Duration::from_secs(60));
        spec.class_name = "Room".to_string();
        let object = start(spec);

        assert!(failed(send(&object, "/")).await);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_constructor_that_throws_fails_the_event() {
    run_in_local(|| async {
        let code = "export class Counter { constructor() { throw new Error('nope'); } }";
        let object = start(spec(code, Duration::from_secs(60)));

        assert!(failed(send(&object, "/")).await);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_constructor_that_never_returns_is_cut() {
    run_in_local(|| async {
        let code = r#"
            export class Counter {
                constructor() { while (true) {} }
                fetch() { return new Response('served'); }
            }
        "#;
        let object = start(spec(code, Duration::from_secs(60)));

        assert!(cut(send(&object, "/")).await);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_object_ends_when_its_mailbox_closes() {
    run_in_local(|| async {
        let (tx, rx) = mpsc::unbounded_channel();
        let run = tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(
            spec(COUNTER, Duration::from_secs(60)),
            rx,
        ));

        let answer = send(&tx, "/slow");
        drop(tx);

        // The event in flight still gets its answer
        assert_eq!(text(answer).await, "slow 0");
        timeout(Duration::from_secs(5), run)
            .await
            .expect("the object ends")
            .unwrap();
    })
    .await;
}
