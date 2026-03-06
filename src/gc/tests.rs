//! Tests for GC tracking.

use super::*;
use crate::DeriveGcTraceable;

// Test derive macro (using crate_path for internal usage)
#[derive(DeriveGcTraceable)]
#[gc(crate_path = "crate")]
struct TestBuffer {
    #[gc(track)]
    data: Vec<u8>,
    #[gc(track)]
    name: String,
    // Not tracked (intentionally unused to test that non-tracked fields are ignored)
    #[allow(dead_code)]
    id: u64,
}

#[test]
fn test_derive_gc_traceable() {
    let buffer = TestBuffer {
        data: Vec::with_capacity(1000),
        name: String::with_capacity(100),
        id: 42,
    };

    // Should only count data (1000) + name (100), not id
    assert_eq!(buffer.external_memory_size(), 1100);
}

#[derive(DeriveGcTraceable)]
#[gc(crate_path = "crate")]
struct EmptyStruct {
    // Intentionally not tracked to test zero-size result
    #[allow(dead_code)]
    value: u64,
}

#[test]
fn test_derive_gc_traceable_no_tracked_fields() {
    let empty = EmptyStruct { value: 123 };
    assert_eq!(empty.external_memory_size(), 0);
}

#[test]
fn test_gc_traceable_vec() {
    let v: Vec<u8> = Vec::with_capacity(1000);
    assert_eq!(v.external_memory_size(), 1000);
}

#[test]
fn test_gc_traceable_string() {
    let mut s = String::with_capacity(500);
    s.push_str("hello");
    assert_eq!(s.external_memory_size(), 500);
}

#[test]
fn test_gc_traceable_option() {
    let some: Option<Vec<u8>> = Some(Vec::with_capacity(100));
    let none: Option<Vec<u8>> = None;

    assert_eq!(some.external_memory_size(), 100);
    assert_eq!(none.external_memory_size(), 0);
}

#[test]
fn test_gc_traceable_nested_vec() {
    let v: Vec<Vec<u8>> = vec![
        Vec::with_capacity(100),
        Vec::with_capacity(200),
        Vec::with_capacity(300),
    ];

    // Base capacity (3 * size_of::<Vec<u8>>()) + inner capacities (100 + 200 + 300)
    let base = v.capacity() * std::mem::size_of::<Vec<u8>>();
    let inner = 100 + 200 + 300;
    assert_eq!(v.external_memory_size(), base + inner);
}

#[test]
fn test_external_memory_guard_basic() {
    // Without a JsLock, no per-isolate accumulator is captured.
    // The guard tracks the amount locally but can't defer to any isolate.
    let guard = ExternalMemoryGuard::new(1000);
    assert_eq!(guard.amount(), 1000);
    drop(guard);
}

#[test]
fn test_external_memory_guard_adjust() {
    let mut guard = ExternalMemoryGuard::new(100);
    assert_eq!(guard.amount(), 100);

    guard.adjust(50);
    assert_eq!(guard.amount(), 150);

    guard.adjust(-30);
    assert_eq!(guard.amount(), 120);
}

#[test]
fn test_external_memory_guard_set() {
    let mut guard = ExternalMemoryGuard::new(100);
    guard.set(500);
    assert_eq!(guard.amount(), 500);
}

#[test]
fn test_tracked_wrapper() {
    let tracked = Tracked::new(Vec::<u8>::with_capacity(256));
    assert_eq!(tracked.get().capacity(), 256);
}

#[test]
fn test_tracked_update_size() {
    let mut tracked = Tracked::new(Vec::<u8>::with_capacity(100));
    let initial_capacity = tracked.get().capacity();

    // Resize the inner vec - reserve additional space
    tracked.get_mut().reserve_exact(1000);
    tracked.update_size();

    // Guard should now track the new capacity
    assert!(tracked.get().capacity() > initial_capacity);
}

// Integration test with real V8 isolate
#[cfg(test)]
mod v8_tests {
    use super::super::*;
    use std::sync::atomic::Ordering;

    fn init_v8() {
        crate::platform::get_platform();
    }

    #[test]
    fn test_js_lock_current() {
        init_v8();

        let limits = openworkers_core::RuntimeLimits::default();
        let mut isolate_wrapper = crate::LockerManagedIsolate::new(limits);

        // Before lock, try_current returns None
        assert!(JsLock::try_current().is_none());

        {
            // First acquire v8::Locker
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);

            // Then create JsLock with per-isolate pending delta
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);

