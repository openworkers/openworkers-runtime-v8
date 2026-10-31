//! The CPU time limit of one request.
//!
//! A request runs its JS in turns (see `Turn`): requests that share an
//! isolate, or a thread, run between its turns. The enforcer reads the
//! thread's CPU clock (`CLOCK_THREAD_CPUTIME_ID`) at the start and end of
//! each turn and takes the difference from the request's budget, so what
//! the others spend is not counted. A budget spent at the end of a turn
//! marks the request terminated, and it stops before its next turn.
//!
//! On Linux, a POSIX timer on the thread's CPU clock is armed with what is
//! left of the budget for the length of each turn. A turn that never yields
//! (a loop with no await) is then cut while it runs: the timer sends
//! SIGALRM, and a signal thread terminates execution through the turn.
//!
//! On macOS, a watchdog thread reads the CPU time of each thread that runs a
//! turn (`thread_info`) every 5 ms, and terminates execution through the turn
//! when the budget is spent (see `macos`). A turn overshoots by one tick at
//! most. Code that does not return to V8 (a Rust function that never ends) is
//! cut when it returns.
//!
//! Elsewhere such a turn runs until the wall clock stops it.
//!
//! Wall-clock time alone does not catch a worker that computes without
//! waiting on I/O: crypto mining, an expensive regex, a tight loop.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{Turn, get_thread_cpu_time};

pub struct CpuEnforcer {
    budget: Duration,
    remaining: Cell<Duration>,
    turn_started: Cell<Option<Duration>>,
    terminated: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    timer: linux::Timer,
    #[cfg(target_os = "macos")]
    target: macos::Target,
}

impl CpuEnforcer {
    /// The CPU budget of `request` on the isolate of `turn`. None when
    /// `timeout_ms` is 0, or when the platform has no thread CPU clock.
    pub fn new(turn: Arc<Turn>, request: u64, timeout_ms: u64) -> Option<Self> {
        if timeout_ms == 0 {
            return None;
        }

        get_thread_cpu_time()?;

        let terminated = Arc::new(AtomicBool::new(false));

        #[cfg(target_os = "linux")]
        let timer = linux::Timer::new(turn, request, Arc::clone(&terminated))?;

        #[cfg(target_os = "macos")]
        let target = macos::Target::new(turn, request, Arc::clone(&terminated));

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = (turn, request);

        let budget = Duration::from_millis(timeout_ms);

        Some(Self {
            budget,
            remaining: Cell::new(budget),
            turn_started: Cell::new(None),
            terminated,
            #[cfg(target_os = "linux")]
            timer,
            #[cfg(target_os = "macos")]
            target,
        })
    }

    /// Starts counting a turn of the request, on the thread that runs it.
    pub fn begin_turn(&self) {
        self.turn_started.set(get_thread_cpu_time());

        #[cfg(target_os = "linux")]
        self.timer.arm(self.remaining.get());

        #[cfg(target_os = "macos")]
        if let Some(started) = self.turn_started.get() {
            macos::arm(&self.target, started, self.remaining.get());
        }
    }

    /// Takes the CPU time of the turn from the budget.
    pub fn end_turn(&self) {
        #[cfg(target_os = "linux")]
        self.timer.disarm();

        #[cfg(target_os = "macos")]
        macos::disarm();

        let (Some(started), Some(now)) = (self.turn_started.take(), get_thread_cpu_time()) else {
            return;
        };

        let left = self
            .remaining
            .get()
            .saturating_sub(now.saturating_sub(started));
        self.remaining.set(left);

        if left.is_zero() {
            self.terminated.store(true, Ordering::SeqCst);
        }
    }

    /// Gives the request its whole budget again, for a new turn of an open
    /// socket. A spent budget stays spent.
    pub fn refill(&self) {
        if !self.was_terminated() {
            self.remaining.set(self.budget);
        }
    }

    pub fn was_terminated(&self) -> bool {
        self.terminated.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Once};
    use std::time::Duration;

