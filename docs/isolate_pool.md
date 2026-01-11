# Isolate Pool - Technical Deep Dive

This document explains the technical implementation of the isolate pool with v8::Locker and LRU eviction.

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Component Breakdown](#component-breakdown)
- [LRU Cache Implementation](#lru-cache-implementation)
- [Thread Safety Model](#thread-safety-model)
- [Lifecycle Management](#lifecycle-management)
- [Performance Characteristics](#performance-characteristics)
- [Testing Guide](#testing-guide)

---

## Architecture Overview

### Layer Diagram

```
┌──────────────────────────────────────────────────────────────────┐
│                     openworkers-runner                           │
│                                                                  │
│  bin/main.rs:                                                    │
│    ├─ init_pool(max_size, limits)  ← Called once at startup      │
│    └─ GET /admin/pool               ← Monitoring endpoint        │
│                                                                  │
│  task_executor.rs:                                               │
│    └─ execute_pooled(worker_id, script, ops, task)               │
│                          │                                       │
└──────────────────────────┼───────────────────────────────────────┘
                           │
                           ▼
┌──────────────────────────────────────────────────────────────────┐
│               openworkers-runtime-v8                             │
│                                                                  │
│  pooled_execution.rs (public API):                               │
│    └─ execute_pooled()                                           │
│         └─ pool.acquire(worker_id)                               │
│              └─ pooled.with_lock_async(|isolate| { ... })        │
│                                                                  │
│  isolate_pool.rs (core implementation):                          │
│    ├─ IsolatePool       ← LRU cache                              │
│    ├─ PooledIsolate     ← RAII guard for pool entry              │
│    └─ IsolateEntry      ← Wrapper around LockerManagedIsolate    │
│                                                                  │
│  locker_managed_isolate.rs:                                      │
│    └─ LockerManagedIsolate ← Wraps v8::UnenteredIsolate          │
│                                                                  │
│  execution_context.rs:                                           │
│    └─ ExecutionContext ← Executes JS code in isolate             │
└──────────────────────────────────────────────────────────────────┘
                           │
                           ▼
┌──────────────────────────────────────────────────────────────────┐
│                      rusty_v8                                    │
│                                                                  │
│  v8::UnenteredIsolate    ← No auto-enter/exit                    │
│  v8::Locker              ← Thread-safe exclusive access          │
│  v8::Unlocker            ← Temporary release                     │
└──────────────────────────────────────────────────────────────────┘
```

---

## Component Breakdown

### 1. IsolatePool (Global Singleton)

```rust
static ISOLATE_POOL: OnceLock<IsolatePool> = OnceLock::new();

pub struct IsolatePool {
    cache: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
    max_size: usize,
    limits: RuntimeLimits,
}
```

**Responsibilities:**

- Manage LRU cache of isolates
- Create new isolates on demand
- Evict least-recently-used isolates when full
- Track statistics (cache hits, total requests)

**Key methods:**

```rust
// Initialize pool (called once at startup)
pub fn init_pool(max_size: usize, limits: RuntimeLimits);

// Acquire isolate for worker_id (async)
pub async fn acquire(&self, worker_id: &str) -> PooledIsolate;

// Get statistics
pub async fn stats(&self) -> PoolStats;
```

### 2. PooledIsolate (RAII Guard)

```rust
pub struct PooledIsolate {
    entry: Arc<Mutex<IsolateEntry>>,
    worker_id: WorkerId,
    pool: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
}
```

**Purpose:** RAII guard that ensures isolate is returned to pool when dropped.

**Key method:**

```rust
pub async fn with_lock_async<F, Fut, R>(&self, f: F) -> R
where
    F: FnOnce(&v8::Isolate) -> Fut,
    Fut: Future<Output = R>,
{
    let mut entry = self.entry.lock().await;  // Rust-level lock
    let _locker = v8::Locker::new(entry.isolate.as_isolate());  // V8-level lock

    entry.total_requests += 1;
    entry.last_accessed = Instant::now();

    f(entry.isolate.as_isolate()).await  // Execute with locked isolate

    // Locks auto-released via Drop
}
```

**Drop behavior:**

```rust
impl Drop for PooledIsolate {
    fn drop(&mut self) {
        // Update LRU cache on drop (mark as recently used)
        let mut pool = self.pool.blocking_lock();
        pool.get(&self.worker_id);  // Refresh LRU position
    }
}
```

### 3. IsolateEntry

```rust
struct IsolateEntry {
    isolate: LockerManagedIsolate,
    created_at: Instant,
    last_accessed: Instant,
    total_requests: usize,
}
```

**Purpose:** Metadata wrapper around isolate for tracking usage.

### 4. LockerManagedIsolate

```rust
pub struct LockerManagedIsolate {
    pub isolate: v8::UnenteredIsolate,
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    pub memory_limit_hit: Arc<AtomicBool>,
    pub use_snapshot: bool,
}
```

**Purpose:**

- Wraps `v8::UnenteredIsolate` (no auto-enter/exit)
- Compatible with `v8::Locker` (unlike `v8::OwnedIsolate`)
- Manages V8 platform and heap limits

**Key difference from OwnedIsolate:**

- `OwnedIsolate` auto-enters on creation → incompatible with Locker
- `UnenteredIsolate` never auto-enters → must use Locker for access

---

## LRU Cache Implementation

### Cache Key: WorkerId

```rust
pub type WorkerId = String;  // e.g., "worker-123"
```

Each worker gets its own cached isolate. Multiple requests to the same worker reuse the same isolate.

### LRU Eviction Policy

When cache is full and a new worker is requested:

```rust
// 1. Check if worker already cached
if let Some(entry) = pool.cache.get(worker_id) {
    // Cache HIT - reuse existing isolate
    return PooledIsolate { entry, ... };
}

// 2. Cache MISS - check if pool is full
if pool.cache.len() >= pool.max_size {
    // Pool is full - evict LRU entry
    if let Some((evicted_id, _)) = pool.cache.pop_lru() {
        log::info!("Isolate LRU eviction: worker {} evicted", evicted_id);
    }
}

// 3. Create new isolate
let isolate = LockerManagedIsolate::new(pool.limits.clone());
let entry = Arc::new(Mutex::new(IsolateEntry::new(isolate)));

// 4. Insert into cache
pool.cache.put(worker_id.clone(), entry.clone());

return PooledIsolate { entry, ... };
```

### LRU Access Update

Every time an isolate is accessed:

```rust
// When PooledIsolate is dropped
impl Drop for PooledIsolate {
    fn drop(&mut self) {
        // Calling .get() refreshes LRU position
        self.pool.blocking_lock().get(&self.worker_id);
    }
}
```

This ensures frequently-used workers stay in cache, rarely-used workers get evicted.

---

## Thread Safety Model

### Double-Lock Pattern

The pool uses **two levels of locking**:

#### Level 1: Rust Mutex (Pool Access)

```rust
Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>
         ↑                               ↑
         └── Pool-level lock             └── Entry-level lock
```

**Purpose:**

- Protect LRU cache operations (get, put, evict)
- Prevent race conditions on cache structure

**Scope:** Very short (just cache lookups)

#### Level 2: V8 Locker (Isolate Access)

```rust
let _locker = v8::Locker::new(isolate);
```

**Purpose:**

- Guarantee exclusive access to V8 isolate
- Required by V8's threading model
- Prevents concurrent V8 API calls

**Scope:** Entire execution duration (~ms)

### Why Two Locks?

**Rust Mutex alone is not enough:**

- V8 isolates are **not thread-safe** - concurrent access causes corruption
- V8 expects only ONE thread to use an isolate at a time
- `v8::Locker` enforces this at the V8 C++ level

**V8 Locker alone is not enough:**

- Need to protect LRU cache structure (Rust data)
- Need to track metadata (created_at, last_accessed, etc.)

### Type System Guarantees

```rust
pub struct Locker {
    _no_send: PhantomData<*mut ()>,  // !Send marker
    // ...
}
```

- `v8::Locker` is **!Send** - cannot transfer between threads
- Must be created and dropped on the same thread
- Rust compiler prevents accidentally sending Locker to another thread

**This guarantees:**

- Locker is always paired with isolate usage
- No isolate access without Locker
- No Locker outliving its scope (RAII)

---

## Lifecycle Management

### Isolate Creation

```rust
// When: Cache miss or pool not initialized
let isolate = LockerManagedIsolate::new(limits);

// Inside LockerManagedIsolate::new():
1. Initialize V8 platform (once, globally)
2. Load snapshot (if available)
3. Create v8::UnenteredIsolate
4. Set heap limits
5. Install memory pressure callback
```

**Cold start time:**

- With snapshot: ~100µs
- Without snapshot: ~3-5ms

### Isolate Reuse

```rust
// When: Cache hit
let pooled = pool.acquire(worker_id).await;  // <10µs
pooled.with_lock_async(|isolate| async move {
    // Execute with cached isolate
}).await;
```

**Warm start time:** <10µs (just lock acquisition)

### Isolate Eviction

```rust
// When: Pool full + cache miss
if pool.cache.len() >= pool.max_size {
    let (evicted_id, entry) = pool.cache.pop_lru().unwrap();

    log::info!("Isolate LRU eviction: worker {} evicted", evicted_id);

    // Arc<Mutex<IsolateEntry>> reference count drops to 0
    // → Mutex<IsolateEntry> dropped
    // → IsolateEntry dropped
    // → LockerManagedIsolate dropped
    // → v8::UnenteredIsolate dropped
    // → V8 isolate destroyed
}
```

**Note:** Isolate is only destroyed when:

1. Evicted from cache AND
2. No active `PooledIsolate` references exist

This prevents destroying an isolate mid-execution.

---

## Performance Characteristics

### Benchmarks (Expected)

```
Cold start (cache miss):
  With snapshot:    ~100µs
  Without snapshot: ~3-5ms

Warm start (cache hit):
  Pool acquire: <1µs
  Locker acquire: <5µs
  Total: <10µs

Throughput (per worker, cached):
  Sequential: 10,000+ req/s
  Concurrent: Limited by lock contention

Memory usage:
  Without ptrcomp: pool_size × heap_max_mb
  With ptrcomp:    pool_size × ~60% × heap_max_mb

  Example (1000 isolates, 50MB heap max):
    Without ptrcomp: 50GB
    With ptrcomp:    30GB
```

### Lock Contention Scenarios

**Scenario 1: Different workers, same time**

```
Thread 1: worker-A → Lock A → Execute (no contention)
Thread 2: worker-B → Lock B → Execute (no contention)
Thread 3: worker-C → Lock C → Execute (no contention)
```

**No contention** - Different isolates

**Scenario 2: Same worker, sequential**

```
Thread 1: worker-A → Lock A → Execute → Unlock A
Thread 2: worker-A → Lock A → Execute (reuses, <10µs)
```

**Fast** - Cache hit, no waiting

**Scenario 3: Same worker, concurrent**

```
Thread 1: worker-A → Lock A → Execute (5ms)
Thread 2: worker-A → WAIT for Lock A... → Execute
```

**Contention** - Thread 2 blocks until Thread 1 finishes

**Mitigation:** In practice, requests to the same worker are rare in concurrent scenarios. Most workloads have high worker diversity.

---

## Testing Guide

### Unit Tests

```bash
cd openworkers-runtime-v8
cargo test --lib isolate_pool
cargo test --lib locker_test
cargo test --lib unentered_isolate_test
```

### Integration Tests

See [../../POOL_TESTING.md](../../POOL_TESTING.md) for full testing procedures.

**Quick test:**

```bash
# Start runner
cd openworkers-runner
cargo run --features v8 --bin openworkers-runner

# Check pool stats (empty)
curl http://localhost:8080/admin/pool
# {"total":1000,"cached":0,"capacity":1000,"hit_rate":0.00}

# Send requests to same worker
for i in {1..10}; do
  curl -H "Host: test-worker.workers.dev.localhost" http://localhost:8080/
done

# Check stats again (should show 1 cached, high hit rate)
curl http://localhost:8080/admin/pool
# {"total":1000,"cached":1,"capacity":1000,"hit_rate":0.90}
```

### Load Testing

```bash
# 10,000 requests, 100 concurrent, same worker
ab -n 10000 -c 100 -H "Host: load-test.workers.dev.localhost" http://localhost:8080/

# Expected:
# - First request: cold start (~100µs)
# - Remaining 9,999: warm starts (<10µs)
# - Throughput: 10,000+ req/s
# - Pool stats: cached=1, hit_rate=0.9999
```

---

## Monitoring

### Pool Statistics

```rust
pub struct PoolStats {
    pub total: usize,    // Max pool size (from config)
    pub cached: usize,   // Currently cached isolates
    pub capacity: usize, // Remaining capacity (total - cached)
}
```

**GET /admin/pool** response:

```json
{
  "total": 1000,
  "cached": 347,
  "capacity": 653,
  "hit_rate": 0.94
}
```

### Key Metrics to Watch

1. **Cache hit rate** - Should be >95% in steady state
   - Low hit rate → Pool too small or too many unique workers

2. **Cached isolates** - Should stabilize at pool size or below
   - Constantly at max → Pool size is appropriate
   - Much below max → Pool size could be reduced

3. **Memory usage** - `cached × heap_max_mb`
   - Monitor system memory to ensure not exceeding limits

### Logging

Enable debug logs to see pool activity:

```bash
RUST_LOG=debug,openworkers_runtime_v8=debug cargo run
```

**Example logs:**

```
DEBUG Isolate pool config: max_size=1000, heap_initial=10MB, heap_max=50MB
INFO  Isolate pool initialized: max_size=1000
DEBUG Isolate cache MISS for worker test-worker
DEBUG Isolate cache HIT for worker test-worker
INFO  Isolate LRU eviction: worker old-worker evicted (cache full at 1000)
```

---

## Common Issues & Solutions

### Issue: High lock contention

**Symptoms:**

- High latency for specific workers
- Many threads blocked waiting for locks

**Solution:**

- Increase pool size (more isolates = less contention)
- Reduce request concurrency per worker
- Check if many requests truly target the same worker

### Issue: High memory usage

**Symptoms:**

- Memory usage exceeds expected `pool_size × heap_max_mb`

**Solution:**

- Enable pointer compression (`v8_enable_pointer_compression`)
- Reduce `ISOLATE_POOL_SIZE`
- Reduce `ISOLATE_HEAP_MAX_MB`

### Issue: Low cache hit rate

**Symptoms:**

- `hit_rate` consistently <90%

**Solution:**

- Increase pool size (more workers can be cached)
- Check if worker IDs are stable (not randomly generated)
- Verify request routing (should route same worker to same pool)

---

## See Also

- [execution_modes.md](./execution_modes.md) - Comparison of all execution modes
- [../../DENO_DEPLOY_PROBLEMS.md](../../DENO_DEPLOY_PROBLEMS.md) - Why process-per-isolate fails
- [../../POOL_TESTING.md](../../POOL_TESTING.md) - Full testing procedures
