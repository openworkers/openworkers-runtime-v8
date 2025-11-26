//! CPU time enforcement using POSIX timers and async-signal-safe communication.
//!
//! This module enforces CPU time limits on V8 execution. Unlike wall-clock time,
//! CPU time only counts actual computation - sleeps, I/O waits, and network
//! latency don't count.
//!
//! ## Platform support
//!
//! - **Linux**: Full support via `timer_create(CLOCK_THREAD_CPUTIME_ID)`
//! - **macOS/BSD**: Not supported (these platforms lack per-thread CPU timers)
//!
//! On unsupported platforms, `CpuEnforcer::new()` returns `None` and wall-clock
//! timeout should be used as fallback.
//!
//! ## Architecture (Linux)
//!
//! 1. Create per-thread POSIX timer with `timer_create(CLOCK_THREAD_CPUTIME_ID)`
//! 2. Timer fires `SIGALRM` when CPU time limit is exceeded
//! 3. Signal handler thread (spawned once globally) receives the signal
//! 4. Handler calls `isolate.terminate_execution()` to abort V8
//!
//! The signal handler itself does NO locks, NO allocations - fully async-signal-safe.
//! All complex logic happens in the dedicated signal processing thread.
//!
//! ## Why CPU time matters
//!
//! A malicious worker could:
//! - Spin in a tight loop doing crypto mining
//! - Run expensive regex operations
//! - Compute large Fibonacci numbers
//!
//! Wall-clock timeout alone won't help if the worker isn't doing I/O.
//! CPU time enforcement catches these cases.

