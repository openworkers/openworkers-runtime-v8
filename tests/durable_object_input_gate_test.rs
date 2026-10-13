//! The input gate of a Durable Object: while it reads its storage, no other
//! event reaches it, neither a new request nor a timer or a response of an
//! earlier event.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectStorageOp, ObjectStorageResult,
    OpFuture, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{ObjectEvent, ObjectSpec};
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// A read takes this long; a write is at once.
const READ_DELAY: Duration = Duration::from_millis(100);

#[derive(Default)]
struct Memory {
    entries: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl OperationsHandler for Memory {
    fn handle_durable_object_storage(
        &self,
        op: ObjectStorageOp,
    ) -> OpFuture<'_, ObjectStorageResult> {
        Box::pin(async move {
            match op {
                ObjectStorageOp::Get { keys } => {
                    tokio::time::sleep(READ_DELAY).await;
                    let entries = self.entries.lock().unwrap();

                    ObjectStorageResult::Values(
                        keys.iter().map(|key| entries.get(key).cloned()).collect(),
                    )
                }
                ObjectStorageOp::Write { puts, .. } => {
                    self.entries.lock().unwrap().extend(puts);

                    ObjectStorageResult::Written { deleted: 0 }
                }
                _ => ObjectStorageResult::Error("not in these tests".to_string()),
            }
        })
    }
}

fn object(class: &str) -> mpsc::UnboundedSender<ObjectEvent> {
    let code = openworkers_transform::parse_worker_code(class.as_bytes(), CodeLanguage::JavaScript)
        .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let spec = ObjectSpec {
        script: Script::new(code),
        class_name: "Gated".to_string(),
        id: "object".to_string(),
        limits: RuntimeLimits::default(),
        ops: Arc::new(Memory::default()),
        idle: Duration::from_secs(60),
        drain: Duration::from_secs(30),
    };

    tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

    tx
}

/// Sends a request; answers the receiver of its response.
fn send(
    object: &mpsc::UnboundedSender<ObjectEvent>,
    path: &str,
) -> oneshot::Receiver<HttpResponse> {
    let (res_tx, res_rx) = oneshot::channel();
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://object{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    object
        .send(ObjectEvent::Fetch(FetchInit::new(request, res_tx)))
        .unwrap();

    res_rx
}

async fn body(response: oneshot::Receiver<HttpResponse>) -> String {
    let response = timeout(Duration::from_secs(5), response)
        .await
        .expect("the object answers in time")
        .expect("the object answers");

    match response.body {
        ResponseBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
        _ => panic!("a plain body"),
    }
}

const GATED: &str = r#"
    export class Gated {
        constructor(state) {
            this.state = state;
            this.log = [];
        }

        async fetch(request) {
            const storage = this.state.storage;

            switch (new URL(request.url).pathname) {
                case '/count': {
                    const count = ((await storage.get('count')) ?? 0) + 1;
                    storage.put('count', count);
                    return new Response(String(count));
                }
                case '/timer':
                    await new Promise((resolve) => setTimeout(resolve, 20));
                    this.log.push('timer');
                    return new Response('timer');
                case '/read':
                    await storage.get('key');
                    this.log.push('read');
                    return new Response('read');
                case '/concurrent-read':
                    await storage.get('key', { allowConcurrency: true });
                    this.log.push('read');
                    return new Response('read');
                case '/log':
                    return new Response(this.log.join(','));
            }
        }
    }
"#;

#[tokio::test(flavor = "current_thread")]
async fn a_read_holds_back_the_next_requests() {
    run_in_local(|| async {
        let object = object(GATED);

        let answers: Vec<_> = (0..3).map(|_| send(&object, "/count")).collect();
        let mut counts = Vec::new();

        for answer in answers {
            counts.push(body(answer).await);
        }

        assert_eq!(counts, ["1", "2", "3"]);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_holds_back_the_timers_of_other_events() {
    run_in_local(|| async {
        let object = object(GATED);

        let timer = send(&object, "/timer");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let read = send(&object, "/read");

        body(timer).await;
        body(read).await;

        assert_eq!(body(send(&object, "/log")).await, "read,timer");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_read_that_allows_concurrency_holds_nothing_back() {
    run_in_local(|| async {
        let object = object(GATED);

        let timer = send(&object, "/timer");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let read = send(&object, "/concurrent-read");

        body(timer).await;
        body(read).await;

        assert_eq!(body(send(&object, "/log")).await, "timer,read");
    })
    .await;
}