            // With JsLock, try_current returns Some
            assert!(JsLock::try_current().is_some());
        }

        // After lock dropped, try_current returns None again
        assert!(JsLock::try_current().is_none());
    }

    #[test]
    fn test_external_memory_with_lock() {
        init_v8();

        let limits = openworkers_core::RuntimeLimits::default();
        let mut isolate_wrapper = crate::LockerManagedIsolate::new(limits);

        {
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);

            // Create a guard while holding the lock
            let guard = ExternalMemoryGuard::new(1_000_000);
            assert_eq!(guard.amount(), 1_000_000);

            drop(guard);
            // Memory should be decremented immediately since we have the lock
        }
    }

    #[test]
    fn test_deferred_adjustment_per_isolate() {
        init_v8();

        let limits = openworkers_core::RuntimeLimits::default();
        let mut isolate_wrapper = crate::LockerManagedIsolate::new(limits);

        // First, ensure a clean state by acquiring and releasing lock
        {
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);
        }

        // Create a guard UNDER lock, then drop WITHOUT lock
        let guard = {
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);

            // Guard is created under lock — captures per-isolate pending delta
            ExternalMemoryGuard::new(500_000)
            // lock dropped here
        };

        // Drop guard without lock — delta should go to per-isolate accumulator
        drop(guard);
        assert_eq!(
            isolate_wrapper.pending_memory_delta.load(Ordering::SeqCst),
            -500_000,
            "Deferred delta should go to per-isolate accumulator"
        );

        // Re-acquire lock — pending delta should be applied to this isolate
        {
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);

            assert_eq!(
                isolate_wrapper.pending_memory_delta.load(Ordering::SeqCst),
                0,
                "Pending delta should be applied on JsLock::new()"
            );
        }
    }

    #[test]
    fn test_two_isolates_independent_deltas() {
        init_v8();

        let limits = openworkers_core::RuntimeLimits::default();
        let mut isolate_a = crate::LockerManagedIsolate::new(limits.clone());
        let mut isolate_b = crate::LockerManagedIsolate::new(limits);

        // Create a guard under isolate A's lock
        let guard_a = {
            let mut locker = v8::Locker::new(&mut isolate_a.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_a.pending_memory_delta);
            ExternalMemoryGuard::new(1_000_000)
        };

        // Drop without lock — should go to isolate A's accumulator, NOT B's
        drop(guard_a);

        assert_eq!(
            isolate_a.pending_memory_delta.load(Ordering::SeqCst),
            -1_000_000,
            "Delta should be in isolate A"
        );

        assert_eq!(
            isolate_b.pending_memory_delta.load(Ordering::SeqCst),
            0,
            "Isolate B should be unaffected"
        );

        // Acquiring isolate B's lock should NOT apply A's delta
        {
            let mut locker = v8::Locker::new(&mut isolate_b.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_b.pending_memory_delta);
        }

        // A's delta should still be pending
        assert_eq!(
            isolate_a.pending_memory_delta.load(Ordering::SeqCst),
            -1_000_000,
            "Isolate A's delta should still be pending"
        );

        // Now acquire A's lock — delta applied
        {
            let mut locker = v8::Locker::new(&mut isolate_a.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_a.pending_memory_delta);
        }

        assert_eq!(
            isolate_a.pending_memory_delta.load(Ordering::SeqCst),
            0,
            "Isolate A's delta should now be applied"
        );
    }

    #[test]
    fn test_guard_created_without_lock_captures_on_adjust() {
        init_v8();

        let limits = openworkers_core::RuntimeLimits::default();
        let mut isolate_wrapper = crate::LockerManagedIsolate::new(limits);

        // Create guard WITHOUT lock — no per-isolate accumulator captured
        let mut guard = ExternalMemoryGuard::new(0);

        // Now adjust UNDER lock — should capture the per-isolate accumulator
        {
            let mut locker = v8::Locker::new(&mut isolate_wrapper.isolate);
            let _gc_lock = JsLock::new(&mut *locker, &isolate_wrapper.pending_memory_delta);
            guard.adjust(100_000);
        }

        // Drop without lock — should defer to the captured accumulator
        drop(guard);

        assert_eq!(
            isolate_wrapper.pending_memory_delta.load(Ordering::SeqCst),
            -100_000,
            "Guard should have captured the per-isolate accumulator during adjust()"
        );
    }
}
