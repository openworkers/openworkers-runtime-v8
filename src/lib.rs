//! # OpenWorkers V8 Runtime
//!
//! This crate provides V8-based JavaScript execution with three distinct execution modes.
//!
//! ## Execution Modes
//!
//! ### 🏆 Mode 1: IsolatePool (Recommended)
//!
//! **Best for:** Production workloads, high throughput (>1000 req/s)
//!
//! ```rust
//! use openworkers_runtime_v8::{init_pool, execute_pooled};
//!
//! // Initialize pool once at startup
//! init_pool(1000, limits);
//!
//! // Execute worker (handles everything internally)
//! execute_pooled("worker-id", script, ops, task).await?;
//! ```
//!
//! **Performance:** <10µs warm start, ~100µs cold start
//! **Thread Model:** Multi-threaded pool with v8::Locker
//! **Memory:** Configurable (pool_size × heap_max)
//!
//! See [`docs/execution_modes.md`](../docs/execution_modes.md) for details.
//!
//! ---
//!
//! ### 🔒 Mode 2: Worker (Maximum Isolation)
//!
//! **Best for:** Maximum security, low request volume (<10 req/s)
//!
//! ```rust
//! use openworkers_runtime_v8::Worker;
//!
//! // Create new isolate per request
//! let mut worker = Worker::new_with_ops(script, limits, ops).await?;
//! worker.exec(task).await?;
//! // Isolate destroyed here
//! ```
//!
//! **Performance:** ~3-5ms per request (creates new isolate)
//! **Thread Model:** Single isolate per request
//! **Memory:** Low (isolate destroyed after each request)
//!
//! ---
//!
//! ### 📦 Mode 3: SharedIsolate (Legacy)
//!
//! **Best for:** Backward compatibility (not recommended for new code)
//!
//! ```rust
//! use openworkers_runtime_v8::{SharedIsolate, ExecutionContext};
//!
//! // Thread-local isolate (one per thread)
//! let mut shared = SharedIsolate::new(limits);
//! let mut ctx = ExecutionContext::new(&mut shared, script, ops)?;
//! ctx.exec(task).await?;
//! ```
//!
//! **Performance:** ~100µs context creation
//! **Thread Model:** Thread-local (not shared across threads)
//! **Memory:** Medium (one isolate per thread)
//!
//! **⚠️ Warning:** Never tested in production. Use IsolatePool instead.
//!
//! ---
//!
//! ## Quick Decision Guide
//!
//! - **Default/Production** → Use **IsolatePool** (fastest, most efficient)
//! - **Maximum Security** → Use **Worker** (per-request isolation)
//! - **Legacy Code** → Use **SharedIsolate** (only if already using it)
//!
//! ## Documentation
//!
//! - [`docs/execution_modes.md`](../docs/execution_modes.md) - Detailed comparison
//! - [`docs/isolate_pool.md`](../docs/isolate_pool.md) - Pool implementation deep dive

pub mod execution_context;
pub mod execution_helpers;
pub mod isolate_pool;
pub mod locker_managed_isolate;
pub mod pooled_execution;
pub mod runtime;
pub mod security;
pub mod shared_isolate;
pub mod snapshot;
pub mod worker;

// Core API
pub use execution_context::ExecutionContext;
pub use isolate_pool::{IsolatePool, PoolStats, get_pool_stats, init_pool};
pub use locker_managed_isolate::LockerManagedIsolate;
pub use pooled_execution::execute_pooled;
pub use runtime::Runtime;
pub use shared_isolate::SharedIsolate;
pub use worker::Worker;

// Re-export common types from openworkers-common
pub use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, HttpResponseMeta, LogEvent, LogLevel,
    Operation, OperationResult, OperationsHandle, OperationsHandler, RequestBody, ResponseBody,
    ResponseSender, RuntimeLimits, ScheduledInit, Script, Task, TaskType, TerminationReason,
    Worker as WorkerTrait,
};
