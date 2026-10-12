//! Scheduler and event loop for async operations (timers, fetch, bindings).
//!
//! Uses FuturesUnordered for timers instead of spawning individual tasks,
//! similar to Deno's approach.
//!
//! All spawned I/O tasks observe a CancellationToken so they are cleaned up
//! when the request ends (timeout, response sent, or context dropped).

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::{Notify, mpsc};
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::stream_manager::{self, StreamManager};
use openworkers_core::{
    D1Error, D1ErrorKind, D1Op, D1Result, DatabaseOp, DatabaseResult, HttpRequest,
    HttpResponseMeta, KvOp, KvResult, ObjectStorageOp, ObjectStorageResult, Operation,
    OperationResult, OperationsHandle, ResponseBody, StorageOp, StorageResult, WebSocketConnection,
    WebSocketId, WebSocketIncoming, WebSocketOutgoing, WebSocketUpgrade,
};

pub type CallbackId = u64;

/// Settled in place of a real result when a request is cancelled: a promise that
/// is never settled parks the event loop for the life of the context.
const CANCELLED: &str = "Operation cancelled";

/// The end of a socket that the guest holds, which sets the close frame of a
/// cancelled request.
#[derive(Clone, Copy)]
enum End {
    /// An outgoing socket: the guest goes away, 1001.
    Client,
    /// The server end of a WebSocketPair: the worker failed, 1011.
    Server,
}

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
    BindingD1(CallbackId, String, D1Op),
    BindingWorker(CallbackId, String, HttpRequest),
    /// A request to the Durable Object of an id
    BindingDurableObject(CallbackId, String, String, HttpRequest),
    /// The storage of the Durable Object this context serves
    ObjectStorage(CallbackId, ObjectStorageOp),
    StreamRead(CallbackId, stream_manager::StreamId),
    StreamCancel(stream_manager::StreamId),
    WebSocketConnect(CallbackId, WebSocketId, String, HashMap<String, String>),
    /// The server end of a WebSocketPair, open until the response of its
    /// event gives it a socket
    WebSocketPair(WebSocketId),
    /// The response is read: the pair it carries, if any, gets the socket of
    /// the upgrade, and the other pairs close
    WebSocketUpgrade(Option<(WebSocketId, WebSocketUpgrade)>),
    WebSocketAccept(WebSocketId),
    WebSocketSend(WebSocketId, WebSocketOutgoing),
    WebSocketClose(WebSocketId, u16, String),
    /// Open a cancellation scope for a new request, optionally linked to a
    /// caller-side token such as a client disconnect.
    BeginRequest(Option<CancellationToken>),
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
    D1Result(CallbackId, D1Result),
    ObjectStorage(CallbackId, ObjectStorageResult),
    WebSocketConnected(CallbackId, WebSocketId),
    WebSocketConnectError(CallbackId, String),
    WebSocketEvent(WebSocketId, WebSocketIncoming),
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
    cancel: CancellationToken,
) {
    let callback_tx = CallbackSender::new(callback_tx, callback_notify);

    let mut timers: FuturesUnordered<Timer> = FuturesUnordered::new();
    let mut cancelled: HashSet<CallbackId> = HashSet::new();
    let mut shutdown = false;

    // Ops are cancelled per request, not per context: a cancelled child leaves the
    // event loop running for the next request on a reused context.
    let mut request_cancel = cancel.child_token();

    // WebSocket state: each WS gets a command channel, the spawned task owns the receiver
    let mut ws_commands: HashMap<WebSocketId, mpsc::UnboundedSender<WebSocketCommand>> =
        HashMap::new();

    // The storage ops of a Durable Object, which run one at a time in the
    // order the guest made them. Started on the first one.
    let mut storage: Option<mpsc::UnboundedSender<(CallbackId, ObjectStorageOp)>> = None;

    // Pairs the response of the request can still carry. None once it is read.
    let mut pairs: Option<HashSet<WebSocketId>> = Some(HashSet::new());

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
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, CANCELLED.into()));
                                    return;
                                }
                                r = execute_fetch_via_ops(request, manager, ops, cancel.clone()) => r,
                            };

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
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::FetchError(promise_id, CANCELLED.into()));
                                    return;
                                }
                                r = execute_binding_fetch_via_ops(binding_name, request, manager, ops, cancel.clone()) => r,
                            };

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
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::StorageResult(callback_id, StorageResult::Error(CANCELLED.into())));
                                    return;
                                }
                                r = ops.handle(Operation::BindingStorage {
                                    binding: binding_name,
                                    op: storage_op,
                                }) => r,
                            };

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
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::KvResult(callback_id, KvResult::Error(CANCELLED.into())));
                                    return;
                                }
                                r = ops.handle(Operation::BindingKv {
                                    binding: binding_name,
                                    op: kv_op,
                                }) => r,
                            };

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
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::DatabaseResult(callback_id, DatabaseResult::Error(CANCELLED.into())));
                                    return;
                                }
                                r = ops.handle(Operation::BindingDatabase {
                                    binding: binding_name,
                                    op: database_op,
                                }) => r,
                            };

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

                    SchedulerMessage::BindingD1(callback_id, binding_name, d1_op) => {
                        let callback_tx = callback_tx.clone();
                        let ops = ops.clone();
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let cancelled = D1Result::Error(D1Error {
                                        kind: D1ErrorKind::Interrupted,
                                        message: CANCELLED.into(),
                                        statement: None,
                                    });

                                    let _ = callback_tx.send(CallbackMessage::D1Result(callback_id, cancelled));
                                    return;
                                }
                                r = ops.handle(Operation::BindingD1 {
                                    binding: binding_name,
                                    op: d1_op,
                                }) => r,
                            };

                            let d1_result = match result {
                                OperationResult::D1(r) => r,
                                _ => D1Result::Error(D1Error {
                                    kind: D1ErrorKind::Unavailable,
                                    message: "Unexpected result type".into(),
                                    statement: None,
                                }),
                            };

                            let _ = callback_tx.send(CallbackMessage::D1Result(callback_id, d1_result));
                        });
                    }

                    SchedulerMessage::BindingWorker(callback_id, binding, request) => {
                        tokio::spawn(answer_binding_request(
                            Operation::BindingWorker { binding, request },
                            callback_id,
                            callback_tx.clone(),
                            stream_manager.clone(),
                            ops.clone(),
                            request_cancel.clone(),
                        ));
                    }

                    SchedulerMessage::BindingDurableObject(callback_id, binding, id, request) => {
                        tokio::spawn(answer_binding_request(
                            Operation::BindingDurableObject { binding, id, request },
                            callback_id,
                            callback_tx.clone(),
                            stream_manager.clone(),
                            ops.clone(),
                            request_cancel.clone(),
                        ));
                    }

                    SchedulerMessage::ObjectStorage(callback_id, op) => {
                        let storage = storage.get_or_insert_with(|| {
                            let (tx, rx) = mpsc::unbounded_channel();
                            tokio::spawn(run_object_storage(rx, callback_tx.clone(), ops.clone()));
                            tx
                        });

                        let _ = storage.send((callback_id, op));
                    }

                    SchedulerMessage::WebSocketConnect(callback_id, ws_id, url, headers) => {
                        // Create command channel BEFORE spawning so we can store the
                        // sender synchronously in the event loop's state.
                        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
                        ws_commands.insert(ws_id, cmd_tx);

                        let callback_tx = callback_tx.clone();
                        let ops = ops.clone();
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            run_websocket(
                                ws_id, callback_id, url, headers,
                                cmd_rx, callback_tx, ops, cancel,
                            ).await;
                        });
                    }

                    SchedulerMessage::WebSocketPair(ws_id) => {
                        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

                        // A pair made after the response never gets a socket, so
                        // it closes at once: its commands go nowhere.
                        if let Some(pairs) = pairs.as_mut() {
                            pairs.insert(ws_id);
                            ws_commands.insert(ws_id, cmd_tx);
                        }

                        let callback_tx = callback_tx.clone();
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            run_server_websocket(ws_id, cmd_rx, callback_tx, cancel).await;
                        });
                    }

                    SchedulerMessage::WebSocketUpgrade(upgrade) => {
                        let mut unused = pairs.take().unwrap_or_default();

                        if let Some((ws_id, upgrade)) = upgrade
                            && unused.remove(&ws_id)
                            && let Some(tx) = ws_commands.get(&ws_id)
                        {
                            let _ = tx.send(WebSocketCommand::Attach(upgrade));
                        }

                        // Dropping the command channel closes the pair
                        for ws_id in unused {
                            ws_commands.remove(&ws_id);
                        }
                    }

                    SchedulerMessage::WebSocketAccept(ws_id) => {
                        if let Some(tx) = ws_commands.get(&ws_id) {
                            let _ = tx.send(WebSocketCommand::Accept);
                        }
                    }

                    SchedulerMessage::WebSocketSend(ws_id, msg) => {
                        if let Some(tx) = ws_commands.get(&ws_id) {
                            let _ = tx.send(WebSocketCommand::Send(msg));
                        }
                    }

                    SchedulerMessage::WebSocketClose(ws_id, code, reason) => {
                        if let Some(tx) = ws_commands.remove(&ws_id) {
                            let _ = tx.send(WebSocketCommand::Close(code, reason));
                        }
                    }

                    SchedulerMessage::StreamRead(callback_id, stream_id) => {
                        let callback_tx = callback_tx.clone();
                        let manager = stream_manager.clone();
                        let cancel = request_cancel.clone();

                        tokio::spawn(async move {
                            let chunk = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {
                                    let _ = callback_tx.send(CallbackMessage::StreamChunk(callback_id, stream_manager::StreamChunk::Error(CANCELLED.into())));
                                    return;
                                }
                                r = manager.read_chunk(stream_id) => match r {
                                    Ok(chunk) => chunk,
                                    Err(e) => stream_manager::StreamChunk::Error(e),
                                },
                            };

                            let _ = callback_tx.send(CallbackMessage::StreamChunk(callback_id, chunk));
                        });
                    }

                    SchedulerMessage::StreamCancel(stream_id) => {
                        stream_manager.close_stream(stream_id);
                    }

                    SchedulerMessage::BeginRequest(abort) => {
                        request_cancel.cancel();
                        request_cancel = cancel.child_token();
                        ws_commands.clear();
                        pairs = Some(HashSet::new());

                        if let Some(abort) = abort {
                            let scope = request_cancel.clone();

                            tokio::spawn(async move {
                                tokio::select! {
                                    _ = abort.cancelled() => scope.cancel(),
                                    _ = scope.cancelled() => {}
                                }
                            });
                        }
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

/// Runs the storage ops of a Durable Object one at a time, in order: a get
/// after a put the guest did not await reads the put. A write that is under
/// way when the context goes still ends, so the guest knows what happened.
async fn run_object_storage(
    mut ops_rx: mpsc::UnboundedReceiver<(CallbackId, ObjectStorageOp)>,
    callback_tx: CallbackSender,
    ops: OperationsHandle,
) {
    while let Some((callback_id, op)) = ops_rx.recv().await {
        let result = match ops.handle(Operation::DurableObjectStorage(op)).await {
            OperationResult::ObjectStorage(result) => result,
            _ => ObjectStorageResult::Error("Unexpected result type for object storage".into()),
        };

        let _ = callback_tx.send(CallbackMessage::ObjectStorage(callback_id, result));
    }
}

/// Runs a request through a binding that answers a Response, a worker or a
/// Durable Object, and hands the response to the guest.
async fn answer_binding_request(
    op: Operation,
    callback_id: CallbackId,
    callback_tx: CallbackSender,
    manager: Arc<StreamManager>,
    ops: OperationsHandle,
    cancel: CancellationToken,
) {
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = callback_tx.send(CallbackMessage::FetchError(callback_id, CANCELLED.into()));
            return;
        }
        r = ops.handle(op) => r,
    };

    let response = match result {
        OperationResult::Http(Ok(response)) => response,
        OperationResult::Http(Err(e)) => {
            let _ = callback_tx.send(CallbackMessage::FetchError(callback_id, e));
            return;
        }
        _ => {
            let _ = callback_tx.send(CallbackMessage::FetchError(
                callback_id,
                "Unexpected result type for a binding request".into(),
            ));
            return;
        }
    };

    let meta = HttpResponseMeta {
        status: response.status,
        status_text: String::new(),
        headers: response.headers.into_iter().collect(),
    };

    let stream_id = manager.create_stream("binding_request".to_string());

    match response.body {
        ResponseBody::None => {
            let _ = manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Bytes(bytes) => {
            let _ = manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                .await;
            let _ = manager
                .write_chunk(stream_id, stream_manager::StreamChunk::Done)
                .await;
        }
        ResponseBody::Stream(rx) => {
            spawn_stream_reader(rx, stream_id, manager.clone(), cancel.clone());
        }
    }

    let _ = callback_tx.send(CallbackMessage::FetchStreamingSuccess(
        callback_id,
        meta,
        stream_id,
    ));
}

