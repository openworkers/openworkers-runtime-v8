//! A worker that owns its isolate. It runs every event through the same
//! [`ExecutionContext`] as the pool, on an isolate no other worker uses.

use crate::execution_context::ExecutionContext;
use crate::locker_managed_isolate::LockerManagedIsolate;
use openworkers_core::{
    Event, OperationsHandle, RuntimeLimits, Script, TerminationReason, WorkerCode,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use v8;

/// One script on its own V8 isolate, for one event at a time.
///
/// **For production:** use [`crate::execute_pinned`], which shares isolates
/// between workers and keeps contexts warm.
pub struct Worker {
    /// Taken by `drop`, which holds the isolate's lock while the context's
    /// V8 handles go.
    context: Option<ExecutionContext>,
    isolate: Arc<LockerManagedIsolate>,
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

    /// Build a Worker with a new V8 isolate
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

    /// Create a new worker with an OperationsHandler
    ///
    /// All operations (fetch, log, etc.) go through the runner's OperationsHandler.
    pub async fn new_with_ops(
        script: Script,
        limits: Option<RuntimeLimits>,
        ops: OperationsHandle,
    ) -> Result<Self, TerminationReason> {
        let limits = limits.unwrap_or_default();
        let isolate = Arc::new(LockerManagedIsolate::new(limits.clone()));

        let context = {
            let (mut locker, _js_lock) = isolate.lock();

            ExecutionContext::new_with_pooled_isolate(
                &mut locker,
                Arc::clone(&isolate),
                isolate.use_snapshot,
                isolate.platform,
                limits,
                script,
                ops,
            )?
        };

        Ok(Self {
            context: Some(context),
            isolate,
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

    fn context(&mut self) -> &mut ExecutionContext {
        self.context.as_mut().expect("the context lives until drop")
    }

    /// Runs one event to its end: the answer, the response body, and every
    /// waitUntil promise.
    pub async fn exec(&mut self, task: Event) -> Result<(), TerminationReason> {
        let context = self.context();

        // reset() clears the flag, so an abort before the event is read first.
        if context.request.aborted.load(Ordering::SeqCst) {
            return Err(TerminationReason::Aborted);
        }

        context.reset().map_err(TerminationReason::Other)?;
        context.begin_request(None);
        context.exec(task).await?;
        context.drain_waituntil().await
    }

    /// How the fetch listener of the last event called respondWith.
    pub fn listener_marks(&self) -> crate::ListenerMarks {
        self.context
            .as_ref()
            .expect("the context lives until drop")
            .listener_marks()
    }

    /// Abort the worker execution
    pub fn abort(&mut self) {
        self.context().abort();
    }

    /// Process pending callbacks (timers, etc.)
    pub fn process_callbacks(&mut self) {
        let (_locker, _js_lock) = self.isolate.lock();

        self.context
            .as_mut()
            .expect("the context lives until drop")
            .process_callbacks();
    }

    /// Get the stream manager for creating/managing native streams
    pub fn stream_manager(&self) -> Arc<crate::runtime::stream_manager::StreamManager> {
        let context = self.context.as_ref().expect("the context lives until drop");

        Arc::clone(&context.request.stream_manager)
    }

    /// Evaluate JavaScript code (for testing/advanced use)
    pub fn evaluate(&mut self, code: &str) -> Result<(), String> {
        let (_locker, _js_lock) = self.isolate.lock();

        self.context
            .as_mut()
            .expect("the context lives until drop")
            .evaluate(&WorkerCode::JavaScript(code.to_string()))
    }

    /// Runs `f` on the locked isolate and the worker's context (for testing).
    pub fn with_isolate<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut v8::Isolate, &v8::Global<v8::Context>) -> R,
    {
        let (mut locker, _js_lock) = self.isolate.lock();
        let context = self.context.as_ref().expect("the context lives until drop");

        f(&mut locker, &context.request.context)
    }

    /// Read a global variable as u32 (for testing/debugging)
    pub fn get_global_u32(&mut self, name: &str) -> Option<u32> {
        self.with_isolate(|isolate, context| {
            use std::pin::pin;

            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);

            let key = v8::String::new(scope, name)?;
            let value = global.get(scope, key.into())?;
            value.uint32_value(scope)
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _locker = self.isolate.isolate.lock();

        self.context.take();
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

/// Read by every context created after it is set, so a host sets it once,
/// before the first worker.
static STRICT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Holds the event model to the specs where it is lax by default. A fetch
/// listener that calls respondWith after the dispatch gets
/// InvalidStateError, and a dispatch that ends without it fails at once
/// (Service Worker spec). A response body chunk that is not a Uint8Array
/// errors the body (Fetch spec). Off by default, which accepts both.
pub fn set_strict(strict: bool) {
    STRICT.store(strict, Ordering::Relaxed);
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

    let options = v8::Object::new(scope);
    let key = v8::String::new(scope, "strict").unwrap();
    let strict = v8::Boolean::new(scope, STRICT.load(Ordering::Relaxed));
    options.set(scope, key.into(), strict.into());

    let receiver = v8::undefined(scope).into();

    let code = v8::String::new(scope, include_str!("js/stream_body.js")).unwrap();
    let engine = v8::Script::compile(scope, code, None)
        .and_then(|script| script.run(scope))
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
        .and_then(|factory| factory.call(scope, receiver, &[options.into()]))
        .ok_or("stream_body.js does not answer an engine")?;

    let dispatch = install
        .call(scope, receiver, &[engine, options.into()])
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
