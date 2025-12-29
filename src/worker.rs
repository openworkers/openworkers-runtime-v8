use crate::runtime::{Runtime, run_event_loop};
use crate::security::{CpuEnforcer, TimeoutGuard};
use openworkers_core::{
    HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script, Task,
    TerminationReason,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use v8;

pub struct Worker {
    pub(crate) runtime: Runtime,
    _event_loop_handle: tokio::task::JoinHandle<()>,
    aborted: Arc<AtomicBool>,
}

impl Worker {
    /// Process pending callbacks (timers, etc.)
    pub fn process_callbacks(&mut self) {
        self.runtime.process_callbacks();
    }

    /// Get the stream manager for creating/managing native streams
    pub fn stream_manager(&self) -> std::sync::Arc<crate::runtime::stream_manager::StreamManager> {
        self.runtime.stream_manager.clone()
    }

    /// Evaluate JavaScript code (for testing/advanced use)
    pub fn evaluate(&mut self, code: &str) -> Result<(), String> {
        self.runtime.evaluate(code)
    }

    /// Get access to the V8 isolate and context (for advanced testing)
    pub fn with_runtime<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        f(&mut self.runtime)
    }
}

impl Worker {
    /// Create a new worker with an OperationsHandler
    ///
    /// All operations (fetch, log, etc.) go through the runner's OperationsHandler.
    pub async fn new_with_ops(
        script: Script,
        limits: Option<RuntimeLimits>,
        ops: OperationsHandle,
    ) -> Result<Self, TerminationReason> {
        let (mut runtime, scheduler_rx, callback_tx) = Runtime::new(limits);

        // Setup addEventListener
        setup_event_listener(&mut runtime).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup addEventListener: {}",
                e
            ))
        })?;

        // Setup environment variables and bindings
        setup_env(&mut runtime, &script.env, &script.bindings).map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to setup env: {}", e))
        })?;

        // Evaluate user script
        runtime.evaluate(&script.code).map_err(|e| {
            TerminationReason::Exception(format!("Script evaluation failed: {}", e))
        })?;

        // Setup ES Modules handler if `export default { fetch }` is used
        // This takes priority over addEventListener('fetch', ...)
        setup_es_modules_handler(&mut runtime).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup ES modules handler: {}",
                e
            ))
        })?;

        // Get stream_manager for event loop
        let stream_manager = runtime.stream_manager.clone();

        // Start event loop in background (with optional Operations handle)
        // Use spawn_local to keep it in the same LocalSet as the V8 worker,
        // which allows nested spawn_local calls in ops (like do_fetch streaming)
        let event_loop_handle = tokio::task::spawn_local(async move {
            run_event_loop(scheduler_rx, callback_tx, stream_manager, ops).await;
        });

        Ok(Self {
            runtime,
            _event_loop_handle: event_loop_handle,
            aborted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create a new worker with default DirectOperations (for testing)
    ///
    /// Note: DirectOperations returns errors for fetch operations.
    /// In production, use `new_with_ops` with a real OperationsHandler.
    pub async fn new(
        script: Script,
        limits: Option<RuntimeLimits>,
    ) -> Result<Self, TerminationReason> {
        let ops: OperationsHandle = Arc::new(openworkers_core::DefaultOps);
        Self::new_with_ops(script, limits, ops).await
    }

    /// Abort the worker execution
    pub fn abort(&mut self) {
        self.aborted.store(true, Ordering::SeqCst);
        // V8 has terminate_execution which we can call
        self.runtime.isolate.terminate_execution();
    }

    pub async fn exec(&mut self, mut task: Task) -> Result<(), TerminationReason> {
        // Check if aborted before starting
        if self.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        // Get limits from runtime
        let limits = &self.runtime.limits;
        let isolate_handle = self.runtime.isolate.thread_safe_handle();

        // Setup security guards:
        // 1. Wall-clock timeout (all platforms) - prevents hanging on I/O
        let wall_guard = TimeoutGuard::new(isolate_handle.clone(), limits.max_wall_clock_time_ms);

        // 2. CPU time limit (Linux only) - prevents CPU-bound infinite loops
        let cpu_guard = CpuEnforcer::new(isolate_handle, limits.max_cpu_time_ms);

        // Execute the task
        let result = match task {
            Task::Fetch(ref mut init) => {
                let fetch_init = init.take().ok_or(TerminationReason::Other(
                    "FetchInit already consumed".to_string(),
                ))?;
                self.trigger_fetch_event(fetch_init, &wall_guard, &cpu_guard)
                    .await
            }
            Task::Scheduled(ref mut init) => {
                let scheduled_init = init.take().ok_or(TerminationReason::Other(
                    "ScheduledInit already consumed".to_string(),
                ))?;
                self.trigger_scheduled_event(scheduled_init)
                    .await
                    .map(|_| HttpResponse {
                        status: 200,
                        headers: vec![],
                        body: ResponseBody::None,
                    })
            }
        };

        // Determine termination reason by checking guards (in priority order)
        self.check_termination_reason(
            result,
            cpu_guard
                .as_ref()
                .map(|g| g.was_terminated())
                .unwrap_or(false),
            wall_guard.was_triggered(),
        )
        // Guards are dropped here, cancelling any pending watchdogs
    }

    /// Check termination reason based on execution result and guard states.
    ///
    /// Priority order:
    /// 1. CPU time limit (most specific - actual computation exceeded)
    /// 2. Wall-clock timeout (execution took too long)
    /// 3. Memory limit (ArrayBuffer allocation failed)
    /// 4. Aborted (via abort() call)
    /// 5. Exception (JS error)
    /// 6. Success
    fn check_termination_reason(
        &self,
        result: Result<HttpResponse, String>,
        cpu_limit_hit: bool,
        wall_timeout_hit: bool,
    ) -> Result<(), TerminationReason> {
        // Check guards first (they caused termination)
        if cpu_limit_hit {
            return Err(TerminationReason::CpuTimeLimit);
        }

        if wall_timeout_hit {
            return Err(TerminationReason::WallClockTimeout);
        }

        // Check memory limit flag
        if self.runtime.memory_limit_hit.load(Ordering::SeqCst) {
            return Err(TerminationReason::MemoryLimit);
        }

        // Check if aborted
        if self.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        // Finally check execution result
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(TerminationReason::Exception(e)),
        }
    }

    async fn trigger_fetch_event(
        &mut self,
        fetch_init: openworkers_core::FetchInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<HttpResponse, String> {
        let req = &fetch_init.req;

        // Create channel for response notification (like JSC)
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel::<String>();

        // Store the sender in runtime so JS can use it
        {
            let mut tx_lock = self.runtime.fetch_response_tx.lock().unwrap();
            *tx_lock = Some(response_tx);
        }

        // Trigger fetch handler
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);

            // Get Request constructor
            let request_key = v8::String::new(scope, "Request").unwrap();
            let request_constructor = global
                .get(scope, request_key.into())
                .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok());

            let request_obj = if let Some(request_ctor) = request_constructor {
                // Create init object with method, headers, body
                let init_obj = v8::Object::new(scope);

                let method_key = v8::String::new(scope, "method").unwrap();
                let method_val = v8::String::new(scope, req.method.as_str()).unwrap();
                init_obj.set(scope, method_key.into(), method_val.into());

                // Create headers object for init
                let headers_obj = v8::Object::new(scope);
                for (key, value) in &req.headers {
                    let k = v8::String::new(scope, key).unwrap();
                    let v = v8::String::new(scope, value).unwrap();
                    headers_obj.set(scope, k.into(), v.into());
                }
                let headers_key = v8::String::new(scope, "headers").unwrap();
                init_obj.set(scope, headers_key.into(), headers_obj.into());

                // Add body if present (convert Bytes to string)
                if let RequestBody::Bytes(body_bytes) = &req.body {
                    if let Ok(body_str) = std::str::from_utf8(body_bytes) {
                        let body_key = v8::String::new(scope, "body").unwrap();
                        let body_val = v8::String::new(scope, body_str).unwrap();
                        init_obj.set(scope, body_key.into(), body_val.into());
                    }
                }

                // Call new Request(url, init)
                let url_val = v8::String::new(scope, &req.url).unwrap();
                request_ctor
                    .new_instance(scope, &[url_val.into(), init_obj.into()])
                    .unwrap_or_else(|| v8::Object::new(scope))
            } else {
                // Fallback to plain object if Request not available
                let obj = v8::Object::new(scope);
                let url_key = v8::String::new(scope, "url").unwrap();
                let url_val = v8::String::new(scope, &req.url).unwrap();
                obj.set(scope, url_key.into(), url_val.into());

                let method_key = v8::String::new(scope, "method").unwrap();
                let method_val = v8::String::new(scope, req.method.as_str()).unwrap();
                obj.set(scope, method_key.into(), method_val.into());

                let headers_obj = v8::Object::new(scope);
                for (key, value) in &req.headers {
                    let k = v8::String::new(scope, key).unwrap();
                    let v = v8::String::new(scope, value).unwrap();
                    headers_obj.set(scope, k.into(), v.into());
                }
                let headers_key = v8::String::new(scope, "headers").unwrap();
                obj.set(scope, headers_key.into(), headers_obj.into());
                obj
            };

            // Trigger fetch handler
            let trigger_key = v8::String::new(scope, "__triggerFetch").unwrap();

            if let Some(trigger_val) = global.get(scope, trigger_key.into())
                && trigger_val.is_function()
            {
                let trigger_fn: v8::Local<v8::Function> = trigger_val.try_into().unwrap();
                let result = trigger_fn.call(scope, global.into(), &[request_obj.into()]);

                // If call returned None, V8 was terminated (CPU/wall-clock timeout)
                if result.is_none() {
                    return Err("Execution terminated".to_string());
                }
            }
        }

        // Process callbacks to allow async operations (Promises, timers, fetch) to complete
        // Strategy: Check frequently with minimal sleep for fast responses,
        // but support long-running async operations (up to 5 seconds)
        for iteration in 0..5000 {
            // Check if execution was terminated (CPU/wall-clock timeout)
            if self.runtime.isolate.is_execution_terminating()
                || wall_guard.was_triggered()
                || cpu_guard
                    .as_ref()
                    .map(|g| g.was_terminated())
                    .unwrap_or(false)
            {
                return Err("Execution terminated".to_string());
            }

            // Process pending callbacks and microtasks
            self.runtime.process_callbacks();

            // Check if response is available and body has been read
            // CRITICAL: Wrap in explicit block to drop V8 scopes BEFORE the await below.
            // V8 scopes (HandleScope, ContextScope) use RAII - they Enter on creation and Exit on drop.
            // If scopes are alive during await, another worker on the same thread may run and corrupt
            // V8's context stack, causing "Cannot exit non-entered context" crashes.
            let response_ready = {
                use std::pin::pin;
                let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.runtime.context);
                let scope = &mut v8::ContextScope::new(&mut scope, context);
                let global = context.global(scope);

                let resp_key = v8::String::new(scope, "__lastResponse").unwrap();
                if let Some(resp_val) = global.get(scope, resp_key.into()) {
                    // Check if it's a valid Response object (not undefined, not a Promise)
                    if !resp_val.is_undefined() && !resp_val.is_null() && !resp_val.is_promise() {
                        if let Some(resp_obj) = resp_val.to_object(scope) {
                            // Check if it has a 'status' property (indicates it's a Response, not a Promise)
                            let status_key = v8::String::new(scope, "status").unwrap();
                            resp_obj.get(scope, status_key.into()).is_some()
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            }; // <-- V8 scopes dropped here, BEFORE the await

            if response_ready {
                break;
            }

            // Adaptive sleep: fast for first checks, slower later
            // This allows fast responses (<1ms) while supporting slow operations
            let sleep_duration = if iteration < 10 {
                // First 10 iterations: no sleep (for immediate sync responses)
                tokio::time::Duration::from_micros(1)
            } else if iteration < 100 {
                // Next 90 iterations: 1ms sleep (for fast async < 100ms)
                tokio::time::Duration::from_millis(1)
            } else {
                // After 100ms: 10ms sleep (for slow operations)
                tokio::time::Duration::from_millis(10)
            };

            tokio::time::sleep(sleep_duration).await;
        }

        // Now read the response from global __lastResponse
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let resp_key = v8::String::new(scope, "__lastResponse").unwrap();
        let resp_val = global
            .get(scope, resp_key.into())
            .ok_or("No response set")?;

        if let Some(resp_obj) = resp_val.to_object(scope) {
            let status_key = v8::String::new(scope, "status").unwrap();
            let status = resp_obj
                .get(scope, status_key.into())
                .and_then(|v| v.uint32_value(scope))
                .unwrap_or(200) as u16;

            // Check if response has _responseStreamId (streaming body)
            let response_stream_id_key = v8::String::new(scope, "_responseStreamId").unwrap();
            let response_stream_id =
                resp_obj
                    .get(scope, response_stream_id_key.into())
                    .and_then(|v| {
                        if v.is_null() || v.is_undefined() {
                            None
                        } else {
                            v.uint32_value(scope).map(|n| n as u64)
                        }
                    });

            // Extract headers first (needed for both paths)
            // Headers can be a Headers instance (with _map) or a plain object
            let mut headers = vec![];
            let headers_key = v8::String::new(scope, "headers").unwrap();
            if let Some(headers_val) = resp_obj.get(scope, headers_key.into())
                && let Some(headers_obj) = headers_val.to_object(scope)
            {
                // Check if this is a Headers instance (has _map)
                let map_key = v8::String::new(scope, "_map").unwrap();
                if let Some(map_val) = headers_obj.get(scope, map_key.into())
                    && let Ok(map_obj) = v8::Local::<v8::Map>::try_from(map_val)
                {
                    // Headers instance - iterate over _map
                    let entries = map_obj.as_array(scope);
                    let len = entries.length();
                    let mut i = 0;
                    while i < len {
                        if let Some(key_val) = entries.get_index(scope, i)
                            && let Some(val_val) = entries.get_index(scope, i + 1)
                            && let Some(key_str) = key_val.to_string(scope)
                            && let Some(val_str) = val_val.to_string(scope)
                        {
                            let key = key_str.to_rust_string_lossy(scope);
                            let value = val_str.to_rust_string_lossy(scope);
                            headers.push((key, value));
                        }
                        i += 2; // Map.as_array returns [key, value, key, value, ...]
                    }
                } else if let Some(props) =
                    headers_obj.get_own_property_names(scope, Default::default())
                {
                    // Plain object - use property iteration
                    for i in 0..props.length() {
                        if let Some(key_val) = props.get_index(scope, i)
                            && let Some(key_str) = key_val.to_string(scope)
                        {
                            let key = key_str.to_rust_string_lossy(scope);
                            if let Some(val) = headers_obj.get(scope, key_val)
                                && let Some(val_str) = val.to_string(scope)
                            {
                                let value = val_str.to_rust_string_lossy(scope);
                                headers.push((key, value));
                            }
                        }
                    }
                }
            }

            // Determine body type: streaming or buffered
            let body = if let Some(stream_id) = response_stream_id {
                // Response stream - take the receiver from StreamManager
                // JS is writing chunks to this stream via __responseStreamWrite
                use crate::runtime::stream_manager::StreamChunk;

                if let Some(receiver) = self.runtime.stream_manager.take_receiver(stream_id) {
                    // Create a channel that converts StreamChunk to Result<Bytes, String>
                    let (tx, rx) = tokio::sync::mpsc::channel(16);

                    // Spawn task to convert StreamChunk -> Result<Bytes, String>
                    tokio::task::spawn_local(async move {
                        let mut receiver = receiver;
                        while let Some(chunk) = receiver.recv().await {
                            match chunk {
                                StreamChunk::Data(bytes) => {
                                    if tx.send(Ok(bytes)).await.is_err() {
                                        break;
                                    }
                                }
                                StreamChunk::Done => {
                                    break;
                                }
                                StreamChunk::Error(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    break;
                                }
                            }
                        }
                    });

                    ResponseBody::Stream(rx)
                } else {
                    // Stream not found - fall back to empty body
                    ResponseBody::None
                }
            } else {
                // Buffered body - use _getRawBody()
                let get_raw_body_key = v8::String::new(scope, "_getRawBody").unwrap();
                let body_bytes = if let Some(get_raw_body_val) =
                    resp_obj.get(scope, get_raw_body_key.into())
                    && let Ok(get_raw_body_fn) =
                        v8::Local::<v8::Function>::try_from(get_raw_body_val)
                {
                    if let Some(result_val) = get_raw_body_fn.call(scope, resp_obj.into(), &[])
                        && let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(result_val)
                    {
                        let len = uint8_array.byte_length();
                        let mut bytes_vec = vec![0u8; len];
                        uint8_array.copy_contents(&mut bytes_vec);
                        bytes::Bytes::from(bytes_vec)
                    } else {
                        bytes::Bytes::new()
                    }
                } else {
                    bytes::Bytes::new()
                };

                ResponseBody::Bytes(body_bytes)
            };

            let response = HttpResponse {
                status,
                headers,
                body,
            };

            let _ = fetch_init.res_tx.send(response);

            // Return success indicator (body already sent via channel)
            return Ok(HttpResponse {
                status,
                headers: vec![],
                body: ResponseBody::None,
            });
        }

        Err("Invalid response object".to_string())
    }

    async fn trigger_scheduled_event(
        &mut self,
        scheduled_init: openworkers_core::ScheduledInit,
    ) -> Result<(), String> {
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            // Trigger scheduled handler
            let global = context.global(scope);
            let handler_key = v8::String::new(scope, "__scheduledHandler").unwrap();

            if let Some(handler_val) = global.get(scope, handler_key.into())
                && handler_val.is_function()
            {
                let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                // Create event object
                let event_obj = v8::Object::new(scope);
                let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                let time_val = v8::Number::new(scope, scheduled_init.time as f64);
                event_obj.set(scope, time_key.into(), time_val.into());

                handler_fn.call(scope, global.into(), &[event_obj.into()]);
            }
        }

        // Process callbacks
        self.runtime.process_callbacks();

        let _ = scheduled_init.res_tx.send(());
        Ok(())
    }
}

