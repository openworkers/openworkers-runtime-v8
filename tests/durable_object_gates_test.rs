//! The gates of a Durable Object. Input gate: while it reads its storage,
//! no other event reaches it, neither a new request nor a timer or a response
//! of an earlier event. Output gate: what leaves it waits for the writes made
//! before it, and only for those.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectStorageOp, ObjectStorageResult,
    OpFuture, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{ObjectEvent, ObjectSpec};
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// A read takes this long
const READ_DELAY: Duration = Duration::from_millis(100);

/// A write takes this long
const WRITE_DELAY: Duration = Duration::from_millis(50);

#[derive(Default)]
struct Memory {
    entries: Mutex<BTreeMap<String, Vec<u8>>>,
    /// When each write was committed
    commits: Mutex<Vec<Instant>>,
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
                    tokio::time::sleep(WRITE_DELAY).await;
                    self.entries.lock().unwrap().extend(puts);
                    self.commits.lock().unwrap().push(Instant::now());

                    ObjectStorageResult::Written { deleted: 0 }
                }
                _ => ObjectStorageResult::Error("not in these tests".to_string()),
            }
        })
    }
}

fn object(class: &str) -> mpsc::UnboundedSender<ObjectEvent> {
    object_with(class, Arc::new(Memory::default()))
}

fn object_with(class: &str, memory: Arc<Memory>) -> mpsc::UnboundedSender<ObjectEvent> {
    let code = openworkers_transform::parse_worker_code(class.as_bytes(), CodeLanguage::JavaScript)
        .unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let spec = ObjectSpec {
        script: Script::new(code),
        class_name: "Gated".to_string(),
        id: "object".to_string(),
        limits: RuntimeLimits::default(),
        ops: memory,
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
                case '/flood':
                    for (let i = 0; i < 40; i++) {
                        storage.put(`k${i}`, i);
                        await new Promise((resolve) => setTimeout(resolve, 10));
                    }
                    return new Response('flood');
                case '/quick':
                    return new Response('quick');
                case '/stream': {
                    const { readable, writable } = new TransformStream();
                    const writer = writable.getWriter();
                    writer.write(new TextEncoder().encode('a'));
                    (async () => {
                        await new Promise((resolve) => setTimeout(resolve, 20));
                        storage.put('streamed', true);
                        await writer.write(new TextEncoder().encode('b'));
                        await writer.close();
                    })();
                    return new Response(readable);
                }
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
async fn a_response_waits_only_for_the_writes_before_it() {
    run_in_local(|| async {
        let object = object(GATED);

        // 40 writes, one every 10 ms, each 50 ms long: about 2 seconds of them
        let flood = send(&object, "/flood");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = Instant::now();
        assert_eq!(body(send(&object, "/quick")).await, "quick");
        let waited = started.elapsed();

        body(flood).await;
        assert!(
            waited < Duration::from_secs(1),
            "the response waited {waited:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_streamed_chunk_waits_for_the_writes_before_it() {
    run_in_local(|| async {
        let memory = Arc::new(Memory::default());
        let object = object_with(GATED, memory.clone());

        let response = timeout(Duration::from_secs(5), send(&object, "/stream"))
            .await
            .unwrap()
            .unwrap();
        let ResponseBody::Stream(mut chunks) = response.body else {
            panic!("a streamed body");
        };

        let mut arrivals = Vec::new();

        while let Some(chunk) = timeout(Duration::from_secs(5), chunks.recv())
            .await
            .unwrap()
        {
            arrivals.push((chunk.unwrap(), Instant::now()));
        }

        // The write may still be under way when a chunk leaves too early
        tokio::time::sleep(WRITE_DELAY * 2).await;
        let committed = memory.commits.lock().unwrap()[0];
        let (last, arrived) = arrivals.last().unwrap();
        assert_eq!(last.as_ref(), b"b");
        assert!(
            *arrived >= committed,
            "the chunk left before the write it follows"
        );
    })
    .await;
}
