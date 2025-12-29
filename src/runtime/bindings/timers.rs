use super::super::SchedulerMessage;
use super::state::TimerState;
use tokio::sync::mpsc;
use v8;

pub fn setup_timers(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };

    // Create External to hold our state
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    global.set(scope, state_key.into(), external.into());

    // Create __nativeScheduleTimeout function
    let schedule_timeout_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 2
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                    && let Some(delay_val) = args.get(1).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let delay = delay_val.value() as u64;
                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleTimeout(id, delay));
                }
            }
        },
    )
    .unwrap();

    // Create __nativeScheduleInterval function
    let schedule_interval_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 2
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                    && let Some(interval_val) = args.get(1).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let interval = interval_val.value() as u64;
                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleInterval(id, interval));
                }
            }
        },
    )
    .unwrap();

    // Create __nativeClearTimer function
    let clear_timer_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            let global = scope.get_current_context().global(scope);
            let state_key = v8::String::new(scope, "__timerState").unwrap();
            let state_val = global.get(scope, state_key.into()).unwrap();

            if state_val.is_external() {
                let external: v8::Local<v8::External> = state_val.try_into().unwrap();
                let state_ptr = external.value() as *mut TimerState;
                let state = unsafe { &*state_ptr };

                if args.length() >= 1
                    && let Some(id_val) = args.get(0).to_uint32(scope)
                {
                    let id = id_val.value() as u64;
                    let _ = state.scheduler_tx.send(SchedulerMessage::ClearTimer(id));
                }
            }
        },
    )
    .unwrap();

    // Register native functions
    let schedule_timeout_key = v8::String::new(scope, "__nativeScheduleTimeout").unwrap();
    global.set(
        scope,
        schedule_timeout_key.into(),
        schedule_timeout_fn.into(),
    );

    let schedule_interval_key = v8::String::new(scope, "__nativeScheduleInterval").unwrap();
    global.set(
        scope,
        schedule_interval_key.into(),
        schedule_interval_fn.into(),
    );

    let clear_timer_key = v8::String::new(scope, "__nativeClearTimer").unwrap();
    global.set(scope, clear_timer_key.into(), clear_timer_fn.into());

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
