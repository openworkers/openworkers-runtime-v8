pub mod bindings;
pub mod callback_handlers;
pub mod crypto;
pub mod scheduler;
pub mod stream_manager;
pub mod streams;
pub mod text_encoding;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{Notify, mpsc};
use v8;

use crate::security::{CustomAllocator, HeapLimitState, install_heap_limit_callback};
use bindings::LogCallback;
use openworkers_core::{DatabaseResult, KvResult, RuntimeLimits, StorageResult, WorkerCode};

// Re-export scheduler types
pub use scheduler::{
    CallbackId, CallbackMessage, CallbackSender, SchedulerMessage, run_event_loop,
};

/// Helper to dispatch binding result callbacks (resolve/reject pattern)
fn dispatch_binding_callbacks(
    scope: &mut v8::PinScope,
    callback_id: CallbackId,
    resolve_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    reject_callbacks: &Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    error_msg: Option<&str>,
    result_value: Option<v8::Local<v8::Value>>,
) {
    if let Some(err_msg) = error_msg {
        // Error - call reject
        let reject_opt = {
            let mut cbs = reject_callbacks.borrow_mut();
            cbs.remove(&callback_id)
        };
        // Cleanup resolve
        resolve_callbacks.borrow_mut().remove(&callback_id);

        if let Some(callback_global) = reject_opt {
            let error_msg_val = v8::String::new(scope, err_msg).unwrap();
            let error = v8::Exception::error(scope, error_msg_val);
            let callback = v8::Local::new(scope, &callback_global);
            let recv = v8::undefined(scope);
            callback.call(scope, recv.into(), &[error]);
        }
    } else if let Some(value) = result_value {
        // Success - call resolve
        let resolve_opt = {
            let mut cbs = resolve_callbacks.borrow_mut();
            cbs.remove(&callback_id)
        };
        // Cleanup reject
        reject_callbacks.borrow_mut().remove(&callback_id);

        if let Some(callback_global) = resolve_opt {
            let callback = v8::Local::new(scope, &callback_global);
            let recv = v8::undefined(scope);
            callback.call(scope, recv.into(), &[value]);
        }
    }
}

pub struct Runtime {
    pub isolate: v8::OwnedIsolate,
    pub context: v8::Global<v8::Context>,
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,
    pub(crate) fetch_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) fetch_error_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) stream_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) _next_callback_id: Rc<RefCell<CallbackId>>,
    /// Channel for fetch response (set during fetch event execution)
    pub(crate) fetch_response_tx: Rc<RefCell<Option<tokio::sync::oneshot::Sender<String>>>>,
    /// V8 Platform (for pump_message_loop)
    platform: &'static v8::SharedRef<v8::Platform>,
    /// Stream manager for native streaming
    pub(crate) stream_manager: Arc<stream_manager::StreamManager>,
    /// Flag set when ArrayBuffer memory limit is hit
    pub(crate) memory_limit_hit: Arc<AtomicBool>,
    /// Runtime resource limits (CPU, wall-clock, memory)
    pub(crate) limits: RuntimeLimits,
    /// Notify to wake up exec() loop when callbacks are ready.
    /// Note: With poll_fn pattern, Worker polls callback_rx directly,
    /// but callback_notify is still used by the event loop to signal.
    #[allow(dead_code)]
    pub(crate) callback_notify: Arc<Notify>,
    /// Heap limit state - must be kept alive for the isolate's lifetime
    /// Dropped after isolate, so the callback pointer remains valid
    #[allow(dead_code)]
    _heap_limit_state: Box<HeapLimitState>,
}

