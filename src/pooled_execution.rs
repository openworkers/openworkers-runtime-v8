//! Pooled execution - ExecutionContext with isolate pool integration
//!
//! This module provides a high-level API for executing worker scripts using
//! the isolate pool. It wraps ExecutionContext and manages the pool lifecycle.

use crate::execution_context::ExecutionContext;
use crate::isolate_pool::get_pool;
use openworkers_core::{Event, OperationsHandle, Script, TerminationReason};

/// Execute a worker script using the isolate pool
///
/// This is the recommended way to execute workers in production. It:
/// 1. Acquires an isolate from the pool (by worker_id)
/// 2. Locks the isolate with v8::Locker
/// 3. Creates an ExecutionContext
/// 4. Executes the task
/// 5. Cleans up and returns the isolate to the pool
///
/// # Arguments
/// * `worker_id` - Unique worker identifier (used as pool key)
/// * `script` - Worker script to execute
/// * `ops` - Operations handle for fetch, KV, etc.
/// * `task` - Task to execute (HTTP request, scheduled event, etc.)
///
/// # Performance
/// - Cache hit: Sub-µs (isolate reused from pool)
/// - Cache miss: Tens of µs with snapshot, few ms without
/// - Same worker_id always reuses the same isolate (warm cache)
///
/// # Example
/// ```ignore
/// use openworkers_runtime_v8::{execute_pooled, RuntimeLimits, Script, Event};
///
/// let result = execute_pooled(
///     "worker_123",
///     script,
///     ops_handle,
///     task,
/// ).await?;
/// ```
pub async fn execute_pooled(
    worker_id: &str,
    script: Script,
    ops: OperationsHandle,
    task: Event,
) -> Result<(), TerminationReason> {
    // Acquire isolate from pool
    let pool = get_pool();
    let pooled = pool.acquire(worker_id).await;

    log::trace!("Acquired pooled isolate for worker_id: {}", worker_id);

    // Get metadata from pooled isolate (needed for ExecutionContext)
    let use_snapshot = pooled.use_snapshot().await;
    let platform = pooled.platform().await;
    let limits = pooled.limits().await;
    let memory_limit_hit = pooled.memory_limit_hit().await;

    // Capture worker_id for logging (avoid lifetime issues)
    let worker_id_str = worker_id.to_string();

    // Execute with locked isolate
    pooled
        .with_lock_async(|isolate| {
            // Execute synchronous part (context creation) first
            let ctx_result = ExecutionContext::new_with_pooled_isolate(
                isolate,
                use_snapshot,
                platform,
                limits,
                memory_limit_hit,
                script,
                ops,
            );

            async move {
                log::trace!(
                    "Executing task for worker_id: {} (with snapshot: {})",
                    worker_id_str,
                    use_snapshot
                );

                // Unwrap the context result
                let mut ctx = ctx_result?;

                // Execute the task
                ctx.exec(task).await
            }
        })
        .await
}
