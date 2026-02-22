//! Integration test: Execute worker with pinned pool

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, get_pinned_pool_stats, init_pinned_pool,
};
use std::sync::Arc;

#[tokio::test(flavor = "current_thread")]
async fn test_pinned_pool_initialization() {
    run_in_local(|| async {
        // Initialize pinned pool
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        // Get stats (counters are global, so other parallel tests may have incremented them)
        let stats = get_pinned_pool_stats();

        // Just verify stats are accessible and hit_rate is sane
        assert!(stats.hit_rate >= 0.0 && stats.hit_rate <= 1.0);

        println!("Pinned pool initialized: {:?}", stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_pinned_pool_stats_after_use() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let ops: OperationsHandle = Arc::new(DefaultOps);

        // Execute a worker to generate some stats
        let code = r#"
            addEventListener('scheduled', event => {
                console.log('Scheduled event handled!');
            });
        "#;

        let script = Script::new(code);
        let (task, rx) = Event::from_schedule("test-stats".to_string(), 1000);

        execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner".to_string(),
            worker_id: "test-worker".to_string(),
            version: 1,
            script,
            ops: ops.clone(),
            task,
            on_warm_hit: None,
        })
        .await
        .unwrap();
        rx.await.unwrap();

        let stats = get_pinned_pool_stats();

        assert!(stats.total_requests > 0);
        println!("Pinned pool stats after use: {:?}", stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_execute_pinned_simple() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        // Create a simple worker script that responds to scheduled events
        let code = r#"
            addEventListener('scheduled', event => {
                console.log('Scheduled event handled!');
            });
        "#;

        let script = Script::new(code);

        // Create a scheduled task
        let (task, _rx) = Event::from_schedule("test-pinned".to_string(), 1000);

        // Create operations handle (DefaultOps for testing)
        let ops: OperationsHandle = Arc::new(DefaultOps);

        // Execute the worker
        let result = execute_pinned(PinnedExecuteRequest {
            owner_id: "test-owner-1".to_string(),
            worker_id: "test-worker-1".to_string(),
            version: 1,
            script,
            ops,
            task,
            on_warm_hit: None,
        })
        .await;

        // Should succeed
        assert!(
            result.is_ok(),
            "Pinned execution should succeed: {:?}",
            result
        );

        // Check stats reflect usage
        let stats = get_pinned_pool_stats();

        assert!(stats.total_requests > 0);

        println!("Pinned execution test passed!");
    })
    .await;
}
