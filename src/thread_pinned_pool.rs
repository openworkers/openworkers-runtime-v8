//! Thread-pinned isolate pool with per-owner isolation.
//!
//! Features:
//! - Thread-local pools (no global mutex contention)
//! - Multiple isolates per owner for concurrency (no serialization)
//! - Owner-based isolation (worker_id or tenant_id)
//! - LRU eviction when pool is full
//! - Queue with backpressure when at capacity
//!
//! Note: Sticky routing has been removed. Use round-robin at the caller level.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

use crate::LockerManagedIsolate;
use crate::execution_context::ExecutionContext;
use crate::gc::JsLock;
use openworkers_core::{Event, OperationsHandle, RuntimeLimits, Script, TerminationReason};

/// Default max reuses before discarding a cached context (prevents memory growth)
const DEFAULT_CONTEXT_MAX_REUSES: u32 = 1000;

// ============================================================================
// Configuration
// ============================================================================

/// Global configuration for thread-pinned pool
static PINNED_POOL_CONFIG: OnceLock<PinnedPoolConfig> = OnceLock::new();

/// Default queue size per thread (max waiters)
pub const DEFAULT_QUEUE_SIZE: usize = 10;

/// Default queue wait timeout in milliseconds
pub const DEFAULT_QUEUE_TIMEOUT_MS: u64 = 5000;

/// Configuration for thread-pinned pool
#[derive(Clone)]
pub struct PinnedPoolConfig {
    /// Maximum isolates per thread
    pub max_per_thread: usize,
    /// Maximum isolates per owner per thread (prevents one tenant from monopolizing)
    /// None means no limit (bounded only by max_per_thread)
    pub max_per_owner: Option<usize>,
    /// Maximum requests waiting in queue per thread (backpressure)
    /// When queue is full, new requests get 503
    pub queue_size: usize,
    /// Maximum time to wait in queue (ms) before returning 503
    pub queue_timeout_ms: u64,
    /// Runtime limits for new isolates
    pub limits: RuntimeLimits,
}

/// Initialize the thread-pinned pool configuration
///
/// Must be called once at startup, before any pool access.
/// Uses default queue settings (size=10, timeout=5000ms).
///
/// # Arguments
/// * `max_per_thread` - Maximum isolates per thread
/// * `limits` - Runtime limits for V8 isolates
pub fn init_pinned_pool(max_per_thread: usize, limits: RuntimeLimits) {
    init_pinned_pool_full(
        max_per_thread,
        None,
        DEFAULT_QUEUE_SIZE,
        DEFAULT_QUEUE_TIMEOUT_MS,
        limits,
    );
}

/// Initialize the thread-pinned pool with per-owner limit
///
/// Uses default queue settings (size=10, timeout=5000ms).
///
/// # Arguments
/// * `max_per_thread` - Maximum isolates per thread
/// * `max_per_owner` - Maximum isolates per owner per thread (None = no limit)
/// * `limits` - Runtime limits for V8 isolates
///
/// # Example
/// ```ignore
/// // Max 100 isolates per thread, max 2 per tenant per thread
/// init_pinned_pool_with_owner_limit(100, Some(2), limits);
/// ```
pub fn init_pinned_pool_with_owner_limit(
    max_per_thread: usize,
    max_per_owner: Option<usize>,
    limits: RuntimeLimits,
) {
    init_pinned_pool_full(
        max_per_thread,
        max_per_owner,
        DEFAULT_QUEUE_SIZE,
        DEFAULT_QUEUE_TIMEOUT_MS,
        limits,
    );
}

