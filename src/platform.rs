//! Global V8 platform and snapshot initialization.
//!
//! V8 can only be initialized once per process. This module provides
//! a single entry point for platform initialization used by all other modules.

use std::sync::OnceLock;
use v8;

static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();
static SNAPSHOT: OnceLock<Option<&'static [u8]>> = OnceLock::new();

/// Get the global V8 platform, initializing it if necessary.
///
/// This is safe to call from multiple threads - the platform is only
/// initialized once and the same reference is returned to all callers.
pub fn get_platform() -> &'static v8::SharedRef<v8::Platform> {
    PLATFORM.get_or_init(|| {
        // Initialize ICU data BEFORE V8 initialization
        // This is required for Intl.DateTimeFormat, NumberFormat, etc.
        // Without this, V8/ICU tries to load data at runtime causing OOM
        v8::icu::set_common_data_77(crate::icudata::ICU_DATA)
            .expect("Failed to initialize ICU data");

        // Set V8 flags before initialization (following workerd's approach)
        // Disable incremental marking - better for small heaps and avoids GC bugs
        v8::V8::set_flags_from_string("--noincremental-marking");

        // On macOS, use single-threaded GC to avoid code collection issues
        // See: https://github.com/cloudflare/workers-sdk/issues/2386
        #[cfg(target_os = "macos")]
        v8::V8::set_flags_from_string("--single-threaded-gc");

        // Increase old space size to give ICU more room for caching
        // DateTimeFormat pattern generators use significant memory
        v8::V8::set_flags_from_string("--max-old-space-size=512");

        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        platform
    })
}

/// Get the runtime snapshot, loading it once from disk.
///
/// Returns `None` if:
/// - The snapshot file doesn't exist
/// - The snapshot file is empty (allows running without snapshot)
/// - The file cannot be read
///
/// The snapshot is leaked into static memory to avoid lifetime issues.
pub fn get_snapshot() -> Option<&'static [u8]> {
    *SNAPSHOT.get_or_init(|| {
        const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");

        match std::fs::read(RUNTIME_SNAPSHOT_PATH) {
            Ok(bytes) if bytes.is_empty() => {
                log::warn!(
                    "Runtime snapshot file is empty: {} - running without snapshot (slower startup)",
                    RUNTIME_SNAPSHOT_PATH
                );
                None
            }
            Ok(bytes) => {
                log::info!(
                    "Loaded runtime snapshot ({} bytes) from {}",
                    bytes.len(),
                    RUNTIME_SNAPSHOT_PATH
                );
                Some(Box::leak(bytes.into_boxed_slice()) as &'static [u8])
            }
            Err(e) => {
                log::warn!(
                    "Failed to load runtime snapshot from {}: {} - running without snapshot (slower startup)",
                    RUNTIME_SNAPSHOT_PATH,
                    e
                );
                None
            }
        }
    })
}
