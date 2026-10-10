mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, get_local_pool_stats, init_pinned_pool,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// A caller that drops the future of a request gives its slot back
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_request_releases_its_slot() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 1,
            max_per_owner: None,
            max_concurrent_per_isolate: 1,
            max_cached_contexts: 1,
            overcommit: false,
            max_context_reuses: 1000,
            limits: RuntimeLimits {
                max_wall_clock_time_ms: 60_000,
                ..Default::default()
            },
        });

        // Never answers
        let code =
            "addEventListener('fetch', (event) => { event.respondWith(new Promise(() => {})); });";

        let request = || {
            let req = HttpRequest {
                method: HttpMethod::Get,
                url: "http://localhost/".to_string(),
                headers: HashMap::new(),
                body: RequestBody::None,
            };
            let (task, _rx) = Event::fetch(req);

            PinnedExecuteRequest {
                owner_id: "owner".to_string(),
                worker_id: "worker".to_string(),
                version: 1,
                script: Script::new(code),
                ops: Arc::new(openworkers_core::DefaultOps),
                task,
                on_warm_hit: None,
                env_updated_at: None,
                abort: None,
                on_marks: None,
            }
        };

        let outcome =
            tokio::time::timeout(Duration::from_millis(300), execute_pinned(request())).await;
        assert!(outcome.is_err(), "the request was meant to hang");

        assert_eq!(get_local_pool_stats().unwrap().in_use, 0);

        // The only isolate is free again, so the next request is not refused
        let outcome =
            tokio::time::timeout(Duration::from_millis(300), execute_pinned(request())).await;
        assert!(
            outcome.is_err(),
            "it ran, and hung, instead of being refused"
        );
    })
    .await;
}
