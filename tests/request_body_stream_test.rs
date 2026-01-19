mod common;

use bytes::Bytes;
use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, ResponseBody, Script};
use openworkers_runtime_v8::Worker;
use rand::Rng;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_text() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Read request body as text
                const text = await event.request.text();
                event.respondWith(new Response('Got: ' + text));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Create a streaming body
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Spawn task to send chunks
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("Hello"))).await.unwrap();
            tx.send(Ok(Bytes::from(" "))).await.unwrap();
            tx.send(Ok(Bytes::from("World"))).await.unwrap();
            // Channel closes when tx is dropped
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert_eq!(body_text, "Got: Hello World");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_reader() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Read request body chunk by chunk
                const reader = event.request.body.getReader();
                const chunks = [];

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;
                    chunks.push(new TextDecoder().decode(value));
                }

                event.respondWith(new Response('Chunks: ' + chunks.length + ' = ' + chunks.join('|')));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Create a streaming body
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Spawn task to send chunks
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("chunk1"))).await.unwrap();
            tx.send(Ok(Bytes::from("chunk2"))).await.unwrap();
            tx.send(Ok(Bytes::from("chunk3"))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert_eq!(body_text, "Chunks: 3 = chunk1|chunk2|chunk3");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_json() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Read request body as JSON
                const data = await event.request.json();
                event.respondWith(new Response('Name: ' + data.name + ', Age: ' + data.age));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Create a streaming body with JSON
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Spawn task to send JSON in chunks
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from(r#"{"name":"#))).await.unwrap();
            tx.send(Ok(Bytes::from(r#""Alice","#))).await.unwrap();
            tx.send(Ok(Bytes::from(r#""age":30}"#))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert_eq!(body_text, "Name: Alice, Age: 30");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_large() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Count total bytes from streaming body
                const reader = event.request.body.getReader();
                let totalBytes = 0;

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;
                    totalBytes += value.length;
                }

                event.respondWith(new Response('Total bytes: ' + totalBytes));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Create a streaming body with many chunks
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Spawn task to send many chunks (simulating large upload)
        let num_chunks = 100;
        let chunk_size = 1024;
        tokio::spawn(async move {
            for _ in 0..num_chunks {
                let chunk = Bytes::from(vec![b'X'; chunk_size]);
                if tx.send(Ok(chunk)).await.is_err() {
                    break;
                }
            }
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        let expected = format!("Total bytes: {}", num_chunks * chunk_size);
        assert_eq!(body_text, expected);
    })
    .await;
}

/// Simple echo test - stream in, stream out
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_echo() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Simply echo the request body back as response
                event.respondWith(new Response(event.request.body, {
                    headers: { 'Content-Type': 'text/plain' }
                }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send chunks with delays to simulate real streaming
        tokio::spawn(async move {
            for msg in ["Hello", " ", "streaming", " ", "world!"] {
                tx.send(Ok(Bytes::from(msg))).await.unwrap();
                sleep(Duration::from_millis(10)).await;
            }
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert_eq!(body_text, "Hello streaming world!");
    })
    .await;
}

/// Transform stream test - read numbers, multiply by 2, stream response
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_transform() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                const results = [];

                // Read all numbers from input stream
                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;

                    // Parse number from chunk and multiply by 2
                    const num = parseInt(new TextDecoder().decode(value), 10);
                    results.push(num * 2);
                }

                // Create response stream that emits transformed values
                const stream = new ReadableStream({
                    start(controller) {
                        for (const result of results) {
                            controller.enqueue(new TextEncoder().encode(result + '\n'));
                        }
                        controller.close();
                    }
                });

                event.respondWith(new Response(stream, {
                    headers: { 'Content-Type': 'text/plain' }
                }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send numbers 1, 2, 3, 4, 5 as separate chunks
        tokio::spawn(async move {
            for n in 1..=5 {
                tx.send(Ok(Bytes::from(n.to_string()))).await.unwrap();
                sleep(Duration::from_millis(5)).await;
            }
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;

        assert!(result.is_ok());
        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        // Response should be streaming
        let rx = match response.body {
            ResponseBody::Stream(rx) => rx,
            _ => panic!("Expected streaming response"),
        };

        // Count chunks received and collect content
        let mut chunk_count = 0;
        let mut all_bytes = Vec::new();
        let mut rx = rx;

        while let Some(result) = rx.recv().await {
            let bytes = result.unwrap();
            chunk_count += 1;
            all_bytes.extend_from_slice(&bytes);
        }

        let body_text = String::from_utf8_lossy(&all_bytes);

        // Should receive exactly 5 chunks (one per number)
        assert_eq!(chunk_count, 5, "Expected 5 chunks, got {}", chunk_count);

        // Should get 2, 4, 6, 8, 10 (each number × 2)
        assert_eq!(body_text, "2\n4\n6\n8\n10\n");
    })
    .await;
}

/// Meta test - single request with bidirectional streaming
/// Send random numbers through request stream, receive doubled values in response stream
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_bidirectional() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();

                // Create a transform stream that doubles each number
                const stream = new ReadableStream({
                    async pull(controller) {
                        const { done, value } = await reader.read();
                        if (done) {
                            controller.close();
                            return;
                        }

                        // Parse number, multiply by 2, send back
                        const num = parseInt(new TextDecoder().decode(value), 10);
                        const result = (num * 2).toString();
                        controller.enqueue(new TextEncoder().encode(result));
                    }
                });

                event.respondWith(new Response(stream));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Generate 5 random numbers
        let mut rng = rand::rng();
        let random_numbers: Vec<u64> = (0..5).map(|_| rng.random_range(1..1000)).collect();
        let expected: Vec<u64> = random_numbers.iter().map(|n| n * 2).collect();

        // Spawn task to send random numbers
        let numbers_to_send = random_numbers.clone();
        let sender = tokio::spawn(async move {
            for value in numbers_to_send {
                tx.send(Ok(Bytes::from(value.to_string()))).await.unwrap();
                sleep(Duration::from_millis(10)).await;
            }
            // tx drops here, closing the stream
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        // Read response chunks
        let response_rx = match response.body {
            ResponseBody::Stream(rx) => rx,
            _ => panic!("Expected streaming response"),
        };

        let mut received_values = Vec::new();
        let mut response_rx = response_rx;

        while let Some(result) = response_rx.recv().await {
            let bytes = result.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            let num: u64 = text.parse().expect("Expected a number");
            received_values.push(num);
        }

        sender.await.unwrap();

        // Verify: each received value should be double the sent value
        assert_eq!(
            received_values, expected,
            "Sent {:?}, expected {:?}, got {:?}",
            random_numbers, expected, received_values
        );
    })
    .await;
}

/// Edge case: empty stream (channel closes immediately)
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_empty() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                event.respondWith(new Response('Length: ' + text.length));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Close channel immediately (empty stream)
        drop(tx);

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Empty stream should result in empty body
        assert_eq!(body_text, "Length: 0");
    })
    .await;
}

/// Edge case: error in stream
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_error() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                try {
                    const text = await event.request.text();
                    event.respondWith(new Response('Got: ' + text));
                } catch (e) {
                    event.respondWith(new Response('Error: ' + e.message, { status: 500 }));
                }
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send some data then an error
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("Hello"))).await.unwrap();
            tx.send(Err("Stream error!".to_string())).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Should have received partial data before error
        // The exact behavior depends on implementation - either we get partial data or error
        assert!(
            body_text.contains("Hello") || body_text.contains("Error"),
            "Got unexpected response: {}",
            body_text
        );
    })
    .await;
}

