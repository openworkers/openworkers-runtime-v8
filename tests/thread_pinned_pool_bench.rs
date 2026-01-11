//! Benchmark: Shared Pool vs Thread-Pinned Pool Architecture
//!
//! This benchmark compares two pool architectures:
//!
//! 1. **Shared Pool**: One global pool, isolates can be used by any thread
//!    - Pro: Better resource utilization, fewer idle isolates
//!    - Con: Cross-thread synchronization, potential cache misses
//!
//! 2. **Thread-Pinned Pool**: Each thread has its own local pool
//!    - Pro: No cross-thread sync, better cache locality, no context switching
//!    - Con: May have idle isolates if load is unbalanced
//!
//! The thread-pinned approach is similar to "thread-per-core" architectures
//! used by high-performance systems like ScyllaDB and Seastar.
//!
//! Run with: cargo test --test thread_pinned_pool_bench -- --nocapture --test-threads=1

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ============================================================================
// Shared Pool Architecture
// ============================================================================

struct SharedIsolatePool {
    isolates: Mutex<Vec<MockIsolate>>,
    acquisitions: AtomicUsize,
    contentions: AtomicUsize,
}

struct MockIsolate {
    id: usize,
    // In real code: v8::UnenteredIsolate
}

impl SharedIsolatePool {
    fn new(size: usize) -> Self {
        Self {
            isolates: Mutex::new((0..size).map(|i| MockIsolate { id: i }).collect()),
            acquisitions: AtomicUsize::new(0),
            contentions: AtomicUsize::new(0),
        }
    }

    fn acquire(&self) -> Option<MockIsolate> {
        self.acquisitions.fetch_add(1, Ordering::Relaxed);

        // Try to acquire lock - count contentions
        let mut attempts = 0;

        loop {
            match self.isolates.try_lock() {
                Ok(mut guard) => {
                    if attempts > 0 {
                        self.contentions.fetch_add(1, Ordering::Relaxed);
                    }
                    return guard.pop();
                }
                Err(_) => {
                    attempts += 1;

                    if attempts > 100 {
                        // Fallback to blocking
                        self.contentions.fetch_add(1, Ordering::Relaxed);
                        return self.isolates.lock().unwrap().pop();
                    }

                    std::hint::spin_loop();
                }
            }
        }
    }

    fn release(&self, isolate: MockIsolate) {
        self.isolates.lock().unwrap().push(isolate);
    }

    fn stats(&self) -> (usize, usize) {
        (
            self.acquisitions.load(Ordering::Relaxed),
            self.contentions.load(Ordering::Relaxed),
        )
    }
}

// ============================================================================
// Thread-Pinned Pool Architecture
// ============================================================================

thread_local! {
    static LOCAL_POOL: RefCell<Option<LocalIsolatePool>> = const { RefCell::new(None) };
}

struct LocalIsolatePool {
    isolates: Vec<MockIsolate>,
    #[allow(dead_code)]
    thread_id: usize,
    acquisitions: usize,
}

impl LocalIsolatePool {
    fn new(size: usize, thread_id: usize) -> Self {
        Self {
            isolates: (0..size)
                .map(|i| MockIsolate {
                    id: thread_id * 100 + i,
                })
                .collect(),
            thread_id,
            acquisitions: 0,
        }
    }

    fn acquire(&mut self) -> Option<MockIsolate> {
        self.acquisitions += 1;
        self.isolates.pop()
    }

    fn release(&mut self, isolate: MockIsolate) {
        self.isolates.push(isolate);
    }
}

fn with_local_pool<F, R>(f: F) -> R
where
    F: FnOnce(&mut LocalIsolatePool) -> R,
{
    LOCAL_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        f(pool.as_mut().expect("Local pool not initialized"))
    })
}

fn init_local_pool(size: usize, thread_id: usize) {
    LOCAL_POOL.with(|pool| {
        *pool.borrow_mut() = Some(LocalIsolatePool::new(size, thread_id));
    });
}

// ============================================================================
// Simulated Work
// ============================================================================

fn simulate_js_execution(_isolate_id: usize) {
    // Simulate ~100µs of JS execution
    let start = Instant::now();

    while start.elapsed() < Duration::from_micros(50) {
        std::hint::spin_loop();
    }
}

fn simulate_io() {
    // Simulate ~1ms of I/O wait
    thread::sleep(Duration::from_micros(500));
}

// ============================================================================
// Benchmark: Shared Pool Multi-threaded
// ============================================================================

