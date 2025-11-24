use super::{CallbackId, SchedulerMessage};
use rusty_v8 as v8;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// Shared state accessible from V8 callbacks (for timers)
#[derive(Clone)]
pub struct TimerState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
}

pub fn setup_console(scope: &mut v8::HandleScope) {
    // Setup native print function
    fn print_callback(
        scope: &mut v8::HandleScope,
        args: v8::FunctionCallbackArguments,
        mut _retval: v8::ReturnValue,
    ) {
        if args.length() > 0
            && let Some(msg_str) = args.get(0).to_string(scope) {
                let msg = msg_str.to_rust_string_lossy(scope);
                println!("{}", msg);
            }
    }

    let global = scope.get_current_context().global(scope);
    let print_fn = v8::Function::new(scope, print_callback).unwrap();
    let print_key = v8::String::new(scope, "print").unwrap();
    global.set(scope, print_key.into(), print_fn.into());

    // Setup console object
    let code = r#"
        globalThis.console = {
            log: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[LOG] ' + msg);
            },
            warn: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[WARN] ' + msg);
            },
            error: function(...args) {
                const msg = args.map(a => String(a)).join(' ');
                print('[ERROR] ' + msg);
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_timers(
    scope: &mut v8::HandleScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    let state = TimerState {
        scheduler_tx: scheduler_tx.clone(),
    };

    // Create External to hold our state
    let state_ptr = Box::into_raw(Box::new(state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    // Create __nativeScheduleTimeout function
    let schedule_timeout_fn = v8::Function::new(scope, schedule_timeout_callback).unwrap();

    // Create __nativeScheduleInterval function
    let schedule_interval_fn = v8::Function::new(scope, schedule_interval_callback).unwrap();

    // Create __nativeClearTimer function
    let clear_timer_fn = v8::Function::new(scope, clear_timer_callback).unwrap();

    // Store state in global so callbacks can access it
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    global.set(scope, state_key.into(), external.into());

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

// V8 callback for __nativeScheduleTimeout
fn schedule_timeout_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    let state_val = global.get(scope, state_key.into()).unwrap();

    if state_val.is_external() {
        let external = unsafe { v8::Local::<v8::External>::cast(state_val) };
        let state_ptr = external.value() as *mut TimerState;
        let state = unsafe { &*state_ptr };

        if args.length() >= 2
            && let Some(id_val) = args.get(0).to_uint32(scope)
                && let Some(delay_val) = args.get(1).to_uint32(scope) {
                    let id = id_val.value() as u64;
                    let delay = delay_val.value() as u64;

                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleTimeout(id, delay));
                }
    }
}

// V8 callback for __nativeScheduleInterval
fn schedule_interval_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    let state_val = global.get(scope, state_key.into()).unwrap();

    if state_val.is_external() {
        let external = unsafe { v8::Local::<v8::External>::cast(state_val) };
        let state_ptr = external.value() as *mut TimerState;
        let state = unsafe { &*state_ptr };

        if args.length() >= 2
            && let Some(id_val) = args.get(0).to_uint32(scope)
                && let Some(interval_val) = args.get(1).to_uint32(scope) {
                    let id = id_val.value() as u64;
                    let interval = interval_val.value() as u64;

                    let _ = state
                        .scheduler_tx
                        .send(SchedulerMessage::ScheduleInterval(id, interval));
                }
    }
}

// V8 callback for __nativeClearTimer
fn clear_timer_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    mut _retval: v8::ReturnValue,
) {
    let global = scope.get_current_context().global(scope);
    let state_key = v8::String::new(scope, "__timerState").unwrap();
    let state_val = global.get(scope, state_key.into()).unwrap();

    if state_val.is_external() {
        let external = unsafe { v8::Local::<v8::External>::cast(state_val) };
        let state_ptr = external.value() as *mut TimerState;
        let state = unsafe { &*state_ptr };

        if args.length() >= 1
            && let Some(id_val) = args.get(0).to_uint32(scope) {
                let id = id_val.value() as u64;
                let _ = state.scheduler_tx.send(SchedulerMessage::ClearTimer(id));
            }
    }
}

pub fn setup_fetch(
    scope: &mut v8::HandleScope,
    _scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    _callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    _next_id: Arc<Mutex<CallbackId>>,
) {
    // Stub for now - will implement later
    let code = r#"
        globalThis.fetch = function(url, options) {
            return Promise.reject(new Error('fetch() not yet implemented in V8 runtime'));
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_url(scope: &mut v8::HandleScope) {
    let code = r#"
        globalThis.URL = class URL {
            constructor(url) {
                this.href = url;
                const parts = url.match(/^(https?):\/\/([^\/]+)(\/.*)?$/);
                if (parts) {
                    this.protocol = parts[1] + ':';
                    this.hostname = parts[2].split(':')[0];
                    this.host = parts[2];
                    this.pathname = parts[3] || '/';
                } else {
                    this.pathname = '/';
                }
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

pub fn setup_response(scope: &mut v8::HandleScope) {
    let code = r#"
        globalThis.Response = function(body, init) {
            init = init || {};
            this.body = String(body || '');
            this.status = init.status || 200;
            this.headers = init.headers || {};
            this.text = async function() {
                return this.body;
            };
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
