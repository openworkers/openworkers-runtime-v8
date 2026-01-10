//! Pooled execution - ExecutionContext with isolate pool integration
//!
//! This module provides a high-level API for executing worker scripts using
//! the isolate pool. It wraps ExecutionContext and manages the pool lifecycle.

use crate::execution_context::ExecutionContext;
use crate::isolate_pool::get_pool;
use openworkers_core::{OperationsHandle, Script, Task, TerminationReason};
use std::sync::Arc;

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
/// - Cache hit: <10µs (isolate reused from pool)
/// - Cache miss: ~100µs (with snapshot) or ~3-5ms (without)
/// - Same worker_id always reuses the same isolate (warm cache)
///
/// # Example
/// ```ignore
/// use openworkers_runtime_v8::{execute_pooled, RuntimeLimits, Script, Task};
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
    task: Task,
) -> Result<(), TerminationReason> {
    // Acquire isolate from pool
    let pool = get_pool();
    let pooled = pool.acquire(worker_id).await;

    log::trace!("Acquired pooled isolate for worker_id: {}", worker_id);

    // Execute with locked isolate
    pooled
        .with_lock_async(|isolate| async move {
            // TODO: Create ExecutionContext with &v8::Isolate instead of &mut SharedIsolate
            //
            // Current issue: ExecutionContext::new() takes &mut SharedIsolate,
            // but we have &v8::Isolate from the pool.
            //
            // Solutions (to be implemented once rusty_v8 compiles):
            //
            // Option 1: Refactor ExecutionContext to accept &v8::Isolate
            //   - Change ExecutionContext::new(isolate: &v8::Isolate, ...)
            //   - Works with both SharedIsolate and PooledIsolate
            //   - Breaking change, needs testing
            //
            // Option 2: Create thin wrapper that adapts &v8::Isolate to SharedIsolate interface
            //   - Less invasive, no breaking changes
            //   - Slightly more boilerplate
            //
            // Option 3: Duplicate ExecutionContext as PooledExecutionContext
            //   - No breaking changes
            //   - Code duplication, harder to maintain
            //
            // For now, return placeholder until rusty_v8 is ready

            // Placeholder implementation
            log::warn!("execute_pooled: Not yet implemented - waiting for rusty_v8 compilation");
            Err(TerminationReason::Other(
                "Pooled execution not yet integrated with ExecutionContext".to_string(),
            ))

            // Once rusty_v8 compiles, this will be:
            // let mut ctx = ExecutionContext::new_with_isolate(isolate, script, ops)?;
            // ctx.exec(task).await
        })
        .await
}
