//! A Durable Object: one instance of a class the worker exports, in a
//! context that lives across its events.
//!
//! Its events interleave at their awaits, as on Cloudflare. Each turn of the
//! object gets the wall clock and CPU limits of an event, and a turn that
//! goes over them stops the whole object: the events in flight fail, and
//! the next event builds the object again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, Sleep};

use super::ExecutionContext;
use crate::LockerManagedIsolate;
use crate::execution_helpers::{
    EventLoopExit, check_exit_condition, get_completion_state, get_response_stream_id,
    read_response_object, read_response_socket, signal_client_disconnect, trigger_fetch_handler,
};
use crate::security::{CpuEnforcer, TimeoutGuard, TurnGuard, next_request_id};
use openworkers_core::{
    FetchInit, OperationsHandle, RequestBody, ResponseSender, RuntimeLimits, Script,
    TerminationReason,
};

/// How long a blockConcurrencyWhile callback may hold the events back.
const GATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Rounds of callbacks a turn runs before it lets the thread go.
const MAX_ROUNDS: usize = 4;

/// What a Durable Object is built from.
#[derive(Clone)]
pub struct ObjectSpec {
    pub script: Script,
    /// The name the worker exports the class under
    pub class_name: String,
    pub id: String,
    pub limits: RuntimeLimits,
    pub ops: OperationsHandle,
    /// The object is dropped after this long with no event in flight
    pub idle: Duration,
}

/// Serves the events of one object until the mailbox closes. The object is
/// built for the first event, dropped when it is idle or stopped, and built
/// again for the next event.
pub async fn run(spec: ObjectSpec, mut mailbox: mpsc::UnboundedReceiver<FetchInit>) {
    while let Some(first) = mailbox.recv().await {
        let object = match DurableObject::new(&spec) {
            Ok(object) => object,
            Err(reason) => {
                tracing::warn!("durable object {}: not built: {reason:?}", spec.id);
                continue;
            }
        };

        if let Err(reason) = object.serve(first, &mut mailbox, spec.idle).await {
            tracing::warn!("durable object {}: stopped: {reason:?}", spec.id);
        }
    }
}

/// An event the object answers.
struct InFlight {
    handle: v8::Global<v8::Object>,
    /// Taken when the response goes out
    res_tx: Option<ResponseSender>,
    /// Set once the guest knows that the client hung up
    disconnected: bool,
}

/// Whether the object takes events.
enum Gate {
    Open,
    /// A blockConcurrencyWhile callback runs
    Closed,
    /// A blockConcurrencyWhile callback failed
    Broken(String),
}

/// What a turn did.
struct TurnOutcome {
    busy: bool,
    gate: Gate,
}

pub struct DurableObject {
    isolate: Arc<LockerManagedIsolate>,
    context: Option<ExecutionContext>,
}

impl DurableObject {
    /// Builds the object in an isolate of its own: runs the worker script,
    /// then the constructor of its class.
    pub fn new(spec: &ObjectSpec) -> Result<Self, TerminationReason> {
        let isolate = Arc::new(LockerManagedIsolate::new(spec.limits.clone()));

        let context = {
            let (mut locker, _js_lock) = isolate.lock();

            ExecutionContext::new_with_pooled_isolate(
                &mut locker,
                Arc::clone(&isolate),
                isolate.use_snapshot,
                isolate.platform,
                spec.limits.clone(),
                spec.script.clone(),
                Arc::clone(&spec.ops),
            )?
        };

        let mut object = Self {
            isolate,
            context: Some(context),
        };

        object.context().construct(&spec.class_name, &spec.id)?;

        Ok(object)
    }

    fn context(&mut self) -> &mut ExecutionContext {
        self.context.as_mut().expect("the context lives until drop")
    }

    /// Answers `first`, then the events of the mailbox, until the object is
    /// idle for `idle` or the mailbox closes. An error is what stopped the
    /// object; the events it had taken fail with it.
    pub async fn serve(
        mut self,
        first: FetchInit,
        mailbox: &mut mpsc::UnboundedReceiver<FetchInit>,
        idle: Duration,
    ) -> Result<(), TerminationReason> {
        self.context().serve(first, mailbox, idle).await
    }
}

impl Drop for DurableObject {
    fn drop(&mut self) {
        let _locker = self.isolate.lock();

        self.context.take();
    }
}

