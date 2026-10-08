//! Global V8 platform and snapshot initialization.
//!
//! V8 can only be initialized once per process. This module provides
//! a single entry point for platform initialization used by all other modules.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::task::Waker;
use std::time::{Duration, Instant};
use v8;

static PLATFORM: OnceLock<v8::SharedRef<v8::Platform>> = OnceLock::new();
static SNAPSHOT: OnceLock<Option<&'static [u8]>> = OnceLock::new();

/// Get the global V8 platform, initializing it if necessary.
///
/// This is safe to call from multiple threads - the platform is only
/// initialized once and the same reference is returned to all callers.
pub fn get_platform() -> &'static v8::SharedRef<v8::Platform> {
    PLATFORM.get_or_init(|| {
        // Initialize ICU data BEFORE V8 initialization
        // This is required for Intl.DateTimeFormat, NumberFormat, etc.
        // Without this, V8/ICU tries to load data at runtime causing OOM
        v8::icu::set_common_data_78(crate::icudata::ICU_DATA)
            .expect("Failed to initialize ICU data");

        // Set V8 flags before initialization (following workerd's approach)
        // Disable incremental marking - better for small heaps and avoids GC bugs
        v8::V8::set_flags_from_string("--noincremental-marking");

        // On macOS, use single-threaded GC to avoid code collection issues
        // See: https://github.com/cloudflare/workers-sdk/issues/2386
        #[cfg(target_os = "macos")]
        v8::V8::set_flags_from_string("--single-threaded-gc");

        // Increase old space size to give ICU more room for caching
        // DateTimeFormat pattern generators use significant memory
        v8::V8::set_flags_from_string("--max-old-space-size=512");

        // Applied last so an operator can override any flag above. V8 flags are
        // process-wide, so no config struct can carry them.
        #[allow(clippy::disallowed_methods)]
        let flags = std::env::var("OW_V8_FLAGS");
        if let Ok(flags) = flags {
            v8::V8::set_flags_from_string(&flags);
        }

        let platform = v8::new_custom_platform(0, false, false, Foreground).make_shared();
        v8::V8::initialize_platform(platform.clone());
        v8::V8::initialize();
        platform
    })
}

/// Get the runtime snapshot, loading it once from disk.
///
/// Returns `None` if:
/// - The snapshot file doesn't exist
/// - The snapshot file is empty (allows running without snapshot)
/// - The file cannot be read
///
/// The snapshot is leaked into static memory to avoid lifetime issues.
pub fn get_snapshot() -> Option<&'static [u8]> {
    *SNAPSHOT.get_or_init(|| {
        const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");

        match std::fs::read(RUNTIME_SNAPSHOT_PATH) {
            Ok(bytes) if bytes.is_empty() => {
                tracing::warn!(
                    "Runtime snapshot file is empty: {} - running without snapshot (slower startup)",
                    RUNTIME_SNAPSHOT_PATH
                );
                None
            }
            Ok(bytes) => {
                tracing::info!(
                    "Loaded runtime snapshot ({} bytes) from {}",
                    bytes.len(),
                    RUNTIME_SNAPSHOT_PATH
                );
                Some(Box::leak(bytes.into_boxed_slice()) as &'static [u8])
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load runtime snapshot from {}: {} - running without snapshot (slower startup)",
                    RUNTIME_SNAPSHOT_PATH,
                    e
                );
                None
            }
        }
    })
}

/// The foreground tasks V8 posts for each isolate, by isolate address.
static FOREGROUND: LazyLock<Mutex<HashMap<usize, Arc<ForegroundTasks>>>> =
    LazyLock::new(Default::default);

fn isolate_key(isolate: &v8::Isolate) -> usize {
    // SAFETY: UnsafeRawIsolatePtr is a transparent wrapper around the
    // isolate's address, the value V8 passes to the platform.
    let address = unsafe {
        std::mem::transmute::<v8::UnsafeRawIsolatePtr, *mut std::ffi::c_void>(
            isolate.as_raw_isolate_ptr(),
        )
    };

    address as usize
}

fn tasks_at(key: usize) -> Arc<ForegroundTasks> {
    let mut all = FOREGROUND.lock().unwrap();

    Arc::clone(all.entry(key).or_insert_with(|| ForegroundTasks::new(key)))
}

/// The task queue of a new isolate. A dead isolate at the same address can
/// have left tasks, which V8 cancelled when it disposed of that isolate;
/// they go.
pub(crate) fn register(isolate: &v8::Isolate) -> Arc<ForegroundTasks> {
    let key = isolate_key(isolate);
    let tasks = ForegroundTasks::new(key);

    FOREGROUND.lock().unwrap().insert(key, Arc::clone(&tasks));

    tasks
}

/// Takes the foreground tasks V8 posts from any thread and wakes the event
/// loop that waits on their isolate, or a WebAssembly.compile answer waits
/// for the next timer or I/O callback.
struct Foreground;

