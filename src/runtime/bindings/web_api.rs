use super::state::PerformanceState;
use std::rc::Rc;
use std::time::Instant;
use v8;

/// Native performance.now() - returns elapsed milliseconds since worker start
/// Rounded to 100µs precision to mitigate timing attacks
#[glue_v8::method(fast, state = Rc<PerformanceState>)]
fn performance_now(state: &Rc<PerformanceState>) -> f64 {
    let micros = state.start.elapsed().as_micros() / 100 * 100;
    micros as f64 / 1000.0
}

/// Setup global aliases for compatibility with browser/Node.js code
/// Adds `self` and `global` as aliases for `globalThis`
pub fn setup_global_aliases(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.self = globalThis;
        globalThis.global = globalThis;
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_performance(scope: &mut v8::PinScope) {
    let state = Rc::new(PerformanceState {
        start: Instant::now(),
    });

    // Store in context slot to keep Rc alive for the context's lifetime
    scope.get_current_context().set_slot(state.clone());

    let now_fn = performance_now_v8_template(scope, &state)
        .get_function(scope)
        .unwrap();

    let context = scope.get_current_context();
    let global = context.global(scope);

    let perf_obj = v8::Object::new(scope);
    let now_key = v8::String::new(scope, "now").unwrap();
    perf_obj.set(scope, now_key.into(), now_fn.into());

    let perf_key = v8::String::new(scope, "performance").unwrap();
    global.set(scope, perf_key.into(), perf_obj.into());
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

pub fn setup_form_data(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.FormData = class FormData {
            constructor() {
                this._entries = [];
            }

            append(name, value, filename) {
                if (value instanceof Blob && filename === undefined && value instanceof File) {
                    filename = value.name;
                }
                this._entries.push([String(name), value, filename]);
            }

            delete(name) {
                const strName = String(name);
                this._entries = this._entries.filter(([k]) => k !== strName);
            }

            get(name) {
                const strName = String(name);
                const entry = this._entries.find(([k]) => k === strName);
                return entry ? entry[1] : null;
            }

            getAll(name) {
                const strName = String(name);
                return this._entries.filter(([k]) => k === strName).map(([, v]) => v);
            }

            has(name) {
                const strName = String(name);
                return this._entries.some(([k]) => k === strName);
            }

            set(name, value, filename) {
                const strName = String(name);
                if (value instanceof Blob && filename === undefined && value instanceof File) {
                    filename = value.name;
                }
                // Remove all existing entries with this name
                this._entries = this._entries.filter(([k]) => k !== strName);
                // Add the new entry
                this._entries.push([strName, value, filename]);
            }

            *entries() {
                for (const [name, value] of this._entries) {
                    yield [name, value];
                }
            }

            *keys() {
                for (const [name] of this._entries) {
                    yield name;
                }
            }

            *values() {
                for (const [, value] of this._entries) {
                    yield value;
                }
            }

            forEach(callback, thisArg) {
                for (const [name, value] of this._entries) {
                    callback.call(thisArg, value, name, this);
                }
            }

            [Symbol.iterator]() {
                return this.entries();
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
                // Handle URL object input - convert to string
                if (url instanceof URL) {
                    url = url.href;
                } else {
                    url = String(url);
                }

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

                    // Handle streaming body from native (passed as _bodyStreamId)
                    if (init._bodyStreamId !== undefined) {
                        this.body = __createNativeStream(init._bodyStreamId);
                    } else {
                        this._initBody(init.body);
                    }
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

            async formData() {
                const contentType = this.headers.get('content-type') || '';
                const formData = new FormData();

                if (contentType.includes('application/x-www-form-urlencoded')) {
                    const text = await this.text();
                    const params = new URLSearchParams(text);
                    for (const [key, value] of params) {
                        formData.append(key, value);
                    }
                } else if (contentType.includes('multipart/form-data')) {
                    const boundaryMatch = contentType.match(/boundary=(?:"([^"]+)"|([^;\s]+))/);
                    if (!boundaryMatch) {
                        throw new TypeError('Missing boundary in multipart/form-data');
                    }
                    const boundary = '--' + (boundaryMatch[1] || boundaryMatch[2]);
                    const boundaryBytes = new TextEncoder().encode(boundary);
                    const crlfcrlf = new Uint8Array([13, 10, 13, 10]); // \r\n\r\n
                    const crlf = new Uint8Array([13, 10]); // \r\n

                    const buffer = await this.arrayBuffer();
                    const data = new Uint8Array(buffer);

                    // Find byte pattern in array
                    const findPattern = (arr, pattern, start = 0) => {
                        outer: for (let i = start; i <= arr.length - pattern.length; i++) {
                            for (let j = 0; j < pattern.length; j++) {
                                if (arr[i + j] !== pattern[j]) continue outer;
                            }
                            return i;
                        }
                        return -1;
                    };

                    let pos = findPattern(data, boundaryBytes);
                    while (pos !== -1) {
                        pos += boundaryBytes.length;

                        // Check for final boundary (--)
                        if (data[pos] === 45 && data[pos + 1] === 45) break;

                        // Skip CRLF after boundary
                        if (data[pos] === 13 && data[pos + 1] === 10) pos += 2;

                        // Find header end
                        const headerEnd = findPattern(data, crlfcrlf, pos);
                        if (headerEnd === -1) break;

                        // Parse headers as text
                        const headerBytes = data.slice(pos, headerEnd);
                        const headerText = new TextDecoder().decode(headerBytes);

                        // Find next boundary for body end
                        const nextBoundary = findPattern(data, boundaryBytes, headerEnd + 4);
                        const bodyEnd = nextBoundary !== -1 ? nextBoundary - 2 : data.length; // -2 for CRLF before boundary

                        // Extract body as bytes
                        const bodyBytes = data.slice(headerEnd + 4, bodyEnd);

                        const nameMatch = headerText.match(/name="([^"]+)"/);
                        if (!nameMatch) {
                            pos = nextBoundary;
                            continue;
                        }

                        const name = nameMatch[1];
                        const filenameMatch = headerText.match(/filename="([^"]+)"/);

                        if (filenameMatch) {
                            const filename = filenameMatch[1];
                            const contentTypeMatch = headerText.match(/Content-Type:\s*([^\r\n]+)/i);
                            const fileType = contentTypeMatch ? contentTypeMatch[1].trim() : 'application/octet-stream';
                            const file = new File([bodyBytes], filename, { type: fileType });
                            formData.append(name, file, filename);
                        } else {
                            // Text field - decode as UTF-8
                            const value = new TextDecoder().decode(bodyBytes);
                            formData.append(name, value);
                        }

                        pos = nextBoundary;
                    }
                } else {
                    throw new TypeError('Invalid content-type for formData()');
                }

                return formData;
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

                let clonedBody = null;
                if (this.body) {
                    const [stream1, stream2] = this.body.tee();
                    this.body = stream1;
                    clonedBody = stream2;
                }

                return new Request(this.url, {
                    method: this.method,
                    headers: new Headers(this.headers),
                    body: clonedBody,
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
        globalThis.Response = class Response {
            constructor(body, init) {
                init = init || {};
                this.status = init.status || 200;
                this.statusText = init.statusText || '';
                this.ok = this.status >= 200 && this.status < 300;
                this.bodyUsed = false;
                this._nativeStreamId = null;

                // Standard Response properties
                this.url = init.url || '';
                this.type = init.type || 'default';
                this.redirected = init.redirected || false;

                // Convert headers to Headers instance
                if (init.headers instanceof Headers) {
                    this.headers = init.headers;
                } else {
                    this.headers = new Headers(init.headers);
                }

                // Support different body types - all wrapped in ReadableStream
                // _isBuffered marks responses where body data is already fully available
                // (created from string/Uint8Array/ArrayBuffer, not a user-provided stream)
                if (body instanceof ReadableStream) {
                    this.body = body;
                    this._isBuffered = false; // User-provided stream - must be streamed
                    if (body._nativeStreamId !== undefined) {
                        this._nativeStreamId = body._nativeStreamId;
                    }
                } else if (body instanceof Uint8Array || body instanceof ArrayBuffer) {
                    const bytes = body instanceof Uint8Array ? body : new Uint8Array(body);
                    this.body = new ReadableStream({
                        start(controller) {
                            controller.enqueue(bytes);
                            controller.close();
                        }
                    });
                    this._isBuffered = true; // Data fully available - can use _getRawBody()
                } else if (body === null || body === undefined) {
                    this.body = null;
                    this._isBuffered = true; // No body - definitely buffered
                } else {
                    const encoder = new TextEncoder();
                    const bytes = encoder.encode(String(body));
                    this.body = new ReadableStream({
                        start(controller) {
                            controller.enqueue(bytes);
                            controller.close();
                        }
                    });
                    this._isBuffered = true; // Data fully available - can use _getRawBody()
                }
            }

            async text() {
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

            async arrayBuffer() {
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

                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                return result.buffer;
            }

            async json() {
                const text = await this.text();
                return JSON.parse(text);
            }

            async formData() {
                const contentType = this.headers.get('content-type') || '';
                const formData = new FormData();

                if (contentType.includes('application/x-www-form-urlencoded')) {
                    const text = await this.text();
                    const params = new URLSearchParams(text);

                    for (const [key, value] of params) {
                        formData.append(key, value);
                    }
                } else if (contentType.includes('multipart/form-data')) {
                    const boundaryMatch = contentType.match(/boundary=(?:"([^"]+)"|([^;\s]+))/);

                    if (!boundaryMatch) {
                        throw new TypeError('Missing boundary in multipart/form-data');
                    }

                    const boundary = '--' + (boundaryMatch[1] || boundaryMatch[2]);
                    const boundaryBytes = new TextEncoder().encode(boundary);
                    const crlfcrlf = new Uint8Array([13, 10, 13, 10]); // \r\n\r\n

                    const buffer = await this.arrayBuffer();
                    const data = new Uint8Array(buffer);

                    // Find byte pattern in array
                    const findPattern = (arr, pattern, start = 0) => {
                        outer: for (let i = start; i <= arr.length - pattern.length; i++) {
                            for (let j = 0; j < pattern.length; j++) {
                                if (arr[i + j] !== pattern[j]) continue outer;
                            }
                            return i;
                        }
                        return -1;
                    };

                    let pos = findPattern(data, boundaryBytes);

                    while (pos !== -1) {
                        pos += boundaryBytes.length;

                        // Check for final boundary (--)
                        if (data[pos] === 45 && data[pos + 1] === 45) break;

                        // Skip CRLF after boundary
                        if (data[pos] === 13 && data[pos + 1] === 10) pos += 2;

                        // Find header end
                        const headerEnd = findPattern(data, crlfcrlf, pos);
                        if (headerEnd === -1) break;

                        // Parse headers as text
                        const headerBytes = data.slice(pos, headerEnd);
                        const headerText = new TextDecoder().decode(headerBytes);

                        // Find next boundary for body end
                        const nextBoundary = findPattern(data, boundaryBytes, headerEnd + 4);
                        const bodyEnd = nextBoundary !== -1 ? nextBoundary - 2 : data.length; // -2 for CRLF before boundary

                        // Extract body as bytes
                        const bodyBytes = data.slice(headerEnd + 4, bodyEnd);

                        const nameMatch = headerText.match(/name="([^"]+)"/);

                        if (!nameMatch) {
                            pos = nextBoundary;
                            continue;
                        }

                        const name = nameMatch[1];
                        const filenameMatch = headerText.match(/filename="([^"]+)"/);

                        if (filenameMatch) {
                            const filename = filenameMatch[1];
                            const contentTypeMatch = headerText.match(/Content-Type:\s*([^\r\n]+)/i);
                            const fileType = contentTypeMatch ? contentTypeMatch[1].trim() : 'application/octet-stream';
                            const file = new File([bodyBytes], filename, { type: fileType });
                            formData.append(name, file, filename);
                        } else {
                            // Text field - decode as UTF-8
                            const value = new TextDecoder().decode(bodyBytes);
                            formData.append(name, value);
                        }

                        pos = nextBoundary;
                    }
                } else {
                    throw new TypeError('Invalid content-type for formData()');
                }

                return formData;
            }

            _getRawBody() {
                if (!this.body || !this.body._controller) {
                    return new Uint8Array(0);
                }

                const queue = this.body._controller._queue;
                if (!queue || queue.length === 0) {
                    return new Uint8Array(0);
                }

                const chunks = [];
                for (const item of queue) {
                    if (item.type === 'chunk' && item.value) {
                        chunks.push(item.value);
                    }
                }

                if (chunks.length === 0) {
                    return new Uint8Array(0);
                }

                if (chunks.length === 1) {
                    return chunks[0];
                }

                const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
                const result = new Uint8Array(totalLength);
                let offset = 0;
                for (const chunk of chunks) {
                    result.set(chunk, offset);
                    offset += chunk.length;
                }

                return result;
            }

            clone() {
                if (this.bodyUsed) {
                    throw new TypeError('Cannot clone a Response whose body has been consumed');
                }

                let clonedBody = null;
                if (this.body) {
                    const [stream1, stream2] = this.body.tee();
                    this.body = stream1;
                    clonedBody = stream2;
                }

                return new Response(clonedBody, {
                    status: this.status,
                    statusText: this.statusText,
                    headers: new Headers(this.headers),
                    url: this.url,
                    type: this.type,
                    redirected: this.redirected
                });
            }

            static json(data, init) {
                init = init || {};
                const body = JSON.stringify(data);
                const headers = new Headers(init.headers);
                if (!headers.has('content-type')) {
                    headers.set('content-type', 'application/json');
                }
                return new Response(body, {
                    status: init.status || 200,
                    statusText: init.statusText || '',
                    headers: headers
                });
            }

            static redirect(url, status) {
                status = status || 302;
                if (![301, 302, 303, 307, 308].includes(status)) {
                    throw new RangeError('Invalid redirect status code');
                }
                const headers = new Headers({ 'Location': url });
                return new Response(null, {
                    status: status,
                    headers: headers
                });
            }

            static error() {
                const response = new Response(null, {
                    status: 0,
                    type: 'error'
                });
                return response;
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// Setup fetch input normalization helper
///
/// Provides:
/// - `__normalizeFetchInput(input, options)` - normalizes Request/URL/string to fetch options
/// - `__bufferBody(body)` - buffers ReadableStream body to Uint8Array
///
/// __normalizeFetchInput handles:
/// - Request objects (clones properties, respects options override)
/// - URL objects (converts to string via href)
/// - String URLs
/// - Normalizes Headers instances to plain objects
///
/// Returns: { url, method, headers, body }
pub fn setup_fetch_helpers(scope: &mut v8::PinScope) {
    let code = r#"
        // Buffer a ReadableStream body to Uint8Array
        globalThis.__bufferBody = async function(body) {
            if (!body || !(body instanceof ReadableStream)) {
                return body;
            }

            const reader = body.getReader();
            const chunks = [];

            while (true) {
                const { done, value } = await reader.read();
                if (done) break;
                chunks.push(value);
            }

            // Concatenate all chunks
            const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
            const result = new Uint8Array(totalLength);
            let offset = 0;

            for (const chunk of chunks) {
                result.set(chunk, offset);
                offset += chunk.length;
            }

            return result;
        };

        globalThis.__normalizeFetchInput = function(input, options) {
            options = options || {};
            let url, method, headers, body;

            if (input instanceof Request) {
                // Request object - clone properties, options can override
                url = input.url;
                method = options.method || input.method || 'GET';
                headers = options.headers !== undefined ? options.headers : input.headers;
                // Body: options.body overrides, otherwise use request body if not consumed
                if (options.body !== undefined) {
                    body = options.body;
                } else if (input.body && !input.bodyUsed) {
                    body = input.body;
                } else {
                    body = null;
                }
            } else if (input instanceof URL) {
                // URL object - convert to string
                url = input.href;
                method = options.method || 'GET';
                headers = options.headers || {};
                body = options.body !== undefined ? options.body : null;
            } else {
                // String or other - convert to string
                url = String(input);
                method = options.method || 'GET';
                headers = options.headers || {};
                body = options.body !== undefined ? options.body : null;
            }

            // Normalize Headers instance to plain object
            if (headers instanceof Headers) {
                const obj = {};
                for (const [key, value] of headers) {
                    obj[key] = value;
                }
                headers = obj;
            }

            return { url, method, headers, body };
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// Remove dangerous globals that could be used for timing attacks (Spectre)
///
/// SharedArrayBuffer and Atomics can be used to create high-precision timers
/// for side-channel attacks. Since we don't expose Web Workers (multi-threaded JS),
/// these APIs have no legitimate use case and are removed for security.
pub fn setup_security_restrictions(scope: &mut v8::PinScope) {
    let code = r#"
        // Remove SharedArrayBuffer - can be used to create high-precision timers
        delete globalThis.SharedArrayBuffer;

        // Remove Atomics - only useful with SharedArrayBuffer, potential for timing attacks
        delete globalThis.Atomics;
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
