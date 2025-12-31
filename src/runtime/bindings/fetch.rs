use super::super::{CallbackId, SchedulerMessage};
use super::state::FetchState;
use openworkers_core::{DatabaseOp, HttpMethod, HttpRequest, KvOp, RequestBody, StorageOp};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use v8;

/// Get FetchState from global scope
fn get_fetch_state<'a>(scope: &mut v8::PinScope) -> Option<&'a FetchState> {
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__fetchState").unwrap();
    let state_val = global.get(scope, state_key.into())?;

    if !state_val.is_external() {
        return None;
    }

    let external: v8::Local<v8::External> = state_val.try_into().ok()?;
    let state_ptr = external.value() as *mut FetchState;
    Some(unsafe { &*state_ptr })
}

/// Parse JS options object into HttpRequest
fn parse_http_request(
    scope: &mut v8::PinScope,
    options: v8::Local<v8::Object>,
) -> Option<HttpRequest> {
    // Get URL
    let url_key = v8::String::new(scope, "url").unwrap();
    let url = options
        .get(scope, url_key.into())
        .and_then(|v| v.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))?;

    // Get method
    let method_key = v8::String::new(scope, "method").unwrap();
    let method_str = options
        .get(scope, method_key.into())
        .and_then(|v| v.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "GET".to_string());
    let method = method_str.parse().unwrap_or(HttpMethod::Get);

    // Get headers
    let mut headers = HashMap::new();
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
                    headers.insert(key, val_str.to_rust_string_lossy(scope));
                }
            }
        }
    }

    // Get body
    let body_key = v8::String::new(scope, "body").unwrap();
    let body = if let Some(body_val) = options.get(scope, body_key.into())
        && !body_val.is_null()
        && !body_val.is_undefined()
    {
        if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(body_val) {
            let len = uint8_array.byte_length();
            let mut buffer = vec![0u8; len];
            uint8_array.copy_contents(&mut buffer);
            RequestBody::Bytes(bytes::Bytes::from(buffer))
        } else if let Some(body_str) = body_val.to_string(scope) {
            RequestBody::Bytes(bytes::Bytes::from(body_str.to_rust_string_lossy(scope)))
        } else {
            RequestBody::None
        }
    } else {
        RequestBody::None
    };

    Some(HttpRequest {
        url,
        method,
        headers,
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
        let mut id = state.next_id.lock().unwrap();
        let current = *id;
        *id += 1;
        current
    };

    state
        .callbacks
        .lock()
        .unwrap()
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
        .lock()
        .unwrap()
        .insert(callback_id, v8::Global::new(scope, error_fn));

    callback_id
}

