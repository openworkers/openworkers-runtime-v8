//! Run: `cargo test --test code_cache_test`
//!
//! Tests for V8 code cache — the replacement for worker snapshots.
//!
//! Code cache stores compiled bytecode only (no heap objects, no shared string table).
//! This makes it thread-safe for concurrent loading of different scripts —
//! the exact scenario that crashes with heap snapshots.

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script, WorkerCode};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::sync::Arc;

fn make_get_request(url: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    }
}

/// Create a packed code cache bundle for given JS code.
fn create_test_code_cache(js_code: &str) -> Vec<u8> {
    let cache = openworkers_runtime_v8::create_code_cache(js_code).unwrap();
    openworkers_runtime_v8::pack_code_cache(js_code, &cache)
}

// ─── Pack / Unpack unit tests ───────────────────────────────────────────────

#[test]
fn test_pack_unpack_roundtrip() {
    let source = "console.log('hello');";
    let cache = openworkers_runtime_v8::create_code_cache(source).unwrap();
    let packed = openworkers_runtime_v8::pack_code_cache(source, &cache);

    assert!(openworkers_runtime_v8::is_code_cache(&packed));

    let (unpacked_source, unpacked_cache) =
        openworkers_runtime_v8::unpack_code_cache(&packed).unwrap();

    assert_eq!(unpacked_source, source);
    assert_eq!(unpacked_cache, &cache[..]);
}

#[test]
fn test_is_code_cache_rejects_old_snapshot() {
    // An old V8 heap snapshot won't start with our magic header
    let fake_snapshot = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00];
    assert!(!openworkers_runtime_v8::is_code_cache(&fake_snapshot));
}

#[test]
fn test_is_code_cache_rejects_too_short() {
    assert!(!openworkers_runtime_v8::is_code_cache(&[]));
    assert!(!openworkers_runtime_v8::is_code_cache(&[0xC0, 0xDE]));
}

// ─── Sequential loading ─────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn test_code_cache_sequential_loading() {
    run_in_local(|| async {
        let packed = create_test_code_cache(
            r#"
            globalThis.default = {
                async fetch(request) {
                    return new Response("code-cache-hello");
                }
            };
            "#,
        );

        for i in 0..10 {
            let script = Script::new(WorkerCode::Snapshot(packed.clone()));
            let mut worker = Worker::new(script, None).await.unwrap();

            let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            assert_eq!(response.status, 200, "request {i} failed");
            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "code-cache-hello");
        }
    })
    .await;
}

// ─── Concurrent loading — SAME code cache ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_code_cache_concurrent_same() {
    let packed = Arc::new(create_test_code_cache(
        r#"
        globalThis.default = {
            async fetch() { return new Response("same-cache"); }
        };
        "#,
    ));

    let mut handles = vec![];

    for i in 0..20 {
        let packed = packed.clone();

        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();

                local
                    .run_until(async {
                        let script = Script::new(WorkerCode::Snapshot((*packed).clone()));
                        let mut worker = Worker::new(script, None).await.unwrap();

                        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                        worker.exec(task).await.unwrap();
                        let response = rx.await.unwrap();

                        assert_eq!(response.status, 200);
                        let body = response.body.collect().await.unwrap();
                        assert_eq!(
                            std::str::from_utf8(&body).unwrap(),
                            "same-cache",
                            "thread {i} wrong body"
                        );
                    })
                    .await;
            });
        });

        handles.push(handle);
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .unwrap_or_else(|e| panic!("thread {i} panicked: {e}"));
    }
}

