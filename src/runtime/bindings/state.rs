use super::super::{CallbackId, SchedulerMessage, stream_manager};
use openworkers_core::LogLevel;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;
use v8;

/// Callback for sending log messages directly (bypasses scheduler)
pub type LogCallback = Arc<dyn Fn(LogLevel, String) + Send + Sync>;

/// Shared state for timer callbacks
#[derive(Clone)]
pub struct TimerState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
}

/// State for console logging (bypasses scheduler, uses direct callback)
#[derive(Clone)]
pub struct ConsoleState {
    pub log_callback: LogCallback,
}

/// Shared state for fetch callbacks
///
/// Uses Rc<RefCell<...>> instead of Arc<Mutex<...>> because V8's Global<Function>
/// is not thread-safe, so we only access these from a single thread.
#[derive(Clone)]
pub struct FetchState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub error_callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Rc<RefCell<CallbackId>>,
}

/// Shared state for stream read callbacks (same pattern as FetchState)
#[derive(Clone)]
pub struct StreamState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Rc<RefCell<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Rc<RefCell<CallbackId>>,
}

/// State for response streaming operations
#[derive(Clone)]
pub struct ResponseStreamState {
    pub manager: Arc<stream_manager::StreamManager>,
}

/// State for performance.now() timing
#[derive(Clone)]
pub struct PerformanceState {
    pub start: std::time::Instant,
}
