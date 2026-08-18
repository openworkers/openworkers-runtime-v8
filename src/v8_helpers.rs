//! V8 helper functions for cross-feature compatibility.
//!
//! This module provides helper functions that abstract over V8 API differences
//! between sandbox and non-sandbox modes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use openworkers_core::RuntimeLimits;

#[cfg(not(feature = "sandbox"))]
use crate::security::CustomAllocator;

/// V8 rounds a semi-space down to a power of two, so this buys 8 MB semi-spaces.
/// Deriving it from `heap_max_mb` instead leaves 4 MB and costs GC throughput on
/// allocation-heavy handlers.
const MAX_YOUNG_GENERATION: usize = 24 * 1024 * 1024;

/// Isolate creation parameters for a worker heap.
///
/// `heap_max_mb` does not bound the JS heap: `platform.rs` sets a process-wide
/// `--max-old-space-size` that overrides it, so it only caps ArrayBuffers and
/// feeds the near-heap-limit callback.
pub fn worker_create_params(
    limits: &RuntimeLimits,
    memory_limit_hit: &Arc<AtomicBool>,
) -> v8::CreateParams {
    #[cfg(feature = "sandbox")]
    let _ = memory_limit_hit;

    let heap_max = limits.heap_max_mb * 1024 * 1024;

    let params = v8::CreateParams::default()
        .heap_limits(limits.heap_initial_mb * 1024 * 1024, heap_max)
        // Never hand a worker more young space than its whole declared heap.
        .set_max_young_generation_size_in_bytes(MAX_YOUNG_GENERATION.min(heap_max))
        .allow_atomics_wait(false);

    // ArrayBuffers live outside the V8 heap, so the cap needs its own allocator.
    #[cfg(not(feature = "sandbox"))]
    let params = params.array_buffer_allocator(
        CustomAllocator::new(heap_max, Arc::clone(memory_limit_hit)).into_v8_allocator(),
    );

    params
}

/// Creates a V8 ArrayBuffer from a Vec<u8>.
///
/// In sandbox mode, V8 must allocate memory itself (security restriction),
/// so we create an ArrayBuffer and copy data into it.
///
/// In non-sandbox mode, we can create a backing store directly from Rust memory
/// for zero-copy performance.
pub fn create_array_buffer_from_vec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: Vec<u8>,
) -> v8::Local<'s, v8::ArrayBuffer> {
    if data.is_empty() {
        return v8::ArrayBuffer::new(scope, 0);
    }

    #[cfg(feature = "sandbox")]
    {
        let len = data.len();
        let ab = v8::ArrayBuffer::new(scope, len);
        let bs = ab.get_backing_store();
        if let Some(ptr) = bs.data() {
            // SAFETY: We just created this ArrayBuffer, so we have exclusive access.
            // The backing store data is valid for the lifetime of the ArrayBuffer.
            let dest = ptr.as_ptr() as *mut u8;
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), dest, len);
            }
        }
        ab
    }

    #[cfg(not(feature = "sandbox"))]
    {
        let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(data).make_shared();
        v8::ArrayBuffer::with_backing_store(scope, &backing_store)
    }
}
