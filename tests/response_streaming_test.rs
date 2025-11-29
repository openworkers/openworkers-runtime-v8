use openworkers_core::{HttpBody, HttpMethod, HttpRequest, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Test that fetch forward returns a streaming response
#[tokio::test]
async fn test_fetch_forward_streaming() {
    let code = r#"
        addEventListener('fetch', (event) => {
            // Direct fetch forward - body should be a native stream
            event.respondWith(fetch('https://httpbin.workers.rocks/bytes/100'));
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
    let _result = worker.exec(task).await;

    // Wait for response with timeout
    let response = tokio::time::timeout(tokio::time::Duration::from_secs(10), rx)
        .await
        .expect("Timeout waiting for response")
        .expect("Channel error");

    assert_eq!(response.status, 200);

    // The response body should be a stream (not bytes)
    assert!(
        response.body.is_stream(),
        "Fetch forward should return streaming body"
    );

    // Consume the stream
    if let HttpBody::Stream(mut rx) = response.body {
        let mut total_bytes = 0;
        while let Some(result) = rx.recv().await {
            match result {
                Ok(bytes) => total_bytes += bytes.len(),
                Err(e) => panic!("Stream error: {}", e),
            }
        }
        assert_eq!(
            total_bytes, 100,
            "Should have received 100 bytes from /bytes/100"
        );
    }
}

/// Test that string responses are streamed
#[tokio::test]
async fn test_string_response_is_streamed() {
    let code = r#"
        addEventListener('fetch', (event) => {
            // Direct string response - should be streamed
            event.respondWith(new Response('Hello World'));
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
    worker.exec(task).await.unwrap();

    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);

    // All responses with body should be streamed
    assert!(
        response.body.is_stream(),
        "String response should be streamed"
    );

    let body_bytes = response.body.collect().await.unwrap();
    let body_text = String::from_utf8_lossy(&body_bytes);
    assert_eq!(body_text, "Hello World");
}

/// Test streaming response with chunked reading
#[tokio::test]
async fn test_streaming_response_chunked() {
    let code = r#"
        addEventListener('fetch', (event) => {
            // Fetch stream endpoint - should receive multiple chunks
            event.respondWith(fetch('https://httpbin.workers.rocks/stream/3'));
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
    let _result = worker.exec(task).await;

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(15), rx)
        .await
        .expect("Timeout")
        .expect("Channel error");

    assert_eq!(response.status, 200);
    assert!(response.body.is_stream(), "Should be streaming");

    // Consume and verify chunks
    if let HttpBody::Stream(mut rx) = response.body {
        let mut chunks = Vec::new();
        while let Some(result) = rx.recv().await {
            match result {
                Ok(bytes) => chunks.push(bytes),
                Err(e) => panic!("Stream error: {}", e),
            }
        }
        // /stream/3 returns 3 JSON objects, one per line
        assert!(chunks.len() >= 1, "Should have received chunks");
    }
}

/// Test that processed fetch (not forward) still works
#[tokio::test]
async fn test_processed_fetch_response() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Fetch but process the response (consume it)
            const upstream = await fetch('https://httpbin.workers.rocks/get');
            const text = await upstream.text();

            // Return a new response with processed content
            event.respondWith(new Response('Processed: ' + text.substring(0, 20)));
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
    let _result = worker.exec(task).await;

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(10), rx)
        .await
        .expect("Timeout")
        .expect("Channel error");

    assert_eq!(response.status, 200);

    // All responses with body should be streamed
    assert!(
        response.body.is_stream(),
        "Processed response should be streamed"
    );

    let body_bytes = response.body.collect().await.unwrap();
    let body = String::from_utf8_lossy(&body_bytes);
    assert!(body.starts_with("Processed:"), "Body: {}", body);
}
