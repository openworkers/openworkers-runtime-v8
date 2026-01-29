//! Heap limit protection for V8 isolates.
//!
//! This module provides protection against V8 OOM crashes by intercepting
//! near-heap-limit conditions and terminating execution gracefully instead
//! of letting V8 call `FatalProcessOutOfMemory` which kills the entire process.
//!
//! ## How it works
//!
//! When V8's heap approaches the configured limit, it calls our callback.
//! The callback:
//! 1. First call: Increases the limit slightly (10%) to give V8 room to GC
//! 2. Subsequent calls: Terminates execution via `isolate.terminate_execution()`
//!
//! This prevents a single misbehaving worker from crashing the entire runner.
//!
//! ## OOM Handler
//!
//! For allocations outside V8's managed heap (like ICU), we also install an
//! OOM error handler that logs the error before V8 aborts.

use std::ffi::{c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use v8::IsolateHandle;

/// State passed to the near-heap-limit callback.
///
/// This struct is heap-allocated and passed to V8 as a raw pointer.
/// It must be kept alive for the lifetime of the isolate.
pub struct HeapLimitState {
    /// Handle to terminate execution (thread-safe)
    pub isolate_handle: IsolateHandle,
    /// Flag to signal that memory limit was hit
    pub memory_limit_hit: Arc<AtomicBool>,
    /// Number of times the callback has been invoked
    pub invocation_count: AtomicU32,
    /// Maximum heap size in bytes (absolute limit)
    pub max_heap_bytes: usize,
}

impl HeapLimitState {
    /// Create a new heap limit state.
    pub fn new(
        isolate_handle: IsolateHandle,
        memory_limit_hit: Arc<AtomicBool>,
        max_heap_bytes: usize,
    ) -> Self {
        Self {
            isolate_handle,
            memory_limit_hit,
            invocation_count: AtomicU32::new(0),
            max_heap_bytes,
        }
    }
}

/// Near-heap-limit callback for V8.
///
/// This callback is invoked when V8's heap is approaching its limit.
/// Returns a new heap limit to allow V8 to continue, or terminates execution
/// if we've already given V8 extra room.
///
/// # Safety
///
/// This function is called from V8's internals. The `data` pointer must be
/// a valid pointer to a `HeapLimitState` that outlives the isolate.
pub unsafe extern "C" fn near_heap_limit_callback(
    data: *mut c_void,
    current_heap_limit: usize,
    initial_heap_limit: usize,
) -> usize {
    // SAFETY: data is a valid pointer to HeapLimitState, passed from install_heap_limit_callback
    let state = unsafe { &*(data as *const HeapLimitState) };

    let count = state.invocation_count.fetch_add(1, Ordering::SeqCst);

    tracing::warn!(
        "Near heap limit callback invoked (count: {}, current: {} MB, initial: {} MB, max: {} MB)",
        count + 1,
        current_heap_limit / (1024 * 1024),
        initial_heap_limit / (1024 * 1024),
        state.max_heap_bytes / (1024 * 1024)
    );

    // First invocation: give V8 a bit more room to try GC
    if count == 0 {
        // Increase by 10%, but don't exceed max
        let extra = current_heap_limit / 10;
        let new_limit = (current_heap_limit + extra).min(state.max_heap_bytes);

        if new_limit > current_heap_limit {
            tracing::warn!(
                "Increasing heap limit to {} MB to allow GC",
                new_limit / (1024 * 1024)
            );
            return new_limit;
        }
    }

    // We've already given extra room or can't give more - terminate execution
    tracing::error!(
        "Heap limit exhausted after {} callbacks, terminating execution",
        count + 1
    );

    // Set the memory limit flag so the runner knows why execution stopped
    state.memory_limit_hit.store(true, Ordering::SeqCst);

    // Terminate execution gracefully instead of letting V8 crash
    state.isolate_handle.terminate_execution();

    // Return current limit (V8 will see termination and stop)
    current_heap_limit
}

/// OOM error handler for V8 isolates.
///
/// Called when V8 or ICU runs out of memory. By the time this is called,
/// V8 is in an unrecoverable state. We log the error for debugging.
///
/// Note: This handler cannot prevent the crash - it's just for better error reporting.
/// The actual prevention is done via the near-heap-limit callback for V8 heap,
/// but ICU allocations are outside our control.
unsafe extern "C" fn oom_error_handler(location: *const c_char, details: &v8::OomDetails) {
    let location_str = if location.is_null() {
        "unknown"
    } else {
        // SAFETY: V8 passes a valid C string
        unsafe { std::ffi::CStr::from_ptr(location) }
            .to_str()
            .unwrap_or("invalid utf8")
    };

    let detail_str = if details.detail.is_null() {
        ""
    } else {
        // SAFETY: V8 passes a valid C string
        unsafe { std::ffi::CStr::from_ptr(details.detail as *const c_char) }
            .to_str()
            .unwrap_or("")
    };

    let oom_type = if details.is_heap_oom {
        "JavaScript heap"
    } else {
        "process/external memory"
    };

    tracing::error!(
        "V8 OOM at {}: {} out of memory{}{}",
        location_str,
        oom_type,
        if detail_str.is_empty() { "" } else { ": " },
        detail_str
    );
}

/// Install heap limit protection on an isolate.
///
/// This must be called after isolate creation but before any JavaScript execution.
/// The returned `Box<HeapLimitState>` must be kept alive for the lifetime of the isolate.
///
/// Installs:
/// 1. Near-heap-limit callback - terminates execution before V8 heap OOM
/// 2. OOM error handler - logs errors for ICU/external memory OOM
///
/// # Example
///
/// ```ignore
/// let mut isolate = v8::Isolate::new(params);
/// let heap_state = install_heap_limit_callback(&mut isolate, memory_limit_hit, max_heap_bytes);
/// // ... use isolate ...
/// // heap_state dropped when isolate is dropped
/// ```
pub fn install_heap_limit_callback(
    isolate: &mut v8::Isolate,
    memory_limit_hit: Arc<AtomicBool>,
    max_heap_bytes: usize,
) -> Box<HeapLimitState> {
    let state = Box::new(HeapLimitState::new(
        isolate.thread_safe_handle(),
        memory_limit_hit,
        max_heap_bytes,
    ));

    let state_ptr = &*state as *const HeapLimitState as *mut c_void;

    // Install near-heap-limit callback for V8 managed heap
    isolate.add_near_heap_limit_callback(near_heap_limit_callback, state_ptr);

    // Install OOM error handler for external memory (ICU, etc.)
    // This won't prevent the crash but provides better error reporting
    isolate.set_oom_error_handler(oom_error_handler);

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heap_limit_state_creation() {
        // Initialize V8 platform first
        crate::platform::get_platform();

        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Create a minimal isolate to get an IsolateHandle
        let isolate = v8::Isolate::new(Default::default());
        let handle = isolate.thread_safe_handle();

        let state = HeapLimitState::new(handle, memory_limit_hit.clone(), 128 * 1024 * 1024);

        assert_eq!(state.invocation_count.load(Ordering::SeqCst), 0);
        assert_eq!(state.max_heap_bytes, 128 * 1024 * 1024);
        assert!(!state.memory_limit_hit.load(Ordering::SeqCst));
    }

    #[test]
    fn test_heap_limit_callback_is_installed() {
        use std::pin::pin;

        // Initialize V8 platform first
        crate::platform::get_platform();

        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Create isolate with heap limits
        let params = v8::CreateParams::default().heap_limits(4 * 1024 * 1024, 32 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        // Install heap limit callback
        let heap_state = install_heap_limit_callback(
            &mut isolate,
            Arc::clone(&memory_limit_hit),
            32 * 1024 * 1024,
        );

        // Verify the state is initialized correctly
        assert_eq!(heap_state.invocation_count.load(Ordering::SeqCst), 0);
        assert_eq!(heap_state.max_heap_bytes, 32 * 1024 * 1024);
        assert!(!memory_limit_hit.load(Ordering::SeqCst));

        // Run some simple JS to verify isolate works
        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let code = v8::String::new(scope, "1 + 1").unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let result = script.run(scope).unwrap();

        assert_eq!(result.int32_value(scope).unwrap(), 2);
    }
}
