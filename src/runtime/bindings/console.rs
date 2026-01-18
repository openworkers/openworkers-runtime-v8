use super::super::SchedulerMessage;
use super::state::ConsoleState;
use openworkers_core::LogLevel;
use std::rc::Rc;
use tokio::sync::mpsc;
use v8;

/// Native console log function
/// Args: level (i32), message (String)
#[glue_v8::method(state = ConsoleState)]
fn console_log(scope: &mut v8::PinScope, state: &Rc<ConsoleState>, level: i32, message: String) {
    let _ = scope;

    let log_level = match level {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        2 => LogLevel::Info,
        3 => LogLevel::Debug,
        4 => LogLevel::Trace,
        _ => LogLevel::Info,
    };

    let _ = state
        .scheduler_tx
        .send(SchedulerMessage::Log(log_level, message));
}

pub fn setup_console(
    scope: &mut v8::PinScope,
    scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
) {
    // Store state in context slot
    let state = ConsoleState { scheduler_tx };
    store_state!(scope, state);

    // Register native console_log using generated wrapper
    let console_log_fn = v8::Function::new(scope, console_log_v8).unwrap();
    register_fn!(scope, "__console_log", console_log_fn);

    // Setup console object using JS that calls __console_log
    exec_js!(
        scope,
        r#"
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
    "#
    );
}
