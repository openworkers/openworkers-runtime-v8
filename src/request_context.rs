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
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::AbortOnDropHandle;

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

    /// Event loop task handle — automatically aborted on drop
    pub event_loop_handle: AbortOnDropHandle<()>,

    /// Abort flag (set on client disconnect or explicit abort)
    pub aborted: Arc<AtomicBool>,

    /// Cancellation token for all spawned I/O tasks — automatically cancelled on drop
    _cancel_guard: DropGuard,
}

impl RequestContext {
    /// Create a new RequestContext. Takes ownership of the CancellationToken
    /// and returns a clone for the caller to pass to the event loop.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: v8::Global<v8::Context>,
        scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
        callback_rx: mpsc::UnboundedReceiver<CallbackMessage>,
        callback_notify: Arc<Notify>,
        fetch_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
        fetch_error_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
        stream_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
        next_callback_id: Rc<RefCell<CallbackId>>,
        fetch_response_tx: Rc<RefCell<Option<tokio::sync::oneshot::Sender<String>>>>,
        stream_manager: Arc<StreamManager>,
        event_loop_handle: tokio::task::JoinHandle<()>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            context,
            scheduler_tx,
            callback_rx,
            callback_notify,
            fetch_callbacks,
            fetch_error_callbacks,
            stream_callbacks,
            next_callback_id,
            fetch_response_tx,
            stream_manager,
            event_loop_handle: AbortOnDropHandle::new(event_loop_handle),
            aborted: Arc::new(AtomicBool::new(false)),
            _cancel_guard: cancel.drop_guard(),
        }
    }
}
