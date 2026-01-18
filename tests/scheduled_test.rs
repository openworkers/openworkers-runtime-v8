mod common;

use common::run_in_local;
use openworkers_core::{Event, Script};
use openworkers_runtime_v8::Worker;

#[tokio::test(flavor = "current_thread")]
async fn test_scheduled_sync_handler() {
    run_in_local(|| async {
        let code = r#"
            globalThis.scheduledExecuted = false;
            addEventListener('scheduled', (event) => {
                globalThis.scheduledExecuted = true;
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::from_schedule("test-1".to_string(), 1234567890);
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        // Verify callback was called
        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        // Check global was set
        let executed = worker.get_global_u32("scheduledExecuted");
        assert_eq!(executed, Some(1), "scheduledExecuted should be true (1)");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_scheduled_async_handler() {
    run_in_local(|| async {
        let code = r#"
            globalThis.asyncValue = 0;
            addEventListener('scheduled', async (event) => {
                // Async operation: wait 50ms then set value
                await new Promise(resolve => setTimeout(resolve, 50));
                globalThis.asyncValue = 42;
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::from_schedule("test-2".to_string(), 1234567890);
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        // The async handler should have completed
        let value = worker.get_global_u32("asyncValue");
        assert_eq!(
            value,
            Some(42),
            "asyncValue should be 42 after async handler"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_scheduled_with_wait_until() {
    run_in_local(|| async {
        let code = r#"
            globalThis.handlerDone = false;
            globalThis.waitUntilDone = false;

            addEventListener('scheduled', (event) => {
                // Handler completes immediately
                globalThis.handlerDone = true;

                // But waitUntil keeps worker alive
                event.waitUntil(new Promise(resolve => {
                    setTimeout(() => {
                        globalThis.waitUntilDone = true;
                        resolve();
                    }, 50);
                }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::from_schedule("test-3".to_string(), 1234567890);
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        // Both handler and waitUntil should have completed
        let handler_done = worker.get_global_u32("handlerDone");
        let wait_until_done = worker.get_global_u32("waitUntilDone");

        assert_eq!(handler_done, Some(1), "handlerDone should be true");
        assert_eq!(wait_until_done, Some(1), "waitUntilDone should be true");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_scheduled_es_modules_style() {
    run_in_local(|| async {
        // ES modules `export default { scheduled }` is transpiled to `globalThis.default = { scheduled }`
        let code = r#"
            globalThis.esModuleScheduled = false;

            globalThis.default = {
                async scheduled(event, env, ctx) {
                    await new Promise(resolve => setTimeout(resolve, 30));
                    globalThis.esModuleScheduled = true;
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::from_schedule("test-4".to_string(), 1234567890);
        let result = worker.exec(task).await;
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let task_result = rx.await.unwrap();
        assert!(task_result.success, "Task should succeed");

        let value = worker.get_global_u32("esModuleScheduled");
        assert_eq!(value, Some(1), "esModuleScheduled should be true");
    })
    .await;
}
