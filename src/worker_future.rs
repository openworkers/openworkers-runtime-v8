//! Custom Future implementation for V8 event loop with proper waker-based polling.

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
        use crate::event_loop::drain_and_process;

        let this = self.get_mut();

        // 1. Check termination (CPU/wall-clock guards)
        if this.ctx.is_terminated(this.wall_guard, this.cpu_guard) {
            return Poll::Ready(Err("Execution terminated".to_string()));
        }

        // 2-4. Drain callbacks, process, pump V8
        if let Err(e) = drain_and_process(cx, this.ctx, &mut this.pending_callbacks) {
            return Poll::Ready(Err(e));
        }

        // 5. Check exit condition with abort handling
        let should_exit = this.ctx.check_exit_with_abort(
            this.exit_condition,
            &this.abort_config,
            &mut this.abort_signaled_at,
        );

        if should_exit {
            return Poll::Ready(Ok(()));
        }

        // 6. Not done yet - waker registered via poll_recv
        Poll::Pending
    }
}
