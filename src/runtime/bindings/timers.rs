use super::super::SchedulerMessage;
use super::state::TimerState;
use tokio::sync::mpsc;
use v8;

/// Create a timer callback that extracts (id, delay) and sends a message
macro_rules! timer_callback {
    ($msg:ident) => {
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_state!(scope, "__timerState", TimerState) else {
                return;
            };

            if args.length() >= 2 {
                if let (Some(id), Some(val)) =
                    (args.get(0).to_uint32(scope), args.get(1).to_uint32(scope))
                {
                    let _ = state.scheduler_tx.send(SchedulerMessage::$msg(
                        id.value() as u64,
                        val.value() as u64,
                    ));
                }
            }
        }
    };
}

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    // Store state in global scope
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };
    store_state!(scope, "__timerState", state);

    // Register native timer functions
    let schedule_timeout_fn = v8::Function::new(scope, timer_callback!(ScheduleTimeout)).unwrap();
    let schedule_interval_fn = v8::Function::new(scope, timer_callback!(ScheduleInterval)).unwrap();

    let clear_timer_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_state!(scope, "__timerState", TimerState) else {
                return;
            };

            if let Some(id) = args.get(0).to_uint32(scope) {
                let _ = state
                    .scheduler_tx
                    .send(SchedulerMessage::ClearTimer(id.value() as u64));
            }
        },
    )
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
