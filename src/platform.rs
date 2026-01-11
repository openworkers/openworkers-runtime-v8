//! Global V8 platform initialization.
//!
//! V8 can only be initialized once per process. This module provides
//! a single entry point for platform initialization used by all other modules.

use std::sync::OnceLock;
use v8;

static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();

/// Get the global V8 platform, initializing it if necessary.
///
/// This is safe to call from multiple threads - the platform is only
/// initialized once and the same reference is returned to all callers.
pub fn get_platform() -> &'static v8::SharedRef<v8::Platform> {
    PLATFORM.get_or_init(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        platform
    })
}
