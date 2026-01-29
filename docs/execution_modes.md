# Execution Modes

Three ways to run JavaScript in the V8 runtime.

## Comparison

| Mode              | Cold Start  | Warm Start | Thread Model             | Use Case         |
| ----------------- | ----------- | ---------- | ------------------------ | ---------------- |
| **IsolatePool**   | Tens of µs  | Sub-µs     | Multi-thread, v8::Locker | High throughput  |
| **Worker**        | Few ms      | Few ms     | Single, per-request      | Max isolation    |
| **SharedIsolate** | Tens of µs  | Tens of µs | Thread-local             | Legacy           |

## IsolatePool

Global LRU cache of isolates, accessible from any thread via `v8::Locker`.

```rust
use openworkers_runtime_v8::{init_pool, execute_pooled};

// Once at startup
init_pool(1000, RuntimeLimits::default());

// Per request
execute_pooled("worker-id", script, ops, event).await?;
```

**How it works:**

1. `acquire(worker_id)` — Get isolate from cache (or create)
2. `v8::Locker` — Lock isolate for current thread
3. Execute JavaScript
4. Unlock, return to pool

**Cache behavior:**

- Hit → reuse existing isolate (sub-µs)
- Miss → create new (tens of µs with snapshot, few ms without)
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

## Thread-Pinned Pool

For multi-tenant scenarios, each thread owns its local pool with per-owner isolation.

```rust
use openworkers_runtime_v8::{init_pinned_pool_full, execute_pinned};

// 100 isolates/thread, max 2/owner, queue of 10, 5s timeout
init_pinned_pool_full(100, Some(2), 10, 5000, limits);

// Round-robin routing at caller level (not sticky!)
// Same owner can use isolates on ANY thread
execute_pinned("owner-id", script, ops, task).await?;
```

**Benefits:**

- Zero cross-thread contention (thread-local pools)
- Per-owner isolation (isolates tagged with owner_id)
- Per-owner concurrency limits (prevents monopolization)
- FIFO queue with backpressure (503 when overloaded)
- Round-robin distribution (no hot spots)

**Trade-off:** Higher memory (isolates duplicated across threads).

### Benchmark Comparison

| Scenario       | Shared Pool | Thread-Pinned |
| -------------- | ----------- | ------------- |
| Warm cache     | Fast        | **Faster**    |
| CPU-bound      | Similar     | Similar       |
| With I/O       | Similar     | Similar       |

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

## Code Pointers

| Mode          | Entry point          | Implementation               |
| ------------- | -------------------- | ---------------------------- |
| Worker        | `Worker::new()`      | `worker.rs`                  |
| IsolatePool   | `execute_pooled()`   | `pooled_execution.rs` → `isolate_pool.rs` |
| Thread-Pinned | `execute_pinned()`   | `thread_pinned_pool.rs`      |
| SharedIsolate | `ExecutionContext::new()` | `execution_context.rs` + `shared_isolate.rs` |