impl Runtime {
    pub fn new(
        limits: Option<RuntimeLimits>,
        log_callback: LogCallback,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<SchedulerMessage>,
        mpsc::UnboundedSender<CallbackMessage>,
        Arc<Notify>,
    ) {
        let limits = limits.unwrap_or_default();

        // Get global V8 platform (initialized once, shared across all modules)
        let platform = crate::platform::get_platform();

        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
        let (callback_tx, callback_rx) = mpsc::unbounded_channel();
        let callback_notify = Arc::new(Notify::new());

        let fetch_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let fetch_error_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let stream_callbacks = Rc::new(RefCell::new(HashMap::new()));
        let next_callback_id = Rc::new(RefCell::new(1));
        let fetch_response_tx = Rc::new(RefCell::new(None));
        let stream_manager = Arc::new(stream_manager::StreamManager::new());

        // Memory limit tracking for ArrayBuffer allocations
        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Convert heap limits from MB to bytes
        let heap_initial = limits.heap_initial_mb * 1024 * 1024;
        let heap_max = limits.heap_max_mb * 1024 * 1024;

        // Create custom ArrayBuffer allocator to enforce memory limits on external memory
        // This is critical: V8 heap limits don't cover ArrayBuffers, Uint8Array, etc.
        let array_buffer_allocator = CustomAllocator::new(heap_max, Arc::clone(&memory_limit_hit));

        // Load snapshot (centralized, handles empty file case)
        let snapshot_ref = crate::platform::get_snapshot();

        // Create isolate with custom allocator and heap limits
        let mut params = v8::CreateParams::default()
            .heap_limits(heap_initial, heap_max)
            .array_buffer_allocator(array_buffer_allocator.into_v8_allocator())
            .allow_atomics_wait(false); // Security: prevent Atomics.wait() from blocking

        if let Some(snapshot_data) = snapshot_ref {
            params = params.snapshot_blob((*snapshot_data).into());
        }

        let mut isolate = v8::Isolate::new(params);

        // Install heap limit callback to prevent V8 OOM from crashing the process
        let heap_limit_state =
            install_heap_limit_callback(&mut isolate, Arc::clone(&memory_limit_hit), heap_max);

        let use_snapshot = snapshot_ref.is_some();

        let context = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut isolate));
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

            v8::Global::new(scope.as_ref(), context)
        };

        let runtime = Self {
            isolate,
            context,
            scheduler_tx,
            callback_rx,
            fetch_callbacks,
            fetch_error_callbacks,
            stream_callbacks,
            _next_callback_id: next_callback_id,
            fetch_response_tx,
            platform,
            stream_manager,
            memory_limit_hit,
            limits,
            callback_notify: callback_notify.clone(),
            _heap_limit_state: heap_limit_state,
        };

        (runtime, scheduler_rx, callback_tx, callback_notify)
    }

    pub fn process_callbacks(&mut self) {
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // 1. Pump V8 Platform message loop (like deno_core)
        // This processes V8's internal task queue (e.g., Atomics.waitAsync, WebAssembly compilation)
        while v8::Platform::pump_message_loop(self.platform, scope, false) {
            // Keep pumping while there are messages
        }

        // 2. Process our custom callbacks (timers, fetch, etc.)
        while let Ok(msg) = self.callback_rx.try_recv() {
            match msg {
                CallbackMessage::ExecuteTimeout(callback_id)
                | CallbackMessage::ExecuteInterval(callback_id) => {
                    // Call the JavaScript __executeTimer function
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
                CallbackMessage::FetchError(callback_id, error_msg) => {
                    // Remove from success callbacks (cleanup)
                    {
                        let mut cbs = self.fetch_callbacks.borrow_mut();
                        cbs.remove(&callback_id);
                    }

                    // Get error callback and call it
                    let error_callback_opt = {
                        let mut cbs = self.fetch_error_callbacks.borrow_mut();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = error_callback_opt {
                        let error_msg_val = v8::String::new(scope, &error_msg).unwrap();
                        let error = v8::Exception::error(scope, error_msg_val);
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[error]);
                    }
                }
                CallbackMessage::FetchStreamingSuccess(callback_id, meta, stream_id) => {
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.borrow_mut();
                        cbs.remove(&callback_id)
                    };

                    // Cleanup error callback
                    {
                        let mut cbs = self.fetch_error_callbacks.borrow_mut();
                        cbs.remove(&callback_id);
                    }

                    if let Some(callback_global) = callback_opt {
                        let meta_obj = v8::Object::new(scope);
                        callback_handlers::populate_fetch_meta(scope, meta_obj, &meta, stream_id);
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[meta_obj.into()]);
                    }
                }
                CallbackMessage::StreamChunk(callback_id, chunk) => {
                    let callback_opt = {
                        let mut cbs = self.stream_callbacks.borrow_mut();
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
                    let (error_msg, result_value) =
                        if let StorageResult::Error(err) = &storage_result {
                            (Some(err.as_str()), None)
                        } else {
                            let result_obj = v8::Object::new(scope);
                            callback_handlers::populate_storage_result(
                                scope,
                                result_obj,
                                storage_result,
                            );
                            (None, Some(result_obj.into()))
                        };
                    dispatch_binding_callbacks(
                        scope,
                        callback_id,
                        &self.fetch_callbacks,
                        &self.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
                CallbackMessage::KvResult(callback_id, kv_result) => {
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
                        &self.fetch_callbacks,
                        &self.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
                CallbackMessage::DatabaseResult(callback_id, database_result) => {
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
                        &self.fetch_callbacks,
                        &self.fetch_error_callbacks,
                        error_msg,
                        result_value,
                    );
                }
            }
        }

        // 3. Process microtasks (Promises, async/await) - like deno_core
        // Use TryCatch to handle any exceptions during microtask processing
        let tc_scope = pin!(v8::TryCatch::new(scope));
        let mut tc_scope = tc_scope.init();
        tc_scope.perform_microtask_checkpoint();

        // Check for exceptions during microtask processing
        if let Some(exception) = tc_scope.exception() {
            let exception_string = exception
                .to_string(&tc_scope)
                .map(|s| s.to_rust_string_lossy(&tc_scope))
                .unwrap_or_else(|| "Unknown exception".to_string());
            eprintln!(
                "Exception during microtask processing: {}",
                exception_string
            );
        }
    }

    /// Process a single callback message in a V8 scope.
    ///
    /// This method handles one callback message from the event loop.
    /// Used by poll_fn-based event loops that poll the channel directly.
    pub fn process_single_callback(&mut self, msg: CallbackMessage) {
        use std::pin::pin;

        let scope = pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        match msg {
            CallbackMessage::ExecuteTimeout(callback_id)
            | CallbackMessage::ExecuteInterval(callback_id) => {
                let global = context.global(scope);
                let execute_timer_key = v8::String::new(scope, "__executeTimer").unwrap();

                if let Some(execute_fn_val) = global.get(scope, execute_timer_key.into())
                    && execute_fn_val.is_function()
                {
                    let execute_fn: v8::Local<v8::Function> = execute_fn_val.try_into().unwrap();
                    let id_val = v8::Number::new(scope, callback_id as f64);
                    execute_fn.call(scope, global.into(), &[id_val.into()]);
                }
            }
            CallbackMessage::FetchError(callback_id, error_msg) => {
                // Remove from success callbacks (cleanup)
                {
                    let mut cbs = self.fetch_callbacks.borrow_mut();
                    cbs.remove(&callback_id);
                }

                // Get error callback and call it
                let error_callback_opt = {
                    let mut cbs = self.fetch_error_callbacks.borrow_mut();
                    cbs.remove(&callback_id)
                };

                if let Some(callback_global) = error_callback_opt {
                    let error_msg_val = v8::String::new(scope, &error_msg).unwrap();
                    let error = v8::Exception::error(scope, error_msg_val);
                    let callback = v8::Local::new(scope, &callback_global);
                    let recv = v8::undefined(scope);
                    callback.call(scope, recv.into(), &[error]);
                }
            }
            CallbackMessage::FetchStreamingSuccess(callback_id, meta, stream_id) => {
                let callback_opt = {
                    let mut cbs = self.fetch_callbacks.borrow_mut();
                    cbs.remove(&callback_id)
                };

                // Cleanup error callback
                {
                    let mut cbs = self.fetch_error_callbacks.borrow_mut();
                    cbs.remove(&callback_id);
                }

                if let Some(callback_global) = callback_opt {
                    let meta_obj = v8::Object::new(scope);
                    callback_handlers::populate_fetch_meta(scope, meta_obj, &meta, stream_id);
                    let callback = v8::Local::new(scope, &callback_global);
                    let recv = v8::undefined(scope);
                    callback.call(scope, recv.into(), &[meta_obj.into()]);
                }
            }
            CallbackMessage::StreamChunk(callback_id, chunk) => {
                let callback_opt = {
                    let mut cbs = self.stream_callbacks.borrow_mut();
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
                let (error_msg, result_value) = if let StorageResult::Error(err) = &storage_result {
                    (Some(err.as_str()), None)
                } else {
                    let result_obj = v8::Object::new(scope);
                    callback_handlers::populate_storage_result(scope, result_obj, storage_result);
                    (None, Some(result_obj.into()))
                };
                dispatch_binding_callbacks(
                    scope,
                    callback_id,
                    &self.fetch_callbacks,
                    &self.fetch_error_callbacks,
                    error_msg,
                    result_value,
                );
            }
            CallbackMessage::KvResult(callback_id, kv_result) => {
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
                    &self.fetch_callbacks,
                    &self.fetch_error_callbacks,
                    error_msg,
                    result_value,
                );
            }
            CallbackMessage::DatabaseResult(callback_id, database_result) => {
                let (error_msg, result_value) = if let DatabaseResult::Error(err) = &database_result
                {
                    (Some(err.as_str()), None)
                } else {
                    let result_obj = v8::Object::new(scope);
                    callback_handlers::populate_database_result(scope, result_obj, database_result);
                    (None, Some(result_obj.into()))
                };
                dispatch_binding_callbacks(
                    scope,
                    callback_id,
                    &self.fetch_callbacks,
                    &self.fetch_error_callbacks,
                    error_msg,
                    result_value,
                );
            }
        }
    }

    /// Pump V8 platform messages and perform microtask checkpoint.
    ///
    /// This must be called regularly to:
    /// 1. Process V8 platform messages (GC, optimizations, etc.)
    /// 2. Execute microtasks (Promise.then, async/await continuations)
    pub fn pump_and_checkpoint(&mut self) {
        use std::pin::pin;

        // Pump V8 platform message loop
        while v8::Platform::pump_message_loop(self.platform, &self.isolate, false) {
            // Continue pumping until no more messages
        }

        // Process microtasks (Promises, async/await)
        let scope = pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let tc_scope = pin!(v8::TryCatch::new(scope));
        let mut tc_scope = tc_scope.init();
        tc_scope.perform_microtask_checkpoint();

        if let Some(exception) = tc_scope.exception() {
            let exception_string = exception
                .to_string(&tc_scope)
                .map(|s| s.to_rust_string_lossy(&tc_scope))
                .unwrap_or_else(|| "Unknown exception".to_string());
            eprintln!(
                "Exception during microtask processing: {}",
                exception_string
            );
        }
    }

    pub fn evaluate(&mut self, worker_code: &WorkerCode) -> Result<(), String> {
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Catch-all handles WebAssembly (behind wasm feature flag) and future variants
        #[allow(unreachable_patterns)]
        let script = match worker_code {
            WorkerCode::JavaScript(code) => code,
            WorkerCode::Snapshot(_) => {
                return Err("Snapshot worker code evaluation not supported yet".to_string());
            }
            _ => {
                return Err("V8 runtime only supports JavaScript code".to_string());
            }
        };

        let code = v8::String::new(scope, script).ok_or("Failed to create script string")?;

        // Use TryCatch to capture JavaScript exceptions
        let tc_scope = pin!(v8::TryCatch::new(scope));
        let tc_scope = tc_scope.init();

        let script_obj = match v8::Script::compile(&tc_scope, code, None) {
            Some(s) => s,
            None => {
                if let Some(exception) = tc_scope.exception() {
                    let msg = exception
                        .to_string(&tc_scope)
                        .map(|s| s.to_rust_string_lossy(&tc_scope))
                        .unwrap_or_else(|| "Unknown error".to_string());

                    // Try to get more detail from message
                    if let Some(message) = tc_scope.message() {
                        let line = message.get_line_number(&tc_scope).unwrap_or(0);
                        let col = message.get_start_column();
                        let source_line = message
                            .get_source_line(&tc_scope)
                            .map(|s| s.to_rust_string_lossy(&tc_scope))
                            .unwrap_or_default();

                        return Err(format!(
                            "SyntaxError at line {}, column {}: {}\n  > {}",
                            line, col, msg, source_line
                        ));
                    }

                    return Err(format!("SyntaxError: {}", msg));
                }

                return Err("Failed to compile script".to_string());
            }
        };

        match script_obj.run(&tc_scope) {
            Some(_) => Ok(()),
            None => {
                if let Some(exception) = tc_scope.exception() {
                    let msg = exception
                        .to_string(&tc_scope)
                        .map(|s| s.to_rust_string_lossy(&tc_scope))
                        .unwrap_or_else(|| "Unknown error".to_string());

                    // Try to get stack trace
                    if let Some(stack) = tc_scope.stack_trace() {
                        let stack_str = stack
                            .to_string(&tc_scope)
                            .map(|s| s.to_rust_string_lossy(&tc_scope))
                            .unwrap_or_default();

                        if !stack_str.is_empty() {
                            return Err(format!("{}\n{}", msg, stack_str));
                        }
                    }

                    return Err(msg);
                }

                Err("Failed to execute script".to_string())
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.scheduler_tx.send(SchedulerMessage::Shutdown);
    }
}

impl crate::event_loop::EventLoopRuntime for Runtime {
    fn callback_rx_mut(&mut self) -> &mut tokio::sync::mpsc::UnboundedReceiver<CallbackMessage> {
        &mut self.callback_rx
    }

    fn process_callback(&mut self, msg: CallbackMessage) {
        self.process_single_callback(msg);
    }

    fn pump_and_checkpoint(&mut self) {
        Runtime::pump_and_checkpoint(self);
    }
}