impl ExecutionContext {
    /// Runs the constructor of the class exported as `class_name`, and puts
    /// the dispatch of the object in place of the dispatch of the worker.
    fn construct(&mut self, class_name: &str, id: &str) -> Result<(), TerminationReason> {
        use std::pin::pin;

        self.request_id = next_request_id();
        let pooled = Arc::clone(&self.pooled);
        let _lock = pooled.lock();
        let wall = TimeoutGuard::new(
            Arc::clone(&pooled.turn),
            self.request_id,
            self.limits.max_wall_clock_time_ms,
        );
        let cpu = CpuEnforcer::new(
            Arc::clone(&pooled.turn),
            self.request_id,
            self.limits.max_cpu_time_ms,
        );
        let turn = TurnGuard::begin(
            &pooled.turn,
            self.request_id,
            cpu.as_ref(),
            &self.memory_hit,
        );

        let result = {
            let mut isolate = self.isolate();
            let scope = pin!(v8::HandleScope::new(&mut isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, &self.request.context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let tc = pin!(v8::TryCatch::new(scope));
            let tc = tc.init();

            let source = v8::String::new(&tc, include_str!("../js/durable_object.js")).unwrap();
            let install = v8::Script::compile(&tc, source, None)
                .and_then(|script| script.run(&tc))
                .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
                .expect("durable_object.js evaluates to a function");

            let dispatch = v8::Local::new(&tc, &self.request.dispatch);
            let class_name = v8::String::new(&tc, class_name).unwrap();
            let id = v8::String::new(&tc, id).unwrap();
            let receiver = v8::undefined(&tc).into();
            let args = [dispatch.into(), class_name.into(), id.into()];

            match install.call(&tc, receiver, &args) {
                Some(object) => Ok(v8::Global::new(&tc, object.to_object(&tc).unwrap())),
                None if tc.has_terminated() => Err(None),
                None => Err(Some(
                    tc.exception()
                        .map(|e| e.to_rust_string_lossy(&tc))
                        .unwrap_or_else(|| "the constructor failed".to_string()),
                )),
            }
        };

        drop(turn);

        if let Some(reason) = self.stopped(wall.was_triggered(), &cpu) {
            return Err(reason);
        }

        match result {
            Ok(object) => {
                self.request.dispatch = object;
                Ok(())
            }
            Err(Some(message)) => Err(TerminationReason::Exception(message)),
            Err(None) => Err(TerminationReason::Other("Execution terminated".to_string())),
        }
    }

    /// The limit a turn went over, if any.
    fn stopped(&self, wall_hit: bool, cpu: &Option<CpuEnforcer>) -> Option<TerminationReason> {
        if cpu.as_ref().is_some_and(|cpu| cpu.was_terminated()) {
            return Some(TerminationReason::CpuTimeLimit);
        }

        if wall_hit {
            return Some(TerminationReason::WallClockTimeout);
        }

        if self.memory_hit.load(Ordering::SeqCst) {
            return Some(TerminationReason::MemoryLimit);
        }

        None
    }

    async fn serve(
        &mut self,
        first: FetchInit,
        mailbox: &mut mpsc::UnboundedReceiver<FetchInit>,
        idle: Duration,
    ) -> Result<(), TerminationReason> {
        let pooled = Arc::clone(&self.pooled);
        let mut foreground = self.pooled.foreground.waiter();
        let cpu = CpuEnforcer::new(
            Arc::clone(&pooled.turn),
            self.request_id,
            self.limits.max_cpu_time_ms,
        );
        let mut queue = VecDeque::from([first]);
        let mut inflight: Vec<InFlight> = Vec::new();
        let mut pending = Vec::with_capacity(16);
        let mut closed = false;
        let mut idle_timer: std::pin::Pin<Box<Sleep>> = Box::pin(tokio::time::sleep(idle));
        let mut gate_timer: Option<std::pin::Pin<Box<Sleep>>> = None;
        let memory_hit = Arc::clone(&self.memory_hit);

        let result = std::future::poll_fn(|cx| {
            self.request.stream_manager.register_waker(cx.waker());
            foreground.register(cx.waker());

            let _lock = pooled.lock();

            if let Some(cpu) = &cpu {
                cpu.refill();
            }

            let turn_wall = TimeoutGuard::new(
                Arc::clone(&pooled.turn),
                self.request_id,
                self.limits.max_wall_clock_time_ms,
            );
            let turn = TurnGuard::begin(&pooled.turn, self.request_id, cpu.as_ref(), &memory_hit);
            let outcome = self.turn(
                cx,
                mailbox,
                &mut queue,
                &mut inflight,
                &mut pending,
                &mut closed,
            );
            drop(turn);

            if let Some(reason) = self.stopped(turn_wall.was_triggered(), &cpu) {
                return Poll::Ready(Err(reason));
            }

            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(message) => return Poll::Ready(Err(TerminationReason::Exception(message))),
            };

            match outcome.gate {
                Gate::Broken(message) => {
                    return Poll::Ready(Err(TerminationReason::Exception(message)));
                }
                Gate::Closed => {
                    let timer = gate_timer
                        .get_or_insert_with(|| Box::pin(tokio::time::sleep(GATE_TIMEOUT)));

                    if timer.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(TerminationReason::Other(
                            "blockConcurrencyWhile held the object for too long".to_string(),
                        )));
                    }
                }
                Gate::Open => gate_timer = None,
            }

            if !inflight.is_empty() || outcome.busy {
                idle_timer.as_mut().reset(Instant::now() + idle);
            }

            if inflight.is_empty() && (closed || idle_timer.as_mut().poll(cx).is_ready()) {
                return Poll::Ready(Ok(()));
            }

            Poll::Pending
        })
        .await;

