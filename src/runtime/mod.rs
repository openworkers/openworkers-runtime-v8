pub mod bindings;
pub mod callback_handlers;
pub mod crypto;
pub mod stream_manager;
pub mod streams;
pub mod text_encoding;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use v8;

use crate::security::CustomAllocator;
use openworkers_core::{
    DatabaseOp, DatabaseResult, HttpRequest, HttpResponseMeta, KvOp, KvResult, Operation,
    OperationResult, OperationsHandle, ResponseBody, RuntimeLimits, StorageOp, StorageResult,
    WorkerCode,
};

pub type CallbackId = u64;

/// Wrapper that sends a callback message AND notifies the exec() loop
#[derive(Clone)]
pub struct CallbackSender {
    tx: mpsc::UnboundedSender<CallbackMessage>,
    notify: Arc<Notify>,
}

impl CallbackSender {
    pub fn new(tx: mpsc::UnboundedSender<CallbackMessage>, notify: Arc<Notify>) -> Self {
        Self { tx, notify }
    }

    pub fn send(
        &self,
        msg: CallbackMessage,
    ) -> Result<(), mpsc::error::SendError<CallbackMessage>> {
        let result = self.tx.send(msg);

        if result.is_ok() {
            self.notify.notify_one();
        }

        result
    }
}

pub enum SchedulerMessage {
    ScheduleTimeout(CallbackId, u64),
    ScheduleInterval(CallbackId, u64),
    ClearTimer(CallbackId),
    FetchStreaming(CallbackId, HttpRequest), // Fetch with streaming (stream created internally)
    BindingFetch(CallbackId, String, HttpRequest), // Fetch via binding (binding_name, request)
    BindingStorage(CallbackId, String, StorageOp), // Storage operation (binding_name, op)
    BindingKv(CallbackId, String, KvOp),     // KV operation (binding_name, op)
    BindingDatabase(CallbackId, String, DatabaseOp), // Database operation (binding_name, op)
    BindingWorker(CallbackId, String, HttpRequest), // Worker binding (binding_name, request)
    StreamRead(CallbackId, stream_manager::StreamId), // Read next chunk from stream
    StreamCancel(stream_manager::StreamId),  // Cancel/close a stream
    Log(openworkers_core::LogLevel, String), // Log message (fire-and-forget)
    Shutdown,
}

pub enum CallbackMessage {
    ExecuteTimeout(CallbackId),
    ExecuteInterval(CallbackId),
    FetchError(CallbackId, String),
    FetchStreamingSuccess(CallbackId, HttpResponseMeta, stream_manager::StreamId), // Fetch metadata + stream ID
    StreamChunk(CallbackId, stream_manager::StreamChunk), // Stream chunk ready
    StorageResult(CallbackId, StorageResult),             // Storage operation result
    KvResult(CallbackId, KvResult),                       // KV operation result
    DatabaseResult(CallbackId, DatabaseResult),           // Database operation result
}

pub struct Runtime {
    pub isolate: v8::OwnedIsolate,
    pub context: v8::Global<v8::Context>,
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,
    pub(crate) fetch_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) fetch_error_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) stream_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) _next_callback_id: Arc<Mutex<CallbackId>>,
    /// Channel for fetch response (set during fetch event execution)
    pub(crate) fetch_response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
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
}

