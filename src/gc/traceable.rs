//! GcTraceable trait for types that want automatic memory tracking.

use super::ExternalMemoryGuard;

/// Trait for types that can report their external memory size to V8.
///
/// Implement this trait for types that hold significant amounts of memory
/// that V8 doesn't know about (Rust allocations, file handles, etc.).
///
/// # Example
///
/// ```ignore
/// use crate::gc::GcTraceable;
///
/// struct ImageBuffer {
///     pixels: Vec<u8>,
///     metadata: String,
/// }
///
/// impl GcTraceable for ImageBuffer {
///     fn external_memory_size(&self) -> usize {
///         self.pixels.capacity() + self.metadata.capacity()
///     }
/// }
/// ```
///
/// # With derive macro (future)
///
/// ```ignore
/// #[derive(GcTraceable)]
/// struct ImageBuffer {
///     #[gc(track)]
///     pixels: Vec<u8>,
///     #[gc(track)]
///     metadata: String,
///     // Fields without #[gc(track)] are not counted
///     width: u32,
///     height: u32,
/// }
/// ```
pub trait GcTraceable {
    /// Returns the size in bytes of external memory held by this value.
    ///
    /// This should include:
    /// - Heap allocations (Vec capacity, String capacity, HashMap entries, etc.)
    /// - Memory-mapped files
    /// - Native handles with associated memory
    ///
    /// This should NOT include:
    /// - The size of `self` (stack/inline size)
    /// - V8-managed memory (already tracked)
    fn external_memory_size(&self) -> usize;
}

// Implementations for common types

impl GcTraceable for Vec<u8> {
    fn external_memory_size(&self) -> usize {
        self.capacity()
    }
}

impl GcTraceable for String {
    fn external_memory_size(&self) -> usize {
        self.capacity()
    }
}

impl GcTraceable for bytes::Bytes {
    fn external_memory_size(&self) -> usize {
        // Bytes is reference-counted, but we report the visible length
        // The actual backing memory might be shared
        self.len()
    }
}

impl<T: GcTraceable> GcTraceable for Option<T> {
    fn external_memory_size(&self) -> usize {
        self.as_ref().map(|v| v.external_memory_size()).unwrap_or(0)
    }
}

impl<T: GcTraceable> GcTraceable for Box<T> {
    fn external_memory_size(&self) -> usize {
        std::mem::size_of::<T>() + (**self).external_memory_size()
    }
}

impl<T: GcTraceable> GcTraceable for std::rc::Rc<T> {
    fn external_memory_size(&self) -> usize {
        // Only count if we're the only reference
        // Otherwise, another Rc is responsible for tracking
        if std::rc::Rc::strong_count(self) == 1 {
            std::mem::size_of::<T>() + (**self).external_memory_size()
        } else {
            0
        }
    }
}

impl<T: GcTraceable> GcTraceable for std::sync::Arc<T> {
    fn external_memory_size(&self) -> usize {
        // Only count if we're the only reference
        if std::sync::Arc::strong_count(self) == 1 {
            std::mem::size_of::<T>() + (**self).external_memory_size()
        } else {
            0
        }
    }
}

impl<T: GcTraceable> GcTraceable for Vec<T> {
    fn external_memory_size(&self) -> usize {
        let base = self.capacity() * std::mem::size_of::<T>();
        let contents: usize = self.iter().map(|v| v.external_memory_size()).sum();
        base + contents
    }
}

/// Helper to create an ExternalMemoryGuard from a GcTraceable value.
///
/// # Example
///
/// ```ignore
/// let buffer = vec![0u8; 1024];
/// let guard = tracked_guard(&buffer);
/// // guard now tracks 1024 bytes
/// ```
pub fn tracked_guard<T: GcTraceable>(value: &T) -> ExternalMemoryGuard {
    ExternalMemoryGuard::new(value.external_memory_size() as i64)
}

/// Wrapper that automatically tracks a value's external memory.
///
/// This combines a value with its memory guard, automatically
/// updating the tracked memory when the value changes.
///
/// # Example
///
/// ```ignore
/// let mut tracked = Tracked::new(vec![0u8; 1024]);
/// // V8 now knows about 1024 bytes
///
/// tracked.get_mut().resize(2048, 0);
/// tracked.update_size();
/// // V8 now knows about 2048 bytes
/// ```
pub struct Tracked<T: GcTraceable> {
    value: T,
    guard: ExternalMemoryGuard,
}

impl<T: GcTraceable> Tracked<T> {
    /// Create a new tracked value.
    pub fn new(value: T) -> Self {
        let guard = tracked_guard(&value);
        Self { value, guard }
    }

    /// Get a reference to the value.
    pub fn get(&self) -> &T {
        &self.value
    }

    /// Get a mutable reference to the value.
    ///
    /// After modifying the value, call `update_size()` to sync
    /// the tracked memory with V8.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Update the tracked size after modifications.
    ///
    /// Call this after mutating the value to ensure V8's
    /// external memory tracking stays accurate.
    pub fn update_size(&mut self) {
        self.guard.set(self.value.external_memory_size() as i64);
    }

    /// Consume and return the inner value.
    ///
    /// The memory guard is dropped, removing the tracking.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T: GcTraceable> std::ops::Deref for Tracked<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T: GcTraceable + std::fmt::Debug> std::fmt::Debug for Tracked<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracked")
            .field("value", &self.value)
            .field("tracked_bytes", &self.guard.amount())
            .finish()
    }
}
