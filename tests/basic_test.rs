use openworkers_runtime_v8::{HttpRequest, Script, Task, TerminationReason, Worker};
use std::collections::HashMap;

#[tokio::test]
async fn test_simple_response() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello World'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await.unwrap();

    assert_eq!(result, TerminationReason::Success);

    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
        String::from_utf8_lossy(response.body.as_bytes().unwrap()),
        "Hello World"
    );
}

#[tokio::test]
async fn test_custom_status() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Not Found', { status: 404 }));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = rx.await.unwrap();
    assert_eq!(response.status, 404);
    assert_eq!(
        String::from_utf8_lossy(response.body.as_bytes().unwrap()),
        "Not Found"
    );
}

#[tokio::test]
async fn test_worker_creation_error() {
    let code = r#"
        this is invalid javascript syntax
    "#;

    let script = Script::new(code);
    let result = Worker::new(script, None, None).await;

    assert!(result.is_err());
}
