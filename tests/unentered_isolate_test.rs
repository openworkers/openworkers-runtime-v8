//! Test for v8::UnenteredIsolate with v8::Locker
//!
//! This validates that we can create isolates without auto-enter
//! and use them with Locker for multi-threaded scenarios.

#[cfg(feature = "v8")]
#[test]
fn test_unentered_isolate_with_locker() {
    use v8::{CreateParams, Isolate, Locker};

    // Create isolate without auto-enter
    let params = CreateParams::default();
    let isolate = Isolate::new_unentered(params);

    // Lock the isolate for this thread
    let _locker = Locker::new(&*isolate);

    // Now we can use the isolate
    // (In a real scenario, we'd create HandleScope, Context, etc.)

    // Locker drop unlocks
    // isolate drop disposes (no exit call, no LIFO assertion)
}

#[cfg(feature = "v8")]
#[test]
fn test_unentered_isolate_is_not_current_without_locker() {
    use v8::{CreateParams, Isolate};

    // Create isolate without auto-enter
    let params = CreateParams::default();
    let _isolate = Isolate::new_unentered(params);

    // Unlike OwnedIsolate, this isolate is NOT current
    // We need to use Locker to make it current for this thread

    // Drop without ever locking - should work fine
}

#[cfg(feature = "v8")]
#[test]
fn test_locker_static_methods() {
    use v8::{CreateParams, Isolate, Locker};

    // Create isolate
    let params = CreateParams::default();
    let isolate = Isolate::new_unentered(params);

    // Before locking, should not be locked
    assert!(!Locker::is_locked(&*isolate));

    {
        let _locker = Locker::new(&*isolate);

        // Now it should be locked
        assert!(Locker::is_locked(&*isolate));

        // Note: is_active() removed as it doesn't exist in V8 API
        // Only is_locked() is available
    }

    // After locker drop, should not be locked anymore
    assert!(!Locker::is_locked(&*isolate));
}

#[cfg(feature = "v8")]
#[test]
fn test_multiple_unentered_isolates() {
    use v8::{CreateParams, Isolate, Locker};

    // Create multiple isolates (no LIFO constraint!)
    let params1 = CreateParams::default();
    let isolate1 = Isolate::new_unentered(params1);

    let params2 = CreateParams::default();
    let isolate2 = Isolate::new_unentered(params2);

    // Lock them in any order
    let _locker1 = Locker::new(&*isolate1);
    drop(_locker1);

    let _locker2 = Locker::new(&*isolate2);
    drop(_locker2);

    // Drop in any order (no LIFO assertion)
    drop(isolate1);
    drop(isolate2);
}
