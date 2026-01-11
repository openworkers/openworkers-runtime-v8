//! Thread-pinned isolate pool with zero contention.
//!
//! Each thread has its own local LRU pool (no global mutex).
//! Use `compute_thread_id()` for sticky routing.

use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::Mutex;

use crate::LockerManagedIsolate;
use crate::execution_context::ExecutionContext;
use openworkers_core::{OperationsHandle, RuntimeLimits, Script, Task, TerminationReason};

// ============================================================================
// Configuration
// ============================================================================

/// Global configuration for thread-pinned pool
static PINNED_POOL_CONFIG: OnceLock<PinnedPoolConfig> = OnceLock::new();

/// Configuration for thread-pinned pool
#[derive(Clone)]
pub struct PinnedPoolConfig {
    /// Maximum isolates per thread
    pub max_per_thread: usize,
    /// Runtime limits for new isolates
    pub limits: RuntimeLimits,
}

/// Initialize the thread-pinned pool configuration
///
/// Must be called once at startup, before any pool access.
pub fn init_pinned_pool(max_per_thread: usize, limits: RuntimeLimits) {
    let config = PinnedPoolConfig {
        max_per_thread,
        limits,
    };

    if PINNED_POOL_CONFIG.set(config).is_err() {
        log::warn!("Thread-pinned pool already initialized");
    } else {
        log::info!(
            "Thread-pinned pool initialized: max_per_thread={}",
            max_per_thread
        );
    }
}

fn get_config() -> &'static PinnedPoolConfig {
    PINNED_POOL_CONFIG
        .get()
        .expect("Thread-pinned pool not initialized. Call init_pinned_pool() at startup.")
}

// ============================================================================
// Thread-Local Pool (Arc-based for async compatibility)
// ============================================================================

/// Entry in the thread-local pool
struct LocalIsolateEntry {
    isolate: LockerManagedIsolate,
    #[allow(dead_code)]
    created_at: Instant,
    total_requests: u64,
}

