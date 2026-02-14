use super::super::SchedulerMessage;
use super::state::TimerState;
use std::rc::Rc;
use tokio::sync::mpsc;
use v8;

// Native timer functions using glue_v8 macro with Fast API
//
// These functions use primitive types (u64) and Fast API with state.
// State is passed via FunctionTemplate data (External containing Rc<TimerState>).

#[glue_v8::method(fast, state = Rc<TimerState>)]
fn native_schedule_timeout(state: &Rc<TimerState>, id: u64, delay: u64) {
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::ScheduleTimeout(id, delay));
}

#[glue_v8::method(fast, state = Rc<TimerState>)]
fn native_schedule_interval(state: &Rc<TimerState>, id: u64, interval: u64) {
    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::ScheduleInterval(id, interval));
}

#[glue_v8::method(fast, state = Rc<TimerState>)]
fn native_clear_timer(state: &Rc<TimerState>, id: u64) {
    let _ = state.scheduler_tx.send(SchedulerMessage::ClearTimer(id));
}

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    // Create state for timer functions
    let state = Rc::new(TimerState {
        scheduler_tx: scheduler_tx.clone(),
    });

    // Store in context slot to keep Rc alive for the context's lifetime
    scope.get_current_context().set_slot(state.clone());

    // Register native functions using Fast API templates
    let schedule_timeout_fn = native_schedule_timeout_v8_template(scope, &state)
        .get_function(scope)
        .unwrap();
    let schedule_interval_fn = native_schedule_interval_v8_template(scope, &state)
        .get_function(scope)
        .unwrap();
    let clear_timer_fn = native_clear_timer_v8_template(scope, &state)
        .get_function(scope)
        .unwrap();

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

        globalThis.setTimeout = function(callback, delay, ...args) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, args.length > 0 ? () => callback(...args) : callback);
            __nativeScheduleTimeout(id, delay || 0);
            return id;
        };

        globalThis.setInterval = function(callback, interval, ...args) {
            const id = globalThis.__nextTimerId++;
            globalThis.__timerCallbacks.set(id, args.length > 0 ? () => callback(...args) : callback);
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
