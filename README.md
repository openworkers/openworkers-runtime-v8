# OpenWorkers Runtime V8

V8-based JavaScript runtime for serverless workers, built on [rusty_v8](https://github.com/denoland/rusty_v8).

## Quick Start

```rust
use openworkers_runtime_v8::{init_pool, execute_pooled, RuntimeLimits, Script, Event};

// Initialize pool once at startup
init_pool(1000, RuntimeLimits::default());

// Execute workers
let script = Script::new(r#"
    addEventListener('fetch', event => {
        event.respondWith(new Response('Hello!'));
    });
"#);

execute_pooled("worker-id", script, ops, event).await?;
```

## Features

- **Isolate pooling** — Fast warm start (~µs), cold start with snapshot (~µs)
- **Streaming** — ReadableStream with backpressure
- **Web APIs** — fetch, setTimeout, Response, Request, URL, console
- **Async/await** — Full Promise support

## Performance

| Mode        | Cold Start | Warm Start |
| ----------- | ---------- | ---------- |
| IsolatePool | ~µs        | Fastest    |
| Worker      | ~ms        | ~ms        |

## Documentation

- [Architecture](docs/architecture.md) — System overview
- [Execution Modes](docs/execution_modes.md) — Pool vs Worker vs SharedIsolate
- [Isolate Pool](docs/isolate_pool.md) — Pool implementation details
- [Streams](docs/streams.md) — Streaming architecture

## Development Setup

This crate uses [openworkers-v8](https://crates.io/crates/openworkers-v8), a fork of rusty_v8 with Locker/UnenteredIsolate support for isolate pooling. V8 binaries are downloaded automatically during build.

## Testing

```bash
# From openworkers-runner (not this crate directly)
cargo test --features v8
```

## License

MIT
