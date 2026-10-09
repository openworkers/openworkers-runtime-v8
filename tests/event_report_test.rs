//! The heap a pool event reports: what the JS heap of its isolate uses when
//! the event ends.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::{
    EventReport, PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, init_pinned_pool,
};

const MIB: usize = 1024 * 1024;

/// Each event keeps 16 MiB of arrays more, and drops 32 MiB of garbage.
const GROWING: &str = r#"
    globalThis.kept = [];
    addEventListener('fetch', (e) => {
        for (let i = 0; i < 16; i++) globalThis.kept.push(new Array(131072).fill(1.5));
        for (let i = 0; i < 32; i++) new Array(131072).fill(2.5);
        e.respondWith(new Response(String(globalThis.kept.length)));
    });
"#;

async fn report(code: &str) -> EventReport {
    let seen = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);

    let (task, _rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    execute_pinned(PinnedExecuteRequest {
        owner_id: "owner".to_string(),
        worker_id: "growing".to_string(),
        version: 1,
        script: Script::new(code),
        ops: Arc::new(DefaultOps),
        task,
        on_warm_hit: None,
        env_updated_at: None,
        abort: None,
        on_report: Some(Box::new(move |report| {
            *sink.lock().unwrap() = Some(report);
        })),
    })
    .await
    .unwrap();

    let report = seen.lock().unwrap().take();

    report.expect("the pool reports each event")
}

#[tokio::test(flavor = "current_thread")]
async fn an_event_reports_the_heap_its_isolate_keeps() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: Default::default(),
    });

    run_in_local(|| async {
        let mut reports = Vec::new();

        // Warm hits: the same context keeps its arrays, 16 MiB per event
        for _ in 0..4 {
            reports.push(report(GROWING).await);
        }

        let used: Vec<usize> = reports.iter().map(|r| r.heap_used_bytes / MIB).collect();

        // 16 MiB more per event; the garbage is collected or not
        assert!(used.windows(2).all(|w| w[1] > w[0]), "{used:?} MiB");
        assert!((64..128).contains(&used[3]), "{used:?} MiB");
    })
    .await;
}
