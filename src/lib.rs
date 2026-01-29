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
//! execute_pooled("worker-id", script, ops, event).await?;
//! ```
//!
//! Performance: Sub-µs warm start, tens of µs cold start (with snapshot)
//!
//! ### Worker (Maximum Isolation)
//!
//! Best for: Maximum security, low request volume
//!
//! ```ignore
//! use openworkers_runtime_v8::Worker;
//!
//! let mut worker = Worker::new_with_ops(script, limits, ops).await?;
//! worker.exec(event).await?;
//! ```
//!
//! Performance: Few ms per request (creates new isolate)

pub mod event_loop;
pub mod execution_context;
pub mod execution_helpers;
pub mod gc;
pub mod icudata;
pub mod isolate_pool;
pub mod locker_managed_isolate;
pub mod platform;
pub mod pooled_execution;
pub mod runtime;
pub mod security;
pub mod shared_isolate;
pub mod snapshot;
pub mod thread_pinned_pool;
pub mod v8_helpers;
pub mod worker;
pub mod worker_future;

// Core API
pub use execution_context::ExecutionContext;
pub use gc::{
    DeferredDestructionQueue, ExternalMemoryGuard, GcTraceable, JsLock, JsLockRef, Tracked,
    tracked_guard,
};

// Re-export derive macro
pub use gc_derive::GcTraceable as DeriveGcTraceable;
pub use isolate_pool::{IsolatePool, PoolStats, get_pool_stats, init_pool};
pub use locker_managed_isolate::LockerManagedIsolate;
pub use pooled_execution::execute_pooled;
pub use runtime::Runtime;
pub use shared_isolate::SharedIsolate;
pub use thread_pinned_pool::{
    DEFAULT_QUEUE_SIZE, DEFAULT_QUEUE_TIMEOUT_MS, LocalPoolStats, PinnedPoolConfig,
    PinnedPoolStats, QUEUE_FULL_ERROR, QUEUE_TIMEOUT_ERROR, execute_pinned, get_local_pool_stats,
    get_pinned_pool_stats, init_pinned_pool, init_pinned_pool_full,
    init_pinned_pool_with_owner_limit,
};
pub use worker::Worker;
pub use worker_future::WorkerFuture;

// Re-export common types from openworkers-core
pub use openworkers_core::{
    Event, EventType, FetchInit, HttpMethod, HttpRequest, HttpResponse, HttpResponseMeta, LogEvent,
    LogLevel, Operation, OperationResult, OperationsHandle, OperationsHandler, RequestBody,
    ResponseBody, ResponseSender, RuntimeLimits, Script, TaskInit, TaskResult, TaskSource,
    TerminationReason, Worker as WorkerTrait,
};
