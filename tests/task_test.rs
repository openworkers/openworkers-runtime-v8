//! Tests for the Task event API (addEventListener('task', ...))

mod common;

use common::run_in_local;
use openworkers_core::{Event, Script, TaskSource};
use openworkers_runtime_v8::Worker;

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_basic() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                globalThis.taskReceived = true;
                globalThis.receivedTaskId = event.taskId;
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "basic-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        let received = worker.get_global_u32("taskReceived");
        assert_eq!(received, Some(1), "Task handler should have been called");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_respond_with_data() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                event.respondWith({
                    success: true,
                    data: { processed: true, taskId: event.taskId }
                });
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "respond-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");
        assert!(task_result.data.is_some(), "Task should return data");

        let data = task_result.data.unwrap();
        assert_eq!(data["processed"], true);
        assert_eq!(data["taskId"], "respond-test");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_with_payload() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                const result = event.payload.value * 2;
                event.respondWith({ success: true, data: { result } });
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let payload = serde_json::json!({ "value": 21 });
        let (task, rx) = Event::task(
            "payload-test".to_string(),
            Some(payload),
            Some(TaskSource::Invoke {
                origin: Some("test".to_string()),
            }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");
        assert_eq!(task_result.data.unwrap()["result"], 42);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_error_response() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                event.respondWith({
                    success: false,
                    error: "Something went wrong"
                });
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "error-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(!task_result.success, "Task should fail");
        assert_eq!(task_result.error.unwrap(), "Something went wrong");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_return_value() {
    run_in_local(|| async {
        // Test returning a value directly instead of using respondWith
        let code = r#"
            addEventListener('task', (event) => {
                return { success: true, data: { answer: 42 } };
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "return-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");
        assert_eq!(task_result.data.unwrap()["answer"], 42);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_exception() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                throw new Error("Intentional error");
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "exception-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(!task_result.success, "Task should fail due to exception");
        assert!(
            task_result.error.unwrap().contains("Intentional error"),
            "Error message should contain the exception"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_async_handler() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', async (event) => {
                await new Promise(resolve => setTimeout(resolve, 10));
                return { success: true, data: { async: true } };
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "async-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");
        assert_eq!(task_result.data.unwrap()["async"], true);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_wait_until() {
    run_in_local(|| async {
        let code = r#"
            globalThis.waitUntilDone = false;

            addEventListener('task', (event) => {
                event.respondWith({ success: true, data: { immediate: true } });

                event.waitUntil(new Promise(resolve => {
                    setTimeout(() => {
                        globalThis.waitUntilDone = true;
                        resolve();
                    }, 20);
                }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::task(
            "waituntil-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            1,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        // waitUntil should have completed
        let wait_until_done = worker.get_global_u32("waitUntilDone");
        assert_eq!(wait_until_done, Some(1), "waitUntil should have completed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_attempt_number() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                event.respondWith({
                    success: true,
                    data: { attempt: event.attempt }
                });
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Test with attempt = 3
        let (task, rx) = Event::task(
            "attempt-test".to_string(),
            None,
            Some(TaskSource::Invoke { origin: None }),
            3,
        );
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");
        assert_eq!(task_result.data.unwrap()["attempt"], 3);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_task_event_schedule_source() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('task', (event) => {
                event.respondWith({
                    success: true,
                    data: {
                        scheduledTime: event.scheduledTime,
                        hasScheduledTime: event.scheduledTime !== undefined
                    }
                });
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::from_schedule("schedule-test".to_string(), 1234567890);
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        let data = task_result.data.unwrap();
        assert_eq!(data["scheduledTime"], 1234567890.0);
        assert_eq!(data["hasScheduledTime"], true);
    })
    .await;
}