/// Spawn a task that reads chunks from a response body stream and writes them
/// to the stream manager. Cancelled when the request's CancellationToken fires.
fn spawn_stream_reader(
    mut rx: mpsc::Receiver<Result<bytes::Bytes, String>>,
    stream_id: stream_manager::StreamId,
    manager: Arc<StreamManager>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                r = rx.recv() => r,
            };

            match chunk {
                Some(Ok(bytes)) => {
                    if manager
                        .write_chunk(stream_id, stream_manager::StreamChunk::Data(bytes))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Some(Err(e)) => {
                    let _ = manager
                        .write_chunk(stream_id, stream_manager::StreamChunk::Error(e))
                        .await;
                    return;
                }
                None => break,
            }
        }

        let _ = manager
            .write_chunk(stream_id, stream_manager::StreamChunk::Done)
            .await;
    });
}

/// Execute fetch via OperationsHandler
async fn execute_fetch_via_ops(
    request: HttpRequest,
    stream_manager: Arc<StreamManager>,
    ops: OperationsHandle,
    cancel: CancellationToken,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let result = ops.handle(Operation::Fetch(request)).await;
    convert_fetch_result_to_stream(result, stream_manager, cancel).await
}

/// Execute binding fetch via OperationsHandler
async fn execute_binding_fetch_via_ops(
    binding_name: String,
    request: HttpRequest,
    stream_manager: Arc<StreamManager>,
    ops: OperationsHandle,
    cancel: CancellationToken,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let result = ops
        .handle(Operation::BindingFetch {
            binding: binding_name,
            request,
        })
        .await;
    convert_fetch_result_to_stream(result, stream_manager, cancel).await
}

