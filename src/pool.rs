//! Isolate pool with per-owner isolation.
//!
//! An isolate serves `max_concurrent_per_isolate` requests at once. At 1 a
//! request has the isolate to itself; above 1 the requests share it and take
//! turns on the V8 Locker through the `AsyncWaiter` fair queue.
//!
//! - Thread-local pools (no global mutex contention)
//! - Owner-based isolation (worker_id or tenant_id)
//! - LRU eviction when pool is full
//! - Warm context caching for sub-ms request handling

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Mutex;

use crate::LockerManagedIsolate;
use crate::async_waiter::AsyncWaiter;
use crate::execution_context::ExecutionContext;
use crate::pool_common::{LocalPoolStats, PinnedExecuteRequest, PinnedPoolConfig, PinnedPoolStats};
use crate::pool_policy::{self, Acquire, ContextKey, IsolateLoad, Limits, PoolLoad, Refusal};
use crate::request_context::RequestContext;
use openworkers_core::{OperationsHandle, RuntimeLimits, TerminationReason};

/// The slots of one isolate and, when there is more than one, the queue that
/// orders their turns on the V8 Locker.
struct ConcurrencyState {
    active: AtomicUsize,
    max_concurrent: usize,
    /// A lone request never waits for the Locker, so a single slot has no
    /// queue and skips the fair-queue gate on every poll.
    async_waiter: Option<Arc<AsyncWaiter>>,
}

impl ConcurrencyState {
    fn new(max_concurrent: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            max_concurrent,
            async_waiter: (max_concurrent > 1).then(|| Arc::new(AsyncWaiter::new())),
        }
    }

    fn try_acquire(&self) -> bool {
        loop {
            let current = self.active.load(Ordering::Acquire);

            if current >= self.max_concurrent {
                return false;
            }

            if self
                .active
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn release(&self) {
        self.active.fetch_sub(1, Ordering::Release);
    }

    fn is_free(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }

    fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Acquire) < self.max_concurrent
    }

    fn async_waiter(&self) -> Option<Arc<AsyncWaiter>> {
        self.async_waiter.clone()
    }
}

// ============================================================================
// Configuration
// ============================================================================

/// Global configuration for the pool
static POOL_CONFIG: OnceLock<PinnedPoolConfig> = OnceLock::new();

/// Initialize the pool with the given configuration.
///
/// Must be called once at startup, before any pool access.
pub fn init_pinned_pool(config: PinnedPoolConfig) {
    assert!(
        config.max_concurrent_per_isolate >= 1,
        "max_concurrent_per_isolate must be at least 1"
    );
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
    isolate_ptr: v8::UnsafeRawIsolatePtr,
    worker_id: String,
    version: i32,
    reuse_count: u32,
    last_used: Instant,
    ops: OperationsHandle,
    env_updated_at: Option<i64>,
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
struct TaggedIsolate {
    /// Owner identifier (immutable after creation)
    owner_id: String,
    concurrency: ConcurrencyState,
    /// The V8 isolate, shared with every ExecutionContext that runs in it
    /// and entered through its Locker.
    isolate: Arc<LockerManagedIsolate>,
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
            isolate: Arc::new(lmi),
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

    /// What the policy sees. `try_lock` fails only while a request's own
    /// bookkeeping holds `inner`, and that request is not idle.
    fn load(&self) -> IsolateLoad<'_> {
        let idle_since = self
            .is_free()
            .then(|| self.inner.try_lock().ok().map(|inner| inner.last_used))
            .flatten();

        IsolateLoad {
            owner: &self.owner_id,
            has_capacity: self.concurrency.has_capacity(),
            idle_since,
        }
    }
}

