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
        if delta == 0 {
            return;
        }

        self.amount += delta;

        match (JsLock::try_current(), &self.pending_delta) {
            // The first isolate this guard meets gets the whole amount: what
            // it held before was reported nowhere
            (Some(lock), None) => {
                lock.adjust_external_memory(self.amount);
                self.pending_delta = Some(lock.pending_delta());
            }
            (Some(lock), Some(captured)) if Arc::ptr_eq(&lock.pending_delta(), captured) => {
                lock.adjust_external_memory(delta);
            }
            // Another isolate's lock, or none: the guard's own isolate takes
            // it when it is locked next
            (_, Some(captured)) => {
                captured.fetch_add(delta, Ordering::SeqCst);
            }
            (None, None) => {}
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
        if self.amount == 0 {
            return;
        }

        // Subtract our tracked memory, from the isolate it was reported to
        match (JsLock::try_current(), &self.pending_delta) {
            (Some(lock), Some(captured)) if Arc::ptr_eq(&lock.pending_delta(), captured) => {
                lock.adjust_external_memory(-self.amount);
            }
            (_, Some(captured)) => {
                captured.fetch_add(-self.amount, Ordering::SeqCst);
            }
            // No isolate ever saw this amount, so none has it to take back
            (_, None) => {}
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
