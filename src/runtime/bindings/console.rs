use super::state::{ConsoleState, LogCallback};
use openworkers_core::{LogLevel, OperationsHandle};
use std::rc::Rc;
use std::sync::Arc;
use v8;

/// Create a LogCallback from an OperationsHandle.
///
/// This creates a callback that calls ops.handle_log() directly,
/// bypassing the scheduler for guaranteed log delivery.
pub fn log_callback_from_ops(ops: &OperationsHandle) -> LogCallback {
    let ops = ops.clone();
    Arc::new(move |level, message| {
        ops.handle_log(level, message);
    })
}

/// Native console log function
/// Args: level (i32), message (String)
///
/// Bypasses the scheduler and calls the log callback directly.
/// This is necessary because the scheduler runs via spawn_local on the same
/// thread as JS execution. When JS runs synchronously without yielding,
/// the scheduler never gets a chance to process Log messages before the
/// worker terminates. By calling the callback directly, logs are guaranteed
/// to be sent before the worker completes.
#[glue_v8::method(state = Rc<ConsoleState>)]
fn console_log(_scope: &mut v8::PinScope, state: &Rc<ConsoleState>, level: i32, message: String) {
    let log_level = match level {
        0 => LogLevel::Error,
        1 => LogLevel::Warn,
        2 => LogLevel::Info,
        3 => LogLevel::Debug,
        4 => LogLevel::Trace,
        _ => LogLevel::Info,
    };

    // Call log callback directly - bypasses scheduler for guaranteed delivery
    (state.log_callback)(log_level, message);
}

pub fn setup_console(scope: &mut v8::PinScope, log_callback: LogCallback) {
    // Create state (passed via FunctionTemplate data)
    let state = Rc::new(ConsoleState { log_callback });

    // Store in context slot to keep Rc alive for the context's lifetime
    scope.get_current_context().set_slot(state.clone());

    // Register native console_log using generated template
    let console_log_fn = console_log_v8_template(scope, &state)
        .get_function(scope)
        .unwrap();
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
