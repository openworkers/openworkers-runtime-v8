//! Realistic Benchmark - Simulates production workload with HTTP mocks
//!
//! This benchmark simulates real worker behavior:
//! - HTTP request handler (not just scheduled events)
//! - fetch() calls with simulated network latency
//! - Request/Response parsing
//! - Multiple workers competing for isolates
//!
//! Based on locker-example/src/http_pool.rs pattern.
//!
//! ## IMPORTANT: V8 Global State Conflicts
//!
//! V8 has internal global state that conflicts when multiple pool architectures
//! are initialized in the same process. Tests MUST be run with --test-threads=1
//! to avoid V8 internal mutex poisoning.
//!
//! Run with:
//!   cargo test --test realistic_bench -- --nocapture --test-threads=1
//!
//! Or run each architecture separately:
//!   cargo test --test realistic_bench bench_legacy -- --nocapture
//!   cargo test --test realistic_bench bench_shared -- --nocapture
//!   cargo test --test realistic_bench bench_pinned -- --nocapture

mod common;

use common::run_in_local;
use openworkers_core::{
    HttpMethod, HttpRequest, HttpResponse, LogLevel, OpFuture, OperationsHandler, RequestBody,
    ResponseBody, RuntimeLimits, Script, Task,
};
use openworkers_runtime_v8::{Worker, execute_pinned, execute_pooled, init_pinned_pool, init_pool};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ============================================================================
// V8 Global State Lock
// ============================================================================
//
// V8 has global state that conflicts when multiple pool architectures are
// initialized in the same process. This lock ensures tests run sequentially.

static V8_TEST_LOCK: Mutex<()> = Mutex::new(());

// ============================================================================
// Mock Operations Handler (simulates real I/O)
// ============================================================================

/// Stats for mock operations
#[derive(Debug, Default)]
struct MockStats {
    fetch_count: AtomicU64,
    total_fetch_latency_us: AtomicU64,
    log_count: AtomicU64,
}

/// Mock operations handler that simulates network latency
struct MockOps {
    /// Simulated fetch latency range (min, max) in ms
    fetch_latency_range: (u64, u64),
    /// Stats tracking
    stats: Arc<MockStats>,
}

impl MockOps {
    fn new(fetch_latency_min_ms: u64, fetch_latency_max_ms: u64) -> Self {
        Self {
            fetch_latency_range: (fetch_latency_min_ms, fetch_latency_max_ms),
            stats: Arc::new(MockStats::default()),
        }
    }

    fn stats(&self) -> Arc<MockStats> {
        Arc::clone(&self.stats)
    }
}

impl OperationsHandler for MockOps {
    fn handle_fetch(&self, request: HttpRequest) -> OpFuture<'_, Result<HttpResponse, String>> {
        let (min, max) = self.fetch_latency_range;
        let stats = Arc::clone(&self.stats);

        Box::pin(async move {
            // Simulate network latency
            let latency = if min == max {
                min
            } else {
                rand::rng().random_range(min..=max)
            };

            tokio::time::sleep(Duration::from_millis(latency)).await;

            // Track stats
            stats.fetch_count.fetch_add(1, Ordering::Relaxed);
            stats
                .total_fetch_latency_us
                .fetch_add(latency * 1000, Ordering::Relaxed);

            // Return mock response
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: ResponseBody::Bytes(
                    format!(
                        r#"{{"url":"{}","method":"{:?}","latency_ms":{}}}"#,
                        request.url, request.method, latency
                    )
                    .into(),
                ),
            })
        })
    }

    fn handle_log(&self, _level: LogLevel, _message: String) {
        self.stats.log_count.fetch_add(1, Ordering::Relaxed);
        // Don't print - too noisy for benchmarks
    }
}

// ============================================================================
// Realistic Worker Scripts
// ============================================================================

/// Simple HTTP handler - just returns a response
const SIMPLE_HANDLER: &str = r#"
    addEventListener('fetch', (event) => {
        event.respondWith(new Response('Hello World', {
            headers: { 'content-type': 'text/plain' }
        }));
    });
"#;

/// Handler that does a single fetch
const FETCH_HANDLER: &str = r#"
    addEventListener('fetch', async (event) => {
        event.respondWith(handleRequest(event.request));
    });

    async function handleRequest(request) {
        // Simulate calling an external API
        const apiResponse = await fetch('https://api.example.com/data');
        const data = await apiResponse.json();

        return new Response(JSON.stringify({
            received: data,
            processed: true
        }), {
            headers: { 'content-type': 'application/json' }
        });
    }
"#;

