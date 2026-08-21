//! What a guest's own `ReadableStream` looks like from the host side.
//!
//! These read the response body while `exec` is still running, the way a real
//! host does. A test that awaits `exec` first can only see bodies that fit in
//! the buffer, which is exactly where the interesting cases start.

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::RequestBody;
use openworkers_core::ResponseBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_core::TerminationReason;
use openworkers_runtime_v8::Worker;

/// An `exec` future, pinned so the body can be read while it runs.
type Exec<'a> = Pin<&'a mut (dyn Future<Output = Result<(), TerminationReason>> + 'a)>;

/// `exec`'s verdict, once it has one.
type ExecResult = Option<Result<(), TerminationReason>>;

/// Runs `fut` while `exec` keeps making progress. A guest only produces while
/// its event loop runs, so a host that waits for the body first deadlocks.
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

/// Paced guests need more than the 50ms production CPU budget.
fn limits() -> RuntimeLimits {
    RuntimeLimits {
        max_cpu_time_ms: 10_000,
        max_wall_clock_time_ms: 20_000,
        ..Default::default()
    }
}

/// A guest that closes its stream on a buffer that is exactly full still has to
/// deliver the end of the body. The response hop holds 16 chunks, so a producer
/// that stops on a multiple of it ends with nowhere to put the closing marker.
#[tokio::test(flavor = "current_thread")]
#[ntest::timeout(30000)] // a stream that never ends would otherwise hang the suite
async fn test_response_stream_closes_on_a_full_buffer() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const encoder = new TextEncoder();
                let sent = 0;

                const stream = new ReadableStream({
                    pull(controller) {
                        if (sent >= 32) {
                            controller.close();

                            return;
                        }

                        controller.enqueue(encoder.encode('chunk' + sent + '\n'));
                        sent += 1;
                    }
                });

                event.respondWith(new Response(stream));
            });
        "#;

        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();

        let (task, rx) = Event::fetch(get("/"));
        let future = worker.exec(task);
        tokio::pin!(future);

        let mut exec: Exec<'_> = future;
        let mut done: ExecResult = None;

        let response = while_running(&mut exec, &mut done, rx).await.unwrap();
        let ResponseBody::Stream(mut body) = response.body else {
            panic!("the guest stream did not reach the host as a stream");
        };

        let mut chunks = 0;

        while let Some(chunk) = while_running(&mut exec, &mut done, body.recv()).await {
            chunk.unwrap();
            chunks += 1;
        }

        assert_eq!(chunks, 32);
    })
    .await;
}

/// `controller.error()` mid-stream is a truncated body, not a complete one, so
/// the host has to receive an error and not a clean end of channel.
#[tokio::test(flavor = "current_thread")]
#[ntest::timeout(30000)] // a stream that never ends would otherwise hang the suite
async fn test_response_stream_error_reaches_the_host() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const encoder = new TextEncoder();
                let sent = 0;

                const stream = new ReadableStream({
                    pull(controller) {
                        if (sent >= 2) {
                            controller.error(new Error('guest gave up'));

                            return;
                        }

                        controller.enqueue(encoder.encode('chunk' + sent));
                        sent += 1;
                    }
                });

                event.respondWith(new Response(stream));
            });
        "#;

        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();

        let (task, rx) = Event::fetch(get("/"));
        let future = worker.exec(task);
        tokio::pin!(future);

        let mut exec: Exec<'_> = future;
        let mut done: ExecResult = None;

        let response = while_running(&mut exec, &mut done, rx).await.unwrap();
        let ResponseBody::Stream(mut body) = response.body else {
            panic!("the guest stream did not reach the host as a stream");
        };

        let mut received = Vec::new();
        let mut error = None;

        while let Some(chunk) = while_running(&mut exec, &mut done, body.recv()).await {
            match chunk {
                Ok(bytes) => received.extend_from_slice(&bytes),
                Err(message) => {
                    error = Some(message);

                    break;
                }
            }
        }

        assert_eq!(String::from_utf8_lossy(&received), "chunk0chunk1");
        assert!(
            error.is_some_and(|message| message.contains("guest gave up")),
            "the guest error has to reach the host, or a truncated body reads as complete"
        );
    })
    .await;
}
