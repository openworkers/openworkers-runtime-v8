//! Regressions of bugs found in an audit: each failed before its fix.
//! Numbered in the order they were found.

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, HttpResponse, OpFuture, OperationsHandler, RequestBody,
    ResponseBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::Worker;
use openworkers_runtime_v8::runtime::stream_manager::{StreamChunk, StreamManager};
use openworkers_runtime_v8::runtime::{CallbackMessage, SchedulerMessage, run_event_loop};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 1. An interval set by a request used to keep firing into the event loop
/// after the request ended (BeginRequest for the next one), so a cached
/// context collected ExecuteInterval messages forever.
#[tokio::test]
async fn interval_outlives_its_request() {
    let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    let notify = Arc::new(tokio::sync::Notify::new());
    let manager = Arc::new(StreamManager::new());
    let cancel = CancellationToken::new();
    let ops: Arc<dyn OperationsHandler> = Arc::new(openworkers_core::DefaultOps);

    let loop_cancel = cancel.clone();
    let _loop = tokio::spawn(async move {
        run_event_loop(scheduler_rx, callback_tx, notify, manager, ops, loop_cancel).await;
    });

    scheduler_tx
        .send(SchedulerMessage::ScheduleInterval(7, 5))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    // The request ends and the next one starts on the same context.
    scheduler_tx
        .send(SchedulerMessage::BeginRequest(None))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    while callback_rx.try_recv().is_ok() {}

    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut after = 0;
    while let Ok(msg) = callback_rx.try_recv() {
        if matches!(msg, CallbackMessage::ExecuteInterval(7)) {
            after += 1;
        }
    }

    assert_eq!(
        after, 0,
        "interval 7 fired {after} times after its request ended"
    );
}

/// Ops whose fetch answers a streaming body that never ends: the sender is
/// parked so the reader task stays alive across the request boundary.
struct HangingFetch {
    keep: std::sync::Mutex<Option<mpsc::Sender<Result<bytes::Bytes, String>>>>,
}

impl OperationsHandler for HangingFetch {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let (tx, rx) = mpsc::channel(4);
        *self.keep.lock().unwrap() = Some(tx);

        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Stream(rx),
            })
        })
    }
}

/// 2. Stream ids used to restart at 1 on `clear()`, and the reader of a
/// cancelled upstream body wrote `Done` by id after it was cancelled, so the
/// next request's stream 1 got a `Done` it never produced.
#[tokio::test]
async fn stale_reader_ends_the_next_requests_stream() {
    let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    let notify = Arc::new(tokio::sync::Notify::new());
    let manager = Arc::new(StreamManager::new());
    let cancel = CancellationToken::new();
    let ops: Arc<dyn OperationsHandler> = Arc::new(HangingFetch {
        keep: std::sync::Mutex::new(None),
    });

    let loop_cancel = cancel.clone();
    let loop_manager = manager.clone();
    let _loop = tokio::spawn(async move {
        run_event_loop(
            scheduler_rx,
            callback_tx,
            notify,
            loop_manager,
            ops,
            loop_cancel,
        )
        .await;
    });

    // Request A: a fetch whose upstream body never ends.
    scheduler_tx
        .send(SchedulerMessage::BeginRequest(None))
        .unwrap();
    scheduler_tx
        .send(SchedulerMessage::FetchStreaming(
            1,
            HttpRequest {
                method: HttpMethod::Get,
                url: "http://upstream/".into(),
                headers: HashMap::new(),
                body: RequestBody::None,
            },
        ))
        .unwrap();

    let old_id = loop {
        match callback_rx.recv().await {
            Some(CallbackMessage::FetchStreamingSuccess(1, _, id)) => break id,
            Some(_) => continue,
            None => panic!("event loop went away"),
        }
    };
    assert_eq!(old_id, 1);

    // Request A ends; the context is reset for request B.
    manager.clear();

    // Request B gets a stream of its own, under an id of its own.
    let new_id = manager.create_stream("request B body".into());
    assert_ne!(new_id, old_id, "ids must not repeat after clear()");

    scheduler_tx
        .send(SchedulerMessage::BeginRequest(None))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Request B's stream is empty and open: request A's reader wrote nowhere.
    let got = tokio::time::timeout(Duration::from_millis(200), manager.read_chunk(new_id)).await;

    match got {
        Err(_) => {}
        Ok(Ok(StreamChunk::Done)) => {
            panic!("request B's stream {new_id} received a Done from request A's reader")
        }
        Ok(other) => panic!("unexpected chunk on request B's stream: {other:?}"),
    }
    assert!(manager.has_sender(new_id), "request B's stream was closed");
}

