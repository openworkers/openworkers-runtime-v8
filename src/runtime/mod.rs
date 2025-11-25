pub mod bindings;
pub mod fetch;
pub mod streams;
pub mod text_encoding;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use v8;

pub use fetch::{FetchRequest, FetchResponse};

pub type CallbackId = u64;

pub enum SchedulerMessage {
    ScheduleTimeout(CallbackId, u64),
    ScheduleInterval(CallbackId, u64),
    ClearTimer(CallbackId),
    Fetch(CallbackId, FetchRequest),
    Shutdown,
}

pub enum CallbackMessage {
    ExecuteTimeout(CallbackId),
    ExecuteInterval(CallbackId),
    FetchSuccess(CallbackId, FetchResponse),
    FetchError(CallbackId, String),
}

pub struct Runtime {
    pub isolate: v8::OwnedIsolate,
    pub context: v8::Global<v8::Context>,
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,
    pub(crate) fetch_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub(crate) _next_callback_id: Arc<Mutex<CallbackId>>,
    /// Channel for fetch response (set during fetch event execution)
    pub(crate) fetch_response_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<String>>>>,
    /// V8 Platform (for pump_message_loop)
    platform: &'static v8::SharedRef<v8::Platform>,
}

impl Runtime {
    pub fn new() -> (
        Self,
        mpsc::UnboundedReceiver<SchedulerMessage>,
        mpsc::UnboundedSender<CallbackMessage>,
    ) {
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
        let next_callback_id = Arc::new(Mutex::new(1));
        let fetch_response_tx = Arc::new(Mutex::new(None));

        // Load snapshot once and cache it in static memory
        static SNAPSHOT: OnceLock<Option<&'static [u8]>> = OnceLock::new();

        let snapshot_ref = SNAPSHOT.get_or_init(|| {
            const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");
            std::fs::read(RUNTIME_SNAPSHOT_PATH)
                .ok()
                .map(|bytes| Box::leak(bytes.into_boxed_slice()) as &'static [u8])
        });

        // Create isolate with or without snapshot
        let mut isolate = if let Some(snapshot_data) = snapshot_ref {
            let params = v8::CreateParams::default().snapshot_blob((*snapshot_data).into());
            v8::Isolate::new(params)
        } else {
            v8::Isolate::new(Default::default())
        };

        let use_snapshot = snapshot_ref.is_some();

        let context = {
            use std::pin::pin;
            let scope = pin!(v8::HandleScope::new(&mut isolate));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);

            // Always setup native bindings (not in snapshot)
            bindings::setup_console(scope);
            bindings::setup_timers(scope, scheduler_tx.clone());
            bindings::setup_fetch(
                scope,
                scheduler_tx.clone(),
                fetch_callbacks.clone(),
                next_callback_id.clone(),
            );

            // Only setup pure JS APIs if no snapshot (they're in the snapshot)
            if !use_snapshot {
                text_encoding::setup_text_encoding(scope);
                streams::setup_readable_stream(scope);
                bindings::setup_url(scope);
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
            _next_callback_id: next_callback_id,
            fetch_response_tx,
            platform,
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
                CallbackMessage::FetchSuccess(callback_id, response) => {
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt
                        && let Ok(response_obj) =
                            fetch::response::create_response_object(scope, response)
                    {
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[response_obj.into()]);
                    }
                }
                CallbackMessage::FetchError(callback_id, error_msg) => {
                    let callback_opt = {
                        let mut cbs = self.fetch_callbacks.lock().unwrap();
                        cbs.remove(&callback_id)
                    };

                    if let Some(callback_global) = callback_opt {
                        let error = v8::String::new(scope, &error_msg).unwrap();
                        let callback = v8::Local::new(scope, &callback_global);
                        let recv = v8::undefined(scope);
                        callback.call(scope, recv.into(), &[error.into()]);
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

        let code = v8::String::new(scope, script).ok_or("Failed to create script")?;
        let script_obj =
            v8::Script::compile(scope, code, None).ok_or("Failed to compile script")?;
        script_obj.run(scope).ok_or("Failed to execute script")?;

        Ok(())
    }
}

pub async fn run_event_loop(
    mut scheduler_rx: mpsc::UnboundedReceiver<SchedulerMessage>,
    callback_tx: mpsc::UnboundedSender<CallbackMessage>,
) {
    let mut running_tasks: HashMap<CallbackId, tokio::task::JoinHandle<()>> = HashMap::new();

    while let Some(msg) = scheduler_rx.recv().await {
        match msg {
            SchedulerMessage::ScheduleTimeout(callback_id, delay_ms) => {
                let callback_tx = callback_tx.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    let _ = callback_tx.send(CallbackMessage::ExecuteTimeout(callback_id));
                });
                running_tasks.insert(callback_id, handle);
            }
            SchedulerMessage::ScheduleInterval(callback_id, interval_ms) => {
                let callback_tx = callback_tx.clone();
                let handle = tokio::spawn(async move {
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
            SchedulerMessage::Fetch(promise_id, request) => {
                let callback_tx = callback_tx.clone();
                tokio::spawn(async move {
                    match fetch::request::execute_fetch(request).await {
                        Ok(response) => {
                            let _ = callback_tx
                                .send(CallbackMessage::FetchSuccess(promise_id, response));
                        }
                        Err(e) => {
                            let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, e));
                        }
                    }
                });
            }
            SchedulerMessage::ClearTimer(callback_id) => {
                if let Some(handle) = running_tasks.remove(&callback_id) {
                    handle.abort();
                }
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

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.scheduler_tx.send(SchedulerMessage::Shutdown);
    }
}
