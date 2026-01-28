//! V8 helper functions for cross-feature compatibility.
//!
//! This module provides helper functions that abstract over V8 API differences
//! between sandbox and non-sandbox modes.

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