impl LocalIsolateEntry {
    fn new(limits: RuntimeLimits) -> Self {
        log::debug!("Creating new thread-local LockerManagedIsolate");
        let start = Instant::now();
        let isolate = LockerManagedIsolate::new(limits);
        let duration = start.elapsed();
        log::info!(
            "Thread-local LockerManagedIsolate created in {:?} (snapshot: {})",
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

type WorkerId = String;

/// Thread-local pool using LRU cache (wrapped in Arc<Mutex> for async)
struct ThreadLocalPool {
    cache: LruCache<WorkerId, Arc<Mutex<LocalIsolateEntry>>>,
    limits: RuntimeLimits,
}

impl ThreadLocalPool {
    fn new(max_size: usize, limits: RuntimeLimits) -> Self {
        Self {
            cache: LruCache::new(
                NonZeroUsize::new(max_size).unwrap_or(NonZeroUsize::new(1).unwrap()),
            ),
            limits,
        }
    }

    fn acquire(&mut self, worker_id: &str) -> (Arc<Mutex<LocalIsolateEntry>>, bool) {
        // Check if cache hit
        let is_hit = self.cache.contains(worker_id);

        if !is_hit {
            log::debug!(
                "Thread-local cache MISS for worker {}, creating new",
                worker_id
            );

            let entry = Arc::new(Mutex::new(LocalIsolateEntry::new(self.limits.clone())));

            if let Some((evicted_id, _)) = self.cache.push(worker_id.to_string(), entry) {
                log::info!("Thread-local LRU eviction: worker {} evicted", evicted_id);
            }
        } else {
            log::debug!("Thread-local cache HIT for worker {}", worker_id);
        }

        // Get Arc clone (this also updates LRU order)
        let entry = Arc::clone(self.cache.get(worker_id).unwrap());
        (entry, is_hit)
    }

    fn stats(&self) -> LocalPoolStats {
        LocalPoolStats {
            cached: self.cache.len(),
            capacity: self.cache.cap().get(),
        }
    }
}

thread_local! {
    /// Thread-local pool instance (Arc-wrapped for async compatibility)
    static LOCAL_POOL: std::cell::RefCell<Option<ThreadLocalPool>> = const { std::cell::RefCell::new(None) };
}

/// Ensure thread-local pool is initialized and acquire an isolate entry
fn acquire_from_local_pool(worker_id: &str) -> (Arc<Mutex<LocalIsolateEntry>>, bool) {
    LOCAL_POOL.with(|pool_cell| {
        let mut pool_opt = pool_cell.borrow_mut();

        // Initialize pool if needed
        if pool_opt.is_none() {
            let config = get_config();
            *pool_opt = Some(ThreadLocalPool::new(
                config.max_per_thread,
                config.limits.clone(),
            ));
            log::debug!(
                "Thread-local pool initialized on thread {:?}",
                std::thread::current().id()
            );
        }

        let pool = pool_opt.as_mut().unwrap();
        pool.acquire(worker_id)
    })
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
    pub cached: usize,
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
// Sticky Routing
// ============================================================================

/// Compute thread ID for a worker using consistent hashing
///
/// This ensures the same worker_id always routes to the same thread,
/// which provides:
/// - Cache hits (same isolate reused)
/// - Security isolation (tenant doesn't share isolate with other tenants)
pub fn compute_thread_id(worker_id: &str, num_threads: usize) -> usize {
    let mut hasher = DefaultHasher::new();
    worker_id.hash(&mut hasher);
    (hasher.finish() as usize) % num_threads
}

// ============================================================================
// Execution API
// ============================================================================

/// Execute a worker script using the thread-pinned pool
///
/// This is similar to `execute_pooled` but uses thread-local storage
/// instead of a global mutex-protected pool.
///
/// # Arguments
/// * `worker_id` - Unique worker identifier (used as pool key)
/// * `script` - Worker script to execute
/// * `ops` - Operations handle for fetch, KV, etc.
/// * `task` - Task to execute (HTTP request, scheduled event, etc.)
///
/// # Performance
/// - No mutex contention (thread-local access)
/// - Cache hit: <10µs (isolate reused)
/// - Cache miss: ~100µs (with snapshot) or ~3-5ms (without)
///
/// # Example
/// ```ignore
/// use openworkers_runtime_v8::{execute_pinned, init_pinned_pool, RuntimeLimits};
///
/// // Initialize once at startup
/// init_pinned_pool(100, RuntimeLimits::default());
///
/// // Execute per request (from routed thread)
/// execute_pinned("worker_123", script, ops, task).await?;
/// ```
pub async fn execute_pinned(
    worker_id: &str,
    script: Script,
    ops: OperationsHandle,
    task: Task,
) -> Result<(), TerminationReason> {
    TOTAL_REQUESTS.fetch_add(1, Ordering::Relaxed);

    // Acquire Arc<Mutex<Entry>> from thread-local pool
    // This is synchronous and doesn't hold RefCell across await
    let (entry_arc, is_hit) = acquire_from_local_pool(worker_id);

    if is_hit {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }

    // Now we can work with the Arc across await points
    let mut entry = entry_arc.lock().await;
    entry.total_requests += 1;

    log::trace!(
        "Acquired thread-local isolate for worker_id: {} (requests: {})",
        worker_id,
        entry.total_requests
    );

    // Get metadata
    let use_snapshot = entry.isolate.use_snapshot;
    let platform = entry.isolate.platform;
    let limits = entry.isolate.limits.clone();
    let memory_limit_hit = Arc::clone(&entry.isolate.memory_limit_hit);

    // Create v8::Locker for thread-safety
    let mut locker = v8::Locker::new(&mut entry.isolate.isolate);

    // Create execution context
    let ctx_result = ExecutionContext::new_with_pooled_isolate(
        &mut *locker,
        use_snapshot,
        platform,
        limits,
        memory_limit_hit,
        script,
        ops,
    );

    // Execute
    let mut ctx = ctx_result?;
    ctx.exec(task).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sticky_routing() {
        let num_threads = 8;

        // Same worker_id should always go to same thread
        let thread1 = compute_thread_id("worker_123", num_threads);
        let thread2 = compute_thread_id("worker_123", num_threads);
        assert_eq!(thread1, thread2);

        // Different workers may go to different threads
        let thread_a = compute_thread_id("worker_a", num_threads);
        let thread_b = compute_thread_id("worker_b", num_threads);
        // They might be same or different, but should be consistent
        assert_eq!(thread_a, compute_thread_id("worker_a", num_threads));
        assert_eq!(thread_b, compute_thread_id("worker_b", num_threads));
    }

    #[test]
    fn test_sticky_routing_distribution() {
        let num_threads = 8;
        let mut distribution = vec![0usize; num_threads];

        // Check distribution of 1000 workers
        for i in 0..1000 {
            let worker_id = format!("worker_{}", i);
            let thread_id = compute_thread_id(&worker_id, num_threads);
            distribution[thread_id] += 1;
        }

        // Each thread should get roughly 125 workers (1000/8)
        // Allow 50% variance
        for count in &distribution {
            assert!(*count > 50, "Thread got too few workers: {}", count);
            assert!(*count < 200, "Thread got too many workers: {}", count);
        }
    }
}
