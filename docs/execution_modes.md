# Execution Modes

Three ways to run JavaScript in the V8 runtime.

## Comparison

| Mode              | Cold Start | Warm Start | Thread Model             | Use Case       |
| ----------------- | ---------- | ---------- | ------------------------ | -------------- |
| **IsolatePool**   | ~100µs     | <10µs      | Multi-thread, v8::Locker | **Production** |
| **Worker**        | ~2-3ms     | ~2-3ms     | Single, per-request      | Max isolation  |
| **SharedIsolate** | ~100µs     | ~100µs     | Thread-local             | Legacy         |

## IsolatePool (Recommended)

Global LRU cache of isolates, accessible from any thread via `v8::Locker`.

```rust
use openworkers_runtime_v8::{init_pool, execute_pooled};

// Once at startup
init_pool(1000, RuntimeLimits::default());

// Per request
execute_pooled("worker-id", script, ops, task).await?;
```

**How it works:**

1. `acquire(worker_id)` — Get isolate from cache (or create)
2. `v8::Locker` — Lock isolate for current thread
3. Execute JavaScript
4. Unlock, return to pool

**Cache behavior:**

- Hit → reuse existing isolate (<10µs)
- Miss → create new (~100µs with snapshot)
- Full → evict LRU, create new

See [isolate_pool.md](./isolate_pool.md) for implementation details.

## Worker (Per-Request Isolate)

Creates a new V8 isolate for each request. Maximum isolation, no state leakage.

```rust
use openworkers_runtime_v8::Worker;

let mut worker = Worker::new_with_ops(script, limits, ops).await?;
worker.exec(task).await?;
// Isolate destroyed on drop
```

**When to use:**

- Security-critical (untrusted code)
- Testing
- Low volume (<10 req/s)

## SharedIsolate (Thread-Local)

One isolate per thread, reused via `ExecutionContext`.

```rust
thread_local! {
    static ISOLATE: SharedIsolate = SharedIsolate::new(limits);
}

let mut ctx = ExecutionContext::new(&mut isolate, script, ops)?;
ctx.exec(task).await?;
// Context destroyed, isolate reused
```

**Legacy mode.** Use IsolatePool instead.

---

## Thread-Pinned Pool (Alternative)

For multi-tenant security, each thread owns its local pool.

```rust
use openworkers_runtime_v8::{init_pinned_pool, execute_pinned, compute_thread_id};

init_pinned_pool(100, limits);  // 100 isolates per thread

// Sticky routing: same worker → same thread
let thread_id = compute_thread_id("worker-id", num_threads);
execute_pinned("worker-id", script, ops, task).await?;
```

**Benefits over shared pool:**

- Zero cross-thread contention
- Better tenant isolation (sticky routing)
- 34% faster warm cache

**Trade-off:** Higher memory (isolates duplicated across threads).

### Benchmark Comparison

| Scenario       | Shared Pool | Thread-Pinned     |
| -------------- | ----------- | ----------------- |
| Warm cache     | 0.64ms      | **0.48ms** (+34%) |
| CPU-bound      | 1.73ms      | 1.80ms            |
| With I/O (5ms) | 7.95ms      | 7.92ms            |

Under high contention (many threads, few isolates), shared pool can degrade to **worse than no pooling**. Thread-pinned avoids this.

---

## Decision Tree

```
Need maximum isolation? → Worker
High throughput (>100 req/s)? → IsolatePool
Multi-tenant security critical? → Thread-Pinned Pool
Legacy code using SharedIsolate? → Keep it (but migrate)
```

## Thread Safety

| Mode          | Locks                  | Guarantee             |
| ------------- | ---------------------- | --------------------- |
| Worker        | None                   | Each request isolated |
| SharedIsolate | `Mutex<SharedIsolate>` | Thread-local          |
| IsolatePool   | `Mutex` + `v8::Locker` | V8-level exclusion    |
| Thread-Pinned | `Mutex` (local)        | Zero cross-thread     |

V8 isolates are **not thread-safe**. `v8::Locker` provides mutual exclusion at the V8 level—required for any cross-thread sharing.
