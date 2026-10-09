//! The CPU limit of requests that share an isolate. Each request is charged
//! the CPU of its own JS only: two requests under the limit both answer, and
//! the one over it dies, not the one that runs beside it.
//!
//! The pool config is for the whole process, so these tests have a file of
//! their own.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, HttpResponse, RequestBody, RuntimeLimits, Script,
    TerminationReason,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, execute_pinned, init_pinned_pool,
};
use tokio::sync::oneshot;

const CPU_LIMIT_MS: u64 = 100;

/// Burns `ms` of CPU per slice, for `slices` slices (forever when absent),
/// with a timer between slices so that another request can run.
const BURNER: &str = r#"
    const burn = (ms) => {
        const end = Date.now() + ms;
        let x = 0;
        while (Date.now() < end) x += Math.sqrt(x + 1);
        return x;
    };

    addEventListener('fetch', (e) => e.respondWith((async () => {
        const params = new URL(e.request.url).searchParams;
        const ms = Number(params.get('ms'));
        const slices = params.has('slices') ? Number(params.get('slices')) : Infinity;

        for (let i = 0; i < slices; i++) {
            burn(ms);
            await new Promise((resolve) => setTimeout(resolve, 0));
        }

        return new Response('done');
    })()));
"#;

/// Twenty requests may share an isolate, as ISOLATE_MAX_CONCURRENT=20 in the
/// runner image; the wall clock is far above the CPU limit, so it is the CPU
/// limit that stops a request.
fn init_pool() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 20,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: RuntimeLimits {
            max_cpu_time_ms: CPU_LIMIT_MS,
            max_wall_clock_time_ms: 5_000,
            ..Default::default()
        },
    });
}

fn request(
    worker_id: &str,
    code: &str,
    query: &str,
) -> (PinnedExecuteRequest, oneshot::Receiver<HttpResponse>) {
    let (task, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://localhost/?{query}"),
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

/// Each spends 60 ms of CPU, under the 100 ms limit; together they spend 120.
#[tokio::test(flavor = "current_thread")]
async fn two_requests_under_the_limit_both_answer() {
    init_pool();

    run_in_local(|| async {
        let (a, a_rx) = request("honest", BURNER, "ms=6&slices=10");
        let (b, b_rx) = request("honest", BURNER, "ms=6&slices=10");

        let (a_done, b_done) = tokio::join!(execute_pinned(a), execute_pinned(b));

        assert_eq!((a_done, b_done), (Ok(()), Ok(())));
        assert_eq!(body(a_rx).await, "done");
        assert_eq!(body(b_rx).await, "done");
    })
    .await;
}

/// One request burns CPU without end; the other spends 30 ms beside it.
#[tokio::test(flavor = "current_thread")]
async fn the_request_over_the_limit_dies_and_not_its_neighbour() {
    init_pool();

    run_in_local(|| async {
        let (guilty, _guilty_rx) = request("shared", BURNER, "ms=6");
        let (innocent, innocent_rx) = request("shared", BURNER, "ms=3&slices=10");
        let started = Instant::now();

        let (guilty_done, innocent_done) =
            tokio::join!(execute_pinned(guilty), execute_pinned(innocent));

        assert_eq!(guilty_done, Err(TerminationReason::CpuTimeLimit));
        assert_eq!(innocent_done, Ok(()));
        assert_eq!(body(innocent_rx).await, "done");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the CPU limit, not the wall clock, ends it: {:?}",
            started.elapsed()
        );
    })
    .await;
}

/// One request burns CPU without end; nineteen spend 20 ms each beside it,
/// 380 ms in all, far above the limit of one request.
#[tokio::test(flavor = "current_thread")]
async fn nineteen_neighbours_of_a_burner_all_answer() {
    init_pool();

    run_in_local(|| async {
        let (guilty, _guilty_rx) = request("crowd", BURNER, "ms=6");
        let started = Instant::now();
        let neighbours: Vec<_> = (0..19)
            .map(|_| request("crowd", BURNER, "ms=2&slices=10"))
            .collect();
        let (requests, receivers): (Vec<_>, Vec<_>) = neighbours.into_iter().unzip();

        let (guilty_done, neighbours_done) = tokio::join!(
            execute_pinned(guilty),
            futures::future::join_all(requests.into_iter().map(execute_pinned))
        );

        assert_eq!(guilty_done, Err(TerminationReason::CpuTimeLimit));
        assert!(
            neighbours_done.iter().all(Result::is_ok),
            "{neighbours_done:?}"
        );

        for rx in receivers {
            assert_eq!(body(rx).await, "done");
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the CPU limit, not the wall clock, ends it: {:?}",
            started.elapsed()
        );
    })
    .await;
}

/// A loop that never yields is cut while it runs, which needs the per-thread
/// CPU timer of Linux.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn a_loop_that_never_yields_is_cut_at_the_limit() {
    init_pool();

    run_in_local(|| async {
        let code = "addEventListener('fetch', () => { while (true) {} });";
        let (looping, _rx) = request("loop", code, "");
        let started = Instant::now();

        assert_eq!(
            execute_pinned(looping).await,
            Err(TerminationReason::CpuTimeLimit)
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cut at the CPU limit: {:?}",
            started.elapsed()
        );
    })
    .await;
}
