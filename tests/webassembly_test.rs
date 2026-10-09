//! WebAssembly.compile answers through a task V8 posts from a background
//! thread. The guest here has no timer and no I/O, so only that task can wake
//! the event loop.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, HttpResponse, RequestBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
use tokio::sync::oneshot;

const SCRIPT: &str = r#"
    addEventListener('fetch', e => e.respondWith((async () => {
        const header = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);
        const module = await WebAssembly.compile(header);

        return new Response(String(module instanceof WebAssembly.Module));
    })()));
"#;

/// Far above the answer, far below the wall clock limit.
const FAST: Duration = Duration::from_secs(1);

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        max_wall_clock_time_ms: 5_000,
        ..RuntimeLimits::default()
    }
}

fn get() -> (Event, oneshot::Receiver<HttpResponse>) {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
}

async fn body(response: HttpResponse) -> String {
    let bytes = response.body.collect().await.unwrap().unwrap();

    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_answers_after_webassembly_compile() {
    run_in_local(|| async {
        let mut worker = Worker::new(Script::new(SCRIPT), Some(limits()))
            .await
            .unwrap();
        let (event, rx) = get();
        let started = Instant::now();

        worker.exec(event).await.unwrap();

        assert_eq!(body(rx.await.unwrap()).await, "true");
        assert!(started.elapsed() < FAST, "took {:?}", started.elapsed());
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_pool_answers_after_webassembly_compile() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: limits(),
    });

    run_in_local(|| async {
        let (event, rx) = get();
        let started = Instant::now();

        execute_pinned(PinnedExecuteRequest {
            owner_id: "owner".to_string(),
            worker_id: "webassembly".to_string(),
            version: 1,
            script: Script::new(SCRIPT),
            ops: Arc::new(DefaultOps),
            task: event,
            on_warm_hit: None,
            env_updated_at: None,
            abort: None,
            on_report: None,
        })
        .await
        .unwrap();

        assert_eq!(body(rx.await.unwrap()).await, "true");
        assert!(started.elapsed() < FAST, "took {:?}", started.elapsed());
    })
    .await;
}
