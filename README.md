# openworkers-runtime-v8

A minimal JavaScript runtime for OpenWorkers based on [rusty_v8](https://github.com/denoland/rusty_v8) - Rust bindings for Google's V8 JavaScript engine.

## Features

- ✅ **V8 Engine** - Production-grade JavaScript engine from Google
- ✅ **Minimal overhead** - Lightweight wrapper over rusty_v8
- ✅ **Event handlers** - addEventListener('fetch'), addEventListener('scheduled')
- ✅ **Simple Response API** - Basic HTTP response handling
- ✅ **Console logging** - console.log/warn/error

## Installation

```toml
[dependencies]
openworkers-runtime-v8 = "0.1"
```

## Usage

```rust
use openworkers_runtime_v8::{Worker, Script, Task, HttpRequest};
use std::collections::HashMap;

#[tokio::main]
async fn main() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from V8!'));
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

## Supported APIs

### Current (v0.1)
- ✅ `addEventListener('fetch', handler)` - Handle HTTP requests
- ✅ `event.respondWith(response)` - Send HTTP response
- ✅ `new Response(body, options)` - Create HTTP responses
- ✅ `console.log/warn/error` - Basic logging
- ✅ `URL` - Basic URL parsing

### In Progress
- ⏳ `setTimeout/setInterval` - Timer implementation in progress
- ⏳ `fetch()` - External HTTP requests planned
- ⏳ Full event loop integration

### Planned
- 📋 V8 Snapshots for faster cold start
- 📋 Full Web APIs compatibility
- 📋 Request/Response/Headers complete implementation

## Examples

```bash
cargo run --example simple_test
```

## Architecture

Built on rusty_v8 for maximum performance and minimal abstraction over V8.

## License

MIT License - See LICENSE file.

## Credits

Built on [rusty_v8](https://github.com/denoland/rusty_v8) by the Deno team.
