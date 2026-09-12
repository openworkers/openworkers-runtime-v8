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

/// What `navigator.userAgent` reports. `Product/Version (comment)` is the HTTP
/// grammar, so a reader splitting on the slash still finds the version.
pub fn setup_navigator_natives(scope: &mut v8::PinScope) {
    let agent = format!("OpenWorkers/{} (v8)", env!("CARGO_PKG_VERSION"));
    let agent = v8::String::new(scope, &agent).unwrap();

    super::native::register_op(scope, "userAgent", agent.into());
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
    super::native::register_op(scope, "performanceNow", now_fn.into());

    // The wall clock when this worker started, which is what `now` counts from.
    let origin = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let origin = v8::Number::new(scope, origin);
    super::native::register_op(scope, "timeOrigin", origin.into());
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
    super::native::register_op(scope, "urlParse", parse_fn.into());

    let update_fn = v8::Function::new(scope, url_update_v8).unwrap();
    super::native::register_op(scope, "urlUpdate", update_fn.into());
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