impl Drop for TaggedIsolate {
    fn drop(&mut self) {
        // Drop the cached contexts under the lock so their v8::Global handles
        // are released now rather than deferred to an isolate that is going away.
        let _locker = self.isolate.isolate.lock();

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

/// Thread-local pool supporting per-owner isolation. The decisions are in
/// `pool_policy`; this performs them.
struct ThreadLocalPool {
    /// All isolates in this pool
    isolates: Vec<Arc<TaggedIsolate>>,
    policy: Limits,
    /// Runtime limits for new isolates
    limits: RuntimeLimits,
}

impl ThreadLocalPool {
    fn new(policy: Limits, limits: RuntimeLimits) -> Self {
        Self {
            isolates: Vec::with_capacity(policy.max_isolates),
            policy,
            limits,
        }
    }

    fn load(&self) -> PoolLoad<'_> {
        PoolLoad {
            isolates: self.isolates.iter().map(|isolate| isolate.load()).collect(),
            limits: self.policy,
        }
    }

    fn acquire(&mut self, owner_id: &str) -> Result<AcquireResult, Refusal> {
        match pool_policy::acquire(&self.load(), owner_id) {
            Acquire::Reuse(index) => {
                let isolate = Arc::clone(&self.isolates[index]);
                // The pool is thread-local, so nothing took the slot since the
                // snapshot.
                assert!(isolate.try_acquire());
                tracing::debug!(
                    "Cache HIT: acquired existing isolate for owner {}",
                    owner_id
                );

                Ok(AcquireResult {
                    isolate,
                    cache_hit: true,
                })
            }
            Acquire::Build => {
                tracing::debug!(
                    "Cache MISS: creating new isolate for owner {} (pool: {}/{})",
                    owner_id,
                    self.isolates.len() + 1,
                    self.policy.max_isolates,
                );

                Ok(self.build(owner_id))
            }
            Acquire::Evict(index) => {
                let old = self.isolates.remove(index);
                tracing::info!(
                    "LRU eviction: evicting isolate for owner {}, replacing with {}",
                    old.owner_id,
                    owner_id,
                );

                Ok(self.build(owner_id))
            }
            Acquire::Overcommit => {
                tracing::warn!(
                    "Pool overcommit: all {} isolates in use, creating extra for owner {}",
                    self.isolates.len(),
                    owner_id,
                );

                Ok(self.build(owner_id))
            }
            Acquire::Refuse(refusal) => {
                tracing::debug!("Pool refused owner {}: {}", owner_id, refusal.message());

                Err(refusal)
            }
        }
    }

    /// Build an isolate for the owner with its first slot taken.
    ///
    /// The Arc is not for threads: the pool is thread-local and the cached
    /// contexts hold Rc state. It lets execute_pinned keep the isolate across
    /// its awaits while the pool keeps its own handle.
    #[allow(clippy::arc_with_non_send_sync)]
    fn build(&mut self, owner_id: &str) -> AcquireResult {
        let config = get_config();
        let isolate = Arc::new(TaggedIsolate::new(
            owner_id.to_string(),
            self.limits.clone(),
            config.max_concurrent_per_isolate,
            config.max_cached_contexts,
        ));
        assert!(isolate.try_acquire());
        self.isolates.push(Arc::clone(&isolate));

        AcquireResult {
            isolate,
            cache_hit: false,
        }
    }

    /// Drop the idle isolates past the ceiling.
    fn reclaim(&mut self) {
        for index in pool_policy::reclaim(&self.load()).into_iter().rev() {
            let removed = self.isolates.remove(index);
            tracing::info!(
                "Cleanup: removed over-limit isolate for owner {}",
                removed.owner_id
            );
        }
    }

    fn stats(&self) -> LocalPoolStats {
        let total = self.isolates.len();
        let in_use = self.isolates.iter().filter(|i| !i.is_free()).count();
        let cached_contexts = self
            .isolates
            .iter()
            .filter_map(|i| i.inner.try_lock().ok())
            .map(|inner| inner.cached_contexts.len())
            .sum();

        LocalPoolStats {
            total,
            in_use,
            cached_contexts,
            capacity: self.policy.max_isolates,
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
                Limits {
                    max_isolates: config.max_per_thread,
                    max_per_owner: config.max_per_owner,
                    overcommit: config.overcommit,
                },
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

/// Acquire an isolate from the local pool, or the refusal that stops it.
fn acquire_from_local_pool(owner_id: &str) -> Result<(Arc<TaggedIsolate>, bool), Refusal> {
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
            pool.reclaim();
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

/// Drop a value under V8 Locker, so any v8::Global it holds is released
/// immediately instead of waiting for the next lock acquisition.
fn drop_under_lock<T>(value: T, pooled: &LockerManagedIsolate) {
    let _locker = pooled.isolate.lock();
    drop(value);
}

fn report(
    on_report: &mut Option<crate::ReportCallback>,
    pooled: &LockerManagedIsolate,
    marks: crate::ListenerMarks,
) {
    let Some(callback) = on_report.take() else {
        return;
    };

    callback(crate::EventReport {
        marks,
        heap_used_bytes: pooled.heap_used_bytes(),
    });
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
        abort,
        mut on_report,
    } = req;
    TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    // Ensure pool is initialized
    ensure_pool_initialized();

    // Fail-fast: the runner gets a refusal as TerminationReason::Other.
    let (isolate_arc, is_hit) = acquire_from_local_pool(&owner_id)
        .map_err(|refusal| TerminationReason::Other(refusal.message().to_string()))?;

    if is_hit {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    let pooled = Arc::clone(&isolate_arc.isolate);
    let (use_snapshot, platform, limits) =
        (pooled.use_snapshot, pooled.platform, pooled.limits.clone());

    // None when the isolate serves one request at a time
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

        let keys: Vec<(ContextKey, u32)> = inner
            .cached_contexts
            .iter()
            .map(|cached| {
                let key = ContextKey {
                    worker_id: &cached.worker_id,
                    version: cached.version,
                    env_updated_at: cached.env_updated_at,
                };
                (key, cached.reuse_count)
            })
            .collect();
        let wanted = ContextKey {
            worker_id: &worker_id,
            version,
            env_updated_at,
        };
        let found = pool_policy::warm_hit(&keys, wanted, get_config().max_context_reuses);

        found.map(|idx| inner.cached_contexts.swap_remove(idx))
        // MutexGuard dropped here
    };

    let warm_hit = cached_context.is_some();

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
            Arc::clone(&pooled),
            platform,
            limits.clone(),
            request,
            async_waiter.clone(),
        );

        if let Some(callback) = on_warm_hit {
            callback(&cached_ops);
        }

        // reset() acquires its own V8 lock internally
        match ec.reset() {
            Ok(()) => {
                ec.begin_request(abort);
                let result = ec.exec(task).await;
                report(&mut on_report, &pooled, ec.listener_marks());

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
                    let config = get_config();

                    pool_policy::keep_context(
                        reuse_cnt,
                        config.max_context_reuses,
                        config.max_cached_contexts,
                    )
                };

                if save_to_cache {
                    let mut inner = isolate_arc.inner.lock().await;

                    let last_used: Vec<Instant> =
                        inner.cached_contexts.iter().map(|c| c.last_used).collect();
                    let evicted = pool_policy::cache_evict(&last_used, inner.max_cached)
                        .map(|idx| inner.cached_contexts.swap_remove(idx));

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
                        drop_under_lock(evicted, &pooled);
                    }
                } else {
                    drop_under_lock(ec, &pooled);
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
                drop_under_lock(ec, &pooled);
                // Fall through to cold path
            }
        }
    }

    // ── Cold path ──────────────────────────────────────────────────────
    tracing::debug!(
        "context COLD: worker={}, creating new context",
        &worker_id[..8.min(worker_id.len())],
    );

    // Acquire the lock to enter the isolate and create the context
    let ctx_result = {
        let (mut locker, _js_lock) = pooled.lock();

        ExecutionContext::new_with_pooled_isolate(
            &mut locker,
            Arc::clone(&pooled),
            use_snapshot,
            platform,
            limits,
            script,
            ops.clone(),
        )
        // locker + js_lock dropped here — V8 mutex released
    };

    // Execute and determine whether to cache
    let (result, new_cached_context) = match ctx_result {
        Ok(mut ctx) => {
            ctx.async_waiter = async_waiter;
            ctx.begin_request(abort);
            let result = ctx.exec(task).await;
            report(&mut on_report, &pooled, ctx.listener_marks());

            let config = get_config();
            let keep =
                pool_policy::keep_context(1, config.max_context_reuses, config.max_cached_contexts);

            if result.is_ok() {
                match ctx.drain_waituntil().await {
                    Ok(()) if !keep => {
                        drop_under_lock(ctx, &pooled);
                        (result, None)
                    }
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
                        drop_under_lock(ctx, &pooled);
                        (Ok(()), None)
                    }
                }
            } else {
                tracing::debug!(
                    "context NOT CACHED: worker={}, reason={:?}",
                    &worker_id[..8.min(worker_id.len())],
                    result.as_ref().err(),
                );
                drop_under_lock(ctx, &pooled);
                (result, None)
            }
        }
        Err(e) => (Err(e), None),
    };

