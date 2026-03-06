//! JsLock - RAII tracking for V8 isolate with GC support.
//!
//! This module provides utilities for tracking when a V8 isolate is locked,
//! enabling deferred external memory adjustments.
//!
//! Pending memory deltas are per-isolate (stored as `Arc<AtomicI64>` in
//! `LockerManagedIsolate`). This prevents cross-isolate contamination when
//! guards are dropped without holding the lock.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Per-isolate state stored in the thread-local.
struct CurrentIsolateState {
    isolate: *mut v8::Isolate,
    pending_delta: Arc<AtomicI64>,
}

thread_local! {
    /// Current isolate + pending delta for this thread (if any).
    /// Used by `JsLock::try_current()` to access the isolate from anywhere.
    static CURRENT_STATE: RefCell<Option<CurrentIsolateState>> = const { RefCell::new(None) };
}

/// RAII guard that tracks when a V8 isolate is locked.
///
/// Create this AFTER acquiring a v8::Locker to enable GC tracking features.
///
/// While this guard exists:
/// - `JsLock::try_current()` returns `Some` for memory tracking
/// - External memory adjustments are applied immediately
///
/// On construction:
/// - Applies any pending deferred memory adjustments for this isolate
/// - Registers the isolate in thread-local for `try_current()`
///
/// On drop:
/// - Unregisters from thread-local
///
/// # Example
///
/// ```ignore
/// // Acquire the locker first (as usual)
/// let mut locker = v8::Locker::new(&mut isolate);
///
/// // Process deferred destructions first (for pooled isolates)
/// destruction_queue.process_all(&mut locker);
///
/// // Then create JsLock for GC tracking
/// let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);
///
/// // Now ExternalMemoryGuard can track memory
/// let guard = ExternalMemoryGuard::new(1_000_000);
/// ```
pub struct JsLock {
    isolate: *mut v8::Isolate,
    #[allow(dead_code)]
    pending_delta: Arc<AtomicI64>,
    previous: Option<CurrentIsolateState>,
}

impl JsLock {
    /// Create a new JsLock for the given isolate.
    ///
    /// The isolate must already be locked via v8::Locker.
    /// This applies any pending memory adjustments and registers for `try_current()`.
    ///
    /// `pending_delta` is the per-isolate deferred memory accumulator from
    /// `LockerManagedIsolate`.
    ///
    /// Note: For pooled isolates, call `DeferredDestructionQueue::process_all()`
    /// BEFORE creating JsLock to ensure deferred handles are cleaned up.
    pub fn new(isolate: &mut v8::Isolate, pending_delta: &Arc<AtomicI64>) -> Self {
        let isolate_ptr = isolate as *mut _;
        let pending_delta = Arc::clone(pending_delta);

        // Apply any deferred memory adjustments for THIS isolate
        let pending = pending_delta.swap(0, Ordering::SeqCst);

        if pending != 0 {
            isolate.adjust_amount_of_external_allocated_memory(pending);
            tracing::trace!(
                "Applied deferred external memory adjustment: {} bytes",
                pending
            );
        }

        // Save previous state (for nested locks) and set ourselves as current
        let previous = CURRENT_STATE.with(|c| {
            c.replace(Some(CurrentIsolateState {
                isolate: isolate_ptr,
                pending_delta: Arc::clone(&pending_delta),
            }))
        });

        Self {
            isolate: isolate_ptr,
            pending_delta,
            previous,
        }
    }

    /// Try to get the current JsLock for this thread.
    ///
    /// Returns `Some` if a JsLock is currently held on this thread,
    /// `None` otherwise.
    ///
    /// This is used by `ExternalMemoryGuard` to determine whether to
    /// apply memory adjustments immediately or defer them.
    pub fn try_current() -> Option<JsLockRef> {
        CURRENT_STATE.with(|c| {
            let borrow = c.borrow();

            borrow.as_ref().map(|state| JsLockRef {
                isolate: state.isolate,
                pending_delta: Arc::clone(&state.pending_delta),
            })
        })
    }

    /// Adjust the amount of external memory tracked by V8.
    ///
    /// Positive values indicate memory was allocated.
    /// Negative values indicate memory was freed.
    pub fn adjust_external_memory(&self, delta: i64) {
        if delta != 0 {
            unsafe {
                (*self.isolate).adjust_amount_of_external_allocated_memory(delta);
            }
            tracing::trace!("Adjusted external memory: {} bytes", delta);
        }
    }

    /// Get the raw isolate pointer (for advanced usage).
    pub fn isolate_ptr(&self) -> *mut v8::Isolate {
        self.isolate
    }
}

impl Drop for JsLock {
    fn drop(&mut self) {
        // Restore previous state (for nested locks)
        CURRENT_STATE.with(|c| c.replace(self.previous.take()));
    }
}

/// A reference to the current JsLock.
///
/// This is a lightweight handle returned by `JsLock::try_current()`.
#[derive(Clone)]
pub struct JsLockRef {
    isolate: *mut v8::Isolate,
    pending_delta: Arc<AtomicI64>,
}

impl JsLockRef {
    /// Adjust the amount of external memory tracked by V8.
    pub fn adjust_external_memory(&self, delta: i64) {
        if delta != 0 {
            unsafe {
                (*self.isolate).adjust_amount_of_external_allocated_memory(delta);
            }
            tracing::trace!("Adjusted external memory (via ref): {} bytes", delta);
        }
    }

    /// Get a clone of the per-isolate pending memory delta accumulator.
    ///
    /// Used by `ExternalMemoryGuard` to capture the target isolate at creation
    /// time, so deferred drops go to the correct isolate.
    pub fn pending_delta(&self) -> Arc<AtomicI64> {
        Arc::clone(&self.pending_delta)
    }
}
