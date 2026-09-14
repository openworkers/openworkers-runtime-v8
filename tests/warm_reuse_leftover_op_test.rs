//! A warm context must not let the previous request's answers reach the next.
//!
//! Callback ids used to restart at 1 on every reuse. An op the previous request
//! left in flight is cancelled when the next one begins, and its answer then
//! carried an id the next request had just handed to its own fetch.

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
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_runtime_v8::PinnedExecuteRequest;
use openworkers_runtime_v8::PinnedPoolConfig;
use openworkers_runtime_v8::execute_pinned;
use openworkers_runtime_v8::init_pinned_pool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Slow for one path, immediate for the other.
struct Upstream;

impl OperationsHandler for Upstream {
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let slow = request.url.ends_with("/slow");

        Box::pin(async move {
            if slow {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }

            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::None,
            })
        })
    }
}

const SCRIPT: &str = r#"
    addEventListener('fetch', (event) => {
        if (new URL(event.request.url).pathname === '/a') {
            fetch('https://upstream/slow').catch(() => {});
            event.respondWith(new Response('a'));
            return;
        }

        event.respondWith(
            fetch('https://upstream/fast').then((r) => new Response('b:' + r.status))
        );
    });
"#;

async fn serve(path: &str) -> (u16, String) {
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://localhost{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    };
    let (task, rx) = Event::fetch(request);

    execute_pinned(PinnedExecuteRequest {
        owner_id: "leftover".to_string(),
        worker_id: "leftover-worker".to_string(),
        version: 1,
        script: Script::new(SCRIPT),
        ops: Arc::new(Upstream),
        task,
        on_warm_hit: None,
        env_updated_at: None,
        abort: None,
    })
    .await
    .unwrap();

    let response = rx.await.unwrap();
    let body = response.body.collect().await.unwrap().unwrap();

    (response.status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "current_thread")]
async fn a_leftover_op_does_not_answer_the_next_request() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        assert_eq!(serve("/a").await, (200, "a".to_string()));

        let (status, body) = serve("/b").await;
        assert_eq!(
            (status, body.as_str()),
            (200, "b:200"),
            "the previous request's cancelled fetch answered this one's"
        );
    })
    .await;
}