/// Initialize the thread-pinned pool with full configuration
///
/// # Arguments
/// * `max_per_thread` - Maximum isolates per thread
/// * `max_per_owner` - Maximum isolates per owner per thread (None = no limit)
/// * `queue_size` - Maximum requests waiting in queue (backpressure)
/// * `queue_timeout_ms` - Max wait time in queue before 503
/// * `limits` - Runtime limits for V8 isolates
///
/// # Example
/// ```ignore
/// // Max 100 isolates/thread, max 2/owner, queue of 20, 3s timeout
/// init_pinned_pool_full(100, Some(2), 20, 3000, limits);
/// ```
pub fn init_pinned_pool_full(
    max_per_thread: usize,
    max_per_owner: Option<usize>,
    queue_size: usize,
    queue_timeout_ms: u64,
    limits: RuntimeLimits,
) {
    let config = PinnedPoolConfig {
        max_per_thread,
        max_per_owner,
        queue_size,
        queue_timeout_ms,
        limits,
    };

    if PINNED_POOL_CONFIG.set(config).is_err() {
        tracing::warn!("Thread-pinned pool already initialized");
    } else {
        tracing::info!(
            "Thread-pinned pool initialized: max_per_thread={}, max_per_owner={:?}, queue_size={}, queue_timeout_ms={}",
            max_per_thread,
            max_per_owner,
            queue_size,
            queue_timeout_ms
        );
    }
}

fn get_config() -> &'static PinnedPoolConfig {
    PINNED_POOL_CONFIG
        .get()
        .expect("Thread-pinned pool not initialized. Call init_pinned_pool() at startup.")
}

// ============================================================================
// Thread-Local Pool with Per-Owner Isolation
// ============================================================================

/// A cached execution context for warm isolate reuse.
///
/// When a request succeeds, the context is cached here for the next request
/// to the same worker+version. On warm hit, we call `ctx.reset()` and dispatch
/// the event directly, skipping the full context creation (~6ms → ~0.1ms).
struct CachedContext {
    ctx: ExecutionContext,
    worker_id: String,
    version: i32,
    reuse_count: u32,
    ops: OperationsHandle,
}

