# Architecture

V8-based JavaScript runtime for serverless workers.

## Module Hierarchy

```
platform.rs                 ← V8 Platform (singleton, once per process)
    │
    ├── Runtime             ← Full V8 engine + context + event loop
    │       └── Worker      ← Wrapper, creates isolate per request
    │
    ├── SharedIsolate       ← Reusable isolate (thread-local, legacy)
    │       └── ExecutionContext ← Disposable context on shared isolate
    │
    └── LockerManagedIsolate    ← Reusable isolate (multi-thread safe)
            └── IsolatePool     ← LRU cache with v8::Locker
```

## Core Structures

| Structure                | File                        | Purpose                         | Creates           |
| ------------------------ | --------------------------- | ------------------------------- | ----------------- |
| **Runtime**              | `runtime/mod.rs`            | V8 isolate + context + channels | New isolate       |
| **Worker**               | `worker.rs`                 | High-level API around Runtime   | New isolate/req   |
| **SharedIsolate**        | `shared_isolate.rs`         | Thread-local reusable isolate   | Once/thread       |
| **ExecutionContext**     | `execution_context.rs`      | Disposable context              | ~100µs            |
| **LockerManagedIsolate** | `locker_managed_isolate.rs` | Pool-compatible isolate         | Once/worker       |
| **IsolatePool**          | `isolate_pool.rs`           | Global LRU cache                | Manages lifecycle |

## Execution Modes

| Mode              | API                       | Performance | Use Case             |
| ----------------- | ------------------------- | ----------- | -------------------- |
| **Worker**        | `Worker::new()`           | ~2-3ms/req  | Max isolation, tests |
| **SharedIsolate** | `ExecutionContext::new()` | ~100µs/req  | Thread-local, legacy |
| **IsolatePool**   | `execute_pooled()`        | <10µs warm  | **Production**       |

See [execution_modes.md](./execution_modes.md) for details.

## Event Loop

All execution modes share the same event loop logic via `event_loop.rs`:

```rust
pub trait EventLoopRuntime {
    fn callback_rx_mut(&mut self) -> &mut Receiver<CallbackMessage>;
    fn process_callback(&mut self, msg: CallbackMessage);
    fn pump_and_checkpoint(&mut self);
}

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
Used by: Runtime          Used by: IsolatePool
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

| File                   | Lines | Purpose                       |
| ---------------------- | ----- | ----------------------------- |
| `runtime/mod.rs`       | ~800  | V8 setup, callback processing |
| `runtime/bindings.rs`  | ~600  | JS native functions           |
| `worker.rs`            | ~700  | Worker API, event loop        |
| `execution_context.rs` | ~500  | Pooled execution context      |
| `isolate_pool.rs`      | ~300  | LRU cache, v8::Locker         |
| `event_loop.rs`        | ~80   | Shared polling logic          |
| `platform.rs`          | ~20   | V8 platform singleton         |

## See Also

- [execution_modes.md](./execution_modes.md) — When to use each mode
- [isolate_pool.md](./isolate_pool.md) — Pool implementation details
- [streams.md](./streams.md) — Streaming architecture
