//! WebSocket native bindings for outgoing (client) connections.
//!
//! Provides native functions called by the JS WebSocket class:
//! - `__nativeWebSocketConnect(url, headers, resolve, reject)` — initiate handshake
//! - `__nativeWebSocketAccept(wsId, dispatcher)` — start receiving messages
//! - `__nativeWebSocketSend(wsId, data)` — send a message
//! - `__nativeWebSocketClose(wsId, code, reason)` — close the connection
//!
//! Connect resolve/reject callbacks reuse FetchState (same CallbackId pattern).
//! Event dispatchers (message/close/error) use WebSocketEventState (keyed by WebSocketId).

use super::state::{FetchState, WebSocketEventState};
use openworkers_core::WebSocketOutgoing;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use v8;

use super::super::scheduler::SchedulerMessage;

/// Register all WebSocket native functions and the JS WebSocket class.
pub fn setup_websocket(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    ws_event_callbacks: Rc<RefCell<HashMap<u64, v8::Global<v8::Function>>>>,
) {
    // Store WebSocketEventState in the context slot
    let ws_state = WebSocketEventState {
        callbacks: ws_event_callbacks,
    };
    store_state!(scope, ws_state);

    // __nativeWebSocketConnect(url, headers, resolve, reject)
    let connect_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            // Parse url
            let url = match args.get(0).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Parse headers object
            let headers = if args.get(1).is_object() {
                serde_v8::from_v8_any::<HashMap<String, String>>(scope, args.get(1))
                    .unwrap_or_default()
            } else {
                HashMap::new()
            };

            // resolve and reject callbacks
            let resolve_cb = args.get(2);
            let reject_cb = args.get(3);

            if !resolve_cb.is_function() || !reject_cb.is_function() {
                return;
            }

            let resolve_fn: v8::Local<v8::Function> = resolve_cb.try_into().unwrap();
            let reject_fn: v8::Local<v8::Function> = reject_cb.try_into().unwrap();

            // Register callbacks using FetchState (same pattern as fetch)
            let callback_id = {
                let mut id = state.next_id.borrow_mut();
                let current = *id;
                *id += 1;
                current
            };

            state
                .callbacks
                .borrow_mut()
                .insert(callback_id, v8::Global::new(scope.as_ref(), resolve_fn));
            state
                .error_callbacks
                .borrow_mut()
                .insert(callback_id, v8::Global::new(scope.as_ref(), reject_fn));

            let _ = state.scheduler_tx.send(SchedulerMessage::WebSocketConnect(
                callback_id,
                url,
                headers,
            ));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeWebSocketConnect", connect_fn);

    // __nativeWebSocketAccept(wsId, dispatcher)
    let accept_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue| {
            let Some(fetch_state) = get_state!(scope, FetchState) else {
                return;
            };
            let fetch_state = fetch_state.clone();

            let Some(ws_state) = get_state!(scope, WebSocketEventState) else {
                return;
            };
            let ws_state = ws_state.clone();

            let ws_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
            let dispatcher_cb = args.get(1);

            if !dispatcher_cb.is_function() {
                return;
            }

            let dispatcher_fn: v8::Local<v8::Function> = dispatcher_cb.try_into().unwrap();

            // Store the dispatcher callback (persistent, not one-shot)
            ws_state
                .callbacks
                .borrow_mut()
                .insert(ws_id, v8::Global::new(scope.as_ref(), dispatcher_fn));

            // Tell the scheduler to start pumping messages
            let _ = fetch_state
                .scheduler_tx
                .send(SchedulerMessage::WebSocketAccept(ws_id));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeWebSocketAccept", accept_fn);

    // __nativeWebSocketSend(wsId, data)
    let send_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };
            let state = state.clone();

            let ws_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
            let data = args.get(1);

            let msg = if data.is_string() {
                let s = data
                    .to_string(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_default();
                WebSocketOutgoing::Text(s)
            } else if data.is_array_buffer() {
                let ab: v8::Local<v8::ArrayBuffer> = data.try_into().unwrap();
                let store = ab.get_backing_store();
                let bytes = store.iter().map(|cell| cell.get()).collect::<Vec<u8>>();
                WebSocketOutgoing::Binary(bytes)
            } else if data.is_array_buffer_view() {
                let view: v8::Local<v8::ArrayBufferView> = data.try_into().unwrap();
                let len = view.byte_length();
                let mut buf = vec![0u8; len];
                view.copy_contents(&mut buf);
                WebSocketOutgoing::Binary(buf)
            } else {
                // Fallback: convert to string
                let s = data
                    .to_string(scope)
                    .map(|s| s.to_rust_string_lossy(scope))
                    .unwrap_or_default();
                WebSocketOutgoing::Text(s)
            };

            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::WebSocketSend(ws_id, msg));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeWebSocketSend", send_fn);

    // __nativeWebSocketClose(wsId, code, reason)
    let close_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };
            let state = state.clone();

            let ws_id = args.get(0).number_value(scope).unwrap_or(0.0) as u64;
            let code = args.get(1).number_value(scope).unwrap_or(1000.0) as u16;
            let reason = args
                .get(2)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_default();

            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::WebSocketClose(ws_id, code, reason));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeWebSocketClose", close_fn);

    // JavaScript WebSocket class + fetch integration
    let code = r#"
        class WebSocket {
            constructor(wsId) {
                this._wsId = wsId;
                this._accepted = false;
                this._listeners = { message: [], close: [], error: [], open: [] };
            }

            accept() {
                if (this._accepted) return;
                this._accepted = true;
                const self = this;

                __nativeWebSocketAccept(this._wsId, function(eventType, arg1, arg2) {
                    if (eventType === 'message') {
                        self._dispatch('message', { type: 'message', data: arg1 });
                    } else if (eventType === 'close') {
                        self._dispatch('close', { type: 'close', code: arg1, reason: arg2, wasClean: true });
                    } else if (eventType === 'error') {
                        self._dispatch('error', { type: 'error', message: arg1 });
                    }
                });
            }

            send(data) {
                if (!this._accepted) throw new Error('WebSocket is not accepted');
                __nativeWebSocketSend(this._wsId, data);
            }

            close(code, reason) {
                if (code === undefined) code = 1000;
                if (reason === undefined) reason = '';
                __nativeWebSocketClose(this._wsId, code, reason);
            }

            addEventListener(type, listener) {
                if (this._listeners[type]) {
                    this._listeners[type].push(listener);
                }
            }

            removeEventListener(type, listener) {
                if (this._listeners[type]) {
                    this._listeners[type] = this._listeners[type].filter(function(l) { return l !== listener; });
                }
            }

            _dispatch(type, event) {
                const listeners = this._listeners[type];

                if (listeners) {
                    for (let i = 0; i < listeners.length; i++) {
                        try { listeners[i](event); } catch(e) { console.error('WebSocket listener error:', e); }
                    }
                }
            }
        }

        globalThis.WebSocket = WebSocket;
    "#;

    exec_js!(scope, code);
}