/// Handler that does multiple fetches (realistic scenario)
const MULTI_FETCH_HANDLER: &str = r#"
    addEventListener('fetch', async (event) => {
        event.respondWith(handleRequest(event.request));
    });

    async function handleRequest(request) {
        // Fetch user data and settings in parallel
        const [userRes, settingsRes] = await Promise.all([
            fetch('https://api.example.com/user'),
            fetch('https://api.example.com/settings')
        ]);

        const user = await userRes.json();
        const settings = await settingsRes.json();

        // Do some processing
        let result = { user, settings, computed: 0 };
        for (let i = 0; i < 1000; i++) {
            result.computed += i;
        }

        return new Response(JSON.stringify(result), {
            headers: { 'content-type': 'application/json' }
        });
    }
"#;

// ============================================================================
// Helper Functions
// ============================================================================

fn print_header(title: &str) {
    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║ {:^73} ║", title);
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
}

fn print_results(name: &str, elapsed: Duration, iterations: u32, stats: &MockStats) {
    let throughput = iterations as f64 / elapsed.as_secs_f64();
    let avg_latency = elapsed / iterations;
    let fetch_count = stats.fetch_count.load(Ordering::Relaxed);
    let total_fetch_us = stats.total_fetch_latency_us.load(Ordering::Relaxed);
    let avg_fetch_ms = if fetch_count > 0 {
        total_fetch_us as f64 / fetch_count as f64 / 1000.0
    } else {
        0.0
    };

    println!("\n  Results for {}:", name);
    println!("    Total time:      {:?}", elapsed);
    println!("    Throughput:      {:.2} req/s", throughput);
    println!("    Avg latency:     {:?}", avg_latency);
    println!("    Fetch calls:     {}", fetch_count);
    println!("    Avg fetch time:  {:.2}ms", avg_fetch_ms);
}

fn make_request() -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/test".to_string(),
        headers: HashMap::from([
            ("host".to_string(), "localhost".to_string()),
            ("user-agent".to_string(), "benchmark/1.0".to_string()),
        ]),
        body: RequestBody::None,
    }
}

// ============================================================================
// Legacy Benchmarks (Worker - new isolate per request)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_simple() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        let iterations = 30u32;

        print_header("Legacy (Worker) - Simple Handler");
        println!("  Config: {} iterations, no fetch calls", iterations);

        let ops = Arc::new(MockOps::new(0, 0));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_HANDLER);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Task::fetch(make_request());
            worker.exec(task).await.unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Legacy Simple", start.elapsed(), iterations, &stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_with_fetch() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        let iterations = 20u32;
        let fetch_latency_ms = 20u64;

        print_header("Legacy (Worker) - Handler with fetch()");
        println!(
            "  Config: {} iterations, fetch latency: {}ms",
            iterations, fetch_latency_ms
        );

        let ops = Arc::new(MockOps::new(fetch_latency_ms, fetch_latency_ms));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(FETCH_HANDLER);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Task::fetch(make_request());
            worker.exec(task).await.unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Legacy with Fetch", start.elapsed(), iterations, &stats);
    })
    .await;
}

// ============================================================================
// Shared Pool Benchmarks
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_simple() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 30u32;

        print_header("Shared Pool - Simple Handler");
        println!("  Config: {} iterations, no fetch calls", iterations);

        let ops = Arc::new(MockOps::new(0, 0));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("shared-simple-{}", i % 10);
            let script = Script::new(SIMPLE_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Shared Simple", start.elapsed(), iterations, &stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_with_fetch() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 20u32;
        let fetch_latency_ms = 20u64;

        print_header("Shared Pool - Handler with fetch()");
        println!(
            "  Config: {} iterations, fetch latency: {}ms",
            iterations, fetch_latency_ms
        );

        let ops = Arc::new(MockOps::new(fetch_latency_ms, fetch_latency_ms));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("shared-fetch-{}", i % 10);
            let script = Script::new(FETCH_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Shared with Fetch", start.elapsed(), iterations, &stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_multi_fetch() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 15u32;
        let fetch_latency_ms = 15u64;

        print_header("Shared Pool - Multi-fetch Handler (parallel fetches)");
        println!(
            "  Config: {} iterations, 2 parallel fetches, {}ms each",
            iterations, fetch_latency_ms
        );

        let ops = Arc::new(MockOps::new(fetch_latency_ms, fetch_latency_ms));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("shared-multi-{}", i % 5);
            let script = Script::new(MULTI_FETCH_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 5 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Shared Multi-Fetch", start.elapsed(), iterations, &stats);
    })
    .await;
}

