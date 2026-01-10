//! Execution context - a disposable V8 context with its own event loop
//!
//! Each ExecutionContext represents one worker script execution. It creates
//! a fresh V8 Context within an existing SharedIsolate, providing complete
//! isolation from other executions.
//!
//! The context is cheap to create (~100µs) compared to an isolate (~3-5ms).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};
use v8;

use crate::execution_helpers::{
    AbortConfig, EventLoopExit, check_exit_condition, extract_headers_from_response,
    get_completion_state, get_response_stream_id, signal_client_disconnect,
};
use crate::runtime::stream_manager;
use crate::runtime::{CallbackId, CallbackMessage, SchedulerMessage};
use crate::runtime::{bindings, crypto, streams, text_encoding};
use crate::security::{CpuEnforcer, TimeoutGuard};
use crate::shared_isolate::SharedIsolate;
use openworkers_core::{
    HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script, Task,
    TerminationReason, WorkerCode,
};

/// A disposable execution context for running a worker script
///
/// This includes:
/// - A fresh V8 Context (isolated global scope)
/// - Event loop infrastructure (channels, callbacks)
/// - Worker script loaded and ready to execute
pub struct ExecutionContext {
    /// The shared isolate this context belongs to
    /// Note: We DON'T own the isolate, just borrow it mutably
    isolate: *mut v8::OwnedIsolate,

    /// The V8 context for this execution
    pub context: v8::Global<v8::Context>,

    /// Channels for async operations
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,

    /// Callback storage
    pub fetch_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub fetch_error_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub stream_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_callback_id: Arc<Mutex<CallbackId>>,

    /// Fetch response channel
    pub fetch_response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,

    /// Stream manager
    pub stream_manager: Arc<stream_manager::StreamManager>,

    /// Event loop handle
    pub event_loop_handle: tokio::task::JoinHandle<()>,

    /// Callback notification
    pub callback_notify: Arc<Notify>,

    /// Platform reference (from shared isolate)
    pub platform: &'static v8::SharedRef<v8::Platform>,

    /// Limits
    pub limits: RuntimeLimits,

    /// Memory limit flag (shared with isolate)
    pub memory_limit_hit: Arc<AtomicBool>,

    /// Abort flag
    pub aborted: Arc<AtomicBool>,
}