// ─── Concurrent loading — DIFFERENT code caches (the key test!) ─────────────
//
// This is the scenario that CRASHES with heap snapshots due to
// SharedHeapDeserializer::DeserializeStringTable not being thread-safe.
// Code cache has no shared heap objects, so this MUST NOT crash.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_code_cache_concurrent_different() {
    let workers_code = vec![
        r#"globalThis.default = { async fetch() { return new Response("worker-A"); } };"#,
        r#"globalThis.default = { async fetch() { return new Response("worker-B"); } };"#,
        r#"
            const msg = "worker-C-" + "dynamic";
            globalThis.default = { async fetch() { return new Response(msg); } };
        "#,
    ];

    let packed_caches: Vec<Arc<Vec<u8>>> = workers_code
        .iter()
        .map(|code| Arc::new(create_test_code_cache(code)))
        .collect();

    let expected = vec!["worker-A", "worker-B", "worker-C-dynamic"];

    let mut handles = vec![];

    // Launch 5 instances of each code cache concurrently (15 total)
    for round in 0..5 {
        for (idx, packed) in packed_caches.iter().enumerate() {
            let packed = packed.clone();
            let expected_body = expected[idx].to_string();

            let handle = tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let local = tokio::task::LocalSet::new();

                    local
                        .run_until(async {
                            let script = Script::new(WorkerCode::Snapshot((*packed).clone()));
                            let mut worker = Worker::new(script, None).await.unwrap();

                            let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                            worker.exec(task).await.unwrap();
                            let response = rx.await.unwrap();

                            assert_eq!(response.status, 200);
                            let body = response.body.collect().await.unwrap();
                            assert_eq!(
                                std::str::from_utf8(&body).unwrap(),
                                expected_body,
                                "round {round} cache {idx} wrong body"
                            );
                        })
                        .await;
                });
            });

            handles.push(handle);
        }
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .unwrap_or_else(|e| panic!("task {i} panicked: {e}"));
    }
}

// ─── Concurrent loading — realistic worker with string-heavy code ───────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_code_cache_concurrent_realistic() {
    let workers_code = vec![
        r#"
            const routes = {
                '/': 'Welcome to the API',
                '/hello': 'Hello, World!',
                '/health': JSON.stringify({ status: 'ok' }),
            };
            const corsHeaders = {
                'Access-Control-Allow-Origin': '*',
                'X-Worker': 'alpha',
            };
            globalThis.default = {
                async fetch(request) {
                    const url = new URL(request.url);
                    const body = routes[url.pathname] || 'Not Found';
                    const status = routes[url.pathname] ? 200 : 404;
                    return new Response(body, { status, headers: corsHeaders });
                }
            };
        "#,
        r#"
            const config = { version: "2.0", features: ["cache", "stream"] };
            globalThis.default = {
                async fetch() {
                    return new Response(JSON.stringify(config), {
                        headers: { 'Content-Type': 'application/json' }
                    });
                }
            };
        "#,
        r#"
            const counter = { value: 0 };
            globalThis.default = {
                async fetch() {
                    counter.value++;
                    return new Response("count=" + counter.value);
                }
            };
        "#,
    ];

    let packed_caches: Vec<Arc<Vec<u8>>> = workers_code
        .iter()
        .map(|code| Arc::new(create_test_code_cache(code)))
        .collect();

    let mut handles = vec![];

    for round in 0..3 {
        for (idx, packed) in packed_caches.iter().enumerate() {
            let packed = packed.clone();

            let handle = tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let local = tokio::task::LocalSet::new();

                    local
                        .run_until(async {
                            let script = Script::new(WorkerCode::Snapshot((*packed).clone()));
                            let mut worker = Worker::new(script, None).await.unwrap();

                            let (task, rx) =
                                Event::fetch(make_get_request("http://localhost/hello"));
                            worker.exec(task).await.unwrap();
                            let response = rx.await.unwrap();

                            // All workers should return 200 for valid routes
                            // or produce valid JSON, just verify no crash
                            assert!(
                                response.status == 200 || response.status == 404,
                                "round {round} worker {idx} unexpected status {}",
                                response.status
                            );
                        })
                        .await;
                });
            });

            handles.push(handle);
        }
    }

    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .unwrap_or_else(|e| panic!("task {i} panicked: {e}"));
    }
}
