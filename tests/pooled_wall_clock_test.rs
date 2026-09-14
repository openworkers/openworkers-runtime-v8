//! The wall clock must cut a pooled guest parked on a promise nothing settles.
//!
//! The watchdog thread only flags the limit; the loop has to wake up to read it.

mod common;

use common::run_in_local;
use openworkers_core::DefaultOps;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::RequestBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_core::TerminationReason;
use openworkers_runtime_v8::PinnedExecuteRequest;
use openworkers_runtime_v8::PinnedPoolConfig;
use openworkers_runtime_v8::execute_pinned;
use openworkers_runtime_v8::init_pinned_pool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

#[tokio::test(flavor = "current_thread")]
async fn a_parked_pooled_guest_is_cut_at_the_wall_clock() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits {
                max_cpu_time_ms: 0,
                max_wall_clock_time_ms: 500,
                ..Default::default()
            },
        });

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };
        let (task, _rx) = Event::fetch(request);
        let started = Instant::now();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            execute_pinned(PinnedExecuteRequest {
                owner_id: "wall-clock".to_string(),
                worker_id: "wall-clock-worker".to_string(),
                version: 1,
                script: Script::new(
                    r#"
                    addEventListener('fetch', async (event) => {
                        await new Promise(() => {});
                        event.respondWith(new Response('never'));
                    });
                    "#,
                ),
                ops: Arc::new(DefaultOps),
                task,
                on_warm_hit: None,
                env_updated_at: None,
                abort: None,
            }),
        )
        .await
        .expect("the wall clock, not this test's timeout, must end the wait");

        assert_eq!(result, Err(TerminationReason::WallClockTimeout));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cut at the 500 ms limit, not later: {:?}",
            started.elapsed()
        );
    })
    .await;
}
