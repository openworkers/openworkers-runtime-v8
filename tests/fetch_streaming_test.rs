use openworkers_core::Script;
use openworkers_runtime_v8::Worker;
use std::time::Duration;

/// Test that fetch with streaming works correctly
/// This tests the flow: JS fetch() -> __nativeFetchStreaming -> execute_fetch_streaming -> StreamManager -> JS Response
#[tokio::test]
async fn test_fetch_streaming_basic() {
    let script = Script::new(
        r#"
            globalThis.fetchResult = null;
            globalThis.fetchError = null;
            globalThis.responseStatus = 0;
            globalThis.responseOk = false;

            globalThis.testFetchStreaming = async function(url) {
                try {
                    const response = await fetch(url);
                    globalThis.responseStatus = response.status;
                    globalThis.responseOk = response.ok;

                    // Read the body via streaming
                    const text = await response.text();
                    globalThis.fetchResult = text;
                } catch (error) {
                    globalThis.fetchError = error.message || String(error);
                }
            };

            addEventListener('fetch', event => {
                event.respondWith(new Response('OK'));
            });
        "#,
    );

    let mut worker = Worker::new(script, None, None)
        .await
        .expect("Worker creation failed");

    // Start the fetch test with httpbin.workers.rocks (dogfooding!)
    worker
        .evaluate("testFetchStreaming('https://httpbin.workers.rocks/get');")
        .expect("Failed to start test");

    // Process callbacks (this is a real HTTP request so may take a few seconds)
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.process_callbacks();
    }

    // Check results
    let (status, ok, result, error) = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let status_key = v8::String::new(scope, "responseStatus").unwrap();
        let status_val = global.get(scope, status_key.into()).unwrap();
        let status = status_val
            .to_number(scope)
            .map(|n| n.value() as i32)
            .unwrap_or(0);

        let ok_key = v8::String::new(scope, "responseOk").unwrap();
        let ok_val = global.get(scope, ok_key.into()).unwrap();
        let ok = ok_val.is_true();

        let result_key = v8::String::new(scope, "fetchResult").unwrap();
        let result_val = global.get(scope, result_key.into()).unwrap();
        let result = if result_val.is_null() || result_val.is_undefined() {
            None
        } else {
            Some(result_val.to_rust_string_lossy(scope))
        };

        let error_key = v8::String::new(scope, "fetchError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        let error = if error_val.is_null() || error_val.is_undefined() {
            None
        } else {
            Some(error_val.to_rust_string_lossy(scope))
        };

        (status, ok, result, error)
    });

    assert!(
        error.is_none(),
        "Fetch should not have error, got: {:?}",
        error
    );
    assert_eq!(status, 200, "Status should be 200");
    assert!(ok, "Response.ok should be true");
    assert!(result.is_some(), "Should have result body");
    let body = result.unwrap();
    // httpbin.workers.rocks/get returns request info as JSON
    assert!(
        body.contains("headers") || body.contains("url"),
        "Body should be JSON with request info: {}",
        body
    );
}

/// Test streaming fetch with chunked reading
#[tokio::test]
async fn test_fetch_streaming_chunked() {
    let script = Script::new(
        r#"
            globalThis.chunkCount = 0;
            globalThis.totalBytes = 0;
            globalThis.streamDone = false;
            globalThis.streamError = null;

            globalThis.testStreamedRead = async function(url) {
                try {
                    const response = await fetch(url);
                    const reader = response.body.getReader();

                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) {
                            globalThis.streamDone = true;
                            break;
                        }
                        globalThis.chunkCount++;
                        globalThis.totalBytes += value.length;
                    }
                } catch (error) {
                    globalThis.streamError = error.message || String(error);
                }
            };

            addEventListener('fetch', event => {
                event.respondWith(new Response('OK'));
            });
        "#,
    );

    let mut worker = Worker::new(script, None, None)
        .await
        .expect("Worker creation failed");

    // Use httpbin.workers.rocks /bytes/1024 endpoint
    worker
        .evaluate("testStreamedRead('https://httpbin.workers.rocks/bytes/1024');")
        .expect("Failed to start test");

    // Process callbacks
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.process_callbacks();
    }

    // Check results
    let (chunk_count, total_bytes, done, error) = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let count_key = v8::String::new(scope, "chunkCount").unwrap();
        let count_val = global.get(scope, count_key.into()).unwrap();
        let count = count_val
            .to_number(scope)
            .map(|n| n.value() as i32)
            .unwrap_or(0);

        let bytes_key = v8::String::new(scope, "totalBytes").unwrap();
        let bytes_val = global.get(scope, bytes_key.into()).unwrap();
        let bytes = bytes_val
            .to_number(scope)
            .map(|n| n.value() as i32)
            .unwrap_or(0);

        let done_key = v8::String::new(scope, "streamDone").unwrap();
        let done_val = global.get(scope, done_key.into()).unwrap();
        let done = done_val.is_true();

        let error_key = v8::String::new(scope, "streamError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        let error = if error_val.is_null() || error_val.is_undefined() {
            None
        } else {
            Some(error_val.to_rust_string_lossy(scope))
        };

        (count, bytes, done, error)
    });

    assert!(error.is_none(), "Should not have error, got: {:?}", error);
    assert!(done, "Stream should be done");
    assert!(
        chunk_count >= 1,
        "Should have received at least 1 chunk, got: {}",
        chunk_count
    );
    // httpbin.workers.rocks/bytes/1024 returns exactly 1024 bytes
    assert_eq!(
        total_bytes, 1024,
        "Should have received 1024 bytes, got: {}",
        total_bytes
    );
}

