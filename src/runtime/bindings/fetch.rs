use super::super::{CallbackId, SchedulerMessage};
use super::state::FetchState;
use crate::v8_helpers::throw_type_error;
use openworkers_core::{
    DatabaseOp, HttpMethod, HttpRequest, KvOp, RequestBody, SqlParam, StorageOp,
};
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
    sql: String,
    #[serde(default)]
    params: Vec<SqlParam>,
}

/// The request a JS options object describes, or why it describes none.
fn parse_http_request(
    scope: &mut v8::PinScope,
    options: v8::Local<v8::Value>,
) -> Result<HttpRequest, String> {
    let opts: FetchOptions = serde_v8::from_v8_any(scope, options)
        .map_err(|e| format!("fetch: the options are not a request object: {e}"))?;
    let method: HttpMethod = opts
        .method
        .parse()
        .map_err(|()| format!("fetch: unknown method \"{}\"", opts.method))?;
    let body = match opts.body {
        Some(buf) => RequestBody::Bytes(bytes::Bytes::from(buf.to_vec())),
        None => RequestBody::None,
    };

    Ok(HttpRequest {
        url: opts.url,
        method,
        headers: opts.headers,
        body,
    })
}

/// The resolve and reject callbacks an async op takes, or a TypeError thrown
/// in their place: an op that returns without them leaves the promise pending.
pub(super) fn resolve_reject<'a>(
    scope: &mut v8::PinScope,
    resolve: v8::Local<'a, v8::Value>,
    reject: v8::Local<'a, v8::Value>,
) -> Option<(v8::Local<'a, v8::Function>, v8::Local<'a, v8::Function>)> {
    let resolve = v8::Local::<v8::Function>::try_from(resolve);
    let reject = v8::Local::<v8::Function>::try_from(reject);

    match (resolve, reject) {
        (Ok(resolve), Ok(reject)) => Some((resolve, reject)),
        _ => {
            throw_type_error(scope, "the resolve and reject callbacks are not functions");
            None
        }
    }
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

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return throw_type_error(scope, "binding fetch: the binding name is not a string");
            };

            let request = match parse_http_request(scope, args.get(1)) {
                Ok(request) => request,
                Err(message) => return throw_type_error(scope, &message),
            };

            let Some((success_fn, error_fn)) = resolve_reject(scope, args.get(2), args.get(3))
            else {
                return;
            };

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

            let request = match parse_http_request(scope, args.get(0)) {
                Ok(request) => request,
                Err(message) => return throw_type_error(scope, &message),
            };

            let Some((resolve, reject)) = resolve_reject(scope, args.get(1), args.get(2)) else {
                return;
            };

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

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return throw_type_error(scope, "storage: the binding name is not a string");
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return throw_type_error(scope, "storage: the operation is not a string");
            };

            let params = match serde_v8::from_v8_any::<StorageParams>(scope, args.get(2)) {
                Ok(params) => params,
                Err(e) => return throw_type_error(scope, &format!("storage: bad parameters: {e}")),
            };

            let storage_op = match operation.as_str() {
                "get" => StorageOp::Get { key: params.key },
                "fetch" => StorageOp::Fetch { key: params.key },
                "put" => match params.body {
                    Some(body) => StorageOp::Put {
                        key: params.key,
                        body: body.to_vec(),
                    },
                    None => return throw_type_error(scope, "storage put: takes a body"),
                },
                "head" => StorageOp::Head { key: params.key },
                "list" => StorageOp::List {
                    prefix: params.prefix,
                    limit: params.limit,
                },
                "delete" => StorageOp::Delete { key: params.key },
                _ => {
                    let message = format!("storage: unknown operation \"{operation}\"");
                    return throw_type_error(scope, &message);
                }
            };

            let Some((resolve_fn, reject_fn)) = resolve_reject(scope, args.get(3), args.get(4))
            else {
                return;
            };

            let callback_id = register_callbacks_with_error(&state, scope, resolve_fn, reject_fn);

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

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return throw_type_error(scope, "kv: the binding name is not a string");
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return throw_type_error(scope, "kv: the operation is not a string");
            };

            let params = match serde_v8::from_v8_any::<KvParams>(scope, args.get(2)) {
                Ok(params) => params,
                Err(e) => return throw_type_error(scope, &format!("kv: bad parameters: {e}")),
            };

            let kv_op = match operation.as_str() {
                "get" => KvOp::Get { key: params.key },
                "put" => match params.value {
                    Some(value) => KvOp::Put {
                        key: params.key,
                        value,
                        expires_in: params.expires_in,
                    },
                    None => return throw_type_error(scope, "kv put: takes a value"),
                },
                "delete" => KvOp::Delete { key: params.key },
                "list" => KvOp::List {
                    prefix: params.prefix,
                    limit: params.limit,
                },
                _ => {
                    let message = format!("kv: unknown operation \"{operation}\"");
                    return throw_type_error(scope, &message);
                }
            };

            let Some((resolve_fn, reject_fn)) = resolve_reject(scope, args.get(3), args.get(4))
            else {
                return;
            };

            let callback_id = register_callbacks_with_error(&state, scope, resolve_fn, reject_fn);

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
    // Args: (binding_name, operation, params, resolve, reject)
    let native_binding_database_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            let Ok(binding_name) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return throw_type_error(scope, "database: the binding name is not a string");
            };

            let Ok(operation) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return throw_type_error(scope, "database: the operation is not a string");
            };

            let params = match serde_v8::from_v8_any::<DatabaseParams>(scope, args.get(2)) {
                Ok(params) => params,
                Err(e) => {
                    return throw_type_error(scope, &format!("database: bad parameters: {e}"));
                }
            };

            let database_op = match operation.as_str() {
                "query" => DatabaseOp::Query {
                    sql: params.sql,
                    params: params.params,
                },
                _ => {
                    let message = format!("database: unknown operation \"{operation}\"");
                    return throw_type_error(scope, &message);
                }
            };

            let Some((resolve_fn, reject_fn)) = resolve_reject(scope, args.get(3), args.get(4))
            else {
                return;
            };

            let callback_id = register_callbacks_with_error(&state, scope, resolve_fn, reject_fn);

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

        globalThis.fetch = async function(input, options) {
            // Normalize input (handles Request, URL, string)
            const normalized = __normalizeFetchInput(input, options);
            let { url, method, headers, body } = normalized;
            let contentType = null;

            // Handle ReadableStream body - buffer it first (returns Uint8Array)
            if (body instanceof ReadableStream) {
                console.warn('[fetch] ReadableStream body detected - buffering entire stream before sending');
                body = await __bufferBody(body);
            }

            // Handle FormData body - serialize to multipart/form-data
            if (body instanceof FormData) {
                const serialized = await __serializeFormData(body);
                body = serialized.body;
                contentType = 'multipart/form-data; boundary=' + serialized.boundary;
            }

            // Handle URLSearchParams body - serialize to form-encoded string
            if (body instanceof URLSearchParams) {
                body = new TextEncoder().encode(body.toString());
                if (!contentType) contentType = 'application/x-www-form-urlencoded;charset=UTF-8';
            }

            // Convert string/primitive body to Uint8Array for native consumption
            if (body !== null && typeof body !== 'object') {
                body = new TextEncoder().encode(String(body));
            }

            // Set Content-Type if not already set (FormData or URLSearchParams)
            if (contentType && !headers['Content-Type'] && !headers['content-type']) {
                headers['Content-Type'] = contentType;
            }

            // Check for WebSocket upgrade
            const upgradeKey = Object.keys(headers).find(
                function(k) { return k.toLowerCase() === 'upgrade'; }
            );

            if (upgradeKey && headers[upgradeKey].toLowerCase() === 'websocket') {
                return new Promise(function(resolve, reject) {
                    __nativeWebSocketConnect(url, headers, function(wsId) {
                        const ws = WebSocket.__adopt(wsId);
                        resolve(new Response(null, { status: 101, webSocket: ws }));
                    }, reject);
                });
            }

            return new Promise((resolve, reject) => {
                const fetchOptions = { url, method, headers, body };

                // Use streaming fetch
                __nativeFetchStreaming(fetchOptions, (meta) => {
                    // meta = {status, statusText, headers, streamId}
                    resolve(__responseFromMeta(meta));
                }, reject);
            });
        };
    "#;

    exec_js!(scope, code);
}