/// Get the max context reuses from CONTEXT_MAX_REUSES env var
fn context_max_reuses() -> u32 {
    static MAX_REUSES: OnceLock<u32> = OnceLock::new();
    *MAX_REUSES.get_or_init(|| {
        std::env::var("CONTEXT_MAX_REUSES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_CONTEXT_MAX_REUSES)
    })
}

/// Inner data for a tagged isolate (protected by mutex)
struct TaggedIsolateInner {
    isolate: LockerManagedIsolate,
    /// Cached context for warm reuse (PINNED mode only)
    cached_context: Option<CachedContext>,
    #[allow(dead_code)]
    created_at: Instant,
    last_used: Instant,
    total_requests: u64,
}

/// A tagged isolate with owner tracking for isolation.
///
/// Each isolate is tagged with an owner_id (worker_id or tenant_id).
/// Multiple isolates can exist for the same owner (for concurrency).
/// The `in_use` flag allows fast lock-free checking of availability.
struct TaggedIsolate {
    /// Owner identifier (immutable after creation)
    owner_id: String,
    /// Marks if this isolate is currently being used (atomic for lock-free check)
    in_use: AtomicBool,
    /// Protected isolate data
    inner: Mutex<TaggedIsolateInner>,
}

impl TaggedIsolate {
    fn new(owner_id: String, limits: RuntimeLimits) -> Self {
        tracing::debug!("Creating new TaggedIsolate for owner: {}", owner_id);
        let start = Instant::now();
        let isolate = LockerManagedIsolate::new(limits);
        let duration = start.elapsed();
        tracing::info!(
            "TaggedIsolate created for owner {} in {:?} (snapshot: {})",
            owner_id,
            duration,
            isolate.use_snapshot
        );

        let now = Instant::now();
        Self {
            owner_id,
            in_use: AtomicBool::new(false),
            inner: Mutex::new(TaggedIsolateInner {
                isolate,
                cached_context: None,
                created_at: now,
                last_used: now,
                total_requests: 0,
            }),
        }
    }

    /// Try to acquire this isolate atomically. Returns true if successful.
    fn try_acquire(&self) -> bool {
        self.in_use
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Release this isolate.
    fn release(&self) {
        self.in_use.store(false, Ordering::Release);
    }

    /// Check if this isolate is free (without acquiring).
    fn is_free(&self) -> bool {
        !self.in_use.load(Ordering::Acquire)
    }
}

/// Result of an acquire operation
struct AcquireResult {
    /// The acquired isolate
    isolate: Arc<TaggedIsolate>,
    /// Whether this was a cache hit (existing isolate with same owner)
    cache_hit: bool,
    /// If an eviction occurred, the evicted owner_id
    #[allow(dead_code)]
    evicted: Option<String>,
}

/// Thread-local pool supporting multiple isolates per owner.
///
/// Unlike the previous LRU-keyed pool, this allows concurrent requests
/// for the same owner to use different isolates (no serialization).
struct ThreadLocalPool {
    /// All isolates in this pool
    isolates: Vec<Arc<TaggedIsolate>>,
    /// Maximum isolates in this pool
    max_isolates: usize,
    /// Maximum isolates per owner (None = no limit)
    max_per_owner: Option<usize>,
    /// Maximum waiters in queue
    queue_size: usize,
    /// Current number of waiters
    current_waiters: usize,
    /// Semaphore for FIFO waking of waiters (starts with 0 permits)
    /// When isolate released: add_permits(1)
    /// When waiting: acquire().await (FIFO guaranteed by tokio)
    release_semaphore: Arc<Semaphore>,
    /// Runtime limits for new isolates
    limits: RuntimeLimits,
}

impl ThreadLocalPool {
    fn new(
        max_isolates: usize,
        max_per_owner: Option<usize>,
        queue_size: usize,
        limits: RuntimeLimits,
    ) -> Self {
        Self {
            isolates: Vec::with_capacity(max_isolates),
            max_isolates,
            max_per_owner,
            queue_size,
            current_waiters: 0,
            // Start with 0 permits - permits are added when isolates are released
            release_semaphore: Arc::new(Semaphore::new(0)),
            limits,
        }
    }

    /// Try to enter the wait queue. Returns true if allowed, false if queue is full.
    fn try_enter_queue(&mut self) -> bool {
        if self.current_waiters < self.queue_size {
            self.current_waiters += 1;
            tracing::debug!(
                "Entered wait queue (waiters: {}/{})",
                self.current_waiters,
                self.queue_size
            );
            true
        } else {
            tracing::warn!(
                "Wait queue full ({}/{}), rejecting request",
                self.current_waiters,
                self.queue_size
            );
            false
        }
    }

    /// Leave the wait queue (either got isolate or gave up)
    fn leave_queue(&mut self) {
        if self.current_waiters > 0 {
            self.current_waiters -= 1;
            tracing::debug!(
                "Left wait queue (waiters: {}/{})",
                self.current_waiters,
                self.queue_size
            );
        }
    }

    /// Get the semaphore handle for waiting (FIFO guaranteed)
    fn get_semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.release_semaphore)
    }

    /// Signal that an isolate was released (wake one waiter in FIFO order)
    fn signal_release(&self) {
        if self.current_waiters > 0 {
            tracing::debug!("Adding permit to wake next waiter (FIFO)");
            self.release_semaphore.add_permits(1);
        }
    }

    /// Count isolates for a given owner (both free and in-use)
    fn count_for_owner(&self, owner_id: &str) -> usize {
        self.isolates
            .iter()
            .filter(|i| i.owner_id == owner_id)
            .count()
    }

    /// Check if owner has reached their per-owner limit
    fn owner_at_limit(&self, owner_id: &str) -> bool {
        match self.max_per_owner {
            Some(limit) => self.count_for_owner(owner_id) >= limit,
            None => false,
        }
    }

    /// Acquire an isolate for the given owner.
    ///
    /// Strategy:
    /// 1. Try to find a FREE isolate with matching owner_id
    /// 2. If not found and owner under limit, create a new one (if pool under capacity)
    /// 3. If pool at capacity, evict LRU (oldest last_used among free isolates)
    /// 4. If owner at limit, return None (caller should try another thread)
    /// 5. If all busy and owner under limit, create anyway (temporary over-limit)
    #[allow(clippy::arc_with_non_send_sync)]
    fn acquire(&mut self, owner_id: &str) -> Option<AcquireResult> {
        // 1. Try to find a FREE isolate with matching owner_id
        for arc in &self.isolates {
            if arc.owner_id == owner_id && arc.try_acquire() {
                tracing::debug!(
                    "Cache HIT: acquired existing isolate for owner {}",
                    owner_id
                );
                return Some(AcquireResult {
                    isolate: Arc::clone(arc),
                    cache_hit: true,
                    evicted: None,
                });
            }
        }

        // Check if owner has reached their limit
        if self.owner_at_limit(owner_id) {
            tracing::debug!(
                "Owner {} at limit ({:?} isolates), cannot create more on this thread",
                owner_id,
                self.max_per_owner
            );
            return None; // Signal caller to try another thread
        }

        // 2. No free matching isolate - create new if under pool limit
        if self.isolates.len() < self.max_isolates {
            tracing::debug!(
                "Cache MISS: creating new isolate for owner {} (pool: {}/{}, owner: {}/{:?})",
                owner_id,
                self.isolates.len() + 1,
                self.max_isolates,
                self.count_for_owner(owner_id) + 1,
                self.max_per_owner
            );

            let entry = Arc::new(TaggedIsolate::new(
                owner_id.to_string(),
                self.limits.clone(),
            ));
            entry.try_acquire(); // Mark as in use
            self.isolates.push(Arc::clone(&entry));

            return Some(AcquireResult {
                isolate: entry,
                cache_hit: false,
                evicted: None,
            });
        }

        // 3. Pool full - find LRU (oldest last_used among FREE isolates)
        let mut lru_idx: Option<usize> = None;
        let mut oldest = Instant::now();

        for (i, arc) in self.isolates.iter().enumerate() {
            // Only consider FREE isolates for eviction
            if arc.is_free() {
                // Try to get last_used without blocking
                if let Ok(guard) = arc.inner.try_lock()
                    && guard.last_used < oldest
                {
                    oldest = guard.last_used;
                    lru_idx = Some(i);
                }
            }
        }

        if let Some(idx) = lru_idx {
            let old = self.isolates.remove(idx);
            let evicted_owner = old.owner_id.clone();
            tracing::info!(
                "LRU eviction: evicting isolate for owner {}, replacing with {}",
                evicted_owner,
                owner_id
            );

            // Create new isolate
            let entry = Arc::new(TaggedIsolate::new(
                owner_id.to_string(),
                self.limits.clone(),
            ));
            entry.try_acquire(); // Mark as in use
            self.isolates.push(Arc::clone(&entry));

            return Some(AcquireResult {
                isolate: entry,
                cache_hit: false,
                evicted: Some(evicted_owner),
            });
        }

        // 4. All isolates busy - create anyway (temporary over-limit)
        // This prevents deadlock but should be monitored
        tracing::warn!(
            "Pool overcommit: all {} isolates busy, creating extra for owner {}",
            self.isolates.len(),
            owner_id
        );

        let entry = Arc::new(TaggedIsolate::new(
            owner_id.to_string(),
            self.limits.clone(),
        ));
        entry.try_acquire();
        self.isolates.push(Arc::clone(&entry));

        Some(AcquireResult {
            isolate: entry,
            cache_hit: false,
            evicted: None,
        })
    }

    /// Clean up over-limit isolates that are no longer in use.
    /// Call this periodically to reclaim memory after overcommit.
    #[allow(dead_code)]
    fn cleanup_overlimit(&mut self) {
        if self.isolates.len() <= self.max_isolates {
            return;
        }

        // Remove free isolates until we're at capacity
        let mut i = 0;
        while i < self.isolates.len() && self.isolates.len() > self.max_isolates {
            if self.isolates[i].is_free() {
                let removed = self.isolates.remove(i);
                tracing::info!(
                    "Cleanup: removed over-limit isolate for owner {}",
                    removed.owner_id
                );
            } else {
                i += 1;
            }
        }
    }

    fn stats(&self) -> LocalPoolStats {
        let total = self.isolates.len();
        let in_use = self.isolates.iter().filter(|i| !i.is_free()).count();

        LocalPoolStats {
            total,
            in_use,
            capacity: self.max_isolates,
        }
    }
}

