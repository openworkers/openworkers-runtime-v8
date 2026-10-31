//! The CPU limit cuts a turn that never yields on macOS, through the watchdog
//! thread of src/security/cpu_enforcer.rs.

#![cfg(target_os = "macos")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, OperationsHandle, RequestBody, Script,
    TerminationReason,
};
use openworkers_runtime_v8::Worker;

/// /spin never yields; /wait yields 40 times on a timer; / answers at once.
const CODE: &str = "addEventListener('fetch', e => e.respondWith((async () => {
    const path = new URL(e.request.url).pathname;

    if (path === '/spin') {
        while (true) {}
    }

    if (path === '/wait') {
        let sum = 0;

        for (let i = 0; i < 40; i++) {
            await new Promise((resolve) => setTimeout(resolve, 1));

            for (let j = 0; j < 10000; j++) sum += j;
        }

        return new Response(String(sum > 0));
    }

    return new Response('ok');
})()));";

fn get(
    path: &str,
) -> (
    Event,
    tokio::sync::oneshot::Receiver<openworkers_core::HttpResponse>,
) {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://localhost{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
}

/// Runs the paths one after the other on one worker; each answer is the body,
/// or the reason the request ended, with the time it took.
async fn run(paths: &[&str]) -> Vec<(Result<String, TerminationReason>, Duration)> {
    let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();

    run_in_local(|| async move {
        let ops: OperationsHandle = Arc::new(DefaultOps);
        let script = Script::new(CODE);
        let mut worker = Worker::new_with_ops(script, None, ops).await.unwrap();
        let mut answers = Vec::new();

        for path in paths {
            let (task, rx) = get(&path);
            let start = Instant::now();
            let answer = match worker.exec(task).await {
                Ok(()) => {
                    let body = rx.await.unwrap().body.collect().await.unwrap().unwrap();

                    Ok(String::from_utf8_lossy(&body).into_owned())
                }
                Err(reason) => Err(reason),
            };

            answers.push((answer, start.elapsed()));
        }

        answers
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_loop_that_never_yields_is_cut() {
    let answers = run(&["/spin"]).await;
    let (answer, took) = &answers[0];

    eprintln!("a loop that never yields was cut after {took:?}");
    assert!(
        matches!(answer, Err(TerminationReason::CpuTimeLimit)),
        "{answer:?}"
    );
    assert!(*took < Duration::from_millis(100), "{took:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_that_yields_is_not_cut_early() {
    let answers = run(&["/wait"]).await;

    assert_eq!(answers[0].0.as_deref().unwrap(), "true");
}

// The next request on the same worker runs normally after a cut
#[tokio::test(flavor = "current_thread")]
async fn the_worker_serves_the_next_request_after_a_cut() {
    let answers = run(&["/spin", "/"]).await;

    assert!(matches!(answers[0].0, Err(TerminationReason::CpuTimeLimit)));
    assert_eq!(answers[1].0.as_deref().unwrap(), "ok");
}

// Two threads: only the one that spins is cut
#[test]
fn only_the_thread_that_spins_is_cut() {
    let on_thread = |path: &'static str| {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run(&[path]))
        })
    };

    let spinning = on_thread("/spin");
    let waiting = on_thread("/wait");
    let spun = spinning.join().unwrap();
    let waited = waiting.join().unwrap();

    assert!(matches!(spun[0].0, Err(TerminationReason::CpuTimeLimit)));
    assert_eq!(waited[0].0.as_deref().unwrap(), "true");
}