/// Edge case: UTF-8 multi-byte character split across chunks
/// Emoji 🎉 = 4 bytes (F0 9F 8E 89), we split it in the middle
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_utf8_boundary() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const text = await event.request.text();
                // Count actual characters (not bytes)
                const charCount = [...text].length;
                event.respondWith(new Response('Chars: ' + charCount + ' Text: ' + text));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // 🎉 = F0 9F 8E 89 (4 bytes)
        // We send: "Hi " + first 2 bytes of emoji | last 2 bytes of emoji + "!"
        tokio::spawn(async move {
            // "Hi " + F0 9F (first half of 🎉)
            tx.send(Ok(Bytes::from(vec![b'H', b'i', b' ', 0xF0, 0x9F])))
                .await
                .unwrap();

            // 8E 89 (second half of 🎉) + "!"
            tx.send(Ok(Bytes::from(vec![0x8E, 0x89, b'!'])))
                .await
                .unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Should correctly reconstruct: "Hi 🎉!" = 5 characters
        assert_eq!(body_text, "Chars: 5 Text: Hi 🎉!");
    })
    .await;
}

/// Edge case: Binary data with embedded null bytes
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_binary_nulls() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const buffer = await event.request.arrayBuffer();
                const bytes = new Uint8Array(buffer);

                // Count null bytes and total length
                let nullCount = 0;
                for (let i = 0; i < bytes.length; i++) {
                    if (bytes[i] === 0) nullCount++;
                }

                event.respondWith(new Response('Length: ' + bytes.length + ' Nulls: ' + nullCount));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send binary data with null bytes embedded
        tokio::spawn(async move {
            // [0x01, 0x00, 0x02, 0x00, 0x00, 0x03]
            tx.send(Ok(Bytes::from(vec![0x01, 0x00, 0x02])))
                .await
                .unwrap();

            tx.send(Ok(Bytes::from(vec![0x00, 0x00, 0x03])))
                .await
                .unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // 6 bytes total, 3 null bytes
        assert_eq!(body_text, "Length: 6 Nulls: 3");
    })
    .await;
}

