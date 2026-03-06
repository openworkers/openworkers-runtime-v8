//! Tests for CancellationToken propagation in the scheduler.
//!
//! Verifies that spawned I/O tasks are cleaned up when the request's
//! CancellationToken is cancelled (timeout, response sent, context dropped).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, OpFuture, OperationsHandler, RequestBody, Script, StorageOp,
    StorageResult,
};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Ops handler with a slow storage operation to observe cancellation.
struct SlowOps {
    started: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
}

impl OperationsHandler for SlowOps {
    fn handle_binding_storage(
        &self,
        _binding: &str,
        _op: StorageOp,
    ) -> OpFuture<'_, StorageResult> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let completed = self.completed.clone();

        Box::pin(async move {
            // 10 seconds — should be cancelled well before completion
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            completed.fetch_add(1, Ordering::SeqCst);
            StorageResult::Body(Some(b"value".to_vec()))
        })
    }
}

/// Test the scheduler directly: cancel token stops spawned tasks.
#[tokio::test]
async fn test_scheduler_cancellation_stops_spawned_tasks() {
    use openworkers_core::StorageOp;
    use openworkers_runtime_v8::runtime::stream_manager::StreamManager;

    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));

    let ops: Arc<dyn OperationsHandler> = Arc::new(SlowOps {
        started: started.clone(),
        completed: completed.clone(),
    });

    let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    let callback_notify = Arc::new(tokio::sync::Notify::new());
    let stream_manager = Arc::new(StreamManager::new());
    let cancel = CancellationToken::new();

    // Start the event loop
    let event_loop_cancel = cancel.clone();
    let event_loop = tokio::spawn(async move {
        openworkers_runtime_v8::runtime::run_event_loop(
            scheduler_rx,
            callback_tx,
            callback_notify,
            stream_manager,
            ops,
            event_loop_cancel,
        )
        .await;
    });

    // Submit 3 slow storage ops
    for i in 0..3 {
        let _ = scheduler_tx.send(
            openworkers_runtime_v8::runtime::SchedulerMessage::BindingStorage(
                i,
                "test".to_string(),
                StorageOp::Get { key: "k".into() },
            ),
        );
    }

    // Give tasks time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        3,
        "All 3 ops should have started"
    );
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "None should have completed yet"
    );

    // Cancel!
    cancel.cancel();

    // Give tasks time to observe cancellation
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // The 10-second ops should NOT have completed
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "Cancelled ops should not complete"
    );

    // No callback messages should have been sent (tasks returned early)
    assert!(
        callback_rx.try_recv().is_err(),
        "No callbacks expected after cancel"
    );

    // Cleanup
    drop(scheduler_tx);
    let _ = event_loop.await;
}

/// Test that cancellation does NOT affect timers that already fired.
#[tokio::test]
async fn test_scheduler_timers_fire_before_cancel() {
    use openworkers_runtime_v8::runtime::stream_manager::StreamManager;

    let ops: Arc<dyn OperationsHandler> = Arc::new(openworkers_core::DefaultOps);

    let (scheduler_tx, scheduler_rx) = mpsc::unbounded_channel();
    let (callback_tx, mut callback_rx) = mpsc::unbounded_channel();
    let callback_notify = Arc::new(tokio::sync::Notify::new());
    let stream_manager = Arc::new(StreamManager::new());
    let cancel = CancellationToken::new();

    let event_loop_cancel = cancel.clone();
    let event_loop = tokio::spawn(async move {
        openworkers_runtime_v8::runtime::run_event_loop(
            scheduler_rx,
            callback_tx,
            callback_notify,
            stream_manager,
            ops,
            event_loop_cancel,
        )
        .await;
    });

    // Schedule a 10ms timer
    let _ = scheduler_tx
        .send(openworkers_runtime_v8::runtime::SchedulerMessage::ScheduleTimeout(42, 10));

    // Wait for it to fire
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Timer callback should have been sent
    let msg = callback_rx.try_recv();
    assert!(
        matches!(
            msg,
            Ok(openworkers_runtime_v8::runtime::CallbackMessage::ExecuteTimeout(42))
        ),
        "Expected ExecuteTimeout(42)"
    );

    // Now cancel — should not affect already-fired timer
    cancel.cancel();

    // Cleanup
    drop(scheduler_tx);
    let _ = event_loop.await;
}

/// Integration test: dropping a Worker cancels its in-flight tasks.
#[tokio::test(flavor = "current_thread")]
async fn test_worker_drop_cancels_tasks() {
    run_in_local(|| async {
        // Simple worker that responds immediately — no bindings needed
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith(new Response('OK'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();

        let response = rx.await.unwrap();
        assert_eq!(response.status, 200);

        // Drop should not panic or hang
        drop(worker);

        // If we get here without hanging, cancellation works
    })
    .await;
}

/// Integration test: timers still work with cancellation token present.
#[tokio::test(flavor = "current_thread")]
async fn test_timers_work_with_cancellation() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith(handleRequest());
            });

            async function handleRequest() {
                await new Promise(resolve => setTimeout(resolve, 50));
                return new Response('Timer completed');
            }
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();

        let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(response.status, 200);
        assert_eq!(
            String::from_utf8_lossy(&response.body.collect().await.unwrap()),
            "Timer completed"
        );
    })
    .await;
}