impl v8::PlatformImpl for Foreground {
    fn post_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task) {
        tasks_at(isolate as usize).post(task, None);
    }

    fn post_non_nestable_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task) {
        tasks_at(isolate as usize).post(task, None);
    }

    fn post_delayed_task(&self, isolate: *mut std::ffi::c_void, task: v8::Task, delay: f64) {
        tasks_at(isolate as usize).post(task, Some(delay));
    }

    fn post_non_nestable_delayed_task(
        &self,
        isolate: *mut std::ffi::c_void,
        task: v8::Task,
        delay: f64,
    ) {
        tasks_at(isolate as usize).post(task, Some(delay));
    }

    // The platform has no idle task support, so V8 posts none.
    fn post_idle_task(&self, _isolate: *mut std::ffi::c_void, _task: v8::IdleTask) {}
}

/// The foreground tasks of one isolate. Only the thread that holds the
/// isolate's lock runs them.
pub(crate) struct ForegroundTasks {
    key: usize,
    queue: Mutex<Queue>,
}

#[derive(Default)]
struct Queue {
    ready: VecDeque<v8::Task>,
    delayed: Vec<(Instant, v8::Task)>,
    wakers: Vec<Waker>,
}

impl ForegroundTasks {
    fn new(key: usize) -> Arc<Self> {
        Arc::new(Self {
            key,
            queue: Mutex::default(),
        })
    }

    /// Drops the tasks of an isolate about to go, while it still exists.
    pub(crate) fn forget(self: &Arc<Self>) {
        let mut all = FOREGROUND.lock().unwrap();

        if all
            .get(&self.key)
            .is_some_and(|tasks| Arc::ptr_eq(tasks, self))
        {
            all.remove(&self.key);
        }

        drop(all);

        let mut queue = self.queue.lock().unwrap();
        queue.ready.clear();
        queue.delayed.clear();
    }

    /// A task without a delay wakes every waiter. A delayed task runs on the
    /// first `run` after its delay and wakes nobody.
    fn post(&self, task: v8::Task, delay: Option<f64>) {
        let mut queue = self.queue.lock().unwrap();

        let Some(delay) = delay else {
            queue.ready.push_back(task);
            let wakers = std::mem::take(&mut queue.wakers);
            drop(queue);

            wakers.into_iter().for_each(Waker::wake);
            return;
        };

        let due = Instant::now() + Duration::from_secs_f64(delay.max(0.0));
        queue.delayed.push((due, task));
    }

    /// A waiter for one event loop. Its waker leaves the list when the loop
    /// ends, or an isolate that gets no task keeps a waker per request.
    pub(crate) fn waiter(self: &Arc<Self>) -> Waiter {
        Waiter {
            tasks: Arc::clone(self),
            waker: None,
        }
    }

    /// Runs the tasks that are due. The caller holds the isolate's lock. A
    /// task can post another, so the queue is unlocked while one runs.
    pub(crate) fn run(&self) {
        while let Some(task) = self.next() {
            task.run();
        }
    }

    fn next(&self) -> Option<v8::Task> {
        let mut queue = self.queue.lock().unwrap();

        if let Some(task) = queue.ready.pop_front() {
            return Some(task);
        }

        let now = Instant::now();
        let due = queue.delayed.iter().position(|(at, _)| *at <= now)?;

        Some(queue.delayed.swap_remove(due).1)
    }
}

/// Wakes its event loop on the next task without a delay.
pub(crate) struct Waiter {
    tasks: Arc<ForegroundTasks>,
    waker: Option<Waker>,
}

impl Waiter {
    /// A task wakes each waker once and takes it out, so the loop registers
    /// on every poll.
    pub(crate) fn register(&mut self, waker: &Waker) {
        let mut queue = self.tasks.queue.lock().unwrap();

        if !queue.wakers.iter().any(|known| known.will_wake(waker)) {
            queue.wakers.push(waker.clone());
        }

        drop(queue);

        if !self
            .waker
            .as_ref()
            .is_some_and(|known| known.will_wake(waker))
        {
            self.waker = Some(waker.clone());
        }
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let Some(mine) = self.waker.take() else {
            return;
        };

        let mut queue = self.tasks.queue.lock().unwrap();
        queue.wakers.retain(|known| !known.will_wake(&mine));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_waiter_leaves_no_waker_behind() {
        let tasks = ForegroundTasks::new(usize::MAX);
        let waker = futures::task::noop_waker();

        let mut waiter = tasks.waiter();
        waiter.register(&waker);
        waiter.register(&waker);
        assert_eq!(tasks.queue.lock().unwrap().wakers.len(), 1);

        drop(waiter);
        assert!(tasks.queue.lock().unwrap().wakers.is_empty());
    }
}
