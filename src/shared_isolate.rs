//! Shared V8 Isolate that can be reused across multiple execution contexts
//!
//! This module implements the Cloudflare Workers architecture:
//! - Few reusable V8 Isolates (expensive to create, ~3-5ms, ~150MB)
//! - Many disposable Contexts (cheap to create, ~100µs, ~10KB)
//! - Each request gets a fresh Context with complete isolation

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use v8;

use crate::security::CustomAllocator;
use openworkers_core::RuntimeLimits;

/// A reusable V8 isolate that can create multiple execution contexts
///
/// This represents the V8 engine instance (heap, GC, JIT compiler) without
/// any specific execution context. It can be reused across many requests by
/// creating fresh contexts for each one.
pub struct SharedIsolate {
    pub isolate: v8::OwnedIsolate,
    pub platform: &'static v8::SharedRef<v8::Platform>,
    pub limits: RuntimeLimits,
    pub memory_limit_hit: Arc<AtomicBool>,
    /// Whether a snapshot was used for initialization
    pub use_snapshot: bool,
}

impl SharedIsolate {
    /// Create a new shared isolate
    ///
    /// This is expensive (~3-5ms) and should be done once at startup,
    /// not per-request.
    pub fn new(limits: RuntimeLimits) -> Self {
        // Initialize V8 platform (once, globally) using OnceLock for safety
        use std::sync::OnceLock;
        static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();

        let platform = PLATFORM.get_or_init(|| {
            let platform = v8::new_default_platform(0, false).make_shared();
            v8::V8::initialize_platform(platform.clone());
            v8::V8::initialize();
            platform
        });

        // Memory limit tracking for ArrayBuffer allocations
        let memory_limit_hit = Arc::new(AtomicBool::new(false));

        // Convert heap limits from MB to bytes
        let heap_initial = limits.heap_initial_mb * 1024 * 1024;
        let heap_max = limits.heap_max_mb * 1024 * 1024;

        // Create custom ArrayBuffer allocator to enforce memory limits on external memory
        // This is critical: V8 heap limits don't cover ArrayBuffers, Uint8Array, etc.
        let array_buffer_allocator = CustomAllocator::new(heap_max, Arc::clone(&memory_limit_hit));

        // Load snapshot once and cache it in static memory
        static SNAPSHOT: OnceLock<Option<&'static [u8]>> = OnceLock::new();

        let snapshot_ref = SNAPSHOT.get_or_init(|| {
            const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");
            std::fs::read(RUNTIME_SNAPSHOT_PATH)
                .ok()
                .map(|bytes| Box::leak(bytes.into_boxed_slice()) as &'static [u8])
        });

        // Create isolate with custom allocator and heap limits
        let isolate = if let Some(snapshot_data) = snapshot_ref {
            let params = v8::CreateParams::default()
                .heap_limits(heap_initial, heap_max)
                .array_buffer_allocator(array_buffer_allocator.into_v8_allocator())
                .snapshot_blob((*snapshot_data).into());
            v8::Isolate::new(params)
        } else {
            let params = v8::CreateParams::default()
                .heap_limits(heap_initial, heap_max)
                .array_buffer_allocator(array_buffer_allocator.into_v8_allocator());
            v8::Isolate::new(params)
        };

        let use_snapshot = snapshot_ref.is_some();

        Self {
            isolate,
            platform,
            limits,
            memory_limit_hit,
            use_snapshot,
        }
    }

    /// Get a thread-safe handle to this isolate for termination
    pub fn thread_safe_handle(&self) -> v8::IsolateHandle {
        self.isolate.thread_safe_handle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_isolate_creation() {
        let limits = RuntimeLimits::default();
        let isolate = SharedIsolate::new(limits);

        // Isolate should be created successfully
        assert!(!isolate.memory_limit_hit.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_multiple_shared_isolates() {
        let limits = RuntimeLimits::default();

        // Should be able to create multiple isolates
        let isolate1 = SharedIsolate::new(limits.clone());
        let isolate2 = SharedIsolate::new(limits);

        // Both should be valid
        assert!(!isolate1.memory_limit_hit.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!isolate2.memory_limit_hit.load(std::sync::atomic::Ordering::Relaxed));
    }
}
