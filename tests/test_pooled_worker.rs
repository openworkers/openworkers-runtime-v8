//! Integration test: Execute real worker with isolate pool

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{get_pool_stats, init_pool};
use std::sync::Arc;

#[tokio::test]
async fn test_pool_initialization() {
    // Initialize pool
    init_pool(10, RuntimeLimits::default());

    // Get stats
    let stats = get_pool_stats().await;

    assert_eq!(stats.total, 10);
    // Note: cached count may vary depending on test execution order (pool is global)

    println!("Pool initialized: {:?}", stats);
}

#[tokio::test]
async fn test_pool_stats_after_use() {
    init_pool(10, RuntimeLimits::default()); // Same as first test (pool is global)

    // TODO: Once execute_pooled is implemented, this will actually execute
    // For now just check stats
    let stats = get_pool_stats().await;

    assert_eq!(stats.capacity, 10);
    println!("Pool capacity: {}", stats.capacity);
}

#[tokio::test(flavor = "current_thread")]
async fn test_execute_pooled_simple() {
    run_in_local(|| async {
        init_pool(10, RuntimeLimits::default());

        // Create a simple worker script that responds to scheduled events
        let code = r#"
            addEventListener('scheduled', event => {
                console.log('Scheduled event handled!');
            });
        "#;

        let script = Script::new(code);

        // Create a scheduled task
        let (task, _rx) = Event::from_schedule("test-pooled".to_string(), 1000);

        // Create operations handle (DefaultOps for testing)
        let ops: OperationsHandle = Arc::new(DefaultOps);

        // Execute the worker
        let result =
            openworkers_runtime_v8::execute_pooled("test-worker-1", script, ops, task).await;

        // Should succeed
        assert!(
            result.is_ok(),
            "Pooled execution should succeed: {:?}",
            result
        );

        // Check that the worker is now cached
        let stats = get_pool_stats().await;
        assert_eq!(stats.cached, 1);

        println!("Pooled execution test passed!");
    })
    .await;
}
