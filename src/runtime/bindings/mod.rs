#[macro_use]
mod macros;

mod console;
mod fetch;
mod native;
mod state;
mod streams;
mod timers;
mod url_pattern;
mod web_api;
mod websocket;

// Re-export state types
pub use state::{FetchState, LogCallback, ResponseStreamState, StreamState, WebSocketEventState};

// Re-export setup functions
pub use console::{log_callback_from_ops, setup_console};
pub use fetch::setup_fetch;
pub use native::register_op;
pub use native::seal as seal_native_namespace;
pub use streams::{setup_response_stream_ops, setup_stream_ops};
pub use timers::setup_timers;
pub use url_pattern::setup_url_pattern_natives;
pub use web_api::{
    setup_fetch_helpers, setup_global_aliases, setup_navigator_natives, setup_performance,
    setup_security_restrictions, setup_url_natives,
};
pub use websocket::setup_websocket;

/// Evaluates the whole shared surface as one script.
///
/// Fourteen separate compiles cost more than one: every context a cold request
/// stands up pays that, and the surface is the same text every time.
pub fn setup_surface(scope: &mut v8::PinScope) {
    use std::sync::LazyLock;

    static SOURCE: LazyLock<String> = LazyLock::new(|| {
        openworkers_wintertc::SURFACE
            .iter()
            .map(|module| module.source)
            .collect::<Vec<_>>()
            .join("\n")
    });

    let code = v8::String::new(scope, &SOURCE).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope).unwrap();
}