type Exec<'a> = Pin<&'a mut (dyn Future<Output = Result<(), TerminationReason>> + 'a)>;
type ExecResult = Option<Result<(), TerminationReason>>;

async fn while_running<F>(exec: &mut Exec<'_>, done: &mut ExecResult, fut: F) -> F::Output
where
    F: Future,
{
    tokio::pin!(fut);

    loop {
        tokio::select! {
            biased;
            output = &mut fut => return output,
            result = exec.as_mut(), if done.is_none() => *done = Some(result),
        }
    }
}

fn get(path: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://localhost{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// 3. A client that hangs up on a stream whose source is waiting (no timer,
/// no I/O) used to leave `exec` parked until the wall clock, although the
/// disconnect was signalled: nothing woke the loop to run the microtasks it
/// queued or to notice the grace period had passed.
#[tokio::test(flavor = "current_thread")]
async fn hang_up_on_a_quiet_stream_is_noticed() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const encoder = new TextEncoder();
                const stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue(encoder.encode('one\n'));
                        controller.enqueue(encoder.encode('two\n'));
                        controller.enqueue(encoder.encode('three\n'));
                        // Waits for an event that never comes: no timer, no I/O.
                    }
                });
                event.respondWith(new Response(stream));
            });
        "#;

        let limits = RuntimeLimits {
            max_cpu_time_ms: 10_000,
            max_wall_clock_time_ms: 6_000,
            ..Default::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();

        let (task, rx) = Event::fetch(get("/"));
        let future = worker.exec(task);
        tokio::pin!(future);
        let mut exec: Exec<'_> = future;
        let mut done: ExecResult = None;

        let response = while_running(&mut exec, &mut done, rx).await.unwrap();
        let ResponseBody::Stream(mut body) = response.body else {
            panic!("not a stream");
        };

        let mut taken = 0;
        while taken < 3 {
            match while_running(&mut exec, &mut done, body.recv()).await {
                Some(Ok(_)) => taken += 1,
                other => panic!("stream ended early: {other:?}"),
            }
        }

        drop(body);
        let hung_up = Instant::now();

        let result = if let Some(result) = done {
            result
        } else {
            tokio::time::timeout(Duration::from_secs(10), exec.as_mut())
                .await
                .expect("exec never returned")
        };

        let took = hung_up.elapsed();
        assert!(
            took < Duration::from_secs(2),
            "exec took {took:?} to notice the client had left (result: {result:?})"
        );
    })
    .await;
}

/// 4. SharedArrayBuffer is deleted from the global, and shared WebAssembly
/// memory used to hand one out, constructor included.
#[tokio::test(flavor = "current_thread")]
async fn shared_array_buffer_is_not_reachable_through_wasm() {
    run_in_local(|| async {
        let mut worker = Worker::new(Script::new("globalThis.x = 0;"), None).await.unwrap();
        worker
            .evaluate(
                r#"
                const refused = (f) => { try { f(); return 0; } catch (e) { return e instanceof TypeError ? 1 : 0; } };

                globalThis.sabGone = typeof SharedArrayBuffer === 'undefined' ? 1 : 0;
                globalThis.atomicsGone = typeof Atomics === 'undefined' ? 1 : 0;
                globalThis.sharedMemoryRefused = refused(() => new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true }));
                globalThis.plainMemoryWorks = new WebAssembly.Memory({ initial: 1 }).buffer.byteLength === 65536 ? 1 : 0;

                // (memory 1 1 shared), and the same memory unshared
                const shared = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 5, 4, 1, 3, 1, 1]);
                const plain = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 5, 4, 1, 1, 1, 1]);
                globalThis.sharedModuleRefused = refused(() => new WebAssembly.Module(shared));
                globalThis.plainModuleWorks = new WebAssembly.Module(plain) instanceof WebAssembly.Module ? 1 : 0;
                globalThis.constructorIsSafe = WebAssembly.Memory.prototype.constructor === WebAssembly.Memory ? 1 : 0;

                globalThis.sharedCompileRefused = 0;
                WebAssembly.compile(shared).then(() => {}, (e) => { globalThis.sharedCompileRefused = e instanceof TypeError ? 1 : 0; });
                "#,
            )
            .unwrap();
        worker.process_callbacks();

        let flags = [
            "sabGone",
            "atomicsGone",
            "sharedMemoryRefused",
            "plainMemoryWorks",
            "sharedModuleRefused",
            "plainModuleWorks",
            "constructorIsSafe",
            "sharedCompileRefused",
        ];

        for flag in flags {
            assert_eq!(worker.get_global_u32(flag), Some(1), "{flag}");
        }
    })
    .await;
}