thread_local! {
    /// Thread-local pool instance
    static LOCAL_POOL: std::cell::RefCell<Option<ThreadLocalPool>> = const { std::cell::RefCell::new(None) };
}

/// Initialize the thread-local pool if needed
fn ensure_pool_initialized() {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();

        if pool_opt.is_none() {
            let config = get_config();
            *pool_opt = Some(ThreadLocalPool::new(
                config.max_per_thread,
                config.max_per_owner,
                config.queue_size,
                config.limits.clone(),
            ));
            tracing::debug!(
                "Thread-local pool initialized on thread {:?} (max_per_owner={:?}, queue_size={})",
                std::thread::current().id(),
                config.max_per_owner,
                config.queue_size
            );
        }
    });
}

/// Acquire an isolate entry from the local pool.
///
/// Returns `Some((isolate, cache_hit))` if successful, or `None` if owner is at their limit.
fn acquire_from_local_pool(owner_id: &str) -> Option<(Arc<TaggedIsolate>, bool)> {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();
        let pool = pool_opt.as_mut().expect("Pool not initialized");

        pool.acquire(owner_id)
            .map(|result| (result.isolate, result.cache_hit))
    })
}

/// Try to enter the wait queue. Returns true if allowed, false if queue is full.
fn try_enter_queue() -> bool {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();
        let pool = pool_opt.as_mut().expect("Pool not initialized");
        pool.try_enter_queue()
    })
}

