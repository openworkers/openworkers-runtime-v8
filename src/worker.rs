use crate::compat::{Script, TerminationReason};
use crate::runtime::{Runtime, run_event_loop};
use crate::security::{CpuEnforcer, TimeoutGuard};
use crate::task::{HttpResponse, ResponseBody, Task};
use std::sync::atomic::Ordering;
use v8;

pub struct Worker {
    pub(crate) runtime: Runtime,
    _event_loop_handle: tokio::task::JoinHandle<()>,
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

    pub async fn new(
        script: Script,
        _log_tx: Option<std::sync::mpsc::Sender<crate::compat::LogEvent>>,
        limits: Option<crate::compat::RuntimeLimits>,
    ) -> Result<Self, String> {
        let (mut runtime, scheduler_rx, callback_tx) = Runtime::new(limits);

        // Setup addEventListener
        setup_event_listener(&mut runtime)?;

        // Evaluate user script
        runtime.evaluate(&script.code)?;

        // Get stream_manager for event loop
        let stream_manager = runtime.stream_manager.clone();

        // Start event loop in background
        let event_loop_handle = tokio::spawn(async move {
            run_event_loop(scheduler_rx, callback_tx, stream_manager).await;
        });

        Ok(Self {
            runtime,
            _event_loop_handle: event_loop_handle,
        })
    }

    pub async fn exec(&mut self, mut task: Task) -> Result<TerminationReason, String> {
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
                let fetch_init = init.take().ok_or("FetchInit already consumed")?;
                self.trigger_fetch_event(fetch_init).await
            }
            Task::Scheduled(ref mut init) => {
                let scheduled_init = init.take().ok_or("ScheduledInit already consumed")?;
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
        let termination_reason = self.determine_termination_reason(
            result.is_ok(),
            cpu_guard
                .as_ref()
                .map(|g| g.was_terminated())
                .unwrap_or(false),
            wall_guard.was_triggered(),
        );

        // Guards are dropped here, cancelling any pending watchdogs
        Ok(termination_reason)
    }

    /// Determine the termination reason based on execution result and guard states.
    ///
    /// Priority order:
    /// 1. CPU time limit (most specific - actual computation exceeded)
    /// 2. Wall-clock timeout (execution took too long)
    /// 3. Memory limit (ArrayBuffer allocation failed)
    /// 4. Exception (JS error)
    /// 5. Success
    fn determine_termination_reason(
        &self,
        success: bool,
        cpu_limit_hit: bool,
        wall_timeout_hit: bool,
    ) -> TerminationReason {
        // Check guards first (they caused termination)
        if cpu_limit_hit {
            return TerminationReason::CpuTimeLimit;
        }

        if wall_timeout_hit {
            return TerminationReason::WallClockTimeout;
        }

        // Check memory limit flag
        if self.runtime.memory_limit_hit.load(Ordering::SeqCst) {
            return TerminationReason::MemoryLimit;
        }

        // Finally check execution result
        if success {
            TerminationReason::Success
        } else {
            TerminationReason::Exception
        }
    }

    async fn trigger_fetch_event(
        &mut self,
        fetch_init: crate::task::FetchInit,
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
                let method_val = v8::String::new(scope, &req.method).unwrap();
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
                if let Some(body_bytes) = &req.body {
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
                let method_val = v8::String::new(scope, &req.method).unwrap();
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
                trigger_fn.call(scope, global.into(), &[request_obj.into()]);
            }
        }

        // Process callbacks to allow async operations (Promises, timers, fetch) to complete
        // Strategy: Check frequently with minimal sleep for fast responses,
        // but support long-running async operations (up to 5 seconds)
        for iteration in 0..5000 {
            // Process pending callbacks and microtasks
            self.runtime.process_callbacks();

            // Check if response is available and body has been read
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
                        if resp_obj.get(scope, status_key.into()).is_some() {
                            // Response is ready! (body is in the ReadableStream queue)
                            break;
                        }
                    }
                }
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

            // Check if response has _nativeStreamId (it's a native stream forward)
            let native_stream_id_key = v8::String::new(scope, "_nativeStreamId").unwrap();
            let native_stream_id = resp_obj
                .get(scope, native_stream_id_key.into())
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
            let body = if let Some(stream_id) = native_stream_id {
                // Native stream forward - create bounded channel for backpressure
                use crate::runtime::stream_manager::StreamChunk;
                use crate::task::RESPONSE_STREAM_BUFFER_SIZE;

                let (tx, rx) = tokio::sync::mpsc::channel(RESPONSE_STREAM_BUFFER_SIZE);
                let stream_manager = self.runtime.stream_manager.clone();

                // Spawn task to read from stream and forward to channel
                // Backpressure: if channel is full, this task waits (slowing upstream)
                tokio::spawn(async move {
                    loop {
                        match stream_manager.read_chunk(stream_id).await {
                            Ok(chunk) => match chunk {
                                StreamChunk::Data(bytes) => {
                                    // send() is async and waits if buffer is full
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
                            },
                            Err(e) => {
                                let _ = tx.send(Err(e)).await;
                                break;
                            }
                        }
                    }
                });

                ResponseBody::Stream(rx)
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
        scheduled_init: crate::task::ScheduledInit,
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

fn setup_event_listener(runtime: &mut Runtime) -> Result<(), String> {
    let code = r#"
        globalThis.addEventListener = function(type, handler) {
            if (type === 'fetch') {
                globalThis.__fetchHandler = handler;
                globalThis.__triggerFetch = function(request) {
                    const event = {
                        request: request,
                        respondWith: function(responseOrPromise) {
                            // Handle both direct Response and Promise<Response>
                            if (responseOrPromise && typeof responseOrPromise.then === 'function') {
                                // It's a Promise, wait for it to resolve
                                responseOrPromise
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
                                // Direct Response object
                                globalThis.__lastResponse = responseOrPromise;
                            }
                        }
                    };

                    // Call handler synchronously
                    try {
                        handler(event);
                    } catch (error) {
                        console.error('[addEventListener] Error in fetch handler:', error);
                        // Set error response
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
