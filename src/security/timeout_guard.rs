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
//! 3. On timeout: sets the flag, and terminates V8 execution if its request
//!    holds the isolate's turn (see `Turn`); a request that shares the
//!    isolate keeps running
//! 4. On drop: sends cancellation signal, joins thread

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::Turn;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tokio::time::Instant;

/// RAII guard that spawns a watchdog thread to terminate the V8 execution of
/// one request on timeout. The guard cancels the watchdog when dropped.
pub struct TimeoutGuard {
    /// Channel to send cancellation signal to watchdog
    cancel_tx: Option<mpsc::Sender<()>>,
    /// Handle to join the watchdog thread
    thread_handle: Option<thread::JoinHandle<()>>,
    /// Flag set when timeout is triggered
    triggered: Arc<AtomicBool>,
    /// When the limit lands, for a loop to poll against
    deadline: Option<Instant>,
}

impl TimeoutGuard {
    /// A guard for `request` on the isolate of `turn`; a `timeout_ms` of 0
    /// disables it.
    pub fn new(turn: Arc<Turn>, request: u64, timeout_ms: u64) -> Self {
        let triggered = Arc::new(AtomicBool::new(false));

        // If timeout is 0, create disabled guard (no watchdog thread)
        if timeout_ms == 0 {
            return Self {
                cancel_tx: None,
                thread_handle: None,
                triggered,
                deadline: None,
            };
        }

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
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
                        tracing::warn!("Wall-clock timeout after {}ms", timeout_ms);
                        triggered_clone.store(true, Ordering::SeqCst);
                        turn.terminate(request);
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
            deadline: Some(deadline),
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
        // Send cancellation signal to watchdog thread
        if let Some(cancel_tx) = self.cancel_tx.take() {
            // Ignore error if thread already exited
            let _ = cancel_tx.send(());
        }

        // Wait for watchdog thread to finish
        if let Some(handle) = self.thread_handle.take() {
            // Join the thread - should complete quickly after cancellation
            if let Err(e) = handle.join() {
                tracing::error!("Timeout watchdog thread panicked: {:?}", e);
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
            deadline: None,
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