        // The handles are V8 objects, freed under the lock
        let _lock = pooled.lock();
        drop(inflight);

        result
    }

    /// One turn of the object: takes the events the gate lets in, runs the
    /// callbacks, and answers the events that are ready. The caller holds
    /// the lock and the turn.
    fn turn(
        &mut self,
        cx: &mut std::task::Context<'_>,
        mailbox: &mut mpsc::UnboundedReceiver<FetchInit>,
        queue: &mut VecDeque<FetchInit>,
        inflight: &mut Vec<InFlight>,
        pending: &mut Vec<crate::runtime::CallbackMessage>,
        closed: &mut bool,
    ) -> Result<TurnOutcome, String> {
        let mut busy = false;

        for round in 0..MAX_ROUNDS {
            let mut admitted = 0;

            while matches!(self.gate(), Gate::Open) {
                let init = match queue.pop_front() {
                    Some(init) => init,
                    None => match mailbox.poll_recv(cx) {
                        Poll::Ready(Some(init)) => init,
                        Poll::Ready(None) => {
                            *closed = true;
                            break;
                        }
                        Poll::Pending => break,
                    },
                };

                inflight.push(self.admit(init)?);
                admitted += 1;
            }

            let count = self.drain_and_process(cx, pending)?;
            let answered = self.answer(inflight);

            if admitted == 0 && count == 0 && !answered {
                break;
            }

            busy = true;

            // More work may be waiting, and nothing else wakes the loop for it
            if round == MAX_ROUNDS - 1 {
                cx.waker().wake_by_ref();
            }
        }

        Ok(TurnOutcome {
            busy,
            gate: self.gate(),
        })
    }

    fn gate(&mut self) -> Gate {
        use std::pin::pin;

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let object = v8::Local::new(scope, &self.request.dispatch);

        let call = |name: &str| {
            let key = v8::String::new(scope, name).unwrap();
            let function = object
                .get(scope, key.into())
                .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
                .expect("durable_object.js answers blocked and failure");

            function.call(scope, object.into(), &[])
        };

        let failure = call("failure").filter(|value| value.is_string());
        let blocked = call("blocked").is_some_and(|value| value.is_true());

        if let Some(failure) = failure {
            return Gate::Broken(failure.to_rust_string_lossy(scope));
        }

        if blocked { Gate::Closed } else { Gate::Open }
    }

    /// Hands the request to the object.
    fn admit(&mut self, init: FetchInit) -> Result<InFlight, String> {
        use std::pin::pin;

        let FetchInit {
            mut req, res_tx, ..
        } = init;

        let body_stream_id = match std::mem::take(&mut req.body) {
            RequestBody::Stream(rx) => Some(self.request.stream_manager.pump_request_body(rx)),
            body => {
                req.body = body;
                None
            }
        };

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let handle = trigger_fetch_handler(
            scope,
            &self.request.dispatch,
            &req.url,
            req.method.as_str(),
            &req.headers,
            &mut req.body,
            body_stream_id,
        )?;

        Ok(InFlight {
            handle,
            res_tx: Some(res_tx),
            disconnected: false,
        })
    }

    /// Sends the responses that are ready, tells the guest of the clients
    /// that hung up, and drops the events that are over. Answers whether it
    /// ran guest code or sent a response.
    fn answer(&mut self, inflight: &mut Vec<InFlight>) -> bool {
        use std::pin::pin;

        let stream_manager = Arc::clone(&self.request.stream_manager);
        let buffer_size = self.limits.stream_buffer_size;
        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let mut answered = false;

        inflight.retain_mut(|event| {
            let handle = Some(v8::Local::new(scope, &event.handle));

            if event.res_tx.is_some()
                && check_exit_condition(scope, handle, EventLoopExit::ResponseReady)
            {
                let res_tx = event.res_tx.take().unwrap();
                answered = true;

                if read_response_socket(scope, handle).is_some() {
                    tracing::warn!("a Durable Object cannot answer with a WebSocket yet");
                } else {
                    match read_response_object(scope, handle, &stream_manager, buffer_size) {
                        Ok((status, response)) if status != 101 => {
                            let _ = res_tx.send(response);
                        }
                        Ok(_) => tracing::warn!("a Durable Object cannot answer 101 yet"),
                        Err(error) => tracing::warn!("durable object response: {error}"),
                    }
                }
            }

            if event.res_tx.is_none() && !event.disconnected {
                let (_, streaming) = get_completion_state(scope, handle);

                if streaming
                    && let Some(stream_id) = get_response_stream_id(scope, handle)
                    && !stream_manager.has_sender(stream_id)
                {
                    event.disconnected = true;
                    answered = true;
                    signal_client_disconnect(scope, handle);
                }
            }

            event.res_tx.is_some()
                || !check_exit_condition(scope, handle, EventLoopExit::FullyComplete)
        });

        answered
    }
}
