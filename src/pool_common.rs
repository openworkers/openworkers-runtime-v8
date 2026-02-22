//! Shared types between pool implementations (simple and multiplexed).

use openworkers_core::{Event, OperationsHandle, RuntimeLimits, Script};

/// Configuration for the isolate pool.
#[derive(Clone)]
pub struct PinnedPoolConfig {
    /// Maximum isolates per thread
    pub max_per_thread: usize,
    /// Maximum isolates per owner per thread (prevents one tenant from monopolizing).
    /// None means no limit (bounded only by max_per_thread).
    pub max_per_owner: Option<usize>,
    /// Maximum concurrent requests per isolate (multiplexing).
    /// Only used with the `multiplexing` feature. Simple pool always uses 1.
    pub max_concurrent_per_isolate: usize,
    /// Maximum cached contexts per isolate (warm hit pool).
    /// Limits memory usage per isolate.
    pub max_cached_contexts: usize,
    /// Runtime limits for new isolates
    pub limits: RuntimeLimits,
}

/// Callback invoked on warm hit with the cached operations handle.
///
/// The runner uses this to update per-request state (log_tx, span) on the
/// cached ops handle, which is shared with the still-running event loop.
pub type WarmHitCallback = Box<dyn FnOnce(&OperationsHandle) + Send>;

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
}

/// Thread-local pool statistics.
#[derive(Debug, Clone)]
pub struct LocalPoolStats {
    /// Total isolates in the pool
    pub total: usize,
    /// Currently in use
    pub in_use: usize,
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