    use super::Turn;

    /// A one-shot POSIX timer on the CPU clock of the thread that creates it.
    pub struct Timer {
        id: libc::timer_t,
        key: usize,
    }

    impl Timer {
        pub fn new(turn: Arc<Turn>, request: u64, terminated: Arc<AtomicBool>) -> Option<Self> {
            static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
            let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);

            let mut id: libc::timer_t = std::ptr::null_mut();
            let mut sigev: libc::sigevent = unsafe { std::mem::zeroed() };
            sigev.sigev_notify = libc::SIGEV_SIGNAL;
            sigev.sigev_signo = libc::SIGALRM;
            sigev.sigev_value.sival_ptr = key as *mut libc::c_void;

            let created =
                unsafe { libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut sigev, &mut id) };

            if created != 0 {
                tracing::error!(
                    "Failed to create CPU timer: {}",
                    std::io::Error::last_os_error()
                );
                return None;
            }

            register(
                key,
                Target {
                    turn,
                    request,
                    terminated,
                },
            );

            Some(Self { id, key })
        }

        /// Fires after `after` of this thread's CPU time. A zero would disarm
        /// the timer, so a spent budget fires at once instead.
        pub fn arm(&self, after: Duration) {
            self.set(after.max(Duration::from_micros(1)));
        }

        pub fn disarm(&self) {
            self.set(Duration::ZERO);
        }

        fn set(&self, after: Duration) {
            let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
            spec.it_value.tv_sec = after.as_secs() as libc::time_t;
            spec.it_value.tv_nsec = after.subsec_nanos() as libc::c_long;

            let set = unsafe { libc::timer_settime(self.id, 0, &spec, std::ptr::null_mut()) };

            if set != 0 {
                tracing::error!(
                    "Failed to set CPU timer: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            unsafe { libc::timer_delete(self.id) };
            unregister(self.key);
        }
    }

    /// What a timer stops when it fires.
    #[derive(Clone)]
    struct Target {
        turn: Arc<Turn>,
        request: u64,
        terminated: Arc<AtomicBool>,
    }

    static TARGETS: std::sync::LazyLock<Mutex<HashMap<usize, Target>>> =
        std::sync::LazyLock::new(|| {
            spawn_signal_thread();
            Mutex::new(HashMap::new())
        });

    fn register(key: usize, target: Target) {
        TARGETS.lock().unwrap().insert(key, target);
    }

    fn unregister(key: usize) {
        TARGETS.lock().unwrap().remove(&key);
    }

    fn spawn_signal_thread() {
        static SPAWNED: Once = Once::new();

        SPAWNED.call_once(|| {
            std::thread::Builder::new()
                .name("cpu-enforcer".into())
                .spawn(signal_thread)
                .expect("Failed to spawn CPU enforcer signal thread");
        });
    }

    /// Receives SIGALRM off the signal handler, which only records it, and
    /// terminates the request whose timer sent it.
    fn signal_thread() {
        use futures::StreamExt;
        use signal_hook::consts::signal;
        use signal_hook::iterator::exfiltrator::raw::WithRawSiginfo;
        use signal_hook_tokio::SignalsInfo;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for CPU enforcer signal thread");

        rt.block_on(async {
            let mut signals = SignalsInfo::with_exfiltrator([signal::SIGALRM], WithRawSiginfo)
                .expect("Failed to register SIGALRM handler");

            while let Some(siginfo) = signals.next().await {
                let key = unsafe { siginfo.si_value().sival_ptr as usize };
                let target = TARGETS.lock().unwrap().get(&key).cloned();

                // A timer dropped after it fired leaves no target
                let Some(target) = target else {
                    continue;
                };

                if !target.terminated.swap(true, Ordering::SeqCst) {
                    tracing::warn!("CPU time limit exceeded for request #{}", target.request);
                    target.turn.terminate(target.request);
                }
            }
        });
    }
}