/// Test progressive streaming with /stream endpoint
#[tokio::test]
async fn test_fetch_progressive_stream() {
    let script = Script::new(
        r#"
            globalThis.chunks = [];
            globalThis.streamDone = false;
            globalThis.streamError = null;

            globalThis.testProgressiveStream = async function(url) {
                try {
                    const response = await fetch(url);
                    const reader = response.body.getReader();
                    const decoder = new TextDecoder();

                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) {
                            globalThis.streamDone = true;
                            break;
                        }
                        // Decode and store each chunk
                        const text = decoder.decode(value);
                        globalThis.chunks.push(text);
                    }
                } catch (error) {
                    globalThis.streamError = error.message || String(error);
                }
            };

            addEventListener('fetch', event => {
                event.respondWith(new Response('OK'));
            });
        "#,
    );

    let mut worker = Worker::new(script, None, None)
        .await
        .expect("Worker creation failed");

    // Use /stream/5 - 5 chunks with delay
    worker
        .evaluate("testProgressiveStream('https://httpbin.workers.rocks/stream/5');")
        .expect("Failed to start test");

    // Process callbacks - need more time for streaming with delays
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.process_callbacks();
    }

    // Check results
    let (chunk_count, done, error) = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let chunks_key = v8::String::new(scope, "chunks").unwrap();
        let chunks_val = global.get(scope, chunks_key.into()).unwrap();
        let chunks_arr: v8::Local<v8::Array> = chunks_val.try_into().expect("Should be array");
        let count = chunks_arr.length() as i32;

        let done_key = v8::String::new(scope, "streamDone").unwrap();
        let done_val = global.get(scope, done_key.into()).unwrap();
        let done = done_val.is_true();

        let error_key = v8::String::new(scope, "streamError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        let error = if error_val.is_null() || error_val.is_undefined() {
            None
        } else {
            Some(error_val.to_rust_string_lossy(scope))
        };

        (count, done, error)
    });

    assert!(error.is_none(), "Should not have error, got: {:?}", error);
    assert!(done, "Stream should be done");
    // Should receive multiple chunks (at least some of the 5 expected)
    assert!(
        chunk_count >= 1,
        "Should have received chunks, got: {}",
        chunk_count
    );
}

/// Test fetch forward - direct streaming from upstream to response
#[tokio::test]
async fn test_fetch_forward() {
    let script = Script::new(
        r#"
            globalThis.forwardStatus = 0;
            globalThis.forwardResult = null;
            globalThis.forwardError = null;

            // Simulate fetch forward by reading the upstream response
            globalThis.testFetchForward = async function(url) {
                try {
                    // This is like: event.respondWith(fetch(url))
                    const response = await fetch(url);
                    globalThis.forwardStatus = response.status;

                    // Verify we can still read the body after forward
                    const text = await response.text();
                    globalThis.forwardResult = text.length > 0;
                } catch (error) {
                    globalThis.forwardError = error.message || String(error);
                }
            };

            addEventListener('fetch', event => {
                event.respondWith(new Response('OK'));
            });
        "#,
    );

    let mut worker = Worker::new(script, None, None)
        .await
        .expect("Worker creation failed");

    // Test with httpbin.workers.rocks
    worker
        .evaluate("testFetchForward('https://httpbin.workers.rocks/get');")
        .expect("Failed to start test");

    // Process callbacks
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        worker.process_callbacks();
    }

    // Check results
    let (status, has_result, error) = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let status_key = v8::String::new(scope, "forwardStatus").unwrap();
        let status_val = global.get(scope, status_key.into()).unwrap();
        let status = status_val
            .to_number(scope)
            .map(|n| n.value() as i32)
            .unwrap_or(0);

        let result_key = v8::String::new(scope, "forwardResult").unwrap();
        let result_val = global.get(scope, result_key.into()).unwrap();
        let has_result = result_val.is_true();

        let error_key = v8::String::new(scope, "forwardError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        let error = if error_val.is_null() || error_val.is_undefined() {
            None
        } else {
            Some(error_val.to_rust_string_lossy(scope))
        };

        (status, has_result, error)
    });

    assert!(
        error.is_none(),
        "Forward should not have error, got: {:?}",
        error
    );
    assert_eq!(status, 200, "Status should be 200");
    assert!(has_result, "Should have received body content");
}
