use bytes::Bytes;
use openworkers_runtime_v8::runtime::stream_manager::StreamChunk;
use openworkers_runtime_v8::{Script, Worker};
use std::time::Duration;

/// Test that the native stream bridge works end-to-end
/// This tests the flow: Rust -> StreamManager -> __nativeStreamRead -> JS ReadableStream
#[tokio::test]
async fn test_native_stream_bridge() {
    // Create worker with script that uses __createNativeStream
    let script = Script::new(
        r#"
            // We'll test the stream by storing results globally
            globalThis.streamResults = [];
            globalThis.streamDone = false;
            globalThis.streamError = null;

            // Test function that reads from a native stream
            globalThis.testNativeStream = async function(streamId) {
                try {
                    const stream = __createNativeStream(streamId);
                    const reader = stream.getReader();

                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) {
                            globalThis.streamDone = true;
                            break;
                        }
                        // Store chunk info (convert Uint8Array to string for easier testing)
                        const text = new TextDecoder().decode(value);
                        globalThis.streamResults.push(text);
                    }
                } catch (error) {
                    globalThis.streamError = error.message;
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

    // Get stream_manager and create a test stream
    let stream_manager = worker.stream_manager();
    let stream_id = stream_manager.create_stream("test://native-stream".to_string());

    // Start the JS test function
    worker
        .evaluate(&format!("testNativeStream({});", stream_id))
        .expect("Failed to start test");

    // Write chunks from Rust
    let chunks = vec!["Hello", " ", "World", "!"];
    for chunk in &chunks {
        stream_manager
            .write_chunk(stream_id, StreamChunk::Data(Bytes::from(*chunk)))
            .await
            .expect("Failed to write chunk");

        // Process callbacks to deliver the chunk to JS
        tokio::time::sleep(Duration::from_millis(10)).await;
        worker.process_callbacks();
    }

    // Close the stream
    stream_manager
        .write_chunk(stream_id, StreamChunk::Done)
        .await
        .expect("Failed to close stream");

    // Process remaining callbacks
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        worker.process_callbacks();
    }

    // Check results from JS using with_runtime
    let (done, results, error) = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        // Check streamDone
        let done_key = v8::String::new(scope, "streamDone").unwrap();
        let done_val = global.get(scope, done_key.into()).unwrap();
        let done = done_val.is_true();

        // Check streamResults
        let results_key = v8::String::new(scope, "streamResults").unwrap();
        let results_val = global.get(scope, results_key.into()).unwrap();
        let results_arr: v8::Local<v8::Array> = results_val.try_into().expect("Should be array");

        let mut results = String::new();
        for i in 0..results_arr.length() {
            let item = results_arr.get_index(scope, i).unwrap();
            results.push_str(&item.to_rust_string_lossy(scope));
        }

        // Check error
        let error_key = v8::String::new(scope, "streamError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        let error = if error_val.is_null() {
            None
        } else {
            Some(error_val.to_rust_string_lossy(scope))
        };

        (done, results, error)
    });

    let expected = chunks.join("");
    assert!(done, "Stream should be marked as done");
    assert_eq!(results, expected, "Stream content mismatch");
    assert!(error.is_none(), "Should have no error, got: {:?}", error);
}

/// Test that stream cancel works
#[tokio::test]
async fn test_native_stream_cancel() {
    let script = Script::new(
        r#"
            globalThis.cancelCalled = false;

            globalThis.testCancel = async function(streamId) {
                const stream = __createNativeStream(streamId);
                const reader = stream.getReader();

                // Read one chunk then cancel
                await reader.read();
                await reader.cancel('Test cancel');
                globalThis.cancelCalled = true;
            };

            addEventListener('fetch', event => {
                event.respondWith(new Response('OK'));
            });
        "#,
    );

    let mut worker = Worker::new(script, None, None)
        .await
        .expect("Worker creation failed");

    let stream_manager = worker.stream_manager();
    let stream_id = stream_manager.create_stream("test://cancel-stream".to_string());

    // Start the test
    worker
        .evaluate(&format!("testCancel({});", stream_id))
        .expect("Failed to start test");

    // Write one chunk
    stream_manager
        .write_chunk(stream_id, StreamChunk::Data(Bytes::from("first")))
        .await
        .expect("Failed to write chunk");

    // Process callbacks
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        worker.process_callbacks();
    }

    // The stream should have been closed by cancel
    assert_eq!(
        stream_manager.active_count(),
        0,
        "Stream should be closed after cancel"
    );
}

/// Test error handling in streams
#[tokio::test]
async fn test_native_stream_error() {
    let script = Script::new(
        r#"
            globalThis.caughtError = null;

            globalThis.testError = async function(streamId) {
                try {
                    const stream = __createNativeStream(streamId);
                    const reader = stream.getReader();

                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                    }
                } catch (error) {
                    globalThis.caughtError = error.message;
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

    let stream_manager = worker.stream_manager();
    let stream_id = stream_manager.create_stream("test://error-stream".to_string());

    // Start the test
    worker
        .evaluate(&format!("testError({});", stream_id))
        .expect("Failed to start test");

    // Send an error
    stream_manager
        .write_chunk(
            stream_id,
            StreamChunk::Error("Test error from Rust".to_string()),
        )
        .await
        .expect("Failed to send error");

    // Process callbacks
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        worker.process_callbacks();
    }

    // Check that error was caught
    let error = worker.with_runtime(|runtime| {
        use std::pin::pin;
        use v8;

        let scope = pin!(v8::HandleScope::new(&mut runtime.isolate));
        let mut scope = scope.init();
        let context = v8::Local::new(&scope, &runtime.context);
        let scope = &mut v8::ContextScope::new(&mut scope, context);
        let global = context.global(scope);

        let error_key = v8::String::new(scope, "caughtError").unwrap();
        let error_val = global.get(scope, error_key.into()).unwrap();
        error_val.to_rust_string_lossy(scope)
    });

    assert!(
        error.contains("Test error from Rust"),
        "Error message mismatch: {}",
        error
    );
}
