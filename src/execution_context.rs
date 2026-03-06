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
    AbortConfig, EventLoopExit, check_exit_condition, get_completion_state, get_response_stream_id,
    read_response_object, signal_client_disconnect, trigger_fetch_handler,
};
use crate::request_context::RequestContext;
use crate::runtime::stream_manager;
use crate::runtime::{bindings, crypto, streams, text_encoding};
use crate::security::{CpuEnforcer, TimeoutGuard};
use openworkers_core::{
    Event, HttpResponse, OperationsHandle, RequestBody, ResponseBody, RuntimeLimits, Script,
    TerminationReason, WorkerCode,
};

/// Acquire the V8 lock for a pooled isolate (null check + deref + lock).
///
/// Returns `None` for the Worker path (null pointer = OwnedIsolate, auto-entered).
///
/// # Safety
///
/// When non-null, `lmi_ptr` must point to a valid `LockerManagedIsolate` that
/// outlives the returned Locker. This is guaranteed by the pool architecture:
/// the LMI is pinned to its thread and outlives all ExecutionContexts.
unsafe fn try_lock_v8(
    lmi_ptr: *mut LockerManagedIsolate,
) -> Option<(v8::Locker<'static>, crate::gc::JsLock)> {
    if lmi_ptr.is_null() {
        return None;
    }

    // SAFETY: caller guarantees lmi_ptr is valid when non-null
    let lmi = unsafe { &mut *lmi_ptr };
    Some(lmi.lock())
}

/// A disposable execution context for running a worker script
///
/// This includes:
/// - Per-isolate state: isolate pointer, platform, limits, memory tracking
/// - Per-request state (via RequestContext): V8 Context, event loop, callbacks
pub struct ExecutionContext {
    /// The shared isolate this context belongs to
    /// Note: We DON'T own the isolate, just borrow it mutably
    isolate: *mut v8::OwnedIsolate,

    /// Pointer to the LockerManagedIsolate for on-demand Locker creation.
    /// Non-null for both pool modes (simple and multiplexed).
    /// Null only for the Worker path (OwnedIsolate, auto-entered).
    /// When non-null, await_event_loop acquires/releases V8 locker per poll cycle.
    pub(crate) lmi_ptr: *mut LockerManagedIsolate,

    /// Platform reference (from shared isolate)
    pub platform: &'static v8::SharedRef<v8::Platform>,

    /// Limits
    pub limits: RuntimeLimits,

    /// Memory limit flag (shared with isolate)
    pub memory_limit_hit: Arc<AtomicBool>,

    /// Per-request state (V8 context, channels, callbacks, streams)
    pub request: RequestContext,

    /// Fair FIFO queue for V8 Locker when multiplexing (None for simple pool path)
    pub(crate) async_waiter: Option<Arc<AsyncWaiter>>,
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
    /// * `memory_limit_hit` - Memory limit tracking flag
    /// * `script` - Worker script to load
    /// * `ops` - Operations handle for async ops
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_pooled_isolate(
        isolate: &mut v8::Isolate,
        lmi_ptr: *mut LockerManagedIsolate,
        use_snapshot: bool,
        platform: &'static v8::SharedRef<v8::Platform>,
        limits: RuntimeLimits,
        memory_limit_hit: Arc<AtomicBool>,
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
        let next_callback_id = Rc::new(RefCell::new(1));
        let fetch_response_tx = Rc::new(RefCell::new(None));
        let stream_manager = Arc::new(stream_manager::StreamManager::new());

        // Create log callback that bypasses scheduler (calls ops.handle_log directly)
        let log_callback = bindings::log_callback_from_ops(&ops);

        // Create NEW context in the pooled isolate
        let context = {
            use std::pin::pin;

            // SAFETY: IsolateScope and HandleScope both need &mut Isolate from the same source.
            // This is the same pattern used in evaluate() (line 535).
            let isolate_ptr = isolate as *mut v8::Isolate;
            let _isolate_scope = unsafe { v8::IsolateScope::new(&mut *isolate_ptr) };
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);

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
            crypto::setup_crypto(scope);

            // Only setup pure JS APIs if no snapshot (they're in the snapshot)
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

            // Security: Remove SharedArrayBuffer and Atomics (Spectre mitigations)
            // Must be done at context creation, not in snapshot (breaks V8 bootstrapping)
            bindings::setup_security_restrictions(scope);

            v8::Global::new(scope.as_ref(), context)
        };