#[test]
fn bench_shared_pool_multithread() {
    let num_threads = 4;
    let isolates_per_thread = 2;
    let requests_per_thread = 100;

    let pool = Arc::new(SharedIsolatePool::new(num_threads * isolates_per_thread));

    println!("\n=== Shared Pool (Multi-threaded) ===");
    println!("   Threads: {}", num_threads);
    println!("   Total isolates: {}", num_threads * isolates_per_thread);
    println!("   Requests per thread: {}", requests_per_thread);

    let start = Instant::now();

    thread::scope(|s| {
        for t in 0..num_threads {
            let pool = Arc::clone(&pool);

            s.spawn(move || {
                for _r in 0..requests_per_thread {
                    // Acquire isolate (may contend with other threads)
                    let isolate = loop {
                        if let Some(iso) = pool.acquire() {
                            break iso;
                        }
                        thread::sleep(Duration::from_micros(10));
                    };

                    // Execute JS
                    simulate_js_execution(isolate.id);

                    // Release isolate before I/O
                    pool.release(isolate);

                    // Simulate I/O
                    simulate_io();
                }

                t // Return thread id
            });
        }
    });

    let elapsed = start.elapsed();
    let (acquisitions, contentions) = pool.stats();
    let total_requests = num_threads * requests_per_thread;

    println!("\n   Results:");
    println!("   Total time: {:?}", elapsed);
    println!(
        "   Throughput: {:.2} req/s",
        total_requests as f64 / elapsed.as_secs_f64()
    );
    println!("   Total acquisitions: {}", acquisitions);
    println!(
        "   Lock contentions: {} ({:.1}%)",
        contentions,
        contentions as f64 / acquisitions as f64 * 100.0
    );
}

// ============================================================================
// Benchmark: Thread-Pinned Pool
// ============================================================================

#[test]
fn bench_thread_pinned_pool() {
    let num_threads = 4;
    let isolates_per_thread = 2;
    let requests_per_thread = 100;

    println!("\n=== Thread-Pinned Pool ===");
    println!("   Threads: {}", num_threads);
    println!("   Isolates per thread: {}", isolates_per_thread);
    println!("   Requests per thread: {}", requests_per_thread);

    let total_acquisitions = Arc::new(AtomicUsize::new(0));
    let start = Instant::now();

    thread::scope(|s| {
        for t in 0..num_threads {
            let acquisitions = Arc::clone(&total_acquisitions);

            s.spawn(move || {
                // Initialize thread-local pool
                init_local_pool(isolates_per_thread, t);

                for _r in 0..requests_per_thread {
                    // Acquire from LOCAL pool (no cross-thread sync!)
                    let isolate = loop {
                        if let Some(iso) = with_local_pool(|p| p.acquire()) {
                            break iso;
                        }
                        thread::sleep(Duration::from_micros(10));
                    };

                    acquisitions.fetch_add(1, Ordering::Relaxed);

                    // Execute JS
                    simulate_js_execution(isolate.id);

                    // Release to LOCAL pool
                    with_local_pool(|p| p.release(isolate));

                    // Simulate I/O
                    simulate_io();
                }

                t
            });
        }
    });

    let elapsed = start.elapsed();
    let total_requests = num_threads * requests_per_thread;

    println!("\n   Results:");
    println!("   Total time: {:?}", elapsed);
    println!(
        "   Throughput: {:.2} req/s",
        total_requests as f64 / elapsed.as_secs_f64()
    );
    println!(
        "   Total acquisitions: {}",
        total_acquisitions.load(Ordering::Relaxed)
    );
    println!("   Lock contentions: 0 (thread-local = no contention!)");
}

// ============================================================================
// Summary
// ============================================================================

#[test]
fn bench_pool_architecture_summary() {
    println!("\n");
    println!("================================================================");
    println!("    Pool Architecture Comparison                                ");
    println!("================================================================");
    println!();
    println!("Shared Pool:");
    println!("  ┌──────────────────────────────────────┐");
    println!("  │           Global Pool                │");
    println!("  │  [iso1] [iso2] [iso3] [iso4] ...     │");
    println!("  └──────────────────────────────────────┘");
    println!("       ↑         ↑         ↑         ↑");
    println!("   Thread1   Thread2   Thread3   Thread4");
    println!("   (contention on mutex)");
    println!();
    println!("Thread-Pinned Pool:");
    println!("  Thread1        Thread2        Thread3        Thread4");
    println!("     ↓              ↓              ↓              ↓");
    println!("  ┌──────┐      ┌──────┐      ┌──────┐      ┌──────┐");
    println!("  │ Pool │      │ Pool │      │ Pool │      │ Pool │");
    println!("  │[i1,i2]│     │[i3,i4]│     │[i5,i6]│     │[i7,i8]│");
    println!("  └──────┘      └──────┘      └──────┘      └──────┘");
    println!("  (no contention, each thread owns its pool)");
    println!();
    println!("Trade-offs:");
    println!();
    println!("  Shared Pool:");
    println!("    + Better utilization if load is uneven");
    println!("    + Simpler work stealing");
    println!("    - Mutex contention under high load");
    println!("    - Cache invalidation when isolate moves between threads");
    println!();
    println!("  Thread-Pinned Pool:");
    println!("    + Zero contention (thread-local access)");
    println!("    + Better cache locality (isolate stays on same core)");
    println!("    + Predictable latency (no lock waiting)");
    println!("    - May have idle isolates if load is unbalanced");
    println!("    - Requires sticky routing (worker_id -> thread)");
    println!();
    println!("Recommendation:");
    println!("  Use thread-pinned for predictable, low-latency workloads.");
    println!("  Use shared pool for bursty, unpredictable workloads.");
    println!();
}