    // Return cached context to pool
    if let Some(cached) = new_cached_context {
        let mut inner = isolate_arc.inner.lock().await;

        let last_used: Vec<Instant> = inner.cached_contexts.iter().map(|c| c.last_used).collect();
        let evicted = pool_policy::cache_evict(&last_used, inner.max_cached)
            .map(|idx| inner.cached_contexts.swap_remove(idx));

        inner.cached_contexts.push(cached);
        drop(inner);

        if let Some(evicted) = evicted {
            drop_under_lock(evicted, &pooled);
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
            overcommit: true,
            max_context_reuses: 1000,
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

    #[test]
    fn test_tagged_isolate_multiplexing() {
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
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 2,
                max_per_owner: None,
                overcommit: true,
            },
            limits,
        );

        // Fill pool with 2 different owners
        let a = pool.acquire("owner_a");
        assert!(!a.unwrap().cache_hit);

        let b = pool.acquire("owner_b");
        assert!(!b.unwrap().cache_hit);

        assert_eq!(pool.isolates.len(), 2);

        // Release both (a was acquired first → older last_used)
        pool.isolates[0].release();
        // Small delay so last_used differs
        std::thread::sleep(std::time::Duration::from_millis(2));
        pool.isolates[1].release();

        // Acquire for new owner_c → should evict owner_a (LRU)
        let c = pool.acquire("owner_c");
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
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 2,
                max_per_owner: None,
                overcommit: true,
            },
            limits,
        );

        // Fill pool and keep acquired (in use)
        assert!(pool.acquire("owner_a").is_ok());
        assert!(pool.acquire("owner_b").is_ok());
        assert_eq!(pool.isolates.len(), 2);

        // All in use — overcommit should create a 3rd
        assert!(pool.acquire("owner_c").is_ok());
        assert_eq!(pool.isolates.len(), 3); // Over limit
    }

    #[test]
    fn test_pool_refuses_when_full_without_overcommit() {
        ensure_test_config();
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 1,
                max_per_owner: None,
                overcommit: false,
            },
            limits,
        );

        assert!(pool.acquire("owner_a").is_ok());
        assert!(matches!(pool.acquire("owner_b"), Err(Refusal::PoolFull)));
        assert_eq!(pool.isolates.len(), 1);

        // The slot frees, and the next owner evicts instead of overcommitting
        pool.isolates[0].release();
        assert!(pool.acquire("owner_b").is_ok());
        assert_eq!(pool.isolates.len(), 1);
    }

    #[test]
    fn test_pool_owner_limit() {
        ensure_test_config();
        // max_per_owner = 1: second acquire for same owner should fail when in use
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 10,
                max_per_owner: Some(1),
                overcommit: true,
            },
            limits,
        );

        assert!(pool.acquire("owner_a").is_ok());

        // Same owner, at limit, isolate in use
        assert!(matches!(
            pool.acquire("owner_a"),
            Err(Refusal::OwnerAtLimit)
        ));

        // Different owner works fine
        assert!(pool.acquire("owner_b").is_ok());
    }

    #[test]
    fn test_pool_cache_hit_on_reacquire() {
        ensure_test_config();
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 10,
                max_per_owner: None,
                overcommit: true,
            },
            limits,
        );

        // Acquire and release
        let a = pool.acquire("owner_a");
        assert!(!a.unwrap().cache_hit); // First time = miss
        pool.isolates[0].release();

        // Re-acquire same owner → cache hit
        let a2 = pool.acquire("owner_a");
        assert!(a2.unwrap().cache_hit);
    }

    #[test]
    fn test_pool_stats() {
        ensure_test_config();
        let limits = openworkers_core::RuntimeLimits::default();
        let mut pool = ThreadLocalPool::new(
            Limits {
                max_isolates: 5,
                max_per_owner: None,
                overcommit: true,
            },
            limits,
        );

        let stats = pool.stats();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.in_use, 0);
        assert_eq!(stats.capacity, 5);

        pool.acquire("owner_a").ok();
        pool.acquire("owner_b").ok();

        let stats = pool.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.in_use, 2);

        pool.isolates[0].release();

        let stats = pool.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.in_use, 1);
    }
}