/// Convert OperationResult to (HttpResponseMeta, StreamId)
async fn convert_fetch_result_to_stream(
    result: OperationResult,
    stream_manager: Arc<StreamManager>,
    cancel: CancellationToken,
) -> Result<(HttpResponseMeta, stream_manager::StreamId), String> {
    let response = match result {
        OperationResult::Http(r) => r?,
        OperationResult::Ack => return Err("Unexpected Ack result for fetch".into()),
        OperationResult::Storage(_) => return Err("Unexpected Storage result for fetch".into()),
        OperationResult::Kv(_) => return Err("Unexpected Kv result for fetch".into()),
        OperationResult::Database(_) => return Err("Unexpected Database result for fetch".into()),
        OperationResult::D1(_) => return Err("Unexpected D1 result for fetch".into()),
        OperationResult::WebSocket(_) => return Err("Unexpected WebSocket result for fetch".into()),
        OperationResult::Images(_) => return Err("Unexpected Images result for fetch".into()),
        OperationResult::ObjectStorage(_) => {
            return Err("Unexpected ObjectStorage result for fetch".into());
        }
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
        ResponseBody::Stream(rx) => {
            spawn_stream_reader(rx, stream_id, stream_manager.clone(), cancel);
        }
    }

    Ok((meta, stream_id))
}

