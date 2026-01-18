//! Benchmark comparing Worker (classic) vs Pool (pooled) execution
//!
//! Run with: cargo test --test worker_vs_pool_bench -- --nocapture

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{Worker, execute_pooled, init_pool};
use std::sync::Arc;
use std::time::Instant;

const SIMPLE_SCRIPT: &str = r#"
    addEventListener('scheduled', event => {
        console.log('Scheduled event handled!');
    });
"#;

/// Benchmark: Worker creation + execution (cold start)
/// Each iteration creates a NEW Worker from scratch
#[tokio::test(flavor = "current_thread")]
async fn bench_worker_cold_start() {
    run_in_local(|| async {
        let iterations = 20;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        let start = Instant::now();

        for _ in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Worker Cold Start (new isolate each time) ===");
        println!("   Iterations: {}", iterations);
        println!("   Total time: {:?}", elapsed);
        println!("   Average: {:?}", avg);
        println!(
            "   Throughput: {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
    })
    .await;
}

/// Benchmark: Worker warm execution (same worker, multiple requests)
/// Creates ONE Worker, reuses it for all requests
#[tokio::test(flavor = "current_thread")]
async fn bench_worker_warm_start() {
    run_in_local(|| async {
        let iterations = 100;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        let script = Script::new(SIMPLE_SCRIPT);
        let mut worker = Worker::new_with_ops(script, None, ops).await.unwrap();

        let start = Instant::now();

        for _ in 0..iterations {
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Worker Warm Start (same isolate, reused) ===");
        println!("   Iterations: {}", iterations);
        println!("   Total time: {:?}", elapsed);
        println!("   Average: {:?}", avg);
        println!(
            "   Throughput: {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
    })
    .await;
}

/// Benchmark: Pool cold start (first request for each worker_id)
/// Each iteration uses a DIFFERENT worker_id (cache miss)
#[tokio::test(flavor = "current_thread")]
async fn bench_pool_cold_start() {
    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 20;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("cold-worker-{}", i);
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pooled(&worker_id, script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Pool Cold Start (new context each time) ===");
        println!("   Iterations: {}", iterations);
        println!("   Total time: {:?}", elapsed);
        println!("   Average: {:?}", avg);
        println!(
            "   Throughput: {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
    })
    .await;
}

/// Benchmark: Pool warm start (same worker_id, reusing cached isolate)
/// All iterations use the SAME worker_id (cache hit)
#[tokio::test(flavor = "current_thread")]
async fn bench_pool_warm_start() {
    run_in_local(|| async {
        init_pool(100, RuntimeLimits::default());

        let iterations = 100;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        // Pre-warm the cache
        {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            execute_pooled("warm-worker", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for _ in 0..iterations {
            let script = Script::new(SIMPLE_SCRIPT);
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

            execute_pooled("warm-worker", script, ops.clone(), task)
                .await
                .unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Pool Warm Start (cached isolate, new context) ===");
        println!("   Iterations: {}", iterations);
        println!("   Total time: {:?}", elapsed);
        println!("   Average: {:?}", avg);
        println!(
            "   Throughput: {:.2} req/s",
            iterations as f64 / elapsed.as_secs_f64()
        );
    })
    .await;
}

/// Summary comparison
#[tokio::test(flavor = "current_thread")]
async fn bench_comparison_summary() {
    println!("\n");
    println!("================================================================");
    println!("    Worker vs Pool Performance Comparison                       ");
    println!("================================================================");
    println!();
    println!("Run individual benchmarks with:");
    println!("  cargo test --test worker_vs_pool_bench -- --nocapture");
    println!();
    println!("Benchmark scenarios:");
    println!("  - Worker Cold Start: New isolate + context per request");
    println!("  - Worker Warm Start: Same isolate, reused for all requests");
    println!("  - Pool Cold Start:   Isolate from pool, new context per request");
    println!("  - Pool Warm Start:   Cached isolate (by worker_id), new context");
    println!();
    println!("Expected results:");
    println!("  - Worker Cold Start:  ~3-5ms (creates new V8 isolate)");
    println!("  - Worker Warm Start:  ~100µs (reuses isolate and context)");
    println!("  - Pool Cold Start:    ~100µs (isolate ready, creates context)");
    println!("  - Pool Warm Start:    ~100µs (cached isolate, creates context)");
    println!();
}
