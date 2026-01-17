use super::super::{CallbackId, SchedulerMessage};
use super::state::FetchState;
use openworkers_core::{DatabaseOp, HttpMethod, HttpRequest, KvOp, RequestBody, StorageOp};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc;
use v8;

/// Request options from JavaScript fetch()
#[derive(Deserialize)]
struct FetchOptions {
    url: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body: Option<serde_v8::JsBuffer>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// Storage operation parameters
#[derive(Deserialize)]
struct StorageParams {
    #[serde(default)]
    key: String,
    #[serde(default)]
    body: Option<serde_v8::JsBuffer>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// KV operation parameters
#[derive(Deserialize)]
struct KvParams {
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: Option<serde_json::Value>,
    #[serde(default, rename = "expiresIn")]
    expires_in: Option<u64>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// Database query parameters
#[derive(Deserialize)]
struct DatabaseParams {
    #[serde(default)]
    sql: String,
    #[serde(default)]
    params: Vec<String>,
}

/// Parse JS options object into HttpRequest using serde_v8
fn parse_http_request(
    scope: &mut v8::PinScope,
    options: v8::Local<v8::Value>,
) -> Option<HttpRequest> {
    let opts: FetchOptions = serde_v8::from_v8_any(scope, options).ok()?;
    let method = opts.method.parse().unwrap_or(HttpMethod::Get);
    let body = match opts.body {
        Some(buf) => RequestBody::Bytes(bytes::Bytes::from(buf.to_vec())),
        None => RequestBody::None,
    };

    Some(HttpRequest {
        url: opts.url,
        method,
        headers: opts.headers,
        body,
    })
}

/// Register success callback and return callback ID
fn register_callback(
    state: &FetchState,
    scope: &mut v8::PinScope,
    success_fn: v8::Local<v8::Function>,
) -> CallbackId {
    let callback_id = {
        let mut id = state.next_id.borrow_mut();
        let current = *id;
        *id += 1;
        current
    };

    state
        .callbacks
        .borrow_mut()
        .insert(callback_id, v8::Global::new(scope, success_fn));

    callback_id
}

/// Register success and error callbacks and return callback ID
fn register_callbacks_with_error(
    state: &FetchState,
    scope: &mut v8::PinScope,
    success_fn: v8::Local<v8::Function>,
    error_fn: v8::Local<v8::Function>,
) -> CallbackId {
    let callback_id = register_callback(state, scope, success_fn);

    state
        .error_callbacks
        .borrow_mut()
        .insert(callback_id, v8::Global::new(scope, error_fn));

    callback_id
}

/// Set up a native binding function for HTTP-based bindings (fetch, worker)
///
/// Both __nativeBindingFetch and __nativeBindingWorker have identical logic,
/// differing only in the SchedulerMessage variant they send.
fn setup_binding_fetch_helper<F>(scope: &mut v8::PinScope, key: &str, message_fn: F)
where
    F: Fn(CallbackId, String, HttpRequest) -> SchedulerMessage + Copy + 'static,
{
    let func = v8::Function::new(
        scope,
        move |scope: &mut v8::PinScope,
              args: v8::FunctionCallbackArguments,
              mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            if args.length() < 4 {
                return;
            }

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return;
            };

            let Some(request) = parse_http_request(scope, args.get(1)) else {
                return;
            };

            let (success_cb, error_cb) = (args.get(2), args.get(3));

            if !success_cb.is_function() || !error_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let error_fn: v8::Local<v8::Function> = error_cb.try_into().unwrap();

            let callback_id = register_callbacks_with_error(&state, scope, success_fn, error_fn);

            let _ = state
                .scheduler_tx
                .send(message_fn(callback_id, binding_name, request));
        },
    )
    .unwrap();

    let global = scope.get_current_context().global(scope);
    let key_v8 = v8::String::new(scope, key).unwrap();
    global.set(scope, key_v8.into(), func.into());
}

pub fn setup_fetch(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    error_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    next_id: Rc<RefCell<CallbackId>>,
) {
    let state = FetchState {
        scheduler_tx,
        callbacks,
        error_callbacks,
        next_id,
    };

    store_state!(scope, state);

    // Create __nativeFetchStreaming for streaming fetch
    let native_fetch_streaming_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            if args.length() < 3 {
                return;
            }

            let Some(request) = parse_http_request(scope, args.get(0)) else {
                return;
            };

            let resolve_val = args.get(1);
            if !resolve_val.is_function() {
                return;
            }
            let resolve: v8::Local<v8::Function> = resolve_val.try_into().unwrap();

            let reject_val = args.get(2);
            if !reject_val.is_function() {
                return;
            }
            let reject: v8::Local<v8::Function> = reject_val.try_into().unwrap();

            let callback_id = register_callbacks_with_error(&state, scope, resolve, reject);

            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::FetchStreaming(callback_id, request));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeFetchStreaming", native_fetch_streaming_fn);

    // Create __nativeBindingFetch for binding-based fetch (assets, storage)
    setup_binding_fetch_helper(scope, "__nativeBindingFetch", |id, name, req| {
        SchedulerMessage::BindingFetch(id, name, req)
    });

