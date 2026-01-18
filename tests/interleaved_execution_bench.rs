//! Benchmark: Interleaved Execution with Simulated Network I/O
//!
//! This benchmark demonstrates the key advantage of the isolate pool pattern:
//! multiple workers can be interleaved on a single thread, releasing the V8 lock
//! during I/O operations.
//!
//! Run with: cargo test --test interleaved_execution_bench -- --nocapture --test-threads=1

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{Worker, execute_pooled, init_pool};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Script that simulates a worker making a fetch call
#[allow(dead_code)]
const FETCH_SCRIPT: &str = r#"
    addEventListener('fetch', event => {
        event.respondWith(new Response('OK'));
    });
"#;

const SCHEDULED_SCRIPT: &str = r#"
    addEventListener('scheduled', event => {
        // Simulate some computation
        let sum = 0;
        for (let i = 0; i < 1000; i++) {
            sum += i;
        }
    });
"#;

// ============================================================================
// Benchmark 1: Sequential Execution (Old Pattern)
// ============================================================================
// Each request must complete before the next one starts.
// This is what we had before: create isolate -> execute -> destroy -> repeat

#[tokio::test(flavor = "current_thread")]
async fn bench_sequential_workers_with_io() {
    run_in_local(|| async {
        let num_requests = 10;
        let io_delay_ms = 50; // Simulated network latency
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n=== Sequential Workers (Old Pattern) ===");
        println!("   Requests: {}", num_requests);
        println!("   Simulated I/O delay: {}ms per request", io_delay_ms);

        let start = Instant::now();

        for i in 0..num_requests {
            // Create worker (cold start)
            let script = Script::new(SCHEDULED_SCRIPT);
            let mut worker = Worker::new_with_ops(script, None, ops.clone())
                .await
                .unwrap();

            // Execute JS
            let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
            worker.exec(task).await.unwrap();
            rx.await.unwrap();

            // Simulate network I/O (blocking the thread!)
            sleep(Duration::from_millis(io_delay_ms)).await;

            // Worker dropped here
            if (i + 1) % 5 == 0 {
                println!("   Completed {}/{} requests", i + 1, num_requests);
            }
        }

        let elapsed = start.elapsed();
        let theoretical_io_time = Duration::from_millis(io_delay_ms * num_requests as u64);

        println!("\n   Results:");
        println!("   Total time: {:?}", elapsed);
        println!("   Theoretical I/O time: {:?}", theoretical_io_time);
        println!(
            "   JS execution overhead: {:?}",
            elapsed.saturating_sub(theoretical_io_time)
        );
        println!(
            "   Throughput: {:.2} req/s",
            num_requests as f64 / elapsed.as_secs_f64()
        );
        println!("   (Sequential: requests are serialized, I/O blocks execution)");
    })
    .await;
}

// ============================================================================
// Benchmark 2: Concurrent Workers with Pool (New Pattern)
// ============================================================================
// Multiple workers can run concurrently, sharing the isolate pool.
// While one worker waits for I/O, another can use the isolate.

