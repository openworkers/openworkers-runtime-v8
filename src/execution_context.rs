//! Execution context - a disposable V8 context with its own event loop
//!
//! Each ExecutionContext represents one worker script execution. It creates
//! a fresh V8 Context within an existing isolate (from the thread-pinned pool),
//! providing complete isolation from other executions.
//!
//! The context is cheap to create (tens of us) compared to an isolate (few ms without snapshot).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use v8;

use crate::LockerManagedIsolate;
use crate::async_waiter::AsyncWaiter;
use crate::execution_helpers::{
    AbortConfig, EventLoopExit, ListenerMarks, check_exit_condition, get_completion_state,
    get_response_stream_id, read_marks, read_response_object, read_task_result,
    signal_client_disconnect, trigger_fetch_handler, trigger_task_handler,
};
use crate::request_context::RequestContext;
use crate::runtime::stream_manager;
use crate::runtime::{bindings, crypto, text_encoding};
use crate::security::{CpuEnforcer, TimeoutGuard, TurnGuard, next_request_id};
use openworkers_core::{
    Event, HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script,
    TerminationReason, WorkerCode,
};

/// A disposable execution context for running a worker script
///
/// This includes:
/// - Per-isolate state: isolate pointer, platform, limits, memory tracking
/// - Per-request state (via RequestContext): V8 Context, event loop, callbacks
pub struct ExecutionContext {
    /// The raw V8 pointer of `pooled`, because `v8::Isolate` is a wrapper
    /// around it and the cell a `v8::Locker` hands out dies with the guard.
    isolate: v8::UnsafeRawIsolatePtr,

    /// The pooled isolate this context runs in. `await_event_loop` takes its
    /// lock for each poll and releases it across I/O waits.
    pub(crate) pooled: Arc<LockerManagedIsolate>,

    /// Platform reference (from shared isolate)
    pub platform: &'static v8::SharedRef<v8::Platform>,

    /// Limits
    pub limits: RuntimeLimits,

    /// Set when a turn of the current event hit the heap or ArrayBuffer
    /// limit (see `Turn`).
    memory_hit: Arc<AtomicBool>,

    /// Per-request state (V8 context, channels, callbacks, streams)
    pub request: RequestContext,

    /// Fair FIFO queue for the V8 Locker; None when the isolate serves one request at a time
    pub(crate) async_waiter: Option<Arc<AsyncWaiter>>,

    /// The guards of the event exec() ran last, kept for drain_waituntil.
    budget: Option<Budget>,

    /// How the fetch listener of the event exec() ran last called respondWith.
    marks: ListenerMarks,

    /// The id of the event exec() ran last, for the isolate's turn.
    request_id: u64,
}

/// The wall clock and CPU limits of one event, from its start to the end of
/// its waitUntil work.
struct Budget {
    wall: TimeoutGuard,
    cpu: Option<CpuEnforcer>,
}

