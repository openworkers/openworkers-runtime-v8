use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test]
async fn test_structured_clone_basic() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const original = { a: 1, b: { c: 2 }, arr: [1, 2, 3] };
            const cloned = structuredClone(original);

            // Modify original
            original.a = 999;
            original.b.c = 999;
            original.arr.push(4);

            // Cloned should be unchanged
            const result = cloned.a === 1
                && cloned.b.c === 2
                && cloned.arr.length === 3
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
}

#[tokio::test]
async fn test_structured_clone_types() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Test various types
            const date = new Date('2024-01-01');
            const map = new Map([['a', 1]]);
            const set = new Set([1, 2, 3]);
            const arr = new Uint8Array([1, 2, 3]);

            const obj = { date, map, set, arr };
            const cloned = structuredClone(obj);

            const dateOk = cloned.date instanceof Date && cloned.date.getTime() === date.getTime();
            const mapOk = cloned.map instanceof Map && cloned.map.get('a') === 1;
            const setOk = cloned.set instanceof Set && cloned.set.has(2);
            const arrOk = cloned.arr instanceof Uint8Array && cloned.arr[0] === 1;

            const result = dateOk && mapOk && setOk && arrOk ? 'OK' : 'FAIL';
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
}

#[tokio::test]
async fn test_structured_clone_circular() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Test circular reference
            const obj = { a: 1 };
            obj.self = obj;

            const cloned = structuredClone(obj);

            const result = cloned.a === 1 && cloned.self === cloned ? 'OK' : 'FAIL';
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
}
