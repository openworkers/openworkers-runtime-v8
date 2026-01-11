# V8 Execution Modes

The runtime-v8 supports **three distinct execution modes**, each with different performance characteristics and use cases.

## Quick Comparison

| Mode              | Cold Start | Warm Start | Memory              | Thread Model        | Status         | Use Case             |
| ----------------- | ---------- | ---------- | ------------------- | ------------------- | -------------- | -------------------- |
| **Worker**        | ~3-5ms     | ~3-5ms     | Low                 | Single per request  | Stable      | Maximum isolation    |
| **SharedIsolate** | ~3-5ms     | ~100µs     | Medium              | Thread-local        | Untested    | Backward compat      |
| **IsolatePool**   | ~100µs     | <10µs      | High (configurable) | Multi-threaded pool | Recommended | Production workloads |

---

## Mode 1: Worker (Per-Request Isolate)

**What it does**: Creates a new V8 isolate for each request, destroys it after execution.

### Architecture

```
Request 1 → Create v8::OwnedIsolate → Execute → Drop isolate
Request 2 → Create v8::OwnedIsolate → Execute → Drop isolate
Request 3 → Create v8::OwnedIsolate → Execute → Drop isolate
```

### Usage

```rust
use openworkers_runtime_v8::Worker;

// Create worker with operations handler
let mut worker = Worker::new_with_ops(script, limits, ops).await?;

// Execute task
worker.exec(task).await?;

// Worker is dropped here, isolate is destroyed
```

### Characteristics

**Pros:**

- **Maximum isolation** - Each request in its own isolate
- **Simple** - No state sharing, no locks
- **No memory leaks** - Isolate destroyed after each request
- **Proven** - Was in production on main branch

**Cons:**

- **Slow** - 3-5ms per request (creating isolate is expensive)
- **No caching** - Code parsed/compiled every time
- **High overhead** - Cannot handle high-throughput workloads

**When to use:**

- Maximum security required (untrusted code)
- Low request volume (<10 req/s)
- Debugging/testing
- WASM runtime fallback

---

## Mode 2: SharedIsolate (Thread-Local Reuse)

**What it does**: One isolate per thread, reused across requests via thread-local storage.

### Architecture

```
Thread 1:
  Request 1 → Get thread-local isolate → Create ExecutionContext → Execute
  Request 2 → Reuse same isolate     → Create ExecutionContext → Execute
  Request 3 → Reuse same isolate     → Create ExecutionContext → Execute

Thread 2:
  Request 4 → Get thread-local isolate (different from Thread 1) → Execute
  ...
```

### Usage

```rust
use openworkers_runtime_v8::{SharedIsolate, ExecutionContext};

// Thread-local storage (initialize once per thread)
thread_local! {
    static SHARED: OnceLock<Arc<Mutex<SharedIsolate>>> = const { OnceLock::new() };
}

// Get or create thread-local isolate
let shared_lock = SHARED.with(|s| {
    s.get_or_init(|| {
        let isolate = SharedIsolate::new(limits);
        Arc::new(Mutex::new(isolate))
    }).clone()
});

let mut shared = shared_lock.lock().await;

// Create fresh ExecutionContext (~100µs)
let mut ctx = ExecutionContext::new(&mut *shared, script, ops)?;

// Execute
ctx.exec(task).await?;

// ExecutionContext dropped, isolate stays alive
```

### Characteristics

**Pros:**

- **Faster than Worker** - ~100µs context creation vs 3-5ms isolate creation
- **No cross-thread sharing** - Each thread owns its isolate
- **Simple locking** - Just tokio::Mutex per thread

**Cons:**

- **Never tested in production** - Experimental
- **Thread-local only** - Cannot share across threads
- **Memory per thread** - Each worker thread holds an isolate
- **No LRU eviction** - Isolates stay alive until thread dies

**When to use:**

- Backward compatibility (if someone depended on this API)
- Single-threaded workloads
- **NOT RECOMMENDED for production**