/// Leave the wait queue
fn leave_queue() {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();
        let pool = pool_opt.as_mut().expect("Pool not initialized");
        pool.leave_queue()
    })
}

/// Get the semaphore handle for FIFO waiting
fn get_semaphore() -> Arc<Semaphore> {
    LOCAL_POOL.with(|pool_cell| {
        let pool_opt = pool_cell.borrow();
        let pool = pool_opt.as_ref().expect("Pool not initialized");
        pool.get_semaphore()
    })
}

/// Release an isolate back to the pool and signal waiters.
///
/// This marks the isolate as available for reuse by other requests.
fn release_to_local_pool(isolate: &TaggedIsolate) {
    isolate.release();

    // Signal any waiters that an isolate is now available
    LOCAL_POOL.with(|pool_cell| {
        let pool_opt = pool_cell.borrow();

        if let Some(pool) = pool_opt.as_ref() {
            pool.signal_release();
        }
    });
}

// ============================================================================
// Statistics
// ============================================================================

/// Global statistics for thread-pinned pool
static TOTAL_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);

/// Statistics for thread-local pool
#[derive(Debug, Clone)]
pub struct LocalPoolStats {
    /// Total isolates in the pool
    pub total: usize,
    /// Currently in use
    pub in_use: usize,
    /// Maximum capacity
    pub capacity: usize,
}

/// Global statistics for thread-pinned pool
#[derive(Debug, Clone)]
pub struct PinnedPoolStats {
    pub total_requests: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub hit_rate: f64,
}

/// Get global statistics
pub fn get_pinned_pool_stats() -> PinnedPoolStats {
    let total = TOTAL_REQUESTS.load(Ordering::Relaxed);
    let hits = CACHE_HITS.load(Ordering::Relaxed);
    let misses = CACHE_MISSES.load(Ordering::Relaxed);

    PinnedPoolStats {
        total_requests: total,
        cache_hits: hits,
        cache_misses: misses,
        hit_rate: if total > 0 {
            hits as f64 / total as f64
        } else {
            0.0
        },
    }
}

/// Get thread-local pool statistics
pub fn get_local_pool_stats() -> Option<LocalPoolStats> {
    LOCAL_POOL.with(|pool| pool.borrow().as_ref().map(|p| p.stats()))
}

// ============================================================================
// Execution API
// ============================================================================

/// Error message returned when queue is full (503 Service Unavailable)
pub const QUEUE_FULL_ERROR: &str = "Queue full - service unavailable";

/// Error message returned when queue wait times out
pub const QUEUE_TIMEOUT_ERROR: &str = "Queue wait timeout";