        // Setup addEventListener (placeholder - actual setup done during context creation)
        Self::setup_event_listener(isolate, &context)?;

        // Setup environment variables and bindings (placeholder)
        Self::setup_env(isolate, &context, &script.env, &script.bindings)?;

        // Evaluate user script (placeholder)
        Self::evaluate_script(isolate, &context, &script.code)?;

        // Setup ES Modules handler (placeholder)
        Self::setup_es_modules_handler(isolate, &context)?;

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

        // Store raw pointer to isolate (safe because ExecutionContext is dropped before Locker)
        // We cast &mut v8::Isolate to *mut v8::OwnedIsolate - this is safe because:
        // 1. The isolate is locked and we have exclusive access
        // 2. ExecutionContext lifetime is tied to the Locker lifetime
        // 3. v8::Isolate and v8::OwnedIsolate have same memory layout
        let isolate_ptr = isolate as *mut v8::Isolate as *mut v8::OwnedIsolate;

        let request = RequestContext::new(
            context,
            scheduler_tx,
            callback_rx,
            callback_notify,
            fetch_callbacks,
            fetch_error_callbacks,
            stream_callbacks,
            next_callback_id,
            fetch_response_tx,
            stream_manager,
            event_loop_handle,
            cancel,
        );

        Ok(Self {
            isolate: isolate_ptr,
            lmi_ptr,
            platform,
            limits,
            memory_limit_hit,
            request,
            async_waiter: None,
        })
    }

    /// Reconstruct an ExecutionContext from a cached RequestContext (warm hit path).
    ///
    /// Used by `execute_pinned` to wrap a cached RequestContext with fresh
    /// per-isolate metadata for the next request.
    pub(crate) fn from_cached(
        isolate: *mut v8::OwnedIsolate,
        lmi_ptr: *mut LockerManagedIsolate,
        platform: &'static v8::SharedRef<v8::Platform>,
        limits: RuntimeLimits,
        memory_limit_hit: Arc<AtomicBool>,
        request: RequestContext,
        async_waiter: Option<Arc<AsyncWaiter>>,
    ) -> Self {
        Self {
            isolate,
            lmi_ptr,
            platform,
            limits,
            memory_limit_hit,
            request,
            async_waiter,
        }
    }

    /// Consume this ExecutionContext, returning the RequestContext and isolate pointer.
    ///
    /// Used by `execute_pinned` to save the RequestContext back to the cache
    /// without aborting the event loop. The remaining EC fields (raw pointers,
    /// static refs, Arcs) are dropped normally.
    pub(crate) fn into_parts(self) -> (RequestContext, *mut v8::OwnedIsolate) {
        (self.request, self.isolate)
    }

    /// Helper: Setup addEventListener in the context
    ///
    /// Uses the shared implementation from worker module.
    fn setup_event_listener(
        isolate: &mut v8::Isolate,
        context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        crate::worker::setup_event_listener(isolate, context).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup addEventListener: {}",
                e
            ))
        })
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

        // SAFETY: IsolateScope needs &mut Isolate, same source as HandleScope below.
        let isolate_ptr = isolate as *mut v8::Isolate;
        let _isolate_scope = unsafe { v8::IsolateScope::new(&mut *isolate_ptr) };

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

    /// Helper: Setup ES modules handler
    ///
    /// Uses the shared implementation from worker module.
    fn setup_es_modules_handler(
        isolate: &mut v8::Isolate,
        context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        crate::worker::setup_es_modules_handler(isolate, context).map_err(|e| {
            TerminationReason::InitializationError(format!(
                "Failed to setup ES modules handler: {}",
                e
            ))
        })
    }

    /// Evaluate JavaScript code in this context
    pub fn evaluate(&mut self, code: &WorkerCode) -> Result<(), String> {
        use std::pin::pin;

        // SAFETY: We need to access both the isolate and context, which are separate fields.
        // This is safe because we have exclusive access to self, and the isolate pointer
        // is valid for the lifetime of this ExecutionContext.
        match code {
            WorkerCode::JavaScript(js) => unsafe {
                let isolate = &mut *self.isolate;
                let _scope = v8::IsolateScope::new(isolate);
                let scope = pin!(v8::HandleScope::new(isolate));
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
            },
            WorkerCode::Snapshot(data) => {
                let (source, cache_bytes) = crate::snapshot::unpack_code_cache(data)
                    .ok_or("Failed to unpack code cache bundle")?;

                unsafe {
                    let isolate = &mut *self.isolate;
                    let _scope = v8::IsolateScope::new(isolate);
                    let scope = pin!(v8::HandleScope::new(isolate));
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

    /// Process pending callbacks (timers, fetch responses, etc.)
    ///
    /// NOTE: This method uses try_recv() polling. For true async behavior,
    /// use WorkerFuture which polls the channel with a waker.
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
    ///
    /// This is the core callback processing logic, extracted to be called
    /// from both process_callbacks() and WorkerFuture::poll().
    pub fn process_single_callback(&mut self, msg: crate::runtime::CallbackMessage) {
        use crate::runtime::CallbackMessage;
        use std::pin::pin;

        // Get callback data before entering V8 scope
        let (fetch_callback, fetch_error_callback) = match &msg {
            CallbackMessage::FetchError(callback_id, _) => {
                let cb1 = self
                    .request
                    .fetch_callbacks
                    .borrow_mut()
                    .remove(callback_id);
                let cb2 = self
                    .request
                    .fetch_error_callbacks
                    .borrow_mut()
                    .remove(callback_id);
                (cb1, cb2)
            }
            CallbackMessage::FetchStreamingSuccess(callback_id, _, _) => {
                let cb1 = self
                    .request
                    .fetch_callbacks
                    .borrow_mut()
                    .remove(callback_id);
                let cb2 = self
                    .request
                    .fetch_error_callbacks
                    .borrow_mut()
                    .remove(callback_id);
                (cb1, cb2)
            }
            _ => (None, None),
        };

        // Now enter V8 scope
        unsafe {
            let isolate = &mut *self.isolate;
            let _scope = v8::IsolateScope::new(isolate);
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.request.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            match msg {
                CallbackMessage::ExecuteTimeout(callback_id)
                | CallbackMessage::ExecuteInterval(callback_id) => {
                    let global = context.global(scope);
                    let execute_timer_key = v8::String::new(scope, "__executeTimer").unwrap();

                    if let Some(execute_fn_val) = global.get(scope, execute_timer_key.into())
                        && execute_fn_val.is_function()
                    {
                        let execute_fn: v8::Local<v8::Function> =
                            execute_fn_val.try_into().unwrap();
                        let id_val = v8::Number::new(scope, callback_id as f64);
                        execute_fn.call(scope, global.into(), &[id_val.into()]);
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
                    use crate::runtime::callback_handlers;

                    if let Some(callback_global) = fetch_callback {
                        let meta_obj = v8::Object::new(scope);
                        callback_handlers::populate_fetch_meta(scope, meta_obj, &meta, stream_id);
                        let callback: v8::Local<v8::Function> =
                            v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[meta_obj.into()]);
                    }
                }
                CallbackMessage::StreamChunk(callback_id, chunk) => {
                    use crate::runtime::callback_handlers;

                    let callback_opt = {
                        let mut cbs = self.request.stream_callbacks.borrow_mut();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let result_obj = v8::Object::new(scope);
                        callback_handlers::populate_stream_chunk_result(scope, result_obj, chunk);
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::StorageResult(callback_id, storage_result) => {
                    use crate::runtime::{callback_handlers, dispatch_binding_callbacks};
                    use openworkers_core::StorageResult;

                    let (error_msg, result_value) =
                        if let StorageResult::Error(err) = &storage_result {
                            (Some(err.as_str()), None)
                        } else {
                            let result_obj = v8::Object::new(scope);
                            callback_handlers::populate_storage_result(
                                scope,
                                result_obj,
                                storage_result,
                                &self.request.stream_manager,
                            );
                            (None, Some(result_obj.into()))
                        };

                    dispatch_binding_callbacks(
                        scope,
                        callback_id,
                        &self.request.fetch_callbacks,
                        &self.request.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
                CallbackMessage::KvResult(callback_id, kv_result) => {
                    use crate::runtime::{callback_handlers, dispatch_binding_callbacks};
                    use openworkers_core::KvResult;

                    let (error_msg, result_value) = if let KvResult::Error(err) = &kv_result {
                        (Some(err.as_str()), None)
                    } else {
                        let result_obj = v8::Object::new(scope);
                        callback_handlers::populate_kv_result(scope, result_obj, kv_result);
                        (None, Some(result_obj.into()))
                    };

                    dispatch_binding_callbacks(
                        scope,
                        callback_id,
                        &self.request.fetch_callbacks,
                        &self.request.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
                CallbackMessage::DatabaseResult(callback_id, database_result) => {
                    use crate::runtime::{callback_handlers, dispatch_binding_callbacks};
                    use openworkers_core::DatabaseResult;

                    let (error_msg, result_value) =
                        if let DatabaseResult::Error(err) = &database_result {
                            (Some(err.as_str()), None)
                        } else {
                            let result_obj = v8::Object::new(scope);
                            callback_handlers::populate_database_result(
                                scope,
                                result_obj,
                                database_result,
                            );
                            (None, Some(result_obj.into()))
                        };

                    dispatch_binding_callbacks(
                        scope,
                        callback_id,
                        &self.request.fetch_callbacks,
                        &self.request.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
            }
        }
        // Note: Microtask checkpoint is NOT done here anymore.
        // It's done in pump_and_checkpoint() which is called after processing
        // all callbacks in a batch. This is more efficient.
    }

    /// Pump V8 platform messages and perform microtask checkpoint
    ///
    /// This must be called regularly to:
    /// 1. Process V8 platform messages (GC, optimizations, etc.)
    /// 2. Execute microtasks (Promise.then, async/await continuations)
    ///
    /// Called from both process_callbacks() and WorkerFuture::poll()
    pub fn pump_and_checkpoint(&mut self) {
        use std::pin::pin;

        // Pump V8 platform message loop (GC, etc.)
        unsafe {
            let isolate = &mut *self.isolate;

            while v8::Platform::pump_message_loop(self.platform, isolate, false) {
                // Continue pumping until no more messages
            }
        }

        // Process microtasks (Promises, async/await) - CRITICAL for Promise resolution!
        // Without this, .then() handlers and async/await continuations never execute.
        unsafe {
            let isolate = &mut *self.isolate;
            let _scope = v8::IsolateScope::new(isolate);
            let scope = pin!(v8::HandleScope::new(isolate));
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
    /// This is used by WorkerFuture to check if the event loop should exit.
    /// Returns true if the loop should exit.
    pub fn check_exit_with_abort(
        &mut self,
        exit_condition: EventLoopExit,
        abort_config: &Option<AbortConfig>,
        abort_signaled_at: &mut Option<tokio::time::Instant>,
    ) -> bool {
        use std::pin::pin;

        unsafe {
            let isolate = &mut *self.isolate;
            let _scope = v8::IsolateScope::new(isolate);
            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.request.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);

            // Basic exit condition check
            let base_exit = check_exit_condition(scope, global, exit_condition);

            // If abort detection is enabled, handle client disconnects
            if let Some(config) = abort_config {
                let (request_complete, active_streams) = get_completion_state(scope, global);

                // Detect client disconnect and signal abort to JS
                if active_streams > 0
                    && abort_signaled_at.is_none()
                    && let Some(stream_id) = get_response_stream_id(scope, global)
                    && !self.request.stream_manager.has_sender(stream_id)
                {
                    *abort_signaled_at = Some(tokio::time::Instant::now());
                    signal_client_disconnect(scope);
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

    /// Execute a task in this context
    pub async fn exec(&mut self, mut task: Event) -> Result<(), TerminationReason> {
        // Check if aborted before starting
        if self.request.aborted.load(Ordering::SeqCst) {
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

    /// Drain remaining background work (waitUntil promises) after exec().
    ///
    /// After exec() returns with `StreamsComplete`, the HTTP response is sent
    /// and all response streams are closed, but waitUntil promises may still
    /// be pending. This method pumps V8 microtasks until `FullyComplete`.
    ///
    /// Creates its own security guards so background work cannot run forever.
    /// Returns Ok if all background work completed, or an error if it timed out.
    pub async fn drain_waituntil(&mut self) -> Result<(), TerminationReason> {
        let isolate_handle = unsafe { (*self.isolate).thread_safe_handle() };

        // Fresh guards for background work — same limits as exec()
        let wall_guard =
            TimeoutGuard::new(isolate_handle.clone(), self.limits.max_wall_clock_time_ms);
        let cpu_guard = CpuEnforcer::new(isolate_handle, self.limits.max_cpu_time_ms);

        let result = self
            .await_event_loop(&wall_guard, &cpu_guard, EventLoopExit::FullyComplete, None)
            .await;

        self.check_termination_reason(
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
        )
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
    /// - CPU time limit exceeded (Linux only)
    #[inline]
    pub fn is_terminated(
        &self,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
    ) -> bool {
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
    /// Uses poll_fn for true async polling instead of sleep-based polling.
    ///
    /// ## Design: poll_fn vs WorkerFuture
    ///
    /// We use `poll_fn` here instead of `WorkerFuture` because:
    /// - `await_event_loop` is a method on `&mut self`
    /// - `WorkerFuture::new` also needs `&mut ExecutionContext`
    /// - Rust's borrowing rules prevent creating WorkerFuture inside a method
    ///
    /// `poll_fn` solves this by letting us define the poll logic inline,
    /// capturing `&mut self` without ownership conflicts.
    async fn await_event_loop(
        &mut self,
        wall_guard: &TimeoutGuard,
        cpu_guard: &Option<CpuEnforcer>,
        exit_condition: EventLoopExit,
        abort_config: Option<AbortConfig>,
    ) -> Result<(), String> {
        use crate::event_loop::drain_and_process;
        use std::task::Poll;

        let mut abort_signaled_at: Option<tokio::time::Instant> = None;
        let mut pending_callbacks: Vec<crate::runtime::CallbackMessage> = Vec::with_capacity(16);
        let lmi_ptr = self.lmi_ptr; // Copy raw pointer (no persistent borrow on self)
        let async_waiter = self.async_waiter.clone(); // Clone Rc (cheap) to avoid borrow on self

        std::future::poll_fn(|cx| {
            // -- Fair queue gate (if multiplexing enabled) --
            // When multiple requests share an isolate, only one can hold the
            // V8 Locker at a time. Others wait in FIFO order.
            if let Some(ref waiter) = async_waiter
                && !waiter.try_lock(cx)
            {
                return Poll::Pending; // Not our turn, will be woken in FIFO order
            }

            // -- Acquire V8 lock (if pooled isolate) --
            // For the LockerManagedIsolate path, we create a Locker + JsLock
            // that will be dropped when this closure returns (including Pending),
            // releasing the V8 mutex for other tasks during I/O waits.
            // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
            let _lock_guard = unsafe { try_lock_v8(lmi_ptr) };

            // 1. Check termination (CPU/wall-clock guards)
            if self.is_terminated(wall_guard, cpu_guard) {
                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                return Poll::Ready(Err("Execution terminated".to_string()));
            }

            // 2-4. Drain callbacks, process, pump V8
            if let Err(e) = drain_and_process(cx, self, &mut pending_callbacks) {
                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                return Poll::Ready(Err(e));
            }

            // 5. Check exit condition with abort handling
            let should_exit =
                self.check_exit_with_abort(exit_condition, &abort_config, &mut abort_signaled_at);

            if should_exit {
                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                return Poll::Ready(Ok(()));
            }

            // 6. Not done yet — release fair queue and V8 lock.
            //    V8 mutex released, other requests can run on this isolate.
            if let Some(ref waiter) = async_waiter {
                waiter.unlock();
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
            let lmi_ptr = self.lmi_ptr;
            let async_waiter = self.async_waiter.clone();

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
                let _lock = unsafe { try_lock_v8(lmi_ptr) };

                use std::pin::pin;
                let result = unsafe {
                    let isolate = &mut *self.isolate;
                    let _scope = v8::IsolateScope::new(isolate);
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    trigger_fetch_handler(
                        scope,
                        &req.url,
                        req.method.as_str(),
                        &req.headers,
                        &mut req.body,
                        body_stream_id,
                    )
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
        let (status, response) = {
            let lmi_ptr = self.lmi_ptr;
            let async_waiter = self.async_waiter.clone();

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
                let _lock = unsafe { try_lock_v8(lmi_ptr) };

                use std::pin::pin;
                let result = unsafe {
                    let isolate = &mut *self.isolate;
                    let _scope = v8::IsolateScope::new(isolate);
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    read_response_object(
                        scope,
                        &self.request.stream_manager,
                        self.limits.stream_buffer_size,
                    )
                };

                if let Some(ref waiter) = async_waiter {
                    waiter.unlock();
                }

                std::task::Poll::Ready(result)
            })
            .await?
        };

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
        let scheduled_time = match &task_init.source {
            Some(openworkers_core::TaskSource::Schedule { time }) => Some(*time),
            _ => None,
        };

        // -- Phase 1: Trigger task handler (fair queue + lock) --
        {
            let lmi_ptr = self.lmi_ptr;
            let async_waiter = self.async_waiter.clone();

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
                let _lock = unsafe { try_lock_v8(lmi_ptr) };

                use std::pin::pin;
                let result: Result<(), String> = unsafe {
                    let isolate = &mut *self.isolate;
                    let _scope = v8::IsolateScope::new(isolate);
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    let global = context.global(scope);

                    let task_handler_key = v8::String::new(scope, "__taskHandler").unwrap();
                    let scheduled_handler_key =
                        v8::String::new(scope, "__scheduledHandler").unwrap();

                    if let Some(handler_val) = global.get(scope, task_handler_key.into())
                        && handler_val.is_function()
                    {
                        let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                        let event_obj = v8::Object::new(scope);

                        let id_key = v8::String::new(scope, "taskId").unwrap();
                        let id_val = v8::String::new(scope, &task_init.task_id).unwrap();
                        event_obj.set(scope, id_key.into(), id_val.into());

                        let attempt_key = v8::String::new(scope, "attempt").unwrap();
                        let attempt_val = v8::Number::new(scope, task_init.attempt as f64);
                        event_obj.set(scope, attempt_key.into(), attempt_val.into());

                        if let Some(payload) = &task_init.payload {
                            let payload_key = v8::String::new(scope, "payload").unwrap();
                            let payload_str = serde_json::to_string(payload).unwrap_or_default();
                            let payload_json = v8::String::new(scope, &payload_str).unwrap();

                            if let Some(parsed) = v8::json::parse(scope, payload_json) {
                                event_obj.set(scope, payload_key.into(), parsed);
                            }
                        }

                        if let Some(time) = scheduled_time {
                            let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                            let time_val = v8::Number::new(scope, time as f64);
                            event_obj.set(scope, time_key.into(), time_val.into());
                        }

                        let call_result =
                            handler_fn.call(scope, global.into(), &[event_obj.into()]);

                        if call_result.is_none() {
                            Err("Execution terminated".to_string())
                        } else {
                            Ok(())
                        }
                    } else if let Some(handler_val) =
                        global.get(scope, scheduled_handler_key.into())
                        && handler_val.is_function()
                    {
                        let handler_fn: v8::Local<v8::Function> = handler_val.try_into().unwrap();

                        let event_obj = v8::Object::new(scope);

                        if let Some(time) = scheduled_time {
                            let time_key = v8::String::new(scope, "scheduledTime").unwrap();
                            let time_val = v8::Number::new(scope, time as f64);
                            event_obj.set(scope, time_key.into(), time_val.into());
                        }

                        let call_result =
                            handler_fn.call(scope, global.into(), &[event_obj.into()]);

                        if call_result.is_none() {
                            Err("Execution terminated".to_string())
                        } else {
                            Ok(())
                        }
                    } else {
                        Ok(())
                    }
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
            let lmi_ptr = self.lmi_ptr;
            let async_waiter = self.async_waiter.clone();

            std::future::poll_fn(|cx| {
                if let Some(ref waiter) = async_waiter
                    && !waiter.try_lock(cx)
                {
                    return std::task::Poll::Pending;
                }

                // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
                let _lock = unsafe { try_lock_v8(lmi_ptr) };

                use std::pin::pin;
                let result = unsafe {
                    let isolate = &mut *self.isolate;
                    let _scope = v8::IsolateScope::new(isolate);
                    let scope = pin!(v8::HandleScope::new(isolate));
                    let mut scope = scope.init();
                    let context = v8::Local::new(&scope, &self.request.context);
                    let scope = &mut v8::ContextScope::new(&mut scope, context);

                    let global = context.global(scope);
                    let result_key = v8::String::new(scope, "__taskResult").unwrap();

                    if let Some(result_val) = global.get(scope, result_key.into()) {
                        if result_val.is_object() {
                            let result_obj: v8::Local<v8::Object> = result_val.try_into().unwrap();

                            let success_key = v8::String::new(scope, "success").unwrap();
                            let success = result_obj
                                .get(scope, success_key.into())
                                .map(|v| v.is_true())
                                .unwrap_or(true);

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
    /// Must be called between requests (warm isolate path). Clears response state,
    /// completion flag, stream state, timer callbacks, stale callbacks, and V8 Global handles.
    ///
    /// Cancels any lingering `terminate_execution` flag before evaluating JS.
    /// Does NOT touch the event loop (it persists across requests).
    /// Does NOT reset `__nextTimerId` (monotonically increasing to avoid ID collisions).
    pub fn reset(&mut self) -> Result<(), String> {
        // Acquire lock if pooled isolate (evaluate accesses V8 directly)
        let lmi_ptr = self.lmi_ptr;
        // SAFETY: lmi_ptr valid for request lifetime (pool-managed)
        let _lock_guard = unsafe { try_lock_v8(lmi_ptr) };

        // 0. Cancel any lingering terminate_execution flag from a previous timeout/abort.
        // Without this, evaluate() below would fail immediately if the flag is still set.
        unsafe {
            (*self.isolate).cancel_terminate_execution();
        }

        // 1. Reset JS globals (response, completion, streams, task result, timers)
        self.evaluate(&WorkerCode::JavaScript(
            r#"
            globalThis.__lastResponse = undefined;
            globalThis.__requestComplete = false;
            globalThis.__lastResponseStreamId = undefined;
            globalThis.__activeResponseStreams = 0;
            globalThis.__taskResult = undefined;
            globalThis.__timerCallbacks.clear();
            globalThis.__intervalIds.clear();
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
        *self.request.next_callback_id.borrow_mut() = 1;

        Ok(())
    }

    /// Abort execution
    pub fn abort(&mut self) {
        self.request
            .aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        unsafe { (*self.isolate).terminate_execution() };
    }
}

impl crate::event_loop::EventLoopRuntime for ExecutionContext {
    fn callback_rx_mut(
        &mut self,
    ) -> &mut tokio::sync::mpsc::UnboundedReceiver<crate::runtime::CallbackMessage> {
        &mut self.request.callback_rx
    }

    fn process_callback(&mut self, msg: crate::runtime::CallbackMessage) {
        self.process_single_callback(msg);
    }

    fn pump_and_checkpoint(&mut self) {
        ExecutionContext::pump_and_checkpoint(self);
    }
}

// Tests for ExecutionContext are in pool_multiplexed.rs (pool integration tests)
// and in Worker tests (worker.rs).
