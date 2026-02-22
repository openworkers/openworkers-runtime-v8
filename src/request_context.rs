//! Per-request state for V8 execution contexts.
//!
//! RequestContext holds all state specific to a single HTTP request or task
//! execution. Multiple RequestContexts can share the same V8 isolate when
//! intra-isolate multiplexing is enabled.
//!
//! Binding state isolation is automatic: `store_state!`/`get_state!` macros
//! use `v8::Context::set_slot()` — each v8::Context gets its own state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::{Notify, mpsc};

use crate::runtime::stream_manager::StreamManager;
use crate::runtime::{CallbackId, CallbackMessage, SchedulerMessage};

/// Per-request execution state.
///
/// Each request gets its own v8::Context, event loop channels, and callback
/// storage. The V8 isolate and platform are shared (held by ExecutionContext).
pub struct RequestContext {
    /// V8 context for this request (isolated global scope)
    pub context: v8::Global<v8::Context>,

    /// Channel to send messages to this request's event loop
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,

    /// Channel to receive callback results from the event loop
    pub callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,

    /// Notification for callback arrival (wakes poll_fn)
    pub callback_notify: Arc<Notify>,

    /// Fetch promise callbacks (resolve side)
    pub fetch_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,

    /// Fetch promise callbacks (reject side)
    pub fetch_error_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,

    /// Stream read callbacks
    pub stream_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,

    /// Monotonic callback ID counter
    pub next_callback_id: Rc<RefCell<CallbackId>>,

    /// One-shot channel for respondWith() response
    pub fetch_response_tx: Rc<RefCell<Option<tokio::sync::oneshot::Sender<String>>>>,

    /// Stream manager for body streaming
    pub stream_manager: Arc<StreamManager>,

    /// Event loop task handle (aborted on drop)
    pub event_loop_handle: tokio::task::JoinHandle<()>,

    /// Abort flag (set on client disconnect or explicit abort)
    pub aborted: Arc<AtomicBool>,
}

impl Drop for RequestContext {
    fn drop(&mut self) {
        self.event_loop_handle.abort();
    }
}
