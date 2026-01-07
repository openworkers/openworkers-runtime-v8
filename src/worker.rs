use crate::runtime::{Runtime, run_event_loop};
use crate::security::{CpuEnforcer, TimeoutGuard};
use openworkers_core::{
    HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script, Task,
    TerminationReason, WorkerCode,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use v8;

/// Condition to check for exiting the event loop
#[derive(Debug, Clone, Copy)]
pub(crate) enum EventLoopExit {
    /// Wait for __lastResponse to be a valid Response object (has status property)
    ResponseReady,
    /// Wait for __requestComplete to be true (handler finished, including async work)
    HandlerComplete,
    /// Wait for __requestComplete && __activeResponseStreams == 0 (fully complete)
    FullyComplete,
}

/// Optional abort handling configuration for event loop.
///
/// When enabled, the event loop will:
/// 1. Detect client disconnects via stream_manager
/// 2. Signal the disconnect to JS via __signalClientDisconnect
/// 3. Allow a grace period for JS to react before force-exiting
#[derive(Clone)]
pub(crate) struct AbortConfig {
    /// Grace period after signaling abort before force-exit
    pub grace_period: tokio::time::Duration,
}

impl AbortConfig {
    pub fn new(grace_period_ms: u64) -> Self {
        Self {
            grace_period: tokio::time::Duration::from_millis(grace_period_ms),
        }
    }
}

impl Default for AbortConfig {
    fn default() -> Self {
        Self::new(100) // 100ms grace period
    }
}

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
        self.runtime
            .evaluate(&WorkerCode::JavaScript(code.to_string()))
    }

    /// Get access to the V8 isolate and context (for advanced testing)
    pub fn with_runtime<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Runtime) -> R,
    {
        f(&mut self.runtime)
    }

    /// Read a global variable as u32 (for testing/debugging)
    pub fn get_global_u32(&mut self, name: &str) -> Option<u32> {
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let key = v8::String::new(scope, name)?;
        let value = global.get(scope, key.into())?;
        value.uint32_value(scope)
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
        let (mut runtime, scheduler_rx, callback_tx, callback_notify) = Runtime::new(limits);

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
            run_event_loop(
                scheduler_rx,
                callback_tx,
                callback_notify,
                stream_manager,
                ops,
            )
            .await;
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
                self.trigger_scheduled_event(scheduled_init, &wall_guard, &cpu_guard)
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
            Err(e) if e.contains("Max event loop iterations") => {
                Err(TerminationReason::MaxIterationsReached)
            }
            Err(e) => Err(TerminationReason::Exception(e)),
        }
    }

    /// Calculate maximum event loop iterations based on wall-clock timeout.
    ///
    /// Formula: 100 base iterations + 1 per 100ms of wall-clock timeout.
    /// This ensures we have enough iterations for long-running async operations
    /// while still having a safety limit.
    #[inline]
    fn max_event_loop_iterations(&self) -> usize {
        100 + (self.runtime.limits.max_wall_clock_time_ms / 100) as usize
    }

    /// Check if execution should be terminated.
    ///
    /// Returns true if any termination condition is met:
    /// - V8 isolate is terminating (e.g., from terminate_execution())
    /// - Wall-clock timeout was triggered
    /// - CPU time limit was exceeded (Linux only)
    #[inline]
    fn is_terminated(&self, wall_guard: &TimeoutGuard, cpu_guard: &Option<CpuEnforcer>) -> bool {
        self.runtime.isolate.is_execution_terminating()
            || wall_guard.was_triggered()
            || cpu_guard
                .as_ref()
                .map(|g| g.was_terminated())
                .unwrap_or(false)
    }

    /// Run the event loop until a condition is met or timeout/termination occurs.
    ///
    /// This is the core loop for processing async operations (Promises, timers, fetch).
    /// It checks termination guards, processes callbacks, and waits for the exit condition.
    ///
    /// When `abort_config` is provided, the loop also:
    /// - Detects client disconnects via stream_manager
    /// - Signals the disconnect to JS
    /// - Allows a grace period before force-exiting (even with active streams)
    async fn await_event_loop(
        &mut self,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
        exit_condition: EventLoopExit,
        abort_config: Option<AbortConfig>,
    ) -> Result<(), String> {
        let max_iterations = self.max_event_loop_iterations();
        let mut abort_signaled_at: Option<tokio::time::Instant> = None;

        for _iteration in 0..max_iterations {
            if self.is_terminated(wall_guard, cpu_guard) {
                return Err("Execution terminated".to_string());
            }

            // Process pending callbacks and microtasks
            self.runtime.process_callbacks();

            // Check exit condition
            // CRITICAL: Wrap in explicit block to drop V8 scopes BEFORE the await below.
            // V8 scopes use RAII - if alive during await, another worker on the same thread
            // may run and corrupt V8's context stack.
            let should_exit = {
                use std::pin::pin;
                let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.runtime.context);
                let scope = &mut v8::ContextScope::new(&mut scope, context);
                let global = context.global(scope);

                // Basic exit condition check
                let base_exit = check_exit_condition(scope, global, exit_condition);

                // If abort detection is enabled, handle client disconnects
                if let Some(ref config) = abort_config {
                    let (request_complete, active_streams) = get_completion_state(scope, global);

                    // Detect client disconnect and signal abort to JS
                    if active_streams > 0 && abort_signaled_at.is_none() {
                        if let Some(stream_id) = get_response_stream_id(scope, global) {
                            if !self.runtime.stream_manager.has_sender(stream_id) {
                                abort_signaled_at = Some(tokio::time::Instant::now());
                                signal_client_disconnect(scope);
                            }
                        }
                    }

                    // Check grace period
                    let grace_exceeded = abort_signaled_at
                        .map(|t| t.elapsed() > config.grace_period)
                        .unwrap_or(false);

                    // Exit if base condition met, OR if request complete and grace exceeded
                    base_exit || (request_complete && grace_exceeded)
                } else {
                    base_exit
                }
            }; // V8 scopes dropped here, BEFORE the await

            if should_exit {
                return Ok(());
            }

            // Wait for scheduler to signal callback ready, or timeout to check guards
            tokio::select! {
                _ = self.runtime.callback_notify.notified() => {
                    // Callback ready, loop will process it
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    // Periodic check for wall_guard timeout (checked at loop start)
                }
            }
        }

        Err("Max event loop iterations reached".to_string())
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

                // Add body if present (as Uint8Array for binary support)
                if let RequestBody::Bytes(body_bytes) = &req.body {
                    if !body_bytes.is_empty() {
                        let len = body_bytes.len();
                        let backing_store =
                            v8::ArrayBuffer::new_backing_store_from_vec(body_bytes.to_vec())
                                .make_shared();
                        let array_buffer =
                            v8::ArrayBuffer::with_backing_store(scope, &backing_store);
                        let uint8_array = v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();

                        let body_key = v8::String::new(scope, "body").unwrap();
                        init_obj.set(scope, body_key.into(), uint8_array.into());
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

        // Wait for response to be ready (no abort detection needed yet)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::ResponseReady, None)
            .await?;

        // Read response from global __lastResponse
        // Wrap in block to drop V8 scopes before the waitUntil loop
        let (status, response) = {
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
                let response_stream_id = resp_obj
                    .get(scope, response_stream_id_key.into())
                    .and_then(|v| {
                        if v.is_null() || v.is_undefined() {
                            None
                        } else {
                            v.uint32_value(scope).map(|n| n as u64)
                        }
                    });

                // Extract headers (handles both Headers instance and plain object)
                let headers = extract_headers_from_response(scope, resp_obj);

                // Determine body type: streaming or buffered
                let body = if let Some(stream_id) = response_stream_id {
                    // Response stream - take the receiver from StreamManager
                    // JS is writing chunks to this stream via __responseStreamWrite
                    use crate::runtime::stream_manager::StreamChunk;

                    if let Some(receiver) = self.runtime.stream_manager.take_receiver(stream_id) {
                        // Use bounded channel with configurable size.
                        // Large buffer (default 1024) allows most JS streams to complete.
                        // For streams larger than buffer: they'll hit backpressure and
                        // eventually timeout via wall clock (safer than memory exhaustion).
                        let buffer_size = self.runtime.limits.stream_buffer_size;
                        let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);

                        // Clone stream_manager to use in the spawned task
                        let stream_manager = self.runtime.stream_manager.clone();

                        // Spawn task to convert StreamChunk -> Result<Bytes, String>
                        // Uses select! to detect client disconnect immediately via tx.closed()
                        tokio::task::spawn_local(async move {
                            let mut receiver = receiver;

                            loop {
                                tokio::select! {
                                    // Wait for next chunk from JS (via StreamManager)
                                    chunk = receiver.recv() => {
                                        match chunk {
                                            Some(StreamChunk::Data(bytes)) => {
                                                if tx.send(Ok(bytes)).await.is_err() {
                                                    // Client disconnected while sending
                                                    stream_manager.close_stream(stream_id);
                                                    break;
                                                }
                                            }
                                            Some(StreamChunk::Done) => {
                                                break;
                                            }
                                            Some(StreamChunk::Error(e)) => {
                                                let _ = tx.send(Err(e)).await;
                                                break;
                                            }
                                            None => {
                                                // Channel closed unexpectedly
                                                break;
                                            }
                                        }
                                    }

                                    // Detect client disconnect immediately when actix drops receiver
                                    _ = tx.closed() => {
                                        // Client disconnected - close stream so JS can detect via has_sender()
                                        stream_manager.close_stream(stream_id);
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
                            && let Ok(uint8_array) =
                                v8::Local::<v8::Uint8Array>::try_from(result_val)
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

                (status, Some(response))
            } else {
                (0, None)
            }
        }; // V8 scopes dropped here

        // Send response if we got one
        let Some(response) = response else {
            return Err("Invalid response object".to_string());
        };

        let _ = fetch_init.res_tx.send(response);

        // Wait for waitUntil promises AND active response streams to complete.
        // With abort detection: signals client disconnect and allows grace period.
        self.await_event_loop(
            wall_guard,
            cpu_guard,
            EventLoopExit::FullyComplete,
            Some(AbortConfig::default()),
        )
        .await?;

        // Return success indicator (body already sent via channel)
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: ResponseBody::None,
        })
    }

    async fn trigger_scheduled_event(
        &mut self,
        scheduled_init: openworkers_core::ScheduledInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<(), String> {
        // Trigger scheduled handler
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

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

                let result = handler_fn.call(scope, global.into(), &[event_obj.into()]);

                // If call returned None, V8 was terminated (CPU/wall-clock timeout)
                if result.is_none() {
                    return Err("Execution terminated".to_string());
                }
            }
        }

        // Wait for handler to complete (including async work and waitUntil promises)
        // No abort detection needed for scheduled events (no streaming response)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::HandlerComplete, None)
            .await?;

        let _ = scheduled_init.res_tx.send(());
        Ok(())
    }
}

/// Check if the specified exit condition is met (free function to avoid borrow conflicts)
fn check_exit_condition(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
    condition: EventLoopExit,
) -> bool {
    match condition {
        EventLoopExit::ResponseReady => {
            // Check __lastResponse is a valid Response object (not undefined, not a Promise)
            let resp_key = v8::String::new(scope, "__lastResponse").unwrap();

            if let Some(resp_val) = global.get(scope, resp_key.into()) {
                if !resp_val.is_undefined() && !resp_val.is_null() && !resp_val.is_promise() {
                    if let Some(resp_obj) = resp_val.to_object(scope) {
                        // Check if it has a 'status' property (indicates it's a Response)
                        let status_key = v8::String::new(scope, "status").unwrap();
                        return resp_obj.get(scope, status_key.into()).is_some();
                    }
                }
            }

            false
        }

        EventLoopExit::HandlerComplete => {
            // Check __requestComplete == true
            let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
            global
                .get(scope, complete_key.into())
                .map(|v| v.is_true())
                .unwrap_or(false)
        }

        EventLoopExit::FullyComplete => {
            // Check __requestComplete && __activeResponseStreams == 0
            let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
            let request_complete = global
                .get(scope, complete_key.into())
                .map(|v| v.is_true())
                .unwrap_or(false);

            if !request_complete {
                return false;
            }

            let streams_key = v8::String::new(scope, "__activeResponseStreams").unwrap();
            let active_streams = global
                .get(scope, streams_key.into())
                .and_then(|v| v.uint32_value(scope))
                .unwrap_or(0);

            active_streams == 0
        }
    }
}

/// Get the completion state: (request_complete, active_streams)
#[inline]
fn get_completion_state(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
) -> (bool, u32) {
    let complete_key = v8::String::new(scope, "__requestComplete").unwrap();
    let request_complete = global
        .get(scope, complete_key.into())
        .map(|v| v.is_true())
        .unwrap_or(false);

    let streams_key = v8::String::new(scope, "__activeResponseStreams").unwrap();
    let active_streams = global
        .get(scope, streams_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0);

    (request_complete, active_streams)
}

/// Get the response stream ID if present
#[inline]
fn get_response_stream_id(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    global: v8::Local<v8::Object>,
) -> Option<u64> {
    let stream_id_key = v8::String::new(scope, "__lastResponseStreamId").unwrap();
    let stream_id_val = global.get(scope, stream_id_key.into())?;

    if stream_id_val.is_undefined() || stream_id_val.is_null() {
        return None;
    }

    stream_id_val.uint32_value(scope).map(|id| id as u64)
}

/// Extract headers from a Response object.
///
/// Headers can be either:
/// - A Headers instance (with internal _map: Map<string, string>)
/// - A plain object with string properties
fn extract_headers_from_response(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    resp_obj: v8::Local<v8::Object>,
) -> Vec<(String, String)> {
    let mut headers = vec![];
    let headers_key = v8::String::new(scope, "headers").unwrap();

    let Some(headers_val) = resp_obj.get(scope, headers_key.into()) else {
        return headers;
    };

    let Some(headers_obj) = headers_val.to_object(scope) else {
        return headers;
    };

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
                headers.push((
                    key_str.to_rust_string_lossy(scope),
                    val_str.to_rust_string_lossy(scope),
                ));
            }

            i += 2; // Map.as_array returns [key, value, key, value, ...]
        }
    } else if let Some(props) = headers_obj.get_own_property_names(scope, Default::default()) {
        // Plain object - use property iteration
        for i in 0..props.length() {
            if let Some(key_val) = props.get_index(scope, i)
                && let Some(key_str) = key_val.to_string(scope)
                && let Some(val) = headers_obj.get(scope, key_val)
                && let Some(val_str) = val.to_string(scope)
            {
                headers.push((
                    key_str.to_rust_string_lossy(scope),
                    val_str.to_rust_string_lossy(scope),
                ));
            }
        }
    }

    headers
}

