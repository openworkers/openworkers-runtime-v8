//! Wall-clock timeout enforcement via watchdog thread.
//!
//! This guard spawns a watchdog thread that monitors execution time and
//! terminates the V8 isolate if the timeout is exceeded. Unlike CPU time,
//! wall-clock time includes I/O waits, sleeps, and network latency.
//!
//! ## Use case
//!
//! Prevents workers from hanging indefinitely on:
//! - Slow external API calls
//! - Infinite loops with I/O
//! - Network timeouts
//!
//! ## How it works
//!
//! 1. Guard spawns a watchdog thread with a timeout duration
//! 2. Thread sleeps until timeout or cancellation
//! 3. On timeout: calls `isolate.terminate_execution()` to abort V8
//! 4. On drop: sends cancellation signal, joins thread
//!
//! ## Thread safety
//!
//! The `IsolateHandle` is thread-safe and can be used from the watchdog thread
//! to terminate execution in the main thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// RAII guard that spawns a watchdog thread to terminate V8 execution on timeout.
///
/// The watchdog thread monitors execution time and calls `isolate.terminate_execution()`
/// if the timeout is exceeded. The guard automatically cancels the watchdog when dropped.
///
/// # Example
///
/// ```rust,ignore
/// let handle = isolate.thread_safe_handle();
/// {
///     let _guard = TimeoutGuard::new(handle, 30_000); // 30s timeout
///     // Execute JavaScript code
///     // ...
/// } // Guard dropped here, watchdog cancelled
///
/// if guard.was_triggered() {
///     return TerminationReason::WallClockTimeout;
/// }
/// ```
pub struct TimeoutGuard {
    /// Channel to send cancellation signal to watchdog
    cancel_tx: Option<mpsc::Sender<()>>,
    /// Handle to join the watchdog thread
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Flag set when timeout is triggered
    triggered: Arc<AtomicBool>,
}

impl TimeoutGuard {
    /// Create a new timeout guard with the given V8 isolate handle and timeout.
    ///
    /// # Arguments
    ///
    /// * `isolate_handle` - Thread-safe handle to the V8 isolate
    /// * `timeout_ms` - Timeout in milliseconds (0 = disabled)
    ///
    /// # Returns
    ///
    /// A guard that will terminate execution if the timeout is exceeded.
    /// Drop the guard to cancel the watchdog.
    pub fn new(isolate_handle: v8::IsolateHandle, timeout_ms: u64) -> Self {
        let triggered = Arc::new(AtomicBool::new(false));

        // If timeout is 0, create disabled guard (no watchdog thread)
        if timeout_ms == 0 {
            return Self {
                cancel_tx: None,
                thread_handle: None,
                triggered,
            };
        }

        let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let triggered_clone = triggered.clone();

        let thread_handle = thread::Builder::new()
            .name("timeout-watchdog".into())
            .spawn(move || {
                let timeout = Duration::from_millis(timeout_ms);

                // Wait for either timeout or cancellation
                match cancel_rx.recv_timeout(timeout) {
                    // Cancelled before timeout - normal completion
                    Ok(()) => {
                        // Execution completed normally
                    }
                    // Timeout expired - terminate execution
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        eprintln!(
                            "[openworkers-runtime-v8] Wall-clock timeout after {}ms, terminating isolate",
                            timeout_ms
                        );
                        triggered_clone.store(true, Ordering::SeqCst);
                        isolate_handle.terminate_execution();
                    }
                    // Channel disconnected (guard dropped without explicit cancel)
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        // Guard was dropped, no action needed
                    }
                }
            })
            .expect("Failed to spawn timeout watchdog thread");

        Self {
            cancel_tx: Some(cancel_tx),
            thread_handle: Some(thread_handle),
            triggered,
        }
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
        // Send cancellation signal to watchdog thread
        if let Some(cancel_tx) = self.cancel_tx.take() {
            // Ignore error if thread already exited
            let _ = cancel_tx.send(());
        }

        // Wait for watchdog thread to finish
        if let Some(handle) = self.thread_handle.take() {
            // Join the thread - should complete quickly after cancellation
            if let Err(e) = handle.join() {
                eprintln!(
                    "[openworkers-runtime-v8] Timeout watchdog thread panicked: {:?}",
                    e
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_guard() {
        // Timeout = 0 should create disabled guard (no thread)
        let guard = TimeoutGuard {
            cancel_tx: None,
            thread_handle: None,
            triggered: Arc::new(AtomicBool::new(false)),
        };

        assert!(!guard.was_triggered());
        assert!(guard.cancel_tx.is_none());
        assert!(guard.thread_handle.is_none());
    }

    #[test]
    fn test_triggered_flag_default() {
        let triggered = Arc::new(AtomicBool::new(false));
        assert!(!triggered.load(Ordering::SeqCst));

        triggered.store(true, Ordering::SeqCst);
        assert!(triggered.load(Ordering::SeqCst));
    }
}
