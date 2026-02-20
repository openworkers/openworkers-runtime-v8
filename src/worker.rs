//! Per-request isolate execution. Creates a new V8 isolate for each request.

use crate::execution_helpers::{
    AbortConfig, EventLoopExit, check_exit_condition, get_completion_state, get_response_stream_id,
    read_response_object, signal_client_disconnect, trigger_fetch_handler,
};
use crate::runtime::{Runtime, run_event_loop};
use crate::security::{CpuEnforcer, TimeoutGuard};
use openworkers_core::{
    Event, HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script,
    TerminationReason, WorkerCode,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use v8;

/// Worker provides per-request V8 isolate execution.
///
/// Each Worker creates a new V8 isolate, runs the JavaScript code, and destroys
/// the isolate when dropped. This provides maximum isolation but is slower than
/// pooled execution.
///
/// **For production:** Use [`crate::execute_pooled`] instead for 1000x better performance.
pub struct Worker {
    pub(crate) runtime: Runtime,
    _event_loop_handle: tokio::task::JoinHandle<()>,
    aborted: Arc<AtomicBool>,
}

/// Builder for creating Workers with flexible configuration.
///
/// Supports two modes:
/// - **Classic**: Creates a new V8 isolate via `build()`
/// - **Pooled**: Uses an existing isolate via `execute_with_isolate()`
///
/// # Example
///
/// ```rust,ignore
/// // Classic mode
/// let worker = Worker::builder()
///     .script(script)
///     .ops(ops)
///     .limits(limits)
///     .build()
///     .await?;
///
/// // Pooled mode (executes immediately, no Worker returned)
/// Worker::builder()
///     .script(script)
///     .ops(ops)
///     .execute_with_isolate(&mut isolate, task)
///     .await?;
/// ```
pub struct WorkerBuilder {
    script: Option<Script>,
    ops: Option<OperationsHandle>,
    limits: Option<RuntimeLimits>,
}

impl WorkerBuilder {
    /// Create a new WorkerBuilder
    pub fn new() -> Self {
        Self {
            script: None,
            ops: None,
            limits: None,
        }
    }

    /// Set the script to execute
    pub fn script(mut self, script: Script) -> Self {
        self.script = Some(script);
        self
    }

    /// Set the operations handle for fetch, KV, etc.
    pub fn ops(mut self, ops: OperationsHandle) -> Self {
        self.ops = Some(ops);
        self
    }

    /// Set the runtime limits
    pub fn limits(mut self, limits: RuntimeLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Build a Worker with a new V8 isolate (classic mode)
    ///
    /// Creates a new isolate, sets up the runtime, and returns a Worker
    /// that owns the isolate.
    pub async fn build(self) -> Result<Worker, TerminationReason> {
        let script = self.script.ok_or_else(|| {
            TerminationReason::InitializationError("Script is required".to_string())
        })?;

        let ops = self.ops.ok_or_else(|| {
            TerminationReason::InitializationError("Operations handle is required".to_string())
        })?;

        Worker::new_with_ops(script, self.limits, ops).await
    }

    /// Execute a task with a pooled isolate (pooled mode)
    ///
    /// Uses the provided isolate to execute the task directly.
    /// Does NOT return a Worker - this is a one-shot execution.
    ///
    /// # Arguments
    /// * `isolate` - Mutable reference to a V8 isolate (from pool, locked via v8::Locker)
    /// * `task` - The task to execute
    ///
    /// # Returns
    /// * `Ok(())` if execution succeeded
    /// * `Err(TerminationReason)` if execution failed
    pub async fn execute_with_isolate(
        self,
        isolate: &mut v8::Isolate,
        task: Event,
    ) -> Result<(), TerminationReason> {
        use crate::runtime::stream_manager::StreamManager;
        use crate::runtime::{bindings, crypto, run_event_loop, streams, text_encoding};
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::rc::Rc;
        use tokio::sync::{Notify, mpsc};

        let script = self.script.ok_or_else(|| {
            TerminationReason::InitializationError("Script is required".to_string())
        })?;

        let ops = self.ops.ok_or_else(|| {
            TerminationReason::InitializationError("Operations handle is required".to_string())
        })?;

        let _limits = self.limits.unwrap_or_default();

        // Create channels (same as Runtime::new)
        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
        let (callback_tx, _callback_rx) = mpsc::unbounded_channel();
        let callback_notify = Arc::new(Notify::new());

        let fetch_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let fetch_error_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let stream_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let next_callback_id = Rc::new(RefCell::new(1u64));
        let stream_manager = Arc::new(StreamManager::new());

        // Create log callback that bypasses scheduler (calls ops.handle_log directly)
        let log_callback = bindings::log_callback_from_ops(&ops);

        // Check if snapshot is available
        use std::sync::OnceLock;
        static USE_SNAPSHOT: OnceLock<bool> = OnceLock::new();
        let use_snapshot = *USE_SNAPSHOT.get_or_init(|| {
            const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");
            std::path::Path::new(RUNTIME_SNAPSHOT_PATH).exists()
        });

        // Create context with bindings on the borrowed isolate
        let context = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            // Setup global aliases (self, global)
            bindings::setup_global_aliases(scope);

            // Native bindings (always needed)
            bindings::setup_console(scope, log_callback.clone());
            bindings::setup_performance(scope);
            bindings::setup_timers(scope, scheduler_tx.clone());
            bindings::setup_fetch_helpers(scope); // Must be before setup_fetch
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

            // Pure JS APIs (only if no snapshot)
            if !use_snapshot {
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

        // Use the SHARED setup functions!
        setup_event_listener(isolate, &context).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup addEventListener: {}",
                e
            ))
        })?;

        setup_env(isolate, &context, &script.env, &script.bindings).map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to setup env: {}", e))
        })?;

        // Evaluate user script
        match &script.code {
            WorkerCode::JavaScript(js) => {
                evaluate_in_context(isolate, &context, js).map_err(|e| {
                    TerminationReason::Exception(format!("Script evaluation failed: {}", e))
                })?;
            }
            WorkerCode::Snapshot(data) => {
                evaluate_code_cache_in_context(isolate, &context, data).map_err(|e| {
                    TerminationReason::Exception(format!("Script evaluation failed: {}", e))
                })?;
            }
            #[allow(unreachable_patterns)]
            _ => {
                return Err(TerminationReason::InitializationError(
                    "V8 runtime only supports JavaScript code".to_string(),
                ));
            }
        }

        setup_es_modules_handler(isolate, &context).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup ES modules handler: {}",
                e
            ))
        })?;

        // Start event loop
        let event_loop_stream_manager = stream_manager.clone();
        let event_loop_callback_notify = callback_notify.clone();

        let event_loop_handle = tokio::task::spawn_local(async move {
            run_event_loop(
                scheduler_rx,
                callback_tx,
                event_loop_callback_notify,
                event_loop_stream_manager,
                ops,
            )
            .await;
        });

        // Execute task
        let result = match task {
            Event::Task(mut init) => {
                let task_init = init.take().ok_or(TerminationReason::Other(
                    "TaskInit already consumed".to_string(),
                ))?;

                // Extract scheduled time if this is a schedule-triggered task
                let scheduled_time = match &task_init.source {
                    Some(openworkers_core::TaskSource::Schedule { time }) => Some(*time),
                    _ => None,
                };

                // Trigger task handler
                {
                    use std::pin::pin;
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let ctx = v8::Local::new(&scope, &context);
                    let scope = &mut v8::ContextScope::new(&mut scope, ctx);

                    let global = ctx.global(scope);

                    // Try __taskHandler first (new unified handler)
                    let handler_key = v8::String::new(scope, "__taskHandler").unwrap();
                    let scheduled_handler_key =
                        v8::String::new(scope, "__scheduledHandler").unwrap();

                    if let Some(handler_val) = global.get(scope, handler_key.into())
                        && handler_val.is_function()
                    {
                        let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                        let event_obj = v8::Object::new(scope);

                        // Set taskId
                        let id_key = v8::String::new(scope, "taskId").unwrap();
                        let id_val = v8::String::new(scope, &task_init.task_id).unwrap();
                        event_obj.set(scope, id_key.into(), id_val.into());

                        // Set payload if present
                        if let Some(payload) = &task_init.payload {
                            let payload_key = v8::String::new(scope, "payload").unwrap();
                            let payload_str = serde_json::to_string(payload).unwrap_or_default();
                            let payload_json = v8::String::new(scope, &payload_str).unwrap();
                            if let Some(parsed) = v8::json::parse(scope, payload_json) {
                                event_obj.set(scope, payload_key.into(), parsed);
                            }
                        }

                        // Set scheduledTime for backward compat
                        if let Some(time) = scheduled_time {
                            let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                            let time_val = v8::Number::new(scope, time as f64);
                            event_obj.set(scope, time_key.into(), time_val.into());
                        }

                        handler_fn.call(scope, global.into(), &[event_obj.into()]);
                    } else if let Some(handler_val) =
                        global.get(scope, scheduled_handler_key.into())
                        && handler_val.is_function()
                    {
                        // Fallback to __scheduledHandler for backward compat
                        let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                        let event_obj = v8::Object::new(scope);
                        if let Some(time) = scheduled_time {
                            let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                            let time_val = v8::Number::new(scope, time as f64);
                            event_obj.set(scope, time_key.into(), time_val.into());
                        }

                        handler_fn.call(scope, global.into(), &[event_obj.into()]);
                    }
                }

                // Wait for completion
                for _ in 0..100 {
                    let complete = {
                        use std::pin::pin;
                        let scope = pin!(v8::HandleScope::new(isolate));
                        let mut scope = scope.init();
                        let ctx = v8::Local::new(&scope, &context);
                        let scope = &mut v8::ContextScope::new(&mut scope, ctx);

                        let global = ctx.global(scope);
                        let key = v8::String::new(scope, "__requestComplete").unwrap();
                        global
                            .get(scope, key.into())
                            .map(|v| v.is_true())
                            .unwrap_or(false)
                    };

                    if complete {
                        // Read __taskResult from JS
                        let task_result = {
                            use std::pin::pin;
                            let scope = pin!(v8::HandleScope::new(isolate));
                            let mut scope = scope.init();
                            let ctx = v8::Local::new(&scope, &context);
                            let scope = &mut v8::ContextScope::new(&mut scope, ctx);

                            let global = ctx.global(scope);
                            let result_key = v8::String::new(scope, "__taskResult").unwrap();

                            if let Some(result_val) = global.get(scope, result_key.into()) {
                                if result_val.is_object() {
                                    let result_obj: v8::Local<v8::Object> =
                                        result_val.try_into().unwrap();

                                    let success_key = v8::String::new(scope, "success").unwrap();
                                    let success = result_obj
                                        .get(scope, success_key.into())
                                        .map(|v| v.is_true())
                                        .unwrap_or(true);

                                    let data_key = v8::String::new(scope, "data").unwrap();
                                    let data =
                                        result_obj.get(scope, data_key.into()).and_then(|v| {
                                            if v.is_undefined() || v.is_null() {
                                                None
                                            } else {
                                                let json_str = v8::json::stringify(scope, v)?;
                                                let json_string =
                                                    json_str.to_rust_string_lossy(scope);
                                                serde_json::from_str(&json_string).ok()
                                            }
                                        });

                                    let error_key = v8::String::new(scope, "error").unwrap();
                                    let error =
                                        result_obj.get(scope, error_key.into()).and_then(|v| {
                                            if v.is_undefined() || v.is_null() {
                                                None
                                            } else {
                                                Some(v.to_rust_string_lossy(scope))
                                            }
                                        });

                                    openworkers_core::TaskResult {
                                        success,
                                        data,
                                        error,
                                    }
                                } else {
                                    openworkers_core::TaskResult::success()
                                }
                            } else {
                                openworkers_core::TaskResult::success()
                            }
                        };

                        let _ = task_init.res_tx.send(task_result);
                        break;
                    }

                    tokio::select! {
                        _ = callback_notify.notified() => {}
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {}
                    }
                }

                Ok(())
            }
            Event::Fetch(_) => Err(TerminationReason::Other(
                "Fetch not yet implemented for pooled mode".to_string(),
            )),
        };

        // Cleanup
        event_loop_handle.abort();

        result
    }
}