/// Get string parameter from JS object
fn get_string_param(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Option<String> {
    let key_v8 = v8::String::new(scope, key).unwrap();
    obj.get(scope, key_v8.into())
        .and_then(|v| v.to_string(scope))
        .map(|s| s.to_rust_string_lossy(scope))
}

/// Get optional string parameter (returns None if null/undefined)
fn get_optional_string_param(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Option<String> {
    let key_v8 = v8::String::new(scope, key).unwrap();
    obj.get(scope, key_v8.into()).and_then(|v| {
        if v.is_null() || v.is_undefined() {
            None
        } else {
            v.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
        }
    })
}

/// Get optional u32 parameter
fn get_optional_u32_param(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Option<u32> {
    let key_v8 = v8::String::new(scope, key).unwrap();
    obj.get(scope, key_v8.into())
        .filter(|v| !v.is_undefined() && !v.is_null())
        .and_then(|v| v.uint32_value(scope))
}

/// Get optional u64 parameter
fn get_optional_u64_param(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Option<u64> {
    get_optional_u32_param(scope, obj, key).map(|v| v as u64)
}

/// Get bytes parameter (from Uint8Array or string)
fn get_bytes_param(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> Vec<u8> {
    let key_v8 = v8::String::new(scope, key).unwrap();

    if let Some(val) = obj.get(scope, key_v8.into()) {
        if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(val) {
            let len = uint8_array.byte_length();
            let mut buffer = vec![0u8; len];
            uint8_array.copy_contents(&mut buffer);
            return buffer;
        } else if let Some(s) = val.to_string(scope) {
            return s.to_rust_string_lossy(scope).into_bytes();
        }
    }

    vec![]
}

/// Get string array parameter
fn get_string_array_param(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Vec<String> {
    let key_v8 = v8::String::new(scope, key).unwrap();

    if let Some(val) = obj.get(scope, key_v8.into())
        && let Ok(array) = v8::Local::<v8::Array>::try_from(val)
    {
        let mut result = Vec::new();

        for i in 0..array.length() {
            if let Some(item) = array.get_index(scope, i)
                && let Some(item_str) = item.to_string(scope)
            {
                result.push(item_str.to_rust_string_lossy(scope));
            }
        }

        return result;
    }

    vec![]
}

pub fn setup_fetch(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    error_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    next_id: Arc<Mutex<CallbackId>>,
) {
    let state = FetchState {
        scheduler_tx,
        callbacks,
        error_callbacks,
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
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 3 {
                return;
            }

            let Some(options) = args.get(0).to_object(scope) else {
                return;
            };
            let Some(request) = parse_http_request(scope, options) else {
                return;
            };

            let resolve_val = args.get(1);
            if !resolve_val.is_function() {
                return;
            }
            let resolve: v8::Local<v8::Function> = resolve_val.try_into().unwrap();

            let callback_id = register_callback(state, scope, resolve);

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

    // Create __nativeBindingFetch for binding-based fetch (assets, storage)
    let native_binding_fetch_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 4 {
                return;
            }

            let Some(binding_name) = args
                .get(0)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(options) = args.get(1).to_object(scope) else {
                return;
            };
            let Some(request) = parse_http_request(scope, options) else {
                return;
            };

            let (success_cb, error_cb) = (args.get(2), args.get(3));
            if !success_cb.is_function() || !error_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let error_fn: v8::Local<v8::Function> = error_cb.try_into().unwrap();

            let callback_id = register_callbacks_with_error(state, scope, success_fn, error_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingFetch(
                callback_id,
                binding_name,
                request,
            ));
        },
    )
    .unwrap();

    let native_binding_fetch_key = v8::String::new(scope, "__nativeBindingFetch").unwrap();
    global.set(
        scope,
        native_binding_fetch_key.into(),
        native_binding_fetch_fn.into(),
    );

    // Create __nativeBindingStorage for storage operations (get/put/head/list/delete)
    let native_binding_storage_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 4 {
                return;
            }

            let Some(binding_name) = args
                .get(0)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(operation) = args
                .get(1)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(params) = args.get(2).to_object(scope) else {
                return;
            };

            let storage_op = match operation.as_str() {
                "get" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    StorageOp::Get { key }
                }
                "put" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    let body = get_bytes_param(scope, params, "body");
                    StorageOp::Put { key, body }
                }
                "head" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    StorageOp::Head { key }
                }
                "list" => {
                    let prefix = get_optional_string_param(scope, params, "prefix");
                    let limit = get_optional_u32_param(scope, params, "limit");
                    StorageOp::List { prefix, limit }
                }
                "delete" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    StorageOp::Delete { key }
                }
                _ => return,
            };

            let success_cb = args.get(3);
            if !success_cb.is_function() {
                return;
            }
            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = register_callback(state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingStorage(
                callback_id,
                binding_name,
                storage_op,
            ));
        },
    )
    .unwrap();

    let native_binding_storage_key = v8::String::new(scope, "__nativeBindingStorage").unwrap();
    global.set(
        scope,
        native_binding_storage_key.into(),
        native_binding_storage_fn.into(),
    );

    // Create __nativeBindingKv for KV operations (get/put/delete)
    let native_binding_kv_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 4 {
                return;
            }

            let Some(binding_name) = args
                .get(0)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(operation) = args
                .get(1)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(params) = args.get(2).to_object(scope) else {
                return;
            };

            let kv_op = match operation.as_str() {
                "get" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    KvOp::Get { key }
                }
                "put" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();

                    // Get value as JSON - convert any JS value to serde_json::Value
                    let value_key = v8::String::new(scope, "value").unwrap();
                    let value_v8 = params
                        .get(scope, value_key.into())
                        .unwrap_or_else(|| v8::undefined(scope).into());

                    let value: serde_json::Value =
                        if let Some(json_str) = v8::json::stringify(scope, value_v8) {
                            let rust_str = json_str.to_rust_string_lossy(scope);
                            serde_json::from_str(&rust_str).unwrap_or(serde_json::Value::Null)
                        } else {
                            serde_json::Value::Null
                        };

                    let expires_in = get_optional_u64_param(scope, params, "expiresIn");
                    KvOp::Put {
                        key,
                        value,
                        expires_in,
                    }
                }
                "delete" => {
                    let key = get_string_param(scope, params, "key").unwrap_or_default();
                    KvOp::Delete { key }
                }
                "list" => {
                    let prefix = get_optional_string_param(scope, params, "prefix");
                    let limit = get_optional_u32_param(scope, params, "limit");
                    KvOp::List { prefix, limit }
                }
                _ => return,
            };

            let success_cb = args.get(3);
            if !success_cb.is_function() {
                return;
            }
            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = register_callback(state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingKv(
                callback_id,
                binding_name,
                kv_op,
            ));
        },
    )
    .unwrap();

    let native_binding_kv_key = v8::String::new(scope, "__nativeBindingKv").unwrap();
    global.set(
        scope,
        native_binding_kv_key.into(),
        native_binding_kv_fn.into(),
    );

    // Create __nativeBindingDatabase for database operations (query)
    let native_binding_database_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 4 {
                return;
            }

            let Some(binding_name) = args
                .get(0)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(operation) = args
                .get(1)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(params) = args.get(2).to_object(scope) else {
                return;
            };

            let database_op = match operation.as_str() {
                "query" => {
                    let sql = get_string_param(scope, params, "sql").unwrap_or_default();
                    let query_params = get_string_array_param(scope, params, "params");
                    DatabaseOp::Query {
                        sql,
                        params: query_params,
                    }
                }
                _ => return,
            };

            let success_cb = args.get(3);
            if !success_cb.is_function() {
                return;
            }
            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = register_callback(state, scope, success_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingDatabase(
                callback_id,
                binding_name,
                database_op,
            ));
        },
    )
    .unwrap();

    let native_binding_database_key = v8::String::new(scope, "__nativeBindingDatabase").unwrap();
    global.set(
        scope,
        native_binding_database_key.into(),
        native_binding_database_fn.into(),
    );

    // Create __nativeBindingWorker for worker-to-worker calls
    let native_binding_worker_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let Some(state) = get_fetch_state(scope) else {
                return;
            };
            if args.length() < 4 {
                return;
            }

            let Some(binding_name) = args
                .get(0)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
            else {
                return;
            };
            let Some(options) = args.get(1).to_object(scope) else {
                return;
            };
            let Some(request) = parse_http_request(scope, options) else {
                return;
            };

            let (success_cb, error_cb) = (args.get(2), args.get(3));
            if !success_cb.is_function() || !error_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let error_fn: v8::Local<v8::Function> = error_cb.try_into().unwrap();

            let callback_id = register_callbacks_with_error(state, scope, success_fn, error_fn);

            let _ = state.scheduler_tx.send(SchedulerMessage::BindingWorker(
                callback_id,
                binding_name,
                request,
            ));
        },
    )
    .unwrap();

    let native_binding_worker_key = v8::String::new(scope, "__nativeBindingWorker").unwrap();
    global.set(
        scope,
        native_binding_worker_key.into(),
        native_binding_worker_fn.into(),
    );

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

        globalThis.fetch = async function(url, options) {
            options = options || {};
            let body = options.body || null;

            // If body is a ReadableStream, buffer it first
            if (body instanceof ReadableStream) {
                console.warn('[fetch] ReadableStream body detected - buffering entire stream before sending');
                body = await __bufferReadableStream(body);
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

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
