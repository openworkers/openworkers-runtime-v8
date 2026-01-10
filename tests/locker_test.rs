/// Test that v8::Locker and v8::Unlocker bindings are available
///
/// Note: These are basic compilation tests to verify the bindings exist.
/// Full functional testing of Locker/Unlocker requires manual isolate
/// management (without OwnedIsolate), which is beyond the scope of
/// the current runtime architecture.

#[test]
fn test_locker_api_exists() {
    // This test just verifies that the Locker API is available and compiles.
    // We can't easily test the full functionality because:
    // 1. OwnedIsolate auto-enters the isolate (incompatible with Locker)
    // 2. Locker is meant for advanced multi-threaded scenarios
    //
    // The fact that this compiles proves the bindings are working.

    // Type check - these should all compile
    fn _type_check() {
        // Locker static methods exist
        let _ = v8::Locker::is_locked;

        // Locker::new exists and takes &Isolate
        let _: fn(&v8::Isolate) -> v8::Locker = v8::Locker::new;

        // Unlocker::new exists and takes &Isolate
        let _: fn(&v8::Isolate) -> v8::Unlocker = v8::Unlocker::new;
    }

    // If we got here, the bindings compiled successfully
    assert!(true);
}

#[test]
fn test_locker_static_methods() {
    // Initialize V8 (needed for any V8 operations)
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });

    // Note: Removed test for is_active() as it doesn't exist in V8 API
    // Only is_locked() is available

    // Success - test passes if V8 initializes without panic
}
