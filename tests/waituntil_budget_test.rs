//! waitUntil work runs on what is left of the request's budget, not on a
//! budget of its own: a request cannot take twice its wall clock limit.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::DefaultOps;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
use std::sync::Arc;

/// Answers at 600 ms; the background work would end at 1600 ms, past the
/// 1000 ms the whole request has.
const CODE: &str = r#"
    addEventListener('fetch', (event) => {
        event.waitUntil(new Promise((resolve) => setTimeout(resolve, 1600)));
        event.respondWith(new Promise((resolve) =>
            setTimeout(() => resolve(new Response('answered')), 600)));
    });
"#;

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 1000,
        ..RuntimeLimits::default()
    }
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
async fn wait_until_shares_the_request_wall_clock() {
    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(CODE), Some(limits()))
            .await
            .unwrap();
        let (event, rx) = get();

        let started = Instant::now();
        let result = worker.exec(event).await;
        let elapsed = started.elapsed();

        // The answer went out before the limit
        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();
        assert_eq!(String::from_utf8_lossy(&body), "answered");

        assert!(
            matches!(result, Err(TerminationReason::WallClockTimeout)),
            "{result:?} after {elapsed:?}"
        );
        assert!(elapsed < Duration::from_millis(1400), "took {elapsed:?}");
    })
    .await;
}

/// The pool does the same; it reports the end of the background work to
/// nobody, so the time it takes is what shows.
#[tokio::test(flavor = "current_thread")]
async fn the_pool_cuts_wait_until_at_the_request_wall_clock() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: limits(),
    });

    run_in_local(|| async {
        let (event, rx) = get();
        let started = Instant::now();

        execute_pinned(PinnedExecuteRequest {
            owner_id: "owner".to_string(),
            worker_id: "budget".to_string(),
            version: 1,
            script: Script::new(CODE),
            ops: Arc::new(DefaultOps),
            task: event,
            on_warm_hit: None,
            env_updated_at: None,
            abort: None,
        })
        .await
        .unwrap();
        let elapsed = started.elapsed();

        assert_eq!(rx.await.unwrap().status, 200);
        assert!(elapsed < Duration::from_millis(1400), "took {elapsed:?}");
    })
    .await;
}
