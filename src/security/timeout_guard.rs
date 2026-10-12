//! Wall-clock timeout enforcement through one watchdog thread per process.
//!
//! Wall-clock time includes I/O waits, sleeps and network latency, so this
//! limit stops a worker that hangs on a slow API or loops with I/O. It is
//! also what stops JS that never yields where the CPU limit has no timer
//! (outside Linux, or with the CPU limit off).
//!
//! ## How it works
//!
//! 1. A guard puts its deadline in a process-wide map, ordered by time
//! 2. The watchdog thread sleeps until the earliest deadline, and wakes up
//!    when a guard adds an earlier one
//! 3. At a deadline: sets the flag, and terminates V8 execution if its
//!    request holds the isolate's turn (see `Turn`); a request that shares
//!    the isolate keeps running
//! 4. On drop: the guard removes its deadline. An idle context costs a map
//!    entry, not a thread.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::Duration;

use super::Turn;
use tokio::time::Instant;

/// A deadline in the watchdog map: when it lands, and a key that keeps two
/// equal instants apart.
type Key = (std::time::Instant, u64);

/// What the watchdog stops at a deadline.
struct Entry {
    turn: Arc<Turn>,
    request: u64,
    triggered: Arc<AtomicBool>,
    timeout_ms: u64,
}

struct Watchdog {
    deadlines: Mutex<BTreeMap<Key, Entry>>,
    earlier: Condvar,
}

static WATCHDOG: LazyLock<Watchdog> = LazyLock::new(|| {
    #[cfg(test)]
    SPAWNED.fetch_add(1, Ordering::SeqCst);

    thread::Builder::new()
        .name("timeout-watchdog".into())
        .spawn(watch)
        .expect("Failed to spawn timeout watchdog thread");

    Watchdog {
        deadlines: Mutex::new(BTreeMap::new()),
        earlier: Condvar::new(),
    }
});

fn watch() {
    let watchdog = &*WATCHDOG;
    let mut deadlines = watchdog.deadlines.lock().unwrap();

    loop {
        let now = std::time::Instant::now();
        let later = deadlines.split_off(&(now, u64::MAX));
        let due = std::mem::replace(&mut *deadlines, later);

        // Terminate outside the lock: a guard that drops meanwhile only
        // misses its entry, and Turn ignores a request that no longer runs
        if !due.is_empty() {
            drop(deadlines);

            for entry in due.into_values() {
                tracing::warn!("Wall-clock timeout after {}ms", entry.timeout_ms);
                entry.triggered.store(true, Ordering::SeqCst);
                entry.turn.terminate(entry.request);
            }

            deadlines = watchdog.deadlines.lock().unwrap();
            continue;
        }

        deadlines = match deadlines.keys().next() {
            Some(&(at, _)) => {
                watchdog
                    .earlier
                    .wait_timeout(deadlines, at.saturating_duration_since(now))
                    .unwrap()
                    .0
            }
            None => watchdog.earlier.wait(deadlines).unwrap(),
        };
    }
}

#[cfg(test)]
static SPAWNED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn watchdog_threads() -> usize {
    SPAWNED.load(Ordering::SeqCst)
}

/// RAII guard that terminates the V8 execution of one request at its
/// deadline. Dropping it withdraws the deadline.
pub struct TimeoutGuard {
    /// The guard's entry in the watchdog map; None when disabled
    key: Option<Key>,
    /// Flag set when timeout is triggered
    triggered: Arc<AtomicBool>,
    /// When the limit lands, for a loop to poll against
    deadline: Option<Instant>,
}

