//! Where the runtime snapshot is read from: `OW_RUNTIME_SNAPSHOT_PATH` at run
//! time wins over the path baked in at build time, which is the build
//! machine's. A path with no file there runs without a snapshot.
//!
//! The snapshot is read once per process, so this is a file of its own.

use openworkers_core::RuntimeLimits;
use openworkers_runtime_v8::LockerManagedIsolate;

#[test]
fn the_snapshot_path_can_be_set_at_run_time() {
    let missing = std::env::temp_dir().join("openworkers-runtime-v8-no-such-snapshot.bin");
    let _ = std::fs::remove_file(&missing);

    // SAFETY: nothing else runs in this process yet; the snapshot is read
    // once, on the first isolate below.
    unsafe { std::env::set_var("OW_RUNTIME_SNAPSHOT_PATH", &missing) };

    let isolate = LockerManagedIsolate::new(RuntimeLimits::default());

    assert!(
        !isolate.use_snapshot,
        "the isolate took a snapshot from somewhere else than {}",
        missing.display()
    );
}