impl Runtime {
    pub fn new(
        limits: Option<RuntimeLimits>,
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

        let fetch_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let fetch_error_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let stream_callbacks = Arc::new(Mutex::new(HashMap::new()));
        let next_callback_id = Arc::new(Mutex::new(1));
        let fetch_response_tx = Arc::new(Mutex::new(None));
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
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id);
                    }

                    // Get error callback and call it
                    let error_callback_opt = {
                        let mut cbs = self.fetch_error_callbacks.lock().unwrap();
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
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    // Cleanup error callback
                    {
                        let mut cbs = self.fetch_error_callbacks.lock().unwrap();
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
                        let mut cbs = self.stream_callbacks.lock().unwrap();
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
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let result_obj = v8::Object::new(scope);
                        callback_handlers::populate_storage_result(
                            scope,
                            result_obj,
                            storage_result,
                        );
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::KvResult(callback_id, kv_result) => {
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let result_obj = v8::Object::new(scope);
                        callback_handlers::populate_kv_result(scope, result_obj, kv_result);
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::DatabaseResult(callback_id, database_result) => {
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let result_obj = v8::Object::new(scope);
                        callback_handlers::populate_database_result(
                            scope,
                            result_obj,
                            database_result,
                        );
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
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
                .map(|s| s.to_rust_string_lossy(&*tc_scope))
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
                    let mut cbs = self.fetch_callbacks.lock().unwrap();
                    cbs.remove(&callback_id);
                }

                // Get error callback and call it
                let error_callback_opt = {
                    let mut cbs = self.fetch_error_callbacks.lock().unwrap();
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
                    let mut cbs = self.fetch_callbacks.lock().unwrap();
                    cbs.remove(&callback_id)
                };

                // Cleanup error callback
                {
                    let mut cbs = self.fetch_error_callbacks.lock().unwrap();
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
                    let mut cbs = self.stream_callbacks.lock().unwrap();
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
                let callback_opt = {
                    let mut cbs = self.fetch_callbacks.lock().unwrap();
                    cbs.remove(&callback_id)
                };

                if let Some(callback_global) = callback_opt {
                    let result_obj = v8::Object::new(scope);
                    callback_handlers::populate_storage_result(scope, result_obj, storage_result);
                    let callback = v8::Local::new(scope, &callback_global);
                    let recv = v8::undefined(scope);
                    callback.call(scope, recv.into(), &[result_obj.into()]);
                }
            }
            CallbackMessage::KvResult(callback_id, kv_result) => {
                let callback_opt = {
                    let mut cbs = self.fetch_callbacks.lock().unwrap();
                    cbs.remove(&callback_id)
                };

                if let Some(callback_global) = callback_opt {
                    let result_obj = v8::Object::new(scope);
                    callback_handlers::populate_kv_result(scope, result_obj, kv_result);
                    let callback = v8::Local::new(scope, &callback_global);
                    let recv = v8::undefined(scope);
                    callback.call(scope, recv.into(), &[result_obj.into()]);
                }
            }
            CallbackMessage::DatabaseResult(callback_id, database_result) => {
                let callback_opt = {
                    let mut cbs = self.fetch_callbacks.lock().unwrap();
                    cbs.remove(&callback_id)
                };

                if let Some(callback_global) = callback_opt {
                    let result_obj = v8::Object::new(scope);
                    callback_handlers::populate_database_result(scope, result_obj, database_result);
                    let callback = v8::Local::new(scope, &callback_global);
                    let recv = v8::undefined(scope);
                    callback.call(scope, recv.into(), &[result_obj.into()]);
                }
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
        while v8::Platform::pump_message_loop(self.platform, &mut self.isolate, false) {
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
                .map(|s| s.to_rust_string_lossy(&*tc_scope))
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
        let mut tc_scope = tc_scope.init();

        let script_obj = match v8::Script::compile(&mut tc_scope, code, None) {
            Some(s) => s,
            None => {
                if let Some(exception) = tc_scope.exception() {
                    let msg = exception
                        .to_string(&tc_scope)
                        .map(|s| s.to_rust_string_lossy(&*tc_scope))
                        .unwrap_or_else(|| "Unknown error".to_string());

                    // Try to get more detail from message
                    if let Some(message) = tc_scope.message() {
                        let line = message.get_line_number(&tc_scope).unwrap_or(0);
                        let col = message.get_start_column();
                        let source_line = message
                            .get_source_line(&tc_scope)
                            .map(|s| s.to_rust_string_lossy(&*tc_scope))
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

        match script_obj.run(&mut tc_scope) {
            Some(_) => Ok(()),
            None => {
                if let Some(exception) = tc_scope.exception() {
                    let msg = exception
                        .to_string(&tc_scope)
                        .map(|s| s.to_rust_string_lossy(&*tc_scope))
                        .unwrap_or_else(|| "Unknown error".to_string());

                    // Try to get stack trace
                    if let Some(stack) = tc_scope.stack_trace() {
                        let stack_str = stack
                            .to_string(&tc_scope)
                            .map(|s| s.to_rust_string_lossy(&*tc_scope))
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

pub async fn run_event_loop(
    mut scheduler_rx: mpsc::UnboundedReceiver<SchedulerMessage>,
    callback_tx: mpsc::UnboundedSender<CallbackMessage>,
    callback_notify: Arc<Notify>,
    stream_manager: Arc<stream_manager::StreamManager>,
    ops: OperationsHandle,
) {
    // Wrap sender with notify for automatic wake-up
    let callback_tx = CallbackSender::new(callback_tx, callback_notify);

    let mut running_tasks: HashMap<CallbackId, tokio::task::JoinHandle<()>> = HashMap::new();

    while let Some(msg) = scheduler_rx.recv().await {
        match msg {
            SchedulerMessage::ScheduleTimeout(callback_id, delay_ms) => {
                let callback_tx = callback_tx.clone();
                let handle = tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let _ = callback_tx.send(CallbackMessage::ExecuteTimeout(callback_id));
                });
                running_tasks.insert(callback_id, handle);
            }
            SchedulerMessage::ScheduleInterval(callback_id, interval_ms) => {
                let callback_tx = callback_tx.clone();
                let handle = tokio::task::spawn_local(async move {
                    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                    interval.tick().await;

                    loop {
                        interval.tick().await;

                        if callback_tx
                            .send(CallbackMessage::ExecuteInterval(callback_id))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                running_tasks.insert(callback_id, handle);
            }
            SchedulerMessage::FetchStreaming(promise_id, request) => {
                // Fetch via runner's OperationsHandler
                let callback_tx = callback_tx.clone();
                let manager = stream_manager.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result = execute_fetch_via_ops(request, manager, ops).await;

                    match result {
                        Ok((meta, stream_id)) => {
                            let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                promise_id, meta, stream_id,
                            ));
                        }
                        Err(e) => {
                            let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, e));
                        }
                    }
                });
            }
            SchedulerMessage::BindingFetch(promise_id, binding_name, request) => {
                // Fetch via binding (runner injects auth)
                let callback_tx = callback_tx.clone();
                let manager = stream_manager.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result =
                        execute_binding_fetch_via_ops(binding_name, request, manager, ops).await;

                    match result {
                        Ok((meta, stream_id)) => {
                            let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                promise_id, meta, stream_id,
                            ));
                        }
                        Err(e) => {
                            let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, e));
                        }
                    }
                });
            }
            SchedulerMessage::BindingStorage(callback_id, binding_name, storage_op) => {
                // Storage operation via binding
                let callback_tx = callback_tx.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result = ops
                        .handle(Operation::BindingStorage {
                            binding: binding_name,
                            op: storage_op,
                        })
                        .await;

                    let storage_result = match result {
                        OperationResult::Storage(r) => r,
                        _ => StorageResult::Error("Unexpected result type".into()),
                    };

                    let _ = callback_tx
                        .send(CallbackMessage::StorageResult(callback_id, storage_result));
                });
            }
            SchedulerMessage::BindingKv(callback_id, binding_name, kv_op) => {
                // KV operation via binding
                let callback_tx = callback_tx.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result = ops
                        .handle(Operation::BindingKv {
                            binding: binding_name,
                            op: kv_op,
                        })
                        .await;

                    let kv_result = match result {
                        OperationResult::Kv(r) => r,
                        _ => KvResult::Error("Unexpected result type".into()),
                    };

                    let _ = callback_tx.send(CallbackMessage::KvResult(callback_id, kv_result));
                });
            }
            SchedulerMessage::BindingDatabase(callback_id, binding_name, database_op) => {
                // Database operation via binding
                let callback_tx = callback_tx.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result = ops
                        .handle(Operation::BindingDatabase {
                            binding: binding_name,
                            op: database_op,
                        })
                        .await;

                    let database_result = match result {
                        OperationResult::Database(r) => r,
                        _ => DatabaseResult::Error("Unexpected result type".into()),
                    };

                    let _ = callback_tx.send(CallbackMessage::DatabaseResult(
                        callback_id,
                        database_result,
                    ));
                });
            }
            SchedulerMessage::BindingWorker(callback_id, binding_name, request) => {
                // Worker binding - execute target worker
                let callback_tx = callback_tx.clone();
                let manager = stream_manager.clone();
                let ops = ops.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let result = ops
                        .handle(Operation::BindingWorker {
                            binding: binding_name,
                            request,
                        })
                        .await;

                    // Worker binding returns HTTP response, same as fetch
                    match result {
                        OperationResult::Http(Ok(response)) => {
                            // Convert to streaming response
                            let meta = HttpResponseMeta {
                                status: response.status,
                                status_text: String::new(),
                                headers: response.headers.into_iter().collect(),
                            };

                            let stream_id = manager.create_stream("worker_binding".to_string());

                            // Handle the response body
                            match response.body {
                                ResponseBody::None => {
                                    let _ = manager
                                        .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                                        .await;
                                }
                                ResponseBody::Bytes(bytes) => {
                                    let _ = manager
                                        .write_chunk(
                                            stream_id,
                                            stream_manager::StreamChunk::Data(bytes),
                                        )
                                        .await;
                                    let _ = manager
                                        .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                                        .await;
                                }
                                ResponseBody::Stream(mut rx) => {
                                    let mgr = manager.clone();

                                    // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                                    // when the LocalSet is dropped (production pattern with thread-pinned pool)
                                    tokio::spawn(async move {
                                        while let Some(result) = rx.recv().await {
                                            match result {
                                                Ok(bytes) => {
                                                    if mgr
                                                        .write_chunk(
                                                            stream_id,
                                                            stream_manager::StreamChunk::Data(
                                                                bytes,
                                                            ),
                                                        )
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    let _ = mgr
                                                        .write_chunk(
                                                            stream_id,
                                                            stream_manager::StreamChunk::Error(e),
                                                        )
                                                        .await;
                                                    return;
                                                }
                                            }
                                        }

                                        let _ = mgr
                                            .write_chunk(
                                                stream_id,
                                                stream_manager::StreamChunk::Done,
                                            )
                                            .await;
                                    });
                                }
                            }

                            let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                callback_id,
                                meta,
                                stream_id,
                            ));
                        }
                        OperationResult::Http(Err(e)) => {
                            let _ = callback_tx.send(CallbackMessage::FetchError(callback_id, e));
                        }
                        _ => {
                            let _ = callback_tx.send(CallbackMessage::FetchError(
                                callback_id,
                                "Unexpected result type for worker binding".into(),
                            ));
                        }
                    }
                });
            }
            SchedulerMessage::StreamRead(callback_id, stream_id) => {
                // Read next chunk from a stream
                let callback_tx = callback_tx.clone();
                let manager = stream_manager.clone();

                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let chunk = match manager.read_chunk(stream_id).await {
                        Ok(chunk) => chunk,
                        Err(e) => stream_manager::StreamChunk::Error(e),
                    };
                    let _ = callback_tx.send(CallbackMessage::StreamChunk(callback_id, chunk));
                });
            }
            SchedulerMessage::StreamCancel(stream_id) => {
                // Cancel/close a stream
                stream_manager.close_stream(stream_id);
            }
            SchedulerMessage::ClearTimer(callback_id) => {
                if let Some(handle) = running_tasks.remove(&callback_id) {
                    handle.abort();
                }
            }
            SchedulerMessage::Log(level, message) => {
                // Fire-and-forget log via ops
                let ops = ops.clone();
                // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
                // when the LocalSet is dropped (production pattern with thread-pinned pool)
                tokio::spawn(async move {
                    let _ = ops.handle(Operation::Log { level, message }).await;
                });
            }
            SchedulerMessage::Shutdown => {
                for (_, handle) in running_tasks.drain() {
                    handle.abort();
                }
                break;
            }
        }
    }
}

