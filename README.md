# OpenWorkers Runtime V8

V8-based JavaScript runtime for serverless workers, built on [openworkers-v8](https://crates.io/crates/openworkers-v8) (fork of rusty_v8).

## Quick Start

```rust
use openworkers_runtime_v8::{
    init_pinned_pool, execute_pinned,
    PinnedPoolConfig, PinnedExecuteRequest, RuntimeLimits,
};

// Initialize pool once at startup
init_pinned_pool(PinnedPoolConfig {
    max_per_thread: 100,
    max_per_owner: Some(10),
    max_concurrent_per_isolate: 1,
    max_cached_contexts: 5,
    limits: RuntimeLimits::default(),
});

// Execute worker
execute_pinned(PinnedExecuteRequest {
    owner_id: "tenant-1".into(),
    worker_id: "worker-1".into(),
    version: 1,
    script,
    ops,
    task: event,
    on_warm_hit: None,
    env_updated_at: None,
}).await?;
```

## Execution Modes

### Pinned Pool (Recommended)

Thread-local isolate pool with per-owner isolation and warm context caching.
Two compile-time variants:

- **Simple** (default): 1 request per isolate (exclusive `AtomicBool`)
- **Multiplexed** (`--features multiplexing`): N concurrent requests per isolate via `AsyncWaiter` fair FIFO queue

### Worker (Maximum Isolation)

Fresh isolate per request. No caching, no Locker. Best for debugging or maximum security.

```rust
let mut worker = Worker::new_with_ops(script, limits, ops).await?;
worker.exec(event).await?;
```

## Performance

| Mode        | Cold Start  | Warm Start |
| ----------- | ----------- | ---------- |
| Pinned Pool | Tens of µs  | Sub-µs     |
| Worker      | Few ms      | Few ms     |

## Features

- **Isolate pooling** with LRU context caching
- **Streaming** with ReadableStream and backpressure
- **Web APIs** — fetch, setTimeout, Response, Request, URL, console
- **Async/await** — Full Promise support
- **Code cache** — V8 code cache for faster cold starts

## Testing

```bash
# From openworkers-runner (not this crate directly)
cargo test --features v8
```

## License

MIT
