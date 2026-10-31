//! The top level of a script is guest code: it runs under the CPU limit, and
//! a cut fails the creation of the worker.

mod common;

use std::sync::Arc;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{DefaultOps, OperationsHandle, Script, TerminationReason};
use openworkers_runtime_v8::Worker;

async fn make(code: &'static str) -> Result<(), TerminationReason> {
    run_in_local(|| async {
        let ops: OperationsHandle = Arc::new(DefaultOps);

        Worker::new_with_ops(Script::new(code), None, ops)
            .await
            .map(|_| ())
    })
    .await
}

// 500 ms of CPU at top level, past the load floor of 400 ms: the end of the
// turn counts it on every platform
#[tokio::test(flavor = "current_thread")]
async fn a_top_level_past_the_cpu_limit_makes_no_worker() {
    let made = make(
        "const end = Date.now() + 500; while (Date.now() < end) {}
        addEventListener('fetch', e => e.respondWith(new Response('ok')));",
    )
    .await;

    assert!(
        matches!(made, Err(TerminationReason::CpuTimeLimit)),
        "{made:?}"
    );
}

// A top level that never ends: the CPU timer (Linux) or the watchdog (macOS)
// cuts it while it runs
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "current_thread")]
async fn a_top_level_that_never_ends_is_cut() {
    let start = std::time::Instant::now();
    let made = make("while (true) {}").await;

    assert!(
        matches!(made, Err(TerminationReason::CpuTimeLimit)),
        "{made:?}"
    );
    assert!(
        start.elapsed() < Duration::from_millis(600),
        "{:?}",
        start.elapsed()
    );
}

// 100 ms of CPU at top level: past the request limit of 50 ms, under the load
// floor
#[tokio::test(flavor = "current_thread")]
async fn a_top_level_under_the_load_floor_makes_a_worker() {
    let made = make(
        "const end = Date.now() + 100; while (Date.now() < end) {}
        addEventListener('fetch', e => e.respondWith(new Response('ok')));",
    )
    .await;

    assert!(made.is_ok(), "{made:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_top_level_that_ends_makes_a_worker() {
    let made = make(
        "let sum = 0; for (let i = 0; i < 1000; i++) sum += i;
        addEventListener('fetch', e => e.respondWith(new Response(String(sum))));",
    )
    .await;

    assert!(made.is_ok(), "{made:?}");
}
