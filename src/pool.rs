//! Isolate pool with per-owner isolation.
//!
//! Two modes selected at compile time:
//!
//! - **Simple** (default): 1 request per isolate (exclusive `AtomicBool`).
//! - **Multiplexed** (`--features multiplexing`): N concurrent requests per
//!   isolate via `AtomicUsize` + `AsyncWaiter` fair FIFO queue.
//!
//! Common features (both modes):
//! - Thread-local pools (no global mutex contention)
//! - Owner-based isolation (worker_id or tenant_id)
//! - LRU eviction when pool is full
//! - Warm context caching for sub-ms request handling

use std::cell::UnsafeCell;
#[cfg(not(feature = "multiplexing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Mutex;

use crate::LockerManagedIsolate;
use crate::execution_context::ExecutionContext;
use crate::gc::JsLock;
use crate::pool_common::{LocalPoolStats, PinnedExecuteRequest, PinnedPoolConfig, PinnedPoolStats};
use crate::request_context::RequestContext;
use openworkers_core::{OperationsHandle, RuntimeLimits, TerminationReason};

// ============================================================================
// Concurrency State (cfg-gated)
// ============================================================================

/// Simple mode: exclusive access via AtomicBool (1 request per isolate).
#[cfg(not(feature = "multiplexing"))]
struct ConcurrencyState {
    in_use: AtomicBool,
}

#[cfg(not(feature = "multiplexing"))]
impl ConcurrencyState {
    fn new(_max_concurrent: usize) -> Self {
        Self {
            in_use: AtomicBool::new(false),
        }
    }

    fn try_acquire(&self) -> bool {
        self.in_use
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    fn release(&self) {
        self.in_use.store(false, Ordering::Release);
    }

    fn is_free(&self) -> bool {
        !self.in_use.load(Ordering::Acquire)
    }

    fn async_waiter(&self) -> Option<Arc<crate::async_waiter::AsyncWaiter>> {
        None
    }
}

/// Multiplexed mode: N concurrent requests via AtomicUsize + AsyncWaiter fair queue.
#[cfg(feature = "multiplexing")]
struct ConcurrencyState {
    active_count: AtomicUsize,
    max_concurrent: usize,
    async_waiter: Arc<crate::async_waiter::AsyncWaiter>,
}

#[cfg(feature = "multiplexing")]
impl ConcurrencyState {
    fn new(max_concurrent: usize) -> Self {
        Self {
            active_count: AtomicUsize::new(0),
            max_concurrent,
            async_waiter: Arc::new(crate::async_waiter::AsyncWaiter::new()),
        }
    }