/// Signal to JS that the client has disconnected.
/// Calls __signalClientDisconnect() which is always defined in setup_event_listener.
#[inline]
fn signal_client_disconnect(scope: &mut v8::ContextScope<v8::HandleScope>) {
    let global = scope.get_current_context().global(scope);
    let fn_key = v8::String::new(scope, "__signalClientDisconnect").unwrap();

    if let Some(fn_val) = global.get(scope, fn_key.into())
        && let Ok(func) = v8::Local::<v8::Function>::try_from(fn_val)
    {
        let _ = func.call(scope, global.into(), &[]);
    }
}

fn setup_env(
    runtime: &mut Runtime,
    env: &Option<std::collections::HashMap<String, String>>,
    bindings: &[openworkers_core::BindingInfo],
) -> Result<(), String> {
    // Build JSON string for env vars
    let env_json = env
        .as_ref()
        .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".to_string()))
        .unwrap_or_else(|| "{}".to_string());

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
                openworkers_core::BindingType::Worker => {
                    // Worker binding has fetch method for worker-to-worker calls
                    format!(
                        r#"{name}: {{
                            fetch: function(request) {{
                                return new Promise((resolve, reject) => {{
                                    // Convert Request to serializable format
                                    const processRequest = async () => {{
                                        let url, method, headers, body;
                                        if (typeof request === 'string') {{
                                            url = request;
                                            method = 'GET';
                                            headers = {{}};
                                            body = null;
                                        }} else if (request instanceof Request) {{
                                            url = request.url;
                                            method = request.method;
                                            headers = Object.fromEntries(request.headers.entries());
                                            body = request.body ? await request.text() : null;
                                        }} else {{
                                            url = request.url || '/';
                                            method = request.method || 'GET';
                                            headers = request.headers || {{}};
                                            body = request.body || null;
                                        }}
                                        return {{ url, method, headers, body }};
                                    }};
                                    processRequest().then((fetchOptions) => {{
                                        __nativeBindingWorker({name}, fetchOptions, (meta) => {{
                                            const stream = __createNativeStream(meta.streamId);
                                            const response = new Response(stream, {{
                                                status: meta.status,
                                                headers: meta.headers
                                            }});
                                            response.ok = meta.status >= 200 && meta.status < 300;
                                            response.statusText = meta.statusText;
                                            resolve(response);
                                        }}, reject);
                                    }}).catch(reject);
                                }});
                            }}
                        }}"#,
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

    runtime.evaluate(&WorkerCode::JavaScript(code))
}