    // Create __nativeBindingStorage for storage operations (get/put/head/list/delete)
    let native_binding_storage_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            if args.length() < 4 {
                return;
            }

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return;
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return;
            };

            let Ok(params) = serde_v8::from_v8_any::<StorageParams>(scope, args.get(2)) else {
                return;
            };

            let storage_op = match operation.as_str() {
                "get" => StorageOp::Get { key: params.key },
                "put" => StorageOp::Put {
                    key: params.key,
                    body: params.body.map(|b| b.to_vec()).unwrap_or_default(),
                },
                "head" => StorageOp::Head { key: params.key },
                "list" => StorageOp::List {
                    prefix: params.prefix,
                    limit: params.limit,
                },
                "delete" => StorageOp::Delete { key: params.key },
                _ => return,
            };

            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let callback_id = register_callback(&state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingStorage(
                callback_id,
                binding_name,
                storage_op,
            ));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeBindingStorage", native_binding_storage_fn);

    // Create __nativeBindingKv for KV operations (get/put/delete)
    let native_binding_kv_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            if args.length() < 4 {
                return;
            }

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return;
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return;
            };

            let Ok(params) = serde_v8::from_v8_any::<KvParams>(scope, args.get(2)) else {
                return;
            };

            let kv_op = match operation.as_str() {
                "get" => KvOp::Get { key: params.key },
                "put" => KvOp::Put {
                    key: params.key,
                    value: params.value.unwrap_or(serde_json::Value::Null),
                    expires_in: params.expires_in,
                },
                "delete" => KvOp::Delete { key: params.key },
                "list" => KvOp::List {
                    prefix: params.prefix,
                    limit: params.limit,
                },
                _ => return,
            };

            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let callback_id = register_callback(&state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingKv(
                callback_id,
                binding_name,
                kv_op,
            ));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeBindingKv", native_binding_kv_fn);

    // Create __nativeBindingDatabase for database operations (query)
    let native_binding_database_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            if args.length() < 4 {
                return;
            }

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return;
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return;
            };

            let Ok(params) = serde_v8::from_v8_any::<DatabaseParams>(scope, args.get(2)) else {
                return;
            };

            let database_op = match operation.as_str() {
                "query" => DatabaseOp::Query {
                    sql: params.sql,
                    params: params.params,
                },
                _ => return,
            };

            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let callback_id = register_callback(&state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingDatabase(
                callback_id,
                binding_name,
                database_op,
            ));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeBindingDatabase", native_binding_database_fn);

    // Create __nativeBindingWorker for worker-to-worker calls
    setup_binding_fetch_helper(scope, "__nativeBindingWorker", |id, name, req| {
        SchedulerMessage::BindingWorker(id, name, req)
    });

    // JavaScript fetch implementation using Promises with streaming support
    let code = r#"
        // Helper to read entire ReadableStream into a string
        async function __bufferReadableStream(stream) {
            const reader = stream.getReader();
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
            return new TextDecoder().decode(result);
        }

        // Serialize FormData to multipart/form-data
        async function __serializeFormData(formData) {
            const boundary = '----OpenWorkersBoundary' + Math.random().toString(36).slice(2);
            const CRLF = '\r\n';
            const parts = [];

            for (const [name, value, filename] of formData._entries) {
                let part = '--' + boundary + CRLF;

                if (value instanceof Blob) {
                    const fname = filename || (value.name ? value.name : 'blob');
                    part += 'Content-Disposition: form-data; name="' + name + '"; filename="' + fname + '"' + CRLF;
                    part += 'Content-Type: ' + (value.type || 'application/octet-stream') + CRLF + CRLF;

                    const headerBytes = new TextEncoder().encode(part);
                    const blobBytes = value._getBytes();
                    const crlfBytes = new TextEncoder().encode(CRLF);

                    parts.push(headerBytes);
                    parts.push(blobBytes);
                    parts.push(crlfBytes);
                } else {
                    part += 'Content-Disposition: form-data; name="' + name + '"' + CRLF + CRLF;
                    part += String(value) + CRLF;
                    parts.push(new TextEncoder().encode(part));
                }
            }

            parts.push(new TextEncoder().encode('--' + boundary + '--' + CRLF));

            // Concatenate all parts
            const totalLength = parts.reduce((sum, p) => sum + p.length, 0);
            const result = new Uint8Array(totalLength);
            let offset = 0;

            for (const part of parts) {
                result.set(part, offset);
                offset += part.length;
            }

            return { body: result, boundary: boundary };
        }

        globalThis.fetch = async function(url, options) {
            options = options || {};
            let body = options.body || null;
            let contentType = null;

            // If body is a ReadableStream, buffer it first
            if (body instanceof ReadableStream) {
                console.warn('[fetch] ReadableStream body detected - buffering entire stream before sending');
                body = await __bufferReadableStream(body);
            }

            // If body is FormData, serialize to multipart/form-data
            if (body instanceof FormData) {
                const serialized = await __serializeFormData(body);
                body = serialized.body;
                contentType = 'multipart/form-data; boundary=' + serialized.boundary;
            }

            return new Promise((resolve, reject) => {
                // Convert Headers instance to plain object
                let headersObj = {};
                const h = options.headers;

                if (h instanceof Headers) {
                    for (const [key, value] of h) {
                        headersObj[key] = value;
                    }
                } else if (h && typeof h === 'object') {
                    headersObj = h;
                }

                // Set Content-Type for FormData if not already set
                if (contentType && !headersObj['Content-Type'] && !headersObj['content-type']) {
                    headersObj['Content-Type'] = contentType;
                }

                const fetchOptions = {
                    url: url,
                    method: options.method || 'GET',
                    headers: headersObj,
                    body: body
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

    exec_js!(scope, code);
}
