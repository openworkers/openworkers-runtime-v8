pub mod bindings;
pub mod crypto;
pub mod stream_manager;
pub mod streams;
pub mod text_encoding;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use v8;

use crate::security::CustomAllocator;
use openworkers_core::{
    DatabaseOp, DatabaseResult, HttpRequest, HttpResponseMeta, KvOp, KvResult, Operation,
    OperationResult, OperationsHandle, ResponseBody, RuntimeLimits, StorageOp, StorageResult,
};

pub type CallbackId = u64;

pub enum SchedulerMessage {
    ScheduleTimeout(CallbackId, u64),
    ScheduleInterval(CallbackId, u64),
    ClearTimer(CallbackId),
    FetchStreaming(CallbackId, HttpRequest), // Fetch with streaming (stream created internally)
    BindingFetch(CallbackId, String, HttpRequest), // Fetch via binding (binding_name, request)
    BindingStorage(CallbackId, String, StorageOp), // Storage operation (binding_name, op)
    BindingKv(CallbackId, String, KvOp),     // KV operation (binding_name, op)
    BindingDatabase(CallbackId, String, DatabaseOp), // Database operation (binding_name, op)
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
}

impl Runtime {
    pub fn new(
        limits: Option<RuntimeLimits>,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<SchedulerMessage>,
        mpsc::UnboundedSender<CallbackMessage>,
    ) {
        let limits = limits.unwrap_or_default();

        // Initialize V8 platform (once, globally) using OnceLock for safety
        use std::sync::OnceLock;
        static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();

        let platform = PLATFORM.get_or_init(|| {
            let platform = v8::new_default_platform(0, false).make_shared();
            v8::V8::initialize_platform(platform.clone());
            v8::V8::initialize();
            platform
        });

        let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
        let (callback_tx, callback_rx) = mpsc::unbounded_channel();

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

        // Load snapshot once and cache it in static memory
        static SNAPSHOT: OnceLock<Option<&'static [u8]>> = OnceLock::new();

        let snapshot_ref = SNAPSHOT.get_or_init(|| {
            const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");
            std::fs::read(RUNTIME_SNAPSHOT_PATH)
                .ok()
                .map(|bytes| Box::leak(bytes.into_boxed_slice()) as &'static [u8])
        });

        // Create isolate with custom allocator and heap limits
        let mut isolate = if let Some(snapshot_data) = snapshot_ref {
            let params = v8::CreateParams::default()
                .heap_limits(heap_initial, heap_max)
                .array_buffer_allocator(array_buffer_allocator.into_v8_allocator())
                .snapshot_blob((*snapshot_data).into());
            v8::Isolate::new(params)
        } else {
            let params = v8::CreateParams::default()
                .heap_limits(heap_initial, heap_max)
                .array_buffer_allocator(array_buffer_allocator.into_v8_allocator());
            v8::Isolate::new(params)
        };

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
        };

        (runtime, scheduler_rx, callback_tx)
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
                    // Fetch with streaming - call JS callback with metadata and stream_id
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
                        // Create metadata object for JS
                        let meta_obj = v8::Object::new(scope);

                        // status
                        let status_key = v8::String::new(scope, "status").unwrap();
                        let status_val = v8::Number::new(scope, meta.status as f64);
                        meta_obj.set(scope, status_key.into(), status_val.into());

                        // statusText
                        let status_text_key = v8::String::new(scope, "statusText").unwrap();
                        let status_text_val = v8::String::new(scope, &meta.status_text).unwrap();
                        meta_obj.set(scope, status_text_key.into(), status_text_val.into());

                        // headers as object
                        let headers_obj = v8::Object::new(scope);
                        for (key, value) in &meta.headers {
                            let k = v8::String::new(scope, key).unwrap();
                            let v = v8::String::new(scope, value).unwrap();
                            headers_obj.set(scope, k.into(), v.into());
                        }
                        let headers_key = v8::String::new(scope, "headers").unwrap();
                        meta_obj.set(scope, headers_key.into(), headers_obj.into());

                        // streamId
                        let stream_id_key = v8::String::new(scope, "streamId").unwrap();
                        let stream_id_val = v8::Number::new(scope, stream_id as f64);
                        meta_obj.set(scope, stream_id_key.into(), stream_id_val.into());

                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[meta_obj.into()]);
                    }
                }
                CallbackMessage::StreamChunk(callback_id, chunk) => {
                    // Stream read result - call the JavaScript callback with the chunk
                    let callback_opt = {
                        let mut cbs = self.stream_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);

                        // Create result object: {done: boolean, value?: Uint8Array, error?: string}
                        let result_obj = v8::Object::new(scope);

                        match chunk {
                            stream_manager::StreamChunk::Data(bytes) => {
                                // {done: false, value: Uint8Array}
                                let done_key = v8::String::new(scope, "done").unwrap();
                                let done_val = v8::Boolean::new(scope, false);
                                result_obj.set(scope, done_key.into(), done_val.into());

                                // Create Uint8Array from bytes using backing store transfer
                                // This converts Bytes -> Vec (1 copy) then transfers ownership to V8
                                let vec = bytes.to_vec();
                                let len = vec.len();
                                let backing_store =
                                    v8::ArrayBuffer::new_backing_store_from_vec(vec);
                                let array_buffer = v8::ArrayBuffer::with_backing_store(
                                    scope,
                                    &backing_store.make_shared(),
                                );
                                let uint8_array =
                                    v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();

                                let value_key = v8::String::new(scope, "value").unwrap();
                                result_obj.set(scope, value_key.into(), uint8_array.into());
                            }
                            stream_manager::StreamChunk::Done => {
                                // {done: true}
                                let done_key = v8::String::new(scope, "done").unwrap();
                                let done_val = v8::Boolean::new(scope, true);
                                result_obj.set(scope, done_key.into(), done_val.into());
                            }
                            stream_manager::StreamChunk::Error(err_msg) => {
                                // {error: string}
                                let error_key = v8::String::new(scope, "error").unwrap();
                                let error_val = v8::String::new(scope, &err_msg).unwrap();
                                result_obj.set(scope, error_key.into(), error_val.into());
                            }
                        }

                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::StorageResult(callback_id, storage_result) => {
                    // Storage operation result - call the JavaScript callback
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);

                        // Create result object based on StorageResult type
                        let result_obj = v8::Object::new(scope);

                        match storage_result {
                            StorageResult::Body(maybe_body) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                if let Some(body) = maybe_body {
                                    let vec = body;
                                    let len = vec.len();
                                    let backing_store =
                                        v8::ArrayBuffer::new_backing_store_from_vec(vec);
                                    let array_buffer = v8::ArrayBuffer::with_backing_store(
                                        scope,
                                        &backing_store.make_shared(),
                                    );
                                    let uint8_array =
                                        v8::Uint8Array::new(scope, array_buffer, 0, len).unwrap();
                                    let body_key = v8::String::new(scope, "body").unwrap();
                                    result_obj.set(scope, body_key.into(), uint8_array.into());
                                }
                            }
                            StorageResult::Head { size, etag } => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                let size_key = v8::String::new(scope, "size").unwrap();
                                let size_val = v8::Number::new(scope, size as f64);
                                result_obj.set(scope, size_key.into(), size_val.into());

                                if let Some(etag_str) = etag {
                                    let etag_key = v8::String::new(scope, "etag").unwrap();
                                    let etag_val = v8::String::new(scope, &etag_str).unwrap();
                                    result_obj.set(scope, etag_key.into(), etag_val.into());
                                }
                            }
                            StorageResult::List { keys, truncated } => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                let arr = v8::Array::new(scope, keys.len() as i32);
                                for (i, key) in keys.iter().enumerate() {
                                    let key_val = v8::String::new(scope, key).unwrap();
                                    arr.set_index(scope, i as u32, key_val.into());
                                }
                                let keys_key = v8::String::new(scope, "keys").unwrap();
                                result_obj.set(scope, keys_key.into(), arr.into());

                                let truncated_key = v8::String::new(scope, "truncated").unwrap();
                                result_obj.set(
                                    scope,
                                    truncated_key.into(),
                                    v8::Boolean::new(scope, truncated).into(),
                                );
                            }
                            StorageResult::Error(err_msg) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, false).into(),
                                );

                                let error_key = v8::String::new(scope, "error").unwrap();
                                let error_val = v8::String::new(scope, &err_msg).unwrap();
                                result_obj.set(scope, error_key.into(), error_val.into());
                            }
                        }

                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::KvResult(callback_id, kv_result) => {
                    // KV operation result - call the JavaScript callback
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);

                        // Create result object based on KvResult type
                        let result_obj = v8::Object::new(scope);

                        match kv_result {
                            KvResult::Value(maybe_value) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                let value_key = v8::String::new(scope, "value").unwrap();
                                if let Some(value) = maybe_value {
                                    let value_val = v8::String::new(scope, &value).unwrap();
                                    result_obj.set(scope, value_key.into(), value_val.into());
                                } else {
                                    result_obj.set(scope, value_key.into(), v8::null(scope).into());
                                }
                            }
                            KvResult::Ok => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );
                            }
                            KvResult::Keys(keys) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                // Create JS array from keys
                                let keys_array =
                                    v8::Array::new(scope, keys.len().try_into().unwrap());

                                for (i, key) in keys.iter().enumerate() {
                                    let key_val = v8::String::new(scope, key).unwrap();
                                    keys_array.set_index(scope, i as u32, key_val.into());
                                }

                                let keys_key = v8::String::new(scope, "keys").unwrap();
                                result_obj.set(scope, keys_key.into(), keys_array.into());
                            }
                            KvResult::Error(err_msg) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, false).into(),
                                );

                                let error_key = v8::String::new(scope, "error").unwrap();
                                let error_val = v8::String::new(scope, &err_msg).unwrap();
                                result_obj.set(scope, error_key.into(), error_val.into());
                            }
                        }

                        callback.call(scope, recv.into(), &[result_obj.into()]);
                    }
                }
                CallbackMessage::DatabaseResult(callback_id, database_result) => {
                    // Database operation result - call the JavaScript callback
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);

                        // Create result object based on DatabaseResult type
                        let result_obj = v8::Object::new(scope);

                        match database_result {
                            DatabaseResult::Rows(rows_json) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, true).into(),
                                );

                                // Parse the JSON rows and set as rows property
                                let rows_key = v8::String::new(scope, "rows").unwrap();
                                let rows_str = v8::String::new(scope, &rows_json).unwrap();

                                // Parse JSON string to JS object
                                let parsed = v8::json::parse(scope, rows_str.into());

                                if let Some(parsed_value) = parsed {
                                    result_obj.set(scope, rows_key.into(), parsed_value);
                                } else {
                                    // Fallback: return the raw string if parsing fails
                                    result_obj.set(scope, rows_key.into(), rows_str.into());
                                }
                            }
                            DatabaseResult::Error(err_msg) => {
                                let success_key = v8::String::new(scope, "success").unwrap();
                                result_obj.set(
                                    scope,
                                    success_key.into(),
                                    v8::Boolean::new(scope, false).into(),
                                );

                                let error_key = v8::String::new(scope, "error").unwrap();
                                let error_val = v8::String::new(scope, &err_msg).unwrap();
                                result_obj.set(scope, error_key.into(), error_val.into());
                            }
                        }

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

    pub fn evaluate(&mut self, script: &str) -> Result<(), String> {
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut self.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

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
    stream_manager: Arc<stream_manager::StreamManager>,
    ops: OperationsHandle,
) {
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

                tokio::task::spawn_local(async move {
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

                tokio::task::spawn_local(async move {
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

                tokio::task::spawn_local(async move {
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

                tokio::task::spawn_local(async move {
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

                tokio::task::spawn_local(async move {
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
            SchedulerMessage::StreamRead(callback_id, stream_id) => {
                // Read next chunk from a stream
                let callback_tx = callback_tx.clone();
                let manager = stream_manager.clone();

                tokio::task::spawn_local(async move {
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
                tokio::task::spawn_local(async move {
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
        status_text: String::new(), // TODO: derive from status code
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

            tokio::task::spawn_local(async move {
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

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.scheduler_tx.send(SchedulerMessage::Shutdown);
    }
}
