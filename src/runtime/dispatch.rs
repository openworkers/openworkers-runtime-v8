//! One answer per callback message, whoever owns the tables.
//!
//! Three event loops answer the same ten messages: the runtime drains its
//! channel, the runtime steps one message, and a pooled request does the same
//! per context. What differed was only where the tables live.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use openworkers_core::DatabaseResult;
use openworkers_core::KvResult;
use openworkers_core::StorageResult;

use super::CallbackId;
use super::CallbackMessage;
use super::WebSocketId;
use super::callback_handlers;
use super::dispatch_binding_callbacks;
use super::dispatch_ws_event;
use super::stream_manager::StreamManager;

type Callbacks = Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>;
type WebSocketCallbacks = Rc<RefCell<HashMap<WebSocketId, v8::Global<v8::Function>>>>;

/// The tables a dispatch reaches for.
pub(crate) struct Tables<'a> {
    pub fetch: &'a Callbacks,
    pub fetch_error: &'a Callbacks,
    pub stream: &'a Callbacks,
    pub ws_event: &'a WebSocketCallbacks,
    pub stream_manager: &'a Arc<StreamManager>,
}

/// Answer one message, inside a scope whose context is already entered.
pub(crate) fn dispatch(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    tables: &Tables<'_>,
    msg: CallbackMessage,
) {
    match msg {
        CallbackMessage::ExecuteTimeout(callback_id)
        | CallbackMessage::ExecuteInterval(callback_id) => {
            let context = scope.get_current_context();
            let global = context.global(scope);
            let key = v8::String::new(scope, "__executeTimer").unwrap();

            if let Some(execute_fn_val) = global.get(scope, key.into())
                && execute_fn_val.is_function()
            {
                let execute_fn: v8::Local<v8::Function> = execute_fn_val.try_into().unwrap();
                let id_val = v8::Number::new(scope, callback_id as f64);
                execute_fn.call(scope, global.into(), &[id_val.into()]);
            }
        }

        // A fetch settles once, so the side that does not answer is dropped with it.
        CallbackMessage::FetchError(callback_id, error_msg) => {
            tables.fetch.borrow_mut().remove(&callback_id);
            let callback = tables.fetch_error.borrow_mut().remove(&callback_id);

            if let Some(callback) = callback {
                let message = v8::String::new(scope, &error_msg).unwrap();
                let error = v8::Exception::error(scope, message);
                let callback = v8::Local::new(scope, &callback);
                let recv = v8::undefined(scope);
                callback.call(scope, recv.into(), &[error]);
            }
        }

        CallbackMessage::FetchStreamingSuccess(callback_id, meta, stream_id) => {
            let callback = tables.fetch.borrow_mut().remove(&callback_id);
            tables.fetch_error.borrow_mut().remove(&callback_id);

            if let Some(callback) = callback {
                let meta_obj = v8::Object::new(scope);
                callback_handlers::populate_fetch_meta(scope, meta_obj, &meta, stream_id);
                let callback = v8::Local::new(scope, &callback);
                let recv = v8::undefined(scope);
                callback.call(scope, recv.into(), &[meta_obj.into()]);
            }
        }

        CallbackMessage::StreamChunk(callback_id, chunk) => {
            let callback = tables.stream.borrow_mut().remove(&callback_id);

            if let Some(callback) = callback {
                let result_obj = v8::Object::new(scope);
                callback_handlers::populate_stream_chunk_result(scope, result_obj, chunk);
                let callback = v8::Local::new(scope, &callback);
                let recv = v8::undefined(scope);
                callback.call(scope, recv.into(), &[result_obj.into()]);
            }
        }

        CallbackMessage::StorageResult(callback_id, result) => {
            let (error_msg, value) = if let StorageResult::Error(err) = &result {
                (Some(err.as_str()), None)
            } else {
                let result_obj = v8::Object::new(scope);
                callback_handlers::populate_storage_result(
                    scope,
                    result_obj,
                    result,
                    tables.stream_manager,
                );

                (None, Some(result_obj.into()))
            };

            settle(scope, tables, callback_id, error_msg, value);
        }

        CallbackMessage::KvResult(callback_id, result) => {
            let (error_msg, value) = if let KvResult::Error(err) = &result {
                (Some(err.as_str()), None)
            } else {
                let result_obj = v8::Object::new(scope);
                callback_handlers::populate_kv_result(scope, result_obj, result);

                (None, Some(result_obj.into()))
            };

            settle(scope, tables, callback_id, error_msg, value);
        }

        CallbackMessage::DatabaseResult(callback_id, result) => {
            let (error_msg, value) = if let DatabaseResult::Error(err) = &result {
                (Some(err.as_str()), None)
            } else {
                let result_obj = v8::Object::new(scope);
                callback_handlers::populate_database_result(scope, result_obj, result);

                (None, Some(result_obj.into()))
            };

            settle(scope, tables, callback_id, error_msg, value);
        }

        CallbackMessage::WebSocketConnected(callback_id, ws_id) => {
            let ws_id_val: v8::Local<v8::Value> = v8::Number::new(scope, ws_id as f64).into();

            settle(scope, tables, callback_id, None, Some(ws_id_val));
        }

        CallbackMessage::WebSocketConnectError(callback_id, error_msg) => {
            settle(scope, tables, callback_id, Some(&error_msg), None);
        }

        CallbackMessage::WebSocketEvent(ws_id, incoming) => {
            dispatch_ws_event(scope, tables.ws_event, ws_id, incoming);
        }
    }
}

fn settle(
    scope: &mut v8::PinScope,
    tables: &Tables<'_>,
    callback_id: CallbackId,
    error_msg: Option<&str>,
    value: Option<v8::Local<v8::Value>>,
) {
    dispatch_binding_callbacks(
        scope,
        callback_id,
        tables.fetch,
        tables.fetch_error,
        error_msg,
        value,
    );
}
