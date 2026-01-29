//! Isolate managed via v8::Locker (no auto-enter/exit)
//!
//! This module provides a wrapper around v8::UnenteredIsolate that is designed
//! for use in multi-threaded isolate pools with v8::Locker.
//!
//! Unlike SharedIsolate which uses OwnedIsolate (auto-enter), LockerManagedIsolate
//! uses UnenteredIsolate and requires explicit locking via v8::Locker.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use v8;

use crate::gc::DeferredDestructionQueue;
#[cfg(not(feature = "sandbox"))]
use crate::security::CustomAllocator;
use crate::security::{HeapLimitState, install_heap_limit_callback};
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
    /// Queue for deferred V8 handle destructions
    ///
    /// Handles dropped without the lock held are queued here and
    /// processed on the next lock acquisition. Wrapped in Arc for
    /// safe sharing during lock acquisition.
    pub deferred_destruction_queue: Arc<DeferredDestructionQueue>,
    /// Heap limit state - must be kept alive for the isolate's lifetime
    #[allow(dead_code)]
    _heap_limit_state: Box<HeapLimitState>,
}

impl LockerManagedIsolate {
    /// Create a new locker-managed isolate
    ///
    /// This is expensive (few ms without snapshot, tens of µs with snapshot)
    /// and should be done lazily by the pool, not per-request.
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

        // Create isolate with UnenteredIsolate (no auto-enter!)
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

        let mut isolate = v8::Isolate::new_unentered(params);

        // Install heap limit callback to prevent V8 OOM from crashing the process
        // We need to lock the isolate temporarily to add the callback
        let heap_limit_state = {
            let mut locker = v8::Locker::new(&mut isolate);
            install_heap_limit_callback(&mut locker, Arc::clone(&memory_limit_hit), heap_max)
        };

        let use_snapshot = snapshot_ref.is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
            deferred_destruction_queue: Arc::new(DeferredDestructionQueue::new()),
            _heap_limit_state: heap_limit_state,
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
