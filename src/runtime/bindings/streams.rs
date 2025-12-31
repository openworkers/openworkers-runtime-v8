use super::super::{CallbackId, SchedulerMessage, stream_manager};
use super::state::StreamState;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use v8;

/// Setup native streaming operations
/// These ops allow JavaScript to read chunks from Rust streams
pub fn setup_stream_ops(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    stream_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    next_callback_id: Arc<Mutex<CallbackId>>,
) {
    let state = StreamState {
        scheduler_tx,
        callbacks: stream_callbacks,
        next_id: next_callback_id,
    };

    // Create External to hold our state
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__streamState").unwrap();
    global.set(scope, state_key.into(), external.into());

    // Create __nativeStreamRead(stream_id, resolve_callback)
    // Uses the same callback pattern as fetch for consistency
    let stream_read_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__streamState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut StreamState;
            let state = unsafe { &*state_ptr };

            // Get stream_id (arg 0)
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                return;
            };

            // Get resolve callback (arg 1)
            let resolve_val = args.get(1);

            if !resolve_val.is_function() {
                return;
            }

            let resolve: v8::Local<v8::Function> = resolve_val.try_into().unwrap();
            let resolve_global = v8::Global::new(scope.as_ref(), resolve);

            // Generate callback ID
            let callback_id = {
                let mut next_id = state.next_id.lock().unwrap();
                let id = *next_id;
                *next_id += 1;
                id
            };

            // Store callback
            {
                let mut callbacks = state.callbacks.lock().unwrap();
                callbacks.insert(callback_id, resolve_global);
            }

            // Send StreamRead message to scheduler
            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::StreamRead(callback_id, stream_id));
        },
    )
    .unwrap();

    let stream_read_key = v8::String::new(scope, "__nativeStreamRead").unwrap();
    global.set(scope, stream_read_key.into(), stream_read_fn.into());

    // Create __nativeStreamCancel(stream_id) - sends cancel message to scheduler
    let stream_cancel_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__streamState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut StreamState;
            let state = unsafe { &*state_ptr };

            // Get stream_id
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                return;
            };

            // Send cancel message
            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::StreamCancel(stream_id));
        },
    )
    .unwrap();

    let stream_cancel_key = v8::String::new(scope, "__nativeStreamCancel").unwrap();
    global.set(scope, stream_cancel_key.into(), stream_cancel_fn.into());

    // JavaScript helper: createNativeStream(streamId) -> ReadableStream
    // Creates a ReadableStream that pulls from Rust via __nativeStreamRead
    // The stream is marked with _nativeStreamId so we can detect it later
    let code = r#"
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
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// Setup response streaming operations
/// These allow JS to stream response body chunks back to Rust
pub fn setup_response_stream_ops(
    scope: &mut v8::PinScope,
    stream_manager: Arc<stream_manager::StreamManager>,
) {
    // Store stream_manager in global for access from callbacks
    let manager_ptr = Arc::into_raw(stream_manager) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, manager_ptr);

    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__responseStreamManager").unwrap();
    global.set(scope, state_key.into(), external.into());

    // __responseStreamCreate() -> stream_id
    let create_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         _args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__responseStreamManager").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let manager_ptr = external.value() as *const stream_manager::StreamManager;
            let manager = unsafe { &*manager_ptr };

            let stream_id = manager.create_stream("response".to_string());
            retval.set(v8::Number::new(scope, stream_id as f64).into());
        },
    )
    .unwrap();

    let create_key = v8::String::new(scope, "__responseStreamCreate").unwrap();
    global.set(scope, create_key.into(), create_fn.into());

    // __responseStreamWrite(stream_id, Uint8Array) -> boolean (true if written, false if full)
    let write_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__responseStreamManager").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let manager_ptr = external.value() as *const stream_manager::StreamManager;
            let manager = unsafe { &*manager_ptr };

            // Get stream_id
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            };

            // Get chunk data (Uint8Array)
            let chunk_val = args.get(1);

            if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(chunk_val) {
                let len = uint8_array.byte_length();
                let mut bytes_vec = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes_vec);

                let result = manager.try_write_chunk(
                    stream_id,
                    stream_manager::StreamChunk::Data(bytes::Bytes::from(bytes_vec)),
                );

                retval.set(v8::Boolean::new(scope, result.is_ok()).into());
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
            }
        },
    )
    .unwrap();

    let write_key = v8::String::new(scope, "__responseStreamWrite").unwrap();
    global.set(scope, write_key.into(), write_fn.into());

    // __responseStreamEnd(stream_id)
    let end_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__responseStreamManager").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let manager_ptr = external.value() as *const stream_manager::StreamManager;
            let manager = unsafe { &*manager_ptr };

            // Get stream_id
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                return;
            };

            // Send Done chunk to signal end of stream
            let _ = manager.try_write_chunk(stream_id, stream_manager::StreamChunk::Done);
        },
    )
    .unwrap();

    let end_key = v8::String::new(scope, "__responseStreamEnd").unwrap();
    global.set(scope, end_key.into(), end_fn.into());

    // __responseStreamIsClosed(stream_id) -> boolean
    // Returns true if the stream's consumer has disconnected (channel closed)
    let is_closed_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__responseStreamManager").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                retval.set(v8::Boolean::new(scope, true).into());
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let manager_ptr = external.value() as *const stream_manager::StreamManager;
            let manager = unsafe { &*manager_ptr };

            // Get stream_id
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                retval.set(v8::Boolean::new(scope, true).into());
                return;
            };

            // Check if stream is closed by checking if sender exists
            let is_closed = !manager.has_sender(stream_id);
            retval.set(v8::Boolean::new(scope, is_closed).into());
        },
    )
    .unwrap();

    let is_closed_key = v8::String::new(scope, "__responseStreamIsClosed").unwrap();
    global.set(scope, is_closed_key.into(), is_closed_fn.into());
}
