//! Integration test: Execute real worker with isolate pool

use openworkers_core::{
    HttpMethod, HttpRequest, OperationsHandle, RequestBody, RuntimeLimits, Script, Task, TaskType,
    WorkerCode,
};
use openworkers_runtime_v8::{get_pool_stats, init_pool};

#[tokio::test]
async fn test_pool_initialization() {
    // Initialize pool
    init_pool(10, RuntimeLimits::default());

    // Get stats
    let stats = get_pool_stats().await;

    assert_eq!(stats.total, 10);
    assert_eq!(stats.cached, 0); // No workers cached yet

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