/// Commands sent from the event loop to a WebSocket management task.
enum WebSocketCommand {
    /// The socket of the upgrade, for the server end of a pair
    Attach(WebSocketUpgrade),
    /// Start delivering incoming messages as callbacks
    Accept,
    /// Send a message on the WebSocket
    Send(WebSocketOutgoing),
    /// Close the WebSocket
    Close(u16, String),
}

/// Manages a single WebSocket connection lifecycle.
///
/// 1. Connects via OperationsHandler
/// 2. Waits for Accept before pumping incoming messages
/// 3. Forwards Send/Close commands to the WS
/// 4. Delivers incoming messages as CallbackMessages
#[allow(clippy::too_many_arguments)]
async fn run_websocket(
    ws_id: WebSocketId,
    callback_id: CallbackId,
    url: String,
    headers: HashMap<String, String>,
    cmd_rx: mpsc::UnboundedReceiver<WebSocketCommand>,
    callback_tx: CallbackSender,
    ops: OperationsHandle,
    cancel: CancellationToken,
) {
    // Phase 1: Connect
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = callback_tx.send(CallbackMessage::WebSocketConnectError(callback_id, CANCELLED.into()));
            return;
        }
        r = ops.handle(Operation::WebSocketConnect { url, headers }) => r,
    };

    let conn = match result {
        OperationResult::WebSocket(Ok(conn)) => {
            let _ = callback_tx.send(CallbackMessage::WebSocketConnected(callback_id, ws_id));
            conn
        }
        OperationResult::WebSocket(Err(e)) => {
            let _ = callback_tx.send(CallbackMessage::WebSocketConnectError(callback_id, e));
            return;
        }
        _ => {
            let _ = callback_tx.send(CallbackMessage::WebSocketConnectError(
                callback_id,
                "Unexpected result type for WebSocket connect".into(),
            ));
            return;
        }
    };

    pump_websocket(ws_id, conn, End::Client, false, cmd_rx, callback_tx, cancel).await;
}

