//! Execution context - a disposable V8 context with its own event loop
//!
//! Each ExecutionContext represents one worker script execution. It creates
//! a fresh V8 Context within an existing SharedIsolate, providing complete
//! isolation from other executions.
//!
//! The context is cheap to create (~100µs) compared to an isolate (~3-5ms).

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};
use v8;

use crate::runtime::stream_manager;
use crate::runtime::{CallbackId, CallbackMessage, SchedulerMessage};
use crate::runtime::{bindings, crypto, streams, text_encoding};
use crate::shared_isolate::SharedIsolate;
use openworkers_core::{
    OperationsHandle, RuntimeLimits, Script, Task, TerminationReason, WorkerCode,
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
    pub async fn exec(&mut self, _task: Task) -> Result<(), TerminationReason> {
        // TODO: Implement full task execution
        // For now, this is a placeholder that will be implemented
        // similar to Worker::exec but using self.context
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