impl TimeoutGuard {
    /// A guard for `request` on the isolate of `turn`; a `timeout_ms` of 0
    /// disables it.
    pub fn new(turn: Arc<Turn>, request: u64, timeout_ms: u64) -> Self {
        static NEXT_KEY: AtomicU64 = AtomicU64::new(0);

        let triggered = Arc::new(AtomicBool::new(false));

        if timeout_ms == 0 {
            return Self {
                key: None,
                triggered,
                deadline: None,
            };
        }

        let timeout = Duration::from_millis(timeout_ms);
        let key = (
            std::time::Instant::now() + timeout,
            NEXT_KEY.fetch_add(1, Ordering::Relaxed),
        );
        let entry = Entry {
            turn,
            request,
            triggered: Arc::clone(&triggered),
            timeout_ms,
        };

        let mut deadlines = WATCHDOG.deadlines.lock().unwrap();
        deadlines.insert(key, entry);

        // The watchdog sleeps until the earliest deadline; only a new
        // earliest one needs to wake it up
        if deadlines.keys().next() == Some(&key) {
            WATCHDOG.earlier.notify_one();
        }

        Self {
            key: Some(key),
            triggered,
            deadline: Some(Instant::now() + timeout),
        }
    }

    /// When the limit lands; None for a disabled guard.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Record the limit as hit from the loop that observed it, so the reason
    /// reads wall clock whichever of the loop and the watchdog got there first.
    pub fn expire(&self) {
        self.triggered.store(true, Ordering::SeqCst);
    }

    /// Check if the timeout was triggered.
    ///
    /// Use this after execution to determine the termination reason.
    pub fn was_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }
}

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key {
            WATCHDOG.deadlines.lock().unwrap().remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locker_managed_isolate::LockerManagedIsolate;
    use openworkers_core::RuntimeLimits;

    fn isolate() -> LockerManagedIsolate {
        LockerManagedIsolate::new(RuntimeLimits::default())
    }

    /// Runs a loop that never yields for `ms` while `holder` holds the turn,
    /// and answers whether it ran to its end.
    fn busy_loop_completes(isolate: &LockerManagedIsolate, holder: u64, ms: u64) -> bool {
        let (mut locker, _js_lock) = isolate.lock();
        isolate.turn.begin(holder);

        let completed = {
            let scope = std::pin::pin!(v8::HandleScope::new(&mut *locker));
            let mut scope = scope.init();
            let context = v8::Context::new(&scope, Default::default());
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let source = format!("const end = Date.now() + {ms}; while (Date.now() < end) {{}} 1");
            let code = v8::String::new(scope, &source).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();

            script.run(scope).is_some()
        };

        isolate.turn.end();
        completed
    }

    #[test]
    fn the_watchdog_cuts_the_request_that_holds_the_turn() {
        let isolate = isolate();
        let guard = TimeoutGuard::new(Arc::clone(&isolate.turn), 7, 50);

        assert!(!busy_loop_completes(&isolate, 7, 10_000));
        assert!(guard.was_triggered());
    }

    #[test]
    fn the_watchdog_spares_another_request_on_the_isolate() {
        let isolate = isolate();
        let guard = TimeoutGuard::new(Arc::clone(&isolate.turn), 7, 50);

        assert!(busy_loop_completes(&isolate, 8, 1_000));
        assert!(guard.was_triggered());
    }

    #[test]
    fn guards_share_one_watchdog_thread() {
        let isolate = isolate();
        let guards: Vec<TimeoutGuard> = (0..50)
            .map(|request| TimeoutGuard::new(Arc::clone(&isolate.turn), request, 60_000))
            .collect();

        assert!(watchdog_threads() <= 1, "{} threads", watchdog_threads());

        drop(guards);
    }

    #[test]
    fn a_later_guard_with_an_earlier_deadline_fires_first() {
        let isolate = isolate();
        let slow = TimeoutGuard::new(Arc::clone(&isolate.turn), 1, 60_000);
        let fast = TimeoutGuard::new(Arc::clone(&isolate.turn), 2, 50);

        let started = std::time::Instant::now();

        while !fast.was_triggered() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }

        assert!(fast.was_triggered());
        assert!(!slow.was_triggered());
    }

    #[test]
    fn a_disabled_guard_never_fires() {
        let isolate = isolate();
        let guard = TimeoutGuard::new(Arc::clone(&isolate.turn), 1, 0);

        thread::sleep(Duration::from_millis(50));

        assert!(guard.deadline().is_none());
        assert!(!guard.was_triggered());
    }
}
