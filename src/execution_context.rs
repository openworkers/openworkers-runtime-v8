//! Execution context - a disposable V8 context with its own event loop
//!
//! Each ExecutionContext represents one worker script execution. It creates
//! a fresh V8 Context within an existing SharedIsolate, providing complete
//! isolation from other executions.
//!
//! The context is cheap to create (~100µs) compared to an isolate (~3-5ms).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use tokio::sync::{mpsc, Notify};
use v8;

use crate::runtime::{bindings, crypto, streams, text_encoding};
use crate::runtime::{CallbackId, CallbackMessage, SchedulerMessage};
use crate::runtime::stream_manager;
use crate::shared_isolate::SharedIsolate;
use openworkers_core::{OperationsHandle, RuntimeLimits, Script, Task, TerminationReason, WorkerCode};

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
        Self::setup_env(&mut shared_isolate.isolate, &context, &script.env, &script.bindings)?;

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
    fn setup_event_listener(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        // TODO: Move the setup_event_listener logic here
        // For now, return Ok
        Ok(())
    }

    /// Helper: Setup environment
    fn setup_env(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
        _env: &Option<HashMap<String, String>>,
        _bindings: &Vec<openworkers_core::BindingInfo>,
    ) -> Result<(), TerminationReason> {
        // TODO: Move the setup_env logic here
        Ok(())
    }

    /// Helper: Evaluate script
    fn evaluate_script(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
        _code: &WorkerCode,
    ) -> Result<(), TerminationReason> {
        // TODO: Move the evaluate logic here
        Ok(())
    }

    /// Helper: Setup ES modules handler
    fn setup_es_modules_handler(
        _isolate: &mut v8::OwnedIsolate,
        _context: &v8::Global<v8::Context>,
    ) -> Result<(), TerminationReason> {
        // TODO: Move the setup_es_modules_handler logic here
        Ok(())
    }

    /// Get a mutable reference to the isolate
    ///
    /// # Safety
    /// This is safe because we have exclusive access during the context lifetime
    fn isolate_mut(&mut self) -> &mut v8::OwnedIsolate {
        unsafe { &mut *self.isolate }
    }

    /// Execute a task in this context
    pub async fn exec(&mut self, _task: Task) -> Result<(), TerminationReason> {
        // TODO: Implement task execution
        // This will be similar to Worker::exec but using self.context
        Ok(())
    }

    /// Abort execution
    pub fn abort(&mut self) {
        self.aborted.store(true, std::sync::atomic::Ordering::SeqCst);
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
