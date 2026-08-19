//! Isolate managed via v8::Locker (no auto-enter/exit)
//!
//! This module provides a wrapper around v8::SharedIsolate that is designed
//! for use in multi-threaded isolate pools with v8::Locker.
//!
//! Unlike Worker's Runtime which uses OwnedIsolate (auto-enter), LockerManagedIsolate
//! uses SharedIsolate and requires explicit locking via v8::Locker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64};
use v8;

use crate::gc::DeferredDestructionQueue;
use crate::security::{HeapLimitState, install_heap_limit_callback};
use openworkers_core::RuntimeLimits;

/// A reusable V8 isolate that requires explicit locking via v8::Locker
///
/// This represents the V8 engine instance (heap, GC, JIT compiler) without
/// automatic entry management. It must be locked with v8::Locker before use.
pub struct LockerManagedIsolate {
    pub isolate: v8::SharedIsolate,
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
    /// Per-isolate pending external memory delta.
    ///
    /// When an `ExternalMemoryGuard` is dropped without the lock held,
    /// the adjustment is accumulated here and applied on next `JsLock::new()`.
    /// Per-isolate (not global) to prevent cross-isolate contamination.
    pub pending_memory_delta: Arc<AtomicI64>,
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

        let heap_max = limits.heap_max_mb * 1024 * 1024;

        // Load snapshot (centralized, handles empty file case)
        let snapshot_ref = crate::platform::get_snapshot();

        let mut params = crate::v8_helpers::worker_create_params(&limits, &memory_limit_hit);

        if let Some(snapshot_data) = snapshot_ref {
            params = params.snapshot_blob((*snapshot_data).into());
        }

        let mut isolate = v8::Isolate::new(params);

        // Install heap limit callback to prevent V8 OOM from crashing the process
        let heap_limit_state =
            install_heap_limit_callback(&mut isolate, Arc::clone(&memory_limit_hit), heap_max);

        // SAFETY: the embedder state attached to this isolate, here and later
        // through the lock, is Send: heap limit state, Arcs and atomics.
        let isolate = unsafe { isolate.try_into_shared() }
            .unwrap_or_else(|e| panic!("isolate cannot be shared: {e}"));

        let use_snapshot = snapshot_ref.is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
            deferred_destruction_queue: Arc::new(DeferredDestructionQueue::new()),
            pending_memory_delta: Arc::new(AtomicI64::new(0)),
            _heap_limit_state: heap_limit_state,
        }
    }

    /// Acquire the V8 lock, process deferred destructions, and create a JsLock.
    ///
    /// This encapsulates the 3-step lock acquisition pattern:
    /// 1. `SharedIsolate::lock()`: acquire V8 mutex
    /// 2. `deferred_destruction_queue.process_all()`: clean up queued handles
    /// 3. `JsLock::new()`: apply deferred memory deltas + enable GC tracking
    ///
    /// Returns both the Locker (RAII mutex) and JsLock (RAII GC tracking).
    /// Both are dropped together when the caller's scope ends.
    pub fn lock(&self) -> (v8::Locker<'_>, crate::gc::JsLock) {
        let mut locker = self.isolate.lock();
        self.deferred_destruction_queue.process_all();
        let js = crate::gc::JsLock::new(&mut locker, &self.pending_memory_delta);
        (locker, js)
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
        let isolate_wrapper = LockerManagedIsolate::new(limits);

        // Create Locker - it handles enter/exit automatically via RAII
        let mut locker = isolate_wrapper.isolate.lock();

        // Now we can use the isolate via DerefMut
        let scope = std::pin::pin!(v8::HandleScope::new(&mut *locker));
        let _scope = scope.init();

        // Locker drop will call exit() automatically
    }
}
