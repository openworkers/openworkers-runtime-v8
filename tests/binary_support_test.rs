use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

#[tokio::test]
async fn test_binary_response() {
    let code = r#"
        addEventListener('fetch', (event) => {
            // Create binary data (Uint8Array)
            const bytes = new Uint8Array([72, 101, 108, 108, 111]); // "Hello"
            event.respondWith(new Response(bytes, { status: 200 }));
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
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);

    // Check binary content
    let body_bytes = response.body.unwrap();
    assert_eq!(body_bytes.as_ref(), b"Hello");
}

#[tokio::test]
async fn test_text_method_with_binary() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Simulate fetch response with binary data
            const bytes = new Uint8Array([72, 101, 108, 108, 111]); // "Hello"
            const response = new Response(bytes);

            // Use .text() method to decode to string
            const text = await response.text();

            event.respondWith(new Response('Got: ' + text));
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
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();
    let body_bytes = response.body.unwrap();
    let body_text = String::from_utf8_lossy(body_bytes.as_ref());
    assert_eq!(body_text, "Got: Hello");
}

#[tokio::test]
async fn test_array_buffer_method() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const bytes = new Uint8Array([1, 2, 3, 4, 5]);
            const response = new Response(bytes);

            // Use .arrayBuffer() method
            const buffer = await response.arrayBuffer();
            const view = new Uint8Array(buffer);

            // Sum all bytes
            let sum = 0;
            for (let i = 0; i < view.length; i++) {
                sum += view[i];
            }

            event.respondWith(new Response('Sum: ' + sum));
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
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();
    let body_bytes = response.body.unwrap();
    let body_text = String::from_utf8_lossy(body_bytes.as_ref());
    assert_eq!(body_text, "Sum: 15"); // 1+2+3+4+5 = 15
}

#[tokio::test]
async fn test_string_still_works() {
    // Backward compatibility: string bodies should still work
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Plain text response'));
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
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();
    let body_bytes = response.body.unwrap();
    let body_text = String::from_utf8_lossy(body_bytes.as_ref());
    assert_eq!(body_text, "Plain text response");
}
