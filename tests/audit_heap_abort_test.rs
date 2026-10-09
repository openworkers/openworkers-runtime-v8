//! Reproduction of an open bug found in an audit: a plain allocation loop of
//! ~32 KB arrays exhausts the fixed 2 MB headroom the near-heap-limit
//! callback grants before V8 unwinds to JS, and V8 aborts the whole process
//! with FatalProcessOutOfMemory instead of the request ending with
//! `MemoryLimit`. Ignored by default and kept in its own binary because a
//! failure kills the test process; run with `--ignored` to see it.

mod common;

use std::collections::HashMap;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::Worker;

#[tokio::test(flavor = "current_thread")]
#[ignore = "open bug, see the doc comment"]
async fn small_allocations_end_with_memory_limit_not_a_process_abort() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const a = [];
                for (;;) a.push(new Array(4096).fill(1.5));
            });
        "#;
        let limits = RuntimeLimits {
            heap_initial_mb: 8,
            heap_max_mb: 32,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 20_000,
            ..Default::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();
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
    })
    .await;
}
