pub mod bindings;
pub mod callback_handlers;
pub mod crypto;
pub(crate) mod dispatch;
pub(crate) use dispatch::Guest;
pub(crate) use dispatch::call_guest;
pub mod scheduler;
pub mod stream_manager;
pub mod text_encoding;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use v8;

use openworkers_core::{WebSocketId, WebSocketIncoming};

// Re-export scheduler types
pub use scheduler::{
    CallbackId, CallbackMessage, CallbackSender, SchedulerMessage, run_event_loop,
};

/// Helper to dispatch binding result callbacks (resolve/reject pattern)
pub(crate) fn dispatch_binding_callbacks(
    scope: &mut v8::PinScope,
    callback_id: CallbackId,
    resolve_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    reject_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    error_msg: Option<&str>,
    result_value: Option<v8::Local<v8::Value>>,
) {
    let resolve = resolve_callbacks.borrow_mut().remove(&callback_id);
    let reject = reject_callbacks.borrow_mut().remove(&callback_id);

    if let Some(err_msg) = error_msg {
        if let Some(reject) = reject {
            let error_msg_val = v8::String::new(scope, err_msg).unwrap();
            let error = v8::Exception::error(scope, error_msg_val);
            let reject = v8::Local::new(scope, &reject);
            call_guest(scope, reject, &[error]);
        }
    } else if let Some(value) = result_value
        && let Some(resolve) = resolve
    {
        let resolve = v8::Local::new(scope, &resolve);

        // A resolve that throws still owes the promise an answer.
        if let (Guest::Threw(thrown), Some(reject)) = (call_guest(scope, resolve, &[value]), reject)
        {
            let reject = v8::Local::new(scope, &reject);
            let exception = v8::Local::new(scope, &thrown);
            call_guest(scope, reject, &[exception]);
        }
    }
}

/// Settle the promise of an invoke call with its result: a value through
/// JSON.parse, an error through reject. A value too large for a V8 string
/// rejects.
#[cfg(feature = "invoke")]
pub(crate) fn settle_invoke(
    scope: &mut v8::PinScope,
    callback_id: CallbackId,
    resolve_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    reject_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    result: openworkers_core::InvokeResult,
) {
    use openworkers_core::InvokeResult;

    let (error, value) = match result {
        InvokeResult::Error(message) => (Some(message), None),
        InvokeResult::Value(value) => match v8::String::new(scope, &value.to_string()) {
            Some(json) => {
                let parsed = v8::json::parse(scope, json).unwrap_or_else(|| v8::null(scope).into());

                (None, Some(parsed))
            }
            None => (Some("invoke: the answer is too large".to_string()), None),
        },
    };

    dispatch_binding_callbacks(
        scope,
        callback_id,
        resolve_callbacks,
        reject_callbacks,
        error.as_deref(),
        value,
    );
}

/// Dispatch a WebSocket event to the JS dispatcher function.
///
/// The dispatcher is a JS callback registered during `ws.accept()`.
/// It's called with (eventType, arg1, arg2) and handles event construction in JS.
pub(crate) fn dispatch_ws_event(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    ws_callbacks: &Rc<RefCell<HashMap<WebSocketId, v8::Global<v8::Function>>>>,
    ws_id: WebSocketId,
    incoming: WebSocketIncoming,
) {
    // For close, remove the callback (connection is done)
    let remove = matches!(incoming, WebSocketIncoming::Closed { .. });

    let callback_global = if remove {
        ws_callbacks.borrow_mut().remove(&ws_id)
    } else {
        ws_callbacks.borrow().get(&ws_id).cloned()
    };

    let Some(callback_global) = callback_global else {
        return;
    };

    let callback = v8::Local::new(scope, &callback_global);

    match incoming {
        WebSocketIncoming::Text(s) => {
            let type_val = v8::String::new(scope, "message").unwrap();
            let data_val = v8::String::new(scope, &s).unwrap();
            call_guest(scope, callback, &[type_val.into(), data_val.into()]);
        }
        WebSocketIncoming::Binary(bytes) => {
            let type_val = v8::String::new(scope, "message").unwrap();
            let ab = crate::v8_helpers::create_array_buffer_from_vec(scope, bytes);
            call_guest(scope, callback, &[type_val.into(), ab.into()]);
        }
        WebSocketIncoming::Closed { code, reason } => {
            let type_val = v8::String::new(scope, "close").unwrap();
            let code_val = v8::Number::new(scope, code as f64);
            let reason_val = v8::String::new(scope, &reason).unwrap();
            call_guest(
                scope,
                callback,
                &[type_val.into(), code_val.into(), reason_val.into()],
            );
        }
        WebSocketIncoming::Error(e) => {
            let type_val = v8::String::new(scope, "error").unwrap();
            let msg_val = v8::String::new(scope, &e).unwrap();
            call_guest(scope, callback, &[type_val.into(), msg_val.into()]);
        }
    };
}
