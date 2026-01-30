# Testing Guidelines

V8 has quirks that can cause flaky tests when running in parallel. This document explains the constraints.

## The Locker Constraint

V8 has a global flag `g_locker_was_ever_used_` (in `v8threads.cc`). Once **any** thread uses `v8::Locker`, V8 expects **ALL** isolate access to use Locker.

This means:
- `LockerManagedIsolate` tests (use Locker)
- `SharedIsolate` tests (don't use Locker)
- Direct `v8::Isolate::new()` tests (don't use Locker)

**Cannot run in parallel** if they actually execute V8 code.

### Symptoms of Mixing

```
libc++ Hardening assertion __n < size() failed: vector[] index out of bounds
```

Random SIGABRT crashes in V8's bundled libc++ = Locker/non-Locker mixing.

### Solutions

1. **Tests that just create isolates** (no JS execution) are usually fine in parallel.

2. **Tests that execute JS code** must either:
   - All use the same pattern (all Locker or all non-Locker)
   - Use `#[serial]` to prevent parallel execution:
     ```rust
     use serial_test::serial;

     #[test]
     #[serial(v8)]
     fn test_shared_isolate_executes_js() { ... }
     ```

3. **Quick diagnosis** - if tests are flaky, confirm with:
   ```bash
   cargo test -- --test-threads=1
   ```
   If single-threaded passes but parallel fails, it's this issue.

## Why This Exists

From V8's source (`v8threads.cc`):

```cpp
void Locker::Initialize(v8::Isolate* isolate) {
    // Record that the Locker has been used at least once.
    base::Relaxed_Store(&g_locker_was_ever_used_, 1);
    isolate_->set_was_locker_ever_used();
}
```

This is V8's thread-safety design: once Locker is used anywhere, it's enforced everywhere.
