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
        const WS_CONNECTING = 0, WS_OPEN = 1, WS_CLOSING = 2, WS_CLOSED = 3;

        // WHATWG-style WebSocket. `new WebSocket(url)` connects and auto-starts
        // receiving. The internal fetch(Upgrade) path adopts an already-connected
        // handle via WebSocket.__adopt(wsId) and keeps the explicit accept() step
        // (so it can hand back the 101 Response before pumping messages).
        class WebSocket {
            constructor(url, protocols) {
                if (url === undefined) {
                    throw new TypeError("Failed to construct 'WebSocket': 1 argument required, but only 0 present.");
                }

                // Validate per spec: scheme must be ws/wss (http/https are normalized),
                // and the URL must not contain a fragment. Invalid input throws SyntaxError.
                const parsed = new URL(String(url));
                if (parsed.hash) {
                    throw WebSocket.__err('SyntaxError', "Failed to construct 'WebSocket': The URL contains a fragment identifier ('" + parsed.hash + "').");
                }
                let target = String(url);
                if (parsed.protocol === 'http:') {
                    target = 'ws://' + target.slice(7);
                } else if (parsed.protocol === 'https:') {
                    target = 'wss://' + target.slice(8);
                } else if (parsed.protocol !== 'ws:' && parsed.protocol !== 'wss:') {
                    throw WebSocket.__err('SyntaxError', "Failed to construct 'WebSocket': The URL's scheme must be either 'ws' or 'wss'.");
                }

                this._wsId = null;
                this._accepted = false;
                this._closing = false;
                this._listeners = { message: [], close: [], error: [], open: [] };
                this.url = target;
                this.binaryType = 'arraybuffer';
                this.bufferedAmount = 0;
                this.extensions = '';
                this.protocol = '';
                this.readyState = WS_CONNECTING;

                const headers = {};
                if (protocols !== undefined && protocols !== null) {
                    headers['Sec-WebSocket-Protocol'] = Array.isArray(protocols) ? protocols.join(', ') : String(protocols);
                }

                const self = this;
                __nativeWebSocketConnect(target, headers, function(wsId) {
                    self._wsId = wsId;
                    if (self._closing) {
                        // close() was called while CONNECTING — abort the fresh socket.
                        __nativeWebSocketClose(wsId, 1000, '');
                        self.readyState = WS_CLOSED;
                        self._dispatch('close', { type: 'close', code: 1006, reason: '', wasClean: false });
                        return;
                    }
                    self._startReceiving();
                    self.readyState = WS_OPEN;
                    self._dispatch('open', { type: 'open' });
                }, function(err) {
                    self.readyState = WS_CLOSED;
                    const message = err == null ? 'connection failed' : String(err);
                    self._dispatch('error', { type: 'error', message: message });
                    self._dispatch('close', { type: 'close', code: 1006, reason: message, wasClean: false });
                });
            }

            static __err(name, message) {
                if (typeof DOMException === 'function') return new DOMException(message, name);
                const e = new Error(message);
                e.name = name;
                return e;
            }

            // Internal: wrap a handle from the fetch(Upgrade) handshake (already open).
            static __adopt(wsId) {
                const ws = Object.create(WebSocket.prototype);
                ws._wsId = wsId;
                ws._accepted = false;
                ws._closing = false;
                ws._listeners = { message: [], close: [], error: [], open: [] };
                ws.url = null;
                ws.binaryType = 'arraybuffer';
                ws.bufferedAmount = 0;
                ws.extensions = '';
                ws.protocol = '';
                ws.readyState = WS_OPEN;
                return ws;
            }

            // Start pumping inbound messages. Auto-called on the standard path;
            // called explicitly via accept() on the fetch(Upgrade) path.
            _startReceiving() {
                if (this._accepted) return;
                this._accepted = true;
                const self = this;

                __nativeWebSocketAccept(this._wsId, function(eventType, arg1, arg2) {
                    if (eventType === 'message') {
                        self._dispatch('message', { type: 'message', data: arg1 });
                    } else if (eventType === 'close') {
                        self.readyState = WS_CLOSED;
                        self._dispatch('close', { type: 'close', code: arg1, reason: arg2, wasClean: true });
                    } else if (eventType === 'error') {
                        self._dispatch('error', { type: 'error', message: arg1 });
                    }
                });
            }

            accept() {
                this._startReceiving();
            }

            send(data) {
                if (this.readyState === WS_CONNECTING) {
                    throw WebSocket.__err('InvalidStateError', "Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.");
                }
                if (this.readyState !== WS_OPEN) {
                    return; // CLOSING/CLOSED: data is silently discarded per spec
                }
                __nativeWebSocketSend(this._wsId, data);
            }

            close(code, reason) {
                if (code !== undefined && code !== 1000 && (code < 3000 || code > 4999)) {
                    throw WebSocket.__err('InvalidAccessError', "Failed to execute 'close' on 'WebSocket': The code must be 1000 or in the range 3000-4999.");
                }
                if (reason !== undefined && new TextEncoder().encode(String(reason)).length > 123) {
                    throw WebSocket.__err('SyntaxError', "Failed to execute 'close' on 'WebSocket': The close reason must not exceed 123 bytes.");
                }
                if (this.readyState === WS_CLOSING || this.readyState === WS_CLOSED) return;
                if (this.readyState === WS_CONNECTING) {
                    // Abort once the pending connect resolves (handled in the constructor).
                    this._closing = true;
                    this.readyState = WS_CLOSING;
                    return;
                }
                this.readyState = WS_CLOSING;
                __nativeWebSocketClose(this._wsId, code === undefined ? 1000 : code, reason === undefined ? '' : reason);
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
                const handler = this['on' + type];
                if (typeof handler === 'function') {
                    try { handler.call(this, event); } catch(e) { console.error('WebSocket on' + type + ' error:', e); }
                }

                const listeners = this._listeners[type];
                if (listeners) {
                    for (let i = 0; i < listeners.length; i++) {
                        try { listeners[i].call(this, event); } catch(e) { console.error('WebSocket listener error:', e); }
                    }
                }
            }
        }

        WebSocket.CONNECTING = WS_CONNECTING;
        WebSocket.OPEN = WS_OPEN;
        WebSocket.CLOSING = WS_CLOSING;
        WebSocket.CLOSED = WS_CLOSED;
        WebSocket.prototype.CONNECTING = WS_CONNECTING;
        WebSocket.prototype.OPEN = WS_OPEN;
        WebSocket.prototype.CLOSING = WS_CLOSING;
        WebSocket.prototype.CLOSED = WS_CLOSED;

        globalThis.WebSocket = WebSocket;
    "#;

    exec_js!(scope, code);
}
