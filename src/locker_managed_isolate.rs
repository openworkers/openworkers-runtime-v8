//! Isolate managed via v8::Locker (no auto-enter/exit)
//!
//! This module provides a wrapper around v8::UnenteredIsolate that is designed
//! for use in multi-threaded isolate pools with v8::Locker.
//!
//! Unlike SharedIsolate which uses OwnedIsolate (auto-enter), LockerManagedIsolate
//! uses UnenteredIsolate and requires explicit locking via v8::Locker.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use v8;

use crate::security::CustomAllocator;
use openworkers_core::RuntimeLimits;

/// A reusable V8 isolate that requires explicit locking via v8::Locker
///
/// This represents the V8 engine instance (heap, GC, JIT compiler) without
/// automatic entry management. It must be locked with v8::Locker before use.
pub struct LockerManagedIsolate {
    pub isolate: v8::UnenteredIsolate,
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    pub memory_limit_hit: Arc<AtomicBool>,
    /// Whether a snapshot was used for initialization
    pub use_snapshot: bool,
}

impl LockerManagedIsolate {
    /// Create a new locker-managed isolate
    ///
    /// This is expensive (~3-5ms without snapshot, ~100µs with snapshot)
    /// and should be done lazily by the pool, not per-request.
    pub fn new(limits: RuntimeLimits) -> Self {
        // Get global V8 platform (initialized once, shared across all modules)
        let platform = crate::platform::get_platform();

        // Memory limit tracking for ArrayBuffer allocations
        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Convert heap limits from MB to bytes
        let heap_initial = limits.heap_initial_mb * 1024 * 1024;
        let heap_max = limits.heap_max_mb * 1024 * 1024;

        // Create custom ArrayBuffer allocator to enforce memory limits on external memory
        // This is critical: V8 heap limits don't cover ArrayBuffers, Uint8Array, etc.
        let array_buffer_allocator = CustomAllocator::new(heap_max, Arc::clone(&memory_limit_hit));

        // Load snapshot (centralized, handles empty file case)
        let snapshot_ref = crate::platform::get_snapshot();

        // Create isolate with UnenteredIsolate (no auto-enter!)
        let mut params = v8::CreateParams::default()
            .heap_limits(heap_initial, heap_max)
            .array_buffer_allocator(array_buffer_allocator.into_v8_allocator())
            .allow_atomics_wait(false); // Security: prevent Atomics.wait() from blocking

        if let Some(snapshot_data) = snapshot_ref {
            params = params.snapshot_blob((*snapshot_data).into());
        }

        let isolate = v8::Isolate::new_unentered(params);

        let use_snapshot = snapshot_ref.is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
        }
    }

    /// Check if memory limit was hit
    pub fn memory_limit_hit(&self) -> bool {
        self.memory_limit_hit
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locker_managed_isolate_creation() {
        let limits = RuntimeLimits::default();
        let isolate = LockerManagedIsolate::new(limits);

        // Isolate should be created successfully
        assert!(
            !isolate
                .memory_limit_hit
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn test_multiple_locker_managed_isolates() {
        let limits = RuntimeLimits::default();

        // Should be able to create multiple isolates without LIFO constraint
        let isolate1 = LockerManagedIsolate::new(limits.clone());
        let isolate2 = LockerManagedIsolate::new(limits);

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

        // Drop in any order - no LIFO assertion!
        drop(isolate1);
        drop(isolate2);
    }

    #[test]
    fn test_with_locker() {
        let limits = RuntimeLimits::default();
        let mut isolate_wrapper = LockerManagedIsolate::new(limits);

        // Create Locker - it handles enter/exit automatically via RAII
        let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);

        // Now we can use the isolate via DerefMut
        let scope = std::pin::pin!(v8::HandleScope::new(&mut *locker));
        let _scope = scope.init();

        // Locker drop will call exit() automatically
    }
}
