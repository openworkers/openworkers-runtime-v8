# Native Streaming Bridge: Architecture and Documentation

This document describes the implementation of the streaming bridge between Rust and JavaScript, a critical part of the runtime enabling efficient data transfer without full memory buffering.

## Overview

The bridge allows Rust to send data chunks to JavaScript asynchronously, and JavaScript to consume them via the standard `ReadableStream` API. This is essential for:

- **Fetch streaming**: receive HTTP responses chunk by chunk
- **Large files**: avoid loading 100MB+ into memory
- **Backpressure**: natural flow control (JS requests when ready)

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                           RUST SIDE                                  │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  ┌──────────────────┐     ┌─────────────────────────────────────┐   │
│  │  Data Source     │     │         StreamManager               │   │
│  │  (reqwest, etc.) │────▶│  - senders: HashMap<StreamId, Tx>   │   │
│  └──────────────────┘     │  - receivers: HashMap<StreamId, Rx> │   │
│                           │  - metadata: HashMap<StreamId, URL> │   │
│         write_chunk()     └─────────────────────────────────────┘   │
│              │                           │                          │
│              │                           │ read_chunk() [async]     │
│              ▼                           ▼                          │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                    mpsc::unbounded_channel                   │   │
│  │                  StreamChunk: Data | Done | Error            │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                      │
├──────────────────────────── EVENT LOOP ─────────────────────────────┤
│                                                                      │
│  scheduler_rx.recv()                                                │
│       │                                                             │
│       ▼                                                             │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │ SchedulerMessage::StreamRead(callback_id, stream_id)        │   │
│  │                                                              │   │
│  │   tokio::spawn(async {                                       │   │
│  │       let chunk = stream_manager.read_chunk(stream_id).await │   │
│  │       callback_tx.send(StreamChunk(callback_id, chunk))      │   │
│  │   })                                                         │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                      │
├──────────────────────────── V8 RUNTIME ─────────────────────────────┤
│                                                                      │
│  callback_rx.try_recv()                                             │
│       │                                                             │
│       ▼                                                             │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │ CallbackMessage::StreamChunk(callback_id, chunk)            │   │
│  │                                                              │   │
│  │   1. Retrieve callback from stream_callbacks[callback_id]   │   │
│  │   2. Create JS object: {done: bool, value?: Uint8Array}     │   │
│  │   3. Call callback(result)                                  │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                      │
└─────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────┐
│                        JAVASCRIPT SIDE                               │
├─────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  const stream = __createNativeStream(streamId);                     │
│  const reader = stream.getReader();                                 │
│                                                                      │
│  while (true) {                                                     │
│      const { done, value } = await reader.read();  ◀─── PULL       │
│      if (done) break;                                               │
│      // process value (Uint8Array)                                  │
│  }                                                                  │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │ ReadableStream with pull()                                   │   │
│  │                                                              │   │
│  │   async pull(controller) {                                   │   │
│  │       return new Promise((resolve) => {                      │   │
│  │           __nativeStreamRead(streamId, (result) => {         │   │
│  │               if (result.done) controller.close();           │   │
│  │               else controller.enqueue(result.value);         │   │
│  │               resolve();                                     │   │
│  │           });                                                │   │
│  │       });                                                    │   │
│  │   }                                                          │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

## Components

### 1. StreamManager (`src/runtime/stream_manager.rs`)

Central stream manager. Thread-safe via `Arc<Mutex<...>>`.

```rust
pub struct StreamManager {
    senders: Arc<Mutex<HashMap<StreamId, UnboundedSender<StreamChunk>>>>,
    receivers: Arc<Mutex<HashMap<StreamId, UnboundedReceiver<StreamChunk>>>>,
    metadata: Arc<Mutex<HashMap<StreamId, String>>>,
    next_id: Arc<Mutex<StreamId>>,
}
```

**Main methods:**

| Method | Description |
|--------|-------------|
| `create_stream(url)` | Creates a new stream, returns `StreamId` |
| `write_chunk(id, chunk)` | Writes a chunk (called by Rust) |
| `read_chunk(id).await` | Reads next chunk (called by scheduler) |
| `close_stream(id)` | Closes and cleans up the stream |