    fn try_acquire(&self) -> bool {
        loop {
            let current = self.active_count.load(Ordering::Acquire);

            if current >= self.max_concurrent {
                return false;
            }

            if self
                .active_count
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn release(&self) {
        self.active_count.fetch_sub(1, Ordering::Release);
    }

    fn is_free(&self) -> bool {
        self.active_count.load(Ordering::Acquire) == 0
    }

    fn async_waiter(&self) -> Option<Arc<crate::async_waiter::AsyncWaiter>> {
        Some(Arc::clone(&self.async_waiter))
    }
}

/// Default max reuses before discarding a cached context (prevents memory growth)
const DEFAULT_CONTEXT_MAX_REUSES: u32 = 1000;

// ============================================================================
// Configuration
// ============================================================================

/// Global configuration for the pool
static POOL_CONFIG: OnceLock<PinnedPoolConfig> = OnceLock::new();

/// Initialize the pool with the given configuration.
///
/// Must be called once at startup, before any pool access.
pub fn init_pinned_pool(config: PinnedPoolConfig) {
    let max_per_thread = config.max_per_thread;
    let max_per_owner = config.max_per_owner;
    let max_concurrent = config.max_concurrent_per_isolate;

    if POOL_CONFIG.set(config).is_err() {
        tracing::warn!("Pool already initialized");
    } else {
        tracing::info!(
            "Pool initialized: max_per_thread={}, max_per_owner={:?}, max_concurrent={}",
            max_per_thread,
            max_per_owner,
            max_concurrent,
        );
    }
}

fn get_config() -> &'static PinnedPoolConfig {
    POOL_CONFIG
        .get()
        .expect("Pool not initialized. Call init_pinned_pool() at startup.")
}

// ============================================================================
// Thread-Local Pool with Per-Owner Isolation
// ============================================================================

/// A cached per-request context for warm isolate reuse.
///
/// When a request succeeds, the RequestContext (V8 context + event loop) is
/// cached here for the next request to the same worker+version. On warm hit,
/// we reconstruct an ExecutionContext from cached parts, call `reset()`, and
/// dispatch the event directly, skipping full context creation.
struct CachedContext {
    request: RequestContext,
    /// Raw pointer to the isolate (cached from original creation for warm reuse)
    isolate_ptr: *mut v8::OwnedIsolate,
    worker_id: String,
    version: i32,
    reuse_count: u32,
    last_used: Instant,
    ops: OperationsHandle,
    env_updated_at: Option<i64>,
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

/// Inner data for a pooled isolate (behind Mutex)
struct TaggedIsolateInner {
    /// Pool of cached contexts for warm reuse
    cached_contexts: Vec<CachedContext>,
    /// Maximum cached contexts to keep per isolate
    max_cached: usize,
    #[allow(dead_code)]
    created_at: Instant,
    last_used: Instant,
    total_requests: u64,
}

/// A tagged isolate with owner tracking and concurrency control.
///
/// Each isolate is tagged with an owner_id. Concurrency is managed by
/// `ConcurrencyState` (exclusive `AtomicBool` or multiplexed `AtomicUsize`).
struct TaggedIsolate {
    /// Owner identifier (immutable after creation)
    owner_id: String,
    /// Concurrency control (cfg-gated: exclusive or multiplexed)
    concurrency: ConcurrencyState,
    /// The V8 isolate (unentered, requires Locker for access).
    /// Behind UnsafeCell for raw pointer access without Mutex aliasing.
    /// SAFETY: Concurrency state prevents eviction while in use. Only
    /// accessed on the thread-local pool's owning thread.
    isolate: UnsafeCell<LockerManagedIsolate>,
    /// Mutable per-request bookkeeping
    inner: Mutex<TaggedIsolateInner>,
}

impl TaggedIsolate {
    fn new(
        owner_id: String,
        limits: RuntimeLimits,
        max_concurrent: usize,
        max_cached: usize,
    ) -> Self {
        tracing::debug!("Creating new TaggedIsolate for owner: {}", owner_id);
        let start = Instant::now();

        let lmi = LockerManagedIsolate::new(limits);
        let duration = start.elapsed();

        tracing::info!(
            "TaggedIsolate created for owner {} in {:?} (snapshot: {}, max_concurrent: {})",
            owner_id,
            duration,
            lmi.use_snapshot,
            max_concurrent,
        );

        let now = Instant::now();
        Self {
            owner_id,
            concurrency: ConcurrencyState::new(max_concurrent),
            isolate: UnsafeCell::new(lmi),
            inner: Mutex::new(TaggedIsolateInner {
                cached_contexts: Vec::with_capacity(max_cached),
                max_cached,
                created_at: now,
                last_used: now,
                total_requests: 0,
            }),
        }
    }

    /// Try to acquire a slot on this isolate.
    fn try_acquire(&self) -> bool {
        self.concurrency.try_acquire()
    }

    /// Release a slot back to the pool.
    fn release(&self) {
        self.concurrency.release();
    }

