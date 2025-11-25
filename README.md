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
Simple Response:   avg=15.5µs, throughput=64k req/s
Async Response:    avg=83.6µs, throughput=11.9k req/s
Worker Creation:   avg=2.9ms, rate=342 workers/s
Complex Scenario:  avg=743µs, throughput=1346 req/s
```

### Runtime Comparison

| Runtime | Engine | Worker::new() | exec() | Total | Language |
|---------|--------|---------------|--------|-------|----------|
| **[V8](https://github.com/openworkers/openworkers-runtime-v8)** | V8 | 2.9ms | **15µs** ⚡ | ~3ms | Rust + C++ |
| **[JSC](https://github.com/openworkers/openworkers-runtime-jsc)** | JavaScriptCore | 495µs* | 434µs | **935µs** 🏆 | Rust + C |
| **[Boa](https://github.com/openworkers/openworkers-runtime-boa)** | Boa | 605µs | 441µs | 1.05ms | 100% Rust |
| **[Deno](https://github.com/openworkers/openworkers-runtime)** | V8 + Deno | 4.6ms | 1.07ms | 5.8ms | Rust + C++ |

*JSC has ~40ms warmup on first run, then stabilizes at 495µs

**V8 has the fastest exec() time** (15µs), making it ideal for high-throughput scenarios.

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

### V8 Platform Integration

Properly uses V8 Platform APIs like Deno:
- `v8::Platform::pump_message_loop()` - Process V8 internal tasks
- `perform_microtask_checkpoint()` - Process Promises with TryCatch
- Adaptive polling with early exit detection

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
