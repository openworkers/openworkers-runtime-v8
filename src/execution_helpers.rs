//! Shared execution helpers for Worker and ExecutionContext
//!
//! This module contains helper functions and types used by both Worker and
//! ExecutionContext for task execution, event loop management, and response handling.

use crate::runtime::stream_manager::{StreamChunk, StreamManager};
use openworkers_core::{HttpResponse, RequestBody, ResponseBody, TaskInit, TaskSource};
use std::collections::HashMap;
use std::sync::Arc;
use v8;

/// Condition to check for exiting the event loop, read from the handle
/// src/js/dispatch.js answers for the event in flight.
#[derive(Debug, Clone, Copy)]
pub enum EventLoopExit {
    /// `answer` is fulfilled: the Response, or the task result
    ResponseReady,
    /// `done` is fulfilled: the answer and every waitUntil promise
    HandlerComplete,
    /// `streamed` is fulfilled: the response body is out
    StreamsComplete,
    /// `done` and `streamed` are both fulfilled
    FullyComplete,
}

/// Optional abort handling configuration for event loop.
///
/// When enabled, the event loop will:
/// 1. Detect client disconnects via stream_manager
/// 2. Signal the disconnect to JS via __signalClientDisconnect
/// 3. Allow a grace period for JS to react before force-exiting
#[derive(Clone)]
pub struct AbortConfig {
    /// Grace period after signaling abort before force-exit
    pub grace_period: tokio::time::Duration,
}

impl AbortConfig {
    pub fn new(grace_period_ms: u64) -> Self {
        Self {
            grace_period: tokio::time::Duration::from_millis(grace_period_ms),
        }
    }
}

impl Default for AbortConfig {
    fn default() -> Self {
        Self::new(100) // 100ms grace period
    }
}

/// The state of the promise the handle holds under `name`. dispatch.js sets
/// all three, so `None` means the handle is not one of its own.
fn promise_state(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: v8::Local<v8::Object>,
    name: &str,
) -> Option<v8::PromiseState> {
    let key = v8::String::new(scope, name).unwrap();
    let value = handle.get(scope, key.into())?;

    v8::Local::<v8::Promise>::try_from(value)
        .ok()
        .map(|promise| promise.state())
}

/// A promise of the handle that no longer waits. dispatch.js fulfils every
/// one; a rejection counts as settled, so a fault in the glue ends the wait
/// instead of holding it to the wall clock.
fn settled(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: v8::Local<v8::Object>,
    name: &str,
) -> bool {
    !matches!(
        promise_state(scope, handle, name),
        Some(v8::PromiseState::Pending)
    )
}

/// Whether `condition` holds for the event in flight. With no event in
/// flight there is nothing to wait for.
pub fn check_exit_condition(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
    condition: EventLoopExit,
) -> bool {
    let Some(handle) = handle else {
        return true;
    };

    match condition {
        EventLoopExit::ResponseReady => settled(scope, handle, "answer"),
        EventLoopExit::HandlerComplete => settled(scope, handle, "done"),
        EventLoopExit::StreamsComplete => settled(scope, handle, "streamed"),
        EventLoopExit::FullyComplete => {
            settled(scope, handle, "done") && settled(scope, handle, "streamed")
        }
    }
}

/// Whether the event is done, and whether its response body is still going
/// out.
pub fn get_completion_state(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
) -> (bool, bool) {
    let Some(handle) = handle else {
        return (true, false);
    };

    (
        settled(scope, handle, "done"),
        !settled(scope, handle, "streamed"),
    )
}

/// The Response the handle answered, once it has.
fn answered_response<'s>(
    scope: &mut v8::ContextScope<'_, 's, v8::HandleScope>,
    handle: v8::Local<v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let key = v8::String::new(scope, "answer").unwrap();
    let value = handle.get(scope, key.into())?;
    let promise = v8::Local::<v8::Promise>::try_from(value).ok()?;

    if promise.state() != v8::PromiseState::Fulfilled {
        return None;
    }

    promise.result(scope).to_object(scope)
}