    /// Check if this isolate has zero active requests.
    fn is_free(&self) -> bool {
        self.concurrency.is_free()
    }
}

impl Drop for TaggedIsolate {
    fn drop(&mut self) {
        // Acquire Locker to safely drop cached V8 contexts and process
        // deferred destructions. Without this, v8::Global handles in
        // CachedContext would drop without the V8 lock held.
        let lmi = self.isolate.get_mut();
        let _locker = v8::Locker::new(&mut lmi.isolate);
        lmi.deferred_destruction_queue.process_all();

        if let Ok(mut inner) = self.inner.try_lock() {
            inner.cached_contexts.clear();
        }
    }
}

/// Result of an acquire operation
struct AcquireResult {
    /// The acquired isolate
    isolate: Arc<TaggedIsolate>,
    /// Whether this was a cache hit (existing isolate with same owner)
    cache_hit: bool,
}

/// Thread-local pool supporting per-owner isolation.
struct ThreadLocalPool {
    /// All isolates in this pool
    isolates: Vec<Arc<TaggedIsolate>>,
    /// Maximum isolates in this pool
    max_isolates: usize,
    /// Maximum isolates per owner (None = no limit)
    max_per_owner: Option<usize>,
    /// Runtime limits for new isolates
    limits: RuntimeLimits,
}

impl ThreadLocalPool {
    fn new(max_isolates: usize, max_per_owner: Option<usize>, limits: RuntimeLimits) -> Self {
        Self {
            isolates: Vec::with_capacity(max_isolates),
            max_isolates,
            max_per_owner,
            limits,
        }
    }

    /// Count isolates for a given owner
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
    /// 1. Find an isolate with capacity for the same owner
    /// 2. Create new isolate if under pool/owner limit
    /// 3. Evict LRU free isolate if pool is full
    /// 4. Overcommit if all isolates are in use
    #[allow(clippy::arc_with_non_send_sync)]
    fn acquire(&mut self, owner_id: &str) -> Option<AcquireResult> {
        let config = get_config();

        // 1. Try to find an isolate with capacity for the same owner
        for arc in &self.isolates {
            if arc.owner_id == owner_id && arc.try_acquire() {
                tracing::debug!(
                    "Cache HIT: acquired existing isolate for owner {}",
                    owner_id,
                );
                return Some(AcquireResult {
                    isolate: Arc::clone(arc),
                    cache_hit: true,
                });
            }
        }

        // Check if owner has reached their isolate limit
        if self.owner_at_limit(owner_id) {
            tracing::debug!(
                "Owner {} at isolate limit ({:?}), all at capacity",
                owner_id,
                self.max_per_owner,
            );
            return None;
        }

        // 2. Create new isolate if under pool limit
        if self.isolates.len() < self.max_isolates {
            tracing::debug!(
                "Cache MISS: creating new isolate for owner {} (pool: {}/{}, owner: {}/{:?})",
                owner_id,
                self.isolates.len() + 1,
                self.max_isolates,
                self.count_for_owner(owner_id) + 1,
                self.max_per_owner,
            );

            let entry = Arc::new(TaggedIsolate::new(
                owner_id.to_string(),
                self.limits.clone(),
                config.max_concurrent_per_isolate,
                config.max_cached_contexts,
            ));
            entry.try_acquire();
            self.isolates.push(Arc::clone(&entry));

            return Some(AcquireResult {
                isolate: entry,
                cache_hit: false,
            });
        }

        // 3. Pool full — evict LRU (oldest last_used among FREE isolates)
        let mut lru_idx: Option<usize> = None;
        let mut oldest = Instant::now();

        for (i, arc) in self.isolates.iter().enumerate() {
            if arc.is_free()
                && let Ok(guard) = arc.inner.try_lock()
                && guard.last_used < oldest
            {
                oldest = guard.last_used;
                lru_idx = Some(i);
            }
        }

        if let Some(idx) = lru_idx {
            let old = self.isolates.remove(idx);
            let evicted_owner = old.owner_id.clone();
            tracing::info!(
                "LRU eviction: evicting isolate for owner {}, replacing with {}",
                evicted_owner,
                owner_id,
            );

            let entry = Arc::new(TaggedIsolate::new(
                owner_id.to_string(),
                self.limits.clone(),
                config.max_concurrent_per_isolate,
                config.max_cached_contexts,
            ));
            entry.try_acquire();
            self.isolates.push(Arc::clone(&entry));

            return Some(AcquireResult {
                isolate: entry,
                cache_hit: false,
            });
        }

        // 4. All isolates in use — overcommit (temporary over-limit)
        tracing::warn!(
            "Pool overcommit: all {} isolates in use, creating extra for owner {}",
            self.isolates.len(),
            owner_id,
        );

        let entry = Arc::new(TaggedIsolate::new(
            owner_id.to_string(),
            self.limits.clone(),
            config.max_concurrent_per_isolate,
            config.max_cached_contexts,
        ));
        entry.try_acquire();
        self.isolates.push(Arc::clone(&entry));

        Some(AcquireResult {
            isolate: entry,
            cache_hit: false,
        })
    }