**Chunk types:**

```rust
pub enum StreamChunk {
    Data(Bytes),      // Binary data
    Done,             // End of stream
    Error(String),    // Error
}
```

### 2. Communication Messages (`src/runtime/mod.rs`)

**Scheduler → Event Loop:**

```rust
pub enum SchedulerMessage {
    // ... other messages ...
    StreamRead(CallbackId, StreamId),   // JS requests a chunk
    StreamCancel(StreamId),              // JS cancels the stream
}
```

**Event Loop → V8 Runtime:**

```rust
pub enum CallbackMessage {
    // ... other messages ...
    StreamChunk(CallbackId, StreamChunk),  // Chunk ready for JS
}
```

### 3. Native Functions (`src/runtime/bindings.rs`)

**`__nativeStreamRead(streamId, callback)`**

Native function called by JS to request the next chunk.

```
Flow:
1. JS calls __nativeStreamRead(streamId, callback)
2. Generates a unique callback_id
3. Stores callback in stream_callbacks[callback_id]
4. Sends StreamRead(callback_id, stream_id) to scheduler
5. (async) Scheduler reads and responds via StreamChunk
6. process_callbacks() calls callback({done, value})
```

**`__nativeStreamCancel(streamId)`**

Cancels a stream (called when JS does `reader.cancel()`).

**`__createNativeStream(streamId)`**

JS helper that creates a standard `ReadableStream` connected to the native bridge.

### 4. ReadableStream with `pull()` support (`src/runtime/streams.rs`)

The `ReadableStream` implementation supports the WHATWG spec `pull()` method:

```javascript
// When read() is called and queue is empty:
if (underlyingSource.pull) {
    // Call pull() to request more data
    underlyingSource.pull(controller);
}
```

This enables the "pull-based" pattern where JS requests data when needed (natural backpressure).

## Detailed Data Flow

### Nominal case: Reading a chunk

```
1. [JS] reader.read()
   │
   ▼
2. [JS] ReadableStream.read() sees empty queue
   │
   ▼
3. [JS] Calls pull(controller)
   │
   ▼
4. [JS] pull() calls __nativeStreamRead(streamId, callback)
   │
   ▼
5. [Rust/V8] __nativeStreamRead:
   - Generates callback_id = 42
   - Stores callback in stream_callbacks[42]
   - Sends StreamRead(42, streamId) via scheduler_tx
   │
   ▼
6. [Rust/Scheduler] run_event_loop receives StreamRead(42, 1):
   - Spawns async task
   - Calls stream_manager.read_chunk(1).await
   │
   ▼
7. [Rust/StreamManager] read_chunk():
   - Takes receiver from HashMap (temporarily)
   - Awaits rx.recv().await
   - Puts receiver back (unless Done/Error)
   - Returns chunk
   │
   ▼
8. [Rust/Scheduler] Task receives chunk:
   - Sends StreamChunk(42, Data(bytes)) via callback_tx
   │
   ▼
9. [Rust/V8] process_callbacks() receives StreamChunk(42, Data):
   - Retrieves callback from stream_callbacks[42]
   - Creates Uint8Array from bytes
   - Creates object {done: false, value: Uint8Array}
   - Calls callback(result)
   │
   ▼
10. [JS] Callback in pull():
    - controller.enqueue(result.value)
    - resolve() the Promise from pull()
    │
    ▼
11. [JS] ReadableStream._processQueue():
    - Takes item from queue
    - Resolves the Promise from read()
    │
    ▼
12. [JS] reader.read() returns {done: false, value: Uint8Array}
```

### Error case

```
1. [Rust] stream_manager.write_chunk(id, StreamChunk::Error("Network error"))
   │
   ▼
2. [JS] callback receives {error: "Network error"}
   │
   ▼
3. [JS] controller.error(new Error("Network error"))
   │
   ▼
4. [JS] reader.read() rejects with error
```

### Cancellation

