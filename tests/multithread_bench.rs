//! Multi-threaded Benchmark - Shared Pool vs Thread-Pinned Pool
//!
//! This benchmark tests real mutex contention with multiple OS threads.
//! Each thread has its own tokio runtime, simulating a real multi-threaded server.
//!
//! Run with:
//!   cargo test --test multithread_bench -- --nocapture --test-threads=1

use openworkers_core::{DefaultOps, Event, OperationsHandle, RuntimeLimits, Script};
use openworkers_runtime_v8::{Worker, execute_pinned, execute_pooled, init_pinned_pool, init_pool};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

// ============================================================================
// Config
// ============================================================================

const SCRIPT: &str = r#"
    addEventListener('scheduled', event => {
        let sum = 0;
        for (let i = 0; i < 1000; i++) {
            sum += i;
        }
    });
"#;

#[derive(Clone)]
struct BenchConfig {
    name: &'static str,
    num_threads: usize,
    requests_per_thread: usize,
    num_workers: usize,
    pool_size: usize,
}

impl BenchConfig {
    fn total_requests(&self) -> usize {
        self.num_threads * self.requests_per_thread
    }
}

// ============================================================================
// Results
// ============================================================================

struct BenchResult {
    name: String,
    total_time: Duration,
    num_requests: usize,
}

impl BenchResult {
    fn print(&self) {
        let throughput = self.num_requests as f64 / self.total_time.as_secs_f64();
        let avg_latency = self.total_time / self.num_requests as u32;

        println!("  {}", self.name);
        println!("    Total time:  {:?}", self.total_time);
        println!("    Throughput:  {:.0} req/s", throughput);
        println!("    Avg latency: {:?}", avg_latency);
    }

    fn throughput(&self) -> f64 {
        self.num_requests as f64 / self.total_time.as_secs_f64()
    }
}

// ============================================================================
// Benchmark Runners
// ============================================================================

fn bench_shared_pool(config: &BenchConfig) -> BenchResult {
    // Initialize pool once (global)
    init_pool(config.pool_size, RuntimeLimits::default());

    let completed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(config.num_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..config.num_threads)
        .map(|thread_id| {
            let config = config.clone();
            let completed = completed.clone();
            let barrier = barrier.clone();

            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let local = LocalSet::new();

                    local
                        .run_until(async {
                            let ops: OperationsHandle = Arc::new(DefaultOps);

                            // Wait for all threads to be ready
                            barrier.wait();

                            for i in 0..config.requests_per_thread {
                                let worker_id =
                                    format!("shared-t{}-w{}", thread_id, i % config.num_workers);
                                let script = Script::new(SCRIPT);
                                let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

                                execute_pooled(&worker_id, script, ops.clone(), task)
                                    .await
                                    .unwrap();
                                rx.await.unwrap();

                                completed.fetch_add(1, Ordering::Relaxed);
                            }
                        })
                        .await;
                });
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    BenchResult {
        name: format!("Shared Pool ({})", config.name),
        total_time: start.elapsed(),
        num_requests: config.total_requests(),
    }
}

fn bench_pinned_pool(config: &BenchConfig) -> BenchResult {
    // Initialize pool once (per-thread pools)
    init_pinned_pool(config.pool_size, RuntimeLimits::default());

    let completed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(config.num_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..config.num_threads)
        .map(|thread_id| {
            let config = config.clone();
            let completed = completed.clone();
            let barrier = barrier.clone();

            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let local = LocalSet::new();

                    local
                        .run_until(async {
                            let ops: OperationsHandle = Arc::new(DefaultOps);

                            // Wait for all threads to be ready
                            barrier.wait();

                            for i in 0..config.requests_per_thread {
                                let worker_id =
                                    format!("pinned-t{}-w{}", thread_id, i % config.num_workers);
                                let script = Script::new(SCRIPT);
                                let (task, rx) = Event::from_schedule("bench".to_string(), 1000);

                                execute_pinned(&worker_id, script, ops.clone(), task)
                                    .await
                                    .unwrap();
                                rx.await.unwrap();

                                completed.fetch_add(1, Ordering::Relaxed);
                            }
                        })
                        .await;
                });
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    BenchResult {
        name: format!("Thread-Pinned ({})", config.name),
        total_time: start.elapsed(),
        num_requests: config.total_requests(),
    }
}

fn bench_legacy(config: &BenchConfig) -> BenchResult {
    let completed = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(config.num_threads));

    let start = Instant::now();

    let handles: Vec<_> = (0..config.num_threads)
        .map(|_thread_id| {
            let config = config.clone();
            let completed = completed.clone();
            let barrier = barrier.clone();

            thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let local = LocalSet::new();

                    local
                        .run_until(async {
                            let ops: OperationsHandle = Arc::new(DefaultOps);

                            // Wait for all threads to be ready
                            barrier.wait();

                            for _ in 0..config.requests_per_thread {
                                // Create new isolate each time (legacy pattern)
                                let script = Script::new(SCRIPT);
                                let mut worker = Worker::new_with_ops(script, None, ops.clone())
                                    .await
                                    .unwrap();

                                let (task, rx) = Event::from_schedule("bench".to_string(), 1000);
                                worker.exec(task).await.unwrap();
                                rx.await.unwrap();

                                // Worker dropped here = isolate destroyed
                                completed.fetch_add(1, Ordering::Relaxed);
                            }
                        })
                        .await;
                });
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    BenchResult {
        name: format!("Legacy ({})", config.name),
        total_time: start.elapsed(),
        num_requests: config.total_requests(),
    }
}

