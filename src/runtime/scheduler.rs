//! Scheduler and event loop for async operations (timers, fetch, bindings).
//!
//! Uses FuturesUnordered for timers instead of spawning individual tasks,
//! similar to Deno's approach.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Notify, mpsc};
use tokio::time::Sleep;

use super::stream_manager::{self, StreamManager};
use openworkers_core::{
    DatabaseOp, DatabaseResult, HttpRequest, HttpResponseMeta, KvOp, KvResult, Operation,
    OperationResult, OperationsHandle, ResponseBody, StorageOp, StorageResult,
};

pub type CallbackId = u64;

/// Wrapper that sends a callback message AND notifies the exec() loop
#[derive(Clone)]
pub struct CallbackSender {
    tx: mpsc::UnboundedSender<CallbackMessage>,
    notify: Arc<Notify>,
}

impl CallbackSender {
    pub fn new(tx: mpsc::UnboundedSender<CallbackMessage>, notify: Arc<Notify>) -> Self {
        Self { tx, notify }
    }

    pub fn send(
        &self,
        msg: CallbackMessage,
    ) -> Result<(), mpsc::error::SendError<CallbackMessage>> {
        let result = self.tx.send(msg);

        if result.is_ok() {
            self.notify.notify_one();
        }

        result
    }
}

pub enum SchedulerMessage {
    ScheduleTimeout(CallbackId, u64),
    ScheduleInterval(CallbackId, u64),
    ClearTimer(CallbackId),
    FetchStreaming(CallbackId, HttpRequest),
    BindingFetch(CallbackId, String, HttpRequest),
    BindingStorage(CallbackId, String, StorageOp),
    BindingKv(CallbackId, String, KvOp),
    BindingDatabase(CallbackId, String, DatabaseOp),
    BindingWorker(CallbackId, String, HttpRequest),
    StreamRead(CallbackId, stream_manager::StreamId),
    StreamCancel(stream_manager::StreamId),
    Log(openworkers_core::LogLevel, String),
    Shutdown,
}

pub enum CallbackMessage {
    ExecuteTimeout(CallbackId),
    ExecuteInterval(CallbackId),
    FetchError(CallbackId, String),
    FetchStreamingSuccess(CallbackId, HttpResponseMeta, stream_manager::StreamId),
    StreamChunk(CallbackId, stream_manager::StreamChunk),
    StorageResult(CallbackId, StorageResult),
    KvResult(CallbackId, KvResult),
    DatabaseResult(CallbackId, DatabaseResult),
}

/// Timer entry for FuturesUnordered
struct Timer {
    id: CallbackId,
    is_interval: bool,
    interval_ms: u64,
    sleep: Pin<Box<Sleep>>,
}

