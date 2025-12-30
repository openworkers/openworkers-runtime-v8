mod common;

use common::run_in_local;
use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test(flavor = "current_thread")]
async fn test_btoa_atob() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Test btoa
                const encoded = btoa('Hello, World!');

                // Test atob
                const decoded = atob(encoded);

                // Test round-trip
                const roundTrip = atob(btoa('Test 123'));

                const result = encoded === 'SGVsbG8sIFdvcmxkIQ=='
                    && decoded === 'Hello, World!'
                    && roundTrip === 'Test 123'
                    ? 'OK' : `FAIL: encoded=${encoded}, decoded=${decoded}`;

                event.respondWith(new Response(result));
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

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_base64_binary() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Test with binary data
                const binary = new Uint8Array([0, 128, 255]);
                const encoded = btoa(String.fromCharCode(...binary));

                // Decode and check
                const decoded = atob(encoded);
                const bytes = new Uint8Array([...decoded].map(c => c.charCodeAt(0)));

                const result = bytes[0] === 0 && bytes[1] === 128 && bytes[2] === 255
                    ? 'OK' : 'FAIL';

                event.respondWith(new Response(result));
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

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}