fn setup_event_listener(runtime: &mut Runtime) -> Result<(), String> {
    let code = r#"
        // Track active response streams - worker stays alive until all streams are closed
        globalThis.__activeResponseStreams = 0;

        // Signal client disconnect to abort response streams
        // Called from Rust when consumer disconnects
        globalThis.__signalClientDisconnect = function() {
            const resp = globalThis.__lastResponse;
            if (resp?.body?._controller?._abortController) {
                const ctrl = resp.body._controller;
                if (!ctrl.signal.aborted) {
                    ctrl._abortController.abort('Client disconnected');
                }
            }
        };

        // Stream response body to Rust (stream all responses with body)
        async function __streamResponseBody(response) {
            if (!response.body) {
                return response;
            }

            // If it's a native stream (fetch forward), just mark it
            // Native streams are managed by Rust, no need to track here
            if (response.body._nativeStreamId !== undefined) {
                response._responseStreamId = response.body._nativeStreamId;
                return response;
            }

            // Stream the body: create output stream and pipe
            const streamId = __responseStreamCreate();
            response._responseStreamId = streamId;

            // Store the stream ID globally so exec() can detect cancellation
            globalThis.__lastResponseStreamId = streamId;

            // Connect the controller to the stream ID so enqueue() can detect disconnect
            // The controller is on the ReadableStream, which is response.body
            if (response.body._controller) {
                response.body._controller._responseStreamId = streamId;
            }

            // Increment active stream counter BEFORE starting async read
            globalThis.__activeResponseStreams++;

            // Helper to convert value to Uint8Array (handles strings and typed arrays)
            const encoder = new TextEncoder();
            function toUint8Array(value) {
                if (typeof value === 'string') {
                    return encoder.encode(value);
                }

                if (value instanceof Uint8Array) {
                    return value;
                }

                if (ArrayBuffer.isView(value)) {
                    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
                }

                // Fallback: convert to string then encode
                return encoder.encode(String(value));
            }

            // Read and forward asynchronously
            (async () => {
                let reader = null;
                let cancelled = false;

                try {
                    reader = response.body.getReader();

                    while (true) {
                        const { value, done } = await reader.read();

                        if (done) break;

                        if (value && value.length > 0) {
                            const chunk = toUint8Array(value);

                            // Try to write, with backpressure handling
                            while (!__responseStreamWrite(streamId, chunk)) {
                                // Check if stream was closed (client disconnected)
                                if (__responseStreamIsClosed(streamId)) {
                                    console.log('[streamResponseBody] Client disconnected, cancelling stream');
                                    cancelled = true;
                                    break;
                                }

                                // Buffer full, wait and retry
                                await new Promise(resolve => setTimeout(resolve, 1));
                            }

                            if (cancelled) break;
                        }
                    }
                } catch (error) {
                    console.error('[streamResponseBody] Error:', error);
                } finally {
                    // Cancel the reader to trigger the source's cancel() callback
                    if (reader && cancelled) {
                        try {
                            await reader.cancel('Client disconnected');
                        } catch (e) {
                            // Ignore cancel errors
                        }
                    }

                    __responseStreamEnd(streamId);
                    // Decrement counter when stream is fully consumed
                    globalThis.__activeResponseStreams--;
                }
            })();

            return response;
        }

        globalThis.addEventListener = function(type, handler) {
            if (type === 'fetch') {
                globalThis.__fetchHandler = handler;
                globalThis.__triggerFetch = function(request) {
                    // Collect promises passed to waitUntil
                    const waitUntilPromises = [];
                    let responsePromise = null;

                    const event = {
                        request: request,
                        waitUntil: function(promise) {
                            waitUntilPromises.push(Promise.resolve(promise));
                        },
                        respondWith: function(responseOrPromise) {
                            // Handle both direct Response and Promise<Response>
                            if (responseOrPromise && typeof responseOrPromise.then === 'function') {
                                responsePromise = responseOrPromise
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
                                responsePromise = __streamResponseBody(responseOrPromise)
                                    .then(response => {
                                        globalThis.__lastResponse = response;
                                    });
                            }
                        }
                    };

                    // Run async to track completion
                    (async () => {
                        try {
                            handler(event);

                            // Wait for response to be set first
                            if (responsePromise) {
                                await responsePromise;
                            }

                            // Then wait for all waitUntil promises to complete
                            if (waitUntilPromises.length > 0) {
                                await Promise.all(waitUntilPromises);
                            }
                        } catch (error) {
                            console.error('[addEventListener] Error in fetch handler:', error);
                            globalThis.__lastResponse = new Response('Handler exception: ' + (error.message || error), { status: 500 });
                        } finally {
                            globalThis.__requestComplete = true;
                        }
                    })();
                };
            } else if (type === 'scheduled') {
                globalThis.__scheduledHandler = async function(event) {
                    // Collect promises passed to waitUntil
                    const waitUntilPromises = [];

                    event.waitUntil = function(promise) {
                        waitUntilPromises.push(Promise.resolve(promise));
                    };

                    try {
                        await handler(event);

                        // Wait for all waitUntil promises to complete
                        if (waitUntilPromises.length > 0) {
                            await Promise.all(waitUntilPromises);
                        }
                    } finally {
                        globalThis.__requestComplete = true;
                    }
                };
            }
        };
    "#;

    runtime.evaluate(&WorkerCode::JavaScript(code.to_string()))
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
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.fetch === 'function') {
            const moduleHandler = globalThis.default;

            // Override __triggerFetch for ES Modules style
            globalThis.__triggerFetch = function(request) {
                // Collect promises passed to waitUntil
                const waitUntilPromises = [];

                const ctx = {
                    waitUntil: (promise) => {
                        waitUntilPromises.push(Promise.resolve(promise));
                    },
                    passThroughOnException: () => {}
                };

                // Run async and track completion separately from response
                (async () => {
                    try {
                        // ES Modules style: fetch(request, env, ctx) returns Response directly
                        const response = await moduleHandler.fetch(request, globalThis.env, ctx);

                        // Process response body for streaming
                        const processed = await __streamResponseBody(response);
                        globalThis.__lastResponse = processed;

                        // Wait for all waitUntil promises to complete (after response is set)
                        if (waitUntilPromises.length > 0) {
                            await Promise.all(waitUntilPromises);
                        }
                    } catch (error) {
                        console.error('[ES Modules] Error in fetch handler:', error);
                        globalThis.__lastResponse = new Response(
                            'Handler exception: ' + (error.message || error),
                            { status: 500 }
                        );
                    } finally {
                        globalThis.__requestComplete = true;
                    }
                })();
            };
        }

        // If export default exists but no fetch, and no addEventListener handler, create a handler that returns 501
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.fetch !== 'function' && typeof globalThis.__triggerFetch !== 'function') {
            globalThis.__triggerFetch = function(request) {
                globalThis.__lastResponse = new Response('Worker does not implement fetch handler', { status: 501 });
                globalThis.__requestComplete = true;
            };
        }

        // Same for scheduled events
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.scheduled === 'function') {
            const moduleScheduled = globalThis.default.scheduled;

            // Wrap to pass env and ctx
            globalThis.__scheduledHandler = async function(event) {
                // Collect promises passed to waitUntil
                const waitUntilPromises = [];

                const ctx = {
                    waitUntil: (promise) => {
                        waitUntilPromises.push(Promise.resolve(promise));
                    }
                };

                try {
                    await moduleScheduled(event, globalThis.env, ctx);

                    // Wait for all waitUntil promises to complete
                    if (waitUntilPromises.length > 0) {
                        await Promise.all(waitUntilPromises);
                    }
                } finally {
                    globalThis.__requestComplete = true;
                }
            };
        }

        // If export default exists but no scheduled, and no addEventListener handler, create a handler that throws
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.scheduled !== 'function' && typeof globalThis.__scheduledHandler !== 'function') {
            globalThis.__scheduledHandler = async function(event) {
                throw new Error('Worker does not implement scheduled handler');
            };
        }
    "#;

    runtime.evaluate(&WorkerCode::JavaScript(code.to_string()))
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