impl std::future::Future for Timer {
    type Output = TimerResult;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        match self.sleep.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(TimerResult {
                id: self.id,
                is_interval: self.is_interval,
                interval_ms: self.interval_ms,
            }),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

struct TimerResult {
    id: CallbackId,
    is_interval: bool,
    interval_ms: u64,
}

fn create_timer(id: CallbackId, delay_ms: u64, is_interval: bool) -> Timer {
    Timer {
        id,
        is_interval,
        interval_ms: delay_ms,
        sleep: Box::pin(tokio::time::sleep(Duration::from_millis(delay_ms))),
    }
}

pub async fn run_event_loop(
    mut scheduler_rx: mpsc::UnboundedReceiver<SchedulerMessage>,
    callback_tx: mpsc::UnboundedSender<CallbackMessage>,
    callback_notify: Arc<Notify>,
    stream_manager: Arc<StreamManager>,
    ops: OperationsHandle,
) {
    let callback_tx = CallbackSender::new(callback_tx, callback_notify);

    let mut timers: FuturesUnordered<Timer> = FuturesUnordered::new();
    let mut cancelled: HashSet<CallbackId> = HashSet::new();
    let mut shutdown = false;

    loop {
        tokio::select! {
            biased;

            // Handle scheduler messages
            msg = scheduler_rx.recv() => {
                let Some(msg) = msg else { break };

                match msg {
                    SchedulerMessage::ScheduleTimeout(id, delay_ms) => {
                        cancelled.remove(&id);
                        timers.push(create_timer(id, delay_ms, false));
                    }

                    SchedulerMessage::ScheduleInterval(id, interval_ms) => {
                        cancelled.remove(&id);
                        timers.push(create_timer(id, interval_ms, true));
                    }

                    SchedulerMessage::ClearTimer(id) => {
                        cancelled.insert(id);
                    }

                    SchedulerMessage::FetchStreaming(promise_id, request) => {
                        let callback_tx = callback_tx.clone();
                        let manager = stream_manager.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result = execute_fetch_via_ops(request, manager, ops).await;

                            match result {
                                Ok((meta, stream_id)) => {
                                    let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                        promise_id, meta, stream_id,
                                    ));
                                }
                                Err(e) => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, e));
                                }
                            }
                        });
                    }

                    SchedulerMessage::BindingFetch(promise_id, binding_name, request) => {
                        let callback_tx = callback_tx.clone();
                        let manager = stream_manager.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result =
                                execute_binding_fetch_via_ops(binding_name, request, manager, ops).await;

                            match result {
                                Ok((meta, stream_id)) => {
                                    let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                        promise_id, meta, stream_id,
                                    ));
                                }
                                Err(e) => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, e));
                                }
                            }
                        });
                    }

                    SchedulerMessage::BindingStorage(callback_id, binding_name, storage_op) => {
                        let callback_tx = callback_tx.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result = ops
                                .handle(Operation::BindingStorage {
                                    binding: binding_name,
                                    op: storage_op,
                                })
                                .await;

                            let storage_result = match result {
                                OperationResult::Storage(r) => r,
                                _ => StorageResult::Error("Unexpected result type".into()),
                            };

                            let _ = callback_tx
                                .send(CallbackMessage::StorageResult(callback_id, storage_result));
                        });
                    }

                    SchedulerMessage::BindingKv(callback_id, binding_name, kv_op) => {
                        let callback_tx = callback_tx.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result = ops
                                .handle(Operation::BindingKv {
                                    binding: binding_name,
                                    op: kv_op,
                                })
                                .await;

                            let kv_result = match result {
                                OperationResult::Kv(r) => r,
                                _ => KvResult::Error("Unexpected result type".into()),
                            };

                            let _ = callback_tx.send(CallbackMessage::KvResult(callback_id, kv_result));
                        });
                    }

                    SchedulerMessage::BindingDatabase(callback_id, binding_name, database_op) => {
                        let callback_tx = callback_tx.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result = ops
                                .handle(Operation::BindingDatabase {
                                    binding: binding_name,
                                    op: database_op,
                                })
                                .await;

                            let database_result = match result {
                                OperationResult::Database(r) => r,
                                _ => DatabaseResult::Error("Unexpected result type".into()),
                            };

                            let _ = callback_tx.send(CallbackMessage::DatabaseResult(
                                callback_id,
                                database_result,
                            ));
                        });
                    }

                    SchedulerMessage::BindingWorker(callback_id, binding_name, request) => {
                        let callback_tx = callback_tx.clone();
                        let manager = stream_manager.clone();
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let result = ops
                                .handle(Operation::BindingWorker {
                                    binding: binding_name,
                                    request,
                                })
                                .await;

                            match result {
                                OperationResult::Http(Ok(response)) => {
                                    let meta = HttpResponseMeta {
                                        status: response.status,
                                        status_text: String::new(),
                                        headers: response.headers.into_iter().collect(),
                                    };

                                    let stream_id = manager.create_stream("worker_binding".to_string());

                                    match response.body {
                                        ResponseBody::None => {
                                            let _ = manager
                                                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                                                .await;
                                        }
                                        ResponseBody::Bytes(bytes) => {
                                            let _ = manager
                                                .write_chunk(
                                                    stream_id,
                                                    stream_manager::StreamChunk::Data(bytes),
                                                )
                                                .await;
                                            let _ = manager
                                                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                                                .await;
                                        }
                                        ResponseBody::Stream(mut rx) => {
                                            let mgr = manager.clone();

                                            tokio::spawn(async move {
                                                while let Some(result) = rx.recv().await {
                                                    match result {
                                                        Ok(bytes) => {
                                                            if mgr
                                                                .write_chunk(
                                                                    stream_id,
                                                                    stream_manager::StreamChunk::Data(
                                                                        bytes,
                                                                    ),
                                                                )
                                                                .await
                                                                .is_err()
                                                            {
                                                                break;
                                                            }
                                                        }
                                                        Err(e) => {
                                                            let _ = mgr
                                                                .write_chunk(
                                                                    stream_id,
                                                                    stream_manager::StreamChunk::Error(e),
                                                                )
                                                                .await;
                                                            return;
                                                        }
                                                    }
                                                }

                                                let _ = mgr
                                                    .write_chunk(
                                                        stream_id,
                                                        stream_manager::StreamChunk::Done,
                                                    )
                                                    .await;
                                            });
                                        }
                                    }

                                    let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
                                        callback_id,
                                        meta,
                                        stream_id,
                                    ));
                                }
                                OperationResult::Http(Err(e)) => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(callback_id, e));
                                }
                                _ => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(
                                        callback_id,
                                        "Unexpected result type for worker binding".into(),
                                    ));
                                }
                            }
                        });
                    }

                    SchedulerMessage::StreamRead(callback_id, stream_id) => {
                        let callback_tx = callback_tx.clone();
                        let manager = stream_manager.clone();

                        tokio::spawn(async move {
                            let chunk = match manager.read_chunk(stream_id).await {
                                Ok(chunk) => chunk,
                                Err(e) => stream_manager::StreamChunk::Error(e),
                            };
                            let _ = callback_tx.send(CallbackMessage::StreamChunk(callback_id, chunk));
                        });
                    }

                    SchedulerMessage::StreamCancel(stream_id) => {
                        stream_manager.close_stream(stream_id);
                    }

                    SchedulerMessage::Log(level, message) => {
                        let ops = ops.clone();

                        tokio::spawn(async move {
                            let _ = ops.handle(Operation::Log { level, message }).await;
                        });
                    }

                    SchedulerMessage::Shutdown => {
                        shutdown = true;
                    }
                }
            }

            // Handle timer completions
            Some(result) = timers.next() => {
                if cancelled.remove(&result.id) {
                    // Timer was cancelled, don't fire callback
                    continue;
                }

                if result.is_interval {
                    // Send interval callback and reschedule
                    let _ = callback_tx.send(CallbackMessage::ExecuteInterval(result.id));
                    timers.push(create_timer(result.id, result.interval_ms, true));
                } else {
                    // Send timeout callback (one-shot)
                    let _ = callback_tx.send(CallbackMessage::ExecuteTimeout(result.id));
                }
            }
        }

        // Exit after processing shutdown if no more timers pending
        if shutdown && timers.is_empty() {
            break;
        }
    }
}

