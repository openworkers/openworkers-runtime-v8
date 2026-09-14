//! A fetch answered with a null-body status must settle the promise.
//!
//! Response refuses a body on 101, 103, 204, 205 and 304. Handing it one throws
//! inside the native callback, which used to settle nothing at all: the guest's
//! promise stayed pending, its handler never returned, and the request held its
//! pool permit until the wall clock cut it off.

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
use std::time::Duration;
use tokio::time::timeout;

/// Answers every fetch the way a cache revalidation does.
struct StatusOps(u16);

impl OperationsHandler for StatusOps {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let status = self.0;

        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers: vec![],
                body: ResponseBody::None,
            })
        })
    }
}

async fn upstream_status_reaches_the_guest(status: u16) {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(
                fetch('https://example.com/asset.js').then((upstream) =>
                    new Response('status=' + upstream.status + ' body=' + (upstream.body === null))
                )
            );
        });
    "#;

    let ops: Arc<dyn OperationsHandler> = Arc::new(StatusOps(status));
    let mut worker = Worker::new_with_ops(Script::new(code), None, ops)
        .await
        .unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(req);

    let answered = timeout(Duration::from_secs(5), async {
        worker.exec(task).await.unwrap();
        rx.await.unwrap()
    })
    .await;

    let response = answered.unwrap_or_else(|_| {
        panic!("status {status} left the fetch promise pending instead of settling")
    });

    assert_eq!(response.status, 200);

    let body = response.body.collect().await.unwrap().unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body, format!("status={status} body=true"));
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_answered_with_304_settles() {
    run_in_local(|| async { upstream_status_reaches_the_guest(304).await }).await;
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_answered_with_204_settles() {
    run_in_local(|| async { upstream_status_reaches_the_guest(204).await }).await;
}
