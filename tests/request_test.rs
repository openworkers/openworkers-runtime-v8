mod common;

use bytes::Bytes;
use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Test Request basic properties
#[tokio::test(flavor = "current_thread")]
async fn test_request_basic() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = event.request;

                // Test instanceof
                const isRequest = req instanceof Request;

                // Test properties
                const hasUrl = typeof req.url === 'string';
                const hasMethod = typeof req.method === 'string';
                const hasHeaders = req.headers instanceof Headers;

                const result = isRequest && hasUrl && hasMethod && hasHeaders
                    ? 'OK' : 'FAIL';

                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/test".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request method and URL
#[tokio::test(flavor = "current_thread")]
async fn test_request_method_url() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = event.request;

                const method = req.method;
                const url = req.url;

                const result = method === 'POST' && url === 'http://localhost/api/users'
                    ? 'OK' : `FAIL: method=${method}, url=${url}`;

                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/api/users".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request headers
#[tokio::test(flavor = "current_thread")]
async fn test_request_headers() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = event.request;

                // Test headers (case-insensitive)
                const contentType = req.headers.get('content-type');
                const auth = req.headers.get('Authorization');

                const result = contentType === 'application/json'
                    && auth === 'Bearer token123'
                    ? 'OK' : `FAIL: ct=${contentType}, auth=${auth}`;

                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers,
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request body with text()
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_text() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = event.request;

                const body = await req.text();

                const result = body === 'Hello, World!'
                    ? 'OK' : `FAIL: body=${body}`;

                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Bytes(Bytes::from("Hello, World!")),
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request body with json()
#[tokio::test(flavor = "current_thread")]
async fn test_request_body_json() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = event.request;

                const data = await req.json();

                const result = data.name === 'test' && data.value === 42
                    ? 'OK' : `FAIL: data=${JSON.stringify(data)}`;

                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Post,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::Bytes(Bytes::from(r#"{"name":"test","value":42}"#)),
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request clone
#[tokio::test(flavor = "current_thread")]
async fn test_request_clone() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const req = new Request('http://example.com/test', {
                    method: 'PUT',
                    headers: { 'X-Custom': 'value' }
                });

                const cloned = req.clone();

                const sameUrl = cloned.url === req.url;
                const sameMethod = cloned.method === req.method;
                const sameHeader = cloned.headers.get('x-custom') === 'value';

                const result = sameUrl && sameMethod && sameHeader
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

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test Request from another Request
#[tokio::test(flavor = "current_thread")]
async fn test_request_from_request() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const original = new Request('http://example.com/', {
                    method: 'POST',
                    headers: { 'Content-Type': 'text/plain' }
                });

                // Create new Request from existing one, overriding method
                const modified = new Request(original, {
                    method: 'PUT'
                });

                const result = modified.url === 'http://example.com/'
                    && modified.method === 'PUT'
                    && modified.headers.get('content-type') === 'text/plain'
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

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}
