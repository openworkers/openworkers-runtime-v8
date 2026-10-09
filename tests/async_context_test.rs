//! AsyncContext and AsyncLocalStorage carried across what a worker awaits:
//! promises, await, timers and microtasks; kept apart between concurrent
//! chains, between requests on a warm context, and between requests that
//! share an isolate.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::run_in_local;
use openworkers_core::{
    DefaultOps, Event, HttpMethod, HttpRequest, HttpResponse, RequestBody, Script,
};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
use tokio::sync::oneshot;

fn get(url: &str) -> (Event, oneshot::Receiver<HttpResponse>) {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
}

async fn text(rx: oneshot::Receiver<HttpResponse>) -> String {
    let bytes = rx.await.unwrap().body.collect().await.unwrap().unwrap();

    String::from_utf8_lossy(&bytes).into_owned()
}

/// Serves one GET through a Worker and answers the body.
async fn fetch(code: &str) -> String {
    let code = code.to_string();

    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), None).await.unwrap();
        let (event, rx) = get("http://localhost/");
        worker.exec(event).await.unwrap();

        text(rx).await
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn the_store_crosses_await_timers_and_microtasks() {
    let code = r#"
        const als = new AsyncLocalStorage();
        const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

        export default {
            fetch() {
                return als.run('request-1', async () => {
                    const seen = [als.getStore()];
                    await sleep(5);
                    seen.push(als.getStore());
                    await new Promise((resolve) => setTimeout(() => { seen.push(als.getStore()); resolve(); }, 5));
                    await new Promise((resolve) => queueMicrotask(() => { seen.push(als.getStore()); resolve(); }));
                    await Promise.resolve().then(() => seen.push(als.getStore()));
                    return new Response(seen.join(','));
                });
            },
        };
    "#;

    assert_eq!(
        fetch(&code.replace("export default", "globalThis.default =")).await,
        "request-1,request-1,request-1,request-1,request-1"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_chains_keep_their_own_store() {
    let code = r#"
        const als = new AsyncLocalStorage();
        const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

        async function chain(name, first, second) {
            return als.run(name, async () => {
                await sleep(first);
                const a = als.getStore();
                await sleep(second);
                return a + '/' + als.getStore();
            });
        }

        globalThis.default = {
            async fetch() {
                // Interleaved: a wakes at 5 and 25, b at 10 and 15
                const results = await Promise.all([chain('a', 5, 20), chain('b', 10, 5)]);
                return new Response(results.join(',') + ',' + String(als.getStore()));
            },
        };
    "#;

    assert_eq!(fetch(code).await, "a/a,b/b,undefined");
}

#[tokio::test(flavor = "current_thread")]
async fn async_context_variable_and_snapshot() {
    let code = r#"
        const v = new AsyncContext.Variable({ name: 'v', defaultValue: 'none' });

        globalThis.default = {
            async fetch() {
                const snapshot = v.run('captured', () => new AsyncContext.Snapshot());
                const later = await v.run('outer', async () => {
                    await null;
                    return [v.get(), snapshot.run(() => v.get())];
                });
                return new Response([v.name, v.get(), ...later].join(','));
            },
        };
    "#;

    assert_eq!(fetch(code).await, "v,none,outer,captured");
}

/// One pool config for every test here: the first init_pinned_pool of the
/// process is the one that holds. Two requests may share an isolate.
fn init_pool() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 2,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: Default::default(),
    });
}

fn pinned(
    worker_id: &str,
    code: &str,
    url: &str,
) -> (PinnedExecuteRequest, oneshot::Receiver<HttpResponse>) {
    let (task, rx) = get(url);

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
        on_report: None,
    };

    (request, rx)
}

/// The pool keeps the context warm; a store the first request entered must
/// not reach the second.
#[tokio::test(flavor = "current_thread")]
async fn a_warm_context_starts_each_request_without_a_store() {
    init_pool();

    let code = r#"
        const als = new AsyncLocalStorage();

        globalThis.default = {
            async fetch() {
                const before = String(als.getStore());
                als.enterWith('left by an earlier request');
                await null;
                return new Response(before);
            },
        };
    "#;

    run_in_local(|| async {
        for _ in 0..2 {
            let (request, rx) = pinned("warm", code, "http://localhost/");
            execute_pinned(request).await.unwrap();

            assert_eq!(text(rx).await, "undefined");
        }
    })
    .await;
}

/// Two requests share one isolate and interleave on its timers; each sees
/// only its own store.
#[tokio::test(flavor = "current_thread")]
async fn requests_that_share_an_isolate_keep_their_own_store() {
    init_pool();

    let code = r#"
        const als = new AsyncLocalStorage();
        const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

        globalThis.default = {
            fetch(request) {
                const name = new URL(request.url).pathname.slice(1);
                const wait = name === 'a' ? [5, 30] : [15, 5];

                return als.run(name, async () => {
                    await sleep(wait[0]);
                    const first = als.getStore();
                    await sleep(wait[1]);
                    return new Response(first + '/' + als.getStore());
                });
            },
        };
    "#;

    run_in_local(|| async {
        let (a, a_rx) = pinned("shared", code, "http://localhost/a");
        let (b, b_rx) = pinned("shared", code, "http://localhost/b");

        let (a_done, b_done) = tokio::join!(execute_pinned(a), execute_pinned(b));
        a_done.unwrap();
        b_done.unwrap();

        assert_eq!(text(a_rx).await, "a/a");
        assert_eq!(text(b_rx).await, "b/b");
    })
    .await;
}
