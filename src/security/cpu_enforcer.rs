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
    remaining: Cell<Duration>,
    turn_started: Cell<Option<Duration>>,
    terminated: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    timer: linux::Timer,
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

        #[cfg(not(target_os = "linux"))]
        let _ = (turn, request);

        Some(Self {
            remaining: Cell::new(Duration::from_millis(timeout_ms)),
            turn_started: Cell::new(None),
            terminated,
            #[cfg(target_os = "linux")]
            timer,
        })
    }

    /// Starts counting a turn of the request, on the thread that runs it.
    pub fn begin_turn(&self) {
        self.turn_started.set(get_thread_cpu_time());

        #[cfg(target_os = "linux")]
        self.timer.arm(self.remaining.get());
    }

    /// Takes the CPU time of the turn from the budget.
    pub fn end_turn(&self) {
        #[cfg(target_os = "linux")]
        self.timer.disarm();

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
        /// Set after the timer is set, cleared before it is disarmed, so an
        /// armed timer that reads as expired has fired.
        armed: Arc<AtomicBool>,
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

            let armed = Arc::new(AtomicBool::new(false));

            register(
                key,
                Target {
                    turn,
                    request,
                    terminated,
                    timer: id as usize,
                    armed: Arc::clone(&armed),
                },
            );

            Some(Self { id, key, armed })
        }

        /// Fires after `after` of this thread's CPU time. A zero would disarm
        /// the timer, so a spent budget fires at once instead.
        pub fn arm(&self, after: Duration) {
            self.set(after.max(Duration::from_micros(1)));
            self.armed.store(true, Ordering::SeqCst);
        }

        pub fn disarm(&self) {
            self.armed.store(false, Ordering::SeqCst);
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
            // Under the lock, so the signal thread never reads a deleted
            // timer, whose id the next timer could reuse
            let mut targets = TARGETS.lock().unwrap();
            targets.remove(&self.key);
            unsafe { libc::timer_delete(self.id) };
        }
    }

    /// What a timer stops when it fires.
    #[derive(Clone)]
    struct Target {
        turn: Arc<Turn>,
        request: u64,
        terminated: Arc<AtomicBool>,
        /// The timer, as `libc::timer_t` is a pointer and the map is shared
        timer: usize,
        armed: Arc<AtomicBool>,
    }

    impl Target {
        /// Whether the timer has fired: armed, and nothing left on it.
        fn expired(&self) -> bool {
            if !self.armed.load(Ordering::SeqCst) {
                return false;
            }

            let mut spec: libc::itimerspec = unsafe { std::mem::zeroed() };
            let read = unsafe { libc::timer_gettime(self.timer as libc::timer_t, &mut spec) };

            read == 0 && spec.it_value.tv_sec == 0 && spec.it_value.tv_nsec == 0
        }

        fn stop(&self) {
            if !self.terminated.swap(true, Ordering::SeqCst) {
                tracing::warn!("CPU time limit exceeded for request #{}", self.request);
                self.turn.terminate(self.request);
            }
        }
    }

    /// The handler is installed before the first target registers, so a
    /// timer armed right after cannot fire into the default action, which
    /// is to kill the process.
    static TARGETS: std::sync::LazyLock<Mutex<HashMap<usize, Target>>> =
        std::sync::LazyLock::new(|| {
            spawn_signal_thread();
            Mutex::new(HashMap::new())
        });

    fn register(key: usize, target: Target) {
        TARGETS.lock().unwrap().insert(key, target);
    }

    /// The target the signal names, if it still exists, and every other
    /// whose timer has fired. SIGALRM is a standard signal: one that arrives
    /// while the handler runs for another is merged with it, and its value
    /// is lost. The timers themselves still say which ones fired.
    fn fired(key: usize) -> Vec<Target> {
        let targets = TARGETS.lock().unwrap();

        targets
            .iter()
            .filter(|(at, target)| **at == key || target.expired())
            .map(|(_, target)| target.clone())
            .collect()
    }

    /// Installs the SIGALRM handler, here and now, and starts the thread
    /// that acts on what it records.
    fn spawn_signal_thread() {
        use signal_hook::consts::signal;
        use signal_hook::iterator::SignalsInfo;
        use signal_hook::iterator::exfiltrator::raw::WithRawSiginfo;

        static SPAWNED: Once = Once::new();

        SPAWNED.call_once(|| {
            let signals = SignalsInfo::with_exfiltrator([signal::SIGALRM], WithRawSiginfo)
                .expect("Failed to register SIGALRM handler");

            std::thread::Builder::new()
                .name("cpu-enforcer".into())
                .spawn(move || signal_thread(signals))
                .expect("Failed to spawn CPU enforcer signal thread");
        });
    }

    /// Receives SIGALRM off the signal handler, which only records it, and
    /// terminates the request whose timer sent it.
    fn signal_thread(
        mut signals: signal_hook::iterator::SignalsInfo<
            signal_hook::iterator::exfiltrator::raw::WithRawSiginfo,
        >,
    ) {
        for siginfo in signals.forever() {
            let key = unsafe { siginfo.si_value().sival_ptr as usize };

            // A timer dropped after it fired leaves no target
            for target in fired(key) {
                target.stop();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Instant;

        /// A timer that fired is found by the sweep whether or not its own
        /// signal was the one delivered.
        #[test]
        fn a_fired_timer_is_found_without_its_signal() {
            crate::platform::get_platform();
            let isolate = crate::v8_helpers::new_isolate(Default::default());
            let turn = Arc::new(Turn::new(
                isolate.thread_safe_handle(),
                Arc::new(AtomicBool::new(false)),
            ));
            let terminated = Arc::new(AtomicBool::new(false));
            let timer = Timer::new(turn, 7, terminated).expect("a CPU timer");

            // Not armed: never reported, whatever the timer reads
            assert!(fired(usize::MAX).iter().all(|t| t.request != 7));

            timer.arm(Duration::from_micros(1));
            let start = Instant::now();
            let mut x = 0u64;

            while start.elapsed() < Duration::from_millis(20) {
                x = x.wrapping_mul(31).wrapping_add(1);
            }

            assert!(x != 0);
            assert!(
                fired(usize::MAX).iter().any(|t| t.request == 7),
                "the sweep did not find the fired timer"
            );

            timer.disarm();
            assert!(fired(usize::MAX).iter().all(|t| t.request != 7));
        }
    }
}