/// How a fetch listener called respondWith, in the two ways the dispatch
/// accepts and the Service Worker spec refuses (wintertc js/dispatch.js).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListenerMarks {
    /// respondWith ran after the listener returned
    pub late: bool,
    /// respondWith ran after an async listener's promise had settled
    pub after_settle: bool,
}

impl ListenerMarks {
    pub fn any(&self) -> bool {
        self.late || self.after_settle
    }
}

/// The marks the dispatch left on the handle of an answered fetch.
pub fn read_marks(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
) -> ListenerMarks {
    let Some(handle) = handle else {
        return ListenerMarks::default();
    };

    let key = v8::String::new(scope, "marks").unwrap();
    let Some(marks) = handle
        .get(scope, key.into())
        .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
    else {
        return ListenerMarks::default();
    };

    let flag = |name: &str| {
        let key = v8::String::new(scope, name).unwrap();

        marks
            .get(scope, key.into())
            .is_some_and(|value| value.is_true())
    };

    ListenerMarks {
        late: flag("late"),
        after_settle: flag("afterSettle"),
    }
}

/// The id of the stream the response body goes out on, if it streams.
pub fn get_response_stream_id(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
) -> Option<u64> {
    let response = answered_response(scope, handle?)?;
    let key = v8::String::new(scope, "_responseStreamId").unwrap();
    let id = response.get(scope, key.into())?;

    if id.is_undefined() || id.is_null() {
        return None;
    }

    id.uint32_value(scope).map(u64::from)
}

/// Extract headers from a Response object.
///
/// Headers can be either:
/// - A Headers instance (with internal _map: Map<string, string>)
/// - A plain object with string properties
pub fn extract_headers_from_response(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    resp_obj: v8::Local<v8::Object>,
) -> Vec<(String, String)> {
    let mut headers = vec![];
    let headers_key = v8::String::new(scope, "headers").unwrap();

    let Some(headers_val) = resp_obj.get(scope, headers_key.into()) else {
        return headers;
    };

    let Some(headers_obj) = headers_val.to_object(scope) else {
        return headers;
    };

    // Check if this is a Headers instance (has _map)
    let map_key = v8::String::new(scope, "_map").unwrap();

    if let Some(map_val) = headers_obj.get(scope, map_key.into())
        && let Ok(map_obj) = v8::Local::<v8::Map>::try_from(map_val)
    {
        // Headers instance - iterate over _map
        let entries = map_obj.as_array(scope);
        let len = entries.length();
        let mut i = 0;

        while i < len {
            if let Some(key_val) = entries.get_index(scope, i)
                && let Some(val_val) = entries.get_index(scope, i + 1)
                && let Some(key_str) = key_val.to_string(scope)
            {
                let key = key_str.to_rust_string_lossy(scope);

                // Check if value is an array (for special headers like set-cookie)
                if let Ok(arr) = v8::Local::<v8::Array>::try_from(val_val) {
                    // Value is an array - push each element as a separate header
                    for j in 0..arr.length() {
                        if let Some(elem) = arr.get_index(scope, j)
                            && let Some(elem_str) = elem.to_string(scope)
                        {
                            headers.push((key.clone(), elem_str.to_rust_string_lossy(scope)));
                        }
                    }
                } else if let Some(val_str) = val_val.to_string(scope) {
                    // Value is a string - push as-is
                    headers.push((key, val_str.to_rust_string_lossy(scope)));
                }
            }

            i += 2; // Map.as_array returns [key, value, key, value, ...]
        }
    } else if let Some(props) = headers_obj.get_own_property_names(scope, Default::default()) {
        // Plain object - use property iteration
        for i in 0..props.length() {
            if let Some(key_val) = props.get_index(scope, i)
                && let Some(key_str) = key_val.to_string(scope)
                && let Some(val) = headers_obj.get(scope, key_val)
                && let Some(val_str) = val.to_string(scope)
            {
                headers.push((
                    key_str.to_rust_string_lossy(scope),
                    val_str.to_rust_string_lossy(scope),
                ));
            }
        }
    }

    headers
}