/// The server end of a WebSocketPair. Until the runner gives it the socket of
/// the upgrade, it keeps what the guest sends, and closes if the response
/// does not carry it or the upgrade fails.
async fn run_server_websocket(
    ws_id: WebSocketId,
    mut cmd_rx: mpsc::UnboundedReceiver<WebSocketCommand>,
    callback_tx: CallbackSender,
    cancel: CancellationToken,
) {
    let closed = |reason: &str| {
        let _ = callback_tx.send(CallbackMessage::WebSocketEvent(
            ws_id,
            WebSocketIncoming::Closed {
                code: 1006,
                reason: reason.into(),
            },
        ));
    };

    let mut upgrade: Option<WebSocketUpgrade> = None;
    let mut accepted = false;
    let mut early = Vec::new();

    let conn = loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => return closed(CANCELLED),

            conn = async { upgrade.as_mut().unwrap().await }, if upgrade.is_some() => {
                match conn {
                    Ok(conn) => break conn,
                    Err(_) => return closed("The client did not upgrade"),
                }
            }

            cmd = cmd_rx.recv() => match cmd {
                Some(WebSocketCommand::Attach(socket)) => upgrade = Some(socket),
                Some(WebSocketCommand::Accept) => accepted = true,
                Some(WebSocketCommand::Send(msg)) => early.push(msg),
                Some(WebSocketCommand::Close(code, reason)) => {
                    let _ = callback_tx.send(CallbackMessage::WebSocketEvent(
                        ws_id,
                        WebSocketIncoming::Closed { code, reason },
                    ));
                    return;
                }
                None => return closed("The response did not carry the socket"),
            },
        }
    };

    for msg in early {
        let _ = conn.send_tx.send(msg);
    }

    pump_websocket(
        ws_id,
        conn,
        End::Server,
        accepted,
        cmd_rx,
        callback_tx,
        cancel,
    )
    .await;
}

/// Moves the messages of a connected socket: the guest's commands out, and
/// the peer's messages in once the guest accepts them.
async fn pump_websocket(
    ws_id: WebSocketId,
    conn: WebSocketConnection,
    end: End,
    mut accepted: bool,
    mut cmd_rx: mpsc::UnboundedReceiver<WebSocketCommand>,
    callback_tx: CallbackSender,
    cancel: CancellationToken,
) {
    let send_tx = conn.send_tx;
    let mut recv_rx = conn.recv_rx;

    // Phase 2: Command loop — handle sends, receives, and accept
    loop {
        tokio::select! {
            biased;

            // The guest may be awaiting close; the socket is not coming back. The
            // frame cannot say 1006, which must never go on the wire; the guest
            // sees 1006, the code a client treats as a dropped connection.
            _ = cancel.cancelled() => {
                let (code, reason) = match end {
                    End::Client => (1001, CANCELLED),
                    End::Server => (1011, "The worker stopped"),
                };

                let _ = send_tx.send(WebSocketOutgoing::Close { code, reason: reason.into() });
                let _ = callback_tx.send(CallbackMessage::WebSocketEvent(
                    ws_id,
                    WebSocketIncoming::Closed { code: 1006, reason: reason.into() },
                ));
                break;
            }

            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };

                match cmd {
                    // Only the server end of a pair is attached, and only once
                    WebSocketCommand::Attach(_) => {}
                    WebSocketCommand::Accept => {
                        accepted = true;
                    }
                    WebSocketCommand::Send(msg) => {
                        let _ = send_tx.send(msg);
                    }
                    // The runner answers the peer's close frame, so the
                    // guest's close event does not wait for it
                    WebSocketCommand::Close(code, reason) => {
                        let _ = send_tx.send(WebSocketOutgoing::Close { code, reason: reason.clone() });
                        let _ = callback_tx.send(CallbackMessage::WebSocketEvent(
                            ws_id,
                            WebSocketIncoming::Closed { code, reason },
                        ));
                        break;
                    }
                }
            }

            msg = recv_rx.recv(), if accepted => {
                let incoming = msg.unwrap_or(WebSocketIncoming::Closed { code: 1006, reason: String::new() });
                let closed = matches!(incoming, WebSocketIncoming::Closed { .. });
                let _ = callback_tx.send(CallbackMessage::WebSocketEvent(ws_id, incoming));

                // The guest learns of a dropped connection as of a closed one
                if closed {
                    break;
                }
            }
        }
    }
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