    /// Clean up over-limit isolates that are free.
    /// Called after releasing to reclaim overcommitted isolates.
    fn cleanup_overlimit(&mut self) {
        if self.isolates.len() <= self.max_isolates {
            return;
        }

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
                config.limits.clone(),
            ));
            tracing::debug!(
                "Thread-local pool initialized on thread {:?} (max_per_owner={:?})",
                std::thread::current().id(),
                config.max_per_owner,
            );
        }
    });
}

/// Acquire an isolate from the local pool.
///
/// Returns `Some((isolate, cache_hit))` if successful, or `None` if at capacity.
fn acquire_from_local_pool(owner_id: &str) -> Option<(Arc<TaggedIsolate>, bool)> {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();
        let pool = pool_opt.as_mut().expect("Pool not initialized");

        pool.acquire(owner_id)
            .map(|result| (result.isolate, result.cache_hit))
    })
}

/// Release an isolate back to the pool and reclaim overcommitted isolates.
fn release_to_local_pool(isolate: &TaggedIsolate) {
    isolate.release();

    // Reclaim any overcommitted isolates that are now free
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();

        if let Some(pool) = pool_opt.as_mut() {
            pool.cleanup_overlimit();
        }
    });
}

// ============================================================================
// Statistics
// ============================================================================

static TOTAL_REQUESTS: AtomicUsize = AtomicUsize::new(0);
static CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
static CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);

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

/// Drop a value under V8 Locker.
///
/// V8 Global handles must be destroyed while the Locker is held.
/// This helper acquires the Locker, processes deferred destructions,
/// then drops the value (which may contain V8 globals).
fn drop_under_lock<T>(value: T, lmi_ptr: *mut LockerManagedIsolate) {
    if lmi_ptr.is_null() {
        drop(value);
        return;
    }

    unsafe {
        let lmi = &mut *lmi_ptr;
        let _locker = v8::Locker::new(&mut lmi.isolate);
        lmi.deferred_destruction_queue.process_all();
        drop(value);
    }
}