/// Tells the event in flight that the client hung up, so a streaming body
/// stops.
pub fn signal_client_disconnect(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
) {
    let Some(handle) = handle else {
        return;
    };

    let key = v8::String::new(scope, "disconnect").unwrap();

    if let Some(value) = handle.get(scope, key.into())
        && let Ok(disconnect) = v8::Local::<v8::Function>::try_from(value)
    {
        disconnect.call(scope, handle.into(), &[]);
    }
}

/// Calls `dispatch[name](argument)` and keeps the handle it answers.
fn dispatch(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    dispatch: &v8::Global<v8::Object>,
    name: &str,
    argument: v8::Local<v8::Value>,
) -> Result<v8::Global<v8::Object>, String> {
    let dispatch = v8::Local::new(scope, dispatch);
    let key = v8::String::new(scope, name).unwrap();
    let function = dispatch
        .get(scope, key.into())
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .expect("dispatch.js answers { fetch, task }");

    // The event starts with no async context, whatever the last one left
    crate::runtime::bindings::clear_async_context(scope);

    // None means V8 was terminated (CPU or wall-clock limit)
    let handle = function
        .call(scope, dispatch.into(), &[argument])
        .ok_or("Execution terminated")?;
    let handle = handle
        .to_object(scope)
        .expect("dispatch.js answers an object for every event");

    Ok(v8::Global::new(scope, handle))
}

/// Hands a task to the guest with the event it reads: `taskId`, `attempt`,
/// `payload`, and `scheduledTime` with `cron` when a schedule fired the task.
pub fn trigger_task_handler(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    dispatcher: &v8::Global<v8::Object>,
    task: &TaskInit,
) -> Result<v8::Global<v8::Object>, String> {
    let event = v8::Object::new(scope);
    set_task_fields(scope, event, task);

    dispatch(scope, dispatcher, "task", event.into())
}

/// The task result the handle answered: what the guest returned, or the
/// error that stopped it.
pub fn read_task_result(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
) -> openworkers_core::TaskResult {
    let result = handle.and_then(|handle| {
        let key = v8::String::new(scope, "done").unwrap();
        let value = handle.get(scope, key.into())?;
        let promise = v8::Local::<v8::Promise>::try_from(value).ok()?;

        (promise.state() == v8::PromiseState::Fulfilled)
            .then(|| promise.result(scope))?
            .to_object(scope)
    });

    let Some(result) = result else {
        return openworkers_core::TaskResult::err("the task gave no result");
    };

    let key = v8::String::new(scope, "success").unwrap();
    let success = result
        .get(scope, key.into())
        .map(|value| value.is_true())
        .unwrap_or(false);

    let key = v8::String::new(scope, "data").unwrap();
    let data = result.get(scope, key.into()).and_then(|value| {
        if value.is_undefined() || value.is_null() {
            return None;
        }

        let json = v8::json::stringify(scope, value)?.to_rust_string_lossy(scope);
        serde_json::from_str(&json).ok()
    });

    let key = v8::String::new(scope, "error").unwrap();
    let error = result.get(scope, key.into()).and_then(|value| {
        (!value.is_undefined() && !value.is_null()).then(|| value.to_rust_string_lossy(scope))
    });

    openworkers_core::TaskResult {
        success,
        data,
        error,
    }
}

