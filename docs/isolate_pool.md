# Isolate Pool Implementation

Technical deep dive into the LRU isolate pool with `v8::Locker`.

For usage patterns, see [execution_modes.md](./execution_modes.md).

## Layer Diagram

```
openworkers-runner
    │
    └─► execute_pooled(worker_id, script, ops, task)
            │
            ▼
        IsolatePool (LRU cache)
            │
            ├─► acquire(worker_id) → PooledIsolate
            │
            └─► PooledIsolate.with_lock_async(|isolate| ...)
                    │
                    ├─► v8::Locker (exclusive access)
                    │
                    └─► ExecutionContext (execute JS)
```

## Core Structures

### IsolatePool

```rust
static ISOLATE_POOL: OnceLock<IsolatePool> = OnceLock::new();

pub struct IsolatePool {
    cache: Arc<Mutex<LruCache<WorkerId, Arc<Mutex<IsolateEntry>>>>>,
    max_size: usize,
    limits: RuntimeLimits,
}
```

Global singleton. LRU cache maps `worker_id` → isolate entry.

### PooledIsolate

```rust
pub struct PooledIsolate {
    entry: Arc<Mutex<IsolateEntry>>,
    worker_id: WorkerId,
    pool: Arc<Mutex<LruCache<...>>>,
}
```

RAII guard. Refreshes LRU position on drop.

### LockerManagedIsolate

```rust
pub struct LockerManagedIsolate {
    pub isolate: v8::UnenteredIsolate,  // Not OwnedIsolate!
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    pub memory_limit_hit: Arc<AtomicBool>,
    pub use_snapshot: bool,
    pub deferred_destruction_queue: Arc<DeferredDestructionQueue>,
    // ... heap limit state
}
```

Uses `UnenteredIsolate` (not `OwnedIsolate`) because:

- `OwnedIsolate` auto-enters → incompatible with `v8::Locker`
- `UnenteredIsolate` requires explicit locking → pool-compatible

## Double-Lock Pattern

Two levels of locking protect different concerns:

```
Level 1: Rust Mutex
├── Protects LRU cache structure
├── Protects metadata (created_at, total_requests)
└── Scope: microseconds (just lookups)

Level 2: v8::Locker
├── V8-level exclusive access to isolate
├── Required by V8's threading model
└── Scope: milliseconds (full execution)
```

**Why both?**

- Rust Mutex alone doesn't satisfy V8's threading requirements
- v8::Locker alone doesn't protect Rust data structures

## LRU Eviction

```rust
// On cache miss
if cache.len() >= max_size {
    let (evicted_id, _) = cache.pop_lru().unwrap();
    // Arc drops → Mutex drops → IsolateEntry drops → V8 isolate destroyed
}

let entry = IsolateEntry::new(limits);
cache.put(worker_id, Arc::new(Mutex::new(entry)));
```

Isolate is only destroyed when:

1. Evicted from cache **AND**
2. No active `PooledIsolate` references exist

This prevents destroying an isolate mid-execution.

## Execution Flow

```rust
pub async fn with_lock_async<F, Fut, R>(&self, f: F) -> R {
    // 1. Rust-level lock
    let mut entry = self.entry.lock().await;

    // 2. V8-level lock (blocks other threads)
    let _locker = v8::Locker::new(entry.isolate.as_isolate());

    // 3. Update stats
    entry.total_requests += 1;

    // 4. Execute with exclusive access
    f(entry.isolate.as_isolate()).await

    // 5. Locks auto-released via Drop
}
```

## Performance

| Operation                  | Time           |
| -------------------------- | -------------- |
| Cache hit + lock           | Fastest (~µs)  |
| Cache miss (with snapshot) | Fast (~µs)     |
| Cache miss (no snapshot)   | Slower (~ms)   |

### Contention Scenarios

```
Different workers, concurrent:
  Thread 1 → worker_A → no contention
  Thread 2 → worker_B → no contention

Same worker, concurrent:
  Thread 1 → worker_A → executing...
  Thread 2 → worker_A → BLOCKED until Thread 1 finishes
```

**Mitigation:** In practice, requests to the same worker are rare in concurrent scenarios.

## Monitoring

```rust
pub struct PoolStats {
    pub total: usize,    // Max pool size
    pub cached: usize,   // Currently cached
    pub capacity: usize, // Remaining (total - cached)
}
```

**Key metrics:**

- `hit_rate > 95%` → pool size adequate
- `cached == total` constantly → pool size appropriate
- High latency for specific workers → contention, increase pool size

## Type Safety

```rust
// v8::Locker is !Send
pub struct Locker {
    _no_send: PhantomData<*mut ()>,
}
```

Rust compiler prevents:

- Sending Locker to another thread
- Using isolate without Locker
- Locker outliving its scope

## Code Pointers

| Component            | File                        | Key functions                     |
| -------------------- | --------------------------- | --------------------------------- |
| Pool singleton       | `isolate_pool.rs`           | `init_pool()`, `get_pool()`       |
| Pool entry point     | `pooled_execution.rs`       | `execute_pooled()`                |
| Isolate wrapper      | `locker_managed_isolate.rs` | `LockerManagedIsolate::new()`     |
| Execution context    | `execution_context.rs`      | `new_with_pooled_isolate()`       |
