mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test(flavor = "current_thread")]
async fn test_blob_basic() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const blob = new Blob(['Hello, ', 'World!'], { type: 'text/plain' });

                const size = blob.size;
                const type = blob.type;
                const text = await blob.text();

                const result = size === 13 && type === 'text/plain' && text === 'Hello, World!'
                    ? 'OK' : `FAIL: size=${size}, type=${type}, text=${text}`;

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

#[tokio::test(flavor = "current_thread")]
async fn test_blob_array_buffer() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const blob = new Blob([new Uint8Array([1, 2, 3])]);
                const buffer = await blob.arrayBuffer();
                const bytes = new Uint8Array(buffer);

                const result = bytes[0] === 1 && bytes[1] === 2 && bytes[2] === 3
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

#[tokio::test(flavor = "current_thread")]
async fn test_blob_slice() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const blob = new Blob(['Hello, World!']);
                const sliced = blob.slice(0, 5);
                const text = await sliced.text();

                const result = text === 'Hello' ? 'OK' : `FAIL: ${text}`;
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

#[tokio::test(flavor = "current_thread")]
async fn test_file() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const file = new File(['content'], 'test.txt', {
                    type: 'text/plain',
                    lastModified: 1234567890
                });

                const name = file.name;
                const size = file.size;
                const type = file.type;
                const lastModified = file.lastModified;
                const text = await file.text();

                const result = name === 'test.txt'
                    && size === 7
                    && type === 'text/plain'
                    && lastModified === 1234567890
                    && text === 'content'
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
