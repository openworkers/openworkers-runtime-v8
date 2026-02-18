//! Stress test for concurrent worker snapshot loading.
//!
//! Reproduces the `StringForwardingTable::GetRawHash` crash that occurs when
//! multiple isolates are created from the same worker snapshot concurrently.
//!
//! The crash happens during `Isolate::new()` → `Snapshot::Initialize()` →
//! `SharedHeapDeserializer::DeserializeStringTable()` when strings in the
//! snapshot contain forwarding indices instead of real hashes.

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

/// Create a worker snapshot with a non-trivial JS worker.
///
/// Uses a realistic worker with string-heavy code to maximize the chance
/// of triggering string externalization during snapshot creation.
fn create_test_snapshot() -> Vec<u8> {
    let js_code = r#"
        // Simulate a realistic worker with string-heavy top-level code
        const routes = {
            '/': 'Welcome to the API',
            '/hello': 'Hello, World!',
            '/health': JSON.stringify({ status: 'ok', uptime: 12345 }),
            '/headers': 'will echo headers',
            '/echo': 'will echo body',
            '/time': new Date('2025-01-01').toISOString(),
            '/version': 'v1.2.3-snapshot-test',
            '/config': JSON.stringify({
                maxRetries: 3,
                timeout: 5000,
                endpoints: ['https://api.example.com', 'https://fallback.example.com'],
                features: { caching: true, compression: false, logging: true }
            }),
        };

        const corsHeaders = {
            'Access-Control-Allow-Origin': '*',
            'Access-Control-Allow-Methods': 'GET, POST, OPTIONS',
            'Access-Control-Allow-Headers': 'Content-Type, Authorization',
            'X-Powered-By': 'OpenWorkers',
            'X-Request-Id': 'snapshot-test',
        };

        globalThis.default = {
            async fetch(request) {
                const url = new URL(request.url);
                const path = url.pathname;

                if (request.method === 'OPTIONS') {
                    return new Response(null, { status: 204, headers: corsHeaders });
                }

                const body = routes[path] || 'Not Found: ' + path;
                const status = routes[path] ? 200 : 404;

                return new Response(body, {
                    status,
                    headers: { ...corsHeaders, 'Content-Type': 'text/plain' }
                });
            }
        };
    "#;

    let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, None).unwrap();
    snapshot.output
}

/// Sequential: create N workers from same snapshot, one at a time.
/// This is the baseline — should always work.
#[tokio::test(flavor = "current_thread")]
async fn test_snapshot_sequential_loading() {
    run_in_local(|| async {
        let snapshot_bytes = create_test_snapshot();

        for i in 0..10 {
            let script = Script::new(WorkerCode::snapshot(snapshot_bytes.clone()));
            let mut worker = Worker::new(script, None).await.unwrap();

            let (task, rx) = Event::fetch(make_get_request("http://localhost/hello"));
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            assert_eq!(response.status, 200, "request {i} failed");
            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "Hello, World!");
        }
    })
    .await;
}

/// Concurrent: create N workers from same snapshot on separate threads.
/// This is what the runner does — each thread loads the snapshot independently.
///
/// This is the test that reproduces the StringForwardingTable crash.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_concurrent_loading_threads() {
    let snapshot_bytes = Arc::new(create_test_snapshot());
    let mut handles = vec![];

    for i in 0..20 {
        let snapshot = snapshot_bytes.clone();

        let handle = tokio::task::spawn_blocking(move || {
            // Each thread creates its own tokio runtime + LocalSet (like the runner's worker pool)
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();

                local
                    .run_until(async {
                        let script = Script::new(WorkerCode::snapshot((*snapshot).clone()));
                        let mut worker = Worker::new(script, None).await.unwrap();

                        let (task, rx) = Event::fetch(make_get_request("http://localhost/version"));
                        worker.exec(task).await.unwrap();
                        let response = rx.await.unwrap();

                        assert_eq!(response.status, 200, "thread {i} failed");
                        let body = response.body.collect().await.unwrap();
                        assert_eq!(
                            std::str::from_utf8(&body).unwrap(),
                            "v1.2.3-snapshot-test",
                            "thread {i} wrong body"
                        );
                    })
                    .await;
            });
        });

        handles.push(handle);
    }

    // Wait for all threads to complete
    for (i, handle) in handles.into_iter().enumerate() {
        handle
            .await
            .unwrap_or_else(|e| panic!("thread {i} panicked: {e}"));
    }
}

