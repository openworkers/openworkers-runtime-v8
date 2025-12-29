use super::super::SchedulerMessage;
use super::state::ConsoleState;
use openworkers_core::LogLevel;
use tokio::sync::mpsc;
use v8;

pub fn setup_console(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    let context = scope.get_current_context();
    let global = context.global(scope);

    // Store console state in External
    let console_state = ConsoleState { scheduler_tx };
    let state_ptr = Box::into_raw(Box::new(console_state)) as *mut std::ffi::c_void;
    let external = v8::External::new(scope, state_ptr);

    // Setup __console_log native function
    let console_log_fn = v8::Function::builder(
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut _retval: v8::ReturnValue| {
            // Get level (first arg) and message (second arg)
            if args.length() < 2 {
                return;
            }

            let level_num = args.get(0).int32_value(scope).unwrap_or(2); // default to Info
            let level = match level_num {
                0 => LogLevel::Error,
                1 => LogLevel::Warn,
                2 => LogLevel::Info,
                3 => LogLevel::Debug,
                4 => LogLevel::Trace,
                _ => LogLevel::Info,
            };

            let msg = args
                .get(1)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_default();

            // Send log via scheduler -> ops
            let data = args.data();

            if let Ok(external) = v8::Local::<v8::External>::try_from(data) {
                let state = unsafe { &*(external.value() as *const ConsoleState) };
                let _ = state.scheduler_tx.send(SchedulerMessage::Log(level, msg));
            }
        },
    )
    .data(external.into())
    .build(scope)
    .unwrap();

    let console_log_key = v8::String::new(scope, "__console_log").unwrap();
    global.set(scope, console_log_key.into(), console_log_fn.into());

    // Setup console object using JS that calls __console_log
    let code = r#"
        function __formatArg(a) {
            if (a instanceof Error) {
                return a.stack || (a.name + ': ' + a.message);
            }

            if (typeof a === 'object' && a !== null) {
                try {
                    return JSON.stringify(a);
                } catch (e) {
                    return String(a);
                }
            }

            return String(a);
        }

        globalThis.console = {
            log: function(...args) {
                __console_log(2, args.map(__formatArg).join(' '));
            },
            info: function(...args) {
                __console_log(2, args.map(__formatArg).join(' '));
            },
            warn: function(...args) {
                __console_log(1, args.map(__formatArg).join(' '));
            },
            error: function(...args) {
                __console_log(0, args.map(__formatArg).join(' '));
            },
            debug: function(...args) {
                __console_log(3, args.map(__formatArg).join(' '));
            },
            trace: function(...args) {
                __console_log(4, args.map(__formatArg).join(' '));
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
