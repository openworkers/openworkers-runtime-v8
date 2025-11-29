use openworkers_core::{HttpBody, HttpMethod, HttpRequest, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test]
async fn test_response_with_stream_chunks() {
    // Test that Response created with a stream works correctly
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Create a stream manually
            const stream = new ReadableStream({
                start(controller) {
                    // Simulate chunked data
                    controller.enqueue(new Uint8Array([72, 101]));      // "He"
                    controller.enqueue(new Uint8Array([108, 108, 111])); // "llo"
                    controller.close();
                }
            });

            // Create Response with stream
            const response = new Response(stream, { status: 200 });

            // Forward it
            event.respondWith(response);
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);

    let body_bytes = response.body.collect().await.unwrap();
    let body_text = String::from_utf8_lossy(&body_bytes);
    assert_eq!(body_text, "Hello");
}

#[tokio::test]
async fn test_stream_consumed_by_text() {
    // Test that reading stream via .text() works
    let code = r#"
        addEventListener('fetch', async (event) => {
            const stream = new ReadableStream({
                start(controller) {
                    controller.enqueue(new Uint8Array([84, 101, 115, 116])); // "Test"
                    controller.close();
                }
            });

            const response = new Response(stream);

            // Consume stream via .text()
            const text = await response.text();

            event.respondWith(new Response('Got: ' + text));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();

    let body_bytes = response.body.collect().await.unwrap();
    let body_text = String::from_utf8_lossy(&body_bytes);
    assert_eq!(body_text, "Got: Test");
}

#[tokio::test]
async fn test_body_used_flag() {
    // Test that bodyUsed flag prevents double consumption
    let code = r#"
        addEventListener('fetch', async (event) => {
            const response = new Response('Test data');

            // First read
            await response.text();

            // Try second read (should throw)
            let errorCaught = false;
            try {
                await response.text();
            } catch (e) {
                errorCaught = e.message.includes('already been consumed');
            }

            event.respondWith(new Response('Error caught: ' + errorCaught));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;

    assert!(result.is_ok());
    let response = rx.await.unwrap();

    let body_bytes = response.body.collect().await.unwrap();
    let body_text = String::from_utf8_lossy(&body_bytes);
    assert_eq!(body_text, "Error caught: true");
}
