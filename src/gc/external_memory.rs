//! External memory tracking for V8 GC.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use super::js_lock::JsLock;

/// RAII guard that tracks external memory with V8's garbage collector.
///
/// When created, reports the memory amount to V8 (if lock held).
/// When dropped, subtracts the memory amount (immediately or deferred).
///
/// The guard captures the per-isolate pending delta accumulator at creation
/// time. This ensures that deferred drops (without the lock held) go to the
/// correct isolate, not a global accumulator.
///
/// # Example
///
/// ```ignore
/// struct LargeBuffer {
///     data: Vec<u8>,
///     _guard: ExternalMemoryGuard,
/// }
///
/// impl LargeBuffer {
///     fn new(size: usize) -> Self {
///         let data = vec![0u8; size];
///         let guard = ExternalMemoryGuard::new(size as i64);
///         Self { data, _guard: guard }
///     }
///
///     fn resize(&mut self, new_size: usize) {
///         let old_size = self.data.len();
///         self.data.resize(new_size, 0);
///         self._guard.adjust((new_size as i64) - (old_size as i64));
///     }
/// }
/// ```
pub struct ExternalMemoryGuard {
    amount: i64,
    /// Per-isolate deferred memory accumulator.
    /// Captured at creation time so deferred drops go to the correct isolate.
    pending_delta: Option<Arc<AtomicI64>>,
}

impl ExternalMemoryGuard {
    /// Create a new guard tracking `amount` bytes of external memory.
    ///
    /// If a JsLock is currently held, the adjustment is applied immediately
    /// and the per-isolate accumulator is captured for future deferred drops.
    ///
    /// If no JsLock is held, the adjustment is deferred to the captured
    /// isolate's accumulator (if available from a previous lock).
    pub fn new(amount: i64) -> Self {
        let pending_delta = if let Some(lock) = JsLock::try_current() {
            if amount != 0 {
                lock.adjust_external_memory(amount);
            }

            Some(lock.pending_delta())
        } else {
            None
        };

        Self {
            amount,
            pending_delta,
        }
    }

    /// Create a guard with zero initial amount.
    ///
    /// Use `adjust()` to update the tracked amount later.
    pub fn empty() -> Self {
        let pending_delta = JsLock::try_current().map(|lock| lock.pending_delta());

        Self {
            amount: 0,
            pending_delta,
        }
    }

    /// Adjust the tracked amount by `delta` bytes.
    ///
    /// Positive delta = more memory allocated.
    /// Negative delta = memory freed.
    pub fn adjust(&mut self, delta: i64) {
        if delta != 0 {
            self.amount += delta;

            if let Some(lock) = JsLock::try_current() {
                lock.adjust_external_memory(delta);

                // Capture the pending delta if we didn't have one yet
                if self.pending_delta.is_none() {
                    self.pending_delta = Some(lock.pending_delta());
                }
            } else if let Some(ref pending) = self.pending_delta {
                pending.fetch_add(delta, Ordering::SeqCst);
            }
        }
    }

    /// Set the tracked amount to a new value.
    ///
    /// This calculates and applies the delta automatically.
    pub fn set(&mut self, new_amount: i64) {
        self.adjust(new_amount - self.amount);
    }

    /// Get the currently tracked amount.
    pub fn amount(&self) -> i64 {
        self.amount
    }
}

impl Drop for ExternalMemoryGuard {
    fn drop(&mut self) {
        if self.amount != 0 {
            // Subtract our tracked memory
            if let Some(lock) = JsLock::try_current() {
                lock.adjust_external_memory(-self.amount);
            } else if let Some(ref pending) = self.pending_delta {
                pending.fetch_add(-self.amount, Ordering::SeqCst);
            }
            // else: no lock and no captured isolate — memory tracking lost.
            // This only happens for guards created without a JsLock (tests).
        }
    }
}

impl Default for ExternalMemoryGuard {
    fn default() -> Self {
        Self::empty()
    }
}

impl std::fmt::Debug for ExternalMemoryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalMemoryGuard")
            .field("amount", &self.amount)
            .finish()
    }
}
