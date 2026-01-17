//! Shared execution helpers for Worker and ExecutionContext
//!
//! This module contains helper functions and types used by both Worker and
//! ExecutionContext for task execution, event loop management, and response handling.

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
                && let Some(val_str) = val_val.to_string(scope)
            {
                headers.push((
                    key_str.to_rust_string_lossy(scope),
                    val_str.to_rust_string_lossy(scope),
                ));
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
