use super::{CallbackId, SchedulerMessage};
use openworkers_core::{HttpBody, HttpMethod, HttpRequest};
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

            let method = HttpMethod::from_str(&method_str).unwrap_or(HttpMethod::Get);

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
                .map(|s| HttpBody::Bytes(bytes::Bytes::from(s.to_rust_string_lossy(scope))))
                .unwrap_or(HttpBody::None);
            // Create HttpRequest
            let request = HttpRequest {
                method,
                url,
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

pub fn setup_blob(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.Blob = class Blob {
            constructor(blobParts = [], options = {}) {
                this.type = options.type || '';
                this._parts = [];

                for (const part of blobParts) {
                    if (part instanceof Blob) {
                        this._parts.push(...part._parts);
                    } else if (part instanceof ArrayBuffer) {
                        this._parts.push(new Uint8Array(part));
                    } else if (ArrayBuffer.isView(part)) {
                        this._parts.push(new Uint8Array(part.buffer, part.byteOffset, part.byteLength));
                    } else {
                        this._parts.push(new TextEncoder().encode(String(part)));
                    }
                }
            }

            get size() {
                return this._parts.reduce((sum, part) => sum + part.byteLength, 0);
            }

            slice(start = 0, end = this.size, contentType = '') {
                const bytes = this._getBytes();
                const sliced = bytes.slice(start, end);
                return new Blob([sliced], { type: contentType });
            }

            async arrayBuffer() {
                return this._getBytes().buffer;
            }

            async text() {
                return new TextDecoder().decode(this._getBytes());
            }

            stream() {
                const bytes = this._getBytes();
                return new ReadableStream({
                    start(controller) {
                        controller.enqueue(bytes);
                        controller.close();
                    }
                });
            }

            _getBytes() {
                if (this._parts.length === 0) return new Uint8Array(0);
                if (this._parts.length === 1) return this._parts[0];

                const totalLength = this._parts.reduce((sum, p) => sum + p.byteLength, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const part of this._parts) {
                    result.set(part, offset);
                    offset += part.byteLength;
                }
                return result;
            }
        };

        globalThis.File = class File extends Blob {
            constructor(fileBits, fileName, options = {}) {
                super(fileBits, options);
                this.name = fileName;
                this.lastModified = options.lastModified || Date.now();
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_abort_controller(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.AbortSignal = class AbortSignal {
            constructor() {
                this.aborted = false;
                this.reason = undefined;
                this._listeners = [];
            }

            addEventListener(type, listener) {
                if (type === 'abort') {
                    this._listeners.push(listener);
                }
            }

            removeEventListener(type, listener) {
                if (type === 'abort') {
                    this._listeners = this._listeners.filter(l => l !== listener);
                }
            }

            throwIfAborted() {
                if (this.aborted) {
                    throw this.reason;
                }
            }

            _abort(reason) {
                if (this.aborted) return;
                this.aborted = true;
                this.reason = reason;
                const event = { type: 'abort', target: this };
                for (const listener of this._listeners) {
                    try { listener(event); } catch (e) { console.error(e); }
                }
            }

            static abort(reason) {
                const signal = new AbortSignal();
                signal._abort(reason || new DOMException('Aborted', 'AbortError'));
                return signal;
            }

            static timeout(ms) {
                const signal = new AbortSignal();
                setTimeout(() => {
                    signal._abort(new DOMException('Timeout', 'TimeoutError'));
                }, ms);
                return signal;
            }
        };

        globalThis.AbortController = class AbortController {
            constructor() {
                this.signal = new AbortSignal();
            }

            abort(reason) {
                this.signal._abort(reason || new DOMException('Aborted', 'AbortError'));
            }
        };

        // DOMException if not defined
        if (typeof DOMException === 'undefined') {
            globalThis.DOMException = class DOMException extends Error {
                constructor(message, name) {
                    super(message);
                    this.name = name || 'Error';
                }
            };
        }
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_structured_clone(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.structuredClone = function(value, options) {
            // Handle transferables (simplified - just ignore them for now)
            const transfer = options?.transfer || [];

            // Use JSON for simple cases, but handle more types
            function clone(obj, seen = new Map()) {
                // Primitives
                if (obj === null || typeof obj !== 'object') {
                    return obj;
                }

                // Check for circular references
                if (seen.has(obj)) {
                    return seen.get(obj);
                }

                // Date
                if (obj instanceof Date) {
                    return new Date(obj.getTime());
                }

                // RegExp
                if (obj instanceof RegExp) {
                    return new RegExp(obj.source, obj.flags);
                }

                // ArrayBuffer
                if (obj instanceof ArrayBuffer) {
                    const copy = new ArrayBuffer(obj.byteLength);
                    new Uint8Array(copy).set(new Uint8Array(obj));
                    return copy;
                }

                // TypedArrays
                if (ArrayBuffer.isView(obj)) {
                    const TypedArrayConstructor = obj.constructor;
                    return new TypedArrayConstructor(clone(obj.buffer, seen), obj.byteOffset, obj.length);
                }

                // Map
                if (obj instanceof Map) {
                    const copy = new Map();
                    seen.set(obj, copy);
                    for (const [key, val] of obj) {
                        copy.set(clone(key, seen), clone(val, seen));
                    }
                    return copy;
                }

                // Set
                if (obj instanceof Set) {
                    const copy = new Set();
                    seen.set(obj, copy);
                    for (const val of obj) {
                        copy.add(clone(val, seen));
                    }
                    return copy;
                }

                // Array
                if (Array.isArray(obj)) {
                    const copy = [];
                    seen.set(obj, copy);
                    for (let i = 0; i < obj.length; i++) {
                        copy[i] = clone(obj[i], seen);
                    }
                    return copy;
                }

                // Plain object
                const copy = {};
                seen.set(obj, copy);
                for (const key of Object.keys(obj)) {
                    copy[key] = clone(obj[key], seen);
                }
                return copy;
            }

            return clone(value);
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_base64(scope: &mut v8::PinScope) {
    let code = r#"
        // Base64 encoding/decoding (atob/btoa)
        const BASE64_CHARS = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

        globalThis.btoa = function(str) {
            const bytes = typeof str === 'string'
                ? new TextEncoder().encode(str)
                : new Uint8Array(str);

            let result = '';
            const len = bytes.length;

            for (let i = 0; i < len; i += 3) {
                const b1 = bytes[i];
                const b2 = i + 1 < len ? bytes[i + 1] : 0;
                const b3 = i + 2 < len ? bytes[i + 2] : 0;

                result += BASE64_CHARS[b1 >> 2];
                result += BASE64_CHARS[((b1 & 3) << 4) | (b2 >> 4)];
                result += i + 1 < len ? BASE64_CHARS[((b2 & 15) << 2) | (b3 >> 6)] : '=';
                result += i + 2 < len ? BASE64_CHARS[b3 & 63] : '=';
            }

            return result;
        };

        globalThis.atob = function(base64) {
            // Remove whitespace and validate
            base64 = base64.replace(/\s/g, '');

            const len = base64.length;
            if (len % 4 !== 0) {
                throw new DOMException('Invalid base64 string', 'InvalidCharacterError');
            }

            // Calculate output length
            let outputLen = (len / 4) * 3;
            if (base64[len - 1] === '=') outputLen--;
            if (base64[len - 2] === '=') outputLen--;

            const bytes = new Uint8Array(outputLen);
            let p = 0;

            for (let i = 0; i < len; i += 4) {
                const c1 = BASE64_CHARS.indexOf(base64[i]);
                const c2 = BASE64_CHARS.indexOf(base64[i + 1]);
                const c3 = base64[i + 2] === '=' ? 0 : BASE64_CHARS.indexOf(base64[i + 2]);
                const c4 = base64[i + 3] === '=' ? 0 : BASE64_CHARS.indexOf(base64[i + 3]);

                if (c1 === -1 || c2 === -1 || (base64[i + 2] !== '=' && c3 === -1) || (base64[i + 3] !== '=' && c4 === -1)) {
                    throw new DOMException('Invalid base64 character', 'InvalidCharacterError');
                }

                bytes[p++] = (c1 << 2) | (c2 >> 4);
                if (base64[i + 2] !== '=') bytes[p++] = ((c2 & 15) << 4) | (c3 >> 2);
                if (base64[i + 3] !== '=') bytes[p++] = ((c3 & 3) << 6) | c4;
            }

            return new TextDecoder().decode(bytes);
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_url_search_params(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.URLSearchParams = class URLSearchParams {
            constructor(init) {
                this._entries = [];

                if (!init) return;

                if (typeof init === 'string') {
                    // Parse query string
                    const str = init.startsWith('?') ? init.slice(1) : init;
                    if (str) {
                        for (const pair of str.split('&')) {
                            const [key, value = ''] = pair.split('=').map(decodeURIComponent);
                            this._entries.push([key, value]);
                        }
                    }
                } else if (init instanceof URLSearchParams) {
                    this._entries = [...init._entries];
                } else if (Array.isArray(init)) {
                    for (const [key, value] of init) {
                        this._entries.push([String(key), String(value)]);
                    }
                } else if (typeof init === 'object') {
                    for (const key of Object.keys(init)) {
                        this._entries.push([key, String(init[key])]);
                    }
                }
            }

            append(name, value) {
                this._entries.push([String(name), String(value)]);
            }

            delete(name) {
                this._entries = this._entries.filter(([k]) => k !== name);
            }

            get(name) {
                const entry = this._entries.find(([k]) => k === name);
                return entry ? entry[1] : null;
            }

            getAll(name) {
                return this._entries.filter(([k]) => k === name).map(([, v]) => v);
            }

            has(name) {
                return this._entries.some(([k]) => k === name);
            }

            set(name, value) {
                const strName = String(name);
                const strValue = String(value);
                let found = false;
                this._entries = this._entries.filter(([k]) => {
                    if (k === strName) {
                        if (!found) {
                            found = true;
                            return true;
                        }
                        return false;
                    }
                    return true;
                });
                if (found) {
                    const idx = this._entries.findIndex(([k]) => k === strName);
                    this._entries[idx][1] = strValue;
                } else {
                    this._entries.push([strName, strValue]);
                }
            }

            sort() {
                this._entries.sort((a, b) => a[0].localeCompare(b[0]));
            }

            toString() {
                return this._entries
                    .map(([k, v]) => encodeURIComponent(k) + '=' + encodeURIComponent(v))
                    .join('&');
            }

            *entries() {
                yield* this._entries;
            }

            *keys() {
                for (const [k] of this._entries) yield k;
            }

            *values() {
                for (const [, v] of this._entries) yield v;
            }

            forEach(callback, thisArg) {
                for (const [key, value] of this._entries) {
                    callback.call(thisArg, value, key, this);
                }
            }

            [Symbol.iterator]() {
                return this.entries();
            }

            get size() {
                return this._entries.length;
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_url(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.URL = class URL {
            constructor(url, base) {
                if (base) {
                    // Handle relative URLs
                    const baseUrl = typeof base === 'string' ? base : base.href;
                    if (url.startsWith('/')) {
                        const match = baseUrl.match(/^(https?:\/\/[^\/]+)/);
                        url = match ? match[1] + url : url;
                    } else if (!url.match(/^https?:\/\//)) {
                        url = baseUrl.replace(/\/[^\/]*$/, '/') + url;
                    }
                }

                this.href = url;
                const match = url.match(/^(https?):\/\/([^\/\?#]+)(\/[^\?#]*)?(\?[^#]*)?(#.*)?$/);
                if (match) {
                    this.protocol = match[1] + ':';
                    this.host = match[2];
                    this.hostname = match[2].split(':')[0];
                    this.port = match[2].includes(':') ? match[2].split(':')[1] : '';
                    this.pathname = match[3] || '/';
                    this.search = match[4] || '';
                    this.hash = match[5] || '';
                    this.origin = this.protocol + '//' + this.host;
                    this.searchParams = new URLSearchParams(this.search);
                } else {
                    this.protocol = '';
                    this.host = '';
                    this.hostname = '';
                    this.port = '';
                    this.pathname = url;
                    this.search = '';
                    this.hash = '';
                    this.origin = '';
                    this.searchParams = new URLSearchParams();
                }
            }

            toString() {
                return this.href;
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

pub fn setup_request(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.Request = class Request {
            constructor(input, init) {
                init = init || {};

                // Handle input - can be a URL string or another Request
                if (input instanceof Request) {
                    // Clone from another Request
                    this.url = input.url;
                    this.method = init.method || input.method;
                    this.headers = new Headers(init.headers || input.headers);
                    // Body handling for clone
                    if (init.body !== undefined) {
                        this._initBody(init.body);
                    } else if (input.body && !input.bodyUsed) {
                        this._initBody(input.body);
                    } else {
                        this.body = null;
                    }
                } else {
                    // URL string
                    this.url = String(input);
                    this.method = (init.method || 'GET').toUpperCase();
                    this.headers = new Headers(init.headers);
                    this._initBody(init.body);
                }

                this.bodyUsed = false;

                // Additional properties (simplified)
                this.mode = init.mode || 'cors';
                this.credentials = init.credentials || 'same-origin';
                this.cache = init.cache || 'default';
                this.redirect = init.redirect || 'follow';
                this.referrer = init.referrer || 'about:client';
                this.integrity = init.integrity || '';
            }

            _initBody(body) {
                if (body instanceof ReadableStream) {
                    this.body = body;
                } else if (body instanceof Uint8Array || body instanceof ArrayBuffer) {
                    const bytes = body instanceof Uint8Array ? body : new Uint8Array(body);
                    this.body = new ReadableStream({
                        start(controller) {
                            controller.enqueue(bytes);
                            controller.close();
                        }
                    });
                } else if (body === null || body === undefined) {
                    this.body = null;
                } else {
                    // String or other
                    const encoder = new TextEncoder();
                    const bytes = encoder.encode(String(body));
                    this.body = new ReadableStream({
                        start(controller) {
                            controller.enqueue(bytes);
                            controller.close();
                        }
                    });
                }
            }

            async text() {
                if (this.bodyUsed) {
                    throw new TypeError('Body has already been consumed');
                }
                this.bodyUsed = true;

                if (!this.body) {
                    return '';
                }

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

                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                const decoder = new TextDecoder();
                return decoder.decode(result);
            }

            async json() {
                const text = await this.text();
                return JSON.parse(text);
            }

            async arrayBuffer() {
                if (this.bodyUsed) {
                    throw new TypeError('Body has already been consumed');
                }
                this.bodyUsed = true;

                if (!this.body) {
                    return new ArrayBuffer(0);
                }

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

                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                return result.buffer;
            }

            clone() {
                if (this.bodyUsed) {
                    throw new TypeError('Cannot clone a Request whose body has been consumed');
                }

                // For simplicity, create a new Request with same properties
                // Note: proper implementation would tee() the body stream
                return new Request(this.url, {
                    method: this.method,
                    headers: this.headers,
                    body: this.body,
                    mode: this.mode,
                    credentials: this.credentials,
                    cache: this.cache,
                    redirect: this.redirect,
                    referrer: this.referrer,
                    integrity: this.integrity
                });
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

            // Support different body types - all wrapped in ReadableStream
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
                // Empty body - body should be null per spec
                this.body = null;
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

/// Setup response streaming operations
/// These allow JS to stream response body chunks back to Rust
pub fn setup_response_stream_ops(
    scope: &mut v8::PinScope,
    stream_manager: std::sync::Arc<super::stream_manager::StreamManager>,
) {
    // Store stream_manager in global for access from callbacks
    let manager_ptr = std::sync::Arc::into_raw(stream_manager) as *mut std::ffi::c_void;
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
            let manager_ptr = external.value() as *const super::stream_manager::StreamManager;
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
            let manager_ptr = external.value() as *const super::stream_manager::StreamManager;
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
                    super::stream_manager::StreamChunk::Data(bytes::Bytes::from(bytes_vec)),
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
            let manager_ptr = external.value() as *const super::stream_manager::StreamManager;
            let manager = unsafe { &*manager_ptr };

            // Get stream_id
            let stream_id = if let Some(id_val) = args.get(0).to_uint32(scope) {
                id_val.value() as u64
            } else {
                return;
            };

            // Send Done chunk to signal end of stream
            let _ = manager.try_write_chunk(stream_id, super::stream_manager::StreamChunk::Done);
        },
    )
    .unwrap();

    let end_key = v8::String::new(scope, "__responseStreamEnd").unwrap();
    global.set(scope, end_key.into(), end_fn.into());
}
