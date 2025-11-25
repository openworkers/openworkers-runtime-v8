use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

#[tokio::test]
async fn test_readable_stream_basic() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Create a simple stream
            const stream = new ReadableStream({
                start(controller) {
                    controller.enqueue(new Uint8Array([72, 101, 108, 108, 111])); // "Hello"
                    controller.close();
                }
            });

            // Read from stream
            const reader = stream.getReader();
            const { done, value } = await reader.read();

            // Create response with the value
            const text = new TextDecoder().decode(value);
            event.respondWith(new Response('Stream says: ' + text));
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
    let body_text = String::from_utf8_lossy(response.body.as_ref().unwrap());
    assert_eq!(body_text, "Stream says: Hello");
}

#[tokio::test]
async fn test_readable_stream_multiple_chunks() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Create stream with multiple chunks
            const stream = new ReadableStream({
                start(controller) {
                    controller.enqueue(new Uint8Array([72, 101]));      // "He"
                    controller.enqueue(new Uint8Array([108, 108, 111])); // "llo"
                    controller.close();
                }
            });

            // Read all chunks
            const reader = stream.getReader();
            const chunks = [];

            while (true) {
                const { done, value } = await reader.read();
                if (done) break;
                chunks.push(value);
            }

            // Concatenate chunks
            const totalLength = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
            const result = new Uint8Array(totalLength);
            let offset = 0;
            for (const chunk of chunks) {
                result.set(chunk, offset);
                offset += chunk.length;
            }

            const text = new TextDecoder().decode(result);
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
    let body_text = String::from_utf8_lossy(response.body.as_ref().unwrap());
    assert_eq!(body_text, "Got: Hello");
}

#[tokio::test]
async fn test_response_body_is_stream() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const response = new Response('Test data');

            // Check that body is a ReadableStream
            const isStream = response.body instanceof ReadableStream;
            event.respondWith(new Response('Is stream: ' + isStream));
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
    let body_text = String::from_utf8_lossy(response.body.as_ref().unwrap());
    assert_eq!(body_text, "Is stream: true");
}

#[tokio::test]
async fn test_stream_locked() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const stream = new ReadableStream({
                start(controller) {
                    controller.enqueue(new Uint8Array([1, 2, 3]));
                    controller.close();
                }
            });

            const reader1 = stream.getReader();

            // Try to get another reader (should fail)
            let errorCaught = false;
            try {
                const reader2 = stream.getReader();
            } catch (e) {
                errorCaught = e.message.includes('locked');
            }

            event.respondWith(new Response('Error caught: ' + errorCaught));
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
    let body_text = String::from_utf8_lossy(response.body.as_ref().unwrap());
    assert_eq!(body_text, "Error caught: true");
}
