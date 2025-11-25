use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

/// Test Headers basic operations
#[tokio::test]
async fn test_headers_basic() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test Headers append
#[tokio::test]
async fn test_headers_append() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test Headers constructor with object
#[tokio::test]
async fn test_headers_from_object() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test Headers iteration
#[tokio::test]
async fn test_headers_iteration() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test Response with Headers
#[tokio::test]
async fn test_response_with_headers() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    // Check headers were extracted correctly
    let custom_header = response
        .headers
        .iter()
        .find(|(k, _)| k == "x-custom-header");
    assert!(custom_header.is_some());
    assert_eq!(custom_header.unwrap().1, "test-value");
}

/// Test Headers delete
#[tokio::test]
async fn test_headers_delete() {
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
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}
