# Architecture

V8-based JavaScript runtime for serverless workers.

## Module Hierarchy

```
platform.rs                     ← V8 Platform (singleton, once per process)
    │
    ├── Runtime                 ← Full V8 engine + context + event loop
    │       └── Worker          ← Wrapper, creates isolate per request
    │
    └── LockerManagedIsolate    ← Reusable isolate (multi-thread safe)
            └── Pool            ← Thread-local pool with warm context reuse
```

## Core Structures

| Structure                | File                        | Purpose                              | Creates         |
| ------------------------ | --------------------------- | ------------------------------------ | --------------- |
| **Runtime**              | `runtime/mod.rs`            | V8 isolate + context + channels      | New isolate     |
| **Worker**               | `worker.rs`                 | High-level API around Runtime        | New isolate/req |
| **ExecutionContext**     | `execution_context.rs`      | Disposable context on pooled isolate | Tens of µs      |
| **LockerManagedIsolate** | `locker_managed_isolate.rs` | Pool-compatible isolate              | Once/worker     |
| **Pool**                 | `pool.rs`                   | Thread-local pool + warm cache       | Per thread      |
| **RequestContext**       | `request_context.rs`        | Per-request V8 context state         | Per request     |

## Execution Modes

| Mode       | API                | Performance   | Use Case                 |
| ---------- | ------------------ | ------------- | ------------------------ |
| **Pool**   | `execute_pinned()` | Sub-µs (warm) | Production, multi-tenant |
| **Worker** | `Worker::new()`    | Few ms/req    | Max isolation, tests     |

**Code pointers:**

- `execute_pinned()` → `pool.rs`
- `Worker` → `worker.rs`

See [execution_modes.md](./execution_modes.md) for details.

## Benchmarks

Historical benchmark (4 OS threads x 20 requests = 80 total, three architectures):

```
┌─────────────────┬──────────────┬─────────────────┐
│ Architecture    │  Throughput  │ vs Worker       │
├─────────────────┼──────────────┼─────────────────┤
│ Worker          │  1,474 req/s │ baseline        │
│ Shared Pool *   │  4,851 req/s │ +229%           │
│ Thread-Pinned   │  5,766 req/s │ +291%           │
└─────────────────┴──────────────┴─────────────────┘

* Shared Pool (cross-thread via global Mutex + v8::Locker) has been removed.
  It degraded under contention: 9x slower than Worker in extreme cases (8 threads, 1 isolate).
  Thread-Pinned (now the only pool mode) avoids this entirely with thread-local pools.
```

Detailed results (runtime-v8, real workers):

| Test           | Worker    | Shared Pool \* | Thread-Pinned   |
| -------------- | --------- | -------------- | --------------- |
| CPU-bound      | 400 req/s | 578 req/s      | 556 req/s       |
| Standard (5ms) | 118 req/s | 126 req/s      | 126 req/s       |
| Warm cache     | N/A       | 1,551 req/s    | **2,077 req/s** |

Synthetic results (raw V8, no runtime overhead):

| Test          | Worker | Shared Pool \* | Thread-Pinned |
| ------------- | ------ | -------------- | ------------- |
| CPU 8t/4i     | 9,945  | 5,423          | **10,450**    |
| Extreme 8t/1i | 10,083 | **1,094**      | **19,511**    |

Run current benchmarks with:

```bash
cargo test --test worker_vs_pool_bench -- --nocapture --test-threads=1
```

## Event Loop

All execution modes share the same event loop logic via `event_loop.rs`:

```rust
pub trait EventLoopRuntime {
    fn callback_rx_mut(&mut self) -> &mut Receiver<CallbackMessage>;
    fn process_callback(&mut self, msg: CallbackMessage);
    fn pump_and_checkpoint(&mut self);
}

// Implemented by Runtime and ExecutionContext
// Used by Worker, ExecutionContext, WorkerFuture
drain_and_process(cx, runtime, buffer) -> Result<()>
```

**Flow:**

1. Poll callback channel (waker-based, true async)
2. Batch process all received callbacks
3. Pump V8 platform + microtask checkpoint

## V8 Threading Model

```
OwnedIsolate              UnenteredIsolate + Locker
─────────────             ─────────────────────────
Auto-enters thread        No auto-enter
Single-thread only        Any thread can lock
Used by: Runtime          Used by: Pool
```

**Why two types?** V8 isolates are single-threaded. `OwnedIsolate` binds to one thread. `UnenteredIsolate` + `v8::Locker` allows any thread to temporarily own the isolate—essential for pooling.

## Data Flow

```
                    ┌─────────────────────────────────┐
                    │         JavaScript              │
                    │  fetch(), setTimeout(), etc.    │
                    └───────────────┬─────────────────┘
                                    │ native call
                                    ▼
┌────────────────────────────────────────────────────────────────┐
│                        Runtime                                 │
│  scheduler_tx ─────────────────────────────► scheduler_rx      │
│       │                                           │            │
│       │  SchedulerMessage::Fetch(id, url)         │            │
│       │  SchedulerMessage::Timer(id, delay)       │            │
│       │  SchedulerMessage::StreamRead(id, sid)    │            │
│       │                                           ▼            │
│       │                                    ┌───────────┐       │
│       │                                    │ Scheduler │       │
│       │                                    │  (tokio)  │       │
│       │                                    └─────┬─────┘       │
│       │                                          │             │
│       │  CallbackMessage::FetchDone(id, resp)    │             │
│       │  CallbackMessage::TimerFired(id)         │             │
│       │  CallbackMessage::StreamChunk(id, data)  │             │
│       │                                          │             │
│  callback_rx ◄───────────────────────────────────┘             │
│       │                                                        │
│       ▼                                                        │
│  process_callback() → call JS function                         │
└────────────────────────────────────────────────────────────────┘
```

## Key Files

| File                        | Lines | Purpose                              |
| --------------------------- | ----- | ------------------------------------ |
| `runtime/mod.rs`            | ~780  | V8 setup, callback processing        |
| `runtime/bindings/`         | ~2600 | JS native functions (folder)         |
| `worker.rs`                 | ~1800 | Worker API, event loop               |
| `execution_context.rs`      | ~1500 | Pooled execution context, warm reuse |
| `pool.rs`                   | ~1090 | Thread-local pool, warm cache        |
| `execution_helpers.rs`      | ~490  | Shared helpers, EventLoopExit        |
| `async_waiter.rs`           | ~210  | Fair FIFO queue (multiplexing)       |
| `locker_managed_isolate.rs` | ~200  | UnenteredIsolate + Locker wrapper    |
| `request_context.rs`        | ~100  | Per-request V8 context state         |
| `event_loop.rs`             | ~80   | Shared polling logic (trait)         |
| `platform.rs`               | ~80   | V8 platform singleton                |

## See Also

- [execution_modes.md](./execution_modes.md) — When to use each mode
- [streams.md](./streams.md) — Streaming architecture
- [testing.md](./testing.md) — V8 testing constraints
