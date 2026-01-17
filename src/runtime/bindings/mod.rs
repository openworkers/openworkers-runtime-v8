#[macro_use]
mod macros;

mod console;
mod fetch;
mod state;
mod streams;
mod timers;
mod web_api;

// Re-export state types
pub use state::{FetchState, StreamState};

// Re-export setup functions
pub use console::setup_console;
pub use fetch::setup_fetch;
pub use streams::{setup_response_stream_ops, setup_stream_ops};
pub use timers::setup_timers;
pub use web_api::{
    setup_abort_controller, setup_base64, setup_blob, setup_form_data, setup_global_aliases,
    setup_headers, setup_performance, setup_request, setup_response, setup_security_restrictions,
    setup_structured_clone, setup_url, setup_url_search_params,
};
