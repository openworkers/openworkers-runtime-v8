//! Tests for create_worker_snapshot — verifies snapshot creation and round-trip execution.
//!
//! All snapshot tests are in a single test function because V8 snapshot creation
//! is not safe to run concurrently with isolate creation on macOS (single-threaded GC).

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script, WorkerCode};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

fn make_get_request(url: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_worker_snapshot() {
    run_in_local(|| async {
        // --- Test 1: create_worker_snapshot returns non-empty bytes ---
        let js_code = r#"globalThis.default = { fetch() { return new Response("ok"); } };"#;
        let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, None).unwrap();
        assert!(!snapshot.output.is_empty(), "snapshot should not be empty");
        assert!(
            snapshot.output.len() > 1000,
            "snapshot should be substantial (got {} bytes)",
            snapshot.output.len()
        );

        // --- Test 2: full round-trip (create snapshot → load → execute) ---
        let js_code = r#"
            globalThis.default = {
                async fetch(request) {
                    return new Response('Hello from snapshot!', {
                        status: 200,
                        headers: { 'Content-Type': 'text/plain' }
                    });
                }
            };
        "#;

        let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, None).unwrap();
        let script = Script::new(WorkerCode::snapshot(snapshot.output));
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::fetch(make_get_request("http://localhost/test"));
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        assert_eq!(response.status, 200);
        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "Hello from snapshot!");

        // --- Test 3: top-level console.log doesn't crash during snapshotting ---
        let js_code = r#"
            console.log('this runs during snapshot creation');
            globalThis.default = {
                async fetch(request) {
                    return new Response('after console.log');
                }
            };
        "#;

        let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, None).unwrap();
        let script = Script::new(WorkerCode::snapshot(snapshot.output));
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "after console.log");

        // --- Test 4: env vars are available during snapshotting ---
        let js_code = r#"
            const secret = globalThis.env.API_KEY;
            globalThis.default = {
                async fetch(request) {
                    return new Response('key=' + secret);
                }
            };
        "#;

        let mut env = HashMap::new();
        env.insert("API_KEY".to_string(), "test-secret-123".to_string());

        let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, Some(&env)).unwrap();
        let script = Script::new(WorkerCode::snapshot(snapshot.output));
        let mut worker = Worker::new(script, None).await.unwrap();

        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "key=test-secret-123");

        // --- Test 5: code that throws returns Err (no panic) ---
        let result =
            openworkers_runtime_v8::create_worker_snapshot("throw new Error('snap boom');", None);
        assert!(result.is_err(), "throwing code should return Err");
        assert!(
            result.unwrap_err().contains("snap boom"),
            "error should contain the thrown message"
        );
    })
    .await;
}
