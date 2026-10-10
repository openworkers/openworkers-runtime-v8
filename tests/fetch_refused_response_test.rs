//! A fetch whose response the guest's Response refuses (a status outside
//! 200-599) rejects, and the body stream the runner opened for it goes with
//! it. It used to stay, with the pump filling it waiting on a reader that
//! never came, until the request ended.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, HttpResponse, OpFuture, OperationsHandler, RequestBody,
    ResponseBody, Script,
};
use openworkers_runtime_v8::Worker;

struct OddStatus;

impl OperationsHandler for OddStatus {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 600,
                headers: vec![],
                body: ResponseBody::Bytes(bytes::Bytes::from_static(b"never read")),
            })
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_response_closes_its_stream() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith(fetch('http://upstream/').then(
                    () => new Response('resolved'),
                    (error) => new Response('rejected: ' + error.message),
                ));
            });
        "#;
        let ops: Arc<dyn OperationsHandler> = Arc::new(OddStatus);
        let mut worker = Worker::new_with_ops(Script::new(code), None, ops)
            .await
            .unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".into(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        worker.exec(task).await.unwrap();

        let body = rx.await.unwrap().body.collect().await.unwrap().unwrap();
        let text = String::from_utf8_lossy(&body);

        assert!(text.starts_with("rejected: "), "got {text}");
        assert_eq!(
            worker.stream_manager().active_count(),
            0,
            "the refused response left its stream open"
        );
    })
    .await;
}
