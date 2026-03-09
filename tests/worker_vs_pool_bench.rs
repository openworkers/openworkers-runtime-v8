//! Benchmark comparing Worker (classic) vs Pinned Pool execution
//!
//! Run with: cargo test --test worker_vs_pool_bench -- --nocapture

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
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

/// Benchmark: Pinned pool cold start (first request for each worker_id)
/// Each iteration uses a DIFFERENT worker_id (cache miss)
#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_cold_start() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 100,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 20;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        let start = Instant::now();

        for i in 0..iterations {
            let worker_id = format!("cold-worker-{}", i);
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
                env_updated_at: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Pinned Pool Cold Start (new context each time) ===");
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

/// Benchmark: Pinned pool warm start (same worker_id, reusing cached isolate)
/// All iterations use the SAME worker_id (cache hit)
#[tokio::test(flavor = "current_thread")]
async fn bench_pinned_warm_start() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 100,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let iterations = 100;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        // Pre-warm the cache
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
                env_updated_at: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();
        }

        let start = Instant::now();

        for _ in 0..iterations {
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
                env_updated_at: None,
            })
            .await
            .unwrap();
            rx.await.unwrap();
        }

        let elapsed = start.elapsed();
        let avg = elapsed / iterations;

        println!("\n=== Pinned Pool Warm Start (cached isolate, new context) ===");
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
    println!("    Worker vs Pinned Pool Performance Comparison                ");
    println!("================================================================");
    println!();
    println!("Run individual benchmarks with:");
    println!("  cargo test --test worker_vs_pool_bench -- --nocapture");
    println!();
    println!("Benchmark scenarios:");
    println!("  - Worker Cold Start:  New isolate + context per request");
    println!("  - Worker Warm Start:  Same isolate, reused for all requests");
    println!("  - Pinned Cold Start:  Isolate from thread-local pool, new context per request");
    println!("  - Pinned Warm Start:  Cached isolate (by worker_id), new context");
    println!();
    println!("Expected results:");
    println!("  - Worker Cold Start:  Slowest (few ms, creates new V8 isolate)");
    println!("  - Worker Warm Start:  Tens of us (reuses isolate and context)");
    println!("  - Pinned Cold Start:  Tens of us (isolate ready, creates context)");
    println!("  - Pinned Warm Start:  Sub-us to few us (cached isolate, creates context)");
    println!();
}
