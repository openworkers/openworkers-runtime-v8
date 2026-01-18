use super::super::SchedulerMessage;
use super::state::TimerState;
use std::rc::Rc;
use tokio::sync::mpsc;
use v8;

// Native timer functions using v8g macro

#[glue_v8::method(state = TimerState)]
fn native_schedule_timeout(scope: &mut v8::PinScope, state: &Rc<TimerState>, id: u64, delay: u64) {
    let _ = scope;
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::ScheduleTimeout(id, delay));
}

#[glue_v8::method(state = TimerState)]
fn native_schedule_interval(
    scope: &mut v8::PinScope,
    state: &Rc<TimerState>,
    id: u64,
    interval: u64,
) {
    let _ = scope;
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::ScheduleInterval(id, interval));
}

#[glue_v8::method(state = TimerState)]
fn native_clear_timer(scope: &mut v8::PinScope, state: &Rc<TimerState>, id: u64) {
    let _ = scope;
    let _ = state.scheduler_tx.send(SchedulerMessage::ClearTimer(id));
}

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    // Store state in context slot
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };
    store_state!(scope, state);

    // Register native functions using generated wrappers
    let schedule_timeout_fn = v8::Function::new(scope, native_schedule_timeout_v8).unwrap();
    let schedule_interval_fn = v8::Function::new(scope, native_schedule_interval_v8).unwrap();
    let clear_timer_fn = v8::Function::new(scope, native_clear_timer_v8).unwrap();

    register_fn!(scope, "__nativeScheduleTimeout", schedule_timeout_fn);
    register_fn!(scope, "__nativeScheduleInterval", schedule_interval_fn);
    register_fn!(scope, "__nativeClearTimer", clear_timer_fn);

    // JavaScript timer wrappers
    exec_js!(
        scope,
        r#"
        globalThis.__timerCallbacks = new Map();
        globalThis.__nextTimerId = 1;
        globalThis.__intervalIds = new Set();

        globalThis.setTimeout = function(callback, delay) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, callback);
            __nativeScheduleTimeout(id, delay || 0);
            return id;
        };

        globalThis.setInterval = function(callback, interval) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, callback);
            globalThis.__intervalIds.add(id);
            __nativeScheduleInterval(id, interval || 0);
            return id;
        };

        globalThis.clearTimeout = function(id) {
            globalThis.__timerCallbacks.delete(id);
            globalThis.__intervalIds.delete(id);
            __nativeClearTimer(id);
        };

        globalThis.clearInterval = globalThis.clearTimeout;

        globalThis.queueMicrotask = function(callback) {
            Promise.resolve().then(callback);
        };

        globalThis.__executeTimer = function(id) {
            const callback = globalThis.__timerCallbacks.get(id);
            if (callback) {
                callback();
                if (!globalThis.__intervalIds.has(id)) {
                    globalThis.__timerCallbacks.delete(id);
                }
            }
        };
    "#
    );
}