/// Execute a worker script using the isolate pool.
///
/// This uses thread-local storage for zero-contention access to isolates.
///
/// ## Warm Isolates
///
/// When the same worker_id+version is executed on the same isolate, the context
/// is reused (warm hit). This skips full context creation and only resets
/// per-request state.
///
/// ## Fail-fast
///
/// If no isolate is available (owner at limit, pool at capacity), returns
/// `Err(TerminationReason::Other("Pool at capacity"))` immediately.
/// The caller (runner) is responsible for queuing/backpressure.
pub async fn execute_pinned(req: PinnedExecuteRequest) -> Result<(), TerminationReason> {
    let PinnedExecuteRequest {
        owner_id,
        worker_id,
        version,
        script,
        ops,
        task,
        on_warm_hit,
        env_updated_at,
    } = req;
    TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    // Ensure pool is initialized
    ensure_pool_initialized();

    // Fail-fast: try to acquire an isolate, return error if at capacity
    let (isolate_arc, is_hit) = acquire_from_local_pool(&owner_id)
        .ok_or_else(|| TerminationReason::Other("Pool at capacity".to_string()))?;

    if is_hit {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    // Get raw pointer to isolate via UnsafeCell (no Mutex aliasing).
    // SAFETY: Concurrency state prevents eviction. Arc keeps memory alive.
    // Single-thread access via LocalSet.
    let lmi_ptr = isolate_arc.isolate.get();

    // Read immutable metadata from the isolate (no Mutex needed)
    let (use_snapshot, platform, limits, memory_limit_hit) = unsafe {
        let lmi = &*lmi_ptr;
        (
            lmi.use_snapshot,
            lmi.platform,
            lmi.limits.clone(),
            Arc::clone(&lmi.memory_limit_hit),
        )
    };

    // Get async_waiter from concurrency state (None for simple, Some for multiplexed)
    let async_waiter = isolate_arc.concurrency.async_waiter();

    // Lock inner briefly for bookkeeping and cached context lookup
    let cached_context = {
        let mut inner = isolate_arc.inner.lock().await;
        inner.total_requests += 1;
        inner.last_used = Instant::now();

        tracing::trace!(
            "Acquired isolate for owner: {} (requests: {})",
            owner_id,
            inner.total_requests,
        );

        // ── Warm hit check ─────────────────────────────────────────────
        let max_reuses = context_max_reuses();
        let mut found_idx = None;

        for (i, cached) in inner.cached_contexts.iter().enumerate() {
            if cached.worker_id == worker_id
                && cached.version == version
                && cached.env_updated_at == env_updated_at
                && cached.reuse_count < max_reuses
            {
                found_idx = Some(i);
                break;
            }
        }

        found_idx.map(|idx| inner.cached_contexts.swap_remove(idx))
        // MutexGuard dropped here
    };

    let warm_hit = cached_context.is_some();

    // Reset memory limit flag
    memory_limit_hit.store(false, Ordering::SeqCst);

    // ── Warm hit path ──────────────────────────────────────────────────
    if warm_hit {
        let CachedContext {
            request,
            isolate_ptr: cached_isolate_ptr,
            worker_id: cached_wid,
            version: cached_ver,
            reuse_count: mut reuse_cnt,
            ops: cached_ops,
            env_updated_at: cached_env_ts,
            ..
        } = cached_context.unwrap();

        reuse_cnt += 1;

        tracing::debug!(
            "context WARM: worker={}, version={}, reuse_count={}",
            &worker_id[..8.min(worker_id.len())],
            version,
            reuse_cnt,
        );

        // Reconstruct EC from cached parts
        let mut ec = ExecutionContext::from_cached(
            cached_isolate_ptr,
            lmi_ptr,
            platform,
            limits.clone(),
            memory_limit_hit.clone(),
            request,
            async_waiter.clone(),
        );

        if let Some(callback) = on_warm_hit {
            callback(&cached_ops);
        }

        // reset() acquires its own V8 lock internally
        match ec.reset() {
            Ok(()) => {
                let result = ec.exec(task).await;

                let save_to_cache = if result.is_err() {
                    tracing::debug!(
                        "context DISCARDED: worker={}, reason={:?}",
                        &worker_id[..8.min(worker_id.len())],
                        result.as_ref().err(),
                    );
                    false
                } else if let Err(reason) = ec.drain_waituntil().await {
                    tracing::debug!(
                        "context DISCARDED: worker={}, reason=drain_waituntil: {:?}",
                        &worker_id[..8.min(worker_id.len())],
                        reason,
                    );
                    false
                } else {
                    true
                };

                if save_to_cache {
                    let mut inner = isolate_arc.inner.lock().await;

                    let evicted = if inner.cached_contexts.len() >= inner.max_cached {
                        // LRU eviction: replace least recently used context
                        inner
                            .cached_contexts
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, c)| c.last_used)
                            .map(|(i, _)| i)
                            .map(|idx| inner.cached_contexts.swap_remove(idx))
                    } else {
                        None
                    };

                    let (request, isolate_ptr) = ec.into_parts();
                    inner.cached_contexts.push(CachedContext {
                        request,
                        isolate_ptr,
                        worker_id: cached_wid,
                        version: cached_ver,
                        reuse_count: reuse_cnt,
                        last_used: Instant::now(),
                        ops: cached_ops,
                        env_updated_at: cached_env_ts,
                    });
                    drop(inner);

                    if let Some(evicted) = evicted {
                        drop_under_lock(evicted, lmi_ptr);
                    }
                } else {
                    drop_under_lock(ec, lmi_ptr);
                }

                release_to_local_pool(&isolate_arc);
                return result;
            }
            Err(e) => {
                tracing::debug!(
                    "context DISCARDED: worker={}, reason=reset_failed: {}",
                    &worker_id[..8.min(worker_id.len())],
                    e,
                );
                drop_under_lock(ec, lmi_ptr);
                // Fall through to cold path
            }
        }
    }

    // ── Cold path ──────────────────────────────────────────────────────
    tracing::debug!(
        "context COLD: worker={}, creating new context",
        &worker_id[..8.min(worker_id.len())],
    );

    // Acquire Locker to enter the unentered isolate, create context
    let ctx_result = {
        let lmi = unsafe { &mut *lmi_ptr };
        let mut locker = v8::Locker::new(&mut lmi.isolate);
        lmi.deferred_destruction_queue.process_all();
        let _js_lock = JsLock::new(&mut locker);

        ExecutionContext::new_with_pooled_isolate(
            &mut locker,
            lmi_ptr,
            use_snapshot,
            platform,
            limits,
            memory_limit_hit,
            script,
            ops.clone(),
        )
        // locker + js_lock dropped here — V8 mutex released
    };

    // Execute and determine whether to cache
    let (result, new_cached_context) = match ctx_result {
        Ok(mut ctx) => {
            ctx.async_waiter = async_waiter;
            let result = ctx.exec(task).await;

            if result.is_ok() {
                match ctx.drain_waituntil().await {
                    Ok(()) => {
                        let (request, isolate_ptr) = ctx.into_parts();
                        (
                            result,
                            Some(CachedContext {
                                request,
                                isolate_ptr,
                                worker_id: worker_id.to_string(),
                                version,
                                reuse_count: 1,
                                last_used: Instant::now(),
                                ops,
                                env_updated_at,
                            }),
                        )
                    }
                    Err(reason) => {
                        tracing::debug!(
                            "context NOT CACHED: worker={}, reason=drain_waituntil: {:?}",
                            &worker_id[..8.min(worker_id.len())],
                            reason,
                        );
                        drop_under_lock(ctx, lmi_ptr);
                        (Ok(()), None)
                    }
                }
            } else {
                tracing::debug!(
                    "context NOT CACHED: worker={}, reason={:?}",
                    &worker_id[..8.min(worker_id.len())],
                    result.as_ref().err(),
                );
                drop_under_lock(ctx, lmi_ptr);
                (result, None)
            }
        }
        Err(e) => (Err(e), None),
    };

    // Return cached context to pool
    if let Some(cached) = new_cached_context {
        let mut inner = isolate_arc.inner.lock().await;

        let evicted = if inner.cached_contexts.len() >= inner.max_cached {
            // LRU eviction: replace least recently used context
            inner
                .cached_contexts
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(i, _)| i)
                .map(|idx| inner.cached_contexts.swap_remove(idx))
        } else {
            None
        };

        inner.cached_contexts.push(cached);
        drop(inner);

        if let Some(evicted) = evicted {
            drop_under_lock(evicted, lmi_ptr);
        }
    }

    // Release the isolate back to the pool
    release_to_local_pool(&isolate_arc);

    result
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool_common::PinnedPoolConfig;

    /// Ensure pool config is initialized for tests that call acquire()
    fn ensure_test_config() {
        let _ = POOL_CONFIG.set(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 1,
            max_cached_contexts: 5,
            limits: openworkers_core::RuntimeLimits::default(),
        });
    }

    #[test]
    fn test_tagged_isolate_acquire_release() {
        let limits = openworkers_core::RuntimeLimits::default();
        let isolate = TaggedIsolate::new("test_owner".to_string(), limits, 1, 10);

        // Should be free initially
        assert!(isolate.is_free());

        // First acquire should succeed
        assert!(isolate.try_acquire());
        assert!(!isolate.is_free());

        // Second acquire should fail (max_concurrent=1)
        assert!(!isolate.try_acquire());

        // Release should make it free again
        isolate.release();
        assert!(isolate.is_free());

        // Can acquire again after release
        assert!(isolate.try_acquire());
    }

    #[cfg(feature = "multiplexing")]
    #[test]
    fn test_tagged_isolate_multiplexing() {
        // Test that max_concurrent > 1 allows multiple concurrent acquires
        let limits = openworkers_core::RuntimeLimits::default();
        let isolate = TaggedIsolate::new("test_owner".to_string(), limits, 3, 10);

        // Should acquire 3 times
        assert!(isolate.try_acquire());
        assert!(isolate.try_acquire());
        assert!(isolate.try_acquire());
        assert!(!isolate.is_free());

        // 4th should fail
        assert!(!isolate.try_acquire());

        // Release one, should be able to acquire again
        isolate.release();
        assert!(isolate.try_acquire());

        // Release all
        isolate.release();
        isolate.release();
        isolate.release();
        assert!(isolate.is_free());
    }

    #[test]
    fn test_pool_lru_eviction() {
        ensure_test_config();
        // Pool of size 2: when full, LRU free isolate should be evicted
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(2, None, limits);

        // Fill pool with 2 different owners
        let a = pool.acquire("owner_a");
        assert!(a.is_some());
        assert!(!a.unwrap().cache_hit);

        let b = pool.acquire("owner_b");
        assert!(b.is_some());
        assert!(!b.unwrap().cache_hit);

        assert_eq!(pool.isolates.len(), 2);

        // Release both (a was acquired first → older last_used)
        pool.isolates[0].release();
        // Small delay so last_used differs
        std::thread::sleep(std::time::Duration::from_millis(2));
        pool.isolates[1].release();

        // Acquire for new owner_c → should evict owner_a (LRU)
        let c = pool.acquire("owner_c");
        assert!(c.is_some());
        assert!(!c.unwrap().cache_hit);
        assert_eq!(pool.isolates.len(), 2); // Still at capacity

        // owner_a should be gone, owner_b and owner_c remain
        let owners: Vec<&str> = pool.isolates.iter().map(|i| i.owner_id.as_str()).collect();
        assert!(owners.contains(&"owner_b"));
        assert!(owners.contains(&"owner_c"));
        assert!(!owners.contains(&"owner_a"));
    }

    #[test]
    fn test_pool_overcommit_when_all_in_use() {
        ensure_test_config();
        // Pool of size 2, both in use → should overcommit
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(2, None, limits);

        // Fill pool and keep acquired (in use)
        let a = pool.acquire("owner_a");
        assert!(a.is_some());

        let b = pool.acquire("owner_b");
        assert!(b.is_some());

        assert_eq!(pool.isolates.len(), 2);

        // All in use — overcommit should create a 3rd
        let c = pool.acquire("owner_c");
        assert!(c.is_some());
        assert_eq!(pool.isolates.len(), 3); // Over limit
    }

    #[test]
    fn test_pool_owner_limit() {
        ensure_test_config();
        // max_per_owner = 1: second acquire for same owner should fail when in use
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(10, Some(1), limits);

        let a = pool.acquire("owner_a");
        assert!(a.is_some());

        // Same owner, at limit, isolate in use → returns None
        let a2 = pool.acquire("owner_a");
        assert!(a2.is_none());

        // Different owner works fine
        let b = pool.acquire("owner_b");
        assert!(b.is_some());
    }

    #[test]
    fn test_pool_cache_hit_on_reacquire() {
        ensure_test_config();
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(10, None, limits);

        // Acquire and release
        let a = pool.acquire("owner_a");
        assert!(a.is_some());
        assert!(!a.unwrap().cache_hit); // First time = miss
        pool.isolates[0].release();

        // Re-acquire same owner → cache hit
        let a2 = pool.acquire("owner_a");
        assert!(a2.is_some());
        assert!(a2.unwrap().cache_hit);
    }

    #[test]
    fn test_pool_stats() {
        ensure_test_config();
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(5, None, limits);

        let stats = pool.stats();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.in_use, 0);
        assert_eq!(stats.capacity, 5);

        pool.acquire("owner_a");
        pool.acquire("owner_b");

        let stats = pool.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.in_use, 2);

        pool.isolates[0].release();

        let stats = pool.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.in_use, 1);
    }
}
