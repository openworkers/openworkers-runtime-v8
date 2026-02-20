//! Thread-safe isolate pool with LRU eviction.
//!
//! Uses v8::Locker for safe multi-threaded access to isolates.

use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Mutex;

use crate::LockerManagedIsolate;
use crate::gc::JsLock;
use openworkers_core::RuntimeLimits;

/// Global isolate pool instance
static ISOLATE_POOL: OnceLock<IsolatePool> = OnceLock::new();

type WorkerId = String;

/// Entry in the pool (isolate + metadata)
struct IsolateEntry {
    isolate: LockerManagedIsolate,
    #[allow(dead_code)]
    created_at: Instant,
    total_requests: u64,
}

impl IsolateEntry {
    fn new(limits: RuntimeLimits) -> Self {
        tracing::debug!("Creating new LockerManagedIsolate");
        let start = Instant::now();
        let isolate = LockerManagedIsolate::new(limits);
        let duration = start.elapsed();
        tracing::info!(
            "LockerManagedIsolate created in {:?} (snapshot: {})",
            duration,
            isolate.use_snapshot
        );

        Self {
            isolate,
            created_at: Instant::now(),
            total_requests: 0,
        }
    }
}

/// Global pool of isolates with LRU eviction
///
/// This pool maintains a cache of isolates keyed by worker_id.
/// When the pool is full, the least recently used isolate is evicted.
pub struct IsolatePool {
    /// LRU cache: worker_id → isolate entry
    cache: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
    /// Max isolates in pool
    max_size: usize,
    /// Limits for new isolates
    limits: RuntimeLimits,
}

impl IsolatePool {
    /// Create new isolate pool
    ///
    /// # Arguments
    /// * `max_size` - Maximum number of isolates to cache (e.g., 1000)
    /// * `limits` - Runtime limits for isolates (heap size, etc.)
    pub fn new(max_size: usize, limits: RuntimeLimits) -> Self {
        tracing::info!(
            "Initializing IsolatePool with max_size={}, heap_max={}MB",
            max_size,
            limits.heap_max_mb
        );

        Self {
            cache: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).unwrap(),
            ))),
            max_size,
            limits,
        }
    }

    /// Acquire isolate for a worker (or create if not exists)
    ///
    /// This method:
    /// 1. Locks the cache
    /// 2. Gets existing entry or creates new
    /// 3. Unlocks cache
    /// 4. Returns PooledIsolate guard
    ///
    /// The returned PooledIsolate can be used to lock the isolate
    /// for the current thread using v8::Locker.
    pub async fn acquire(&self, worker_id: &str) -> PooledIsolate {
        let mut cache = self.cache.lock().await;

        // Try to get from cache
        let entry = if let Some(entry) = cache.get(worker_id) {
            tracing::debug!("Isolate cache HIT for worker {}", worker_id);
            Arc::clone(entry)
        } else {
            tracing::debug!("Isolate cache MISS for worker {}, creating new", worker_id);

            // Create new isolate entry
            let entry = Arc::new(Mutex::new(IsolateEntry::new(self.limits.clone())));

            // Insert into LRU cache
            // If cache is full, LRU will evict the least recently used
            if let Some((evicted_worker_id, _)) =
                cache.push(worker_id.to_string(), Arc::clone(&entry))
            {
                tracing::info!(
                    "Isolate LRU eviction: worker {} evicted (cache full at {})",
                    evicted_worker_id,
                    self.max_size
                );

                // Evicted entry will be dropped when Arc refcount reaches 0
                // If it's still in use (PooledIsolate exists), drop is deferred
            }

            entry
        };

        // Unlock cache before returning guard
        drop(cache);

        PooledIsolate {
            entry,
            worker_id: worker_id.to_string(),
            pool: Arc::clone(&self.cache),
        }
    }

    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        let cache = self.cache.lock().await;
        PoolStats {
            total: self.max_size,
            cached: cache.len(),
            capacity: cache.cap().get(),
        }
    }
}

/// RAII guard for isolate usage
///
/// This guard provides safe access to an isolate from the pool.
/// When dropped, it marks the isolate as recently used in the LRU cache.
pub struct PooledIsolate {
    entry: Arc<Mutex<IsolateEntry>>,
    worker_id: WorkerId,
    pool: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
}

