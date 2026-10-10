//! A hit on the heap limit used to leave the isolate's limit raised by the
//! headroom the callback granted, so each hit on a long-lived isolate left
//! it larger than the last. The limit goes back to its configured value
//! after the event that hit it.

mod common;

use std::collections::HashMap;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::Worker;

fn heap_size_limit(worker: &mut Worker) -> usize {
    worker.with_isolate(|isolate, _| isolate.get_heap_statistics().heap_size_limit())
}

#[tokio::test(flavor = "current_thread")]
async fn the_heap_limit_does_not_ratchet_up_across_events() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const arrays = [];
                for (;;) arrays.push(new Array(131072).fill(1.5));
            });
        "#;
        let limits = RuntimeLimits {
            heap_initial_mb: 1,
            heap_max_mb: 64,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 20_000,
            ..Default::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();
        let configured = heap_size_limit(&mut worker);
        let mut after_hits = Vec::new();

        for _ in 0..3 {
            let (task, _rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: "http://localhost/".into(),
                headers: HashMap::new(),
                body: RequestBody::None,
            });
            let result = worker.exec(task).await;

            assert!(
                matches!(result, Err(TerminationReason::MemoryLimit)),
                "expected MemoryLimit, got {result:?}"
            );

            after_hits.push(heap_size_limit(&mut worker));
        }

        // The callback grants a quarter of the heap; none of it stays
        let headroom = 16 * 1024 * 1024;

        assert!(
            after_hits[0] < configured + headroom,
            "one hit left the limit at {} bytes, configured {configured}",
            after_hits[0]
        );
        assert!(
            after_hits.iter().all(|limit| *limit == after_hits[0]),
            "the limit moved across hits: {after_hits:?}"
        );
    })
    .await;
}