/// Execute a worker script using the thread-pinned pool.
///
/// This uses thread-local storage for zero-contention access to isolates.
/// Multiple isolates can exist for the same owner, enabling concurrent execution.
///
/// ## Warm Isolates
///
/// When the same worker_id+version is executed on the same isolate, the context
/// is reused (warm hit). This skips the full context creation (~6ms) and only
/// resets per-request state (~0.1ms). On any error, the context is discarded.
///
/// # Arguments
/// * `owner_id` - Owner identifier (worker_id or tenant_id) for isolation
/// * `worker_id` - Worker identifier for context caching
/// * `version` - Worker version for cache invalidation
/// * `script` - Worker script to execute
/// * `ops` - Operations handle for fetch, KV, etc. (used on cold path only)
/// * `task` - Task to execute (HTTP request, scheduled event, etc.)
/// * `on_warm_hit` - Optional callback invoked with the cached ops on warm hit.
///   Used by the runner to update per-request state (log_tx, span) on the
///   cached ops handle that the event loop holds.
///
/// # Isolation Model
/// - Isolates are tagged with owner_id
/// - A request only uses isolates with matching owner_id
/// - If all matching isolates are busy, a new one is created (concurrency)
/// - Per-owner limit prevents one tenant from monopolizing a thread
/// - When at limit, request waits in queue (with timeout)
/// - If queue is full, returns 503 immediately
/// - LRU eviction when pool is full
///
/// # Returns
/// - `Ok(())` on successful execution
/// - `Err(TerminationReason::Other(QUEUE_FULL_ERROR))` if queue is full (503)
/// - `Err(TerminationReason::Other(QUEUE_TIMEOUT_ERROR))` if wait times out
/// - Other errors for execution failures
///
/// # Performance
/// - No mutex contention (thread-local access)
/// - Cache hit: Sub-µs (isolate reused)
/// - Cache miss: Tens of µs with snapshot, few ms without
///
/// # Example
/// ```ignore
/// use openworkers_runtime_v8::{execute_pinned, init_pinned_pool_full, RuntimeLimits};
///
/// // Initialize with per-owner limit and queue
/// init_pinned_pool_full(100, Some(2), 20, 5000, RuntimeLimits::default());
///
/// // Execute per request (caller handles thread distribution)
/// match execute_pinned("tenant_1", "worker_123", 1, script, ops, task, None).await {
///     Ok(()) => { /* success */ }
///     Err(TerminationReason::Other(msg)) if msg == QUEUE_FULL_ERROR => {
///         // Return 503 Service Unavailable
///     }
///     Err(e) => { /* other error */ }
/// }
/// ```
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

