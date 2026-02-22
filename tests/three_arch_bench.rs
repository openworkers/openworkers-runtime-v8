//! Architecture Benchmark - Legacy vs Thread-Pinned Pool
//!
//! This benchmark compares execution modes in runtime-v8:
//! 1. Legacy (Worker) - New isolate per request
//! 2. Thread-Pinned Pool (execute_pinned) - Thread-local pools, zero contention
//!
//! Run with:
//!   cargo test --test three_arch_bench bench_legacy -- --nocapture --test-threads=1
//!   cargo test --test three_arch_bench bench_pinned -- --nocapture --test-threads=1

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

const SIMPLE_SCRIPT: &str = r#"
    addEventListener('scheduled', event => {
        let sum = 0;
        for (let i = 0; i < 1000; i++) {
            sum += i;
        }
    });
"#;

const CPU_HEAVY_SCRIPT: &str = r#"
    addEventListener('scheduled', event => {
        let sum = 0;
        for (let i = 0; i < 10000; i++) {
            sum += Math.sqrt(i) * Math.sin(i);
        }
    });
"#;

// ============================================================================
// Helper Functions
// ============================================================================

fn print_header(title: &str) {
    println!("\n╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║ {:^73} ║", title);
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
}

fn print_results(name: &str, elapsed: Duration, iterations: u32, io_delay_ms: Option<u64>) {
    let throughput = iterations as f64 / elapsed.as_secs_f64();
    let avg_latency = elapsed / iterations;

    println!("\n  Results for {}:", name);
    println!("    Total time:    {:?}", elapsed);
    println!("    Throughput:    {:.2} req/s", throughput);
    println!("    Avg latency:   {:?}", avg_latency);

    if let Some(io_ms) = io_delay_ms {
        let theoretical_io = Duration::from_millis(io_ms * iterations as u64);
        println!("    I/O overhead:  {:?}", theoretical_io);
        println!(
            "    JS overhead:   {:?}",
            elapsed.saturating_sub(theoretical_io)
        );
    }
}

// ============================================================================
// Legacy Benchmarks (Worker - new isolate per request)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_standard() {
    run_in_local(|| async {
        let iterations = 20u32;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Legacy (Worker) - Standard Workload");
        println!(
            "  Config: {} iterations, {}ms I/O delay",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();

            sleep(Duration::from_millis(io_delay_ms)).await;

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Legacy", start.elapsed(), iterations, Some(io_delay_ms));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_cpu_bound() {
    run_in_local(|| async {
        let iterations = 30u32;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Legacy (Worker) - CPU-Bound (no I/O)");
        println!("  Config: {} iterations, no I/O delay", iterations);

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(CPU_HEAVY_SCRIPT);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Legacy", start.elapsed(), iterations, None);
    })
    .await;
}

// ============================================================================
// Thread-Pinned Pool Benchmarks (execute_pinned - thread-local, zero contention)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_standard() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 100,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 20u32;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Thread-Pinned Pool (execute_pinned) - Standard Workload");
        println!(
            "  Config: {} iterations, {}ms I/O delay, max_per_thread=100",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("pinned-worker-{}", i % 10);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: worker_id,
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops: ops.clone(),
                task,
                on_warm_hit: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();

            sleep(Duration::from_millis(io_delay_ms)).await;

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results(
            "Thread-Pinned",
            start.elapsed(),
            iterations,
            Some(io_delay_ms),
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_cpu_bound() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 100,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 30u32;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Thread-Pinned Pool (execute_pinned) - CPU-Bound (no I/O)");
        println!(
            "  Config: {} iterations, no I/O delay, max_per_thread=100",
            iterations
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("pinned-cpu-{}", i % 10);
            let script = Script::new(CPU_HEAVY_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: worker_id,
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops: ops.clone(),
                task,
                on_warm_hit: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Thread-Pinned", start.elapsed(), iterations, None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_warm_cache() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 100,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 50u32;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Thread-Pinned Pool (execute_pinned) - Warm Cache (same worker_id)");
        println!(
            "  Config: {} iterations, same worker_id (100% cache hit)",
            iterations
        );

        // Pre-warm
        {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            execute_pinned(PinnedExecuteRequest {
                owner_id: "warm-pinned".to_string(),
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops: ops.clone(),
                task,
                on_warm_hit: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: "warm-pinned".to_string(),
                worker_id: "test-worker".to_string(),
                version: 1,
                script,
                ops: ops.clone(),
                task,
                on_warm_hit: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();

            if (i + 1) % 25 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Thread-Pinned (warm)", start.elapsed(), iterations, None);
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
    println!("  Three Architecture Benchmark - Run Instructions");
    println!("════════════════════════════════════════════════════════════════════════════════");
    println!();
    println!("  IMPORTANT: Run each architecture SEPARATELY due to V8 state conflicts.");
    println!();
    println!("  1. Legacy (Worker - new isolate per request):");
    println!(
        "     cargo test --test three_arch_bench bench_legacy -- --nocapture --test-threads=1"
    );
    println!();
    println!("  2. Thread-Pinned Pool (execute_pinned - thread-local, zero contention):");
    println!(
        "     cargo test --test three_arch_bench bench_pinned -- --nocapture --test-threads=1"
    );
    println!();
    println!("  Expected results (approximate):");
    println!("  ┌─────────────────────┬───────────────┬────────────────────────────────────────┐");
    println!("  │ Architecture        │ Throughput    │ Notes                                  │");
    println!("  ├─────────────────────┼───────────────┼────────────────────────────────────────┤");
    println!("  │ Legacy (Worker)     │ Slowest       │ Creates new isolate each time (~ms)   │");
    println!("  │ Thread-Pinned Pool  │ Fastest       │ Reuses isolates, no mutex contention  │");
    println!("  │ Any (warm cache)    │ Very fast     │ Same worker_id = cache hit            │");
    println!("  └─────────────────────┴───────────────┴────────────────────────────────────────┘");
    println!();
    println!("  Key insights:");
    println!("  - Thread-Pinned wins in CPU-bound scenarios (no mutex contention)");
    println!("  - Warm cache (same worker_id) is fastest for all pool architectures");
    println!("  - For multi-tenant security, Thread-Pinned with sticky routing is required");
    println!();
}
