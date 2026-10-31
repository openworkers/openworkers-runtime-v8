//! reset() before each event clears the timers and the async context of the
//! guest, with what it took before guest code ran: a guest cannot break it.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, OperationsHandle, RequestBody, Script,
    TerminationReason,
};
use openworkers_runtime_v8::Worker;

/// /break tries to replace the map of timer callbacks; each event sets a timer.
const CODE: &str = "addEventListener('fetch', e => e.respondWith((async () => {
    if (new URL(e.request.url).pathname === '/break') {
        globalThis.__timerCallbacks = null;
    }

    await new Promise((resolve) => setTimeout(resolve, 1));

    return new Response('ok');
})()));";

async fn run(paths: &[&str]) -> Vec<Result<String, TerminationReason>> {
    run_code(CODE, paths).await
}

async fn run_code(code: &str, paths: &[&str]) -> Vec<Result<String, TerminationReason>> {
    let code = code.to_string();
    let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();

    run_in_local(|| async move {
        let ops: OperationsHandle = Arc::new(DefaultOps);
        let mut worker = Worker::new_with_ops(Script::new(code), None, ops)
            .await
            .unwrap();
        let mut answers = Vec::new();

        for path in paths {
            let (task, rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://localhost{path}"),
                headers: HashMap::new(),
                body: RequestBody::None,
            });

            answers.push(match worker.exec(task).await {
                Ok(()) => {
                    let body = rx.await.unwrap().body.collect().await.unwrap().unwrap();

                    Ok(String::from_utf8_lossy(&body).into_owned())
                }
                Err(reason) => Err(reason),
            });
        }

        answers
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn requests_reset_between_them() {
    let answers = run(&["/", "/", "/"]).await;

    assert!(
        answers.iter().all(|answer| answer.as_deref() == Ok("ok")),
        "{answers:?}"
    );
}

// The map globals are read-only: the assignment does nothing, and the next
// events keep their timers
#[tokio::test(flavor = "current_thread")]
async fn a_guest_cannot_replace_the_timer_maps() {
    let answers = run(&["/break", "/", "/"]).await;

    assert!(
        answers.iter().all(|answer| answer.as_deref() == Ok("ok")),
        "{answers:?}"
    );
}

// A guest that replaces the clear method of the map, with a loop: the reset
// calls Map.prototype.clear as it was before guest code, so the next events
// run, and fast
#[tokio::test(flavor = "current_thread")]
async fn a_guest_that_replaces_clear_does_not_hold_the_reset() {
    let code = "globalThis.__timerCallbacks.clear = function () { for (;;) {} };
        globalThis.__intervalIds.clear = function () { for (;;) {} };
        Map.prototype.clear = function () { for (;;) {} };
        addEventListener('fetch', e => e.respondWith(new Response('ok')));";
    let start = std::time::Instant::now();
    let answers = run_code(code, &["/", "/", "/"]).await;

    assert!(
        answers.iter().all(|answer| answer.as_deref() == Ok("ok")),
        "{answers:?}"
    );
    assert!(
        start.elapsed() < std::time::Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
}