/// Rapid sequential: create and destroy workers as fast as possible.
/// Tests that snapshot data isn't corrupted by isolate teardown.
#[tokio::test(flavor = "current_thread")]
async fn test_snapshot_rapid_create_destroy() {
    run_in_local(|| async {
        let snapshot_bytes = create_test_snapshot();

        for i in 0..50 {
            let script = Script::new(WorkerCode::snapshot(snapshot_bytes.clone()));
            let mut worker = Worker::new(script, None).await.unwrap();

            let url = if i % 3 == 0 {
                "http://localhost/health"
            } else if i % 3 == 1 {
                "http://localhost/hello"
            } else {
                "http://localhost/config"
            };

            let (task, rx) = Event::fetch(make_get_request(url));
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();
            assert_eq!(response.status, 200, "iteration {i} failed for {url}");

            // Worker is dropped here, isolate destroyed
        }
    })
    .await;
}

/// Multiple snapshots created, then loaded SEQUENTIALLY.
/// Isolates whether the crash is from snapshot creation or concurrent loading.
#[tokio::test(flavor = "current_thread")]
async fn test_multiple_snapshots_sequential_loading() {
    run_in_local(|| async {
        // Create 3 different snapshots (same as concurrent test)
        let workers_code = vec![
            r#"globalThis.default = { async fetch() { return new Response("worker-A"); } };"#,
            r#"globalThis.default = { async fetch() { return new Response("worker-B"); } };"#,
            r#"
                const msg = "worker-C-" + "dynamic";
                globalThis.default = { async fetch() { return new Response(msg); } };
            "#,
        ];

        let snapshots: Vec<Vec<u8>> = workers_code
            .iter()
            .map(|code| {
                openworkers_runtime_v8::create_worker_snapshot(code, None)
                    .unwrap()
                    .output
            })
            .collect();

        let expected = vec!["worker-A", "worker-B", "worker-C-dynamic"];

        // Load each sequentially, 3 rounds
        for round in 0..3 {
            for (idx, snapshot_bytes) in snapshots.iter().enumerate() {
                let script = Script::new(WorkerCode::snapshot(snapshot_bytes.clone()));
                let mut worker = Worker::new(script, None).await.unwrap();

                let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                worker.exec(task).await.unwrap();
                let response = rx.await.unwrap();

                assert_eq!(response.status, 200);
                let body = response.body.collect().await.unwrap();
                assert_eq!(
                    std::str::from_utf8(&body).unwrap(),
                    expected[idx],
                    "round {round} snapshot {idx} wrong body"
                );
            }
        }
    })
    .await;
}

