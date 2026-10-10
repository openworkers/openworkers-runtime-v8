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

use crate::security::{HeapLimitState, Turn, install_heap_limit_callback};
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
    /// Per-isolate pending external memory delta.
    ///
    /// When an `ExternalMemoryGuard` is dropped without the lock held,
    /// the adjustment is accumulated here and applied on next `JsLock::new()`.
    /// Per-isolate (not global) to prevent cross-isolate contamination.
    pub pending_memory_delta: Arc<AtomicI64>,
    /// The foreground tasks V8 posts for this isolate.
    pub(crate) foreground: Arc<crate::platform::ForegroundTasks>,
    /// The request that runs JS here, for the guards that stop one request.
    pub(crate) turn: Arc<Turn>,
    /// Heap limit state - must be kept alive for the isolate's lifetime
    pub(crate) heap_limit_state: Box<HeapLimitState>,
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

        let params = crate::v8_helpers::worker_create_params(&limits, &memory_limit_hit);
        let mut isolate = crate::v8_helpers::new_isolate(params);
        let foreground = crate::platform::register(&isolate);
        let turn = Arc::new(Turn::new(
            isolate.thread_safe_handle(),
            Arc::clone(&memory_limit_hit),
        ));

        // Install heap limit callback to prevent V8 OOM from crashing the process
        let heap_limit_state =
            install_heap_limit_callback(&mut isolate, Arc::clone(&memory_limit_hit), heap_max);

        // SAFETY: the embedder state attached to this isolate, here and later
        // through the lock, is Send: heap limit state, Arcs and atomics.
        let isolate = unsafe { isolate.try_into_shared() }
            .unwrap_or_else(|e| panic!("isolate cannot be shared: {e}"));

        let use_snapshot = crate::platform::get_snapshot().is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
            pending_memory_delta: Arc::new(AtomicI64::new(0)),
            foreground,
            turn,
            heap_limit_state,
        }
    }

    /// Acquire the V8 lock and create a JsLock.
    ///
    /// Returns both the Locker (RAII mutex) and JsLock (RAII GC tracking).
    /// Both are dropped together when the caller's scope ends.
    pub fn lock(&self) -> (v8::Locker<'_>, crate::gc::JsLock) {
        let mut locker = self.isolate.lock();
        let js = crate::gc::JsLock::new(&mut locker, &self.pending_memory_delta);
        (locker, js)
    }

    /// The bytes the JS heap of this isolate uses. Takes the isolate lock.
    pub fn heap_used_bytes(&self) -> usize {
        let (mut locker, _js_lock) = self.lock();

        locker.get_heap_statistics().used_heap_size()
    }

    /// Check if memory limit was hit
    pub fn memory_limit_hit(&self) -> bool {
        self.memory_limit_hit
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for LockerManagedIsolate {
    fn drop(&mut self) {
        self.foreground.forget();
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
