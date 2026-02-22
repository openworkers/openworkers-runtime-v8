//! # OpenWorkers V8 Runtime
//!
//! V8-based JavaScript execution for serverless workers.
//!
//! ## Execution Modes
//!
//! ### Pinned Pool (Recommended for Production)
//!
//! Thread-local isolate pool with per-owner isolation. Two implementations
//! selected at compile time via feature flags:
//!
//! - **Simple** (default): One request per isolate (exclusive access).
//!   Best for most workloads.
//! - **Multiplexed** (`--features multiplexing`): Multiple concurrent requests
//!   per isolate via `AsyncWaiter` fair queue. Best for high-concurrency,
//!   I/O-bound workers.
//!
//! ```ignore
//! use openworkers_runtime_v8::{init_pinned_pool, execute_pinned, PinnedPoolConfig, PinnedExecuteRequest};
//!
//! // Initialize pool once at startup
//! init_pinned_pool(PinnedPoolConfig { .. });
//!
//! // Execute worker
//! execute_pinned(PinnedExecuteRequest { .. }).await?;
//! ```
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

pub mod async_waiter;
pub mod event_loop;
pub mod execution_context;
pub mod execution_helpers;
pub mod gc;
pub mod icudata;
pub mod locker_managed_isolate;
pub mod platform;
pub mod pool_common;
pub mod request_context;
pub mod runtime;
pub mod security;
pub mod snapshot;
pub mod v8_helpers;
pub mod worker;
pub mod worker_future;

mod pool;

// Core API
pub use async_waiter::AsyncWaiter;
pub use execution_context::ExecutionContext;
pub use gc::{
    DeferredDestructionQueue, ExternalMemoryGuard, GcTraceable, JsLock, JsLockRef, Tracked,
    tracked_guard,
};

// Re-export derive macro
pub use gc_derive::GcTraceable as DeriveGcTraceable;
pub use locker_managed_isolate::LockerManagedIsolate;
pub use pool_common::{
    LocalPoolStats, PinnedExecuteRequest, PinnedPoolConfig, PinnedPoolStats, WarmHitCallback,
};
pub use runtime::Runtime;
pub use worker::Worker;
pub use worker_future::WorkerFuture;

pub use pool::{execute_pinned, get_local_pool_stats, get_pinned_pool_stats, init_pinned_pool};

// Snapshot & Code Cache API
pub use snapshot::{
    SnapshotOutput, create_code_cache, create_runtime_snapshot, is_code_cache, pack_code_cache,
    unpack_code_cache,
};

#[cfg(feature = "unsafe-worker-snapshot")]
pub use snapshot::create_worker_snapshot;

// Re-export common types from openworkers-core
pub use openworkers_core::{
    Event, EventType, FetchInit, HttpMethod, HttpRequest, HttpResponse, HttpResponseMeta, LogEvent,
    LogLevel, Operation, OperationResult, OperationsHandle, OperationsHandler, RequestBody,
    ResponseBody, ResponseSender, RuntimeLimits, Script, TaskInit, TaskResult, TaskSource,
    TerminationReason, Worker as WorkerTrait,
};
