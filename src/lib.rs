//! # OpenWorkers V8 Runtime
//!
//! This crate provides V8-based JavaScript execution with four distinct execution modes.
//!
//! ## Execution Modes
//!
//! ### 🏆 Mode 1: Thread-Pinned Pool (Recommended for Multi-Tenant)
//!
//! **Best for:** Multi-tenant production, security isolation, CPU-bound workloads
//!
//! ```rust
//! use openworkers_runtime_v8::{init_pinned_pool, execute_pinned, compute_thread_id};
//!
//! // Initialize pool once at startup
//! init_pinned_pool(100, limits);  // 100 isolates per thread
//!
//! // Sticky routing for security (same worker → same thread)
//! let thread_id = compute_thread_id("worker-id", num_threads);
//! // Route request to thread_id, then:
//! execute_pinned("worker-id", script, ops, task).await?;
//! ```
//!
//! **Performance:** Zero mutex contention, <10µs warm start
//! **Thread Model:** Thread-local pools, no cross-thread sharing
//! **Security:** Strong tenant isolation (sticky routing)
//!
//! ---
//!
//! ### 🚀 Mode 2: Shared Pool
//!
//! **Best for:** I/O-bound workloads, resource sharing
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
//! **Thread Model:** Global mutex-protected LRU cache
//! **⚠️ Warning:** Can have contention under CPU-bound load
//!
//! ---
//!
//! ### 🔒 Mode 3: Worker (Maximum Isolation)
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
//! **Performance:** ~2-3ms per request (creates new isolate)
//! **Thread Model:** Single isolate per request
//! **Memory:** Low (isolate destroyed after each request)
//!
//! ---
//!
//! ### 📦 Mode 4: SharedIsolate (Legacy)
//!
//! **Best for:** Backward compatibility only
//!
//! **⚠️ Warning:** Deprecated. Use Thread-Pinned Pool instead.
//!
//! ---
//!
//! ## Quick Decision Guide
//!
//! - **Multi-Tenant** → Use **Thread-Pinned Pool** (security + performance)
//! - **I/O-Heavy** → Use **Shared Pool** (good resource utilization)
//! - **Maximum Security** → Use **Worker** (per-request isolation)
//! - **Legacy Code** → Use **SharedIsolate** (only if already using it)
//!
//! ## Documentation
//!
//! - [`docs/thread-pinned-vs-shared-pool.md`](../docs/thread-pinned-vs-shared-pool.md) - Architecture comparison
//! - [`docs/execution_modes.md`](../docs/execution_modes.md) - Detailed comparison

pub mod execution_context;
pub mod execution_helpers;
pub mod isolate_pool;
pub mod locker_managed_isolate;
pub mod pooled_execution;
pub mod runtime;
pub mod security;
pub mod shared_isolate;
pub mod snapshot;
pub mod thread_pinned_pool;
pub mod worker;
pub mod worker_future;

// Core API
pub use execution_context::ExecutionContext;
pub use isolate_pool::{IsolatePool, PoolStats, get_pool_stats, init_pool};
pub use locker_managed_isolate::LockerManagedIsolate;
pub use pooled_execution::execute_pooled;
pub use runtime::Runtime;
pub use shared_isolate::SharedIsolate;
pub use thread_pinned_pool::{
    LocalPoolStats, PinnedPoolConfig, PinnedPoolStats, compute_thread_id, execute_pinned,
    get_local_pool_stats, get_pinned_pool_stats, init_pinned_pool,
};
pub use worker::Worker;
pub use worker_future::WorkerFuture;

// Re-export common types from openworkers-common
pub use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, HttpResponseMeta, LogEvent, LogLevel,
    Operation, OperationResult, OperationsHandle, OperationsHandler, RequestBody, ResponseBody,
    ResponseSender, RuntimeLimits, ScheduledInit, Script, Task, TaskType, TerminationReason,
    Worker as WorkerTrait,
};
