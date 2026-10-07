//! Per-request isolate execution. Creates a new V8 isolate for each request.

use crate::execution_helpers::{
    AbortConfig, EventLoopExit, check_exit_condition, get_completion_state, get_response_stream_id,
    read_response_object, read_task_result, signal_client_disconnect, trigger_fetch_handler,
    trigger_task_handler,
};
use crate::runtime::{Runtime, run_event_loop};
use crate::security::{CpuEnforcer, TimeoutGuard};
use openworkers_core::{
    Event, HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script,
    TerminationReason, WorkerCode,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::AbortOnDropHandle;
use v8;

/// Worker provides per-request V8 isolate execution.
///
/// Each Worker creates a new V8 isolate, runs the JavaScript code, and destroys
/// the isolate when dropped. This provides maximum isolation but is slower than
/// pooled execution.
///
/// **For production:** Use [`crate::execute_pinned`] instead for better performance.
pub struct Worker {
    // The two handles go before `runtime`, which owns the isolate: fields drop
    // in order, and a handle has to drop while its isolate exists.
    /// The handle `dispatch` answered for the event in flight.
    pending: Option<v8::Global<v8::Object>>,
    /// The `{ fetch, task }` object src/js/dispatch.js evaluates to.
    dispatch: v8::Global<v8::Object>,
    pub(crate) runtime: Runtime,
    _event_loop_handle: AbortOnDropHandle<()>,
    aborted: Arc<AtomicBool>,
    _cancel_guard: DropGuard,
}

/// Builder for a Worker that owns a new V8 isolate. The pool runs
/// requests through [`crate::execute_pinned`] instead.
///
/// # Example
///
/// ```rust,ignore
/// let worker = Worker::builder()
///     .script(script)
///     .ops(ops)
///     .limits(limits)
///     .build()
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

        let dispatch = install_dispatch(&mut runtime.isolate, &runtime.context).map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to install the dispatch: {e}"))
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

        // Get stream_manager for event loop
        let stream_manager = runtime.stream_manager.clone();
        let cancel = CancellationToken::new();
        let event_loop_cancel = cancel.clone();

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
                event_loop_cancel,
            )
            .await;
        });

        Ok(Self {
            pending: None,
            dispatch,
            runtime,
            _event_loop_handle: AbortOnDropHandle::new(event_loop_handle),
            aborted: Arc::new(AtomicBool::new(false)),
            _cancel_guard: cancel.drop_guard(),
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

    /// Clear what the previous request left behind.
    ///
    /// A Worker keeps its isolate for its whole life, so a second `exec` would
    /// otherwise fire the previous request's timers and read its callbacks.
    /// The pooled path does the same in `ExecutionContext::reset`.
    fn reset_request_state(&mut self) -> Result<(), String> {
        self.pending = None;

        self.evaluate(
            r#"
            globalThis.__timerCallbacks.clear();
            globalThis.__intervalIds.clear();
            "#,
        )?;

        // Drops the senders of any stream the previous request abandoned, so a
        // host still holding one sees the body end instead of waiting on it.
        self.runtime.stream_manager.clear();

        // Callbacks the previous request queued belong to a response nobody
        // will read.
        while self.runtime.callback_rx.try_recv().is_ok() {}

        self.runtime.fetch_callbacks.borrow_mut().clear();
        self.runtime.fetch_error_callbacks.borrow_mut().clear();
        self.runtime.stream_callbacks.borrow_mut().clear();
        self.runtime.ws_event_callbacks.borrow_mut().clear();
        *self.runtime.fetch_response_tx.borrow_mut() = None;

        Ok(())
    }

    pub async fn exec(&mut self, mut task: Event) -> Result<(), TerminationReason> {
        // Check if aborted before starting
        if self.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        self.reset_request_state()
            .map_err(TerminationReason::Other)?;

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
        use std::future::Future;
        use std::task::Poll;

        let mut abort_signaled_at: Option<tokio::time::Instant> = None;
        let mut pending_callbacks: Vec<CallbackMessage> = Vec::with_capacity(16);

        let mut deadline = wall_guard
            .deadline()
            .map(|at| Box::pin(tokio::time::sleep_until(at)));

        std::future::poll_fn(|cx| {
            // 0. A client that hangs up is seen by the task pumping the body,
            //    which wakes this loop through the stream manager.
            self.runtime.stream_manager.register_waker(cx.waker());

            // The watchdog only sets a flag; nothing else wakes a parked loop
            // to read it, so the deadline is polled here as well.
            if let Some(sleep) = deadline.as_mut()
                && sleep.as_mut().poll(cx).is_ready()
            {
                wall_guard.expire();
                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // 1. Check termination (CPU/wall-clock guards)
            if self.is_terminated(wall_guard, cpu_guard) {
                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // 2-5. Coalesced event loop: process callbacks in a loop while
            // more work arrives during processing.
            const MAX_COALESCE_ROUNDS: usize = 4;

            for round in 0..MAX_COALESCE_ROUNDS {
                let count = match drain_and_process(cx, &mut self.runtime, &mut pending_callbacks) {
                    Ok(c) => c,
                    Err(e) => return Poll::Ready(Err(e)),
                };

                // Check exit condition with abort handling
                // CRITICAL: Wrap in explicit block to drop V8 scopes BEFORE returning Pending.
                let should_exit = {
                    use std::pin::pin;
                    let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.runtime.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);
                    let handle = self
                        .pending
                        .as_ref()
                        .map(|handle| v8::Local::new(scope, handle));

                    // Basic exit condition check
                    let base_exit = check_exit_condition(scope, handle, exit_condition);

                    // If abort detection is enabled, handle client disconnects
                    if let Some(ref config) = abort_config {
                        let (request_complete, streaming) = get_completion_state(scope, handle);

                        // Detect client disconnect and signal abort to JS
                        if streaming
                            && abort_signaled_at.is_none()
                            && let Some(stream_id) = get_response_stream_id(scope, handle)
                            && !self.runtime.stream_manager.has_sender(stream_id)
                        {
                            abort_signaled_at = Some(tokio::time::Instant::now());
                            signal_client_disconnect(scope, handle);
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

                if count == 0 || round == MAX_COALESCE_ROUNDS - 1 {
                    break;
                }
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

        // Hand the request to the guest
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            self.pending = Some(trigger_fetch_handler(
                scope,
                &self.dispatch,
                &req.url,
                req.method.as_str(),
                &req.headers,
                &mut req.body,
                body_stream_id,
            )?);
        }

        // Wait for response to be ready (no abort detection needed yet)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::ResponseReady, None)
            .await?;

        let (status, response) = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let handle = self
                .pending
                .as_ref()
                .map(|handle| v8::Local::new(scope, handle));

            read_response_object(
                scope,
                handle,
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
        {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            self.pending = Some(trigger_task_handler(scope, &self.dispatch, &task_init)?);
        }

        // Wait for handler to complete (including async work and waitUntil promises)
        // No abort detection needed for task events (no streaming response)
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::HandlerComplete, None)
            .await?;

        let task_result = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut self.runtime.isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let handle = self
                .pending
                .as_ref()
                .map(|handle| v8::Local::new(scope, handle));

            read_task_result(scope, handle)
        };

        let _ = task_init.res_tx.send(task_result);
        Ok(())
    }
}

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

/// Installs `globalThis.env` from `js/env.js`: the variables and the bindings
/// go in as JSON data, so no JavaScript is generated here.
pub(crate) fn setup_env(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
    env: &Option<std::collections::HashMap<String, String>>,
    bindings: &[openworkers_core::BindingInfo],
) -> Result<(), String> {
    use openworkers_core::BindingType;

    let vars =
        serde_json::to_string(&env.clone().unwrap_or_default()).expect("a string map serialises");

    let bindings: Vec<serde_json::Value> = bindings
        .iter()
        .map(|binding| {
            let kind = match binding.binding_type {
                BindingType::Assets => "assets",
                BindingType::Storage => "storage",
                BindingType::Kv => "kv",
                BindingType::Database => "database",
                BindingType::Worker => "worker",
                BindingType::Images => "images",
            };

            serde_json::json!({ "name": binding.name, "type": kind })
        })
        .collect();
    let bindings = serde_json::to_string(&bindings).expect("a JSON value serialises");

    let code = format!("{}({vars}, {bindings});", include_str!("js/env.js"));

    evaluate_in_context(isolate, context, &code)
}

/// Installs `addEventListener` and answers the `{ fetch, task }` object the
/// host keeps out of the guest's reach. The dispatch is openworkers-wintertc's
/// DISPATCH, shared by every engine; `js/stream_body.js` gives it V8's part,
/// the response body streaming.
pub(crate) fn install_dispatch(
    isolate: &mut v8::Isolate,
    context: &v8::Global<v8::Context>,
) -> Result<v8::Global<v8::Object>, String> {
    use std::pin::pin;

    let scope = pin!(v8::HandleScope::new(isolate));
    let mut scope = scope.init();
    let ctx = v8::Local::new(&scope, context);
    let scope = &mut v8::ContextScope::new(&mut scope, ctx);

    let code = v8::String::new(scope, openworkers_wintertc::DISPATCH).unwrap();
    let install = v8::Script::compile(scope, code, None)
        .and_then(|script| script.run(scope))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .ok_or("the wintertc dispatch does not evaluate to a function")?;

    let code = v8::String::new(scope, include_str!("js/stream_body.js")).unwrap();
    let engine = v8::Script::compile(scope, code, None)
        .and_then(|script| script.run(scope))
        .ok_or("stream_body.js does not run")?;

    let receiver = v8::undefined(scope).into();
    let dispatch = install
        .call(scope, receiver, &[engine])
        .and_then(|value| value.to_object(scope))
        .ok_or("the dispatch did not answer an object")?;

    Ok(v8::Global::new(scope, dispatch))
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
