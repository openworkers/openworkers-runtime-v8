//! External memory tracking for V8 GC.

use super::js_lock::{JsLock, defer_memory_adjustment};

/// RAII guard that tracks external memory with V8's garbage collector.
///
/// When created, reports the memory amount to V8 (if lock held).
/// When dropped, subtracts the memory amount (immediately or deferred).
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
}

impl ExternalMemoryGuard {
    /// Create a new guard tracking `amount` bytes of external memory.
    ///
    /// If a JsLock is currently held, the adjustment is applied immediately.
    /// Otherwise, it's deferred until the next lock is acquired.
    pub fn new(amount: i64) -> Self {
        if amount != 0 {
            if let Some(lock) = JsLock::try_current() {
                lock.adjust_external_memory(amount);
            } else {
                defer_memory_adjustment(amount);
            }
        }

        Self { amount }
    }

    /// Create a guard with zero initial amount.
    ///
    /// Use `adjust()` to update the tracked amount later.
    pub fn empty() -> Self {
        Self { amount: 0 }
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
            } else {
                defer_memory_adjustment(delta);
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
            } else {
                defer_memory_adjustment(-self.amount);
            }
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