impl PooledIsolate {
    /// Execute closure with locked isolate (blocking)
    ///
    /// Creates v8::Locker (which handles enter/exit automatically), calls closure, drops locker.
    /// This is a blocking operation - use with_lock_async for async code.
    pub fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut v8::Isolate) -> R,
    {
        tracing::trace!("Acquiring entry lock for worker {}", self.worker_id);
        let mut entry = self.entry.blocking_lock();

        // Update stats before creating Locker (to avoid double borrow)
        entry.total_requests += 1;

        tracing::trace!(
            "Creating v8::Locker for worker {} (before v8::Locker::new)",
            self.worker_id
        );

        // Create v8::Locker - handles enter/exit automatically via RAII
        let mut locker = v8::Locker::new(&mut entry.isolate.isolate);

        tracing::trace!(
            "v8::Locker created for worker {}, executing closure",
            self.worker_id
        );

        // Execute user closure with mutable reference via DerefMut
        let result = f(&mut locker);

        tracing::trace!(
            "Closure executed, v8::Locker will be dropped for worker {}",
            self.worker_id
        );

        result
    }

    /// Execute async closure with locked isolate
    ///
    /// Creates v8::Locker (which handles enter/exit automatically), calls async closure, drops locker.
    /// This is the preferred method for async operations.
    ///
    /// Note: The Locker is held for the entire duration of the async operation,
    /// so the isolate remains locked via the V8 mutex.
    ///
    /// **WARNING:** The Locker also calls `Isolate::Enter()` which sets the
    /// thread-local "current isolate". If multiple tasks interleave on the same
    /// thread (via `spawn_local`), each holding a Locker for a different isolate,
    /// `Isolate::GetCurrent()` will return the wrong isolate for resumed tasks.
    /// Use `IsolateGuard` (enter/exit per V8 work block) to fix this.
    pub async fn with_lock_async<F, Fut, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut v8::Isolate) -> Fut,
        Fut: std::future::Future<Output = R>,
    {
        tracing::trace!("Acquiring entry lock for worker {}", self.worker_id);
        let mut entry = self.entry.lock().await;

        // Update stats before creating Locker (to avoid double borrow)
        entry.total_requests += 1;
        let total_requests = entry.total_requests;

        // Clone Arc to the destruction queue so we can access it after borrowing isolate
        let destruction_queue = Arc::clone(&entry.isolate.deferred_destruction_queue);

        tracing::trace!(
            "Creating v8::Locker for worker {} (before v8::Locker::new, total_requests: {})",
            self.worker_id,
            total_requests
        );

        // Create v8::Locker - handles enter/exit automatically via RAII
        let mut locker = v8::Locker::new(&mut entry.isolate.isolate);

        tracing::trace!(
            "v8::Locker created for worker {}, processing deferred destructions",
            self.worker_id
        );

        // Process any pending deferred handle destructions (while lock is held)
        destruction_queue.process_all();

        // Register JsLock for GC tracking (applies any pending memory adjustments)
        let _js_lock = JsLock::new(&mut locker);

        tracing::trace!(
            "Isolate locked for worker {} (total_requests: {})",
            self.worker_id,
            total_requests
        );

        // Execute async user closure with mutable reference via DerefMut
        let result = f(&mut locker).await;

        tracing::trace!("Isolate unlocked for worker {}", self.worker_id);

        // JsLock drops here, unregistering from thread-local

        // Locker drop will call exit() automatically here
        result
    }

    /// Get worker ID
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// Get whether snapshot was used for isolate creation
    pub async fn use_snapshot(&self) -> bool {
        let entry = self.entry.lock().await;
        entry.isolate.use_snapshot
    }

    /// Get platform reference
    pub async fn platform(&self) -> &'static v8::SharedRef<v8::Platform> {
        let entry = self.entry.lock().await;
        entry.isolate.platform
    }

    /// Get runtime limits
    pub async fn limits(&self) -> RuntimeLimits {
        let entry = self.entry.lock().await;
        entry.isolate.limits.clone()
    }

    /// Get memory limit hit flag
    pub async fn memory_limit_hit(&self) -> Arc<std::sync::atomic::AtomicBool> {
        let entry = self.entry.lock().await;
        Arc::clone(&entry.isolate.memory_limit_hit)
    }
}

impl Drop for PooledIsolate {
    fn drop(&mut self) {
        // Touch LRU to mark as recently used
        // This is async-safe because we use try_lock
        if let Ok(mut cache) = self.pool.try_lock() {
            // get() touches the entry in LRU (marks as recently used)
            cache.get(&self.worker_id);
            tracing::trace!("Isolate marked as recently used: {}", self.worker_id);
        }
        // If lock fails, isolate will still be in cache but not marked as used
        // This is acceptable - better than blocking on drop
    }
}

/// Pool statistics
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Maximum capacity of the pool
    pub total: usize,
    /// Current number of cached isolates
    pub cached: usize,
    /// LRU capacity (same as total)
    pub capacity: usize,
}

