# Streaming Architecture

Rust ↔ JavaScript streaming bridge for efficient data transfer without full buffering.

## Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                         RUST                                     │
│                                                                  │
│  Data Source          StreamManager           Scheduler          │
│  (HTTP, body)  ──────► senders[id] ◄─────────► read_chunk()      │
│      │                 receivers[id]               │             │
│      │ write_chunk()   high_water_mark             │             │
│      ▼                 (backpressure)              ▼             │
│  mpsc::channel ────── StreamChunk::Data|Done|Error ─────────────►│
│                                                                  │
├──────────────────────────────────────────────────────────────────┤
│                       JAVASCRIPT                                 │
│                                                                  │
│  // Reading (fetch response body, request body)                  │
│  const stream = __createNativeStream(streamId);                  │
│  const reader = stream.getReader();                              │
│  const { done, value } = await reader.read();  ◄── pull()        │
│                                                                  │
│  // Writing (streaming response)                                 │
│  const streamId = __responseStreamCreate();                      │
│  __responseStreamWrite(streamId, chunk);                         │
│  __responseStreamEnd(streamId);                                  │
└──────────────────────────────────────────────────────────────────┘
```

## Components

### StreamManager (`runtime/stream_manager.rs`)

```rust
pub struct StreamManager {
    senders: Arc<Mutex<HashMap<StreamId, Sender<StreamChunk>>>>,
    receivers: Arc<Mutex<HashMap<StreamId, Receiver<StreamChunk>>>>,
    metadata: Arc<Mutex<HashMap<StreamId, String>>>,
    next_id: Arc<Mutex<StreamId>>,
    high_water_mark: usize,  // Channel capacity for backpressure
}

pub enum StreamChunk {
    Data(Bytes),
    Done,
    Error(String),
}
```

| Method                     | Called by | Purpose                            |
| -------------------------- | --------- | ---------------------------------- |
| `create_stream(url)`       | Rust      | Create bounded channel, return ID  |
| `write_chunk(id, chunk)`   | Rust      | Send data (async, backpressure)    |
| `try_write_chunk(id, chunk)` | Rust    | Send data (sync, fails if full)    |
| `read_chunk(id).await`     | Scheduler | Receive data for JS callback       |
| `take_receiver(id)`        | Rust      | Take receiver for HttpBody::Stream |
| `pump_request_body(rx)`    | Rust      | Bridge mpsc::Receiver to stream    |
| `close_stream(id)`         | Both      | Cleanup                            |

### Messages

```rust
// JS → Scheduler
SchedulerMessage::StreamRead(callback_id, stream_id)
SchedulerMessage::StreamCancel(stream_id)

// Scheduler → V8
CallbackMessage::StreamChunk(callback_id, StreamChunk)
```

### JavaScript API

```javascript
// Reading streams (fetch response, request body)
__nativeStreamRead(streamId, callback);
__nativeStreamCancel(streamId);
const stream = __createNativeStream(streamId);  // → ReadableStream

// Writing streams (streaming response)
const id = __responseStreamCreate();
__responseStreamWrite(id, uint8Array);  // → bool (false if full)
__responseStreamEnd(id);
__responseStreamIsClosed(id);           // → bool
```

## Data Flow

```
1. JS: reader.read()
2. JS: queue empty → call pull(controller)
3. JS: pull() calls __nativeStreamRead(streamId, cb)
4. Rust: store cb, send StreamRead to scheduler
5. Scheduler: await stream_manager.read_chunk(id)
6. Scheduler: send StreamChunk(cb_id, data) to V8
7. V8: process_callback() → call cb({done, value})
8. JS: controller.enqueue(value), resolve pull()
9. JS: reader.read() returns {done: false, value}
```

## Backpressure

Pull-based model: JS requests data when ready.

```javascript
// ReadableStream calls pull() only when:
// 1. Internal queue is below highWaterMark
// 2. Consumer called read()

async pull(controller) {
    return new Promise(resolve => {
        __nativeStreamRead(streamId, result => {
            if (result.done) controller.close();
            else controller.enqueue(result.value);
            resolve();
        });
    });
}
```

No unbounded buffering—Rust waits for JS to pull.

## Usage

### Rust (producer)

```rust
let stream_id = stream_manager.create_stream();

tokio::spawn(async move {
    while let Some(chunk) = response.chunk().await? {
        stream_manager.write_chunk(stream_id, StreamChunk::Data(chunk))?;
    }
    stream_manager.write_chunk(stream_id, StreamChunk::Done)?;
});
```

### JavaScript (consumer)

```javascript
const response = await fetch("https://example.com/large-file");
const reader = response.body.getReader();

while (true) {
  const { done, value } = await reader.read();
  if (done) break;
  // Process chunk (value is Uint8Array)
}
```

## Thread Safety

- `StreamManager` is `Clone` + `Send` via `Arc<Mutex<...>>`
- Channels are thread-safe (`mpsc::channel` with bounded capacity)
- One reader per stream (WHATWG spec)

## Memory

- `Bytes` uses reference counting (zero-copy)
- Receivers removed during `read_chunk()` to prevent deadlock
- Auto-cleanup on `Done` or `Error`

## Code Pointers

| Component      | File                        | Key functions                          |
| -------------- | --------------------------- | -------------------------------------- |
| StreamManager  | `runtime/stream_manager.rs` | `create_stream()`, `write_chunk()`, `read_chunk()` |
| JS bindings    | `runtime/bindings/streams.rs` | `__nativeStreamRead()`               |
| Scheduler      | `runtime/scheduler.rs`      | `SchedulerMessage::StreamRead`         |
