//! Custom V8 ArrayBuffer allocator with memory limit enforcement.
//!
//! V8's heap limits (`CreateParams::heap_limits`) only cover the JavaScript heap,
//! NOT ArrayBuffer allocations (Uint8Array, Buffer, etc.). This allocator tracks
//! and limits external memory to prevent memory bombs.
//!
//! ## How it works
//!
//! 1. V8 calls `allocate()` when JS does `new ArrayBuffer(n)` or `new Uint8Array(n)`
//! 2. We track total allocated bytes in an atomic counter
//! 3. If limit exceeded, return NULL → V8 throws `RangeError: Array buffer allocation failed`
//! 4. On `free()`, we decrement the counter

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use v8::{RustAllocatorVtable, UniqueRef};

/// Custom ArrayBuffer allocator that tracks and limits external memory.
///
/// # Example
///
/// ```rust,ignore
/// let memory_flag = Arc::new(AtomicBool::new(false));
/// let allocator = CustomAllocator::new(128 * 1024 * 1024, memory_flag.clone());
///
/// let params = v8::CreateParams::default()
///     .array_buffer_allocator(allocator.into_v8_allocator());
/// let isolate = v8::Isolate::new(params);
///
/// // After execution, check if limit was hit
/// if memory_flag.load(Ordering::SeqCst) {
///     println!("Memory limit exceeded!");
/// }
/// ```
pub struct CustomAllocator {
    /// Maximum allowed bytes for ArrayBuffer allocations
    max: usize,
    /// Current total allocated bytes (atomic for thread-safety)
    count: AtomicUsize,
    /// Flag set when memory limit is hit (shared with runtime)
    memory_limit_hit: Arc<AtomicBool>,
}

