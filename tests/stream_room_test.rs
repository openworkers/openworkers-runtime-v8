//! A response body that streams faster than its reader: the body pump waits
//! for room in the channel of the stream, and nothing hangs when the worker
//! goes or the request is cancelled.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, OperationsHandle, RequestBody, RuntimeLimits,
    Script, TerminationReason,
};
use openworkers_runtime_v8::{ExecutionContext, LockerManagedIsolate, Worker};

/// Streams `count` pieces, each the last digit of its number; with no count,
/// for ever.
fn code(count: Option<u32>) -> String {
    let limit = count.map_or("Infinity".to_string(), |count| count.to_string());

    format!(
        "addEventListener('fetch', e => {{
            const encoder = new TextEncoder();
            let n = 0;

            e.respondWith(new Response(new ReadableStream({{
                pull(controller) {{
                    if (n < {limit}) {{
                        controller.enqueue(encoder.encode(String(n++ % 10)));
                    }} else {{
                        controller.close();
                    }}
                }},
            }})));
        }});"
    )
}

fn get() -> (
    Event,
    tokio::sync::oneshot::Receiver<openworkers_core::HttpResponse>,
) {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_thousand_pieces_arrive_whole_and_in_order() {
    let body = run_in_local(|| async {
        let ops: OperationsHandle = Arc::new(DefaultOps);
        let mut worker = Worker::new_with_ops(Script::new(code(Some(1000))), None, ops)
            .await
            .unwrap();
        let (task, rx) = get();
        let (_, body) = tokio::join!(worker.exec(task), async {
            rx.await.unwrap().body.collect().await.unwrap().unwrap()
        });

        body
    })
    .await;
    let expected: String = (0..1000)
        .map(|n| char::from(b'0' + (n % 10) as u8))
        .collect();

    assert_eq!(String::from_utf8_lossy(&body), expected);
}

// The body is never read: the pump fills the channel and waits for room. The
// worker then drops; nothing hangs
#[tokio::test(flavor = "current_thread")]
async fn a_worker_that_drops_while_its_pump_waits_does_not_hang() {
    let done = tokio::time::timeout(
        Duration::from_secs(5),
        run_in_local(|| async {
            let ops: OperationsHandle = Arc::new(DefaultOps);
            let mut worker = Worker::new_with_ops(Script::new(code(None)), None, ops)
                .await
                .unwrap();
            let (task, rx) = get();
            let _ = tokio::time::timeout(Duration::from_millis(200), worker.exec(task)).await;

            drop(worker);
            drop(rx);
        }),
    )
    .await;

    assert!(done.is_ok(), "the drop hung");
}

// A cancel while the pump waits for room ends the request
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_while_the_pump_waits_ends_the_request() {
    use tokio_util::sync::CancellationToken;

    let ended = run_in_local(|| async {
        let ops: OperationsHandle = Arc::new(DefaultOps);
        let limits = RuntimeLimits::default();
        let isolate = Arc::new(LockerManagedIsolate::new(limits.clone()));
        let mut context = {
            let (mut locker, _js_lock) = isolate.lock();

            ExecutionContext::new_with_pooled_isolate(
                &mut locker,
                Arc::clone(&isolate),
                isolate.use_snapshot,
                isolate.platform,
                limits,
                Script::new(code(None)),
                ops,
            )
            .unwrap()
        };
        let cancel = CancellationToken::new();
        let (task, rx) = get();

        context.begin_request(Some(cancel.clone()));

        // The response comes, and its body is kept and never read
        let (ended, (_response, cancelled_at)) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), context.exec(task)),
            async {
                let response = rx.await;

                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel.cancel();

                (response, std::time::Instant::now())
            }
        );

        (ended, cancelled_at.elapsed())
    })
    .await;
    let (ended, after_cancel) = ended;
    let ended = ended.expect("the request did not end after the cancel");

    // It ends because of the cancel, not because the pump spun to the CPU limit
    assert!(
        !matches!(ended, Err(TerminationReason::CpuTimeLimit)),
        "{ended:?}"
    );
    assert!(after_cancel < Duration::from_millis(50), "{after_cancel:?}");
}