#[tokio::test(flavor = "current_thread")]
async fn bench_concurrent_pool_with_io() {
    run_in_local(|| async {
        init_pool(10, RuntimeLimits::default());

        let num_requests = 10;
        let io_delay_ms = 50; // Simulated network latency
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n=== Concurrent Pool Workers (New Pattern) ===");
        println!("   Requests: {}", num_requests);
        println!("   Simulated I/O delay: {}ms per request", io_delay_ms);
        println!("   Pool size: 10 isolates");

        let start = Instant::now();

        // Spawn all requests concurrently
        let mut handles = Vec::new();

        for i in 0..num_requests {
            let ops = ops.clone();
            let handle = tokio::task::spawn_local(async move {
                let worker_id = format!("concurrent-worker-{}", i);

                // Execute JS using pool
                let script = Script::new(SCHEDULED_SCRIPT);
                let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
                execute_pooled(&worker_id, script, ops, task).await.unwrap();
                rx.await.unwrap();

                // Simulate network I/O
                // During this time, the isolate is available for other workers!
                sleep(Duration::from_millis(io_delay_ms)).await;

                i
            });
            handles.push(handle);
        }

        // Wait for all to complete
        let mut completed = 0;

        for handle in handles {
            let _i = handle.await.unwrap();
            completed += 1;

            if completed % 5 == 0 {
                println!("   Completed {}/{} requests", completed, num_requests);
            }
        }

        let elapsed = start.elapsed();
        let theoretical_io_time = Duration::from_millis(io_delay_ms as u64); // All I/O overlaps!

        println!("\n   Results:");
        println!("   Total time: {:?}", elapsed);
        println!(
            "   Sequential I/O time would be: {:?}",
            Duration::from_millis(io_delay_ms * num_requests as u64)
        );
        println!(
            "   Actual I/O time (overlapped): ~{:?}",
            theoretical_io_time
        );
        println!(
            "   Throughput: {:.2} req/s",
            num_requests as f64 / elapsed.as_secs_f64()
        );
        println!("   (Concurrent: I/O overlaps, massive speedup!)");
    })
    .await;
}

// ============================================================================
// Benchmark 3: High Concurrency Stress Test
// ============================================================================
// Many concurrent requests with limited pool size

#[tokio::test(flavor = "current_thread")]
async fn bench_high_concurrency_pool() {
    run_in_local(|| async {
        init_pool(5, RuntimeLimits::default()); // Only 5 isolates

        let num_requests = 50;
        let io_delay_ms = 20;
        let ops: OperationsHandle = Arc::new(DefaultOps);

        println!("\n=== High Concurrency Pool Test ===");
        println!("   Requests: {}", num_requests);
        println!("   Pool size: 5 isolates (10:1 contention ratio)");
        println!("   Simulated I/O delay: {}ms per request", io_delay_ms);

        let start = Instant::now();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();

        for i in 0..num_requests {
            let ops = ops.clone();
            let completed = completed.clone();

            let handle = tokio::task::spawn_local(async move {
                let worker_id = format!("stress-worker-{}", i);

                // Execute JS
                let script = Script::new(SCHEDULED_SCRIPT);
                let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
                execute_pooled(&worker_id, script, ops, task).await.unwrap();
                rx.await.unwrap();

                // Simulate I/O
                sleep(Duration::from_millis(io_delay_ms)).await;

                completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        let elapsed = start.elapsed();

        println!("\n   Results:");
        println!("   Total time: {:?}", elapsed);
        println!(
            "   Throughput: {:.2} req/s",
            num_requests as f64 / elapsed.as_secs_f64()
        );
        println!("   Avg per request: {:?}", elapsed / num_requests as u32);
        println!("   (Pool handles contention gracefully)");
    })
    .await;
}

// ============================================================================
// Summary
// ============================================================================

#[tokio::test(flavor = "current_thread")]
async fn bench_io_interleaving_summary() {
    println!("\n");
    println!("================================================================");
    println!("    I/O Interleaving Benchmark Summary                          ");
    println!("================================================================");
    println!();
    println!("Key insight: The pool pattern allows workers to RELEASE the");
    println!("V8 isolate while waiting for I/O, enabling interleaving.");
    println!();
    println!("Pattern comparison (10 requests, 50ms I/O each):");
    println!();
    println!("  Sequential (old pattern):");
    println!("    Request 1: [JS][----I/O----]");
    println!("    Request 2:                  [JS][----I/O----]");
    println!("    Request 3:                                   [JS][----I/O----]");
    println!("    Total: ~500ms+ (serialized)");
    println!();
    println!("  Concurrent Pool (new pattern):");
    println!("    Request 1: [JS][----I/O----]");
    println!("    Request 2: [JS][----I/O----]");
    println!("    Request 3: [JS][----I/O----]");
    println!("    Total: ~50ms+ (parallelized I/O)");
    println!();
    println!("The pool enables 10x+ throughput for I/O-bound workloads!");
    println!();
}
