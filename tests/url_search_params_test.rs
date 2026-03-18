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

fn make_req() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

async fn run_url_test(code: &'static str) -> String {
    run_in_local(|| async {
        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();
        let (task, rx) = Event::fetch(make_req());
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();
        let body = &response.body.collect().await.unwrap();
        std::str::from_utf8(body).unwrap().to_string()
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_url_basic() {
    let result = run_url_test(
        r#"
        addEventListener('fetch', async (event) => {
            const u = new URL('https://example.com/path?q=1#frag');
            const ok = u.protocol === 'https:'
                && u.hostname === 'example.com'
                && u.host === 'example.com'
                && u.port === ''
                && u.pathname === '/path'
                && u.search === '?q=1'
                && u.hash === '#frag'
                && u.origin === 'https://example.com';
            event.respondWith(new Response(ok ? 'OK' : JSON.stringify({
                protocol: u.protocol, hostname: u.hostname, host: u.host,
                port: u.port, pathname: u.pathname, search: u.search,
                hash: u.hash, origin: u.origin
            })));
        });
    "#,
    )
    .await;
    assert_eq!(result, "OK");
}

#[tokio::test(flavor = "current_thread")]
async fn test_url_userinfo() {
    // Regression: hostname was 'user' instead of 'example.com' due to broken host parsing
    let result = run_url_test(
        r#"
        addEventListener('fetch', async (event) => {
            const u = new URL('http://user:pass@example.com/path');
            const ok = u.hostname === 'example.com'
                && u.host === 'example.com'
                && u.username === 'user'
                && u.password === 'pass'
                && u.pathname === '/path';
            event.respondWith(new Response(ok ? 'OK' : JSON.stringify({
                hostname: u.hostname, host: u.host,
                username: u.username, password: u.password
            })));
        });
    "#,
    )
    .await;
    assert_eq!(result, "OK");
}

#[tokio::test(flavor = "current_thread")]
async fn test_url_userinfo_with_port() {
    let result = run_url_test(
        r#"
        addEventListener('fetch', async (event) => {
            const u = new URL('https://admin:s3cr3t@api.example.com:8080/v1');
            const ok = u.hostname === 'api.example.com'
                && u.host === 'api.example.com:8080'
                && u.port === '8080'
                && u.username === 'admin'
                && u.password === 's3cr3t'
                && u.pathname === '/v1';
            event.respondWith(new Response(ok ? 'OK' : JSON.stringify({
                hostname: u.hostname, host: u.host, port: u.port,
                username: u.username, password: u.password
            })));
        });
    "#,
    )
    .await;
    assert_eq!(result, "OK");
}

#[tokio::test(flavor = "current_thread")]
async fn test_url_custom_scheme() {
    // Regression: postgresql:// was not parsed (only https? was supported)
    let result = run_url_test(
        r#"
        addEventListener('fetch', async (event) => {
            const u = new URL('postgresql://dbuser:dbpass@db.example.com/mydb');
            const ok = u.protocol === 'postgresql:'
                && u.hostname === 'db.example.com'
                && u.username === 'dbuser'
                && u.password === 'dbpass'
                && u.pathname === '/mydb';
            event.respondWith(new Response(ok ? 'OK' : JSON.stringify({
                protocol: u.protocol, hostname: u.hostname,
                username: u.username, password: u.password, pathname: u.pathname
            })));
        });
    "#,
    )
    .await;
    assert_eq!(result, "OK");
}

#[tokio::test(flavor = "current_thread")]
async fn test_url_no_userinfo() {
    let result = run_url_test(
        r#"
        addEventListener('fetch', async (event) => {
            const u = new URL('https://example.com:3000/path');
            const ok = u.hostname === 'example.com'
                && u.host === 'example.com:3000'
                && u.port === '3000'
                && u.username === ''
                && u.password === '';
            event.respondWith(new Response(ok ? 'OK' : JSON.stringify({
                hostname: u.hostname, host: u.host, port: u.port
            })));
        });
    "#,
    )
    .await;
    assert_eq!(result, "OK");
}
