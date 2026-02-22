//! Architecture Benchmark - Legacy Worker vs Pinned Pool
//!
//! Compares the actual runtime-v8 implementations:
//! - Legacy: Worker::new_with_ops (new isolate per request)
//! - Pinned Pool: execute_pinned (thread-local pool, zero contention)
//!
//! Run with:
//!   cargo test --test architecture_bench bench_legacy -- --nocapture --test-threads=1
//!   cargo test --test architecture_bench bench_pinned -- --nocapture --test-threads=1

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

            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
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

            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
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
// Pinned Pool Benchmark (execute_pinned - thread-local, zero contention)
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_standard() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 20;
        let io_delay_ms = 5u64;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Pinned Pool - Standard Workload                            ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!(
            "  Iterations: {}, I/O delay: {}ms, max_per_thread: 10\n",
            iterations, io_delay_ms
        );

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("worker-{}", i);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: worker_id.clone(),
                worker_id: worker_id,
                version: 1,
                script,
                ops: ops.clone(),
                task,
                on_warm_hit: None,
            })
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
async fn bench_pinned_cpu_bound() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 30;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Pinned Pool - CPU-bound (no I/O)                           ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!("  Iterations: {}, max_per_thread: 10\n", iterations);

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("cpu-worker-{}", i);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: worker_id.clone(),
                worker_id: worker_id,
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
async fn bench_pinned_warm_cache() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 50;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n╔═══════════════════════════════════════════════════════════════════════╗");
        println!("║             Pinned Pool - Warm Cache (same worker_id)                  ║");
        println!("╚═══════════════════════════════════════════════════════════════════════╝");
        println!("  Iterations: {}, max_per_thread: 10\n", iterations);

        // Pre-warm
        {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: "warm-worker".to_string(),
                worker_id: "warm-worker".to_string(),
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
            // Same worker_id = cache hit
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pinned(PinnedExecuteRequest {
                owner_id: "warm-worker".to_string(),
                worker_id: "warm-worker".to_string(),
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
    println!("  IMPORTANT: Run Legacy and Pinned tests SEPARATELY due to V8 state conflicts.");
    println!();
    println!("  Legacy benchmarks (Worker - new isolate per request):");
    println!(
        "    cargo test --test architecture_bench bench_legacy -- --nocapture --test-threads=1"
    );
    println!();
    println!("  Pinned Pool benchmarks (execute_pinned - thread-local pool):");
    println!(
        "    cargo test --test architecture_bench bench_pinned -- --nocapture --test-threads=1"
    );
    println!();
    println!("  Expected results:");
    println!("    - Legacy: ~400-500 req/s (creates new isolate each time)");
    println!("    - Pinned: ~500-800 req/s (reuses isolates from thread-local pool)");
    println!("    - Pinned (warm): ~1000+ req/s (same worker_id = cache hit)");
    println!();
}