// ============================================================================
// Linux implementation
// ============================================================================

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, Once};

    /// CPU time enforcer using POSIX timers.
    pub struct CpuEnforcer {
        timer_id: libc::timer_t,
        enforcer_id: usize,
        terminated: Arc<AtomicBool>,
    }

    impl CpuEnforcer {
        /// Create a new CPU enforcer with the given timeout in milliseconds.
        ///
        /// Returns `None` if:
        /// - `timeout_ms` is 0 (disabled)
        /// - Timer creation fails (permissions, resource limits)
        pub fn new(isolate_handle: v8::IsolateHandle, timeout_ms: u64) -> Option<Self> {
            if timeout_ms == 0 {
                return None;
            }

            // Generate unique enforcer ID
            static ENFORCER_COUNTER: AtomicUsize = AtomicUsize::new(1);
            let enforcer_id = ENFORCER_COUNTER.fetch_add(1, Ordering::Relaxed);

            // Create POSIX timer
            let mut timer_id: libc::timer_t = std::ptr::null_mut();
            let mut sigev: libc::sigevent = unsafe { std::mem::zeroed() };

            sigev.sigev_notify = libc::SIGEV_SIGNAL;
            sigev.sigev_signo = libc::SIGALRM;
            sigev.sigev_value.sival_ptr = enforcer_id as *mut libc::c_void;

            let ret = unsafe {
                libc::timer_create(libc::CLOCK_THREAD_CPUTIME_ID, &mut sigev, &mut timer_id)
            };

            if ret != 0 {
                eprintln!(
                    "[openworkers-runtime-v8] Failed to create CPU timer: {}",
                    std::io::Error::last_os_error()
                );
                return None;
            }

            let terminated = Arc::new(AtomicBool::new(false));

            // Register in global registry
            register_enforcer(enforcer_id, isolate_handle, terminated.clone());

            // Arm the timer
            let timeout_secs = timeout_ms / 1000;
            let timeout_nsecs = (timeout_ms % 1000) * 1_000_000;

            let mut timer_spec: libc::itimerspec = unsafe { std::mem::zeroed() };
            timer_spec.it_value.tv_sec = timeout_secs as i64;
            timer_spec.it_value.tv_nsec = timeout_nsecs as i64;

            let ret =
                unsafe { libc::timer_settime(timer_id, 0, &timer_spec, std::ptr::null_mut()) };

            if ret != 0 {
                eprintln!(
                    "[openworkers-runtime-v8] Failed to arm CPU timer: {}",
                    std::io::Error::last_os_error()
                );
                unsafe { libc::timer_delete(timer_id) };
                unregister_enforcer(enforcer_id);
                return None;
            }

            Some(Self {
                timer_id,
                enforcer_id,
                terminated,
            })
        }

        /// Check if the CPU limit was exceeded and termination occurred.
        pub fn was_terminated(&self) -> bool {
            self.terminated.load(Ordering::SeqCst)
        }

        /// Reset the timer for a new request (useful for worker pooling).
        ///
        /// This rearms the CPU timer with the given timeout, allowing the same
        /// worker to handle multiple requests with fresh CPU budgets.
        pub fn reset(&self, timeout_ms: u64) -> Result<(), std::io::Error> {
            // Reset terminated flag
            self.terminated.store(false, Ordering::SeqCst);

            // Rearm the timer
            let timeout_secs = timeout_ms / 1000;
            let timeout_nsecs = (timeout_ms % 1000) * 1_000_000;

            let mut timer_spec: libc::itimerspec = unsafe { std::mem::zeroed() };
            timer_spec.it_value.tv_sec = timeout_secs as i64;
            timer_spec.it_value.tv_nsec = timeout_nsecs as i64;
            // it_interval stays 0 = one-shot timer

            let ret =
                unsafe { libc::timer_settime(self.timer_id, 0, &timer_spec, std::ptr::null_mut()) };

            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        }
    }

    impl Drop for CpuEnforcer {
        fn drop(&mut self) {
            unsafe { libc::timer_delete(self.timer_id) };
            unregister_enforcer(self.enforcer_id);
        }
    }

    // Global registry for signal handler
    struct EnforcerData {
        isolate_handle: v8::IsolateHandle,
        terminated: Arc<AtomicBool>,
    }

    impl Clone for EnforcerData {
        fn clone(&self) -> Self {
            Self {
                isolate_handle: self.isolate_handle.clone(),
                terminated: self.terminated.clone(),
            }
        }
    }

    static ENFORCER_REGISTRY: std::sync::LazyLock<Mutex<HashMap<usize, EnforcerData>>> =
        std::sync::LazyLock::new(|| {
            spawn_signal_handler_thread();
            Mutex::new(HashMap::new())
        });

    fn register_enforcer(
        enforcer_id: usize,
        isolate_handle: v8::IsolateHandle,
        terminated: Arc<AtomicBool>,
    ) {
        let mut map = ENFORCER_REGISTRY.lock().unwrap();
        map.insert(
            enforcer_id,
            EnforcerData {
                isolate_handle,
                terminated,
            },
        );
    }

    fn unregister_enforcer(enforcer_id: usize) {
        let mut map = ENFORCER_REGISTRY.lock().unwrap();
        map.remove(&enforcer_id);
    }

    fn spawn_signal_handler_thread() {
        static SIGNAL_THREAD_SPAWNED: Once = Once::new();

        SIGNAL_THREAD_SPAWNED.call_once(|| {
            std::thread::Builder::new()
                .name("cpu-enforcer".into())
                .spawn(signal_handler_thread)
                .expect("Failed to spawn CPU enforcer signal handler thread");
        });
    }

    fn signal_handler_thread() {
        use futures::StreamExt;
        use signal_hook::consts::signal;
        use signal_hook::iterator::exfiltrator::raw::WithRawSiginfo;
        use signal_hook_tokio::SignalsInfo;

        // Create a dedicated single-threaded tokio runtime for signal handling
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for CPU enforcer signal handler");

        rt.block_on(async {
            // Setup async-signal-safe SIGALRM handler with raw siginfo extraction
            let mut signals = SignalsInfo::with_exfiltrator([signal::SIGALRM], WithRawSiginfo)
                .expect("Failed to register SIGALRM handler");

            while let Some(siginfo) = signals.next().await {
                // Extract enforcer_id from signal value (set in timer_create)
                let enforcer_id = unsafe { siginfo.si_value().sival_ptr as usize };

                // Lookup ONLY this specific enforcer in registry
                let data = {
                    let map = ENFORCER_REGISTRY.lock().unwrap();
                    map.get(&enforcer_id).cloned()
                };

                if let Some(enforcer_data) = data {
                    if !enforcer_data.terminated.swap(true, Ordering::SeqCst) {
                        eprintln!(
                            "[openworkers-runtime-v8] CPU time limit exceeded for enforcer #{}, terminating isolate",
                            enforcer_id
                        );
                        enforcer_data.isolate_handle.terminate_execution();
                    }
                } else {
                    // Timer fired but enforcer was already dropped (normal race condition)
                    eprintln!(
                        "[openworkers-runtime-v8] SIGALRM for unknown enforcer #{} (already dropped?)",
                        enforcer_id
                    );
                }
            }
        });
    }
}

#[cfg(target_os = "linux")]
pub use linux::CpuEnforcer;

// ============================================================================
// Non-Linux stub implementation
// ============================================================================

/// CPU enforcer stub for non-Linux platforms.
///
/// On macOS/BSD, per-thread CPU timers are not available.
/// Use `TimeoutGuard` for wall-clock timeout as fallback.
#[cfg(not(target_os = "linux"))]
pub struct CpuEnforcer;

#[cfg(not(target_os = "linux"))]
impl CpuEnforcer {
    /// Always returns `None` on non-Linux platforms.
    pub fn new(_isolate_handle: v8::IsolateHandle, _timeout_ms: u64) -> Option<Self> {
        None
    }

    /// Always returns `false` on non-Linux platforms.
    pub fn was_terminated(&self) -> bool {
        false
    }

    /// No-op on non-Linux platforms.
    pub fn reset(&self, _timeout_ms: u64) -> Result<(), std::io::Error> {
        Ok(())
    }
}
