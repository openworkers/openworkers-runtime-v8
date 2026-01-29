//! Legacy thread-local isolate reuse.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use v8;

#[cfg(not(feature = "sandbox"))]
use crate::security::CustomAllocator;
use openworkers_core::RuntimeLimits;

/// A reusable V8 isolate that can create multiple execution contexts
///
/// This represents the V8 engine instance (heap, GC, JIT compiler) without
/// any specific execution context. It can be reused across many requests by
/// creating fresh contexts for each one.
pub struct SharedIsolate {
    pub isolate: v8::OwnedIsolate,
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    pub memory_limit_hit: Arc<AtomicBool>,
    /// Whether a snapshot was used for initialization
    pub use_snapshot: bool,
}

impl SharedIsolate {
    /// Create a new shared isolate
    ///
    /// This is expensive (few ms without snapshot, tens of µs with snapshot)
    /// and should be done once at startup, not per-request.
    pub fn new(limits: RuntimeLimits) -> Self {
        // Get global V8 platform (initialized once, shared across all modules)
        let platform = crate::platform::get_platform();

        // Memory limit tracking for ArrayBuffer allocations
        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Convert heap limits from MB to bytes
        let heap_initial = limits.heap_initial_mb * 1024 * 1024;
        let heap_max = limits.heap_max_mb * 1024 * 1024;

        // Load snapshot (centralized, handles empty file case)
        let snapshot_ref = crate::platform::get_snapshot();

        // Create isolate params
        // In sandbox mode, V8 manages memory allocation, so we use the default allocator.
        // In non-sandbox mode, we use a custom allocator to enforce memory limits.
        #[cfg(not(feature = "sandbox"))]
        let params = {
            // Create custom ArrayBuffer allocator to enforce memory limits on external memory
            // This is critical: V8 heap limits don't cover ArrayBuffers, Uint8Array, etc.
            let array_buffer_allocator =
                CustomAllocator::new(heap_max, Arc::clone(&memory_limit_hit));
            v8::CreateParams::default()
                .heap_limits(heap_initial, heap_max)
                .array_buffer_allocator(array_buffer_allocator.into_v8_allocator())
                .allow_atomics_wait(false)
        };

        #[cfg(feature = "sandbox")]
        let params = v8::CreateParams::default()
            .heap_limits(heap_initial, heap_max)
            .allow_atomics_wait(false);

        let mut params = params;

        if let Some(snapshot_data) = snapshot_ref {
            params = params.snapshot_blob((*snapshot_data).into());
        }

        let isolate = v8::Isolate::new(params);

        let use_snapshot = snapshot_ref.is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
        }
    }

    /// Get a thread-safe handle to this isolate for termination
    pub fn thread_safe_handle(&self) -> v8::IsolateHandle {
        self.isolate.thread_safe_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_isolate_creation() {
        let limits = RuntimeLimits::default();
        let isolate = SharedIsolate::new(limits);

        // Isolate should be created successfully
        assert!(
            !isolate
                .memory_limit_hit
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn test_multiple_shared_isolates() {
        let limits = RuntimeLimits::default();

        // Should be able to create multiple isolates
        let isolate1 = SharedIsolate::new(limits.clone());
        let isolate2 = SharedIsolate::new(limits);

        // Both should be valid
        assert!(
            !isolate1
                .memory_limit_hit
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert!(
            !isolate2
                .memory_limit_hit
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }
}