// ============================================================================
// Thread-Pinned Pool Benchmarks
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_simple() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pinned_pool(100, RuntimeLimits::default());

        let iterations = 30u32;

        print_header("Thread-Pinned Pool - Simple Handler");
        println!("  Config: {} iterations, no fetch calls", iterations);

        let ops = Arc::new(MockOps::new(0, 0));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("pinned-simple-{}", i % 10);
            let script = Script::new(SIMPLE_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pinned(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Pinned Simple", start.elapsed(), iterations, &stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_with_fetch() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pinned_pool(100, RuntimeLimits::default());

        let iterations = 20u32;
        let fetch_latency_ms = 20u64;

        print_header("Thread-Pinned Pool - Handler with fetch()");
        println!(
            "  Config: {} iterations, fetch latency: {}ms",
            iterations, fetch_latency_ms
        );

        let ops = Arc::new(MockOps::new(fetch_latency_ms, fetch_latency_ms));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("pinned-fetch-{}", i % 10);
            let script = Script::new(FETCH_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pinned(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Pinned with Fetch", start.elapsed(), iterations, &stats);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_multi_fetch() {
    let _lock = V8_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    run_in_local(|| async {
        init_pinned_pool(100, RuntimeLimits::default());

        let iterations = 15u32;
        let fetch_latency_ms = 15u64;

        print_header("Thread-Pinned Pool - Multi-fetch Handler (parallel fetches)");
        println!(
            "  Config: {} iterations, 2 parallel fetches, {}ms each",
            iterations, fetch_latency_ms
        );

        let ops = Arc::new(MockOps::new(fetch_latency_ms, fetch_latency_ms));
        let stats = ops.stats();

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("pinned-multi-{}", i % 5);
            let script = Script::new(MULTI_FETCH_HANDLER);
            let (task, rx) = Task::fetch(make_request());

            execute_pinned(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            let _response = rx.await.unwrap();

            if (i + 1) % 5 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Pinned Multi-Fetch", start.elapsed(), iterations, &stats);
    })
    .await;
}

// ============================================================================
// Summary
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_summary() {
    println!("\n");
    println!("════════════════════════════════════════════════════════════════════════════════");
    println!("  Realistic Benchmark - Run Instructions");
    println!("════════════════════════════════════════════════════════════════════════════════");
    println!();
    println!("  This benchmark uses MockOps to simulate real fetch() latency.");
    println!("  It tests HTTP handlers (not just scheduled events).");
    println!();
    println!("  ✅ All architectures now support async fetch() operations!");
    println!();
    println!("  ⚠️  V8 GLOBAL STATE CONFLICT:");
    println!("  V8 initializes differently for Legacy vs Pool architectures.");
    println!("  Tests MUST be run separately per architecture to avoid conflicts.");
    println!();
    println!("  Run each architecture separately:");
    println!();
    println!("  Legacy:");
    println!("    cargo test --test realistic_bench bench_legacy -- --nocapture");
    println!();
    println!("  Shared Pool:");
    println!("    cargo test --test realistic_bench bench_shared -- --nocapture");
    println!();
    println!("  Thread-Pinned Pool:");
    println!("    cargo test --test realistic_bench bench_pinned -- --nocapture");
    println!();
    println!("  ──────────────────────────────────────────────────────────────────────────────");
    println!("  BENCHMARK RESULTS (run separately per architecture):");
    println!("  ──────────────────────────────────────────────────────────────────────────────");
    println!("  │ Architecture      │ Simple       │ With fetch() │ vs Legacy   │");
    println!("  ├───────────────────┼──────────────┼──────────────┼─────────────┤");
    println!("  │ Legacy (Worker)   │ ~406 req/s   │ ~39 req/s    │ baseline    │");
    println!("  │ Shared Pool       │ ~582 req/s   │ ~38 req/s    │ +43% sync   │");
    println!("  │ Thread-Pinned     │ ~651 req/s   │ ~39 req/s    │ +60% sync   │");
    println!("  └───────────────────┴──────────────┴──────────────┴─────────────┘");
    println!();
    println!("  Multi-fetch (2 parallel fetches, 15ms each):");
    println!("  │ Shared Pool       │ ~49 req/s, 20ms latency  │ ✅ parallel  │");
    println!("  │ Thread-Pinned     │ ~48 req/s, 21ms latency  │ ✅ parallel  │");
    println!();
    println!("  Key findings:");
    println!("  - ✅ All architectures support async fetch() operations");
    println!("  - ✅ Parallel fetches execute concurrently (15ms each → 20ms total)");
    println!("  - Pools are 43-60% faster than Legacy for sync workloads");
    println!("  - With I/O (fetch), all architectures perform similarly (~39 req/s)");
    println!("  - I/O latency dominates total time when fetch() is used");
    println!("  - Thread-Pinned recommended for production (security + best perf)");
    println!();
}