```
1. [JS] reader.cancel("User cancelled")
   │
   ▼
2. [JS] ReadableStream.cancel() calls underlyingSource.cancel()
   │
   ▼
3. [JS] __nativeStreamCancel(streamId)
   │
   ▼
4. [Rust] Sends StreamCancel(streamId) to scheduler
   │
   ▼
5. [Rust] stream_manager.close_stream(streamId)
   - Removes sender, receiver, metadata
```

## Usage

### From Rust (create and feed a stream)

```rust
// Get the stream_manager
let stream_manager = worker.stream_manager();

// Create a stream
let stream_id = stream_manager.create_stream("https://api.example.com/data".to_string());

// Write chunks (typically in an async task)
tokio::spawn(async move {
    let mut response = reqwest::get(url).await?;

    while let Some(chunk) = response.chunk().await? {
        stream_manager.write_chunk(stream_id, StreamChunk::Data(chunk))?;
    }

    stream_manager.write_chunk(stream_id, StreamChunk::Done)?;
});

// Pass stream_id to JavaScript...
```

### From JavaScript (consume a stream)

```javascript
// Create a ReadableStream from a native stream_id
const stream = __createNativeStream(streamId);
const reader = stream.getReader();

// Read all chunks
let totalBytes = 0;
while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    totalBytes += value.length;
    // Process value (Uint8Array)
}

console.log(`Received ${totalBytes} bytes`);
```

### Integration with Response

```javascript
// Create a Response with a streamed body
const stream = __createNativeStream(streamId);
const response = new Response(stream, {
    headers: { 'Content-Type': 'application/octet-stream' }
});

// Use standard methods
const text = await response.text();
// or
const buffer = await response.arrayBuffer();
```

## Technical Considerations

### Thread Safety

- `StreamManager` is `Clone` and thread-safe via `Arc<Mutex<...>>`
- `mpsc::unbounded` channels are thread-safe
- Only one reader can read a stream at a time (WHATWG semantics)

### Memory Management

- `Bytes` uses reference counting (no copy)
- Receivers are temporarily removed during `read_chunk()` to avoid deadlocks
- Streams are automatically cleaned up after `Done` or `Error`

### "Take and put back" Pattern

```rust
pub async fn read_chunk(&self, stream_id: StreamId) -> Result<StreamChunk, String> {
    // 1. Take the receiver (only one reader at a time)
    let rx = {
        let mut receivers = self.receivers.lock().unwrap();
        receivers.remove(&stream_id)
    };

    if let Some(mut rx) = rx {
        // 2. Wait for next chunk
        let result = rx.recv().await;

        // 3. Put receiver back (unless stream is done)
        if let Some(ref chunk) = result {
            match chunk {
                StreamChunk::Done | StreamChunk::Error(_) => {
                    // Don't put back - stream is finished
                }
                StreamChunk::Data(_) => {
                    self.receivers.lock().unwrap().insert(stream_id, rx);
                }
            }
        }

        result.ok_or_else(|| "Stream closed".to_string())
    } else {
        Err("Stream not found or already being read".to_string())
    }
}
```

### Shared Callback ID

The `next_callback_id` is shared between fetch and streams. This guarantees ID uniqueness but means fetch and stream callbacks use separate HashMaps:

- `fetch_callbacks: HashMap<CallbackId, Global<Function>>`
- `stream_callbacks: HashMap<CallbackId, Global<Function>>`

## Tests

Tests are in `tests/native_stream_test.rs`:

| Test | Description |
|------|-------------|
| `test_native_stream_bridge` | Complete flow with multiple chunks |
| `test_native_stream_cancel` | Stream cancellation |
| `test_native_stream_error` | Error propagation |

## Next Steps (Phase 4)

The bridge is ready to implement **Fetch Forward with Streaming**:

```rust
// execute_fetch_streaming() that:
// 1. Creates a stream_id
// 2. Spawns a task that reads HTTP response chunk by chunk
// 3. Returns immediately with stream_id
// 4. JS can start reading before everything is downloaded
```

This will enable:

```javascript
addEventListener('fetch', async event => {
    // Direct forward without buffering
    event.respondWith(fetch('https://api.example.com/large-file'));
});
```
