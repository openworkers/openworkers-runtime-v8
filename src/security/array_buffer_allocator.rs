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

/// Called by V8 when JS code does `new ArrayBuffer(n)` or `new Uint8Array(n)`.
/// Returns a pointer to zeroed memory, or NULL if the limit is exceeded.
#[allow(clippy::unnecessary_cast)]
unsafe extern "C" fn allocate(allocator: &CustomAllocator, n: usize) -> *mut c_void {
    // Optimistically add n bytes to our running total (atomic, thread-safe)
    allocator.count.fetch_add(n, Ordering::SeqCst);

    // Read the new total to check against the limit
    let count_loaded = allocator.count.load(Ordering::SeqCst);

    // If we've exceeded the limit, reject the allocation
    if count_loaded > allocator.max {
        eprintln!(
            "[openworkers-runtime-v8] ArrayBuffer allocation denied: {}MB exceeds limit of {}MB",
            count_loaded / 1024 / 1024,
            allocator.max / 1024 / 1024
        );
        // Set flag so the runtime knows why we failed
        allocator.memory_limit_hit.store(true, Ordering::SeqCst);
        // IMPORTANT: rollback the count since we're not actually allocating
        // Without this, failed allocations would permanently "use up" the quota
        allocator.count.fetch_sub(n, Ordering::SeqCst);
        // Return NULL - V8 will throw RangeError: Array buffer allocation failed
        return std::ptr::null::<*mut [u8]>() as *mut c_void;
    }

    // Allocate n bytes of zeroed memory:
    // 1. vec![0u8; n] creates a Vec with n zero bytes
    // 2. .into_boxed_slice() converts to Box<[u8]> (fixed size, no capacity overhead)
    // 3. Box::into_raw() gives ownership to V8 (we get a raw pointer back)
    // 4. Cast to *mut c_void for the C ABI
    Box::into_raw(vec![0u8; n].into_boxed_slice()) as *mut [u8] as *mut c_void
}

/// Called by V8 for uninitialized allocation (performance optimization).
/// Same as `allocate` but doesn't zero the memory.
#[allow(clippy::unnecessary_cast)]
#[allow(clippy::uninit_vec)]
unsafe extern "C" fn allocate_uninitialized(allocator: &CustomAllocator, n: usize) -> *mut c_void {
    allocator.count.fetch_add(n, Ordering::SeqCst);

    let count_loaded = allocator.count.load(Ordering::SeqCst);

    if count_loaded > allocator.max {
        eprintln!(
            "[openworkers-runtime-v8] ArrayBuffer allocation denied: {}MB exceeds limit of {}MB",
            count_loaded / 1024 / 1024,
            allocator.max / 1024 / 1024
        );
        allocator.memory_limit_hit.store(true, Ordering::SeqCst);
        allocator.count.fetch_sub(n, Ordering::SeqCst);
        return std::ptr::null::<*mut [u8]>() as *mut c_void;
    }

    // Allocate uninitialized memory (faster than zeroing)
    let mut store = Vec::with_capacity(n);
    // SAFETY: We just allocated capacity for n bytes, and V8 will initialize them
    unsafe { store.set_len(n) };

    Box::into_raw(store.into_boxed_slice()) as *mut [u8] as *mut c_void
}

/// Called by V8 when an ArrayBuffer is garbage collected.
/// We decrement our counter and free the memory.
unsafe extern "C" fn free(allocator: &CustomAllocator, data: *mut c_void, n: usize) {
    allocator.count.fetch_sub(n, Ordering::SeqCst);
    // SAFETY: data was allocated by allocate/allocate_uninitialized with size n
    let _ = unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(data as *mut u8, n)) };
}

/// Called when the allocator itself is dropped (isolate destroyed).
unsafe extern "C" fn drop(allocator: *const CustomAllocator) {
    // SAFETY: allocator was created via Arc::into_raw in into_v8_allocator
    let _ = unsafe { Arc::from_raw(allocator) };
}
