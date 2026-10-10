//! A plain allocation loop of ~32 KB arrays used to exhaust the fixed 2 MB
//! headroom the near-heap-limit callback granted before V8 unwound to JS,
//! and V8 aborted the whole process with FatalProcessOutOfMemory instead
//! of the request ending with `MemoryLimit`. Kept in its own binary because
//! a regression kills the test process.

mod common;

use std::collections::HashMap;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::Worker;

#[tokio::test(flavor = "current_thread")]
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
