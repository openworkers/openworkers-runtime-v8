use super::{CallbackId, SchedulerMessage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use v8;

// Shared state accessible from V8 callbacks (for timers)
#[derive(Clone)]
pub struct TimerState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
}

pub fn setup_console(scope: &mut v8::PinScope) {
    let context = scope.get_current_context();
    let global = context.global(scope);

    // Setup native print function using closure
    let print_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            if args.length() > 0
                && let Some(msg_str) = args.get(0).to_string(scope)
            {
                let msg = msg_str.to_rust_string_lossy(scope);
                println!("{}", msg);
            }
        },
    )
    .unwrap();

    let print_key = v8::String::new(scope, "print").unwrap();
    global.set(scope, print_key.into(), print_fn.into());

    // Setup console object
    let code = r#"
        globalThis.console = {
            log: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[LOG] ' + msg);
            },
            warn: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[WARN] ' + msg);
            },
            error: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[ERROR] ' + msg);
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };

    // Create External to hold our state
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    global.set(scope, state_key.into(), external.into());

    // Create __nativeScheduleTimeout function
    let schedule_timeout_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 2
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                    && let Some(delay_val) = args.get(1).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let delay = delay_val.value() as u64;
                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleTimeout(id, delay));
                }
            }
        },
    )
    .unwrap();

    // Create __nativeScheduleInterval function
    let schedule_interval_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 2
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                    && let Some(interval_val) = args.get(1).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let interval = interval_val.value() as u64;
                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleInterval(id, interval));
                }
            }
        },
    )
    .unwrap();

    // Create __nativeClearTimer function
    let clear_timer_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 1
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let _ = state.scheduler_tx.send(SchedulerMessage::ClearTimer(id));
                }
            }
        },
    )
    .unwrap();

    // Register native functions
    let schedule_timeout_key = v8::String::new(scope, "__nativeScheduleTimeout").unwrap();
    global.set(
        scope,
        schedule_timeout_key.into(),
        schedule_timeout_fn.into(),
    );

    let schedule_interval_key = v8::String::new(scope, "__nativeScheduleInterval").unwrap();
    global.set(
        scope,
        schedule_interval_key.into(),
        schedule_interval_fn.into(),
    );

    let clear_timer_key = v8::String::new(scope, "__nativeClearTimer").unwrap();
    global.set(scope, clear_timer_key.into(), clear_timer_fn.into());

    // JavaScript timer wrappers
    let code = r#"
        globalThis.__timerCallbacks = new Map();
        globalThis.__nextTimerId = 1;
        globalThis.__intervalIds = new Set();

        globalThis.setTimeout = function(callback, delay) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, callback);
            __nativeScheduleTimeout(id, delay || 0);
            return id;
        };

        globalThis.setInterval = function(callback, interval) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, callback);
            globalThis.__intervalIds.add(id);
            __nativeScheduleInterval(id, interval || 0);
            return id;
        };

        globalThis.clearTimeout = function(id) {
            globalThis.__timerCallbacks.delete(id);
            globalThis.__intervalIds.delete(id);
            __nativeClearTimer(id);
        };

        globalThis.clearInterval = globalThis.clearTimeout;

        globalThis.__executeTimer = function(id) {
            const callback = globalThis.__timerCallbacks.get(id);
            if (callback) {
                callback();
                // For setTimeout (not interval), remove the callback after execution
                if (!globalThis.__intervalIds.has(id)) {
                    globalThis.__timerCallbacks.delete(id);
                }
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

// Shared state for fetch callbacks
#[derive(Clone)]
pub struct FetchState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Arc<Mutex<CallbackId>>,
}

pub fn setup_fetch(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    next_id: Arc<Mutex<CallbackId>>,
) {
    let state = FetchState {
        scheduler_tx,
        callbacks,
        next_id,
    };

    // Create External to hold our state
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    // Store state in global
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__fetchState").unwrap();
    global.set(scope, state_key.into(), external.into());

    // Create native fetch function as closure
    let native_fetch_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__fetchState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut FetchState;
            let state = unsafe { &*state_ptr };

            if args.length() < 3 {
                return;
            }

            // Parse fetch options (arg 0)
            let options = match args.get(0).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Get URL
            let url_key = v8::String::new(scope, "url").unwrap();
            let url = match options
                .get(scope, url_key.into())
                .and_then(|v| v.to_string(scope))
            {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get method
            let method_key = v8::String::new(scope, "method").unwrap();
            let method_str = options
                .get(scope, method_key.into())
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "GET".to_string());

            let method = super::fetch::HttpMethod::from_str(&method_str)
                .unwrap_or(super::fetch::HttpMethod::Get);

            // Get headers
            let mut headers = std::collections::HashMap::new();
            let headers_key = v8::String::new(scope, "headers").unwrap();
            if let Some(headers_val) = options.get(scope, headers_key.into())
                && let Some(headers_obj) = headers_val.to_object(scope)
                && let Some(props) = headers_obj.get_own_property_names(scope, Default::default())
            {
                for i in 0..props.length() {
                    if let Some(key_val) = props.get_index(scope, i)
                        && let Some(key_str) = key_val.to_string(scope)
                    {
                        let key = key_str.to_rust_string_lossy(scope);
                        if let Some(val) = headers_obj.get(scope, key_val)
                            && let Some(val_str) = val.to_string(scope)
                        {
                            let value = val_str.to_rust_string_lossy(scope);
                            headers.insert(key, value);
                        }
                    }
                }
            }

            // Get body
            let body_key = v8::String::new(scope, "body").unwrap();
            let body = options
                .get(scope, body_key.into())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope));

            // Create FetchRequest
            let request = super::fetch::FetchRequest {
                url,
                method,
                headers,
                body,
            };

            // Get resolve callback
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

            // Send fetch message to scheduler
            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::Fetch(callback_id, request));
        },
    )
    .unwrap();

    let native_fetch_key = v8::String::new(scope, "__nativeFetch").unwrap();
    global.set(scope, native_fetch_key.into(), native_fetch_fn.into());

    // Create __nativeFetchStreaming for streaming fetch
    let native_fetch_streaming_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__fetchState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut FetchState;
            let state = unsafe { &*state_ptr };

            if args.length() < 3 {
                return;
            }

            // Parse fetch options (arg 0)
            let options = match args.get(0).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Get URL
            let url_key = v8::String::new(scope, "url").unwrap();
            let url = match options
                .get(scope, url_key.into())
                .and_then(|v| v.to_string(scope))
            {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get method
            let method_key = v8::String::new(scope, "method").unwrap();
            let method_str = options
                .get(scope, method_key.into())
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "GET".to_string());

            let method = super::fetch::HttpMethod::from_str(&method_str)
                .unwrap_or(super::fetch::HttpMethod::Get);

            // Get headers
            let mut headers = std::collections::HashMap::new();
            let headers_key = v8::String::new(scope, "headers").unwrap();
            if let Some(headers_val) = options.get(scope, headers_key.into())
                && let Some(headers_obj) = headers_val.to_object(scope)
                && let Some(props) = headers_obj.get_own_property_names(scope, Default::default())
            {
                for i in 0..props.length() {
                    if let Some(key_val) = props.get_index(scope, i)
                        && let Some(key_str) = key_val.to_string(scope)
                    {
                        let key = key_str.to_rust_string_lossy(scope);
                        if let Some(val) = headers_obj.get(scope, key_val)
                            && let Some(val_str) = val.to_string(scope)
                        {
                            let value = val_str.to_rust_string_lossy(scope);
                            headers.insert(key, value);
                        }
                    }
                }
            }

            // Get body
            let body_key = v8::String::new(scope, "body").unwrap();
            let body = options
                .get(scope, body_key.into())
                .filter(|v| !v.is_null() && !v.is_undefined())
                .and_then(|v| v.to_string(scope))
                .map(|s| s.to_rust_string_lossy(scope));

            // Create FetchRequest
            let request = super::fetch::FetchRequest {
                url,
                method,
                headers,
                body,
            };

            // Get resolve callback
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

            // Send FetchStreaming message to scheduler
            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::FetchStreaming(callback_id, request));
        },
    )
    .unwrap();

    let native_fetch_streaming_key = v8::String::new(scope, "__nativeFetchStreaming").unwrap();
    global.set(
        scope,
        native_fetch_streaming_key.into(),
        native_fetch_streaming_fn.into(),
    );

    // JavaScript fetch implementation using Promises with streaming support
    let code = r#"
        globalThis.fetch = function(url, options) {
            return new Promise((resolve, reject) => {
                options = options || {};
                const fetchOptions = {
                    url: url,
                    method: options.method || 'GET',
                    headers: options.headers || {},
                    body: options.body || null
                };

                // Use streaming fetch
                __nativeFetchStreaming(fetchOptions, (meta) => {
                    // meta = {status, statusText, headers, streamId}
                    const stream = __createNativeStream(meta.streamId);
                    const response = new Response(stream, {
                        status: meta.status,
                        headers: meta.headers
                    });
                    response.ok = meta.status >= 200 && meta.status < 300;
                    response.statusText = meta.statusText;
                    resolve(response);
                }, reject);
            });
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_url(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.URL = class URL {
            constructor(url) {
                this.href = url;
                const parts = url.match(/^(https?):\/\/([^\/]+)(\/.*)?$/);
                if (parts) {
                    this.protocol = parts[1] + ':';
                    this.hostname = parts[2].split(':')[0];
                    this.host = parts[2];
                    this.pathname = parts[3] || '/';
                } else {
                    this.pathname = '/';
                }
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_headers(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.Headers = class Headers {
            constructor(init) {
                this._map = new Map();

                if (init) {
                    if (init instanceof Headers) {
                        // Copy from another Headers object
                        for (const [key, value] of init) {
                            this._map.set(key, value);
                        }
                    } else if (Array.isArray(init)) {
                        // Array of [key, value] pairs
                        for (const [key, value] of init) {
                            this.append(key, value);
                        }
                    } else if (typeof init === 'object') {
                        // Plain object
                        for (const key of Object.keys(init)) {
                            this.append(key, init[key]);
                        }
                    }
                }
            }

            // Normalize header name (lowercase)
            _normalizeKey(name) {
                return String(name).toLowerCase();
            }

            append(name, value) {
                const key = this._normalizeKey(name);
                const strValue = String(value);
                if (this._map.has(key)) {
                    this._map.set(key, this._map.get(key) + ', ' + strValue);
                } else {
                    this._map.set(key, strValue);
                }
            }

            delete(name) {
                this._map.delete(this._normalizeKey(name));
            }

            get(name) {
                const value = this._map.get(this._normalizeKey(name));
                return value !== undefined ? value : null;
            }

            has(name) {
                return this._map.has(this._normalizeKey(name));
            }

            set(name, value) {
                this._map.set(this._normalizeKey(name), String(value));
            }

            // Iteration methods
            *entries() {
                yield* this._map.entries();
            }

            *keys() {
                yield* this._map.keys();
            }

            *values() {
                yield* this._map.values();
            }

            forEach(callback, thisArg) {
                for (const [key, value] of this._map) {
                    callback.call(thisArg, value, key, this);
                }
            }

            // Make Headers iterable
            [Symbol.iterator]() {
                return this.entries();
            }

            // getSetCookie returns all Set-Cookie headers as array
            getSetCookie() {
                const cookies = [];
                const value = this._map.get('set-cookie');
                if (value) {
                    // Split by ', ' but be careful with cookie values
                    cookies.push(value);
                }
                return cookies;
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_response(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.Response = function(body, init) {
            init = init || {};
            this.status = init.status || 200;
            this.statusText = init.statusText || '';
            this.ok = this.status >= 200 && this.status < 300;
            this.bodyUsed = false;

            // Convert headers to Headers instance
            if (init.headers instanceof Headers) {
                this.headers = init.headers;
            } else {
                this.headers = new Headers(init.headers);
            }
            this._nativeStreamId = null;  // Will be set if body is a native stream

            // Support different body types
            if (body instanceof ReadableStream) {
                // Already a stream - use it directly
                this.body = body;
                // Check if this is a native stream (from fetch)
                if (body._nativeStreamId !== undefined) {
                    this._nativeStreamId = body._nativeStreamId;
                }
            } else if (body instanceof Uint8Array || body instanceof ArrayBuffer) {
                // Binary data - wrap in a stream
                const bytes = body instanceof Uint8Array ? body : new Uint8Array(body);
                this.body = new ReadableStream({
                    start(controller) {
                        controller.enqueue(bytes);
                        controller.close();
                    }
                });
            } else if (body === null || body === undefined) {
                // Empty body
                this.body = new ReadableStream({
                    start(controller) {
                        controller.close();
                    }
                });
            } else {
                // String or other - convert to bytes and wrap in stream
                const encoder = new TextEncoder();
                const bytes = encoder.encode(String(body));
                this.body = new ReadableStream({
                    start(controller) {
                        controller.enqueue(bytes);
                        controller.close();
                    }
                });
            }

            // text() method - read stream and decode to string
            this.text = async function() {
                if (this.bodyUsed) {
                    throw new TypeError('Body has already been consumed');
                }
                this.bodyUsed = true;

                const reader = this.body.getReader();
                const chunks = [];

                try {
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                } finally {
                    reader.releaseLock();
                }

                // Concatenate all chunks
                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                const decoder = new TextDecoder();
                return decoder.decode(result);
            };

            // arrayBuffer() method - read stream and return buffer
            this.arrayBuffer = async function() {
                if (this.bodyUsed) {
                    throw new TypeError('Body has already been consumed');
                }
                this.bodyUsed = true;

                const reader = this.body.getReader();
                const chunks = [];

                try {
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                } finally {
                    reader.releaseLock();
                }

                // Concatenate all chunks
                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                return result.buffer;
            };

            // json() method - decode and parse
            this.json = async function() {
                const text = await this.text();
                return JSON.parse(text);
            };

            // Internal method to synchronously get raw body bytes
            // Used by the Rust runtime to extract response body
            this._getRawBody = function() {
                if (!this.body || !this.body._controller) {
                    return new Uint8Array(0);
                }

                const queue = this.body._controller._queue;
                if (!queue || queue.length === 0) {
                    return new Uint8Array(0);
                }

                // Concatenate all chunks in the queue
                const chunks = [];
                for (const item of queue) {
                    if (item.type === 'chunk' && item.value) {
                        chunks.push(item.value);
                    }
                }

                if (chunks.length === 0) {
                    return new Uint8Array(0);
                }

                // Single chunk - return directly
                if (chunks.length === 1) {
                    return chunks[0];
                }

                // Multiple chunks - concatenate
                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                return result;
            };
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// Shared state for stream read callbacks (same pattern as FetchState)
#[derive(Clone)]
pub struct StreamState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Arc<Mutex<CallbackId>>,
}

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
