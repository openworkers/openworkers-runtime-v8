//! The memory limit of requests that share an isolate. The request that
//! allocates past the limit ends with MemoryLimit; a request beside it that
//! ends later answers.
//!
//! The pool config is for the whole process, so these tests have a file of
//! their own.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, HttpResponse, RequestBody, RuntimeLimits, Script,
    TerminationReason,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, init_pinned_pool,
};
use tokio::sync::oneshot;

/// Keeps 1 MiB buffers without end, with a timer between them.
const BUFFERS: &str = r#"
    addEventListener('fetch', (e) => e.respondWith((async () => {
        const kept = [];
        for (;;) {
            kept.push(new Uint8Array(1024 * 1024));
            await new Promise((resolve) => setTimeout(resolve, 0));
        }
    })()));
"#;

/// Keeps JS arrays without end, with a timer between them.
const OBJECTS: &str = r#"
    addEventListener('fetch', (e) => e.respondWith((async () => {
        const kept = [];
        for (let i = 0; ; i++) {
            kept.push(new Array(100000).fill(i));
            await new Promise((resolve) => setTimeout(resolve, 0));
        }
    })()));
"#;

/// Waits 20 timers of 10 ms, so it ends after its neighbour hits the limit.
const PATIENT: &str = r#"
    addEventListener('fetch', (e) => e.respondWith((async () => {
        for (let i = 0; i < 20; i++) {
            await new Promise((resolve) => setTimeout(resolve, 10));
        }
        return new Response('done');
    })()));
"#;

fn init_pool() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 2,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: RuntimeLimits {
            heap_initial_mb: 1,
            heap_max_mb: 16,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 5_000,
            ..Default::default()
        },
    });
}

fn request(worker_id: &str, code: &str) -> (PinnedExecuteRequest, oneshot::Receiver<HttpResponse>) {
    let (task, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    let request = PinnedExecuteRequest {
        owner_id: "owner".to_string(),
        worker_id: worker_id.to_string(),
        version: 1,
        script: Script::new(code),
        ops: Arc::new(DefaultOps),
        task,
        on_warm_hit: None,
        env_updated_at: None,
        abort: None,
        on_marks: None,
    };

    (request, rx)
}

async fn body(rx: oneshot::Receiver<HttpResponse>) -> String {
    let bytes = rx.await.unwrap().body.collect().await.unwrap().unwrap();

    String::from_utf8_lossy(&bytes).into_owned()
}

/// Runs `guilty` and the patient request on one isolate, and checks that
/// only `guilty` ends with MemoryLimit.
async fn only_the_guilty_request_hits_the_limit(worker_id: &str, guilty: &str) {
    init_pool();

    // Two workers of one owner: separate contexts on one isolate
    let (guilty, _guilty_rx) = request(&format!("{worker_id}-guilty"), guilty);
    let (patient, patient_rx) = request(&format!("{worker_id}-patient"), PATIENT);

    let (guilty_done, patient_done) = tokio::join!(execute_pinned(guilty), execute_pinned(patient));

    assert_eq!(guilty_done, Err(TerminationReason::MemoryLimit));
    assert_eq!(patient_done, Ok(()));
    assert_eq!(body(patient_rx).await, "done");
}

#[cfg(not(feature = "sandbox"))]
#[tokio::test(flavor = "current_thread")]
async fn array_buffers_past_the_limit_stop_their_request_only() {
    run_in_local(|| only_the_guilty_request_hits_the_limit("buffers", BUFFERS)).await;
}

#[tokio::test(flavor = "current_thread")]
async fn js_objects_past_the_heap_limit_stop_their_request_only() {
    run_in_local(|| only_the_guilty_request_hits_the_limit("objects", OBJECTS)).await;
}
