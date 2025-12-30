mod common;

use common::run_in_local;
use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Test that string responses are streamed
#[tokio::test(flavor = "current_thread")]
async fn test_string_response_is_streamed() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                // Direct string response - should be streamed
                event.respondWith(new Response('Hello World'));
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
    })
    .await;
}
