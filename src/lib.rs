//! # OpenWorkers V8 Runtime
//!
//! V8-based JavaScript execution for serverless workers.
//!
//! ## Execution Modes
//!
//! ### Shared Pool (Recommended for Production)
//!
//! Best for: High-volume production workloads, I/O-bound workers
//!
//! ```ignore
//! use openworkers_runtime_v8::{init_pool, execute_pooled};
//!
//! // Initialize pool once at startup
//! init_pool(1000, limits);
//!
//! // Execute worker (handles everything internally)
//! execute_pooled("worker-id", script, ops, task).await?;
//! ```
//!
//! Performance: <10µs warm start, ~100µs cold start (with snapshot)
//!
//! ### Worker (Maximum Isolation)
//!
//! Best for: Maximum security, low request volume
//!
//! ```ignore
//! use openworkers_runtime_v8::Worker;
//!
//! let mut worker = Worker::new_with_ops(script, limits, ops).await?;
//! worker.exec(task).await?;
//! ```
//!
//! Performance: ~2-3ms per request (creates new isolate)

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

// Re-export common types from openworkers-core
pub use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, HttpResponseMeta, LogEvent, LogLevel,
    Operation, OperationResult, OperationsHandle, OperationsHandler, RequestBody, ResponseBody,
    ResponseSender, RuntimeLimits, ScheduledInit, Script, Task, TaskType, TerminationReason,
    Worker as WorkerTrait,
};
