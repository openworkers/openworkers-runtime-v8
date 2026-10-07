//! With max_cached_contexts at 0 the pool caches nothing: a context serves one
//! request and goes.

mod common;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, HttpResponse, OpFuture, OperationsHandler, RequestBody,
    ResponseBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, get_local_pool_stats, init_pinned_pool,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Holds each request on an outbound fetch, so 20 of them overlap.
struct Upstream;

impl OperationsHandler for Upstream {
    fn handle_fetch(&self, _request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;

            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::None,
            })
        })
    }
}

/// Answers how many requests this context has seen.
const SCRIPT: &str = r#"
    let seen = 0;
    addEventListener('fetch', (event) => {
        seen++;
        event.respondWith(fetch('https://upstream/').then(() => new Response(String(seen))));
    });
"#;

async fn serve() -> String {
    let (task, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    execute_pinned(PinnedExecuteRequest {
        owner_id: "tenant".to_string(),
        worker_id: "worker".to_string(),
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

    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn a_pool_without_a_cache_keeps_no_context() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 1,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 0,
            overcommit: false,
            max_context_reuses: 1000,
            limits: RuntimeLimits::default(),
        });

        for _ in 0..3 {
            assert_eq!(serve().await, "1");
        }

        let handles: Vec<_> = (0..20).map(|_| tokio::task::spawn_local(serve())).collect();

        for handle in handles {
            assert_eq!(handle.await.unwrap(), "1");
        }

        let stats = get_local_pool_stats().unwrap();
        assert_eq!(stats.total, 1, "the 20 requests shared one isolate");
        assert_eq!(stats.cached_contexts, 0);
    })
    .await;
}