---

## Mode 3: IsolatePool (LRU Pool with v8::Locker)

**What it does**: Pool of isolates shared across threads, accessed via v8::Locker, evicted via LRU.

### Architecture

```
Thread Pool (N threads):
  Thread 1 ┐
  Thread 2 ├─→ IsolatePool (LRU cache)
  Thread 3 ┘      ├─ worker_A → LockerManagedIsolate
                  ├─ worker_B → LockerManagedIsolate
                  ├─ worker_C → LockerManagedIsolate
                  └─ ... (up to max_size isolates)

Access pattern:
1. pool.acquire(worker_id) → Get PooledIsolate for this worker
2. pooled.with_lock_async(|isolate| { ... }) → v8::Locker locks isolate
3. Execute inside lock
4. Locker dropped, isolate returned to pool
```

### Usage

```rust
use openworkers_runtime_v8::{init_pool, execute_pooled};

// Initialize pool once at startup
init_pool(
    1000, // max_size: up to 1000 cached isolates
    RuntimeLimits {
        heap_initial_mb: 10,
        heap_max_mb: 50,
        ..Default::default()
    }
);

// Execute worker (simple API, handles everything)
let result = execute_pooled(
    "worker-123",  // worker_id (used as cache key)
    script,
    ops,
    task,
).await?;
```

### Detailed Execution Flow

```rust
// What happens inside execute_pooled():

// 1. Acquire pooled isolate from cache
let pooled = pool.acquire(worker_id).await;
//    ↓
//    If cache HIT: Reuse existing isolate (<10µs)
//    If cache MISS: Create new isolate (~100µs with snapshot)
//    If pool FULL: Evict LRU isolate, create new one

// 2. Lock isolate with v8::Locker
pooled.with_lock_async(|isolate: &v8::Isolate| async move {
    //    ↓
    //    v8::Locker ensures exclusive access
    //    Only ONE thread can access this isolate at a time

    // 3. Create ExecutionContext with locked isolate
    let mut ctx = ExecutionContext::new_with_isolate(isolate, script, ops)?;

    // 4. Execute task
    ctx.exec(task).await

    // 5. ExecutionContext dropped
    // 6. v8::Locker auto-unlocked (RAII)
}).await
// 7. Isolate returned to pool, available for next request
```

### Characteristics

**Pros:**

- **Ultra-fast warm starts** - <10µs when cached
- **Efficient caching** - LRU eviction keeps hot workers, drops cold
- **Multi-threaded** - Any thread can access any worker's isolate
- **Memory bounded** - Pool size × heap limit (configurable)
- **Thread-safe** - v8::Locker + Rust's type system guarantee safety
- **Matches Cloudflare Workers** - Proven architecture at billions req/day

**Cons:**

- **Lock contention possible** - If many threads access same worker simultaneously
- **Memory usage** - Can be high (e.g., 1000 × 50MB = 50GB, but ptrcomp reduces to ~30GB)
- **Complexity** - More complex than simple per-request isolates

**When to use:**

- **Production workloads** (RECOMMENDED)
- High request volume (>1000 req/s)
- Need global distribution (minimize memory footprint)
- Want Cloudflare Workers-like performance

### Performance Metrics

Real-world expectations:

```bash
# Cold start (cache miss, worker never seen before)
Isolate creation: ~100µs (with snapshot) or ~3-5ms (without)

# Warm start (cache hit, worker cached)
Pool acquire + lock: <10µs

# Throughput
Per-worker: 10,000+ req/s (when cached)
Hit rate in steady state: >95%

# Memory
Without ptrcomp: pool_size × heap_max_mb (e.g., 1000 × 50MB = 50GB)
With ptrcomp:    pool_size × ~30MB      (e.g., 1000 × 30MB = 30GB)
```

---

## Configuration

### Environment Variables (IsolatePool)

