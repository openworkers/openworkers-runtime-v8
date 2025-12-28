use super::super::{CallbackId, SchedulerMessage};
use super::state::FetchState;
use openworkers_core::{DatabaseOp, HttpMethod, HttpRequest, KvOp, RequestBody, StorageOp};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use v8;

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
                .map(|s| RequestBody::Bytes(bytes::Bytes::from(s.to_rust_string_lossy(scope))))
                .unwrap_or(RequestBody::None);

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

    // Create __nativeBindingFetch for binding-based fetch (assets, storage)
    let native_binding_fetch_fn = v8::Function::new(
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

            // Args: (bindingName, options, successCallback, errorCallback)
            if args.length() < 4 {
                return;
            }

            // Get binding name (arg 0)
            let binding_name = match args.get(0).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Parse fetch options (arg 1)
            let options = match args.get(1).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Get URL/path
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
            let body = if let Some(body_val) = options.get(scope, body_key.into())
                && !body_val.is_null()
                && !body_val.is_undefined()
            {
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(body_val) {
                    let len = uint8_array.byte_length();
                    let mut buffer = vec![0u8; len];
                    uint8_array.copy_contents(&mut buffer);
                    openworkers_core::RequestBody::Bytes(bytes::Bytes::from(buffer))
                } else if let Some(body_str) = body_val.to_string(scope) {
                    openworkers_core::RequestBody::Bytes(bytes::Bytes::from(
                        body_str.to_rust_string_lossy(scope),
                    ))
                } else {
                    openworkers_core::RequestBody::None
                }
            } else {
                openworkers_core::RequestBody::None
            };

            let request = openworkers_core::HttpRequest {
                url,
                method,
                headers,
                body,
            };

            // Store callbacks
            let success_cb = args.get(2);
            let error_cb = args.get(3);

            if !success_cb.is_function() || !error_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();
            let error_fn: v8::Local<v8::Function> = error_cb.try_into().unwrap();

            let callback_id = {
                let mut id = state.next_id.lock().unwrap();
                let current = *id;
                *id += 1;
                current
            };

            {
                let mut cbs = state.callbacks.lock().unwrap();
                cbs.insert(callback_id, v8::Global::new(scope, success_fn));
            }

            {
                let mut error_cbs = state.error_callbacks.lock().unwrap();
                error_cbs.insert(callback_id, v8::Global::new(scope, error_fn));
            }

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
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__fetchState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut FetchState;
            let state = unsafe { &*state_ptr };

            // Args: (bindingName, operation, params, successCallback)
            if args.length() < 4 {
                return;
            }

            // Get binding name (arg 0)
            let binding_name = match args.get(0).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get operation name (arg 1)
            let operation = match args.get(1).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get params object (arg 2)
            let params = match args.get(2).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Build StorageOp from operation name and params
            let storage_op = match operation.as_str() {
                "get" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    StorageOp::Get { key }
                }
                "put" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();

                    let body_key = v8::String::new(scope, "body").unwrap();
                    let body = if let Some(body_val) = params.get(scope, body_key.into()) {
                        if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(body_val) {
                            let len = uint8_array.byte_length();
                            let mut buffer = vec![0u8; len];
                            uint8_array.copy_contents(&mut buffer);
                            buffer
                        } else if let Some(body_str) = body_val.to_string(scope) {
                            body_str.to_rust_string_lossy(scope).into_bytes()
                        } else {
                            vec![]
                        }
                    } else {
                        vec![]
                    };

                    StorageOp::Put { key, body }
                }
                "head" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    StorageOp::Head { key }
                }
                "list" => {
                    let prefix_key = v8::String::new(scope, "prefix").unwrap();
                    let prefix = params.get(scope, prefix_key.into()).and_then(|v| {
                        if v.is_null() || v.is_undefined() {
                            None
                        } else {
                            v.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
                        }
                    });

                    let limit_key = v8::String::new(scope, "limit").unwrap();
                    let limit = params
                        .get(scope, limit_key.into())
                        .filter(|v| !v.is_undefined() && !v.is_null())
                        .and_then(|v| v.uint32_value(scope));

                    StorageOp::List { prefix, limit }
                }
                "delete" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    StorageOp::Delete { key }
                }
                _ => return, // Unknown operation
            };

            // Get success callback (arg 3)
            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = {
                let mut id = state.next_id.lock().unwrap();
                let current = *id;
                *id += 1;
                current
            };

            {
                let mut cbs = state.callbacks.lock().unwrap();
                cbs.insert(callback_id, v8::Global::new(scope, success_fn));
            }

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
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__fetchState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut FetchState;
            let state = unsafe { &*state_ptr };

            // Args: (bindingName, operation, params, successCallback)
            if args.length() < 4 {
                return;
            }

            // Get binding name (arg 0)
            let binding_name = match args.get(0).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get operation name (arg 1)
            let operation = match args.get(1).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get params object (arg 2)
            let params = match args.get(2).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Build KvOp from operation name and params
            let kv_op = match operation.as_str() {
                "get" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    KvOp::Get { key }
                }
                "put" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();

                    let value_key = v8::String::new(scope, "value").unwrap();
                    let value = params
                        .get(scope, value_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();

                    // Optional expiresIn (TTL in seconds)
                    let expires_in_key = v8::String::new(scope, "expiresIn").unwrap();
                    let expires_in = params
                        .get(scope, expires_in_key.into())
                        .filter(|v| !v.is_undefined() && !v.is_null())
                        .and_then(|v| v.uint32_value(scope))
                        .map(|v| v as u64);

                    KvOp::Put {
                        key,
                        value,
                        expires_in,
                    }
                }
                "delete" => {
                    let key_key = v8::String::new(scope, "key").unwrap();
                    let key = params
                        .get(scope, key_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();
                    KvOp::Delete { key }
                }
                "list" => {
                    let prefix_key = v8::String::new(scope, "prefix").unwrap();
                    let prefix = params.get(scope, prefix_key.into()).and_then(|v| {
                        if v.is_null() || v.is_undefined() {
                            None
                        } else {
                            v.to_string(scope).map(|s| s.to_rust_string_lossy(scope))
                        }
                    });

                    let limit_key = v8::String::new(scope, "limit").unwrap();
                    let limit = params
                        .get(scope, limit_key.into())
                        .filter(|v| !v.is_undefined() && !v.is_null())
                        .and_then(|v| v.uint32_value(scope));

                    KvOp::List { prefix, limit }
                }
                _ => return, // Unknown operation
            };

            // Get success callback (arg 3)
            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = {
                let mut id = state.next_id.lock().unwrap();
                let current = *id;
                *id += 1;
                current
            };

            {
                let mut cbs = state.callbacks.lock().unwrap();
                cbs.insert(callback_id, v8::Global::new(scope, success_fn));
            }

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
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__fetchState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if !state_val.is_external() {
                return;
            }

            let external: v8::Local<v8::External> = state_val.try_into().unwrap();
            let state_ptr = external.value() as *mut FetchState;
            let state = unsafe { &*state_ptr };

            // Args: (bindingName, operation, params, successCallback)
            if args.length() < 4 {
                return;
            }

            // Get binding name (arg 0)
            let binding_name = match args.get(0).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get operation name (arg 1)
            let operation = match args.get(1).to_string(scope) {
                Some(s) => s.to_rust_string_lossy(scope),
                None => return,
            };

            // Get params object (arg 2)
            let params = match args.get(2).to_object(scope) {
                Some(obj) => obj,
                None => return,
            };

            // Build DatabaseOp from operation name and params
            let database_op = match operation.as_str() {
                "query" => {
                    let sql_key = v8::String::new(scope, "sql").unwrap();
                    let sql = params
                        .get(scope, sql_key.into())
                        .and_then(|v| v.to_string(scope))
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_default();

                    // Get params array
                    let params_key = v8::String::new(scope, "params").unwrap();
                    let query_params: Vec<String> =
                        if let Some(params_val) = params.get(scope, params_key.into()) {
                            if let Ok(params_array) = v8::Local::<v8::Array>::try_from(params_val) {
                                let mut result = Vec::new();

                                for i in 0..params_array.length() {
                                    if let Some(item) = params_array.get_index(scope, i)
                                        && let Some(item_str) = item.to_string(scope)
                                    {
                                        result.push(item_str.to_rust_string_lossy(scope));
                                    }
                                }

                                result
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        };

                    DatabaseOp::Query {
                        sql,
                        params: query_params,
                    }
                }
                _ => return, // Unknown operation
            };

            // Get success callback (arg 3)
            let success_cb = args.get(3);

            if !success_cb.is_function() {
                return;
            }

            let success_fn: v8::Local<v8::Function> = success_cb.try_into().unwrap();

            let callback_id = {
                let mut id = state.next_id.lock().unwrap();
                let current = *id;
                *id += 1;
                current
            };

            {
                let mut cbs = state.callbacks.lock().unwrap();
                cbs.insert(callback_id, v8::Global::new(scope, success_fn));
            }

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
