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

use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, Sleep};

use super::ExecutionContext;
use crate::LockerManagedIsolate;
use crate::execution_helpers::{
    EventLoopExit, check_exit_condition, get_completion_state, get_response_stream_id,
    read_response_object, read_response_socket, signal_client_disconnect, trigger_fetch_handler,
};
use crate::runtime::{CallbackMessage, SchedulerMessage};
use crate::security::{CpuEnforcer, TimeoutGuard, TurnGuard, next_request_id};
use openworkers_core::{
    FetchInit, HttpResponse, OperationsHandle, RequestBody, ResponseBody, ResponseSender,
    RuntimeLimits, Script, TerminationReason, WebSocketId, WebSocketIncoming, WebSocketUpgrade,
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
    /// Once the mailbox closes, the events in flight fail after this long
    pub drain: Duration,
}

/// Serves the events of one object until the mailbox closes. The object is
/// built for the first event, dropped when it is idle or stopped, and built
/// again for the next event.
pub async fn run(spec: ObjectSpec, mut mailbox: mpsc::UnboundedReceiver<ObjectEvent>) {
    while let Some(first) = mailbox.recv().await {
        let object = match DurableObject::new(&spec) {
            Ok(object) => object,
            Err(reason) => {
                tracing::warn!("durable object {}: not built: {reason:?}", spec.id);
                continue;
            }
        };

        if let Err(reason) = object
            .serve(first, &mut mailbox, spec.idle, spec.drain)
            .await
        {
            tracing::warn!("durable object {}: stopped: {reason:?}", spec.id);
        }
    }
}

/// An event for an object.
pub enum ObjectEvent {
    Fetch(FetchInit),
    Alarm(AlarmInit),
    Socket(SocketEvent),
}

/// What came on a WebSocket of the object, which the runner holds.
pub struct SocketEvent {
    pub socket: WebSocketId,
    pub incoming: WebSocketIncoming,
}

/// The alarm of the object is due: its alarm() handler runs, and `done`
/// gets the outcome. The host retries an alarm that failed.
pub struct AlarmInit {
    /// How many times this alarm failed before
    pub retry_count: u32,
    pub done: oneshot::Sender<Result<(), String>>,
}

/// An event the object has taken.
enum InFlight {
    Fetch(FetchInFlight),
    Alarm(AlarmInFlight),
    /// What durable_object.js answers for the handler of a socket event
    Socket(v8::Global<v8::Object>),
}

struct FetchInFlight {
    handle: v8::Global<v8::Object>,
    /// Taken when the response goes out
    res_tx: Option<ResponseSender>,
    /// The socket of the client, for a 101 that hands over a socket
    upgrade: Option<WebSocketUpgrade>,
    /// Set once the guest knows that the client hung up
    disconnected: bool,
}

struct AlarmInFlight {
    /// What durable_object.js answers for the run of the handler
    run: v8::Global<v8::Object>,
    /// Taken when the handler ends
    done: Option<oneshot::Sender<Result<(), String>>>,
}

impl InFlight {
    /// Whether the caller no longer waits for the event. Registers the
    /// waker for when it leaves.
    fn caller_left(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        match self {
            InFlight::Fetch(event) => event
                .res_tx
                .as_mut()
                .is_some_and(|res_tx| res_tx.poll_closed(cx).is_ready()),
            InFlight::Alarm(event) => event
                .done
                .as_mut()
                .is_some_and(|done| done.poll_closed(cx).is_ready()),
            InFlight::Socket(_) => false,
        }
    }
}

/// What the object lets through.
enum Gate {
    State {
        /// A blockConcurrencyWhile callback runs: no event goes in
        blocked: bool,
        /// A storage write is under way: no response goes out
        writing: bool,
        /// A storage read is under way: nothing but storage results goes in
        reading: bool,
    },
    /// A blockConcurrencyWhile callback or a storage write failed
    Broken(String),
}

