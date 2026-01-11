//! Three Architecture Benchmark - Legacy vs Shared Pool vs Thread-Pinned Pool
//!
//! This benchmark compares the three execution modes in runtime-v8:
//! 1. Legacy (Worker) - New isolate per request
//! 2. Shared Pool (execute_pooled) - Global mutex-protected LRU cache
//! 3. Thread-Pinned Pool (execute_pinned) - Thread-local pools, zero contention
//!
//! Run with:
//!   cargo test --test three_arch_bench bench_legacy -- --nocapture --test-threads=1
//!   cargo test --test three_arch_bench bench_shared -- --nocapture --test-threads=1
//!   cargo test --test three_arch_bench bench_pinned -- --nocapture --test-threads=1

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, OperationsHandle, RuntimeLimits, Script, Task};
use openworkers_runtime_v8::{Worker, execute_pinned, execute_pooled, init_pinned_pool, init_pool};
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

            let (task, rx) = Task::scheduled(1000);
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

            let (task, rx) = Task::scheduled(1000);
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
// Shared Pool Benchmarks (execute_pooled - global mutex LRU cache)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_standard() {
    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 20u32;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Shared Pool (execute_pooled) - Standard Workload");
        println!(
            "  Config: {} iterations, {}ms I/O delay, pool_size=100",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("shared-worker-{}", i % 10); // 10 unique workers
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            sleep(Duration::from_millis(io_delay_ms)).await;

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results(
            "Shared Pool",
            start.elapsed(),
            iterations,
            Some(io_delay_ms),
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_cpu_bound() {
    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 30u32;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Shared Pool (execute_pooled) - CPU-Bound (no I/O)");
        println!(
            "  Config: {} iterations, no I/O delay, pool_size=100",
            iterations
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("shared-cpu-{}", i % 10);
            let script = Script::new(CPU_HEAVY_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Shared Pool", start.elapsed(), iterations, None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_shared_warm_cache() {
    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 50u32;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        print_header("Shared Pool (execute_pooled) - Warm Cache (same worker_id)");
        println!(
            "  Config: {} iterations, same worker_id (100% cache hit)",
            iterations
        );

        // Pre-warm
        {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);
            execute_pooled("warm-shared", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled("warm-shared", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            if (i + 1) % 25 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        print_results("Shared Pool (warm)", start.elapsed(), iterations, None);
    })
    .await;
}

// ============================================================================
// Thread-Pinned Pool Benchmarks (execute_pinned - thread-local, zero contention)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_standard() {
    run_in_local(|| async {
        init_pinned_pool(100, RuntimeLimits::default());

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
            let (task, rx) = Task::scheduled(1000);

            execute_pinned(&worker_id, script, ops.clone(), task)
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
        init_pinned_pool(100, RuntimeLimits::default());

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
            let (task, rx) = Task::scheduled(1000);

            execute_pinned(&worker_id, script, ops.clone(), task)
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
        init_pinned_pool(100, RuntimeLimits::default());

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
            let (task, rx) = Task::scheduled(1000);
            execute_pinned("warm-pinned", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pinned("warm-pinned", script, ops.clone(), task)
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
    println!("  2. Shared Pool (execute_pooled - global mutex LRU cache):");
    println!(
        "     cargo test --test three_arch_bench bench_shared -- --nocapture --test-threads=1"
    );
    println!();
    println!("  3. Thread-Pinned Pool (execute_pinned - thread-local, zero contention):");
    println!(
        "     cargo test --test three_arch_bench bench_pinned -- --nocapture --test-threads=1"
    );
    println!();
    println!("  Expected results (approximate):");
    println!("  ┌─────────────────────┬───────────────┬────────────────────────────────────────┐");
    println!("  │ Architecture        │ Throughput    │ Notes                                  │");
    println!("  ├─────────────────────┼───────────────┼────────────────────────────────────────┤");
    println!("  │ Legacy (Worker)     │ ~400-500 req/s│ Creates new isolate each time (~2ms)  │");
    println!("  │ Shared Pool         │ ~500-800 req/s│ Reuses isolates, has mutex overhead   │");
    println!("  │ Thread-Pinned Pool  │ ~800-1000 req/s│ Reuses isolates, no mutex contention  │");
    println!("  │ Any (warm cache)    │ ~1500+ req/s  │ Same worker_id = cache hit            │");
    println!("  └─────────────────────┴───────────────┴────────────────────────────────────────┘");
    println!();
    println!("  Key insights:");
    println!("  - Thread-Pinned wins in CPU-bound scenarios (no mutex contention)");
    println!("  - Shared Pool can degrade under high contention (worse than Legacy!)");
    println!("  - Warm cache (same worker_id) is fastest for all pool architectures");
    println!("  - For multi-tenant security, Thread-Pinned with sticky routing is required");
    println!();
}
