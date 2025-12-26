use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test]
async fn test_abort_controller_basic() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const controller = new AbortController();
            const signal = controller.signal;

            // Initially not aborted
            const before = signal.aborted;

            // Abort
            controller.abort();

            // Now aborted
            const after = signal.aborted;

            const result = before === false && after === true ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
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

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = &response.body.collect().await.unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

#[tokio::test]
async fn test_abort_signal_listener() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const controller = new AbortController();
            let called = false;

            controller.signal.addEventListener('abort', () => {
                called = true;
            });

            controller.abort();

            const result = called ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
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

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = &response.body.collect().await.unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

#[tokio::test]
async fn test_abort_signal_reason() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const controller = new AbortController();
            controller.abort('custom reason');

            const result = controller.signal.reason === 'custom reason' ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
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

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = &response.body.collect().await.unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

#[tokio::test]
async fn test_abort_signal_static_abort() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const signal = AbortSignal.abort();

            const result = signal.aborted === true ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
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

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = &response.body.collect().await.unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}
