//! Test that fetch() without ops throws an error instead of hanging

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

/// Verify that fetch() without ops implementation rejects immediately
/// instead of hanging forever.
///
/// This test guards against regression of a bug where the reject callback
/// was not registered, causing fetch errors to be silently ignored.
#[tokio::test(flavor = "current_thread")]
async fn fetch_without_ops_should_reject_not_hang() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                try {
                    // This fetch() has no ops implementation, should throw
                    const response = await fetch('https://example.com/');
                    event.respondWith(new Response('Unexpected success'));
                } catch (e) {
                    // Expected: "Fetch not available" error
                    event.respondWith(new Response('Error: ' + e.message, { status: 500 }));
                }
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

        let (task, rx) = Event::fetch(req);

        // Should complete within 1 second (not hang for 30s wall timeout)
        let result = timeout(Duration::from_secs(1), async {
            worker.exec(task).await.unwrap();
            rx.await.unwrap()
        })
        .await;

        let response = result.expect("fetch() without ops should reject quickly, not hang");

        // Should get error response (500) with "Fetch not available" message
        assert_eq!(response.status, 500);

        let body = response.body.collect().await.unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("Fetch not available"),
            "Expected 'Fetch not available' error, got: {}",
            body_str
        );
    })
    .await;
}