/// Multiple different snapshots loaded concurrently.
/// Tests that different snapshot blobs don't interfere with each other.
///
/// NOTE: Snapshot creation (`create_worker_snapshot`) uses V8's SnapshotCreator
/// which modifies global V8 state. Creating snapshots concurrently with
/// `Isolate::new()` loading snapshots causes StringForwardingTable corruption.
/// The fix: serialize all snapshot creation through a mutex.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_multiple_snapshots_concurrent() {
    use std::sync::Mutex;
    static SNAPSHOT_MUTEX: Mutex<()> = Mutex::new(());

    // Create several different snapshots sequentially
    let workers_code = vec![
        r#"globalThis.default = { async fetch() { return new Response("worker-A"); } };"#,
        r#"globalThis.default = { async fetch() { return new Response("worker-B"); } };"#,
        r#"
            const msg = "worker-C-" + "dynamic";
            globalThis.default = { async fetch() { return new Response(msg); } };
        "#,
    ];

    let snapshots: Vec<Arc<Vec<u8>>> = workers_code
        .iter()
        .map(|code| {
            let snap = openworkers_runtime_v8::create_worker_snapshot(code, None).unwrap();
            Arc::new(snap.output)
        })
        .collect();

    let expected = vec!["worker-A", "worker-B", "worker-C-dynamic"];

    let mut handles = vec![];

    // Launch 5 instances of each snapshot concurrently
    for round in 0..5 {
        for (idx, snapshot) in snapshots.iter().enumerate() {
            let snapshot = snapshot.clone();
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
                            let script = Script::new(WorkerCode::snapshot((*snapshot).clone()));
                            let mut worker = Worker::new(script, None).await.unwrap();

                            let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                            worker.exec(task).await.unwrap();
                            let response = rx.await.unwrap();

                            assert_eq!(response.status, 200);
                            let body = response.body.collect().await.unwrap();
                            assert_eq!(
                                std::str::from_utf8(&body).unwrap(),
                                expected_body,
                                "round {round} snapshot {idx} wrong body"
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

/// Minimal reproduction: create 2 snapshots, then load ONLY the first one concurrently.
/// If this crashes, the act of creating a second snapshot corrupts state needed by Isolate::new().
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_two_snapshots_load_first_only() {
    // Create two snapshots
    let snap1 = openworkers_runtime_v8::create_worker_snapshot(
        r#"globalThis.default = { async fetch() { return new Response("snap1"); } };"#,
        None,
    )
    .unwrap();

    let _snap2 = openworkers_runtime_v8::create_worker_snapshot(
        r#"globalThis.default = { async fetch() { return new Response("snap2"); } };"#,
        None,
    )
    .unwrap();

    // Only load snap1 concurrently (snap2 is created but never loaded)
    let snapshot_bytes = Arc::new(snap1.output);
    let mut handles = vec![];

    for i in 0..10 {
        let snapshot = snapshot_bytes.clone();

        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();

                local
                    .run_until(async {
                        let script = Script::new(WorkerCode::snapshot((*snapshot).clone()));
                        let mut worker = Worker::new(script, None).await.unwrap();

                        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                        worker.exec(task).await.unwrap();
                        let response = rx.await.unwrap();

                        assert_eq!(response.status, 200);
                        let body = response.body.collect().await.unwrap();
                        assert_eq!(std::str::from_utf8(&body).unwrap(), "snap1", "thread {i}");
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

/// Minimal: 3 snapshots, load each on spawn_blocking SEQUENTIALLY (one at a time).
/// If this crashes, the issue is SnapshotCreator corrupting V8 global state,
/// NOT concurrent Isolate::new().
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multiple_snapshots_spawn_blocking_sequential() {
    let workers_code = vec![
        r#"globalThis.default = { async fetch() { return new Response("A"); } };"#,
        r#"globalThis.default = { async fetch() { return new Response("B"); } };"#,
        r#"globalThis.default = { async fetch() { return new Response("C"); } };"#,
    ];

    let snapshots: Vec<Vec<u8>> = workers_code
        .iter()
        .map(|code| {
            openworkers_runtime_v8::create_worker_snapshot(code, None)
                .unwrap()
                .output
        })
        .collect();

    let expected = vec!["A", "B", "C"];

    // Load each snapshot ONE AT A TIME on spawn_blocking
    for (idx, snapshot_bytes) in snapshots.iter().enumerate() {
        let snapshot = snapshot_bytes.clone();
        let exp = expected[idx].to_string();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();

                local
                    .run_until(async {
                        let script = Script::new(WorkerCode::snapshot(snapshot));
                        let mut worker = Worker::new(script, None).await.unwrap();

                        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                        worker.exec(task).await.unwrap();
                        let response = rx.await.unwrap();

                        assert_eq!(response.status, 200);
                        let body = response.body.collect().await.unwrap();
                        assert_eq!(std::str::from_utf8(&body).unwrap(), exp);
                    })
                    .await;
            });
        })
        .await
        .unwrap();
    }
}

/// Snapshot with env vars loaded concurrently.
/// Tests that env var injection doesn't cause snapshot corruption.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_snapshot_with_env_concurrent() {
    let js_code = r#"
        const key = globalThis.env?.SECRET || "no-secret";
        globalThis.default = {
            async fetch() {
                return new Response("secret=" + key);
            }
        };
    "#;

    let mut env = HashMap::new();
    env.insert("SECRET".to_string(), "hunter2".to_string());

    let snapshot = openworkers_runtime_v8::create_worker_snapshot(js_code, Some(&env)).unwrap();
    let snapshot_bytes = Arc::new(snapshot.output);

    let mut handles = vec![];

    for i in 0..15 {
        let snapshot = snapshot_bytes.clone();

        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async {
                let local = tokio::task::LocalSet::new();

                local
                    .run_until(async {
                        let script = Script::new(WorkerCode::snapshot((*snapshot).clone()));
                        let mut worker = Worker::new(script, None).await.unwrap();

                        let (task, rx) = Event::fetch(make_get_request("http://localhost/"));
                        worker.exec(task).await.unwrap();
                        let response = rx.await.unwrap();

                        assert_eq!(response.status, 200);
                        let body = response.body.collect().await.unwrap();
                        assert_eq!(
                            std::str::from_utf8(&body).unwrap(),
                            "secret=hunter2",
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
