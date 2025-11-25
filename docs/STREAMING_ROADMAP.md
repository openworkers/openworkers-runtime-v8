# Streaming Implementation Roadmap

## Overview

Transform the runtime from "string-based" to "stream-based" to support:
- ReadableStream / WritableStream
- Efficient fetch forward without memory copying
- Response streaming for large files
- Backpressure and flow control

---

## Phase 1: Bytes Support (instead of String) ✅

**Difficulty:** ⭐⭐☆☆☆
**Status:** Completed

### Objectives
- Replace `body: String` with `body: Bytes`
- Native binary support (images, videos, etc.)
- No forced UTF-8 conversion

### Changes

#### 1.1 Modify FetchResponse
```rust
// src/runtime/fetch/mod.rs
use bytes::Bytes;

pub struct FetchResponse {
    pub status: u16,
    pub status_text: String,
    pub headers: HashMap<String, String>,
    pub body: Bytes,  // ← instead of String
}
```

#### 1.2 Modify HttpResponse
```rust
// src/task.rs
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Bytes>,
}
```

#### 1.3 Adapt execute_fetch
```rust
// src/runtime/fetch/request.rs
let body = response
    .bytes()  // ← instead of .text()
    .await
    .map_err(|e| format!("Failed to read body: {}", e))?;
```

#### 1.4 Adapt create_response_object
```rust
// src/runtime/fetch/response.rs
// Create ArrayBuffer instead of String
let body_buf = v8::ArrayBuffer::new(scope, response.body.len());
// ... copy bytes to backing store
let uint8_array = v8::Uint8Array::new(scope, body_buf, 0, response.body.len());
```

### Tests Phase 1
```javascript
// Binary test
const response = await fetch('https://httpbin.org/bytes/1000');
const buffer = await response.arrayBuffer();
console.assert(buffer.byteLength === 1000);
```

---

## Phase 2: Basic ReadableStream (JS) ✅

**Difficulty:** ⭐⭐⭐☆☆
**Status:** Completed

### Objectives
- Implement ReadableStream API (WHATWG spec)
- Support methods: `getReader()`, `read()`, `cancel()`
- Integration with Response

### Implementation

See `src/runtime/streams.rs` for the full implementation including:
- `ReadableStream` class
- `ReadableStreamDefaultController`
- `ReadableStreamDefaultReader`
- `pull()` support for native streams

### Tests Phase 2
```javascript
// Test ReadableStream
const stream = new ReadableStream({
    start(controller) {
        controller.enqueue(new Uint8Array([1, 2, 3]));
        controller.enqueue(new Uint8Array([4, 5, 6]));
        controller.close();
    }
});

const reader = stream.getReader();
const { done, value } = await reader.read();
console.assert(!done);
console.assert(value[0] === 1);
```

---

## Phase 3: Rust ↔ JS Bridge ✅

**Difficulty:** ⭐⭐⭐⭐☆
**Status:** Completed

### Objectives
- Create "ops" to communicate between Rust and JS streams
- Allow Rust to "push" chunks to JS
- Handle backpressure via pull-based model

### Architecture

```
Rust Stream (reqwest)
    ↓
  write_chunk()
    ↓
  mpsc::channel
    ↓
  StreamManager.read_chunk() [async]
    ↓
  CallbackMessage::StreamChunk
    ↓
  JS callback
    ↓
  controller.enqueue()
    ↓
  reader.read()
```

### Implementation

See `docs/streams.md` for complete documentation.

Key components:
- `StreamManager` (`src/runtime/stream_manager.rs`)
- `__nativeStreamRead()` / `__nativeStreamCancel()` (`src/runtime/bindings.rs`)
- `__createNativeStream()` helper
- `StreamRead` / `StreamChunk` messages (`src/runtime/mod.rs`)

### Tests Phase 3
```rust
// tests/native_stream_test.rs
- test_native_stream_bridge    // Complete flow with multiple chunks
- test_native_stream_cancel    // Stream cancellation
- test_native_stream_error     // Error propagation
```

---

## Phase 4: Fetch Forward with Streaming ✅

**Difficulty:** ⭐⭐⭐☆☆
**Status:** Completed

### Objectives
- Fetch returns a Response with body = ReadableStream
- Support direct forward: `event.respondWith(fetch(url))`
- No intermediate buffer

### Implementation

Key components:
- `execute_fetch_streaming()` (`src/runtime/fetch/request.rs`)
- `FetchResponseMeta` (`src/runtime/fetch/mod.rs`)
- `__nativeFetchStreaming` (`src/runtime/bindings.rs`)
- Updated `fetch()` JS wrapper to use streaming by default