pub async fn execute_pinned(req: PinnedExecuteRequest) -> Result<(), TerminationReason> {
    let PinnedExecuteRequest {
        owner_id,
        worker_id,
        version,
        script,
        ops,
        task,
        on_warm_hit,
    } = req;
    TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    // Ensure pool is initialized
    ensure_pool_initialized();

    let timeout_ms = get_config().queue_timeout_ms;
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();

    // Try to acquire an isolate, waiting in queue if at limit
    let (isolate_arc, is_hit) = loop {
        // Try to acquire
        if let Some(result) = acquire_from_local_pool(&owner_id) {
            break result;
        }

        // Owner at limit - try to enter queue
        if !try_enter_queue() {
            // Queue is full - return 503 immediately
            tracing::warn!("Owner {} at limit and queue full, returning 503", owner_id);
            return Err(TerminationReason::Other(QUEUE_FULL_ERROR.to_string()));
        }

        // Wait for an isolate to be released (with timeout, FIFO order)
        let semaphore = get_semaphore();
        let remaining = timeout.saturating_sub(start.elapsed());

        if remaining.is_zero() {
            leave_queue();
            tracing::warn!(
                "Owner {} queue wait timeout after {}ms",
                owner_id,
                timeout_ms
            );
            return Err(TerminationReason::Other(QUEUE_TIMEOUT_ERROR.to_string()));
        }

        tracing::debug!(
            "Owner {} waiting in queue FIFO (remaining: {:?})",
            owner_id,
            remaining
        );

        // Wait with timeout - FIFO guaranteed by tokio Semaphore
        match tokio::time::timeout(remaining, semaphore.acquire()).await {
            Ok(Ok(permit)) => {
                // Got permit - forget it (we don't need to hold it, just the signal)
                permit.forget();
                // Leave queue and retry
                leave_queue();
                tracing::debug!(
                    "Owner {} woken from queue (FIFO), retrying acquire",
                    owner_id
                );
                // Loop will retry acquire
            }
            Ok(Err(_)) => {
                // Semaphore closed (shouldn't happen)
                leave_queue();
                tracing::error!("Semaphore closed unexpectedly");
                return Err(TerminationReason::Other("Internal error".to_string()));
            }
            Err(_) => {
                // Timeout
                leave_queue();
                tracing::warn!(
                    "Owner {} queue wait timeout after {}ms",
                    owner_id,
                    timeout_ms
                );
                return Err(TerminationReason::Other(QUEUE_TIMEOUT_ERROR.to_string()));
            }
        }
    };

    if is_hit {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    // Lock the inner data for exclusive access
    let mut inner = isolate_arc.inner.lock().await;
    inner.total_requests += 1;
    inner.last_used = Instant::now();

    tracing::trace!(
        "Acquired thread-local isolate for owner: {} (requests: {})",
        owner_id,
        inner.total_requests
    );

    // Get metadata from the isolate
    let use_snapshot = inner.isolate.use_snapshot;
    let platform = inner.isolate.platform;
    let limits = inner.isolate.limits.clone();
    let memory_limit_hit = Arc::clone(&inner.isolate.memory_limit_hit);
    let destruction_queue = Arc::clone(&inner.isolate.deferred_destruction_queue);

    // ── Warm hit check ─────────────────────────────────────────────────
    // Take cached_context BEFORE creating Locker to avoid overlapping borrows.
    // v8::Locker borrows inner.isolate.isolate, so we can't access inner.cached_context
    // after that. Taking it out first is safe — no V8 interaction until locker exists.
    let max_reuses = context_max_reuses();
    let mut cached_context = inner.cached_context.take();

    // Decide: warm hit, discard, or cold path
    let warm_hit = if let Some(ref cached) = cached_context {
        if cached.worker_id == worker_id
            && cached.version == version
            && cached.reuse_count < max_reuses
        {
            true
        } else {
            // Different worker/version or max reuses — discard
            if cached.reuse_count >= max_reuses {
                tracing::debug!(
                    "context DISCARDED: worker={}, reason=max_reuses ({})",
                    &cached.worker_id[..8.min(cached.worker_id.len())],
                    max_reuses
                );
            } else {
                tracing::debug!(
                    "context DISCARDED: worker={}, reason=different_worker (new={})",
                    &cached.worker_id[..8.min(cached.worker_id.len())],
                    &worker_id[..8.min(worker_id.len())]
                );
            }

            cached_context = None;
            false
        }
    } else {
        false
    };

    tracing::trace!(
        "Creating v8::Locker for owner {} (before v8::Locker::new)",
        owner_id
    );

    // Create v8::Locker for thread-safety
    let mut locker = v8::Locker::new(&mut inner.isolate.isolate);

    tracing::trace!(
        "v8::Locker created for owner {}, processing deferred destructions",
        owner_id
    );

    // Process any pending deferred handle destructions (while lock is held)
    destruction_queue.process_all();

    // Register JsLock for GC tracking (applies any pending memory adjustments)
    let _js_lock = JsLock::new(&mut locker);

    // Reset memory limit flag
    memory_limit_hit.store(false, Ordering::SeqCst);

    // ── Warm hit path ──────────────────────────────────────────────────
    if warm_hit {
        let cached = cached_context.as_mut().unwrap();
        cached.reuse_count += 1;
        let reuse_count = cached.reuse_count;

        tracing::debug!(
            "context WARM: worker={}, version={}, reuse_count={}",
            &worker_id[..8.min(worker_id.len())],
            version,
            reuse_count
        );

        // Update per-request state on the cached ops handle.
        // The event loop is still running with this ops — updating log_tx/span
        // via interior mutability makes it serve the new request correctly.
        if let Some(callback) = on_warm_hit {
            callback(&cached.ops);
        }

        match cached.ctx.reset() {
            Ok(()) => {
                let result = cached.ctx.exec(task).await;

                if result.is_err() {
                    let reason = result.as_ref().err().unwrap();
                    tracing::debug!(
                        "context DISCARDED: worker={}, reason={:?}",
                        &worker_id[..8.min(worker_id.len())],
                        reason
                    );
                    // Don't put it back — discard on error
                    cached_context = None;
                } else {
                    // Drain waitUntil promises in background.
                    // exec() returned at StreamsComplete (response already sent),
                    // but V8 needs pumping for background promises to resolve.
                    if let Err(reason) = cached.ctx.drain_waituntil().await {
                        tracing::debug!(
                            "context DISCARDED: worker={}, reason=drain_waituntil: {:?}",
                            &worker_id[..8.min(worker_id.len())],
                            reason
                        );
                        cached_context = None;
                    }
                }

                // Drop locker BEFORE writing back to inner (releases borrow on inner.isolate)
                drop(_js_lock);
                drop(locker);

                // Now safe to write cached_context back
                inner.cached_context = cached_context;
                drop(inner);

                release_to_local_pool(&isolate_arc);
                return result;
            }
            Err(e) => {
                tracing::debug!(
                    "context DISCARDED: worker={}, reason=reset_failed: {}",
                    &worker_id[..8.min(worker_id.len())],
                    e
                );
                cached_context = None;
                // Fall through to cold path
            }
        }
    }

    // Drop the old cached context (if any) before creating a new one
    drop(cached_context);

    // ── Cold path ──────────────────────────────────────────────────────
    tracing::debug!(
        "context COLD: worker={}, creating new context",
        &worker_id[..8.min(worker_id.len())]
    );

    let ctx_result = ExecutionContext::new_with_pooled_isolate(
        &mut locker,
        use_snapshot,
        platform,
        limits,
        memory_limit_hit,
        script,
        ops.clone(),
    );

    // Execute and determine whether to cache
    let (result, new_cached_context) = match ctx_result {
        Ok(mut ctx) => {
            let result = ctx.exec(task).await;

            if result.is_ok() {
                // Drain waitUntil promises before caching (V8 needs pumping)
                if let Err(reason) = ctx.drain_waituntil().await {
                    tracing::debug!(
                        "context NOT CACHED: worker={}, reason=drain_waituntil: {:?}",
                        &worker_id[..8.min(worker_id.len())],
                        reason
                    );
                    (Ok(()), None)
                } else {
                    (
                        result,
                        Some(CachedContext {
                            ctx,
                            worker_id: worker_id.to_string(),
                            version,
                            reuse_count: 1,
                            ops,
                        }),
                    )
                }
            } else {
                tracing::debug!(
                    "context NOT CACHED: worker={}, reason={:?}",
                    &worker_id[..8.min(worker_id.len())],
                    result.as_ref().err()
                );
                (result, None)
            }
        }
        Err(e) => (Err(e), None),
    };

    // Drop locker BEFORE writing back to inner (releases borrow on inner.isolate)
    drop(_js_lock);
    drop(locker);

    // Now safe to write cached_context back
    inner.cached_context = new_cached_context;
    drop(inner);

    // Release the isolate back to the pool (marks as available)
    release_to_local_pool(&isolate_arc);

    result
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tagged_isolate_acquire_release() {
        // Test atomic acquire/release using a real isolate
        let limits = openworkers_core::RuntimeLimits::default();
        let isolate = crate::locker_managed_isolate::LockerManagedIsolate::new(limits);

        let isolate_meta = TaggedIsolate {
            owner_id: "test_owner".to_string(),
            in_use: AtomicBool::new(false),
            inner: Mutex::new(TaggedIsolateInner {
                isolate,
                cached_context: None,
                created_at: Instant::now(),
                last_used: Instant::now(),
                total_requests: 0,
            }),
        };

        // Should be free initially
        assert!(isolate_meta.is_free());

        // First acquire should succeed
        assert!(isolate_meta.try_acquire());
        assert!(!isolate_meta.is_free());

        // Second acquire should fail (already in use)
        assert!(!isolate_meta.try_acquire());

        // Release should make it free again
        isolate_meta.release();
        assert!(isolate_meta.is_free());

        // Can acquire again after release
        assert!(isolate_meta.try_acquire());
    }
}
