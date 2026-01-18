//! JsLock - RAII tracking for V8 isolate with GC support.
//!
//! This module provides utilities for tracking when a V8 isolate is locked,
//! enabling deferred external memory adjustments.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};

/// Global pending memory delta for deferred adjustments.
/// When memory is freed without holding the lock, we accumulate here.
/// Applied on next JsLock construction.
static PENDING_MEMORY_DELTA: AtomicI64 = AtomicI64::new(0);

thread_local! {
    /// Current isolate pointer for this thread (if any).
    /// Used by `JsLock::try_current()` to access the isolate from anywhere.
    static CURRENT_ISOLATE: Cell<Option<*mut v8::Isolate>> = const { Cell::new(None) };
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
/// - Applies any pending deferred memory adjustments
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
/// let _gc_lock = JsLock::new(&mut *locker);
///
/// // Now ExternalMemoryGuard can track memory
/// let guard = ExternalMemoryGuard::new(1_000_000);
/// ```
pub struct JsLock {
    isolate: *mut v8::Isolate,
    previous: Option<*mut v8::Isolate>,
}

impl JsLock {
    /// Create a new JsLock for the given isolate.
    ///
    /// The isolate must already be locked via v8::Locker.
    /// This applies any pending memory adjustments and registers for `try_current()`.
    ///
    /// Note: For pooled isolates, call `DeferredDestructionQueue::process_all()`
    /// BEFORE creating JsLock to ensure deferred handles are cleaned up.
    pub fn new(isolate: &mut v8::Isolate) -> Self {
        let isolate_ptr = isolate as *mut _;

        // Apply any deferred memory adjustments
        let pending = PENDING_MEMORY_DELTA.swap(0, Ordering::SeqCst);

        if pending != 0 {
            isolate.adjust_amount_of_external_allocated_memory(pending);
            log::trace!(
                "Applied deferred external memory adjustment: {} bytes",
                pending
            );
        }

        // Save previous isolate (for nested locks) and set ourselves as current
        let previous = CURRENT_ISOLATE.with(|c| c.replace(Some(isolate_ptr)));

        Self {
            isolate: isolate_ptr,
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
        CURRENT_ISOLATE.with(|c| c.get().map(|ptr| JsLockRef { isolate: ptr }))
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
            log::trace!("Adjusted external memory: {} bytes", delta);
        }
    }

    /// Get the raw isolate pointer (for advanced usage).
    pub fn isolate_ptr(&self) -> *mut v8::Isolate {
        self.isolate
    }
}

impl Drop for JsLock {
    fn drop(&mut self) {
        // Restore previous isolate (for nested locks)
        CURRENT_ISOLATE.with(|c| c.set(self.previous));
    }
}

/// A reference to the current JsLock.
///
/// This is a lightweight handle returned by `JsLock::try_current()`.
#[derive(Clone, Copy)]
pub struct JsLockRef {
    isolate: *mut v8::Isolate,
}

impl JsLockRef {
    /// Adjust the amount of external memory tracked by V8.
    pub fn adjust_external_memory(&self, delta: i64) {
        if delta != 0 {
            unsafe {
                (*self.isolate).adjust_amount_of_external_allocated_memory(delta);
            }
            log::trace!("Adjusted external memory (via ref): {} bytes", delta);
        }
    }
}

/// Defer a memory adjustment until the next JsLock is acquired.
///
/// This is called when memory is freed without holding the lock.
pub(crate) fn defer_memory_adjustment(delta: i64) {
    if delta != 0 {
        PENDING_MEMORY_DELTA.fetch_add(delta, Ordering::SeqCst);
        log::trace!("Deferred external memory adjustment: {} bytes", delta);
    }
}

/// Get the current pending memory delta (for testing/debugging).
#[cfg(test)]
pub(crate) fn pending_memory_delta() -> i64 {
    PENDING_MEMORY_DELTA.load(Ordering::SeqCst)
}