/// Edge case: Trying to read body twice (should fail - body already consumed)
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_double_consume() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // First read - should work
                const text1 = await event.request.text();

                // Second read - body already consumed
                try {
                    const text2 = await event.request.text();
                    event.respondWith(new Response('ERROR: Got second read: ' + text2));
                } catch (e) {
                    event.respondWith(new Response('OK: First=' + text1 + ' Error=' + e.message));
                }
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        tokio::spawn(async move {
            tx.send(Ok(Bytes::from("Hello"))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Should indicate body was already consumed
        assert!(
            body_text.starts_with("OK:"),
            "Expected body used error, got: {}",
            body_text
        );
    })
    .await;
}

/// Edge case: Body never consumed - JS responds without reading
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_never_consumed() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Respond immediately without reading body
                event.respondWith(new Response('Ignored body'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send data that will never be read
        let sender = tokio::spawn(async move {
            for i in 0..5 {
                // Channel might close early since JS doesn't read
                if tx
                    .send(Ok(Bytes::from(format!("chunk{}", i))))
                    .await
                    .is_err()
                {
                    return "channel_closed";
                }

                sleep(Duration::from_millis(10)).await;
            }

            "all_sent"
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);
        assert_eq!(body_text, "Ignored body");

        // Sender might complete or might get channel closed - both are OK
        let _sender_result = sender.await.unwrap();
    })
    .await;
}

/// Edge case: Backpressure - fast producer, slow consumer
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_backpressure() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();
                let total = 0;
                let chunks = 0;

                while (true) {
                    const { done, value } = await reader.read();
                    if (done) break;

                    // Simulate slow consumer
                    await new Promise(r => setTimeout(r, 20));

                    total += value.length;
                    chunks++;
                }

                event.respondWith(new Response('Chunks: ' + chunks + ' Bytes: ' + total));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        // Small buffer to test backpressure
        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(2);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Fast producer - sends chunks as fast as possible
        let producer = tokio::spawn(async move {
            let mut sent = 0;

            for i in 0..20 {
                // With buffer size 2 and slow consumer, this should block
                if tx
                    .send(Ok(Bytes::from(format!("data{:02}", i))))
                    .await
                    .is_err()
                {
                    break;
                }

                sent += 1;
            }

            sent
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let chunks_sent = producer.await.unwrap();

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Should have received all 20 chunks
        assert_eq!(chunks_sent, 20);
        assert!(
            body_text.contains("Chunks: 20"),
            "Expected 20 chunks, got: {}",
            body_text
        );
    })
    .await;
}

/// Edge case: Reading body as arrayBuffer (not just text/json)
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_arraybuffer() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const buffer = await event.request.arrayBuffer();
                const view = new Uint8Array(buffer);

                // Sum all bytes
                let sum = 0;
                for (let i = 0; i < view.length; i++) {
                    sum += view[i];
                }

                event.respondWith(new Response('Length: ' + view.length + ' Sum: ' + sum));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send bytes [1, 2, 3] and [4, 5]
        tokio::spawn(async move {
            tx.send(Ok(Bytes::from(vec![1u8, 2, 3]))).await.unwrap();
            tx.send(Ok(Bytes::from(vec![4u8, 5]))).await.unwrap();
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // 5 bytes, sum = 1+2+3+4+5 = 15
        assert_eq!(body_text, "Length: 5 Sum: 15");
    })
    .await;
}

/// Edge case: JS stops reading before stream ends (cancellation)
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_stream_partial_read() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const reader = event.request.body.getReader();

                // Read only first chunk, ignore the rest
                const { value } = await reader.read();
                const firstChunk = new TextDecoder().decode(value);

                // Cancel the rest
                await reader.cancel();

                event.respondWith(new Response('First: ' + firstChunk));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let (tx, rx) = mpsc::channel::<Result<Bytes, String>>(16);

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Stream(rx),
        };

        // Send multiple chunks
        tokio::spawn(async move {
            for i in 1..=10 {
                if tx
                    .send(Ok(Bytes::from(format!("chunk{}", i))))
                    .await
                    .is_err()
                {
                    // Channel closed by consumer
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        });

        let (task, response_rx) = Event::fetch(req);
        let result = worker.exec(task).await;
        assert!(result.is_ok());

        let response = response_rx.await.unwrap();
        let body_bytes = response.body.collect().await.unwrap();
        let body_text = String::from_utf8_lossy(&body_bytes);

        // Should have read only first chunk
        assert_eq!(body_text, "First: chunk1");
    })
    .await;
}