fn set_task_fields(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    event: v8::Local<v8::Object>,
    task: &TaskInit,
) {
    let key = v8::String::new(scope, "taskId").unwrap();
    let value = v8::String::new(scope, &task.task_id).unwrap();
    event.set(scope, key.into(), value.into());

    let key = v8::String::new(scope, "attempt").unwrap();
    let value = v8::Number::new(scope, task.attempt as f64);
    event.set(scope, key.into(), value.into());

    if let Some(payload) = &task.payload {
        let key = v8::String::new(scope, "payload").unwrap();
        let json = serde_json::to_string(payload).expect("a JSON value serialises");
        let json = v8::String::new(scope, &json).unwrap();

        if let Some(parsed) = v8::json::parse(scope, json) {
            event.set(scope, key.into(), parsed);
        }
    }

    if let Some(TaskSource::Schedule { time, cron }) = &task.source {
        let key = v8::String::new(scope, "scheduledTime").unwrap();
        let value = v8::Number::new(scope, *time as f64);
        event.set(scope, key.into(), value.into());

        if let Some(cron) = cron {
            let key = v8::String::new(scope, "cron").unwrap();
            let value = v8::String::new(scope, cron).unwrap();
            event.set(scope, key.into(), value.into());
        }
    }
}

/// Hands a request to the guest as a `Request` built from these parts, and
/// answers the handle for it. A streamed body arrives as `body_stream_id`, a
/// buffered one is taken out of `body`.
#[allow(clippy::too_many_arguments)]
pub fn trigger_fetch_handler(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    dispatcher: &v8::Global<v8::Object>,
    url: &str,
    method: &str,
    headers: &HashMap<String, String>,
    body: &mut RequestBody,
    body_stream_id: Option<u64>,
) -> Result<v8::Global<v8::Object>, String> {
    let global = scope.get_current_context().global(scope);

    // Get Request constructor
    let request_key = v8::String::new(scope, "Request").unwrap();
    let request_constructor = global
        .get(scope, request_key.into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok());

    let request_obj = if let Some(request_ctor) = request_constructor {
        // Create init object with method, headers, body
        let init_obj = v8::Object::new(scope);

        let method_key = v8::String::new(scope, "method").unwrap();
        let method_val = v8::String::new(scope, method).unwrap();
        init_obj.set(scope, method_key.into(), method_val.into());

        // A client may put a body on a GET; the standard's refusal is for guest
        // code, not for what arrived on the wire.
        let from_host_key = v8::String::new(scope, "_fromHost").unwrap();
        let from_host_val = v8::Boolean::new(scope, true);
        init_obj.set(scope, from_host_key.into(), from_host_val.into());

        // Create headers object for init
        let headers_obj = v8::Object::new(scope);

        for (key, value) in headers {
            let k = v8::String::new(scope, key).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            headers_obj.set(scope, k.into(), v.into());
        }

        let headers_key = v8::String::new(scope, "headers").unwrap();
        init_obj.set(scope, headers_key.into(), headers_obj.into());

        // Add body - either as stream ID or buffered Uint8Array
        if let Some(stream_id) = body_stream_id {
            // Streaming body - pass stream ID so JS can create ReadableStream
            let stream_id_key = v8::String::new(scope, "_bodyStreamId").unwrap();
            let stream_id_val = v8::Number::new(scope, stream_id as f64);
            init_obj.set(scope, stream_id_key.into(), stream_id_val.into());
        } else if let RequestBody::Bytes(body_bytes) = std::mem::take(body)
            && !body_bytes.is_empty()
        {
            // Buffered body - pass as Uint8Array for binary support
            let len = body_bytes.len();
            let vec: Vec<u8> = body_bytes.into(); // zero-copy if uniquely owned
            let array_buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, vec);
            let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();

            let body_key = v8::String::new(scope, "body").unwrap();
            init_obj.set(scope, body_key.into(), uint8_array.into());
        }

        // Call new Request(url, init)
        let url_val = v8::String::new(scope, url).unwrap();
        request_ctor
            .new_instance(scope, &[url_val.into(), init_obj.into()])
            .unwrap_or_else(|| v8::Object::new(scope))
    } else {
        // Fallback to plain object if Request not available
        let obj = v8::Object::new(scope);
        let url_key = v8::String::new(scope, "url").unwrap();
        let url_val = v8::String::new(scope, url).unwrap();
        obj.set(scope, url_key.into(), url_val.into());

        let method_key = v8::String::new(scope, "method").unwrap();
        let method_val = v8::String::new(scope, method).unwrap();
        obj.set(scope, method_key.into(), method_val.into());

        let headers_obj = v8::Object::new(scope);

        for (key, value) in headers {
            let k = v8::String::new(scope, key).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            headers_obj.set(scope, k.into(), v.into());
        }

        let headers_key = v8::String::new(scope, "headers").unwrap();
        obj.set(scope, headers_key.into(), headers_obj.into());
        obj
    };

    dispatch(scope, dispatcher, "fetch", request_obj.into())
}