#### 4.1 execute_fetch_streaming function
```rust
pub async fn execute_fetch_streaming(
    request: FetchRequest,
    stream_manager: Arc<StreamManager>,
) -> Result<(FetchResponseMeta, StreamId), String> {
    let response = client.send().await?;

    let status = response.status().as_u16();
    let headers = extract_headers(&response);

    // Create a stream
    let stream_id = stream_manager.create_stream(url);

    // Spawn task to stream the body
    let manager = stream_manager.clone();
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if manager.write_chunk(stream_id, StreamChunk::Data(bytes)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = manager.write_chunk(stream_id, StreamChunk::Error(e.to_string()));
                    break;
                }
            }
        }
        let _ = manager.write_chunk(stream_id, StreamChunk::Done);
    });

    Ok((FetchResponseMeta { status, headers }, stream_id))
}
```

#### 4.2 Fetch returns Response with stream
```javascript
globalThis.fetch = function(url, options) {
    return new Promise((resolve, reject) => {
        const fetchOptions = {
            url: url,
            method: options?.method || 'GET',
            headers: options?.headers || {},
            body: options?.body || null
        };

        __nativeFetchStreaming(fetchOptions, (meta) => {
            // meta = {status, statusText, headers, streamId}
            const stream = __createNativeStream(meta.streamId);
            const response = new Response(stream, {
                status: meta.status,
                headers: meta.headers
            });
            response.ok = meta.status >= 200 && meta.status < 300;
            response.statusText = meta.statusText;
            resolve(response);
        }, reject);
    });
};
```

### Tests Phase 4
```rust
// tests/fetch_streaming_test.rs
- test_fetch_streaming_basic    // Basic fetch with streaming body
- test_fetch_streaming_chunked  // Reading body chunk by chunk
- test_fetch_forward            // Direct forward scenario
```

```javascript
// Direct forward
addEventListener('fetch', event => {
    event.respondWith(fetch('https://api.example.com/data'));
    // ✅ No buffer, direct streaming!
});

// Process by chunks
addEventListener('fetch', async event => {
    const response = await fetch('https://api.example.com/large-file');
    const reader = response.body.getReader();

    let total = 0;
    while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        total += value.length;
    }

    event.respondWith(new Response(`Received ${total} bytes`));
});
```

---

## Phase 5: Response.body Streaming

**Difficulty:** ⭐⭐⭐☆☆
**Status:** Pending

### Objectives
- Worker can return a Response with a stream
- Support progressive content generation

### Implementation

```javascript
// Generate content progressively
addEventListener('fetch', event => {
    const stream = new ReadableStream({
        async start(controller) {
            for (let i = 0; i < 10; i++) {
                await sleep(100);
                controller.enqueue(new TextEncoder().encode(`Chunk ${i}\n`));
            }
            controller.close();
        }
    });

    event.respondWith(new Response(stream, {
        headers: { 'Content-Type': 'text/plain' }
    }));
});
```

---

## Phase 6: Optimizations

**Difficulty:** ⭐⭐⭐⭐⭐
**Status:** Pending

### 6.1 Zero-Copy Transfer
```rust
// Use v8::BackingStore to avoid copies
let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(bytes.to_vec());
let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store);
```

### 6.2 Backpressure
```rust
// Limit buffer size
pub struct StreamResource {
    tx: mpsc::Sender<StreamChunk>, // with limit
    high_water_mark: usize,
}
```

### 6.3 Chunk Size Tuning
```rust
// Adapt chunk size based on network
const OPTIMAL_CHUNK_SIZE: usize = 64 * 1024; // 64KB
```

---

## Benefits Summary

| Before | After |
|--------|-------|
| Body as String | Body as Bytes/Stream |
| Everything in memory | Progressive streaming |
| 100MB = 100MB RAM | 100MB = ~64KB RAM (buffer) |
| No forward | Efficient direct forward |
| Text only | Binary supported |
| No backpressure | Backpressure handled |

---

## Prioritization

**Recommended order:**
1. **Phase 1** (critical) - Bytes support ✅
2. **Phase 2** (important) - ReadableStream JS ✅
3. **Phase 3** (complex) - Complete bridge ✅
4. **Phase 4** (quick win) - Basic fetch forward ✅
5. **Phase 5** (nice to have) - Response streaming
6. **Phase 6** (optimization) - Zero-copy, etc.

**MVP (Minimum Viable Product):**
- Phase 1 + Phase 2 = Basic stream support ✅

**Production-Ready:**
- Phases 1-4 = Efficient fetch forward ✅

**Complete:**
- All phases = Full Workers compatibility
