//! The pool configuration, request and stats types.

use openworkers_core::{Event, OperationsHandle, RuntimeLimits, Script};

/// Configuration for the isolate pool.
#[derive(Clone)]
pub struct PinnedPoolConfig {
    /// Maximum isolates per thread
    pub max_per_thread: usize,
    /// Maximum isolates per owner per thread (prevents one tenant from monopolizing).
    /// None means no limit (bounded only by max_per_thread).
    pub max_per_owner: Option<usize>,
    /// Requests an isolate serves at once, at least 1. At 1 a request has the
    /// isolate to itself; above 1 the requests share it through the fair queue.
    pub max_concurrent_per_isolate: usize,
    /// Maximum cached contexts per isolate (warm hit pool).
    /// Limits memory usage per isolate.
    pub max_cached_contexts: usize,
    /// Build past `max_per_thread` when every isolate is in use. Off, a
    /// request past the ceiling fails with `Pool at capacity`.
    pub overcommit: bool,
    /// Requests a cached context serves before it is left to age out.
    pub max_context_reuses: u32,
    /// Runtime limits for new isolates
    pub limits: RuntimeLimits,
}

/// Callback invoked on warm hit with the cached operations handle.
///
/// The runner uses this to update per-request state (log_tx, span) on the
/// cached ops handle, which is shared with the still-running event loop.
pub type WarmHitCallback = Box<dyn FnOnce(&OperationsHandle) + Send>;

/// What an event leaves for the caller to record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventReport {
    /// How the fetch listener called respondWith; the Service Worker spec
    /// refuses what `marks.any()` reports.
    pub marks: crate::execution_helpers::ListenerMarks,
    /// The bytes the JS heap of the isolate uses when the event ends. The
    /// heap limit (`RuntimeLimits::heap_max_mb`) applies to the whole
    /// isolate, which the requests of one owner can share. The value counts
    /// garbage that no GC has collected yet, and not the peak the event
    /// reached before it ended.
    pub heap_used_bytes: usize,
}

/// Called once, after the event, with its report.
pub type ReportCallback = Box<dyn FnOnce(EventReport) + Send>;

/// Request parameters for `execute_pinned`.
///
/// Groups all arguments into a single struct for readability and extensibility.
pub struct PinnedExecuteRequest {
    /// Owner identifier (tenant_id) for isolate pool isolation
    pub owner_id: String,
    /// Worker identifier for context caching
    pub worker_id: String,
    /// Worker version for cache invalidation
    pub version: i32,
    /// Worker script to execute
    pub script: Script,
    /// Operations handle for fetch, KV, etc. (used on cold path only)
    pub ops: OperationsHandle,
    /// Task to execute (HTTP request, scheduled event, etc.)
    pub task: Event,
    /// Optional callback invoked with the cached ops on warm hit.
    /// Used by the runner to update per-request state (log_tx, span).
    pub on_warm_hit: Option<WarmHitCallback>,
    /// Environment timestamp for cache invalidation (None = don't check)
    pub env_updated_at: Option<i64>,
    /// Cancelled by the caller to stop this request's ops, on client disconnect.
    pub abort: Option<tokio_util::sync::CancellationToken>,
    /// Called once, after the event, with its report.
    pub on_report: Option<ReportCallback>,
}

/// Thread-local pool statistics.
#[derive(Debug, Clone)]
pub struct LocalPoolStats {
    /// Total isolates in the pool
    pub total: usize,
    /// Currently in use
    pub in_use: usize,
    /// Contexts held for warm reuse. An isolate whose bookkeeping is locked
    /// at that moment counts none.
    pub cached_contexts: usize,
    /// Maximum capacity
    pub capacity: usize,
}

/// Global statistics for the isolate pool.
#[derive(Debug, Clone)]
pub struct PinnedPoolStats {
    pub total_requests: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub hit_rate: f64,
}