fn setup_env(
    runtime: &mut Runtime,
    env: &Option<std::collections::HashMap<String, String>>,
    bindings: &[openworkers_core::BindingInfo],
) -> Result<(), String> {
    // Build JSON string for env vars
    let env_json = if let Some(env_map) = env {
        let pairs: Vec<String> = env_map
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}:{}",
                    serde_json::to_string(k).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_string())
                )
            })
            .collect();
        format!("{{{}}}", pairs.join(","))
    } else {
        "{}".to_string()
    };

    // Build binding object definitions
    let binding_defs: Vec<String> = bindings
        .iter()
        .map(|b| {
            let name = serde_json::to_string(&b.name).unwrap_or_else(|_| "\"\"".to_string());
            match b.binding_type {
                openworkers_core::BindingType::Assets => {
                    // Assets binding has fetch() only
                    format!(
                        r#"{name}: {{
                            fetch: function(path, options) {{
                                options = options || {{}};
                                return new Promise((resolve, reject) => {{
                                    const fetchOptions = {{
                                        url: path,
                                        method: options.method || 'GET',
                                        headers: options.headers || {{}},
                                        body: options.body || null
                                    }};
                                    __nativeBindingFetch({name}, fetchOptions, (meta) => {{
                                        const stream = __createNativeStream(meta.streamId);
                                        const response = new Response(stream, {{
                                            status: meta.status,
                                            headers: meta.headers
                                        }});
                                        response.ok = meta.status >= 200 && meta.status < 300;
                                        response.statusText = meta.statusText;
                                        resolve(response);
                                    }}, reject);
                                }});
                            }}
                        }}"#,
                    )
                }
                openworkers_core::BindingType::Storage => {
                    // Storage binding has get/put/head/list/delete
                    format!(
                        r#"{name}: {{
                            get: function(key) {{
                                return new Promise((resolve, reject) => {{
                                    __nativeBindingStorage({name}, 'get', {{ key }}, (result) => {{
                                        if (!result.success) {{
                                            reject(new Error(result.error));
                                        }} else if (result.body) {{
                                            resolve(new TextDecoder().decode(result.body));
                                        }} else {{
                                            resolve(null);
                                        }}
                                    }});
                                }});
                            }},
                            put: function(key, value) {{
                                return new Promise((resolve, reject) => {{
                                    const body = typeof value === 'string' ? new TextEncoder().encode(value) : value;
                                    __nativeBindingStorage({name}, 'put', {{ key, body }}, (result) => {{
                                        if (!result.success) {{
                                            reject(new Error(result.error));
                                        }} else {{
                                            resolve();
                                        }}
                                    }});
                                }});
                            }},
                            head: function(key) {{
                                return new Promise((resolve, reject) => {{
                                    __nativeBindingStorage({name}, 'head', {{ key }}, (result) => {{
                                        if (!result.success) {{
                                            reject(new Error(result.error));
                                        }} else {{
                                            resolve({{ size: result.size, etag: result.etag }});
                                        }}
                                    }});
                                }});
                            }},
                            list: function(options) {{
                                options = options || {{}};
                                return new Promise((resolve, reject) => {{
                                    __nativeBindingStorage({name}, 'list', {{ prefix: options.prefix, limit: options.limit }}, (result) => {{
                                        if (!result.success) {{
                                            reject(new Error(result.error));
                                        }} else {{
                                            resolve({{ keys: result.keys, truncated: result.truncated }});
                                        }}
                                    }});
                                }});
                            }},
                            delete: function(key) {{
                                return new Promise((resolve, reject) => {{
                                    __nativeBindingStorage({name}, 'delete', {{ key }}, (result) => {{
                                        if (!result.success) {{
                                            reject(new Error(result.error));
                                        }} else {{
                                            resolve();
                                        }}
                                    }});
                                }});
                            }}
                        }}"#,
                    )
                }
                openworkers_core::BindingType::Kv => {
                    // KV binding has get/put/delete/list
                    // Use IIFE to capture binding name as a string constant
                    format!(
                        r#"{name}: (function() {{
                            const __bindingName = {name};
                            return {{
                                get: function(key) {{
                                    return new Promise((resolve, reject) => {{
                                        __nativeBindingKv(__bindingName, 'get', {{ key }}, (result) => {{
                                            if (!result.success) {{
                                                reject(new Error(result.error));
                                            }} else {{
                                                resolve(result.value);
                                            }}
                                        }});
                                    }});
                                }},
                                put: function(key, value, options) {{
                                    return new Promise((resolve, reject) => {{
                                        const params = {{ key, value }};
                                        if (options && options.expiresIn) {{
                                            params.expiresIn = options.expiresIn;
                                        }}
                                        __nativeBindingKv(__bindingName, 'put', params, (result) => {{
                                            if (!result.success) {{
                                                reject(new Error(result.error));
                                            }} else {{
                                                resolve();
                                            }}
                                        }});
                                    }});
                                }},
                                delete: function(key) {{
                                    return new Promise((resolve, reject) => {{
                                        __nativeBindingKv(__bindingName, 'delete', {{ key }}, (result) => {{
                                            if (!result.success) {{
                                                reject(new Error(result.error));
                                            }} else {{
                                                resolve();
                                            }}
                                        }});
                                    }});
                                }},
                                list: function(options) {{
                                    return new Promise((resolve, reject) => {{
                                        const params = {{}};
                                        if (options) {{
                                            if (options.prefix) params.prefix = options.prefix;
                                            if (options.limit) params.limit = options.limit;
                                        }}
                                        __nativeBindingKv(__bindingName, 'list', params, (result) => {{
                                            if (!result.success) {{
                                                reject(new Error(result.error));
                                            }} else {{
                                                resolve(result.keys);
                                            }}
                                        }});
                                    }});
                                }}
                            }};
                        }})()"#,
                    )
                }
                openworkers_core::BindingType::Database => {
                    // Database binding has query method
                    // Use IIFE to capture binding name as a string constant
                    format!(
                        r#"{name}: (function() {{
                            const __bindingName = {name};
                            return {{
                                query: function(sql, params) {{
                                    return new Promise((resolve, reject) => {{
                                        const queryParams = {{
                                            sql,
                                            params: params || []
                                        }};
                                        __nativeBindingDatabase(__bindingName, 'query', queryParams, (result) => {{
                                            if (!result.success) {{
                                                reject(new Error(result.error));
                                            }} else {{
                                                resolve(result.rows);
                                            }}
                                        }});
                                    }});
                                }}
                            }};
                        }})()"#,
                    )
                }
            }
        })
        .collect();

    let bindings_json = if binding_defs.is_empty() {
        String::new()
    } else {
        format!(", {{{}}}", binding_defs.join(","))
    };

    // Set globalThis.env as read-only with both env vars and bindings
    let code = format!(
        r#"Object.defineProperty(globalThis, 'env', {{
            value: Object.freeze(Object.assign({}{})),
            writable: false,
            enumerable: true,
            configurable: false
        }});"#,
        env_json, bindings_json
    );

    runtime.evaluate(&code)
}

