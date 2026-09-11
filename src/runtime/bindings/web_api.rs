use super::state::PerformanceState;
use serde::Serialize;
use std::rc::Rc;
use std::time::Instant;
use url::Url;
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

    // Keeps the Rc alive for the context; the templates below only borrow it.
    crate::context_slots::set(scope, state.clone());

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

/// The second tier of streams, which stands on the host's `ReadableStream`.
pub fn setup_streams(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::STREAMS.source;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// The event core `AbortSignal` and `MessagePort` are built on.
pub fn setup_events(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::EVENTS.source;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_abort_controller(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::ABORT.source;

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

        // Lookup table for decoding: charCode → 6-bit value (supports base64url too)
        const B64_CODES = new Uint8Array(256);
        for (let i = 0; i < BASE64_CHARS.length; i++) {
            B64_CODES[BASE64_CHARS.charCodeAt(i)] = i;
        }
        B64_CODES[0x2d] = 62; // '-' (base64url)
        B64_CODES[0x5f] = 63; // '_' (base64url)

        // btoa: binary string → base64
        // Each char is treated as a single byte (charCodeAt), NOT UTF-8.
        globalThis.btoa = function(str) {
            const len = str.length;
            let result = '';

            for (let i = 0; i < len; i += 3) {
                const b1 = str.charCodeAt(i);
                const b2 = i + 1 < len ? str.charCodeAt(i + 1) : 0;
                const b3 = i + 2 < len ? str.charCodeAt(i + 2) : 0;

                if (b1 > 255 || b2 > 255 || b3 > 255) {
                    throw new DOMException('Invalid character', 'InvalidCharacterError');
                }

                result +=
                    BASE64_CHARS[b1 >> 2] +
                    BASE64_CHARS[((b1 & 3) << 4) | (b2 >> 4)] +
                    BASE64_CHARS[((b2 & 15) << 2) | (b3 >> 6)] +
                    BASE64_CHARS[b3 & 63];
            }

            if (len % 3 === 2) {
                result = result.substring(0, result.length - 1) + '=';
            } else if (len % 3 === 1) {
                result = result.substring(0, result.length - 2) + '==';
            }

            return result;
        };

        // atob: base64 → binary string
        // Returns a string where each char is one byte (latin-1), NOT UTF-8.
        globalThis.atob = function(base64) {
            base64 = base64.replace(/[\s=]/g, '');
            const len = base64.length;
            const outLen = (len * 3) >>> 2;
            let result = '';

            for (let i = 0, j = 0; j < outLen; i += 4) {
                const a = B64_CODES[base64.charCodeAt(i)];
                const b = B64_CODES[base64.charCodeAt(i + 1)];
                const c = B64_CODES[base64.charCodeAt(i + 2)];
                const d = B64_CODES[base64.charCodeAt(i + 3)];

                result += String.fromCharCode((a << 2) | (b >> 4));
                if (++j < outLen) result += String.fromCharCode(((b & 15) << 4) | (c >> 2));
                if (++j < outLen) result += String.fromCharCode(((c & 3) << 6) | (d & 63));
                j++;
            }

            return result;
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// The WHATWG components the JS `URL` class reads back from the parser
#[derive(Serialize)]
struct UrlParts {
    href: String,
    origin: String,
    protocol: String,
    username: String,
    password: String,
    host: String,
    hostname: String,
    port: String,
    pathname: String,
    search: String,
    hash: String,
}

impl UrlParts {
    fn of(url: &Url) -> Self {
        let port = url.port().map(|p| p.to_string()).unwrap_or_default();
        let hostname = url.host_str().unwrap_or("").to_string();

        let host = if port.is_empty() {
            hostname.clone()
        } else {
            format!("{}:{}", hostname, port)
        };

        Self {
            href: url.as_str().to_string(),
            origin: url.origin().ascii_serialization(),
            protocol: format!("{}:", url.scheme()),
            username: url.username().to_string(),
            password: url.password().unwrap_or("").to_string(),
            host,
            hostname,
            port,
            pathname: url.path().to_string(),
            search: match url.query() {
                Some(query) if !query.is_empty() => format!("?{}", query),
                _ => String::new(),
            },
            hash: match url.fragment() {
                Some(fragment) if !fragment.is_empty() => format!("#{}", fragment),
                _ => String::new(),
            },
        }
    }
}

fn parse_url(input: &str, base: Option<&str>) -> Option<Url> {
    match base {
        Some(base) => Url::parse(base).ok()?.join(input).ok(),
        None => Url::parse(input).ok(),
    }
}

/// WHATWG setters ignore a value they cannot apply, so every failure is silent
fn apply_url_part(url: &mut Url, part: &str, value: &str) {
    match part {
        "protocol" => {
            let _ = url.set_scheme(value.split(':').next().unwrap_or(""));
        }
        "username" => {
            let _ = url.set_username(value);
        }
        "password" => {
            let _ = url.set_password((!value.is_empty()).then_some(value));
        }
        "host" => {
            // An IPv6 literal is bracketed and carries colons of its own
            let host_end = value.rfind(']').map(|end| end + 1).unwrap_or(0);

            let (hostname, port) = match value[host_end..].find(':') {
                Some(colon) => (
                    &value[..host_end + colon],
                    Some(&value[host_end + colon + 1..]),
                ),
                None => (value, None),
            };

            if url.set_host(Some(hostname)).is_ok()
                && let Some(port) = port
                && let Ok(port) = port.parse::<u16>()
            {
                let _ = url.set_port(Some(port));
            }
        }
        "hostname" => {
            let _ = url.set_host(Some(value));
        }
        "port" => {
            if value.is_empty() {
                let _ = url.set_port(None);
            } else if let Ok(port) = value.parse::<u16>() {
                let _ = url.set_port(Some(port));
            }
        }
        "pathname" => {
            if !url.cannot_be_a_base() {
                url.set_path(value);
            }
        }
        "search" => {
            let query = value.strip_prefix('?').unwrap_or(value);
            url.set_query((!query.is_empty()).then_some(query));
        }
        "hash" => {
            let fragment = value.strip_prefix('#').unwrap_or(value);
            url.set_fragment((!fragment.is_empty()).then_some(fragment));
        }
        _ => {}
    }
}

/// Returns the components, or null when the input is not a valid URL
#[glue_v8::method]
fn url_parse(input: String, base: Option<String>) -> Option<UrlParts> {
    parse_url(&input, base.as_deref()).map(|url| UrlParts::of(&url))
}

#[glue_v8::method]
fn url_update(href: String, part: String, value: String) -> Option<UrlParts> {
    let mut url = parse_url(&href, None)?;
    apply_url_part(&mut url, &part, &value);

    Some(UrlParts::of(&url))
}

/// Register the parser the `URL` class calls; it cannot live in the snapshot
pub fn setup_url_natives(scope: &mut v8::PinScope) {
    let parse_fn = v8::Function::new(scope, url_parse_v8).unwrap();
    super::native::register_op(scope, "urlParse", parse_fn);

    let update_fn = v8::Function::new(scope, url_update_v8).unwrap();
    super::native::register_op(scope, "urlUpdate", update_fn);
}

/// Define `URL` and `URLSearchParams`, both driven by the host's url ops
pub fn setup_url(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::URL.source;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_headers(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::HEADERS.source;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_request(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::REQUEST.source;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_response(scope: &mut v8::PinScope) {
    let code = openworkers_wintertc::RESPONSE.source;

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

        // Generic helper for binding calls with Promise wrapper
        // nativeFn: the native binding function to call (bindingName, method, params, resolve, reject)
        // Returns: Promise that resolves with data or rejects with error
        globalThis.__bindingCall = function(nativeFn, bindingName, method, params) {
            return new Promise((resolve, reject) => {
                nativeFn(bindingName, method, params, resolve, reject);
            });
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
