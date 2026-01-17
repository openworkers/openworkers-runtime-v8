use super::super::SchedulerMessage;
use super::state::TimerState;
use tokio::sync::mpsc;
use v8;

/// Helper to get timer state from global scope
fn get_timer_state<'s>(scope: &mut v8::PinScope<'s, '_>) -> Option<&'s TimerState> {
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState")?;
    let state_val = global.get(scope, state_key.into())?;

    if !state_val.is_external() {
        return None;
    }

    let external: v8::Local<v8::External> = state_val.try_into().ok()?;
    let state_ptr = external.value() as *const TimerState;
    Some(unsafe { &*state_ptr })
}

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    // Store state in global scope
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    global.set(scope, state_key.into(), external.into());

    // __nativeScheduleTimeout
    let schedule_timeout_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_timer_state(scope) else {
                return;
            };

            if args.length() >= 2 {
                if let (Some(id), Some(delay)) =
                    (args.get(0).to_uint32(scope), args.get(1).to_uint32(scope))
                {
                    let _ = state.scheduler_tx.send(SchedulerMessage::ScheduleTimeout(
                        id.value() as u64,
                        delay.value() as u64,
                    ));
                }
            }
        },
    )
    .unwrap();

    // __nativeScheduleInterval
    let schedule_interval_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_timer_state(scope) else {
                return;
            };

            if args.length() >= 2 {
                if let (Some(id), Some(interval)) =
                    (args.get(0).to_uint32(scope), args.get(1).to_uint32(scope))
                {
                    let _ = state.scheduler_tx.send(SchedulerMessage::ScheduleInterval(
                        id.value() as u64,
                        interval.value() as u64,
                    ));
                }
            }
        },
    )
    .unwrap();

    // __nativeClearTimer
    let clear_timer_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_timer_state(scope) else {
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

    // Register native functions
    let global = scope.get_current_context().global(scope);

    let key = v8::String::new(scope, "__nativeScheduleTimeout").unwrap();
    global.set(scope, key.into(), schedule_timeout_fn.into());

    let key = v8::String::new(scope, "__nativeScheduleInterval").unwrap();
    global.set(scope, key.into(), schedule_interval_fn.into());

    let key = v8::String::new(scope, "__nativeClearTimer").unwrap();
    global.set(scope, key.into(), clear_timer_fn.into());

    // JavaScript timer wrappers
    let code = r#"
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
                // For setTimeout (not interval), remove the callback after execution
                if (!globalThis.__intervalIds.has(id)) {
                    globalThis.__timerCallbacks.delete(id);
                }
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
