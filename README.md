# OpenWorkers Runtime - V8

A JavaScript runtime for OpenWorkers based on [rusty_v8](https://github.com/denoland/rusty_v8) - Rust bindings for Google's V8 JavaScript engine.

## Features

- ✅ **V8 Engine** - Google's high-performance JavaScript engine
- ✅ **Async/Await** - Full Promise support with microtask processing
- ✅ **Timers** - setTimeout, setInterval, clearTimeout, clearInterval
- ✅ **Fetch API** - HTTP requests to external APIs
- ✅ **Event Handlers** - addEventListener('fetch'), addEventListener('scheduled')
- ✅ **Console Logging** - console.log/warn/error
- ✅ **URL API** - Basic URL parsing

## Performance

Run benchmark:
```bash
cargo run --example benchmark --release
```

### Results (Apple Silicon, Release Mode)

```
Worker::new(): avg=1.9ms, min=977µs, max=4.4ms
exec():        avg=96µs, min=58µs, max=169µs
Total:         avg=2.0ms, min=1.0ms, max=4.6ms
```

### Runtime Comparison

| Runtime | Engine | Worker::new() | exec() | Total | Language |
|---------|--------|---------------|--------|-------|----------|
| **[V8](https://github.com/openworkers/openworkers-runtime-v8)** | V8 | 1.9ms | **96µs** ⚡ | 2.0ms | Rust + C++ |
| **[JSC](https://github.com/openworkers/openworkers-runtime-jsc)** | JavaScriptCore | 0.5ms* | 400µs | 0.9ms | Rust + C |
| **[Boa](https://github.com/openworkers/openworkers-runtime-boa)** | Boa | 1.1ms | 610µs | 1.7ms | 100% Rust |
| **[Deno](https://github.com/openworkers/openworkers-runtime)** | V8 + Deno | 21.9ms | 774µs | 22.7ms | Rust + C++ |

*JSC has ~41ms warmup on first run, then stabilizes

**V8 has the fastest exec() time**, making it ideal for high-throughput scenarios.

## Installation

```toml
[dependencies]
openworkers-runtime-v8 = { path = "../openworkers-runtime-v8" }
```

## Usage

```rust
use openworkers_runtime_v8::{Worker, Script, Task, HttpRequest};
use std::collections::HashMap;

#[tokio::main]
async fn main() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Async handler with fetch forward
            const response = await fetch('https://api.example.com/data');
            event.respondWith(response);
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = rx.await.unwrap();
    println!("Status: {}", response.status);
}
```

## Testing

```bash
# Run all tests (16 tests)
cargo test

# Run with output
cargo test -- --nocapture

# Run benchmarks
cargo test --test benchmark_test -- --nocapture
```

### Test Coverage

- **Basic Tests** (3) - Response handling, status codes, error cases
- **Timers & Fetch** (8) - Async operations, timers, fetch forward, Promises
- **Benchmarks** (5) - Performance metrics

**Total: 16 tests** ✅

## Supported JavaScript APIs

### Timers
- `setTimeout(callback, delay)` - Execute callback after delay
- `setInterval(callback, interval)` - Execute callback repeatedly
- `clearTimeout(id)` - Cancel timeout
- `clearInterval(id)` - Cancel interval

### Fetch API
- `fetch(url, options)` - HTTP requests (GET, POST, PUT, DELETE, PATCH, HEAD)
- Promise-based with async/await support
- Custom headers and body
- Full response handling

### Other APIs
- `console.log/warn/error` - Logging
- `URL` - Basic URL parsing
- `Response` - Create HTTP responses
- `addEventListener` - Event handling

## Architecture

```
src/
├── lib.rs              # Public API
├── worker.rs           # Worker with event handlers
├── task.rs             # Task types (Fetch, Scheduled)
├── compat.rs           # Compatibility layer
├── snapshot.rs         # V8 snapshot support (planned)
└── runtime/
    ├── mod.rs          # Runtime & event loop
    ├── bindings.rs     # JavaScript bindings (timers, fetch, console)
    └── fetch/          # Fetch implementation
```

## Key Implementation Details

### Microtask Processing

V8 requires explicit microtask processing for Promises:
```rust
isolate.perform_microtask_checkpoint();
```

This enables full async/await support.

### Adaptive Event Loop

The worker uses adaptive sleep for optimal performance:
- Immediate check for sync responses (<1ms)
- 1ms sleep for fast async (<100ms)
- 10ms sleep for long operations (up to 5s)

## Other Runtime Implementations

OpenWorkers supports multiple JavaScript engines:

- **[openworkers-runtime](https://github.com/openworkers/openworkers-runtime)** - Deno-based (V8 + Deno extensions)
- **[openworkers-runtime-jsc](https://github.com/openworkers/openworkers-runtime-jsc)** - JavaScriptCore
- **[openworkers-runtime-boa](https://github.com/openworkers/openworkers-runtime-boa)** - Boa (100% Rust)
- **openworkers-runtime-v8** - This runtime (V8 via rusty_v8)

## License

MIT License - See LICENSE file.

## Credits

Built on [rusty_v8](https://github.com/denoland/rusty_v8) by the Deno team.
