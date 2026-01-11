//! Generic event loop for V8 execution contexts.
//!
//! This module provides a unified polling mechanism used by Worker,
//! ExecutionContext, and WorkerFuture. The core logic is:
//!
//! 1. Poll callback channel (with waker registration)
//! 2. Process callbacks in batch
//! 3. Pump V8 platform + microtask checkpoint

use std::pin::Pin;
use std::task::Context;
use tokio::sync::mpsc;

use crate::runtime::CallbackMessage;

/// Trait for types that can run a V8 event loop.
///
/// Implementors provide access to the callback channel and
/// methods for processing callbacks and V8 maintenance.
pub trait EventLoopRuntime {
    /// Get mutable access to the callback receiver.
    fn callback_rx_mut(&mut self) -> &mut mpsc::UnboundedReceiver<CallbackMessage>;

    /// Process a single callback message.
    fn process_callback(&mut self, msg: CallbackMessage);

    /// Pump V8 platform messages and run microtask checkpoint.
    fn pump_and_checkpoint(&mut self);
}

/// Drain and process all pending callbacks.
///
/// This is the core callback processing logic shared by Worker,
/// ExecutionContext, and WorkerFuture.
///
/// Steps:
/// 1. Poll callback channel until Pending (registers waker)
/// 2. Process all received callbacks in batch
/// 3. Pump V8 platform + microtask checkpoint
///
/// # Arguments
/// * `cx` - The async task context (provides waker)
/// * `runtime` - The runtime implementing EventLoopRuntime
/// * `pending_callbacks` - Buffer for batching callbacks (reused across polls)
///
/// # Returns
/// * `Ok(())` - Callbacks processed, continue event loop
/// * `Err(msg)` - Channel closed, event loop should exit
pub fn drain_and_process<R: EventLoopRuntime>(
    cx: &mut Context<'_>,
    runtime: &mut R,
    pending_callbacks: &mut Vec<CallbackMessage>,
) -> Result<(), String> {
    // 1. Poll callback channel - TRUE ASYNC with waker
    //    This drains all available messages without blocking
    loop {
        match Pin::new(runtime.callback_rx_mut()).poll_recv(cx) {
            std::task::Poll::Ready(Some(msg)) => {
                pending_callbacks.push(msg);
            }
            std::task::Poll::Ready(None) => {
                return Err("Event loop channel closed".to_string());
            }
            std::task::Poll::Pending => break,
        }
    }

    // 2. Process all received callbacks in batch
    for msg in pending_callbacks.drain(..) {
        runtime.process_callback(msg);
    }

    // 3. V8 platform messages + microtask checkpoint
    runtime.pump_and_checkpoint();

    Ok(())
}