/// Execute fetch via OperationsHandler, converting HttpResponse to StreamManager pattern
async fn execute_fetch_via_ops(
    request: HttpRequest,
    stream_manager: Arc<stream_manager::StreamManager>,
    ops: OperationsHandle,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    // Call the OperationsHandler with Fetch operation
    let result = ops.handle(Operation::Fetch(request)).await;
    convert_fetch_result_to_stream(result, stream_manager).await
}

/// Execute binding fetch via OperationsHandler
async fn execute_binding_fetch_via_ops(
    binding_name: String,
    request: HttpRequest,
    stream_manager: Arc<stream_manager::StreamManager>,
    ops: OperationsHandle,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    // Call the OperationsHandler with BindingFetch operation
    let result = ops
        .handle(Operation::BindingFetch {
            binding: binding_name,
            request,
        })
        .await;
    convert_fetch_result_to_stream(result, stream_manager).await
}

/// Convert OperationResult to (HttpResponseMeta, StreamId)
async fn convert_fetch_result_to_stream(
    result: OperationResult,
    stream_manager: Arc<stream_manager::StreamManager>,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    // Extract the HTTP response
    let response = match result {
        OperationResult::Http(r) => r?,
        OperationResult::Ack => return Err("Unexpected Ack result for fetch".into()),
        OperationResult::Storage(_) => return Err("Unexpected Storage result for fetch".into()),
        OperationResult::Kv(_) => return Err("Unexpected Kv result for fetch".into()),
        OperationResult::Database(_) => return Err("Unexpected Database result for fetch".into()),
    };

    // Convert HttpResponse to (HttpResponseMeta, StreamId)
    let meta = HttpResponseMeta {
        status: response.status,
        status_text: status_text(response.status),
        headers: response.headers.into_iter().collect(),
    };

    // Create a stream in the StreamManager
    let stream_id = stream_manager.create_stream("ops_fetch".to_string());

    // Handle the response body
    match response.body {
        ResponseBody::None => {
            // No body - just signal done
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Bytes(bytes) => {
            // Buffered body - write all bytes then done
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                .await;
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Stream(mut rx) => {
            // Streaming body - spawn task to forward chunks
            let manager = stream_manager.clone();

            // IMPORTANT: Use tokio::spawn (not spawn_local) so this task survives
            // when the LocalSet is dropped (production pattern with thread-pinned pool)
            // Also allows this to be called from tokio::spawn contexts (BindingFetch, etc.)
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    match result {
                        Ok(bytes) => {
                            if manager
                                .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = manager
                                .write_chunk(stream_id, stream_manager::StreamChunk::Error(e))
                                .await;
                            return;
                        }
                    }
                }

                let _ = manager
                    .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                    .await;
            });
        }
    }

    Ok((meta, stream_id))
}

/// Get the status text for an HTTP status code
fn status_text(status: u16) -> String {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
    .to_string()
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