// ============================================================================
// Public API for pool management
// ============================================================================

/// Initialize the global isolate pool
///
/// Should be called once at startup, before handling any requests.
///
/// # Arguments
/// * `max_size` - Maximum number of isolates to cache (e.g., 1000)
/// * `limits` - Runtime limits for isolates (heap size, etc.)
///
/// # Example
/// ```ignore
/// use openworkers_runtime_v8::{init_pool, RuntimeLimits};
///
/// init_pool(1000, RuntimeLimits {
///     heap_initial_mb: 10,
///     heap_max_mb: 50,
///     ..Default::default()
/// });
/// ```
pub fn init_pool(max_size: usize, limits: RuntimeLimits) {
    if ISOLATE_POOL
        .set(IsolatePool::new(max_size, limits))
        .is_err()
    {
        tracing::warn!("Isolate pool already initialized");
    } else {
        tracing::info!("Isolate pool initialized: max_size={}", max_size);
    }
}

/// Get the global isolate pool
///
/// Panics if pool was not initialized via `init_pool()`.
pub(crate) fn get_pool() -> &'static IsolatePool {
    ISOLATE_POOL
        .get()
        .expect("Isolate pool not initialized. Call init_pool() at startup.")
}

/// Get pool statistics (for metrics/observability)
///
/// # Example
/// ```ignore
/// let stats = get_pool_stats().await;
/// println!("Pool: {}/{} isolates cached", stats.cached, stats.total);
/// ```
pub async fn get_pool_stats() -> PoolStats {
    get_pool().stats().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_acquire_and_release() {
        let pool = IsolatePool::new(10, RuntimeLimits::default());

        let pooled = pool.acquire("worker_1").await;
        pooled
            .with_lock_async(|isolate| {
                // Isolate is locked and usable
                let is_terminating = isolate.is_execution_terminating();
                async move {
                    assert!(!is_terminating);
                }
            })
            .await;
        // Drop releases back to pool
    }

    #[tokio::test]
    async fn test_pool_cache_hit() {
        let pool = IsolatePool::new(10, RuntimeLimits::default());

        // First acquire (cache miss)
        {
            let _pooled = pool.acquire("worker_1").await;
        }

        // Second acquire (cache hit)
        {
            let _pooled = pool.acquire("worker_1").await;
            // Should reuse same isolate
        }

        let stats = pool.stats().await;
        assert_eq!(stats.cached, 1);
    }

    #[tokio::test]
    async fn test_pool_lru_eviction() {
        let pool = IsolatePool::new(2, RuntimeLimits::default()); // Small pool

        // Fill pool
        {
            let _p1 = pool.acquire("worker_1").await;
        }
        {
            let _p2 = pool.acquire("worker_2").await;
        }

        // This should evict worker_1 (LRU)
        {
            let _p3 = pool.acquire("worker_3").await;
        }

        let stats = pool.stats().await;
        assert_eq!(stats.cached, 2);
    }

    #[tokio::test]
    async fn test_pool_concurrent_access() {
        use tokio::task::LocalSet;

        let local = LocalSet::new();
        local
            .run_until(async {
                let pool = Arc::new(IsolatePool::new(10, RuntimeLimits::default()));

                // Multiple tasks accessing different workers (on same thread via LocalSet)
                let handles: Vec<_> = (0..5)
                    .map(|i| {
                        let pool = Arc::clone(&pool);
                        tokio::task::spawn_local(async move {
                            let worker_id = format!("worker_{}", i);
                            let pooled = pool.acquire(&worker_id).await;
                            pooled
                                .with_lock_async(|_isolate| {
                                    // Simulate work
                                    async move {
                                        tokio::time::sleep(std::time::Duration::from_millis(10))
                                            .await;
                                    }
                                })
                                .await;
                        })
                    })
                    .collect();

                for handle in handles {
                    handle.await.unwrap();
                }

                let stats = pool.stats().await;
                assert_eq!(stats.cached, 5);
            })
            .await;
    }

    #[tokio::test]
    async fn test_pool_same_worker_sequential() {
        let pool = Arc::new(IsolatePool::new(10, RuntimeLimits::default()));

        // Same worker, multiple sequential requests
        for _ in 0..10 {
            let pooled = pool.acquire("worker_1").await;
            pooled
                .with_lock_async(|_isolate| {
                    // Simulate work
                    async move {}
                })
                .await;
        }

        // Should still have only 1 isolate in cache
        let stats = pool.stats().await;
        assert_eq!(stats.cached, 1);
    }
}