fn setup_event_listener(runtime: &mut Runtime) -> Result<(), String> {
    let code = r#"
        // Stream response body to Rust (stream all responses with body)
        async function __streamResponseBody(response) {
            if (!response.body) {
                return response;
            }

            // If it's a native stream (fetch forward), just mark it
            if (response.body._nativeStreamId !== undefined) {
                response._responseStreamId = response.body._nativeStreamId;
                return response;
            }

            // Stream the body: create output stream and pipe
            const streamId = __responseStreamCreate();
            response._responseStreamId = streamId;

            // Read and forward asynchronously
            (async () => {
                try {
                    const reader = response.body.getReader();
                    while (true) {
                        const { value, done } = await reader.read();
                        if (done) break;
                        if (value && value.length > 0) {
                            while (!__responseStreamWrite(streamId, value)) {
                                await new Promise(resolve => setTimeout(resolve, 1));
                            }
                        }
                    }
                } catch (error) {
                    console.error('[streamResponseBody] Error:', error);
                } finally {
                    __responseStreamEnd(streamId);
                }
            })();

            return response;
        }

        globalThis.addEventListener = function(type, handler) {
            if (type === 'fetch') {
                globalThis.__fetchHandler = handler;
                globalThis.__triggerFetch = function(request) {
                    const event = {
                        request: request,
                        respondWith: function(responseOrPromise) {
                            // Handle both direct Response and Promise<Response>
                            if (responseOrPromise && typeof responseOrPromise.then === 'function') {
                                responseOrPromise
                                    .then(response => __streamResponseBody(response))
                                    .then(response => {
                                        globalThis.__lastResponse = response;
                                    })
                                    .catch(error => {
                                        console.error('[respondWith] Promise rejected:', error);
                                        globalThis.__lastResponse = new Response(
                                            'Promise rejected: ' + (error.message || error),
                                            { status: 500 }
                                        );
                                    });
                            } else {
                                __streamResponseBody(responseOrPromise)
                                    .then(response => {
                                        globalThis.__lastResponse = response;
                                    });
                            }
                        }
                    };

                    try {
                        handler(event);
                    } catch (error) {
                        console.error('[addEventListener] Error in fetch handler:', error);
                        globalThis.__lastResponse = new Response('Handler exception: ' + (error.message || error), { status: 500 });
                    }
                };
            } else if (type === 'scheduled') {
                globalThis.__scheduledHandler = handler;
            }
        };
    "#;

    runtime.evaluate(code)
}

