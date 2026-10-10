//! Every form of HeadersInit that fetch takes is validated once, by Headers.
//!
//! __normalizeFetchInput used to pass a plain object or an array of pairs raw
//! to the host: the array was refused, a number was refused, and a value that
//! holds a CR or LF went to the runner as it was.

mod common;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::HttpResponse;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::ResponseBody;
use openworkers_core::Script;
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::sync::Arc;

/// Answers every fetch with the headers it carried, sorted, one per line.
struct Echo;

impl OperationsHandler for Echo {
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let mut lines: Vec<String> = request
            .headers
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        lines.sort();

        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Bytes(lines.join("\n").into()),
            })
        })
    }
}

/// Runs `expr` in a fetch handler and answers what it resolved to, or what it
/// threw as `threw <name>: <message>`.
async fn outcome_of(expr: &str) -> String {
    let code = format!(
        r#"
        addEventListener('fetch', async (event) => {{
            let out;
            try {{
                out = await ({expr});
            }} catch (e) {{
                out = 'threw ' + e.name + ': ' + e.message;
            }}
            event.respondWith(new Response(String(out)));
        }});
        "#
    );

    run_in_local(|| async move {
        let ops: Arc<dyn OperationsHandler> = Arc::new(Echo);
        let mut worker = Worker::new_with_ops(Script::new(code), None, ops)
            .await
            .unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_array_of_pairs_is_accepted() {
    let out = outcome_of(
        "fetch('https://upstream/', { headers: [['a', 'b'], ['X-Two', '2']] }).then((r) => r.text())",
    )
    .await;

    assert_eq!(out, "a=b\nx-two=2");
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_object_is_validated() {
    let out = outcome_of("fetch('https://upstream/', { headers: { x: 'a\\r\\nb' } })").await;

    assert!(out.starts_with("threw TypeError:"), "got: {out}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_number_value_becomes_its_string() {
    let out =
        outcome_of("fetch('https://upstream/', { headers: { 'x-n': 5 } }).then((r) => r.text())")
            .await;

    assert_eq!(out, "x-n=5");
}

#[tokio::test(flavor = "current_thread")]
async fn a_headers_instance_still_works() {
    let out = outcome_of(
        "fetch('https://upstream/', { headers: new Headers({ 'Content-Type': 'text/plain' }) }).then((r) => r.text())",
    )
    .await;

    assert_eq!(out, "content-type=text/plain");
}

#[tokio::test(flavor = "current_thread")]
async fn a_request_carries_its_own_headers() {
    let out = outcome_of(
        "fetch(new Request('https://upstream/', { headers: [['via', 'request']] })).then((r) => r.text())",
    )
    .await;

    assert_eq!(out, "via=request");
}
