//! Shared execution helpers for Worker and ExecutionContext
//!
//! This module contains helper functions and types used by both Worker and
//! ExecutionContext for task execution, event loop management, and response handling.

use crate::runtime::stream_manager::{StreamChunk, StreamManager};
use openworkers_core::{HttpResponse, RequestBody, ResponseBody};
use std::collections::HashMap;
use std::sync::Arc;
use v8;

/// Condition to check for exiting the event loop
#[derive(Debug, Clone, Copy)]
pub enum EventLoopExit {
    /// Wait for __lastResponse to be a valid Response object (has status property)
    ResponseReady,
    /// Wait for __requestComplete to be true (handler finished, including async work)
    HandlerComplete,
    /// Wait for __requestComplete && __activeResponseStreams == 0 (fully complete)
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

/// Check if the specified exit condition is met (free function to avoid borrow conflicts)
pub fn check_exit_condition(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
    condition: EventLoopExit,
) -> bool {
    match condition {
        EventLoopExit::ResponseReady => {
            // Check __lastResponse is a valid Response object (not undefined, not a Promise)
            let resp_key = v8::String::new(scope, "__lastResponse").unwrap();

            if let Some(resp_val) = global.get(scope, resp_key.into())
                && !resp_val.is_undefined()
                && !resp_val.is_null()
                && !resp_val.is_promise()
                && let Some(resp_obj) = resp_val.to_object(scope)
            {
                // Check if it has a 'status' property (indicates it's a Response)
                let status_key = v8::String::new(scope, "status").unwrap();
                return resp_obj.get(scope, status_key.into()).is_some();
            }

            false
        }

        EventLoopExit::HandlerComplete => {
            // Check __requestComplete == true
            let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
            global
                .get(scope, complete_key.into())
                .map(|v| v.is_true())
                .unwrap_or(false)
        }

        EventLoopExit::FullyComplete => {
            // Check __requestComplete && __activeResponseStreams == 0
            let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
            let request_complete = global
                .get(scope, complete_key.into())
                .map(|v| v.is_true())
                .unwrap_or(false);

            if !request_complete {
                return false;
            }

            let streams_key = v8::String::new(scope, "__activeResponseStreams").unwrap();
            let active_streams = global
                .get(scope, streams_key.into())
                .and_then(|v| v.uint32_value(scope))
                .unwrap_or(0);

            active_streams == 0
        }
    }
}

/// Get the completion state: (request_complete, active_streams)
#[inline]
pub fn get_completion_state(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
) -> (bool, u32) {
    let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
    let request_complete = global
        .get(scope, complete_key.into())
        .map(|v| v.is_true())
        .unwrap_or(false);

    let streams_key = v8::String::new(scope, "__activeResponseStreams").unwrap();
    let active_streams = global
        .get(scope, streams_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0);

    (request_complete, active_streams)
}

/// Get the response stream ID if present
#[inline]
pub fn get_response_stream_id(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
) -> Option<u64> {
    let stream_id_key = v8::String::new(scope, "__lastResponseStreamId").unwrap();
    let stream_id_val = global.get(scope, stream_id_key.into())?;

    if stream_id_val.is_undefined() || stream_id_val.is_null() {
        return None;
    }

    stream_id_val.uint32_value(scope).map(|id| id as u64)
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

/// Signal to JS that the client has disconnected.
/// Calls __signalClientDisconnect() which is always defined in setup_event_listener.
#[inline]
pub fn signal_client_disconnect(scope: &mut v8::ContextScope<v8::HandleScope>) {
    let global = scope.get_current_context().global(scope);
    let fn_key = v8::String::new(scope, "__signalClientDisconnect").unwrap();

    if let Some(fn_val) = global.get(scope, fn_key.into())
        && let Ok(func) = v8::Local::<v8::Function>::try_from(fn_val)
    {
        let _ = func.call(scope, global.into(), &[]);
    }
}

/// Trigger the fetch handler with a request created from the given parameters.
///
/// This is shared between Worker and ExecutionContext to avoid code duplication.
/// Creates a Request object and calls __triggerFetch with it.
///
/// # Arguments
/// * `scope` - V8 context scope
/// * `url` - Request URL
/// * `method` - HTTP method
/// * `headers` - Request headers
/// * `body` - Request body (will be consumed if Bytes)
/// * `body_stream_id` - Optional stream ID for streaming bodies
///
/// # Returns
/// Ok(()) if handler was triggered, Err if execution was terminated
pub fn trigger_fetch_handler(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    url: &str,
    method: &str,
    headers: &HashMap<String, String>,
    body: &mut RequestBody,
    body_stream_id: Option<u64>,
) -> Result<(), String> {
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

    // Trigger fetch handler
    let trigger_key = v8::String::new(scope, "__triggerFetch").unwrap();

    if let Some(trigger_val) = global.get(scope, trigger_key.into())
        && trigger_val.is_function()
    {
        let trigger_fn: v8::Local<v8::Function> = trigger_val.try_into().unwrap();
        let result = trigger_fn.call(scope, global.into(), &[request_obj.into()]);

        // If call returned None, V8 was terminated (CPU/wall-clock timeout)
        if result.is_none() {
            return Err("Execution terminated".to_string());
        }
    }

    Ok(())
}

/// Read the response object from `__lastResponse` and convert to HttpResponse.
///
/// This is shared between Worker and ExecutionContext to avoid code duplication.
///
/// # Arguments
/// * `scope` - V8 context scope
/// * `stream_manager` - StreamManager for handling streaming responses
/// * `buffer_size` - Buffer size for response stream channel
///
/// # Returns
/// A tuple of (status, HttpResponse) or an error
pub fn read_response_object(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    stream_manager: &Arc<StreamManager>,
    buffer_size: usize,
) -> Result<(u16, HttpResponse), String> {
    let global = scope.get_current_context().global(scope);

    let resp_key = v8::String::new(scope, "__lastResponse").unwrap();
    let resp_val = global
        .get(scope, resp_key.into())
        .ok_or("No response set")?;

    let resp_obj = resp_val.to_object(scope).ok_or("Invalid response object")?;

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