// ============================================================================
// Print Helpers
// ============================================================================

fn print_header(title: &str) {
    println!();
    println!("╔═══════════════════════════════════════════════════════════════════════════╗");
    println!("║ {:^73} ║", title);
    println!("╚═══════════════════════════════════════════════════════════════════════════╝");
}

fn print_comparison(shared: &BenchResult, pinned: &BenchResult) {
    let shared_tp = shared.throughput();
    let pinned_tp = pinned.throughput();
    let diff = ((pinned_tp - shared_tp) / shared_tp) * 100.0;

    println!();
    println!("  Comparison:");
    println!("    Shared Pool:   {:.0} req/s", shared_tp);
    println!("    Thread-Pinned: {:.0} req/s", pinned_tp);

    if diff > 0.0 {
        println!("    Winner: Thread-Pinned (+{:.1}%)", diff);
    } else {
        println!("    Winner: Shared Pool ({:.1}%)", diff);
    }
}

fn print_comparison_all(legacy: &BenchResult, shared: &BenchResult, pinned: &BenchResult) {
    let legacy_tp = legacy.throughput();
    let shared_tp = shared.throughput();
    let pinned_tp = pinned.throughput();

    let shared_vs_legacy = ((shared_tp - legacy_tp) / legacy_tp) * 100.0;
    let pinned_vs_legacy = ((pinned_tp - legacy_tp) / legacy_tp) * 100.0;
    let pinned_vs_shared = ((pinned_tp - shared_tp) / shared_tp) * 100.0;

    println!();
    println!("  ┌─────────────────┬────────────┬─────────────────┐");
    println!("  │ Architecture    │ Throughput │ vs Legacy       │");
    println!("  ├─────────────────┼────────────┼─────────────────┤");
    println!(
        "  │ Legacy          │ {:>6.0} req/s │ baseline        │",
        legacy_tp
    );
    println!(
        "  │ Shared Pool     │ {:>6.0} req/s │ {:>+6.1}%         │",
        shared_tp, shared_vs_legacy
    );
    println!(
        "  │ Thread-Pinned   │ {:>6.0} req/s │ {:>+6.1}%         │",
        pinned_tp, pinned_vs_legacy
    );
    println!("  └─────────────────┴────────────┴─────────────────┘");
    println!();
    println!("  Thread-Pinned vs Shared: {:>+.1}%", pinned_vs_shared);
}

// ============================================================================
// Tests
// ============================================================================

#[test]
fn bench_all_architectures() {
    print_header("All Architectures Comparison (4 threads × 20 req)");

    let config = BenchConfig {
        name: "all",
        num_threads: 4,
        requests_per_thread: 20,
        num_workers: 5,
        pool_size: 20,
    };

    println!(
        "  Config: {} threads × {} req = {} total",
        config.num_threads,
        config.requests_per_thread,
        config.total_requests(),
    );

    let legacy = bench_legacy(&config);
    legacy.print();

    let shared = bench_shared_pool(&config);
    shared.print();

    let pinned = bench_pinned_pool(&config);
    pinned.print();

    print_comparison_all(&legacy, &shared, &pinned);
}

#[test]
fn bench_4_threads_cpu_bound() {
    print_header("4 Threads - CPU Bound (25 req/thread = 100 total)");

    let config = BenchConfig {
        name: "4t-cpu",
        num_threads: 4,
        requests_per_thread: 25,
        num_workers: 5,
        pool_size: 20,
    };

    println!(
        "  Config: {} threads × {} req = {} total, {} workers, pool={}",
        config.num_threads,
        config.requests_per_thread,
        config.total_requests(),
        config.num_workers,
        config.pool_size
    );

    let shared = bench_shared_pool(&config);
    shared.print();

    let pinned = bench_pinned_pool(&config);
    pinned.print();

    print_comparison(&shared, &pinned);
}

#[test]
fn bench_8_threads_high_contention() {
    print_header("8 Threads - High Contention (50 req/thread, pool=10)");

    let config = BenchConfig {
        name: "8t-contention",
        num_threads: 8,
        requests_per_thread: 50,
        num_workers: 5,
        pool_size: 10,
    };

    println!(
        "  Config: {} threads × {} req = {} total, {} workers, pool={}",
        config.num_threads,
        config.requests_per_thread,
        config.total_requests(),
        config.num_workers,
        config.pool_size
    );

    let shared = bench_shared_pool(&config);
    shared.print();

    let pinned = bench_pinned_pool(&config);
    pinned.print();

    print_comparison(&shared, &pinned);
}

#[test]
fn bench_4_threads_warm_cache() {
    print_header("4 Threads - Warm Cache (same worker per thread)");

    let config = BenchConfig {
        name: "4t-warm",
        num_threads: 4,
        requests_per_thread: 50,
        num_workers: 1, // Same worker_id per thread = cache hit
        pool_size: 20,
    };

    println!(
        "  Config: {} threads × {} req = {} total, 1 worker/thread (100% cache hit)",
        config.num_threads,
        config.requests_per_thread,
        config.total_requests(),
    );

    let shared = bench_shared_pool(&config);
    shared.print();

    let pinned = bench_pinned_pool(&config);
    pinned.print();

    print_comparison(&shared, &pinned);
}
