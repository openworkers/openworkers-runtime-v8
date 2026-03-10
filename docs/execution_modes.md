# Execution Modes

Two ways to run JavaScript in the V8 runtime.

## Comparison

| Mode       | Cold Start | Warm Start | Thread Model             | Use Case      |
| ---------- | ---------- | ---------- | ------------------------ | ------------- |
| **Pool**   | Few ms     | Sub-µs     | Thread-local, v8::Locker | Production    |
| **Worker** | Few ms     | Few ms     | Single, per-request      | Max isolation |

A cross-thread Shared Pool (`execute_pooled()`) existed previously but was removed.
It used a global `Mutex<LruCache>` + `v8::Locker` for cross-thread isolate sharing,
but degraded under contention (9x slower than Worker in extreme cases).
See [architecture.md](./architecture.md) for historical benchmark data.

## Pool (Thread-Pinned)

Thread-local isolate pool with per-owner isolation. This is the production mode.

Two implementations selected at compile time:

- **Simple** (default): One request per isolate (exclusive access).
- **Multiplexed** (`--features multiplexing`): Multiple concurrent requests per isolate
  via `AsyncWaiter` fair FIFO queue.

```rust
use openworkers_runtime_v8::{init_pinned_pool, execute_pinned, PinnedPoolConfig, PinnedExecuteRequest};

// Initialize once at startup
init_pinned_pool(PinnedPoolConfig {
    max_per_thread: 100,
    max_per_owner: Some(2),
    max_concurrent_per_isolate: 20,
    max_cached_contexts: 10,
    limits: RuntimeLimits::default(),
});

// Execute worker
execute_pinned(PinnedExecuteRequest {
    owner_id: "tenant-1".into(),
    worker_id: "worker-123".into(),
    version: 1,
    script,
    ops,
    task,
    on_warm_hit: None,
    env_updated_at: None,
}).await?;
```

**Benefits:**

- Zero cross-thread contention (thread-local pools)
- Per-owner isolation (isolates tagged with owner_id)
- Per-owner concurrency limits (prevents monopolization)
- FIFO queue with backpressure (503 when overloaded)
- LRU eviction for cached contexts (stale versions evicted)

**Trade-off:** Higher memory (isolates duplicated across threads).

### Warm Context Reuse

When the same `worker_id + version` hits the same isolate, the `ExecutionContext` is
**reused** instead of recreated. This is the "warm hit" path.

**What's reused:** The V8 isolate, compiled bytecode, the tokio event loop task.

**What's reset** (via `ctx.reset()`): JS globals (`__lastResponse`, `__requestComplete`,
`__activeResponseStreams`, `__taskResult`), abort flag, stream manager, stale callbacks,
V8 callback storage (Global handles).

**Performance:** Cold context creation ~6ms, warm reset ~0.1ms.

**Safety invariant:** On any error during `exec()` or `drain_waituntil()`, the context
is discarded (not put back in cache). A max reuse count (`CONTEXT_MAX_REUSES`, default 1000)
prevents unbounded memory growth.

### waitUntil: Two-Phase Completion

`waitUntil` lets workers do background work after sending the response. The two execution
paths handle this differently because of V8's threading constraints.

**The core constraint:** V8 microtasks (promises, timer callbacks) must be pumped on the
thread that owns the isolate, via `await_event_loop` -> `drain_and_process`. If nobody
pumps, JS callbacks never fire.

#### Worker (oneshot) - single phase

```
exec(task)
  -> trigger_fetch_handler()
  -> await_event_loop(ResponseReady)       // wait for Response object
  -> res_tx.send(response)                 // HTTP response sent to client HERE
  -> await_event_loop(FullyComplete)       // pump V8 for waitUntil + streams
  -> return
```

`exec()` blocks for the full waitUntil duration, but the **HTTP client** gets the response
immediately (via `res_tx`). Since the Worker is disposable, blocking `exec()` has no impact.

#### ExecutionContext (warm reuse) - two phases

```
Phase 1: exec(task)
  -> trigger_fetch_handler()
  -> await_event_loop(ResponseReady)       // wait for Response object
  -> res_tx.send(response)                 // HTTP response sent to client HERE
  -> await_event_loop(StreamsComplete)     // wait for streaming body only
  -> return Ok(())                         // exec() returns, waitUntil still pending

Phase 2: drain_waituntil()
  -> create fresh TimeoutGuard + CpuEnforcer
  -> await_event_loop(FullyComplete)       // pump V8 for remaining waitUntil work
  -> return Ok(()) or Err(timeout)
```

The caller in `pool.rs` orchestrates both phases:

```rust
let result = cached.ctx.exec(task).await;        // phase 1 - response already sent

if result.is_ok() {
    if let Err(reason) = cached.ctx.drain_waituntil().await {  // phase 2
        // waitUntil timed out or threw -> discard context
        cached_context = None;
    }
}
```

**Why two phases?** `exec()` returns early so the pool knows the request is done. The
`drain_waituntil()` call has its own fresh security guards (wall clock, CPU time), so
background work can't run longer than the configured limits.

**Why not `tokio::spawn` the waitUntil?** V8 is not thread-safe. Microtask pumping
requires a `HandleScope` on the owning thread. Background work must run on the same
pinned thread.

#### EventLoopExit variants

| Variant           | Condition                                           | Used by                    |
| ----------------- | --------------------------------------------------- | -------------------------- |
| `ResponseReady`   | `__lastResponse` has a `status` property            | Both (wait for Response)   |
| `HandlerComplete` | `__requestComplete == true`                         | Task events                |
| `StreamsComplete` | `__activeResponseStreams == 0`                      | ExecutionContext (phase 1) |
| `FullyComplete`   | `__requestComplete && __activeResponseStreams == 0` | Worker, drain_waituntil    |

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

---

## Decision Tree

```
Production (multi-tenant)? -> Pool (warm reuse, zero contention)
Need maximum isolation?    -> Worker (oneshot, no state leakage)
```

## Thread Safety

| Mode   | Locks                | Guarantee             |
| ------ | -------------------- | --------------------- |
| Worker | None                 | Each request isolated |
| Pool   | `v8::Locker` (local) | Zero cross-thread     |

V8 isolates are **not thread-safe**. `v8::Locker` provides mutual exclusion at the V8 level.

## Code Pointers

| Mode   | Entry point        | Implementation                     |
| ------ | ------------------ | ---------------------------------- |
| Worker | `Worker::new()`    | `worker.rs`                        |
| Pool   | `execute_pinned()` | `pool.rs` + `execution_context.rs` |

Key methods for warm reuse:

| Method                                  | File                        | Role                                  |
| --------------------------------------- | --------------------------- | ------------------------------------- |
| `ExecutionContext::reset()`             | `execution_context.rs`      | Clear per-request state for reuse     |
| `ExecutionContext::drain_waituntil()`   | `execution_context.rs`      | Pump V8 for background promises       |
| `StreamManager::clear()`                | `runtime/stream_manager.rs` | Reset streams between requests        |
| `check_exit_condition(StreamsComplete)` | `execution_helpers.rs`      | Exit when streams done, not waitUntil |
