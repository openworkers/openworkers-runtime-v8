//! WorkerFuture - Custom Future for event loop polling
//!
//! This module provides a proper `Future` implementation that replaces the
//! previous double-loop pattern with true async polling using wakers.
//!
//! ## Before (double loop with polling)
//!
//! ```ignore
//! for _iteration in 0..max_iterations {
//!     while let Ok(msg) = callback_rx.try_recv() { ... }  // Polling!
//!     tokio::select! {
//!         _ = notify.notified() => {}
//!         _ = sleep(100ms) => {}  // Artificial timeout!
//!     }
//! }
//! ```
//!
//! ## After (proper Future with wakers)
//!
//! ```ignore
//! impl Future for WorkerFuture<'_> {
//!     fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
//!         // poll_recv uses the waker from cx - no polling!
//!         match Pin::new(&mut callback_rx).poll_recv(cx) { ... }
//!     }
//! }
//! ```

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::execution_context::ExecutionContext;
use crate::execution_helpers::{AbortConfig, EventLoopExit};
use crate::runtime::CallbackMessage;
use crate::security::{CpuEnforcer, TimeoutGuard};

/// Future that drives the worker event loop
///
/// This Future polls the callback channel and processes messages until
/// the exit condition is met or execution is terminated.
pub struct WorkerFuture<'a> {
    ctx: &'a mut ExecutionContext,
    wall_guard: &'a TimeoutGuard,
    cpu_guard: &'a Option<CpuEnforcer>,
    exit_condition: EventLoopExit,
    abort_config: Option<AbortConfig>,
    /// Timestamp when abort was signaled (for grace period)
    abort_signaled_at: Option<tokio::time::Instant>,
    /// Buffer for callbacks to process in batch
    pending_callbacks: Vec<CallbackMessage>,
}

impl<'a> WorkerFuture<'a> {
    /// Create a new WorkerFuture
    ///
    /// # Arguments
    /// * `ctx` - The execution context to drive
    /// * `wall_guard` - Wall-clock timeout guard
    /// * `cpu_guard` - CPU time guard (Linux only)
    /// * `exit_condition` - When to consider the event loop complete
    /// * `abort_config` - Optional abort handling configuration
    pub fn new(
        ctx: &'a mut ExecutionContext,
        wall_guard: &'a TimeoutGuard,
        cpu_guard: &'a Option<CpuEnforcer>,
        exit_condition: EventLoopExit,
        abort_config: Option<AbortConfig>,
    ) -> Self {
        Self {
            ctx,
            wall_guard,
            cpu_guard,
            exit_condition,
            abort_config,
            abort_signaled_at: None,
            pending_callbacks: Vec::with_capacity(16),
        }
    }
}

impl Future for WorkerFuture<'_> {
    type Output = Result<(), String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // 1. Check termination (CPU/wall-clock guards)
        if this.ctx.is_terminated(this.wall_guard, this.cpu_guard) {
            return Poll::Ready(Err("Execution terminated".to_string()));
        }

        // 2. Poll callback channel - TRUE ASYNC with waker
        //    This drains all available messages without blocking
        loop {
            match Pin::new(&mut this.ctx.callback_rx).poll_recv(cx) {
                Poll::Ready(Some(msg)) => {
                    this.pending_callbacks.push(msg);
                }
                Poll::Ready(None) => {
                    // Channel closed - event loop task ended
                    return Poll::Ready(Err("Event loop channel closed".to_string()));
                }
                Poll::Pending => break,
            }
        }

        // 3. Process all received callbacks in batch
        if !this.pending_callbacks.is_empty() {
            for msg in this.pending_callbacks.drain(..) {
                this.ctx.process_single_callback(msg);
            }
        }

        // 4. V8 platform messages + microtask checkpoint
        this.ctx.pump_and_checkpoint();

        // 5. Check exit condition with abort handling
        let should_exit = this.ctx.check_exit_with_abort(
            this.exit_condition,
            &this.abort_config,
            &mut this.abort_signaled_at,
        );

        if should_exit {
            return Poll::Ready(Ok(()));
        }

        // 6. Not done yet - the waker from cx will wake us when:
        //    - A new callback arrives (via poll_recv registration)
        Poll::Pending
    }
}
