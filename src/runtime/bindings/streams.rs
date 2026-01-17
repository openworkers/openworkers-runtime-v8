use super::super::{CallbackId, SchedulerMessage, stream_manager};
use super::state::{ResponseStreamState, StreamState};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;
use v8;

/// Native stream read - reads a chunk from a native stream
#[gv8::method(state = Rc<StreamState>)]
fn native_stream_read(
    scope: &mut v8::PinScope,
    state: &Rc<StreamState>,
    stream_id: u64,
    resolve: v8::Local<v8::Function>,
) {
    let resolve_global = v8::Global::new(scope.as_ref(), resolve);

    // Generate callback ID
    let callback_id = {
        let mut next_id = state.next_id.borrow_mut();
        let id = *next_id;
        *next_id += 1;
        id
    };

    // Store callback
    state
        .callbacks
        .borrow_mut()
        .insert(callback_id, resolve_global);

    // Send StreamRead message to scheduler
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::StreamRead(callback_id, stream_id));
}

/// Native stream cancel - cancels a native stream
#[gv8::method(state = Rc<StreamState>)]
fn native_stream_cancel(scope: &mut v8::PinScope, state: &Rc<StreamState>, stream_id: u64) {
    let _ = scope;
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::StreamCancel(stream_id));
}

/// Setup native streaming operations
/// These ops allow JavaScript to read chunks from Rust streams
pub fn setup_stream_ops(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    stream_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    next_callback_id: Rc<RefCell<CallbackId>>,
) {
    let state = StreamState {
        scheduler_tx,
        callbacks: stream_callbacks,
        next_id: next_callback_id,
    };

    store_state!(scope, state);

    let stream_read_fn = v8::Function::new(scope, native_stream_read_v8).unwrap();
    let stream_cancel_fn = v8::Function::new(scope, native_stream_cancel_v8).unwrap();

    register_fn!(scope, "__nativeStreamRead", stream_read_fn);
    register_fn!(scope, "__nativeStreamCancel", stream_cancel_fn);

    // JavaScript helper: createNativeStream(streamId) -> ReadableStream
    exec_js!(
        scope,
        r#"
        globalThis.__createNativeStream = function(streamId) {
            const stream = new ReadableStream({
                async pull(controller) {
                    return new Promise((resolve) => {
                        __nativeStreamRead(streamId, (result) => {
                            if (result.error) {
                                controller.error(new Error(result.error));
                            } else if (result.done) {
                                controller.close();
                            } else {
                                controller.enqueue(result.value);
                            }
                            resolve();
                        });
                    });
                },
                cancel(reason) {
                    __nativeStreamCancel(streamId);
                }
            });
            // Mark this stream as a native stream so we can forward it directly
            stream._nativeStreamId = streamId;
            return stream;
        };
    "#
    );
}

/// Create a new response stream
#[gv8::method(state = Rc<ResponseStreamState>)]
fn response_stream_create(scope: &mut v8::PinScope, state: &Rc<ResponseStreamState>) -> u64 {
    let _ = scope;
    state.manager.create_stream("response".to_string())
}

/// Write to a response stream - returns true if written, false if full/error
#[gv8::method(state = Rc<ResponseStreamState>)]
fn response_stream_write(
    scope: &mut v8::PinScope,
    state: &Rc<ResponseStreamState>,
    stream_id: u64,
    chunk: v8::Local<v8::Uint8Array>,
) -> bool {
    let _ = scope;
    let len = chunk.byte_length();
    let mut bytes_vec = vec![0u8; len];
    chunk.copy_contents(&mut bytes_vec);

    state
        .manager
        .try_write_chunk(
            stream_id,
            stream_manager::StreamChunk::Data(bytes::Bytes::from(bytes_vec)),
        )
        .is_ok()
}

/// End a response stream
#[gv8::method(state = Rc<ResponseStreamState>)]
fn response_stream_end(scope: &mut v8::PinScope, state: &Rc<ResponseStreamState>, stream_id: u64) {
    let _ = scope;
    let _ = state
        .manager
        .try_write_chunk(stream_id, stream_manager::StreamChunk::Done);
}

/// Check if a response stream is closed
#[gv8::method(state = Rc<ResponseStreamState>)]
fn response_stream_is_closed(
    scope: &mut v8::PinScope,
    state: &Rc<ResponseStreamState>,
    stream_id: u64,
) -> bool {
    let _ = scope;
    !state.manager.has_sender(stream_id)
}

/// Setup response streaming operations
/// These allow JS to stream response body chunks back to Rust
pub fn setup_response_stream_ops(
    scope: &mut v8::PinScope,
    stream_manager: Arc<stream_manager::StreamManager>,
) {
    let state = ResponseStreamState {
        manager: stream_manager,
    };

    store_state!(scope, state);

    let create_fn = v8::Function::new(scope, response_stream_create_v8).unwrap();
    let write_fn = v8::Function::new(scope, response_stream_write_v8).unwrap();
    let end_fn = v8::Function::new(scope, response_stream_end_v8).unwrap();
    let is_closed_fn = v8::Function::new(scope, response_stream_is_closed_v8).unwrap();

    register_fn!(scope, "__responseStreamCreate", create_fn);
    register_fn!(scope, "__responseStreamWrite", write_fn);
    register_fn!(scope, "__responseStreamEnd", end_fn);
    register_fn!(scope, "__responseStreamIsClosed", is_closed_fn);
}
