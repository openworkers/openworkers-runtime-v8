//! Deferred destruction queue for V8 handles.
//!
//! This module provides thread-safe deferred destruction of V8 Global handles
//! that may be dropped from threads that don't hold the isolate lock.
//!
//! ## Problem
//!
//! V8 Global handles should ideally be dropped while holding the isolate lock.
//! In a multi-threaded pool architecture, an object containing a Global handle
//! might be dropped from a thread that doesn't currently hold the lock.
//!
//! ## Solution
//!
//! Queue handles for deferred destruction, then process the queue whenever
//! the lock is acquired (in `JsLock::new()`).
//!
//! ## Usage
//!
//! ```ignore
//! use crate::gc::DeferredDestructionQueue;
//!
//! // Per-isolate queue (stored in LockerManagedIsolate)
//! let queue = DeferredDestructionQueue::new();
//!
//! // From any thread, defer a handle destruction
//! queue.defer(my_global_handle);
//!
//! // When acquiring the lock, process pending destructions
//! queue.process_all(&mut isolate);
//! ```

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Type-erased V8 handle wrapper for deferred destruction.
///
/// This wraps any `v8::Global<T>` in a type-erased container that can be
/// stored in a heterogeneous queue and safely destroyed later.
pub struct DeferredHandle {
    /// Raw pointer to the boxed Global handle
    ptr: *mut (),

    /// Function pointer to drop the boxed handle
    drop_fn: fn(*mut ()),
}

// SAFETY: DeferredHandle is Send because:
// - The ptr points to a heap-allocated v8::Global<T>
// - v8::Global<T> is designed for cross-thread storage
// - We only access ptr during drop (under lock)
unsafe impl Send for DeferredHandle {}

impl DeferredHandle {
    /// Create a new deferred handle from a v8::Global<T>.
    ///
    /// The handle is moved into heap storage and will be destroyed
    /// when this DeferredHandle is dropped.
    pub fn new<T: 'static>(handle: v8::Global<T>) -> Self {
        let boxed = Box::new(handle);
        let ptr = Box::into_raw(boxed) as *mut ();

        Self {
            ptr,
            drop_fn: |ptr| {
                // SAFETY: ptr was created from Box::into_raw of a v8::Global<T>
                let _ = unsafe { Box::from_raw(ptr as *mut v8::Global<T>) };
            },
        }
    }
}

impl Drop for DeferredHandle {
    fn drop(&mut self) {
        // SAFETY: We own the ptr and this is the only place we drop it
        (self.drop_fn)(self.ptr);
    }
}

/// Per-isolate queue for deferred V8 handle destruction.
///
/// This queue is thread-safe and can receive handles from any thread.
/// Pending handles are processed when `process_all()` is called under
/// the isolate lock.
pub struct DeferredDestructionQueue {
    /// Queue of handles pending destruction
    queue: Mutex<VecDeque<DeferredHandle>>,

    /// Fast check for pending items (avoids lock acquisition on hot path)
    pending_count: AtomicU64,
}

impl Default for DeferredDestructionQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DeferredDestructionQueue {
    /// Create a new empty queue.
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(8)),
            pending_count: AtomicU64::new(0),
        }
    }

    /// Queue a handle for deferred destruction.
    ///
    /// This is thread-safe and can be called from any thread without
    /// holding the isolate lock.
    pub fn defer<T: 'static>(&self, handle: v8::Global<T>) {
        let deferred = DeferredHandle::new(handle);
        self.queue
            .lock()
            .expect("deferred destruction queue poisoned")
            .push_back(deferred);
        self.pending_count.fetch_add(1, Ordering::Release);

        tracing::trace!("Deferred V8 handle destruction (pending: {})", self.len());
    }

    /// Check if there are pending destructions.
    ///
    /// This is a fast lock-free check.
    #[inline]
    pub fn has_pending(&self) -> bool {
        self.pending_count.load(Ordering::Acquire) > 0
    }

    /// Get the number of pending destructions.
    #[inline]
    pub fn len(&self) -> u64 {
        self.pending_count.load(Ordering::Acquire)
    }

    /// Check if the queue is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        !self.has_pending()
    }

    /// Process all pending destructions.
    ///
    /// This should be called while holding the v8::Locker for the associated isolate.
    /// The v8::Global handles will be dropped, which is safe because:
    /// - v8::Global internally stores a pointer to its isolate
    /// - The caller holds the v8::Locker ensuring the isolate is accessible
    /// - Dropping a Global just decrements a ref count in V8
    ///
    /// Call this before creating `JsLock` to ensure cleanup happens before any work.
    pub fn process_all(&self) {
        // Fast path: no pending destructions
        if !self.has_pending() {
            return;
        }

        // Take all pending handles
        let handles: VecDeque<DeferredHandle> = {
            let mut queue = self
                .queue
                .lock()
                .expect("deferred destruction queue poisoned");
            std::mem::take(&mut *queue)
        };

        let count = handles.len();

        if count == 0 {
            return;
        }

        // Drop all handles - this happens under the lock since caller holds it
        // The handles are dropped here when they go out of scope
        drop(handles);

        // Update counter
        self.pending_count
            .fetch_sub(count as u64, Ordering::Release);

        tracing::trace!("Processed {} deferred handle destructions", count);
    }
}

impl std::fmt::Debug for DeferredDestructionQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredDestructionQueue")
            .field("pending_count", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_creation() {
        let queue = DeferredDestructionQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert!(!queue.has_pending());
    }

    // Note: Full integration tests require a V8 isolate and are in gc/tests.rs
}
