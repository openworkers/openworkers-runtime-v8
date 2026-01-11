//! Architecture Benchmark - Legacy Worker vs Shared Pool
//!
//! Compares the actual runtime-v8 implementations:
//! - Legacy: Worker::new_with_ops (new isolate per request)
//! - Shared Pool: execute_pooled (current pool implementation)
//!
//! Run with:
//!   cargo test --test architecture_bench bench_legacy -- --nocapture --test-threads=1
//!   cargo test --test architecture_bench bench_pool -- --nocapture --test-threads=1

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, OperationsHandle, RuntimeLimits, Script, Task};
use openworkers_runtime_v8::{Worker, execute_pooled, init_pool};
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

// ============================================================================
// Legacy Benchmark (Worker - new isolate per request)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_standard() {
    run_in_local(|| async {
        let iterations = 20;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Legacy (Worker) - Standard Workload                        ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!(
            "  Iterations: {}, I/O delay: {}ms\n",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            // Create NEW Worker for each request (Legacy pattern)
            let script = Script::new(SIMPLE_SCRIPT);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Task::scheduled(1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();

            // Simulate I/O
            sleep(Duration::from_millis(io_delay_ms)).await;

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        let elapsed = start.elapsed();
        let theoretical_io = Duration::from_millis(io_delay_ms * iterations as u64);

        println!("\n  Results:");
        println!("    Total time:    {:?}", elapsed);
        println!(
            "    Throughput:    {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
        println!("    Avg latency:   {:?}", elapsed / iterations as u32);
        println!("    I/O overhead:  {:?}", theoretical_io);
        println!(
            "    JS overhead:   {:?}",
            elapsed.saturating_sub(theoretical_io)
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_legacy_cpu_bound() {
    run_in_local(|| async {
        let iterations = 30;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Legacy (Worker) - CPU-bound (no I/O)                       ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!("  Iterations: {}\n", iterations);

        let start = Instant::now();

        for i in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
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

        let elapsed = start.elapsed();

        println!("\n  Results:");
        println!("    Total time:    {:?}", elapsed);
        println!(
            "    Throughput:    {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
        println!("    Avg latency:   {:?}", elapsed / iterations as u32);
    })
    .await;
}

// ============================================================================
// Shared Pool Benchmark (execute_pooled - current implementation)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_pool_standard() {
    run_in_local(|| async {
        init_pool(10, RuntimeLimits::default());

        let iterations = 20;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Shared Pool - Standard Workload                            ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!(
            "  Iterations: {}, I/O delay: {}ms, Pool size: 10\n",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("worker-{}", i);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            // Simulate I/O
            sleep(Duration::from_millis(io_delay_ms)).await;

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        let elapsed = start.elapsed();
        let theoretical_io = Duration::from_millis(io_delay_ms * iterations as u64);

        println!("\n  Results:");
        println!("    Total time:    {:?}", elapsed);
        println!(
            "    Throughput:    {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
        println!("    Avg latency:   {:?}", elapsed / iterations as u32);
        println!("    I/O overhead:  {:?}", theoretical_io);
        println!(
            "    JS overhead:   {:?}",
            elapsed.saturating_sub(theoretical_io)
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pool_cpu_bound() {
    run_in_local(|| async {
        init_pool(10, RuntimeLimits::default());

        let iterations = 30;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Shared Pool - CPU-bound (no I/O)                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!("  Iterations: {}, Pool size: 10\n", iterations);

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("cpu-worker-{}", i);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            if (i + 1) % 10 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        let elapsed = start.elapsed();

        println!("\n  Results:");
        println!("    Total time:    {:?}", elapsed);
        println!(
            "    Throughput:    {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
        println!("    Avg latency:   {:?}", elapsed / iterations as u32);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bench_pool_warm_cache() {
    run_in_local(|| async {
        init_pool(10, RuntimeLimits::default());

        let iterations = 50;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Shared Pool - Warm Cache (same worker_id)                  ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!("  Iterations: {}, Pool size: 10\n", iterations);

        // Pre-warm
        {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);
            execute_pooled("warm-worker", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for i in 0..iterations {
            // Same worker_id = cache hit
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Task::scheduled(1000);

            execute_pooled("warm-worker", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();

            if (i + 1) % 25 == 0 {
                println!("  Completed {}/{}", i + 1, iterations);
            }
        }

        let elapsed = start.elapsed();

        println!("\n  Results:");
        println!("    Total time:    {:?}", elapsed);
        println!(
            "    Throughput:    {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
        println!("    Avg latency:   {:?}", elapsed / iterations as u32);
    })
    .await;
}

// ============================================================================
// Summary
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_summary() {
    println!("\n");
    println!("════════════════════════════════════════════════════════════════════════════");
    println!("  Runtime-v8 Architecture Benchmark - Run Instructions");
    println!("════════════════════════════════════════════════════════════════════════════");
    println!();
    println!("  IMPORTANT: Run Legacy and Pool tests SEPARATELY due to V8 state conflicts.");
    println!();
    println!("  Legacy benchmarks (Worker - new isolate per request):");
    println!(
        "    cargo test --test architecture_bench bench_legacy -- --nocapture --test-threads=1"
    );
    println!();
    println!("  Pool benchmarks (execute_pooled - shared pool):");
    println!("    cargo test --test architecture_bench bench_pool -- --nocapture --test-threads=1");
    println!();
    println!("  Expected results:");
    println!("    - Legacy: ~400-500 req/s (creates new isolate each time)");
    println!("    - Pool:   ~500-800 req/s (reuses isolates from pool)");
    println!("    - Pool (warm): ~1000+ req/s (same worker_id = cache hit)");
    println!();
}