/// What a turn did.
struct TurnOutcome {
    /// It took an event
    admitted: bool,
    gate: Gate,
    /// A socket that is not hibernatable is open, and keeps the object
    pinned: bool,
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
    /// idle for `idle` or the mailbox closes. Once it closes, the events in
    /// flight have `drain` to end. An error is what stopped the object; the
    /// events it had taken fail with it.
    pub async fn serve(
        mut self,
        first: ObjectEvent,
        mailbox: &mut mpsc::UnboundedReceiver<ObjectEvent>,
        idle: Duration,
        drain: Duration,
    ) -> Result<(), TerminationReason> {
        self.context().serve(first, mailbox, idle, drain).await
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
        first: ObjectEvent,
        mailbox: &mut mpsc::UnboundedReceiver<ObjectEvent>,
        idle: Duration,
        drain: Duration,
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
        let mut pending = VecDeque::with_capacity(16);
        let mut closed = false;
        let mut idle_timer: std::pin::Pin<Box<Sleep>> = Box::pin(tokio::time::sleep(idle));
        let mut gate_timer: Option<std::pin::Pin<Box<Sleep>>> = None;
        let mut drain_timer: Option<std::pin::Pin<Box<Sleep>>> = None;
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
            let active = !inflight.is_empty();
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
                Gate::State { blocked: true, .. } => {
                    let timer = gate_timer
                        .get_or_insert_with(|| Box::pin(tokio::time::sleep(GATE_TIMEOUT)));

                    if timer.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(TerminationReason::Other(
                            "blockConcurrencyWhile held the object for too long".to_string(),
                        )));
                    }
                }
                Gate::State { blocked: false, .. } => gate_timer = None,
            }

            // A caller that left takes its event out of the count; the code
            // of the event may still run, but it keeps the object no longer
            inflight.retain_mut(|event| !event.caller_left(cx));

            // Timers do not keep the object: only events in flight and the
            // sockets whose listeners live in it do
            if active || outcome.admitted || outcome.pinned || !inflight.is_empty() {
                idle_timer.as_mut().reset(Instant::now() + idle);
            }

            if inflight.is_empty() && (closed || idle_timer.as_mut().poll(cx).is_ready()) {
                return Poll::Ready(Ok(()));
            }

            if closed {
                let timer = drain_timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(drain)));

                if timer.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Err(TerminationReason::Other(
                        "events in flight did not end after the object closed".to_string(),
                    )));
                }
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
        mailbox: &mut mpsc::UnboundedReceiver<ObjectEvent>,
        queue: &mut VecDeque<ObjectEvent>,
        inflight: &mut Vec<InFlight>,
        pending: &mut VecDeque<CallbackMessage>,
        closed: &mut bool,
    ) -> Result<TurnOutcome, String> {
        let mut took = false;

        for round in 0..MAX_ROUNDS {
            let mut admitted = 0;

            while matches!(
                self.gate(),
                Gate::State {
                    blocked: false,
                    reading: false,
                    ..
                }
            ) {
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

            let count = self.drain_gated(cx, pending)?;

            // A failed write also opens the output gate: its response must
            // not go out before the object stops
            let answered = match self.gate() {
                Gate::Broken(_) => break,
                Gate::State { writing, .. } => !writing && self.answer(inflight),
            };

            took |= admitted > 0;

            if admitted == 0 && count == 0 && !answered {
                break;
            }

            // More work may be waiting, and nothing else wakes the loop for it
            if round == MAX_ROUNDS - 1 {
                cx.waker().wake_by_ref();
            }
        }

        Ok(TurnOutcome {
            admitted: took,
            gate: self.gate(),
            pinned: self.pinned(),
        })
    }

    fn pinned(&mut self) -> bool {
        use std::pin::pin;

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let object = v8::Local::new(scope, &self.request.dispatch);

        let key = v8::String::new(scope, "pinned").unwrap();
        let pinned = object
            .get(scope, key.into())
            .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
            .expect("durable_object.js answers pinned");

        pinned
            .call(scope, object.into(), &[])
            .is_some_and(|value| value.is_true())
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
                .expect("durable_object.js answers blocked, failure and writing");

            function.call(scope, object.into(), &[])
        };

        let failure = call("failure").filter(|value| value.is_string());
        let blocked = call("blocked").is_some_and(|value| value.is_true());
        let writing = call("writing").is_some_and(|value| value.is_true());
        let reading = call("reading").is_some_and(|value| value.is_true());

        if let Some(failure) = failure {
            return Gate::Broken(failure.to_rust_string_lossy(scope));
        }

        Gate::State {
            blocked,
            writing,
            reading,
        }
    }

    /// Runs the callbacks that came, in order. While the object reads its
    /// storage, only storage results run; the others wait in `pending` until
    /// the reads end (input gate). Answers how many ran.
    fn drain_gated(
        &mut self,
        cx: &mut std::task::Context<'_>,
        pending: &mut VecDeque<CallbackMessage>,
    ) -> Result<usize, String> {
        loop {
            match self.request.callback_rx.poll_recv(cx) {
                Poll::Ready(Some(message)) => pending.push_back(message),
                Poll::Ready(None) => return Err("Event loop channel closed".to_string()),
                Poll::Pending => break,
            }
        }

        let mut count = 0;

        loop {
            let reading = matches!(self.gate(), Gate::State { reading: true, .. });
            let next = if reading {
                pending
                    .iter()
                    .position(|message| matches!(message, CallbackMessage::ObjectStorage(..)))
            } else if pending.is_empty() {
                None
            } else {
                Some(0)
            };

            let Some(index) = next else {
                break;
            };

            let message = pending.remove(index).expect("the index is in the queue");

            // The microtasks of each callback run before the gate is read
            // again: a result can start the next read
            self.process_single_callback(message);
            self.pump_and_checkpoint();
            count += 1;
        }

        Ok(count)
    }

    /// Hands the event to the object.
    fn admit(&mut self, event: ObjectEvent) -> Result<InFlight, String> {
        match event {
            ObjectEvent::Fetch(init) => self.admit_fetch(init).map(InFlight::Fetch),
            ObjectEvent::Alarm(init) => Ok(InFlight::Alarm(self.admit_alarm(init))),
            ObjectEvent::Socket(event) => Ok(InFlight::Socket(self.admit_socket(event))),
        }
    }

    fn admit_socket(&mut self, event: SocketEvent) -> v8::Global<v8::Object> {
        use std::pin::pin;

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let object = v8::Local::new(scope, &self.request.dispatch);

        let key = v8::String::new(scope, "socket").unwrap();
        let socket = object
            .get(scope, key.into())
            .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
            .expect("durable_object.js answers socket");

        let (kind, data, reason): (&str, v8::Local<v8::Value>, v8::Local<v8::Value>) =
            match event.incoming {
                WebSocketIncoming::Text(text) => (
                    "message",
                    v8::String::new(scope, &text).unwrap().into(),
                    v8::undefined(scope).into(),
                ),
                WebSocketIncoming::Binary(bytes) => (
                    "message",
                    crate::v8_helpers::create_array_buffer_from_vec(scope, bytes).into(),
                    v8::undefined(scope).into(),
                ),
                WebSocketIncoming::Closed { code, reason } => (
                    "close",
                    v8::Number::new(scope, code as f64).into(),
                    v8::String::new(scope, &reason).unwrap().into(),
                ),
                WebSocketIncoming::Error(error) => (
                    "error",
                    v8::String::new(scope, &error).unwrap().into(),
                    v8::undefined(scope).into(),
                ),
            };

        let id = v8::Number::new(scope, event.socket as f64);
        let kind = v8::String::new(scope, kind).unwrap();
        let run = socket
            .call(
                scope,
                object.into(),
                &[id.into(), kind.into(), data, reason],
            )
            .and_then(|run| run.to_object(scope))
            .expect("socket() answers an object and does not throw");

        v8::Global::new(scope, run)
    }

    fn admit_alarm(&mut self, init: AlarmInit) -> AlarmInFlight {
        use std::pin::pin;

        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let object = v8::Local::new(scope, &self.request.dispatch);

        let key = v8::String::new(scope, "alarm").unwrap();
        let alarm = object
            .get(scope, key.into())
            .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok())
            .expect("durable_object.js answers alarm");
        let retry_count = v8::Integer::new_from_unsigned(scope, init.retry_count);
        let run = alarm
            .call(scope, object.into(), &[retry_count.into()])
            .and_then(|run| run.to_object(scope))
            .expect("alarm() answers an object and does not throw");

        AlarmInFlight {
            run: v8::Global::new(scope, run),
            done: Some(init.done),
        }
    }

    fn admit_fetch(&mut self, init: FetchInit) -> Result<FetchInFlight, String> {
        use std::pin::pin;

        let FetchInit {
            mut req,
            res_tx,
            upgrade,
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

        Ok(FetchInFlight {
            handle,
            res_tx: Some(res_tx),
            upgrade,
            disconnected: false,
        })
    }

    /// Sends the responses and the alarm outcomes that are ready, tells the
    /// guest of the clients that hung up, and drops the events that are
    /// over. Answers whether it ran guest code or sent an answer.
    fn answer(&mut self, inflight: &mut Vec<InFlight>) -> bool {
        use std::pin::pin;

        let stream_manager = Arc::clone(&self.request.stream_manager);
        let scheduler_tx = self.request.scheduler_tx.clone();
        let buffer_size = self.limits.stream_buffer_size;
        let mut isolate = self.isolate();
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &self.request.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let mut answered = false;

        inflight.retain_mut(|event| {
            let event = match event {
                InFlight::Fetch(event) => event,
                InFlight::Socket(run) => {
                    let run = v8::Local::new(scope, &*run);
                    let done = v8::String::new(scope, "done").unwrap();

                    if !run
                        .get(scope, done.into())
                        .is_some_and(|done| done.is_true())
                    {
                        return true;
                    }

                    answered = true;

                    return false;
                }
                InFlight::Alarm(alarm) => {
                    let run = v8::Local::new(scope, &alarm.run);
                    let done = v8::String::new(scope, "done").unwrap();

                    if !run
                        .get(scope, done.into())
                        .is_some_and(|done| done.is_true())
                    {
                        return true;
                    }

                    let error = v8::String::new(scope, "error").unwrap();
                    let outcome = match run.get(scope, error.into()) {
                        Some(error) if error.is_string() => Err(error.to_rust_string_lossy(scope)),
                        _ => Ok(()),
                    };

                    // A dropped receiver is a host that stopped waiting
                    if let Some(done) = alarm.done.take() {
                        done.send(outcome).ok();
                    }

                    answered = true;

                    return false;
                }
            };
            let handle = Some(v8::Local::new(scope, &event.handle));

            if event.res_tx.is_some()
                && check_exit_condition(scope, handle, EventLoopExit::ResponseReady)
            {
                let res_tx = event.res_tx.take().unwrap();
                answered = true;

                let socket = read_response_socket(scope, handle);

                match read_response_object(scope, handle, &stream_manager, buffer_size) {
                    Ok((status, response)) => {
                        let response = match (socket, event.upgrade.take()) {
                            (Some(ws_id), Some(upgrade)) => {
                                let _ = scheduler_tx
                                    .send(SchedulerMessage::ObjectUpgrade(ws_id, upgrade));
                                response
                            }
                            (Some(_), None) => refused(
                                "The response carries a WebSocket, but the request is not a WebSocket upgrade",
                            ),
                            (None, _) if status == 101 => {
                                refused("A 101 response must carry the client end of a WebSocketPair")
                            }
                            (None, _) => response,
                        };

                        let _ = res_tx.send(response);
                    }
                    Err(error) => tracing::warn!("durable object response: {error}"),
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

/// The answer to a response the object cannot give: the caller sees why.
fn refused(message: &str) -> HttpResponse {
    HttpResponse {
        status: 500,
        headers: vec![("content-type".to_string(), "text/plain".to_string())],
        body: ResponseBody::Bytes(message.to_string().into()),
    }
}