impl ExecutionContext {
    /// Create a new execution context with a pooled isolate.
    ///
    /// The isolate must already be locked with v8::Locker before calling this method.
    ///
    /// # Arguments
    /// * `isolate` - Mutable reference to the locked isolate (via v8::Locker's DerefMut)
    /// * `use_snapshot` - Whether the isolate was created with a snapshot
    /// * `platform` - V8 platform reference
    /// * `limits` - Runtime limits
    /// * `script` - Worker script to load
    /// * `ops` - Operations handle for async ops
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_pooled_isolate(
        isolate: &mut v8::Isolate,
        pooled: Arc<LockerManagedIsolate>,
        use_snapshot: bool,
        platform: &'static v8::SharedRef<v8::Platform>,
        limits: RuntimeLimits,
        script: Script,
        ops: OperationsHandle,
    ) -> Result<Self, TerminationReason> {
        // Create channels for this context
        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
        let (callback_tx, callback_rx) = mpsc::unbounded_channel();
        let callback_notify = Arc::new(Notify::new());

        let fetch_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let fetch_error_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let stream_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let ws_event_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let next_callback_id = Rc::new(RefCell::new(1));
        let fetch_response_tx = Rc::new(RefCell::new(None));
        let stream_manager = Arc::new(stream_manager::StreamManager::new());

        // Create log callback that bypasses scheduler (calls ops.handle_log directly)
        let log_callback = bindings::log_callback_from_ops(&ops);

        let slots = Rc::new(crate::context_slots::ContextSlots::default());

        // Create NEW context in the pooled isolate
        let context = {
            use std::pin::pin;

            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            crate::context_slots::attach(scope, &slots);

            // Setup global aliases (self, global) for compatibility
            bindings::setup_global_aliases(scope);

            // Always setup native bindings (not in snapshot)
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
            bindings::setup_websocket(scope, ws_event_callbacks.clone());
            crypto::setup_crypto(scope);

            // Native text encoding (can't be serialized in snapshot)
            text_encoding::setup_text_encoding_natives(scope);
            bindings::setup_url_natives(scope);
            bindings::setup_url_pattern_natives(scope);
            bindings::setup_compression_natives(scope);
            bindings::setup_navigator_natives(scope);
            bindings::setup_async_context_natives(scope);

            bindings::seal_native_namespace(scope);

            // Only setup pure JS APIs if no snapshot (they're in the snapshot)
            if !use_snapshot {
                bindings::setup_surface(scope);
            }

            // Security: Remove SharedArrayBuffer and Atomics (Spectre mitigations)
            // Must be done at context creation, not in snapshot (breaks V8 bootstrapping)
            bindings::setup_security_restrictions(scope);

            v8::Global::new(scope.as_ref(), context)
        };

        let dispatch = crate::worker::install_dispatch(isolate, &context).map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to install the dispatch: {e}"))
        })?;

        // Setup environment variables and bindings (placeholder)
        Self::setup_env(isolate, &context, &script.env, &script.bindings)?;

        // Evaluate user script (placeholder)
        let evaluated = Self::evaluate_script(isolate, &context, &script.code);

        // No turn holds the isolate here: a memory limit hit while the script
        // loads belongs to the event that loads it
        if pooled.memory_limit_hit.swap(false, Ordering::SeqCst) {
            pooled.heap_limit_state.restore(isolate);

            return Err(TerminationReason::MemoryLimit);
        }

        evaluated?;

        // Start event loop in background (with optional Operations handle)
        // Use tokio::spawn (not spawn_local) so the event loop survives LocalSet drops.
        // This is critical for warm context reuse: the LocalSet is dropped between
        // requests, but the event loop must stay alive to keep callback_tx open.
        // The event loop only does async I/O (fetch, timers, bindings) — no V8 access.
        let event_loop_stream_manager = stream_manager.clone();
        let event_loop_callback_notify = callback_notify.clone();
        let cancel = CancellationToken::new();
        let event_loop_cancel = cancel.clone();

        let event_loop_handle = tokio::spawn(async move {
            crate::runtime::run_event_loop(
                scheduler_rx,
                callback_tx,
                event_loop_callback_notify,
                event_loop_stream_manager,
                ops,
                event_loop_cancel,
            )
            .await;
        });

        // SAFETY: the isolate is locked here, and every later use re-acquires
        // the lock through `pooled` before rebuilding a `v8::Isolate`.
        let isolate_ptr = unsafe { isolate.as_raw_isolate_ptr() };

        let request = RequestContext::new(
            context,
            slots,
            dispatch,
            scheduler_tx,
            callback_rx,
            callback_notify,
            fetch_callbacks,
            fetch_error_callbacks,
            stream_callbacks,
            ws_event_callbacks,
            next_callback_id,
            fetch_response_tx,
            stream_manager,
            event_loop_handle,
            cancel,
        );

        Ok(Self {
            isolate: isolate_ptr,
            pooled,
            platform,
            limits,
            memory_hit: Arc::new(AtomicBool::new(false)),
            request,
            async_waiter: None,
            budget: None,
            marks: ListenerMarks::default(),
            request_id: 0,
        })
    }

    /// Reconstruct an ExecutionContext from a cached RequestContext (warm hit path).
    ///
    /// Used by `execute_pinned` to wrap a cached RequestContext with fresh
    /// per-isolate metadata for the next request.
    pub(crate) fn from_cached(
        isolate: v8::UnsafeRawIsolatePtr,
        pooled: Arc<LockerManagedIsolate>,
        platform: &'static v8::SharedRef<v8::Platform>,
        limits: RuntimeLimits,
        request: RequestContext,
        async_waiter: Option<Arc<AsyncWaiter>>,
    ) -> Self {
        Self {
            isolate,
            pooled,
            platform,
            limits,
            memory_hit: Arc::new(AtomicBool::new(false)),
            request,
            async_waiter,
            budget: None,
            marks: ListenerMarks::default(),
            request_id: 0,
        }
    }

    /// Consume this ExecutionContext, returning the RequestContext and isolate pointer.
    ///
    /// Used by `execute_pinned` to save the RequestContext back to the cache
    /// without aborting the event loop. The remaining EC fields (raw pointers,
    /// static refs, Arcs) are dropped normally.
    pub(crate) fn into_parts(self) -> (RequestContext, v8::UnsafeRawIsolatePtr) {
        (self.request, self.isolate)
    }

    /// Rebuild the isolate handle. Only valid while this context's lock is held.
    fn isolate(&self) -> v8::Isolate {
        // SAFETY: the pointer comes from a live isolate owned by the pool, and
        // callers hold its lock.
        unsafe { v8::Isolate::from_raw_isolate_ptr(self.isolate) }
    }

    /// Helper: Setup environment
    ///
    /// Uses the shared implementation from worker module.
    fn setup_env(
        isolate: &mut v8::Isolate,
        context: &v8::Global<v8::Context>,
        env: &Option<HashMap<String, String>>,
        bindings: &[openworkers_core::BindingInfo],
    ) -> Result<(), TerminationReason> {
        crate::worker::setup_env(isolate, context, env, bindings).map_err(|e| {
            TerminationReason::InitializationError(format!("Failed to setup env: {}", e))
        })
    }

    /// Helper: Evaluate script
    fn evaluate_script(
        isolate: &mut v8::Isolate,
        context: &v8::Global<v8::Context>,
        code: &WorkerCode,
    ) -> Result<(), TerminationReason> {
        use std::pin::pin;

        match code {
            WorkerCode::JavaScript(js) => {
                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context_local = v8::Local::new(&scope, context);
                let scope = &mut v8::ContextScope::new(&mut scope, context_local);

                let code_str = v8::String::new(scope, js).ok_or_else(|| {
                    TerminationReason::InitializationError(
                        "Failed to create script string".to_string(),
                    )
                })?;

                let tc_scope = pin!(v8::TryCatch::new(scope));
                let tc_scope = tc_scope.init();

                let script_obj = match v8::Script::compile(&tc_scope, code_str, None) {
                    Some(s) => s,
                    None => {
                        let msg = tc_scope
                            .exception()
                            .and_then(|e| e.to_string(&tc_scope))
                            .map(|s| s.to_rust_string_lossy(&tc_scope))
                            .unwrap_or_else(|| "Unknown compile error".to_string());
                        return Err(TerminationReason::Exception(format!(
                            "SyntaxError: {}",
                            msg
                        )));
                    }
                };

                match script_obj.run(&tc_scope) {
                    Some(_) => Ok(()),
                    None => {
                        let msg = tc_scope
                            .exception()
                            .and_then(|e| e.to_string(&tc_scope))
                            .map(|s| s.to_rust_string_lossy(&tc_scope))
                            .unwrap_or_else(|| "Unknown runtime error".to_string());
                        Err(TerminationReason::Exception(msg))
                    }
                }
            }
            WorkerCode::Snapshot(data) => {
                // Code cache: unpack source + bytecode, compile with ConsumeCodeCache, then run
                let (source, cache_bytes) =
                    crate::snapshot::unpack_code_cache(data).ok_or_else(|| {
                        TerminationReason::InitializationError(
                            "Failed to unpack code cache bundle".to_string(),
                        )
                    })?;

                let scope = pin!(v8::HandleScope::new(isolate));
                let mut scope = scope.init();
                let context_local = v8::Local::new(&scope, context);
                let scope = &mut v8::ContextScope::new(&mut scope, context_local);

                let code_str = v8::String::new(scope, source).ok_or_else(|| {
                    TerminationReason::InitializationError("Failed to create V8 string".to_string())
                })?;

                let cached_data = v8::script_compiler::CachedData::new(cache_bytes);
                let mut src =
                    v8::script_compiler::Source::new_with_cached_data(code_str, None, cached_data);

                let tc_scope = pin!(v8::TryCatch::new(scope));
                let tc_scope = tc_scope.init();

                let script_obj = v8::script_compiler::compile(
                    &tc_scope,
                    &mut src,
                    v8::script_compiler::CompileOptions::ConsumeCodeCache,
                    v8::script_compiler::NoCacheReason::NoReason,
                )
                .ok_or_else(|| {
                    let msg = tc_scope
                        .exception()
                        .and_then(|e| e.to_string(&tc_scope))
                        .map(|s| s.to_rust_string_lossy(&tc_scope))
                        .unwrap_or_else(|| "Failed to compile with code cache".to_string());
                    TerminationReason::Exception(msg)
                })?;

                if src.get_cached_data().is_some_and(|c| c.rejected()) {
                    tracing::warn!("Code cache rejected (V8 version mismatch?)");
                }

                match script_obj.run(&tc_scope) {
                    Some(_) => Ok(()),
                    None => {
                        let msg = tc_scope
                            .exception()
                            .and_then(|e| e.to_string(&tc_scope))
                            .map(|s| s.to_rust_string_lossy(&tc_scope))
                            .unwrap_or_else(|| "Unknown runtime error".to_string());
                        Err(TerminationReason::Exception(msg))
                    }
                }
            }
            #[allow(unreachable_patterns)]
            _ => Err(TerminationReason::InitializationError(
                "V8 runtime only supports JavaScript code".to_string(),
            )),
        }
    }

    /// Evaluate JavaScript code in this context
    pub fn evaluate(&mut self, code: &WorkerCode) -> Result<(), String> {
        use std::pin::pin;

        match code {
            WorkerCode::JavaScript(js) => {
                let mut isolate = self.isolate();
                let scope = pin!(v8::HandleScope::new(&mut isolate));
                let mut scope = scope.init();
                let context = v8::Local::new(&scope, &self.request.context);
                let scope = &mut v8::ContextScope::new(&mut scope, context);

                let source = v8::String::new(scope, js)
                    .ok_or_else(|| "Failed to create V8 string".to_string())?;

                let script = v8::Script::compile(scope, source, None)
                    .ok_or_else(|| "Failed to compile script".to_string())?;

                script
                    .run(scope)
                    .ok_or_else(|| "Script execution failed".to_string())?;

                Ok(())
            }
            WorkerCode::Snapshot(data) => {
                let (source, cache_bytes) = crate::snapshot::unpack_code_cache(data)
                    .ok_or("Failed to unpack code cache bundle")?;

                {
                    let mut isolate = self.isolate();
                    let scope = pin!(v8::HandleScope::new(&mut isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    let code_str = v8::String::new(scope, source)
                        .ok_or_else(|| "Failed to create V8 string".to_string())?;

                    let cached_data = v8::script_compiler::CachedData::new(cache_bytes);
                    let mut src = v8::script_compiler::Source::new_with_cached_data(
                        code_str,
                        None,
                        cached_data,
                    );

                    let script = v8::script_compiler::compile(
                        scope,
                        &mut src,
                        v8::script_compiler::CompileOptions::ConsumeCodeCache,
                        v8::script_compiler::NoCacheReason::NoReason,
                    )
                    .ok_or("Failed to compile with code cache")?;

                    if src.get_cached_data().is_some_and(|c| c.rejected()) {
                        tracing::warn!("Code cache rejected (V8 version mismatch?)");
                    }

                    script
                        .run(scope)
                        .ok_or_else(|| "Script execution failed".to_string())?;
                }

                Ok(())
            }
            #[allow(unreachable_patterns)]
            _ => Err("V8 runtime only supports JavaScript code".to_string()),
        }
    }

    /// Takes every callback that has arrived, runs them, then runs V8's
    /// tasks and microtasks. Polling the channel registers the waker.
    /// Answers how many callbacks ran, or an error once the channel closed.
    fn drain_and_process(
        &mut self,
        cx: &mut std::task::Context<'_>,
        pending_callbacks: &mut Vec<crate::runtime::CallbackMessage>,
    ) -> Result<usize, String> {
        loop {
            match self.request.callback_rx.poll_recv(cx) {
                std::task::Poll::Ready(Some(msg)) => pending_callbacks.push(msg),
                std::task::Poll::Ready(None) => {
                    return Err("Event loop channel closed".to_string());
                }
                std::task::Poll::Pending => break,
            }
        }

        let count = pending_callbacks.len();

        for msg in pending_callbacks.drain(..) {
            self.process_single_callback(msg);
        }

        self.pump_and_checkpoint();

        Ok(count)
    }

    /// Process pending callbacks (timers, fetch responses, etc.)
    pub fn process_callbacks(&mut self) {
        // Process our custom callbacks (timers, fetch, etc.)
        while let Ok(msg) = self.request.callback_rx.try_recv() {
            self.process_single_callback(msg);
        }

        // Pump V8 platform messages and process microtasks AFTER callbacks
        // This ensures Promise.then() handlers run immediately after resolution
        self.pump_and_checkpoint();
    }

    /// Process a single callback message in a V8 scope
    pub fn process_single_callback(&mut self, msg: crate::runtime::CallbackMessage) {
        use crate::runtime::dispatch;
        use std::pin::pin;

        let tables = dispatch::Tables {
            fetch: &self.request.fetch_callbacks,
            fetch_error: &self.request.fetch_error_callbacks,
            stream: &self.request.stream_callbacks,
            ws_event: &self.request.ws_event_callbacks,
            stream_manager: &self.request.stream_manager,
        };

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // A callback restores its own frame, if it has one; none leaks in
        bindings::clear_async_context(scope);
        dispatch::dispatch(scope, &tables, msg);

        // Note: Microtask checkpoint is NOT done here anymore.
        // It's done in pump_and_checkpoint() which is called after processing
        // all callbacks in a batch. This is more efficient.
    }

    /// Pump V8 platform messages and perform microtask checkpoint
    ///
    /// This must be called regularly to:
    /// 1. Process V8 platform messages (GC, optimizations, etc.)
    /// 2. Execute microtasks (Promise.then, async/await continuations)
    pub fn pump_and_checkpoint(&mut self) {
        use std::pin::pin;

        // Run the tasks V8 posted (GC, WebAssembly compilation)
        self.pooled.foreground.run();

        // Process microtasks (Promises, async/await) - CRITICAL for Promise resolution!
        // Without this, .then() handlers and async/await continuations never execute.
        {
            let mut isolate = self.isolate();
            let scope = pin!(v8::HandleScope::new(&mut isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.request.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            let tc_scope = pin!(v8::TryCatch::new(scope));
            let mut tc_scope = tc_scope.init();
            tc_scope.perform_microtask_checkpoint();

            // Check for exceptions during microtask processing
            if let Some(exception) = tc_scope.exception() {
                let exception_string = exception
                    .to_string(&tc_scope)
                    .map(|s| s.to_rust_string_lossy(&tc_scope))
                    .unwrap_or_else(|| "Unknown exception".to_string());
                tracing::warn!(
                    "Exception during microtask processing: {}",
                    exception_string
                );
            }
        }
    }

    /// Check exit condition with abort handling
    ///
    /// Returns true if the loop should exit.
    pub fn check_exit_with_abort(
        &mut self,
        exit_condition: EventLoopExit,
        abort_config: &Option<AbortConfig>,
        abort_signaled_at: &mut Option<tokio::time::Instant>,
    ) -> bool {
        use std::pin::pin;

        {
            let mut isolate = self.isolate();
            let scope = pin!(v8::HandleScope::new(&mut isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.request.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let handle = self
                .request
                .pending
                .as_ref()
                .map(|handle| v8::Local::new(scope, handle));

            // Basic exit condition check
            let base_exit = check_exit_condition(scope, handle, exit_condition);

            // If abort detection is enabled, handle client disconnects
            if let Some(config) = abort_config {
                let (request_complete, streaming) = get_completion_state(scope, handle);

                // Detect client disconnect and signal abort to JS
                if streaming
                    && abort_signaled_at.is_none()
                    && let Some(stream_id) = get_response_stream_id(scope, handle)
                    && !self.request.stream_manager.has_sender(stream_id)
                {
                    *abort_signaled_at = Some(tokio::time::Instant::now());
                    signal_client_disconnect(scope, handle);
                }

                // Check grace period
                let grace_exceeded = abort_signaled_at
                    .as_ref()
                    .map(|t| t.elapsed() > config.grace_period)
                    .unwrap_or(false);

                // Exit if base condition met, OR if request complete and grace exceeded
                base_exit || (request_complete && grace_exceeded)
            } else {
                base_exit
            }
        }
    }

    /// Open a cancellation scope for the task about to run. Cancelling `abort`
    /// then stops this request's in-flight ops without touching the event loop
    /// that a reused context shares with later requests.
    pub fn begin_request(&self, abort: Option<CancellationToken>) {
        let _ = self
            .request
            .scheduler_tx
            .send(crate::runtime::SchedulerMessage::BeginRequest(abort));
    }

    /// Execute a task in this context
    pub async fn exec(&mut self, mut task: Event) -> Result<(), TerminationReason> {
        self.marks = ListenerMarks::default();

        // Check if aborted before starting
        if self.request.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        // The guards stop this event only, not one that shares the isolate
        self.request_id = next_request_id();
        self.memory_hit.store(false, Ordering::SeqCst);
        let turn = Arc::clone(&self.pooled.turn);
        let wall_guard = TimeoutGuard::new(
            Arc::clone(&turn),
            self.request_id,
            self.limits.max_wall_clock_time_ms,
        );
        let cpu_guard = CpuEnforcer::new(turn, self.request_id, self.limits.max_cpu_time_ms);

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
        let outcome = self.check_termination_reason(
            result,
            cpu_guard
                .as_ref()
                .map(|g| g.was_terminated())
                .unwrap_or(false),
            wall_guard.was_triggered(),
        );

        self.restore_heap_limit();

        // The waitUntil work drain_waituntil runs next spends what is left of
        // this budget. On an error there is none to run, and the guards drop.
        if outcome.is_ok() {
            self.budget = Some(Budget {
                wall: wall_guard,
                cpu: cpu_guard,
            });
        }

        outcome
    }

    /// Drain remaining background work (waitUntil promises) after exec().
    ///
    /// After exec() returns with `StreamsComplete`, the HTTP response is sent
    /// and all response streams are closed, but waitUntil promises may still
    /// be pending. This method pumps V8 microtasks until `FullyComplete`.
    ///
    /// The work runs on what is left of the budget exec() started, so an
    /// event and its waitUntil work share one wall clock and CPU limit.
    /// Returns Ok if all background work completed, or an error if it timed out.
    pub async fn drain_waituntil(&mut self) -> Result<(), TerminationReason> {
        let Budget {
            wall: wall_guard,
            cpu: cpu_guard,
        } = match self.budget.take() {
            Some(budget) => budget,
            // No event ran, so nothing has spent a budget yet
            None => {
                self.request_id = next_request_id();
                let turn = Arc::clone(&self.pooled.turn);

                Budget {
                    wall: TimeoutGuard::new(
                        Arc::clone(&turn),
                        self.request_id,
                        self.limits.max_wall_clock_time_ms,
                    ),
                    cpu: CpuEnforcer::new(turn, self.request_id, self.limits.max_cpu_time_ms),
                }
            }
        };

        let result = self
            .await_event_loop(&wall_guard, &cpu_guard, EventLoopExit::FullyComplete, None)
            .await;

        let outcome = self.check_termination_reason(
            result.map(|_| HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::None,
            }),
            cpu_guard
                .as_ref()
                .map(|g| g.was_terminated())
                .unwrap_or(false),
            wall_guard.was_triggered(),
        );

        self.restore_heap_limit();

        outcome
    }

    /// Puts the heap limit back after an event that hit it: the callback
    /// raised it so the event could end, and V8 would keep it raised.
    fn restore_heap_limit(&self) {
        if !self.memory_hit.load(Ordering::SeqCst) {
            return;
        }

        let _lock = self.pooled.lock();
        let mut isolate = self.isolate();
        self.pooled.heap_limit_state.restore(&mut isolate);
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
        if self.memory_hit.load(Ordering::SeqCst) {
            return Err(TerminationReason::MemoryLimit);
        }

        // Check if aborted
        if self.request.aborted.load(Ordering::SeqCst) {
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

    /// Check if execution should be terminated
    ///
    /// Returns true if any termination condition is met:
    /// - V8 execution terminating
    /// - Wall-clock timeout triggered
    /// - CPU time limit exceeded
    #[inline]
    pub fn is_terminated(
        &self,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> bool {
        {
            self.isolate().is_execution_terminating()
                || self.memory_hit.load(Ordering::SeqCst)
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
    /// Uses poll_fn for true async polling instead of sleep-based polling.
    async fn await_event_loop(
        &mut self,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
        exit_condition: EventLoopExit,
        abort_config: Option<AbortConfig>,
    ) -> Result<(), String> {
        use std::future::Future;
        use std::task::Poll;

        let mut abort_signaled_at: Option<tokio::time::Instant> = None;
        let mut pending_callbacks: Vec<crate::runtime::CallbackMessage> = Vec::with_capacity(16);
        let pooled = Arc::clone(&self.pooled); // Its own handle, so the closure can borrow self
        let async_waiter = self.async_waiter.clone(); // Clone Rc (cheap) to avoid borrow on self
        let mut foreground = self.pooled.foreground.waiter();
        let request_id = self.request_id;
        let memory_hit = Arc::clone(&self.memory_hit);

        let mut deadline = wall_guard
            .deadline()
            .map(|at| Box::pin(tokio::time::sleep_until(at)));

        std::future::poll_fn(|cx| {
            // A client that hangs up is seen by the task pumping the body,
            // which wakes this loop through the stream manager.
            self.request.stream_manager.register_waker(cx.waker());
            // So does a task V8 posts from a background thread.
            foreground.register(cx.waker());

            // The watchdog only sets a flag; nothing else wakes a parked loop
            // to read it, so the deadline is polled here as well.
            if let Some(sleep) = deadline.as_mut()
                && sleep.as_mut().poll(cx).is_ready()
            {
                wall_guard.expire();
                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // -- Fair queue gate (when requests share the isolate) --
            // When multiple requests share an isolate, only one can hold the
            // V8 Locker at a time. Others wait in FIFO order.
            if let Some(ref waiter) = async_waiter
                && !waiter.try_lock(cx)
            {
                return Poll::Pending; // Not our turn, will be woken in FIFO order
            }

            // The Locker and JsLock drop when this closure returns, Pending
            // included, so the V8 mutex is free for other tasks during I/O waits.
            let _lock_guard = pooled.lock();
            let turn = TurnGuard::begin(&pooled.turn, request_id, cpu_guard.as_ref(), &memory_hit);

            // 1. Check termination (CPU/wall-clock guards)
            if self.is_terminated(wall_guard, cpu_guard) {
                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // 2-5. Coalesced event loop: process callbacks in a loop while
            // more work arrives during processing. This avoids releasing and
            // re-acquiring the V8 lock between bursts of callbacks.
            // Cap iterations to prevent starving other requests on the same isolate.
            const MAX_COALESCE_ROUNDS: usize = 4;

            for round in 0..MAX_COALESCE_ROUNDS {
                let count = match self.drain_and_process(cx, &mut pending_callbacks) {
                    Ok(c) => c,
                    Err(e) => {
                        if let Some(ref waiter) = async_waiter {
                            waiter.unlock();
                        }

                        return Poll::Ready(Err(e));
                    }
                };

                // Check exit condition after each round
                let should_exit = self.check_exit_with_abort(
                    exit_condition,
                    &abort_config,
                    &mut abort_signaled_at,
                );

                if should_exit {
                    if let Some(ref waiter) = async_waiter {
                        waiter.unlock();
                    }

                    return Poll::Ready(Ok(()));
                }

                // No callbacks processed in this round — no more work pending
                if count == 0 || round == MAX_COALESCE_ROUNDS - 1 {
                    break;
                }

                // Callbacks were processed — loop to check if more arrived
                // during processing (e.g., promise chains, microtasks)
            }

            // A guard that cut this turn's JS also cut what would wake this
            // loop again, so the request ends here
            drop(turn);
            let terminated = self.is_terminated(wall_guard, cpu_guard);

            // 6. Not done yet — release fair queue and V8 lock.
            //    V8 mutex released, other requests can run on this isolate.
            if let Some(ref waiter) = async_waiter {
                waiter.unlock();
            }

            if terminated {
                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            Poll::Pending
        })
        .await
    }

    /// Trigger a fetch event
    ///
    /// Split into per-phase V8 locks to release the V8 mutex during I/O waits.
    /// Phase 0: Setup body stream (no lock needed — pure Rust)
    /// Phase 1: Trigger fetch handler (under lock)
    /// Phase 2: Wait for response (lock-per-poll in await_event_loop)
    /// Phase 3: Read response (under lock)
    /// Phase 4: Wait for streams (lock-per-poll)
    async fn trigger_fetch_event(
        &mut self,
        fetch_init: openworkers_core::FetchInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<HttpResponse, String> {
        let mut req = fetch_init.req;

        // -- Phase 0: Setup (no lock needed — pure Rust) --
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel::<String>();

        {
            let mut tx_lock = self.request.fetch_response_tx.borrow_mut();
            *tx_lock = Some(response_tx);
        }

        let body_stream_id: Option<u64> = if matches!(&req.body, RequestBody::Stream(_)) {
            let RequestBody::Stream(rx) = std::mem::take(&mut req.body) else {
                unreachable!()
            };
            Some(self.request.stream_manager.pump_request_body(rx))
        } else {
            None
        };

        // -- Phase 1: Trigger fetch handler (fair queue + lock) --
        {
            let pooled = Arc::clone(&self.pooled);
            let async_waiter = self.async_waiter.clone();
            let request_id = self.request_id;
            let memory_hit = Arc::clone(&self.memory_hit);

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                let _lock = pooled.lock();
                let _turn =
                    TurnGuard::begin(&pooled.turn, request_id, cpu_guard.as_ref(), &memory_hit);

                use std::pin::pin;
                let result = {
                    let mut isolate = self.isolate();
                    let scope = pin!(v8::HandleScope::new(&mut isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    trigger_fetch_handler(
                        scope,
                        &self.request.dispatch,
                        &req.url,
                        req.method.as_str(),
                        &req.headers,
                        &mut req.body,
                        body_stream_id,
                    )
                    .map(|handle| {
                        self.request.pending = Some(handle);
                    })
                };

                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                std::task::Poll::Ready(result)
            })
            .await?;
        }

        // -- Phase 2: Wait for response (lock-per-poll in await_event_loop) --
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::ResponseReady, None)
            .await?;

        // -- Phase 3: Read response (fair queue + lock) --
        let ((status, response), marks) = {
            let pooled = Arc::clone(&self.pooled);
            let async_waiter = self.async_waiter.clone();
            let request_id = self.request_id;
            let memory_hit = Arc::clone(&self.memory_hit);

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                let _lock = pooled.lock();
                let _turn =
                    TurnGuard::begin(&pooled.turn, request_id, cpu_guard.as_ref(), &memory_hit);

                use std::pin::pin;
                let result = {
                    let mut isolate = self.isolate();
                    let scope = pin!(v8::HandleScope::new(&mut isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);
                    let handle = self
                        .request
                        .pending
                        .as_ref()
                        .map(|handle| v8::Local::new(scope, handle));

                    let marks = read_marks(scope, handle);

                    read_response_object(
                        scope,
                        handle,
                        &self.request.stream_manager,
                        self.limits.stream_buffer_size,
                    )
                    .map(|answer| (answer, marks))
                };

                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                std::task::Poll::Ready(result)
            })
            .await?
        };

        self.marks = marks;
        let _ = fetch_init.res_tx.send(response);

        // -- Phase 4: Wait for streams (lock-per-poll) --
        self.await_event_loop(
            wall_guard,
            cpu_guard,
            EventLoopExit::StreamsComplete,
            Some(AbortConfig::default()),
        )
        .await?;

        Ok(HttpResponse {
            status,
            headers: vec![],
            body: ResponseBody::None,
        })
    }

    /// Trigger a task event
    ///
    /// Split into per-phase V8 locks like trigger_fetch_event.
    async fn trigger_task_event(
        &mut self,
        task_init: openworkers_core::TaskInit,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> Result<(), String> {
        // -- Phase 1: Trigger task handler (fair queue + lock) --
        {
            let pooled = Arc::clone(&self.pooled);
            let async_waiter = self.async_waiter.clone();
            let request_id = self.request_id;
            let memory_hit = Arc::clone(&self.memory_hit);

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                let _lock = pooled.lock();
                let _turn =
                    TurnGuard::begin(&pooled.turn, request_id, cpu_guard.as_ref(), &memory_hit);

                use std::pin::pin;
                let result: Result<(), String> = {
                    let mut isolate = self.isolate();
                    let scope = pin!(v8::HandleScope::new(&mut isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    trigger_task_handler(scope, &self.request.dispatch, &task_init).map(|handle| {
                        self.request.pending = Some(handle);
                    })
                };

                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                std::task::Poll::Ready(result)
            })
            .await?;
        }

        // -- Phase 2: Wait for handler to complete (lock-per-poll) --
        self.await_event_loop(wall_guard, cpu_guard, EventLoopExit::HandlerComplete, None)
            .await?;

        // -- Phase 3: Read task result (fair queue + lock) --
        let task_result = {
            let pooled = Arc::clone(&self.pooled);
            let async_waiter = self.async_waiter.clone();
            let request_id = self.request_id;
            let memory_hit = Arc::clone(&self.memory_hit);

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                let _lock = pooled.lock();
                let _turn =
                    TurnGuard::begin(&pooled.turn, request_id, cpu_guard.as_ref(), &memory_hit);

                use std::pin::pin;
                let result = {
                    let mut isolate = self.isolate();
                    let scope = pin!(v8::HandleScope::new(&mut isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);
                    let handle = self
                        .request
                        .pending
                        .as_ref()
                        .map(|handle| v8::Local::new(scope, handle));

                    read_task_result(scope, handle)
                };

                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                std::task::Poll::Ready(result)
            })
            .await
        };

        let _ = task_init.res_tx.send(task_result);
        Ok(())
    }

    /// Reset per-request JS and Rust state for context reuse.
    ///
    /// Must be called between requests (warm isolate path). Clears the previous
    /// event's handle, stream state, timer callbacks, stale callbacks, and V8
    /// Global handles.
    ///
    /// Cancels any lingering `terminate_execution` flag before evaluating JS.
    /// Does NOT touch the event loop (it persists across requests).
    /// Does NOT reset `__nextTimerId` (monotonically increasing to avoid ID collisions).
    pub fn reset(&mut self) -> Result<(), String> {
        let pooled = Arc::clone(&self.pooled);
        let _lock_guard = pooled.lock();

        // 0. Cancel any lingering terminate_execution flag from a previous timeout/abort.
        // Without this, evaluate() below would fail immediately if the flag is still set.
        {
            self.isolate().cancel_terminate_execution();
        }

        // 1. Drop the previous event's handle, under the lock, its budget, and
        // its timers
        self.request.pending = None;
        self.budget = None;

        self.evaluate(&WorkerCode::JavaScript(
            r#"
            globalThis.__timerCallbacks.clear();
            globalThis.__intervalIds.clear();
            globalThis.__ow.asyncContextSet(undefined);
            "#
            .to_string(),
        ))?;

        // 2. Reset Rust-side abort flag
        self.request.aborted.store(false, Ordering::SeqCst);

        // 3. Clear stream manager (removes all senders/receivers/metadata)
        self.request.stream_manager.clear();

        // 4. Drain stale callbacks (timers/fetch from previous request)
        while self.request.callback_rx.try_recv().is_ok() {}

        // 5. Reset fetch_response_tx
        *self.request.fetch_response_tx.borrow_mut() = None;

        // 6. Clear V8 callback storage (prevents Global handle leaks)
        self.request.fetch_callbacks.borrow_mut().clear();
        self.request.fetch_error_callbacks.borrow_mut().clear();
        self.request.stream_callbacks.borrow_mut().clear();
        self.request.ws_event_callbacks.borrow_mut().clear();
        // The id counter keeps counting: an answer to the previous request that
        // lands late must find no callback, not the next request's.

        Ok(())
    }

    /// How the fetch listener of the last event called respondWith.
    pub fn listener_marks(&self) -> ListenerMarks {
        self.marks
    }

    /// Abort execution
    pub fn abort(&mut self) {
        self.request
            .aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.isolate().terminate_execution();
    }
}

// ExecutionContext is tested through execute_pinned, in tests/.