/// The Response the handle answered, as an HttpResponse: its body streams
/// when the guest streams it, and is buffered otherwise.
pub fn read_response_object(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    handle: Option<v8::Local<v8::Object>>,
    stream_manager: &Arc<StreamManager>,
    buffer_size: usize,
) -> Result<(u16, HttpResponse), String> {
    let resp_obj = handle
        .and_then(|handle| answered_response(scope, handle))
        .ok_or("No response set")?;

    let status_key = v8::String::new(scope, "status").unwrap();
    let status = resp_obj
        .get(scope, status_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(200) as u16;

    // Check if response has _responseStreamId (streaming body)
    let response_stream_id_key = v8::String::new(scope, "_responseStreamId").unwrap();
    let response_stream_id = resp_obj
        .get(scope, response_stream_id_key.into())
        .and_then(|v| {
            if v.is_null() || v.is_undefined() {
                None
            } else {
                v.uint32_value(scope).map(|n| n as u64)
            }
        });

    // Extract headers (handles both Headers instance and plain object)
    let headers = extract_headers_from_response(scope, resp_obj);

    // Determine body type: streaming or buffered
    let body = if let Some(stream_id) = response_stream_id {
        // Response stream - take the receiver from StreamManager
        if let Some(receiver) = stream_manager.take_receiver(stream_id) {
            let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);

            // Clone stream_manager to use in the spawned task
            let stream_manager = stream_manager.clone();

            // Spawn task to convert StreamChunk -> Result<Bytes, String>
            // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
            // when the LocalSet is dropped (production pattern with thread-pinned pool)
            tokio::spawn(async move {
                let mut receiver = receiver;

                loop {
                    tokio::select! {
                        chunk = receiver.recv() => {
                            match chunk {
                                Some(StreamChunk::Data(bytes)) => {
                                    if tx.send(Ok(bytes)).await.is_err() {
                                        stream_manager.close_stream(stream_id);
                                        break;
                                    }
                                }
                                Some(StreamChunk::Done) => {
                                    break;
                                }
                                Some(StreamChunk::Error(e)) => {
                                    let _ = tx.send(Err(e)).await;
                                    break;
                                }
                                None => {
                                    break;
                                }
                            }
                        }

                        _ = tx.closed() => {
                            stream_manager.close_stream(stream_id);
                            break;
                        }
                    }
                }
            });

            ResponseBody::Stream(rx)
        } else {
            ResponseBody::None
        }
    } else {
        // Buffered body - use _getRawBody()
        let get_raw_body_key = v8::String::new(scope, "_getRawBody").unwrap();
        let body_bytes = if let Some(get_raw_body_val) =
            resp_obj.get(scope, get_raw_body_key.into())
            && let Ok(get_raw_body_fn) = v8::Local::<v8::Function>::try_from(get_raw_body_val)
        {
            if let Some(result_val) = get_raw_body_fn.call(scope, resp_obj.into(), &[])
                && let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(result_val)
            {
                let len = uint8_array.byte_length();
                let mut bytes_vec = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes_vec);
                bytes::Bytes::from(bytes_vec)
            } else {
                bytes::Bytes::new()
            }
        } else {
            bytes::Bytes::new()
        };

        ResponseBody::Bytes(body_bytes)
    };

    let response = HttpResponse {
        status,
        headers,
        body,
    };

    Ok((status, response))
}