impl CustomAllocator {
    /// Create a new allocator with the given limit.
    ///
    /// # Arguments
    ///
    /// * `max_bytes` - Maximum total bytes allowed for ArrayBuffer allocations
    /// * `memory_limit_hit` - Shared flag set when limit is exceeded
    pub fn new(max_bytes: usize, memory_limit_hit: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            max: max_bytes,
            count: AtomicUsize::new(0),
            memory_limit_hit,
        })
    }

    /// Convert to V8 allocator for use in `CreateParams`.
    pub fn into_v8_allocator(self: Arc<Self>) -> UniqueRef<v8::Allocator> {
        let vtable: &'static RustAllocatorVtable<CustomAllocator> = &RustAllocatorVtable {
            allocate,
            allocate_uninitialized,
            free,
            drop,
        };

        unsafe { v8::new_rust_allocator(Arc::into_raw(self), vtable) }
    }

    /// Get current memory usage in bytes.
    #[allow(dead_code)]
    pub fn current_usage(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// The layout of an allocation of `n` bytes, or None past what the
/// allocator can address.
fn layout(n: usize) -> Option<std::alloc::Layout> {
    std::alloc::Layout::array::<u8>(n).ok()
}

/// Takes `n` bytes of the budget, or answers false with the budget as it was.
fn reserve(allocator: &CustomAllocator, n: usize) -> bool {
    let before = allocator.count.fetch_add(n, Ordering::SeqCst);

    if before
        .checked_add(n)
        .is_some_and(|total| total <= allocator.max)
    {
        return true;
    }

    // V8 calls this from inside its allocator, so the report stays off the
    // tracing subscriber and its locks.
    eprintln!(
        "[openworkers-runtime-v8] ArrayBuffer allocation denied: {}MB exceeds limit of {}MB",
        before.saturating_add(n) / 1024 / 1024,
        allocator.max / 1024 / 1024
    );
    // Set flag so the runtime knows why we failed
    allocator.memory_limit_hit.store(true, Ordering::SeqCst);
    // Rollback the count since we're not actually allocating; without this,
    // failed allocations would permanently "use up" the quota
    allocator.count.fetch_sub(n, Ordering::SeqCst);

    false
}

/// `n` bytes from the system, zeroed or not, or null when it has none: V8
/// then throws a RangeError, where an abort here would take the process.
fn allocate_bytes(allocator: &CustomAllocator, n: usize, zeroed: bool) -> *mut c_void {
    if !reserve(allocator, n) {
        return std::ptr::null_mut();
    }

    if n == 0 {
        return std::ptr::NonNull::<u8>::dangling().as_ptr() as *mut c_void;
    }

    // SAFETY: the layout has a non-zero size.
    let ptr = match layout(n) {
        Some(layout) if zeroed => unsafe { std::alloc::alloc_zeroed(layout) },
        Some(layout) => unsafe { std::alloc::alloc(layout) },
        None => std::ptr::null_mut(),
    };

    if ptr.is_null() {
        eprintln!(
            "[openworkers-runtime-v8] ArrayBuffer allocation of {}MB failed: out of memory",
            n / 1024 / 1024
        );
        allocator.count.fetch_sub(n, Ordering::SeqCst);
    }

    ptr as *mut c_void
}

/// Called by V8 when JS code does `new ArrayBuffer(n)` or `new Uint8Array(n)`.
/// Returns a pointer to zeroed memory, or NULL if the limit is exceeded or the
/// system has no memory for it.
unsafe extern "C" fn allocate(allocator: &CustomAllocator, n: usize) -> *mut c_void {
    allocate_bytes(allocator, n, true)
}

/// Called by V8 for uninitialized allocation (performance optimization).
/// Same as `allocate` but doesn't zero the memory.
unsafe extern "C" fn allocate_uninitialized(allocator: &CustomAllocator, n: usize) -> *mut c_void {
    allocate_bytes(allocator, n, false)
}

/// Called by V8 when an ArrayBuffer is garbage collected.
/// We decrement our counter and free the memory.
unsafe extern "C" fn free(allocator: &CustomAllocator, data: *mut c_void, n: usize) {
    allocator.count.fetch_sub(n, Ordering::SeqCst);

    if n == 0 {
        return;
    }

    // SAFETY: data was allocated by allocate_bytes with this layout.
    if let Some(layout) = layout(n) {
        unsafe { std::alloc::dealloc(data as *mut u8, layout) };
    }
}

/// Called when the allocator itself is dropped (isolate destroyed).
unsafe extern "C" fn drop(allocator: *const CustomAllocator) {
    // SAFETY: allocator was created via Arc::into_raw in into_v8_allocator
    let _ = unsafe { Arc::from_raw(allocator) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator(max: usize) -> (Arc<CustomAllocator>, Arc<AtomicBool>) {
        let hit = Arc::new(AtomicBool::new(false));
        (CustomAllocator::new(max, Arc::clone(&hit)), hit)
    }

    #[test]
    fn memory_the_system_refuses_is_null_not_an_abort() {
        let (allocator, hit) = allocator(usize::MAX);

        let ptr = unsafe { allocate(&allocator, usize::MAX / 2) };

        assert!(ptr.is_null());
        assert_eq!(allocator.current_usage(), 0);
        assert!(
            !hit.load(Ordering::SeqCst),
            "the limit was not what refused it"
        );
    }

    #[test]
    fn the_limit_counts_what_is_live() {
        let (allocator, hit) = allocator(100);

        let first = unsafe { allocate(&allocator, 60) };
        assert!(!first.is_null());
        assert!(
            unsafe { std::slice::from_raw_parts(first as *const u8, 60) }
                .iter()
                .all(|byte| *byte == 0)
        );

        let second = unsafe { allocate(&allocator, 60) };
        assert!(second.is_null());
        assert!(hit.load(Ordering::SeqCst));
        assert_eq!(allocator.current_usage(), 60);

        unsafe { free(&allocator, first, 60) };
        assert_eq!(allocator.current_usage(), 0);

        let empty = unsafe { allocate_uninitialized(&allocator, 0) };
        assert!(!empty.is_null());
        unsafe { free(&allocator, empty, 0) };
    }
}