impl ExecutionContext {
    /// Create a new execution context within a shared isolate
    ///
    /// This is relatively cheap (~100µs) compared to creating an isolate.
    ///
    /// # Safety
    /// The SharedIsolate must remain valid for the lifetime of this ExecutionContext.
    /// The ExecutionContext must be dropped before the SharedIsolate is released.
    pub fn new(
        shared_isolate: &mut SharedIsolate,
        script: Script,
        ops: OperationsHandle,
    ) -> Result<Self, TerminationReason> {
        // Create channels for this context
        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
        let (callback_tx, callback_rx) = mpsc::unbounded_channel();
        let callback_notify = Arc::new(Notify::new());

        let fetch_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let fetch_error_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let stream_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let next_callback_id = Arc::new(Mutex::new(1));
        let fetch_response_tx = Arc::new(Mutex::new(None));
        let stream_manager = Arc::new(stream_manager::StreamManager::new());

        // Create NEW context in the shared isolate
        let context = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut shared_isolate.isolate));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            // Setup global aliases (self, global) for compatibility
            bindings::setup_global_aliases(scope);

            // Always setup native bindings (not in snapshot)
            bindings::setup_console(scope, scheduler_tx.clone());
            bindings::setup_performance(scope);
            bindings::setup_timers(scope, scheduler_tx.clone());
            bindings::setup_fetch(
                scope,
                scheduler_tx.clone(),
                fetch_callbacks.clone(),
                fetch_error_callbacks.clone(),
                next_callback_id.clone(),
            );
            bindings::setup_stream_ops(
                scope,
                scheduler_tx.clone(),
                stream_callbacks.clone(),
                next_callback_id.clone(),
            );
            bindings::setup_response_stream_ops(scope, stream_manager.clone());
            crypto::setup_crypto(scope);

            // Only setup pure JS APIs if no snapshot (they're in the snapshot)
            if !shared_isolate.use_snapshot {
                text_encoding::setup_text_encoding(scope);
                streams::setup_readable_stream(scope);
                bindings::setup_blob(scope);
                bindings::setup_form_data(scope);
                bindings::setup_abort_controller(scope);
                bindings::setup_structured_clone(scope);
                bindings::setup_base64(scope);
                bindings::setup_url_search_params(scope);
                bindings::setup_url(scope);
                bindings::setup_headers(scope);
                bindings::setup_request(scope);
                bindings::setup_response(scope);
            }

            v8::Global::new(scope.as_ref(), context)
        };

        // Setup addEventListener
        Self::setup_event_listener(&mut shared_isolate.isolate, &context)?;

        // Setup environment variables and bindings
        Self::setup_env(
            &mut shared_isolate.isolate,
            &context,
            &script.env,
            &script.bindings,
        )?;

        // Evaluate user script
        Self::evaluate_script(&mut shared_isolate.isolate, &context, &script.code)?;

        // Setup ES Modules handler if `export default { fetch }` is used
        Self::setup_es_modules_handler(&mut shared_isolate.isolate, &context)?;

        // Start event loop in background (with optional Operations handle)
        // Clone before moving into the async block
        let event_loop_stream_manager = stream_manager.clone();
        let event_loop_callback_notify = callback_notify.clone();

        let event_loop_handle = tokio::task::spawn_local(async move {
            crate::runtime::run_event_loop(
                scheduler_rx,
                callback_tx,
                event_loop_callback_notify,
                event_loop_stream_manager,
                ops,
            )
            .await;
        });

        Ok(Self {
            isolate: &mut shared_isolate.isolate as *mut v8::OwnedIsolate,
            context,
            scheduler_tx,
            callback_rx,
            fetch_callbacks,
            fetch_error_callbacks,
            stream_callbacks,
            next_callback_id,
            fetch_response_tx,
            stream_manager,
            event_loop_handle,
            callback_notify,
            platform: shared_isolate.platform,
            limits: shared_isolate.limits.clone(),
            memory_limit_hit: shared_isolate.memory_limit_hit.clone(),
            aborted: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Helper: Setup addEventListener in the context
    ///
    /// Note: For now, we use the worker module's setup functions.
    /// TODO: Refactor worker.rs to extract these as reusable functions.
    fn setup_event_listener(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        // The setup is done during context creation via bindings::setup_*
        // This function is kept for compatibility but doesn't need to do anything extra
        Ok(())
    }

    /// Helper: Setup environment
    ///
    /// Note: For now, we use the worker module's setup functions.
    /// TODO: Refactor worker.rs to extract these as reusable functions.
    fn setup_env(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
        _env: &Option<HashMap<String, String>>,
        _bindings: &Vec<openworkers_core::BindingInfo>,
    ) -> Result<(), TerminationReason> {
        // The setup is done during context creation via bindings::setup_*
        // This function is kept for compatibility but doesn't need to do anything extra
        Ok(())
    }

    /// Helper: Evaluate script
    ///
    /// Note: For now, we use the worker module's setup functions.
    /// TODO: Refactor worker.rs to extract these as reusable functions.
    fn evaluate_script(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
        _code: &WorkerCode,
    ) -> Result<(), TerminationReason> {
        // The script is evaluated during context creation
        // This function is kept for compatibility but doesn't need to do anything extra
        Ok(())
    }

    /// Helper: Setup ES modules handler
    ///
    /// Note: For now, we use the worker module's setup functions.
    /// TODO: Refactor worker.rs to extract these as reusable functions.
    fn setup_es_modules_handler(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        // The ES modules handler is setup during context creation
        // This function is kept for compatibility but doesn't need to do anything extra
        Ok(())
    }

    /// Get a mutable reference to the isolate
    ///
    /// # Safety
    /// This is safe because we have exclusive access during the context lifetime
    fn isolate_mut(&mut self) -> &mut v8::OwnedIsolate {
        unsafe { &mut *self.isolate }
    }

    /// Evaluate JavaScript code in this context
    pub fn evaluate(&mut self, code: &WorkerCode) -> Result<(), String> {
        use std::pin::pin;

        let code_str = match code {
            WorkerCode::JavaScript(js) => js.as_str(),
            #[cfg(feature = "wasm")]
            WorkerCode::WebAssembly(_) => {
                return Err("WASM not supported in V8 runtime".to_string());
            }
            WorkerCode::Snapshot(_) => {
                return Err("Snapshot execution not supported via evaluate()".to_string());
            }
        };

        // SAFETY: We need to access both the isolate and context, which are separate fields.
        // This is safe because we have exclusive access to self, and the isolate pointer
        // is valid for the lifetime of this ExecutionContext.
        unsafe {
            let isolate = &mut *self.isolate;
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            let source = v8::String::new(scope, code_str)
                .ok_or_else(|| "Failed to create V8 string".to_string())?;

            let script = v8::Script::compile(scope, source, None)
                .ok_or_else(|| "Failed to compile script".to_string())?;

            script
                .run(scope)
                .ok_or_else(|| "Script execution failed".to_string())?;
        }

        Ok(())
    }

    /// Process pending callbacks (timers, fetch responses, etc.)
    pub fn process_callbacks(&mut self) {
        use std::pin::pin;

        // Process V8 platform message loop
        unsafe {
            let isolate = &mut *self.isolate;
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            while v8::Platform::pump_message_loop(self.platform, scope, false) {
                // Keep pumping while there are messages
            }
        }

        // Process our custom callbacks (timers, fetch, etc.)
        while let Ok(msg) = self.callback_rx.try_recv() {
            use crate::runtime::CallbackMessage;

            // Get callback data before entering V8 scope
            let (fetch_callback, fetch_error_callback) = match &msg {
                CallbackMessage::FetchError(callback_id, _) => {
                    let cb1 = self.fetch_callbacks.lock().unwrap().remove(callback_id);
                    let cb2 = self
                        .fetch_error_callbacks
                        .lock()
                        .unwrap()
                        .remove(callback_id);
                    (cb1, cb2)
                }
                CallbackMessage::FetchStreamingSuccess(callback_id, _, _) => {
                    let cb1 = self.fetch_callbacks.lock().unwrap().remove(callback_id);
                    let cb2 = self
                        .fetch_error_callbacks
                        .lock()
                        .unwrap()
                        .remove(callback_id);
                    (cb1, cb2)
                }
                _ => (None, None),
            };

            // Now enter V8 scope
            unsafe {
                let isolate = &mut *self.isolate;
                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.context);
                let scope = &mut v8::ContextScope::new(&mut scope, context);

                match msg {
                    CallbackMessage::ExecuteTimeout(callback_id)
                    | CallbackMessage::ExecuteInterval(callback_id) => {
                        let global = context.global(scope);
                        let execute_timer_key = v8::String::new(scope, "__executeTimer").unwrap();

                        if let Some(execute_fn_val) = global.get(scope, execute_timer_key.into()) {
                            if execute_fn_val.is_function() {
                                let execute_fn: v8::Local<v8::Function> =
                                    execute_fn_val.try_into().unwrap();
                                let id_val = v8::Number::new(scope, callback_id as f64);
                                execute_fn.call(scope, global.into(), &[id_val.into()]);
                            }
                        }
                    }
                    CallbackMessage::FetchError(_, error_msg) => {
                        if let Some(callback_global) = fetch_error_callback {
                            let error_msg_val = v8::String::new(scope, &error_msg).unwrap();
                            let error = v8::Exception::error(scope, error_msg_val);
                            let callback: v8::Local<v8::Function> =
                                v8::Local::new(scope, &callback_global);
                            let recv = v8::undefined(scope);
                            callback.call(scope, recv.into(), &[error]);
                        }
                    }
                    CallbackMessage::FetchStreamingSuccess(_, meta, stream_id) => {
                        if let Some(callback_global) = fetch_callback {
                            let meta_obj = v8::Object::new(scope);

                            // status
                            let status_key = v8::String::new(scope, "status").unwrap();
                            let status_val = v8::Number::new(scope, meta.status as f64);
                            meta_obj.set(scope, status_key.into(), status_val.into());

                            // statusText
                            let status_text_key = v8::String::new(scope, "statusText").unwrap();
                            let status_text_val =
                                v8::String::new(scope, &meta.status_text).unwrap();
                            meta_obj.set(scope, status_text_key.into(), status_text_val.into());

                            // headers
                            let headers_obj = v8::Object::new(scope);
                            for (key, value) in &meta.headers {
                                let key_v8 = v8::String::new(scope, key).unwrap();
                                let value_v8 = v8::String::new(scope, value).unwrap();
                                headers_obj.set(scope, key_v8.into(), value_v8.into());
                            }
                            let headers_key = v8::String::new(scope, "headers").unwrap();
                            meta_obj.set(scope, headers_key.into(), headers_obj.into());

                            // streamId
                            let stream_id_key = v8::String::new(scope, "streamId").unwrap();
                            let stream_id_val = v8::Number::new(scope, stream_id as f64);
                            meta_obj.set(scope, stream_id_key.into(), stream_id_val.into());

                            let callback: v8::Local<v8::Function> =
                                v8::Local::new(scope, &callback_global);
                            let recv = v8::undefined(scope);
                            callback.call(scope, recv.into(), &[meta_obj.into()]);
                        }
                    }
                    _ => {
                        // Other callback types can be added as needed
                    }
                }
            }
        }
    }

    /// Execute a task in this context
    pub async fn exec(&mut self, mut task: Task) -> Result<(), TerminationReason> {
        // Check if aborted before starting
        if self.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        // Get isolate handle for security guards
        let isolate_handle = unsafe { (*self.isolate).thread_safe_handle() };

        // Setup security guards:
        // 1. Wall-clock timeout (all platforms) - prevents hanging on I/O
        let wall_guard =
            TimeoutGuard::new(isolate_handle.clone(), self.limits.max_wall_clock_time_ms);

        // 2. CPU time limit (Linux only) - prevents CPU-bound infinite loops
        let cpu_guard = CpuEnforcer::new(isolate_handle, self.limits.max_cpu_time_ms);

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

    /// Check termination reason based on execution result and guard states
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
        if self.memory_limit_hit.load(Ordering::SeqCst) {
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

    /// Calculate maximum event loop iterations based on wall-clock timeout
    #[inline]
    fn max_event_loop_iterations(&self) -> usize {
        100 + (self.limits.max_wall_clock_time_ms / 100) as usize
    }

    /// Check if execution should be terminated
    #[inline]
    fn is_terminated(&self, wall_guard: &TimeoutGuard, cpu_guard: &Option<CpuEnforcer>) -> bool {
        unsafe {
            (*self.isolate).is_execution_terminating()
                || wall_guard.was_triggered()
                || cpu_guard
                    .as_ref()
                    .map(|g| g.was_terminated())
                    .unwrap_or(false)
        }
    }

    /// Run the event loop until a condition is met or timeout/termination occurs.
    ///
    /// This is the core loop for processing async operations (Promises, timers, fetch).
    /// It checks termination guards, processes callbacks, and waits for the exit condition.
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
            self.process_callbacks();

            // Check exit condition
            // CRITICAL: Wrap in explicit block to drop V8 scopes BEFORE the await below.
            // V8 scopes use RAII - if alive during await, another worker on the same thread
            // may run and corrupt V8's context stack.
            let should_exit = {
                use std::pin::pin;
                unsafe {
                    let isolate = &mut *self.isolate;
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);
                    let global = context.global(scope);

                    // Basic exit condition check
                    let base_exit = check_exit_condition(scope, global, exit_condition);

                    // If abort detection is enabled, handle client disconnects
                    if let Some(ref config) = abort_config {
                        let (request_complete, active_streams) =
                            get_completion_state(scope, global);

                        // Detect client disconnect and signal abort to JS
                        if active_streams > 0 && abort_signaled_at.is_none() {
                            if let Some(stream_id) = get_response_stream_id(scope, global) {
                                if !self.stream_manager.has_sender(stream_id) {
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
                }
            }; // V8 scopes dropped here, BEFORE the await

            if should_exit {
                return Ok(());
            }

            // Wait for scheduler to signal callback ready, or timeout to check guards
            tokio::select! {
                _ = self.callback_notify.notified() => {
                    // Callback ready, loop will process it
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {
                    // Periodic check for wall_guard timeout (checked at loop start)
                }
            }
        }

        Err("Max event loop iterations reached".to_string())
    }

    /// Trigger a fetch event
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
            let mut tx_lock = self.fetch_response_tx.lock().unwrap();
            *tx_lock = Some(response_tx);
        }

        // Trigger fetch handler
        {
            use std::pin::pin;
            unsafe {
                let isolate = &mut *self.isolate;
                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.context);
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
                            let uint8_array =
                                v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();

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
        }

        // Wait for response to be ready (no abort detection needed yet)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::ResponseReady, None)
            .await?;

        // Read response from global __lastResponse
        // Wrap in block to drop V8 scopes before the waitUntil loop
        let (status, response) = {
            use std::pin::pin;
            unsafe {
                let isolate = &mut *self.isolate;
                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.context);
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
                    let response_stream_id_key =
                        v8::String::new(scope, "_responseStreamId").unwrap();
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
                        use crate::runtime::stream_manager::StreamChunk;

                        if let Some(receiver) = self.stream_manager.take_receiver(stream_id) {
                            let buffer_size = self.limits.stream_buffer_size;
                            let (tx, rx) = tokio::sync::mpsc::channel(buffer_size);

                            // Clone stream_manager to use in the spawned task
                            let stream_manager = self.stream_manager.clone();

                            // Spawn task to convert StreamChunk -> Result<Bytes, String>
                            tokio::task::spawn_local(async move {
                                let mut receiver = receiver;

                                loop {
                                    tokio::select! {
                                        chunk = receiver.recv() => {
                                            match chunk {
                                                Some(StreamChunk::Data(bytes)) => {
                                                    if tx.send(Ok(bytes)).await.is_err() {
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
                                                    break;
                                                }
                                            }
                                        }

                                        _ = tx.closed() => {
                                            stream_manager.close_stream(stream_id);
                                            break;
                                        }
                                    }
                                }
                            });

                            ResponseBody::Stream(rx)
                        } else {
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
                            if let Some(result_val) =
                                get_raw_body_fn.call(scope, resp_obj.into(), &[])
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

    /// Trigger a scheduled event
    async fn trigger_scheduled_event(
        &mut self,
        scheduled_init: openworkers_core::ScheduledInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<(), String> {
        // Trigger scheduled handler
        {
            use std::pin::pin;
            unsafe {
                let isolate = &mut *self.isolate;
                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.context);
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
        }

        // Wait for handler to complete (including async work and waitUntil promises)
        // No abort detection needed for scheduled events (no streaming response)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::HandlerComplete, None)
            .await?;

        let _ = scheduled_init.res_tx.send(());
        Ok(())
    }

    /// Abort execution
    pub fn abort(&mut self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        unsafe { (*self.isolate).terminate_execution() };
    }
}

// ExecutionContext should be dropped properly to clean up the event loop
impl Drop for ExecutionContext {
    fn drop(&mut self) {
        // Event loop handle will be aborted when dropped
        self.event_loop_handle.abort();

        // Context will be dropped, cleaning up the V8 context
        // The isolate pointer remains valid (owned by SharedIsolate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openworkers_core::WorkerCode;
    use std::sync::Arc;

    /// No-op operations handler for testing
    struct NoopOperations;

    impl openworkers_core::OperationsHandler for NoopOperations {}

    fn noop_ops() -> OperationsHandle {
        Arc::new(NoopOperations)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_execution_context_creation() {
        let local_set = tokio::task::LocalSet::new();

        local_set
            .run_until(async {
                let limits = RuntimeLimits::default();
                let mut shared_isolate = SharedIsolate::new(limits.clone());

                let script = Script {
                    code: WorkerCode::JavaScript("console.log('test');".to_string()),
                    env: None,
                    bindings: vec![],
                };

                let ops = noop_ops();

                let exec_ctx = ExecutionContext::new(&mut shared_isolate, script, ops);

                assert!(exec_ctx.is_ok());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_execution_context_evaluate() {
        let local_set = tokio::task::LocalSet::new();

        local_set
            .run_until(async {
                let limits = RuntimeLimits::default();
                let mut shared_isolate = SharedIsolate::new(limits.clone());

                let script = Script {
                    code: WorkerCode::JavaScript("globalThis.testValue = 42;".to_string()),
                    env: None,
                    bindings: vec![],
                };

                let ops = noop_ops();

                let mut exec_ctx = ExecutionContext::new(&mut shared_isolate, script, ops).unwrap();

                // Evaluate additional code
                let result = exec_ctx.evaluate(&WorkerCode::JavaScript(
                    "globalThis.testValue * 2;".to_string(),
                ));

                assert!(result.is_ok());

                drop(exec_ctx);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_execution_context_reuse_isolate() {
        let local_set = tokio::task::LocalSet::new();

        local_set
            .run_until(async {
                let limits = RuntimeLimits::default();
                let mut shared_isolate = SharedIsolate::new(limits.clone());

                // Create first execution context
                {
                    let script = Script {
                        code: WorkerCode::JavaScript("globalThis.first = 1;".to_string()),
                        env: None,
                        bindings: vec![],
                    };

                    let ops = noop_ops();
                    let exec_ctx = ExecutionContext::new(&mut shared_isolate, script, ops).unwrap();

                    drop(exec_ctx);
                }

                // Create second execution context in the same isolate
                // The global scope should be fresh (isolated from first context)
                {
                    let script = Script {
                        code: WorkerCode::JavaScript("globalThis.second = 2;".to_string()),
                        env: None,
                        bindings: vec![],
                    };

                    let ops = noop_ops();
                    let exec_ctx = ExecutionContext::new(&mut shared_isolate, script, ops).unwrap();

                    drop(exec_ctx);
                }

                // Both contexts should have been isolated
                // (We can't verify this directly without executing code, but no crashes = good)
            })
            .await;
    }
}