impl Default for WorkerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Worker {
    /// Create a new WorkerBuilder for flexible Worker construction
    pub fn builder() -> WorkerBuilder {
        WorkerBuilder::new()
    }

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
        // Create log callback that bypasses scheduler (calls ops.handle_log directly)
        let log_callback = crate::runtime::bindings::log_callback_from_ops(&ops);

        // With unsafe-worker-snapshot feature: extract heap snapshot if present
        // (isolate will be created from it). Code cache bundles are handled during evaluate().
        #[cfg(feature = "unsafe-worker-snapshot")]
        let worker_snapshot = match &script.code {
            WorkerCode::Snapshot(data) if !crate::snapshot::is_code_cache(data) => {
                Some(data.clone())
            }
            _ => None,
        };

        let (mut runtime, scheduler_rx, callback_tx, callback_notify) = Runtime::new(
            limits,
            log_callback,
            #[cfg(feature = "unsafe-worker-snapshot")]
            worker_snapshot,
        );

        // Setup addEventListener
        setup_event_listener(&mut runtime.isolate, &runtime.context).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup addEventListener: {}",
                e
            ))
        })?;

        // Setup environment variables and bindings
        setup_env(
            &mut runtime.isolate,
            &runtime.context,
            &script.env,
            &script.bindings,
        )
        .map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to setup env: {}", e))
        })?;

        // Evaluate user script
        runtime.evaluate(&script.code).map_err(|e| {
            TerminationReason::Exception(format!("Script evaluation failed: {}", e))
        })?;

        // Setup ES Modules handler if `export default { fetch }` is used
        // This takes priority over addEventListener('fetch', ...)
        setup_es_modules_handler(&mut runtime.isolate, &runtime.context).map_err(|e| {
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

    pub async fn exec(&mut self, mut task: Event) -> Result<(), TerminationReason> {
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
            Event::Fetch(ref mut init) => {
                let fetch_init = init.take().ok_or(TerminationReason::Other(
                    "FetchInit already consumed".to_string(),
                ))?;
                self.trigger_fetch_event(fetch_init, &wall_guard, &cpu_guard)
                    .await
            }
            Event::Task(ref mut init) => {
                let task_init = init.take().ok_or(TerminationReason::Other(
                    "TaskInit already consumed".to_string(),
                ))?;
                self.trigger_task_event(task_init, &wall_guard, &cpu_guard)
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
    /// Uses poll_fn for true async polling instead of sleep-based polling.
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
        use crate::event_loop::drain_and_process;
        use crate::runtime::CallbackMessage;
        use std::task::Poll;

        let mut abort_signaled_at: Option<tokio::time::Instant> = None;
        let mut pending_callbacks: Vec<CallbackMessage> = Vec::with_capacity(16);

        std::future::poll_fn(|cx| {
            // 1. Check termination (CPU/wall-clock guards)
            if self.is_terminated(wall_guard, cpu_guard) {
                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // 2-4. Drain callbacks, process, pump V8
            if let Err(e) = drain_and_process(cx, &mut self.runtime, &mut pending_callbacks) {
                return Poll::Ready(Err(e));
            }

            // 5. Check exit condition with abort handling
            // CRITICAL: Wrap in explicit block to drop V8 scopes BEFORE returning Pending.
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
                    if active_streams > 0
                        && abort_signaled_at.is_none()
                        && let Some(stream_id) = get_response_stream_id(scope, global)
                        && !self.runtime.stream_manager.has_sender(stream_id)
                    {
                        abort_signaled_at = Some(tokio::time::Instant::now());
                        signal_client_disconnect(scope);
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
            }; // V8 scopes dropped here

            if should_exit {
                return Poll::Ready(Ok(()));
            }

            // 6. Not done yet - waker registered via poll_recv
            Poll::Pending
        })
        .await
    }

    async fn trigger_fetch_event(
        &mut self,
        fetch_init: openworkers_core::FetchInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<HttpResponse, String> {
        let mut req = fetch_init.req;

        // Create channel for response notification (like JSC)
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel::<String>();

        // Store the sender in runtime so JS can use it
        {
            let mut tx_lock = self.runtime.fetch_response_tx.borrow_mut();
            *tx_lock = Some(response_tx);
        }

        // Handle streaming request body - set up pump before entering V8
        // Note: Only take the body if it's a Stream, otherwise leave it for later
        let body_stream_id: Option<u64> = if matches!(&req.body, RequestBody::Stream(_)) {
            let RequestBody::Stream(rx) = std::mem::take(&mut req.body) else {
                unreachable!()
            };
            Some(self.runtime.stream_manager.pump_request_body(rx))
        } else {
            None
        };

        // Trigger fetch handler using shared helper
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            trigger_fetch_handler(
                scope,
                &req.url,
                req.method.as_str(),
                &req.headers,
                &mut req.body,
                body_stream_id,
            )?;
        }

        // Wait for response to be ready (no abort detection needed yet)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::ResponseReady, None)
            .await?;

        // Read response from global __lastResponse using shared helper
        let (status, response) = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            read_response_object(
                scope,
                &self.runtime.stream_manager,
                self.runtime.limits.stream_buffer_size,
            )?
        };

        let _ = fetch_init.res_tx.send(response);

        // Wait for waitUntil promises AND active response streams to complete.
        // Worker (oneshot) pumps V8 microtasks here — without this, JS callbacks
        // from timers/promises would never execute. For warm reuse (ExecutionContext),
        // StreamsComplete is used instead and background work is drained separately.
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

    async fn trigger_task_event(
        &mut self,
        task_init: openworkers_core::TaskInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<(), String> {
        // Extract scheduled time if this is a schedule-triggered task
        let scheduled_time = match &task_init.source {
            Some(openworkers_core::TaskSource::Schedule { time }) => Some(*time),
            _ => None,
        };

        // Trigger task handler
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            let global = context.global(scope);

            // Try __taskHandler first (new unified handler)
            let task_handler_key = v8::String::new(scope, "__taskHandler").unwrap();
            let scheduled_handler_key = v8::String::new(scope, "__scheduledHandler").unwrap();

            if let Some(handler_val) = global.get(scope, task_handler_key.into())
                && handler_val.is_function()
            {
                let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                // Create event object with full task info
                let event_obj = v8::Object::new(scope);

                // Set taskId
                let id_key = v8::String::new(scope, "taskId").unwrap();
                let id_val = v8::String::new(scope, &task_init.task_id).unwrap();
                event_obj.set(scope, id_key.into(), id_val.into());

                // Set attempt
                let attempt_key = v8::String::new(scope, "attempt").unwrap();
                let attempt_val = v8::Number::new(scope, task_init.attempt as f64);
                event_obj.set(scope, attempt_key.into(), attempt_val.into());

                // Set payload if present
                if let Some(payload) = &task_init.payload {
                    let payload_key = v8::String::new(scope, "payload").unwrap();
                    let payload_str = serde_json::to_string(payload).unwrap_or_default();
                    let payload_json = v8::String::new(scope, &payload_str).unwrap();
                    if let Some(parsed) = v8::json::parse(scope, payload_json) {
                        event_obj.set(scope, payload_key.into(), parsed);
                    }
                }

                // Set scheduledTime for backward compat
                if let Some(time) = scheduled_time {
                    let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                    let time_val = v8::Number::new(scope, time as f64);
                    event_obj.set(scope, time_key.into(), time_val.into());
                }

                let result = handler_fn.call(scope, global.into(), &[event_obj.into()]);

                if result.is_none() {
                    return Err("Execution terminated".to_string());
                }
            } else if let Some(handler_val) = global.get(scope, scheduled_handler_key.into())
                && handler_val.is_function()
            {
                // Fallback to __scheduledHandler for backward compat
                let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                let event_obj = v8::Object::new(scope);
                if let Some(time) = scheduled_time {
                    let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                    let time_val = v8::Number::new(scope, time as f64);
                    event_obj.set(scope, time_key.into(), time_val.into());
                }

                let result = handler_fn.call(scope, global.into(), &[event_obj.into()]);

                if result.is_none() {
                    return Err("Execution terminated".to_string());
                }
            }
        }

        // Wait for handler to complete (including async work and waitUntil promises)
        // No abort detection needed for task events (no streaming response)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::HandlerComplete, None)
            .await?;

        // Read __taskResult from JS and send it back
        let task_result = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            let global = context.global(scope);
            let result_key = v8::String::new(scope, "__taskResult").unwrap();

            if let Some(result_val) = global.get(scope, result_key.into()) {
                if result_val.is_object() {
                    let result_obj: v8::Local<v8::Object> = result_val.try_into().unwrap();

                    // Extract success
                    let success_key = v8::String::new(scope, "success").unwrap();
                    let success = result_obj
                        .get(scope, success_key.into())
                        .map(|v| v.is_true())
                        .unwrap_or(true);

                    // Extract data (serialize to JSON)
                    let data_key = v8::String::new(scope, "data").unwrap();
                    let data = result_obj.get(scope, data_key.into()).and_then(|v| {
                        if v.is_undefined() || v.is_null() {
                            None
                        } else {
                            let json_str = v8::json::stringify(scope, v)?;
                            let json_string = json_str.to_rust_string_lossy(scope);
                            serde_json::from_str(&json_string).ok()
                        }
                    });

                    // Extract error
                    let error_key = v8::String::new(scope, "error").unwrap();
                    let error = result_obj.get(scope, error_key.into()).and_then(|v| {
                        if v.is_undefined() || v.is_null() {
                            None
                        } else {
                            Some(v.to_rust_string_lossy(scope))
                        }
                    });

                    openworkers_core::TaskResult {
                        success,
                        data,
                        error,
                    }
                } else {
                    openworkers_core::TaskResult::success()
                }
            } else {
                openworkers_core::TaskResult::success()
            }
        };

        let _ = task_init.res_tx.send(task_result);
        Ok(())
    }
}

// Helper functions are now in execution_helpers module

/// Evaluate JavaScript code in a V8 context
///
/// This is the shared helper used by all setup functions.
/// Works with both owned isolates (Worker mode) and borrowed isolates (pooled mode).
pub(crate) fn evaluate_in_context(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
    code: &str,
) -> Result<(), String> {
    use std::pin::pin;

    let scope = pin!(v8::HandleScope::new(isolate));
    let mut scope = scope.init();
    let ctx = v8::Local::new(&scope, context);
    let scope = &mut v8::ContextScope::new(&mut scope, ctx);

    let code_str = v8::String::new(scope, code).ok_or("Failed to create V8 string")?;

    let tc = pin!(v8::TryCatch::new(scope));
    let tc = tc.init();

    let script_obj = v8::Script::compile(&tc, code_str, None).ok_or_else(|| {
        tc.exception()
            .and_then(|e| e.to_string(&tc).map(|s| s.to_rust_string_lossy(&tc)))
            .unwrap_or_else(|| "Compile error".to_string())
    })?;

    script_obj.run(&tc).ok_or_else(|| {
        tc.exception()
            .and_then(|e| e.to_string(&tc).map(|s| s.to_rust_string_lossy(&tc)))
            .unwrap_or_else(|| "Runtime error".to_string())
    })?;

    Ok(())
}

/// Evaluate a packed code cache bundle (source + bytecode) in a V8 context.
///
/// Uses `v8::script_compiler::compile` with `ConsumeCodeCache` to skip parse+compile,
/// then runs the resulting script.
pub(crate) fn evaluate_code_cache_in_context(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
    data: &[u8],
) -> Result<(), String> {
    use std::pin::pin;

    let (source, cache_bytes) =
        crate::snapshot::unpack_code_cache(data).ok_or("Failed to unpack code cache bundle")?;

    let scope = pin!(v8::HandleScope::new(isolate));
    let mut scope = scope.init();
    let ctx = v8::Local::new(&scope, context);
    let scope = &mut v8::ContextScope::new(&mut scope, ctx);

    let code_str = v8::String::new(scope, source).ok_or("Failed to create V8 string")?;
    let cached_data = v8::script_compiler::CachedData::new(cache_bytes);
    let mut src = v8::script_compiler::Source::new_with_cached_data(code_str, None, cached_data);

    let tc = pin!(v8::TryCatch::new(scope));
    let tc = tc.init();

    let script_obj = v8::script_compiler::compile(
        &tc,
        &mut src,
        v8::script_compiler::CompileOptions::ConsumeCodeCache,
        v8::script_compiler::NoCacheReason::NoReason,
    )
    .ok_or_else(|| {
        tc.exception()
            .and_then(|e| e.to_string(&tc).map(|s| s.to_rust_string_lossy(&tc)))
            .unwrap_or_else(|| "Failed to compile with code cache".to_string())
    })?;

    if src.get_cached_data().is_some_and(|c| c.rejected()) {
        tracing::warn!("Code cache rejected (V8 version mismatch?)");
    }

    script_obj.run(&tc).ok_or_else(|| {
        tc.exception()
            .and_then(|e| e.to_string(&tc).map(|s| s.to_rust_string_lossy(&tc)))
            .unwrap_or_else(|| "Runtime error".to_string())
    })?;

    Ok(())
}

pub(crate) fn setup_env(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
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
                    // Supports Request, URL, and string inputs (like standard fetch)
                    format!(
                        r#"{name}: {{
                            fetch: function(input, options) {{
                                return new Promise((resolve, reject) => {{
                                    const {{ url, method, headers, body }} = __normalizeFetchInput(input, options);
                                    const fetchOptions = {{ url, method, headers, body }};
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
                    // Storage binding has get/put/head/list/delete/fetch
                    format!(
                        r#"{name}: {{
                            get: (key) => __bindingCall(__nativeBindingStorage, {name}, 'get', {{ key }})
                                .then(r => r.body ? new TextDecoder().decode(r.body) : null),
                            put: (key, value) => {{
                                const body = typeof value === 'string' ? new TextEncoder().encode(value) : value;
                                return __bindingCall(__nativeBindingStorage, {name}, 'put', {{ key, body }}).then(() => {{}});
                            }},
                            head: (key) => __bindingCall(__nativeBindingStorage, {name}, 'head', {{ key }})
                                .then(r => ({{ size: r.size, etag: r.etag }})),
                            list: (options) => __bindingCall(__nativeBindingStorage, {name}, 'list', {{ prefix: options?.prefix, limit: options?.limit }})
                                .then(r => ({{ keys: r.keys, truncated: r.truncated }})),
                            delete: (key) => __bindingCall(__nativeBindingStorage, {name}, 'delete', {{ key }}).then(() => {{}}),
                            fetch: function(input, options) {{
                                const {{ url }} = __normalizeFetchInput(input, options);
                                const key = new URL(url, 'http://localhost').pathname;
                                return __bindingCall(__nativeBindingStorage, {name}, 'fetch', {{ key }})
                                    .then(r => {{
                                        const body = r.streamId !== undefined
                                            ? __createNativeStream(r.streamId)
                                            : r.body;
                                        return new Response(body, {{ status: r.status, headers: r.headers }});
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
                            const __n = {name};
                            return {{
                                get: (key) => __bindingCall(__nativeBindingKv, __n, 'get', {{ key }}).then(r => r.value),
                                put: (key, value, options) => {{
                                    const params = {{ key, value }};
                                    if (options?.expiresIn) params.expiresIn = options.expiresIn;
                                    return __bindingCall(__nativeBindingKv, __n, 'put', params).then(() => {{}});
                                }},
                                delete: (key) => __bindingCall(__nativeBindingKv, __n, 'delete', {{ key }}).then(() => {{}}),
                                list: (options) => {{
                                    const params = {{}};
                                    if (options?.prefix) params.prefix = options.prefix;
                                    if (options?.limit) params.limit = options.limit;
                                    return __bindingCall(__nativeBindingKv, __n, 'list', params).then(r => r.keys);
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
                            const __n = {name};
                            return {{
                                query: (sql, params) => __bindingCall(__nativeBindingDatabase, __n, 'query', {{ sql, params: params || [] }}).then(r => r.rows)
                            }};
                        }})()"#,
                    )
                }
                openworkers_core::BindingType::Worker => {
                    // Worker binding has fetch method for worker-to-worker calls
                    // Supports Request, URL, and string inputs (like standard fetch)
                    format!(
                        r#"{name}: {{
                            fetch: function(input, options) {{
                                return new Promise((resolve, reject) => {{
                                    const processRequest = async () => {{
                                        const normalized = __normalizeFetchInput(input, options);
                                        // Buffer ReadableStream body for serialization
                                        normalized.body = await __bufferBody(normalized.body);
                                        return normalized;
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

    evaluate_in_context(isolate, context, &code)
}

pub(crate) fn setup_event_listener(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
) -> Result<(), String> {
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

        // Stream response body to Rust (only for true streaming responses)
        async function __streamResponseBody(response) {
            if (!response.body) {
                return response;
            }

            // If it's a native stream (fetch forward), just mark it
            // Native streams are managed by Rust, no need to track here
            if (response.body._nativeStreamId !== undefined) {
                response._responseStreamId = response.body._nativeStreamId;
                globalThis.__lastResponseStreamId = response.body._nativeStreamId;
                return response;
            }

            // Check if this is a buffered response (created from string/Uint8Array/ArrayBuffer)
            // These have _isBuffered = true, set by the Response constructor
            // For these, skip streaming and let Rust use _getRawBody() instead
            if (response._isBuffered) {
                // Buffered response - data already available, no need to stream
                return response;
            }

            // True streaming response - create output stream and pipe
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
                            // Call handler and capture return value
                            const result = handler(event);

                            // If handler returns a Response or Promise<Response>, use it
                            // ONLY if respondWith() was not already called (respondWith has priority)
                            // (Service Worker / Cloudflare Workers compatibility)
                            if (!responsePromise && result instanceof Response) {
                                responsePromise = __streamResponseBody(result)
                                    .then(response => {
                                        globalThis.__lastResponse = response;
                                    });
                            } else if (!responsePromise && result && typeof result.then === 'function') {
                                // Handler returned a Promise - could be Promise<Response>
                                responsePromise = result
                                    .then(response => {
                                        if (response instanceof Response) {
                                            return __streamResponseBody(response)
                                                .then(processed => {
                                                    globalThis.__lastResponse = processed;
                                                });
                                        }
                                    })
                                    .catch(error => {
                                        console.error('[addEventListener] Handler promise rejected:', error);
                                        globalThis.__lastResponse = new Response(
                                            'Handler promise rejected: ' + (error.message || error),
                                            { status: 500 }
                                        );
                                    });
                            }

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
            } else if (type === 'task') {
                globalThis.__taskHandler = async function(event) {
                    // Collect promises passed to waitUntil
                    const waitUntilPromises = [];

                    // Default result (success with no data)
                    globalThis.__taskResult = { success: true };

                    event.waitUntil = function(promise) {
                        waitUntilPromises.push(Promise.resolve(promise));
                    };

                    event.respondWith = function(result) {
                        if (result && typeof result === 'object') {
                            globalThis.__taskResult = {
                                success: result.success !== false,
                                data: result.data,
                                error: result.error
                            };
                        } else {
                            globalThis.__taskResult = { success: true, data: result };
                        }
                    };

                    try {
                        const result = await handler(event);

                        // If handler returns a value and respondWith wasn't called, use it
                        if (result !== undefined && globalThis.__taskResult.data === undefined) {
                            if (result && typeof result === 'object' && 'success' in result) {
                                globalThis.__taskResult = {
                                    success: result.success !== false,
                                    data: result.data,
                                    error: result.error
                                };
                            } else {
                                globalThis.__taskResult = { success: true, data: result };
                            }
                        }

                        // Wait for all waitUntil promises to complete
                        if (waitUntilPromises.length > 0) {
                            await Promise.all(waitUntilPromises);
                        }
                    } catch (error) {
                        console.error('[task] Handler error:', error);
                        globalThis.__taskResult = {
                            success: false,
                            error: error.message || String(error)
                        };
                    } finally {
                        globalThis.__requestComplete = true;
                    }
                };
            }
        };
    "#;

    evaluate_in_context(isolate, context, code)
}

/// Setup ES Modules handler if `export default { fetch }` is used
///
/// This checks if the user script exported a default object with a fetch method.
/// If so, it overrides __triggerFetch to use the ES Modules style (direct return)
/// instead of the Service Worker style (event.respondWith).
///
/// ES Modules style takes priority over addEventListener.
pub(crate) fn setup_es_modules_handler(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
) -> Result<(), String> {
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

        // Same for task events (ES modules style)
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.task === 'function') {
            const moduleTask = globalThis.default.task;

            // Wrap to pass env and ctx, and handle return value
            globalThis.__taskHandler = async function(event) {
                const waitUntilPromises = [];
                globalThis.__taskResult = { success: true };

                event.waitUntil = function(promise) {
                    waitUntilPromises.push(Promise.resolve(promise));
                };

                event.respondWith = function(result) {
                    if (result && typeof result === 'object') {
                        globalThis.__taskResult = {
                            success: result.success !== false,
                            data: result.data,
                            error: result.error
                        };
                    } else {
                        globalThis.__taskResult = { success: true, data: result };
                    }
                };

                const ctx = {
                    waitUntil: event.waitUntil
                };

                try {
                    const result = await moduleTask(event, globalThis.env, ctx);

                    // If handler returns a value and respondWith wasn't called, use it
                    if (result !== undefined && globalThis.__taskResult.data === undefined) {
                        if (result && typeof result === 'object' && 'success' in result) {
                            globalThis.__taskResult = {
                                success: result.success !== false,
                                data: result.data,
                                error: result.error
                            };
                        } else {
                            globalThis.__taskResult = { success: true, data: result };
                        }
                    }

                    // Wait for all waitUntil promises
                    if (waitUntilPromises.length > 0) {
                        await Promise.all(waitUntilPromises);
                    }
                } catch (e) {
                    globalThis.__taskResult = {
                        success: false,
                        error: e.message || String(e)
                    };
                } finally {
                    globalThis.__requestComplete = true;
                }
            };
        }

        // If export default exists but no task, and no addEventListener handler, create a fallback
        if (typeof globalThis.default === 'object' && globalThis.default !== null && typeof globalThis.default.task !== 'function' && typeof globalThis.__taskHandler !== 'function') {
            // Fall back to scheduled handler if it exists (backward compat)
            if (typeof globalThis.__scheduledHandler === 'function') {
                globalThis.__taskHandler = globalThis.__scheduledHandler;
            }
        }

        // Final fallback: if __triggerFetch is still not defined (no addEventListener, no valid export default),
        // create a 501 handler. This handles cases like:
        // - globalThis.default = null
        // - globalThis.default = 42
        // - globalThis.default = "string"
        // - No handler defined at all
        if (typeof globalThis.__triggerFetch !== 'function') {
            globalThis.__triggerFetch = function(request) {
                globalThis.__lastResponse = new Response('Worker does not implement fetch handler', { status: 501 });
                globalThis.__requestComplete = true;
            };
        }
    "#;

    evaluate_in_context(isolate, context, code)
}

impl openworkers_core::Worker for Worker {
    async fn new(script: Script, limits: Option<RuntimeLimits>) -> Result<Self, TerminationReason> {
        Worker::new(script, limits).await
    }

    async fn exec(&mut self, task: Event) -> Result<(), TerminationReason> {
        Worker::exec(self, task).await
    }

    fn abort(&mut self) {
        Worker::abort(self)
    }
}
