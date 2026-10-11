//! A redeployed worker does not keep its previous deployments in the isolate:
//! each one holds its whole bundle, and a few redeploys would fill the heap.

mod common;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, OperationsHandler, RequestBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, get_local_pool_stats, init_pinned_pool,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct NoOps;

impl OperationsHandler for NoOps {}

fn init_pool() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 1000,
        limits: RuntimeLimits {
            heap_initial_mb: 1,
            heap_max_mb: 16,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 5_000,
            ..Default::default()
        },
    });
}

/// A deployment whose bundle keeps about 4 MB alive, like a large app.
fn script(version: i32) -> Script {
    Script::new(format!(
        r#"
        const ballast = new Array(1_000_000).fill({version});
        addEventListener('fetch', (event) => {{
            event.respondWith(new Response('v{version}:' + ballast.length));
        }});
        "#
    ))
}

/// The response body, and whether the request reused a cached context.
/// None when the request was stopped.
async fn serve(version: i32, env_updated_at: Option<i64>) -> Option<(String, bool)> {
    let (task, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    let warm = Arc::new(AtomicBool::new(false));
    let warm_flag = warm.clone();

    let result = execute_pinned(PinnedExecuteRequest {
        owner_id: "tenant".to_string(),
        worker_id: "worker".to_string(),
        version,
        script: script(version),
        ops: Arc::new(NoOps),
        task,
        on_warm_hit: Some(Box::new(move |_| warm_flag.store(true, Ordering::SeqCst))),
        env_updated_at,
        abort: None,
        on_report: None,
    })
    .await;

    if result.is_err() {
        return None;
    }

    let response = rx.await.ok()?;
    let body = response.body.collect().await.ok()??;

    Some((
        String::from_utf8_lossy(&body).into_owned(),
        warm.load(Ordering::SeqCst),
    ))
}

#[tokio::test(flavor = "current_thread")]
async fn redeploys_do_not_fill_the_heap_with_old_versions() {
    run_in_local(|| async {
        init_pool();

        for version in 1..=12 {
            let served = serve(version, None).await;
            let expected = format!("v{version}:1000000");

            assert_eq!(
                served.map(|(body, _)| body),
                Some(expected),
                "version {version}"
            );
        }

        assert_eq!(get_local_pool_stats().unwrap().cached_contexts, 1);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_late_request_on_an_older_version_keeps_the_newer_context() {
    run_in_local(|| async {
        init_pool();

        serve(2, None).await.unwrap();
        assert!(serve(2, None).await.unwrap().1, "v2 is warm");

        // Still on the previous deployment during a rollout
        serve(1, None).await.unwrap();

        assert!(serve(2, None).await.unwrap().1, "v2 is still warm");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_newer_environment_replaces_the_context_built_with_the_older_one() {
    run_in_local(|| async {
        init_pool();

        serve(1, Some(100)).await.unwrap();
        serve(1, Some(200)).await.unwrap();

        assert_eq!(get_local_pool_stats().unwrap().cached_contexts, 1);
        assert!(
            serve(1, Some(200)).await.unwrap().1,
            "the new environment is warm"
        );
    })
    .await;
}