```bash
# .env for openworkers-runner
ISOLATE_POOL_SIZE=1000           # Max cached isolates (default: 1000)
ISOLATE_HEAP_INITIAL_MB=10       # Initial heap per isolate (default: 10MB)
ISOLATE_HEAP_MAX_MB=50           # Max heap per isolate (default: 50MB)
```

### Monitoring (IsolatePool)

```bash
# Get pool statistics
curl http://localhost:8080/admin/pool

# Response:
{
  "total": 1000,       # Max pool size
  "cached": 347,       # Currently cached isolates
  "capacity": 1000,    # Remaining capacity
  "hit_rate": 0.94     # Cache hit rate (94%)
}
```

---

## Choosing the Right Mode

### Decision Tree

```
Start here
    ↓
Is security more important than performance?
    ├─ YES → Use Worker mode (per-request isolate)
    └─ NO  → Continue
             ↓
             Do you need >100 req/s throughput?
             ├─ YES → Use IsolatePool (RECOMMENDED)
             └─ NO  → Continue
                      ↓
                      Do you have existing code using SharedIsolate?
                      ├─ YES → Keep SharedIsolate (backward compat)
                      └─ NO  → Use IsolatePool anyway (best default)
```

### Summary

- **Default/Recommended**: **IsolatePool** - Best performance, production-ready
- **Maximum Security**: **Worker** - Each request isolated, slower but safer
- **Legacy/Compat**: **SharedIsolate** - Only if already using it

---

## Thread Safety Guarantees

### Worker Mode

- Thread-safe: Each request gets its own isolate
- No locks needed: No shared state

### SharedIsolate Mode

- Thread-safe: Each thread has its own isolate
- Single lock per thread: `Mutex<SharedIsolate>`

### IsolatePool Mode

- Thread-safe: Double-lock pattern
  1. **Rust lock**: `Arc<Mutex<IsolateEntry>>` - Pool-level access control
  2. **V8 lock**: `v8::Locker` - V8-level thread safety
- Type system enforced: `!Send` on Locker prevents thread transfer
- RAII guarantees: Locks auto-released via Drop trait

**Why v8::Locker is critical for IsolatePool:**

V8 isolates are **not thread-safe** by default. The `v8::Locker` API provides mutual exclusion:

- Only ONE thread can hold a Locker for a given isolate at a time
- Attempting to lock from another thread blocks until released
- This allows safe sharing of isolates across threads

Without Locker, concurrent access would cause:

- Memory corruption
- Segfaults
- Undefined behavior

Rust's type system ensures we can't forget the Locker:

- `v8::Locker` is `!Send` (cannot transfer between threads)
- Must be created on the thread that uses the isolate
- Automatically dropped when scope exits (RAII)

---

## Migration Guide

### From Worker to IsolatePool

**Before** (worker.rs):

```rust
let mut worker = Worker::new_with_ops(script, limits, ops).await?;
worker.exec(task).await?;
```

**After** (main.rs + task_executor.rs):

```rust
// In main.rs - once at startup
init_pool(1000, limits);

// In task_executor.rs - per request
execute_pooled(worker_id, script, ops, task).await?;
```

### From SharedIsolate to IsolatePool

**Before**:

```rust
let shared_lock = get_thread_local_isolate();
let mut shared = shared_lock.lock().await;
let mut ctx = ExecutionContext::new(&mut *shared, script, ops)?;
ctx.exec(task).await?;
```

**After**:

```rust
// In main.rs - once at startup
init_pool(1000, limits);

// In task_executor.rs - per request
execute_pooled(worker_id, script, ops, task).await?;
```

---

## See Also

- [isolate_pool.md](./isolate_pool.md) - Deep dive into pool implementation
- [ARCHITECTURE.md](./ARCHITECTURE.md) - Overall runtime architecture
- [../../DENO_DEPLOY_PROBLEMS.md](../../DENO_DEPLOY_PROBLEMS.md) - Why process-per-isolate fails at scale
