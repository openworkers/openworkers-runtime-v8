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
execute_pinned("owner-id", "worker-id", version, script, ops, task, Some(on_warm_hit)).await?;
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

### Warm Context Reuse

When the same `worker_id + version` hits the same isolate, the `ExecutionContext` is
**reused** instead of recreated. This is the "warm hit" path.

**What's reused:** The V8 isolate, compiled bytecode, the tokio event loop task.

**What's reset** (via `ctx.reset()`): JS globals (`__lastResponse`, `__requestComplete`,
`__activeResponseStreams`, `__taskResult`), abort flag, stream manager, stale callbacks,
V8 callback storage (Global handles).

**Performance:** Cold context creation ~6ms → warm reset ~0.1ms.

**Safety invariant:** On any error during `exec()` or `drain_waituntil()`, the context
is discarded (not put back in cache). A max reuse count (`CONTEXT_MAX_REUSES`, default 1000)
prevents unbounded memory growth.

### waitUntil: Two-Phase Completion

`waitUntil` lets workers do background work after sending the response. The two execution
paths handle this differently because of V8's threading constraints.

**The core constraint:** V8 microtasks (promises, timer callbacks) must be pumped on the
thread that owns the isolate, via `await_event_loop` → `drain_and_process`. If nobody
pumps, JS callbacks never fire.

#### Worker (oneshot) — single phase

```
exec(task)
  → trigger_fetch_handler()
  → await_event_loop(ResponseReady)       // wait for Response object
  → res_tx.send(response)                 // ← HTTP response sent to client HERE
  → await_event_loop(FullyComplete)       // pump V8 for waitUntil + streams
  → return
```

`exec()` blocks for the full waitUntil duration, but the **HTTP client** gets the response
immediately (via `res_tx`). Since the Worker is disposable, blocking `exec()` has no impact.

#### ExecutionContext (warm reuse) — two phases

```
Phase 1: exec(task)
  → trigger_fetch_handler()
  → await_event_loop(ResponseReady)       // wait for Response object
  → res_tx.send(response)                 // ← HTTP response sent to client HERE
  → await_event_loop(StreamsComplete)     // wait for streaming body only
  → return Ok(())                         // ← exec() returns, waitUntil still pending

Phase 2: drain_waituntil()
  → create fresh TimeoutGuard + CpuEnforcer
  → await_event_loop(FullyComplete)       // pump V8 for remaining waitUntil work
  → return Ok(()) or Err(timeout)
```

The caller in `thread_pinned_pool.rs` orchestrates both phases:

```rust
let result = cached.ctx.exec(task).await;        // phase 1 — response already sent

if result.is_ok() {
    if let Err(reason) = cached.ctx.drain_waituntil().await {  // phase 2
        // waitUntil timed out or threw → discard context
        cached_context = None;
    }
}
```

**Why two phases?** `exec()` returns early so the pool knows the request is done. The
`drain_waituntil()` call has its own fresh security guards (wall clock, CPU time), so
background work can't run longer than the configured limits.

**Why not `tokio::spawn` the waitUntil?** V8 is not thread-safe. Microtask pumping
requires a `HandleScope` on the owning thread. Background work must run on the same
pinned thread — it can't be offloaded.

#### EventLoopExit variants

| Variant           | Condition                                             | Used by                |
| ----------------- | ----------------------------------------------------- | ---------------------- |
| `ResponseReady`   | `__lastResponse` has a `status` property              | Both (wait for Response) |
| `HandlerComplete` | `__requestComplete == true`                           | Task events            |
| `StreamsComplete` | `__activeResponseStreams == 0`                         | ExecutionContext (phase 1) |
| `FullyComplete`   | `__requestComplete && __activeResponseStreams == 0`    | Worker, drain_waituntil |

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
| Thread-Pinned | `execute_pinned()`   | `thread_pinned_pool.rs` (warm reuse + `CachedContext`) |
| SharedIsolate | `ExecutionContext::new()` | `execution_context.rs` + `shared_isolate.rs` |

Key methods for warm reuse:

| Method                         | File                     | Role                                    |
| ------------------------------ | ------------------------ | --------------------------------------- |
| `ExecutionContext::reset()`    | `execution_context.rs`   | Clear per-request state for reuse       |
| `ExecutionContext::drain_waituntil()` | `execution_context.rs` | Pump V8 for background promises  |
| `StreamManager::clear()`      | `runtime/stream_manager.rs` | Reset streams between requests       |
| `check_exit_condition(StreamsComplete)` | `execution_helpers.rs` | Exit when streams done, not waitUntil |
