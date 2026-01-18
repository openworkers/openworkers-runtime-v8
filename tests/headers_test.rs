mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Test Headers basic operations
#[tokio::test(flavor = "current_thread")]
async fn test_headers_basic() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers();

                // Test set and get
                headers.set('Content-Type', 'application/json');
                const ct = headers.get('Content-Type');

                // Test case-insensitivity
                const ctLower = headers.get('content-type');
                const ctUpper = headers.get('CONTENT-TYPE');

                // Test has
                const hasIt = headers.has('content-type');
                const notHas = headers.has('x-not-there');

                const result = ct === 'application/json'
                    && ctLower === 'application/json'
                    && ctUpper === 'application/json'
                    && hasIt === true
                    && notHas === false
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

/// Test Headers append
#[tokio::test(flavor = "current_thread")]
async fn test_headers_append() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers();

                headers.append('Accept', 'text/html');
                headers.append('Accept', 'application/json');

                const accept = headers.get('accept');

                const result = accept === 'text/html, application/json' ? 'OK' : 'FAIL: ' + accept;
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

/// Test Headers constructor with object
#[tokio::test(flavor = "current_thread")]
async fn test_headers_from_object() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers({
                    'Content-Type': 'text/plain',
                    'X-Custom': 'value'
                });

                const ct = headers.get('content-type');
                const custom = headers.get('x-custom');

                const result = ct === 'text/plain' && custom === 'value' ? 'OK' : 'FAIL';
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

/// Test Headers iteration
#[tokio::test(flavor = "current_thread")]
async fn test_headers_iteration() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers({
                    'A': '1',
                    'B': '2'
                });

                // Test entries()
                const entries = [...headers.entries()];

                // Test keys()
                const keys = [...headers.keys()];

                // Test values()
                const values = [...headers.values()];

                // Test forEach
                let forEachResult = '';
                headers.forEach((value, key) => {
                    forEachResult += key + '=' + value + ';';
                });

                const result = entries.length === 2
                    && keys.length === 2
                    && values.length === 2
                    && forEachResult.includes('a=1')
                    && forEachResult.includes('b=2')
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

/// Test Response with Headers
#[tokio::test(flavor = "current_thread")]
async fn test_response_with_headers() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers();
                headers.set('X-Custom-Header', 'test-value');
                headers.set('Content-Type', 'text/plain');

                const response = new Response('Hello', { headers });

                // Check response.headers is a Headers instance
                const isHeaders = response.headers instanceof Headers;
                const hasCustom = response.headers.get('x-custom-header') === 'test-value';

                const result = isHeaders && hasCustom ? 'OK' : 'FAIL';
                event.respondWith(response);
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

        let custom_header = response
            .headers
            .iter()
            .find(|(k, _)| k == "x-custom-header");
        assert!(custom_header.is_some());
        assert_eq!(custom_header.unwrap().1, "test-value");
    })
    .await;
}

/// Test Headers delete
#[tokio::test(flavor = "current_thread")]
async fn test_headers_delete() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const headers = new Headers({
                    'A': '1',
                    'B': '2'
                });

                headers.delete('a');

                const hasA = headers.has('a');
                const hasB = headers.has('b');

                const result = !hasA && hasB ? 'OK' : 'FAIL';
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
