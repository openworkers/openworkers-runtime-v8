//! CPU time measurement for monitoring worker execution.
//!
//! This module provides utilities to measure actual CPU time consumed by a thread,
//! excluding time spent waiting for I/O, sleeping, or blocked on locks.

use std::time::Duration;

/// Get current thread's CPU time (time actually spent executing on CPU).
///
/// This excludes time spent waiting for I/O, sleeping, or blocked on locks.
/// Uses `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` on Unix.
///
/// # Platform Support
///
/// - **Unix (Linux, macOS, BSD)**: Supported via `CLOCK_THREAD_CPUTIME_ID`
/// - **Windows**: Not supported (returns None)
///
/// # Returns
///
/// CPU time as a Duration, or None if measurement failed.
#[cfg(unix)]
pub fn get_thread_cpu_time() -> Option<Duration> {
    use std::mem::MaybeUninit;

    let mut time = MaybeUninit::<libc::timespec>::uninit();

    // SAFETY: clock_gettime is safe to call with valid pointers
    let ret = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) };

    if ret == 0 {
        let time = unsafe { time.assume_init() };

        // Convert timespec to Duration
        let secs = time.tv_sec as u64;
        let nanos = time.tv_nsec as u32;

        Some(Duration::new(secs, nanos))
    } else {
        None
    }
}

#[cfg(not(unix))]
pub fn get_thread_cpu_time() -> Option<Duration> {
    // CPU time measurement not supported on this platform
    None
}

/// RAII guard that measures CPU time spent in a block of code.
///
/// # Example
///
/// ```rust,ignore
/// use openworkers_runtime_v8::security::CpuTimer;
///
/// let timer = CpuTimer::start();
/// // ... do work ...
/// let elapsed = timer.elapsed();
/// println!("CPU time: {:?}", elapsed);
/// ```
pub struct CpuTimer {
    start: Duration,
}

impl CpuTimer {
    /// Start measuring CPU time.
    pub fn start() -> Self {
        Self {
            start: get_thread_cpu_time().unwrap_or_default(),
        }
    }

    /// Get elapsed CPU time since timer was started.
    pub fn elapsed(&self) -> Duration {
        get_thread_cpu_time()
            .unwrap_or(self.start)
            .saturating_sub(self.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_get_thread_cpu_time() {
        let time = get_thread_cpu_time();
        assert!(time.is_some(), "Should be able to get CPU time");

        let time = time.unwrap();
        assert!(time.as_nanos() > 0, "CPU time should be non-zero");
    }

    #[test]
    fn test_cpu_timer_measures_computation() {
        let timer = CpuTimer::start();

        // Do some CPU-intensive work
        let mut sum = 0u64;
        for i in 0..1_000_000 {
            sum = sum.wrapping_add(i);
        }

        let elapsed = timer.elapsed();

        // Prevent optimization
        assert!(sum > 0);

        // Should have measured some CPU time
        assert!(
            elapsed.as_micros() > 0,
            "Should measure CPU time for computation"
        );
    }

    #[test]
    fn test_cpu_timer_ignores_sleep() {
        let timer = CpuTimer::start();

        // Sleep doesn't consume CPU time
        thread::sleep(Duration::from_millis(10));

        let elapsed = timer.elapsed();

        // CPU time should be very small (< 1ms) despite 10ms sleep
        assert!(
            elapsed.as_millis() < 5,
            "Sleep should not count as CPU time, got {:?}",
            elapsed
        );
    }
}