/// Setup ES Modules handler if `export default { fetch }` is used
///
/// This checks if the user script exported a default object with a fetch method.
/// If so, it overrides __triggerFetch to use the ES Modules style (direct return)
/// instead of the Service Worker style (event.respondWith).
///
/// ES Modules style takes priority over addEventListener.
fn setup_es_modules_handler(runtime: &mut Runtime) -> Result<(), String> {
    let code = r#"
        // Check if ES Modules style is used: export default { fetch }
        if (typeof globalThis.default === 'object' && typeof globalThis.default.fetch === 'function') {
            const moduleHandler = globalThis.default;

            // Override __triggerFetch for ES Modules style
            globalThis.__triggerFetch = async function(request) {
                try {
                    // ES Modules style: fetch(request, env, ctx) returns Response directly
                    const response = await moduleHandler.fetch(request, globalThis.env, {
                        waitUntil: () => {},
                        passThroughOnException: () => {}
                    });

                    // Process response body for streaming
                    const processed = await __streamResponseBody(response);
                    globalThis.__lastResponse = processed;
                } catch (error) {
                    console.error('[ES Modules] Error in fetch handler:', error);
                    globalThis.__lastResponse = new Response(
                        'Handler exception: ' + (error.message || error),
                        { status: 500 }
                    );
                }
            };
        }

        // Same for scheduled events
        if (typeof globalThis.default === 'object' && typeof globalThis.default.scheduled === 'function') {
            const moduleScheduled = globalThis.default.scheduled;

            // Wrap to pass env and ctx
            globalThis.__scheduledHandler = function(event) {
                return moduleScheduled(event, globalThis.env, {
                    waitUntil: () => {}
                });
            };
        }
    "#;

    runtime.evaluate(code)
}

impl openworkers_core::Worker for Worker {
    async fn new(script: Script, limits: Option<RuntimeLimits>) -> Result<Self, TerminationReason> {
        Worker::new(script, limits).await
    }

    async fn exec(&mut self, task: Task) -> Result<(), TerminationReason> {
        Worker::exec(self, task).await
    }

    fn abort(&mut self) {
        Worker::abort(self)
    }
}
