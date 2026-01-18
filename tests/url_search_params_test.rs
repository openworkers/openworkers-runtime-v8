mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test(flavor = "current_thread")]
async fn test_url_search_params_basic() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const params = new URLSearchParams('foo=1&bar=2');

                const foo = params.get('foo');
                const bar = params.get('bar');
                const baz = params.get('baz');

                const result = foo === '1' && bar === '2' && baz === null
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
async fn test_url_search_params_methods() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const params = new URLSearchParams();

                // append
                params.append('a', '1');
                params.append('a', '2');

                // getAll
                const all = params.getAll('a');

                // has
                const hasA = params.has('a');
                const hasB = params.has('b');

                // set (replaces all)
                params.set('a', '3');
                const afterSet = params.getAll('a');

                // delete
                params.delete('a');
                const afterDelete = params.has('a');

                const result = all.length === 2
                    && hasA === true
                    && hasB === false
                    && afterSet.length === 1
                    && afterSet[0] === '3'
                    && afterDelete === false
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
async fn test_url_search_params_iteration() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const params = new URLSearchParams('a=1&b=2');

                const entries = [...params.entries()];
                const keys = [...params.keys()];
                const values = [...params.values()];

                let forEachResult = '';
                params.forEach((v, k) => { forEachResult += k + v; });

                const result = entries.length === 2
                    && keys.join(',') === 'a,b'
                    && values.join(',') === '1,2'
                    && forEachResult === 'a1b2'
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
async fn test_url_with_search_params() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const url = new URL('https://example.com/path?foo=bar&num=42');

                const foo = url.searchParams.get('foo');
                const num = url.searchParams.get('num');
                const pathname = url.pathname;
                const search = url.search;

                const result = foo === 'bar'
                    && num === '42'
                    && pathname === '/path'
                    && search === '?foo=bar&num=42'
                    ? 'OK' : `FAIL: foo=${foo}, num=${num}`;

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
