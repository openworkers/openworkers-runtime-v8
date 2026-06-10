//! WebSocket client API conformance in the real V8 isolate.
//!
//! This exercises the WHATWG-conformance logic that runs in-isolate (URL
//! validation via OW's URL, error types via OW's DOMException, readyState
//! constants, the internal __adopt path) WITHOUT any network: every assertion
//! either throws during construction (before the native connect) or operates on
//! an adopted handle (which performs no native call). The real wire round-trip
//! is covered separately (the Brumal end-to-end test).

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test(flavor = "current_thread")]
async fn websocket_client_api_conformance() {
    run_in_local(|| async {
        let code = r#"
            function throwsName(fn, name) {
                try { fn(); return false; } catch (e) { return e && e.name === name; }
            }

            addEventListener('fetch', (event) => {
                const checks = [];
                const check = (name, cond) => checks.push([name, !!cond]);

                // The class exists with the right ready-state constants.
                check('typeof WebSocket === function', typeof WebSocket === 'function');
                check('WebSocket.CONNECTING === 0', WebSocket.CONNECTING === 0);
                check('WebSocket.OPEN === 1', WebSocket.OPEN === 1);
                check('WebSocket.CLOSING === 2', WebSocket.CLOSING === 2);
                check('WebSocket.CLOSED === 3', WebSocket.CLOSED === 3);

                // Construction validation (throws synchronously, before any connect).
                check('no-arg -> TypeError', throwsName(() => new WebSocket(), 'TypeError'));
                check('fragment -> SyntaxError', throwsName(() => new WebSocket('wss://h/p#frag'), 'SyntaxError'));
                check('ftp scheme -> SyntaxError', throwsName(() => new WebSocket('ftp://h/'), 'SyntaxError'));
                check('garbage url -> SyntaxError', throwsName(() => new WebSocket('not a url'), 'SyntaxError'));

                // __adopt path (fetch Upgrade): no native call, already OPEN.
                const a = WebSocket.__adopt(1);
                check('adopt -> OPEN', a.readyState === 1);
                check('instance OPEN constant', a.OPEN === 1);
                check('binaryType default arraybuffer', a.binaryType === 'arraybuffer');
                check('has addEventListener', typeof a.addEventListener === 'function');

                // close() validation throws before touching the native binding.
                check('close(500) -> InvalidAccessError', throwsName(() => WebSocket.__adopt(2).close(500), 'InvalidAccessError'));
                check('close long reason -> SyntaxError', throwsName(() => WebSocket.__adopt(3).close(1000, 'x'.repeat(124)), 'SyntaxError'));

                const failed = checks.filter((c) => !c[1]).map((c) => c[0]);
                event.respondWith(new Response(JSON.stringify({ ok: failed.length === 0, failed }), {
                    status: failed.length === 0 ? 200 : 500,
                    headers: { 'content-type': 'application/json' },
                }));
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

        let response = timeout(Duration::from_secs(5), async {
            worker.exec(task).await.unwrap();
            rx.await.unwrap()
        })
        .await
        .expect("worker should complete quickly (no network in this test)");

        let body = response.body.collect().await.unwrap();
        let body_str = String::from_utf8_lossy(&body);

        assert_eq!(
            response.status, 200,
            "WebSocket conformance checks failed: {}",
            body_str
        );
    })
    .await;
}