/// Execute fetch via OperationsHandler
async fn execute_fetch_via_ops(
    request: HttpRequest,
    stream_manager: Arc<StreamManager>,
    ops: OperationsHandle,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let result = ops.handle(Operation::Fetch(request)).await;
    convert_fetch_result_to_stream(result, stream_manager).await
}

/// Execute binding fetch via OperationsHandler
async fn execute_binding_fetch_via_ops(
    binding_name: String,
    request: HttpRequest,
    stream_manager: Arc<StreamManager>,
    ops: OperationsHandle,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let result = ops
        .handle(Operation::BindingFetch {
            binding: binding_name,
            request,
        })
        .await;
    convert_fetch_result_to_stream(result, stream_manager).await
}

/// Convert OperationResult to (HttpResponseMeta, StreamId)
async fn convert_fetch_result_to_stream(
    result: OperationResult,
    stream_manager: Arc<StreamManager>,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let response = match result {
        OperationResult::Http(r) => r?,
        OperationResult::Ack => return Err("Unexpected Ack result for fetch".into()),
        OperationResult::Storage(_) => return Err("Unexpected Storage result for fetch".into()),
        OperationResult::Kv(_) => return Err("Unexpected Kv result for fetch".into()),
        OperationResult::Database(_) => return Err("Unexpected Database result for fetch".into()),
    };

    let meta = HttpResponseMeta {
        status: response.status,
        status_text: status_text(response.status),
        headers: response.headers.into_iter().collect(),
    };

    let stream_id = stream_manager.create_stream("ops_fetch".to_string());

    match response.body {
        ResponseBody::None => {
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Bytes(bytes) => {
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                .await;
            let _ = stream_manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Stream(mut rx) => {
            let manager = stream_manager.clone();

            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    match result {
                        Ok(bytes) => {
                            if manager
                                .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = manager
                                .write_chunk(stream_id, stream_manager::StreamChunk::Error(e))
                                .await;
                            return;
                        }
                    }
                }

                let _ = manager
                    .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                    .await;
            });
        }
    }

    Ok((meta, stream_id))
}

/// Get the status text for an HTTP status code
fn status_text(status: u16) -> String {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
    .to_string()
}