/// The CPU limit on macOS: a watchdog thread reads the CPU time of the threads
/// that run a turn, and terminates the turn of a request that spent its budget.
///
/// Each thread that runs a turn has one slot, made on its first turn and
/// dropped with the thread. A turn arms the slot with its start, its budget
/// and its target, and disarms it at its end. The watchdog holds the lock of a
/// slot while it reads the CPU time and terminates, so it never mixes two
/// turns.
///
/// Failures fail closed: when the watchdog thread cannot start or has ended,
/// a new request starts terminated. When `thread_info` fails for a slot, the
/// watchdog logs it once with its code and counts it; that turn is not cut,
/// and the end-of-turn accounting still ends the request at its next turn.
#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
    use std::time::Duration;

    use super::Turn;

    /// The time between two checks, and so the overshoot of a turn at most.
    const TICK: Duration = Duration::from_millis(5);

    /// Ticks with nothing armed before the watchdog parks.
    const IDLE_TICKS: u32 = 100;

    unsafe extern "C" {
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }

    /// What a turn stops when it spends its budget.
    #[derive(Clone)]
    pub struct Target {
        turn: Arc<Turn>,
        request: u64,
        terminated: Arc<AtomicBool>,
    }

    impl Target {
        /// A request whose watchdog is gone starts terminated: no turn of it
        /// would be cut.
        pub fn new(turn: Arc<Turn>, request: u64, terminated: Arc<AtomicBool>) -> Self {
            #[cfg(test)]
            let dead = DEAD.with(std::cell::Cell::get);
            #[cfg(not(test))]
            let dead = false;

            // Fail closed: a request that starts terminated runs no JavaScript
            if dead || !watchdog().alive.load(Ordering::SeqCst) {
                tracing::error!("CPU watchdog is not running: request #{request} stops");
                terminated.store(true, Ordering::SeqCst);
            }

            Self {
                turn,
                request,
                terminated,
            }
        }
    }

    #[cfg(test)]
    thread_local! {
        /// Makes this thread act as if the watchdog were gone.
        pub static DEAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    struct Armed {
        /// CPU time of the thread at the start of the turn, in microseconds
        start: u64,
        budget: u64,
        target: Target,
    }

    struct Slot {
        port: libc::mach_port_t,
        armed: Mutex<Option<Armed>>,
        errors: AtomicU64,
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            // SAFETY: the port is the send right that mach_thread_self gave
            // this slot; nothing uses it after the slot drops
            #[allow(deprecated)]
            unsafe {
                mach_port_deallocate(libc::mach_task_self(), self.port)
            };
        }
    }

    struct Watchdog {
        slots: Mutex<Vec<Weak<Slot>>>,
        parked: AtomicBool,
        wake: Condvar,
        wake_lock: Mutex<()>,
        alive: AtomicBool,
    }

    fn watchdog() -> &'static Watchdog {
        static WATCHDOG: OnceLock<Watchdog> = OnceLock::new();

        WATCHDOG.get_or_init(|| {
            let started = std::thread::Builder::new()
                .name("cpu-watchdog".into())
                .spawn(run);

            if let Err(error) = &started {
                tracing::error!("CPU watchdog thread failed to start: {error}");
            }

            Watchdog {
                slots: Mutex::new(Vec::new()),
                parked: AtomicBool::new(false),
                wake: Condvar::new(),
                wake_lock: Mutex::new(()),
                alive: AtomicBool::new(started.is_ok()),
            }
        })
    }

    thread_local! {
        static SLOT: RefCell<Option<Arc<Slot>>> = const { RefCell::new(None) };
    }

    /// The slot of this thread, made and registered on first use.
    fn slot() -> Arc<Slot> {
        SLOT.with(|slot| {
            slot.borrow_mut()
                .get_or_insert_with(|| {
                    // SAFETY: mach_thread_self has no precondition; the slot
                    // keeps the send right and deallocates it when it drops
                    #[allow(deprecated)]
                    let port = unsafe { libc::mach_thread_self() };
                    let slot = Arc::new(Slot {
                        port,
                        armed: Mutex::new(None),
                        errors: AtomicU64::new(0),
                    });

                    if let Ok(mut slots) = watchdog().slots.lock() {
                        slots.push(Arc::downgrade(&slot));
                    }

                    slot
                })
                .clone()
        })
    }

    /// Arms the slot of this thread for a turn that started at `start`, the CPU
    /// time of the thread from `get_thread_cpu_time` (clock_gettime), with
    /// `budget` left. The watchdog compares it with `thread_info`; the two
    /// agree within some microseconds.
    pub fn arm(target: &Target, start: Duration, budget: Duration) {
        let slot = slot();

        if let Ok(mut armed) = slot.armed.lock() {
            *armed = Some(Armed {
                start: start.as_micros() as u64,
                budget: budget.as_micros() as u64,
                target: target.clone(),
            });
        }

        let watchdog = watchdog();

        if watchdog.parked.load(Ordering::SeqCst) {
            let _guard = watchdog.wake_lock.lock();

            watchdog.wake.notify_one();
        }
    }

    pub fn disarm() {
        SLOT.with(|slot| {
            if let Some(slot) = slot.borrow().as_ref()
                && let Ok(mut armed) = slot.armed.lock()
            {
                *armed = None;
            }
        });
    }

    /// The user and system CPU time of a thread, in microseconds.
    #[cfg(test)]
    fn cpu_time(port: libc::mach_port_t) -> Option<u64> {
        cpu_time_or_code(port).ok()
    }

    fn cpu_time_or_code(port: libc::mach_port_t) -> Result<u64, libc::kern_return_t> {
        // SAFETY: thread_basic_info is plain integers, valid when all zero
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::THREAD_BASIC_INFO_COUNT;

        // SAFETY: info is a thread_basic_info and count its size in integer_t
        let code = unsafe {
            libc::thread_info(
                port,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut info as *mut _ as libc::thread_info_t,
                &mut count,
            )
        };

        if code != libc::KERN_SUCCESS {
            return Err(code);
        }

        let micros = |time: libc::time_value_t| {
            time.seconds.max(0) as u64 * 1_000_000 + time.microseconds.max(0) as u64
        };

        Ok(micros(info.user_time) + micros(info.system_time))
    }

    /// Checks one slot: terminates its turn when the budget is spent. Answers
    /// whether the slot is armed.
    fn check(slot: &Slot) -> bool {
        let Ok(armed) = slot.armed.lock() else {
            return false;
        };
        let Some(armed) = armed.as_ref() else {
            return false;
        };

        match cpu_time_or_code(slot.port) {
            Ok(now) => {
                if now.saturating_sub(armed.start) >= armed.budget
                    && !armed.target.terminated.swap(true, Ordering::SeqCst)
                {
                    tracing::warn!(
                        "CPU time limit exceeded for request #{}",
                        armed.target.request
                    );
                    armed.target.turn.terminate(armed.target.request);
                }
            }
            Err(code) => {
                if slot.errors.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::error!(
                        "CPU watchdog: thread_info failed with {code}; this turn is not cut"
                    );
                }
            }
        }

        true
    }

    fn run() {
        let ended = std::panic::catch_unwind(|| {
            let watchdog = watchdog();
            let mut idle = 0;

            loop {
                std::thread::sleep(TICK);

                let slots: Vec<Arc<Slot>> = match watchdog.slots.lock() {
                    Ok(mut slots) => {
                        slots.retain(|slot| slot.strong_count() > 0);
                        slots.iter().filter_map(Weak::upgrade).collect()
                    }
                    Err(_) => break,
                };

                let mut armed = false;

                for slot in &slots {
                    armed |= check(slot);
                }

                drop(slots);

                idle = if armed { 0 } else { idle + 1 };

                if idle >= IDLE_TICKS {
                    let Ok(guard) = watchdog.wake_lock.lock() else {
                        break;
                    };

                    watchdog.parked.store(true, Ordering::SeqCst);

                    // A turn armed since the last tick, or while parking
                    let any = watchdog.slots.lock().is_ok_and(|slots| {
                        slots
                            .iter()
                            .filter_map(Weak::upgrade)
                            .any(|slot| slot.armed.lock().is_ok_and(|armed| armed.is_some()))
                    });

                    if !any {
                        drop(watchdog.wake.wait(guard));
                    }

                    watchdog.parked.store(false, Ordering::SeqCst);
                    idle = 0;
                }
            }
        });

        if ended.is_err() {
            tracing::error!("CPU watchdog thread panicked");
        }

        watchdog().alive.store(false, Ordering::SeqCst);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // A thread that ends leaves no live slot: its slot drops with it, and
        // its port with the slot
        #[test]
        fn a_thread_that_ends_leaves_no_slot() {
            let slot = std::thread::spawn(|| {
                let slot = Arc::downgrade(&slot());

                disarm();
                slot
            })
            .join()
            .unwrap();

            assert!(slot.upgrade().is_none());
        }

        // With no watchdog, the top level of the script is cut at once, so no
        // worker is made
        #[test]
        fn with_no_watchdog_no_worker_is_made() {
            use crate::Worker;
            use openworkers_core::{DefaultOps, Script, TerminationReason};

            DEAD.with(|dead| dead.set(true));

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            let start = std::time::Instant::now();
            let made = local.block_on(&runtime, async {
                let code = "while (true) {}";
                let ops: openworkers_core::OperationsHandle = Arc::new(DefaultOps);

                Worker::new_with_ops(Script::new(code), None, ops)
                    .await
                    .err()
            });

            DEAD.with(|dead| dead.set(false));

            assert!(
                matches!(made, Some(TerminationReason::CpuTimeLimit)),
                "{made:?}"
            );
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "{:?}",
                start.elapsed()
            );
        }

        // When the watchdog is gone, the next event of a live context ends
        // before any JavaScript
        #[test]
        fn with_no_watchdog_a_request_runs_no_javascript() {
            use crate::{ExecutionContext, LockerManagedIsolate};
            use openworkers_core::{
                DefaultOps, Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script,
                TerminationReason,
            };

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();
            let (answer, took) = local.block_on(&runtime, async {
                let code = "addEventListener('fetch', e => { while (true) {} });";
                let ops: openworkers_core::OperationsHandle = Arc::new(DefaultOps);
                let limits = RuntimeLimits::default();
                let isolate = Arc::new(LockerManagedIsolate::new(limits.clone()));
                let mut context = {
                    let (mut locker, _js_lock) = isolate.lock();

                    ExecutionContext::new_with_pooled_isolate(
                        &mut locker,
                        Arc::clone(&isolate),
                        isolate.use_snapshot,
                        isolate.platform,
                        limits,
                        Script::new(code),
                        ops,
                    )
                    .unwrap()
                };
                let (task, _rx) = Event::fetch(HttpRequest {
                    method: HttpMethod::Get,
                    url: "http://localhost/".to_string(),
                    headers: std::collections::HashMap::new(),
                    body: RequestBody::None,
                });

                DEAD.with(|dead| dead.set(true));

                let start = std::time::Instant::now();
                let answer = context.exec(task).await;

                DEAD.with(|dead| dead.set(false));

                (answer, start.elapsed())
            });

            assert!(
                matches!(answer, Err(TerminationReason::CpuTimeLimit)),
                "{answer:?}"
            );
            assert!(took < Duration::from_millis(100), "{took:?}");
        }

        #[test]
        fn the_cpu_time_of_this_thread_grows() {
            let slot = slot();
            let first = cpu_time(slot.port).unwrap();
            let end = std::time::Instant::now() + Duration::from_millis(20);

            while std::time::Instant::now() < end {}

            assert!(cpu_time(slot.port).unwrap() >= first + 10_000);
        }
    }
}
