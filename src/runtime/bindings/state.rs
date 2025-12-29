use super::super::{CallbackId, SchedulerMessage};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use v8;

/// Shared state for timer callbacks
#[derive(Clone)]
pub struct TimerState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
}

/// State for console logging (uses scheduler to send to ops)
#[derive(Clone)]
pub struct ConsoleState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
}

/// Shared state for fetch callbacks
#[derive(Clone)]
pub struct FetchState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub error_callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Arc<Mutex<CallbackId>>,
}

/// Shared state for stream read callbacks (same pattern as FetchState)
#[derive(Clone)]
pub struct StreamState {
    pub scheduler_tx: mpsc::UnboundedSender<SchedulerMessage>,
    pub callbacks: Arc<Mutex<HashMap<CallbackId, v8::Global<v8::Function>>>>,
    pub next_id: Arc<Mutex<CallbackId>>,
}
