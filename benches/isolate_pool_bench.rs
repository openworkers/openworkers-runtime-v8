//! Benchmarks for IsolatePool performance
//!
//! Measures:
//! - Cold start (cache miss)
//! - Warm start (cache hit)
//! - Concurrent access
//! - LRU eviction overhead

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use openworkers_core::RuntimeLimits;
use openworkers_runtime_v8::IsolatePool;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::runtime::Runtime;

/// Benchmark: Cold start (cache miss - first acquisition of a worker)
fn bench_cold_start(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let pool = Arc::new(IsolatePool::new(1000, RuntimeLimits::default()));

    c.bench_function("pool_cold_start", |b| {
        let counter = AtomicUsize::new(0);
        b.to_async(&rt).iter(|| {
            let pool = Arc::clone(&pool);
            let id = counter.fetch_add(1, Ordering::Relaxed);
            async move {
                // Use unique worker ID for each iteration (always cache miss)
                let worker_id = format!("worker_{}", id);

                let pooled = pool.acquire(&worker_id).await;

                // Simulate minimal work with lock
                pooled
                    .with_lock_async(|isolate| {
                        let terminating = isolate.is_execution_terminating();
                        async move {
                            black_box(terminating);
                        }
                    })
                    .await;
            }
        });
    });
}

/// Benchmark: Warm start (cache hit - reusing cached worker)
fn bench_warm_start(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let pool = Arc::new(IsolatePool::new(1000, RuntimeLimits::default()));

    // Pre-warm the cache
    rt.block_on(async {
        let pooled = pool.acquire("hot-worker").await;
        pooled.with_lock_async(|_isolate| async {}).await;
    });

    c.bench_function("pool_warm_start", |b| {
        b.to_async(&rt).iter(|| {
            let pool = Arc::clone(&pool);
            async move {
                // Always use same worker ID (cache hit)
                let pooled = pool.acquire("hot-worker").await;

                pooled
                    .with_lock_async(|isolate| {
                        let terminating = isolate.is_execution_terminating();
                        async move {
                            black_box(terminating);
                        }
                    })
                    .await;
            }
        });
    });
}

/// Benchmark: Throughput with varying pool sizes
fn bench_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("pool_throughput");

    for pool_size in [10, 100, 1000] {
        group.bench_with_input(
            BenchmarkId::new("pool_size", pool_size),
            &pool_size,
            |b, &size| {
                let pool = Arc::new(IsolatePool::new(size, RuntimeLimits::default()));

                b.to_async(&rt).iter(|| {
                    let pool = Arc::clone(&pool);
                    async move {
                        // Simulate 10 sequential requests to same worker
                        for _ in 0..10 {
                            let pooled = pool.acquire("throughput-worker").await;
                            pooled.with_lock_async(|_isolate| async {}).await;
                        }
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: Concurrent access (multiple tasks)
fn bench_concurrent_access(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("pool_concurrent");

    for num_workers in [1, 5, 10, 50] {
        group.bench_with_input(
            BenchmarkId::new("workers", num_workers),
            &num_workers,
            |b, &count| {
                let pool = Arc::new(IsolatePool::new(1000, RuntimeLimits::default()));

                b.to_async(&rt).iter(|| {
                    let pool = Arc::clone(&pool);
                    async move {
                        use tokio::task::LocalSet;

                        let local = LocalSet::new();
                        local
                            .run_until(async {
                                let handles: Vec<_> = (0..count)
                                    .map(|i| {
                                        let pool = Arc::clone(&pool);
                                        tokio::task::spawn_local(async move {
                                            let worker_id = format!("concurrent_{}", i);
                                            let pooled = pool.acquire(&worker_id).await;
                                            pooled.with_lock_async(|_isolate| async {}).await;
                                        })
                                    })
                                    .collect();

                                for handle in handles {
                                    handle.await.unwrap();
                                }
                            })
                            .await;
                    }
                });
            },
        );
    }
    group.finish();
}

/// Benchmark: LRU eviction overhead
fn bench_lru_eviction(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    c.bench_function("pool_lru_eviction", |b| {
        // Small pool to force frequent evictions
        let pool = Arc::new(IsolatePool::new(10, RuntimeLimits::default()));

        b.to_async(&rt).iter(|| {
            let pool = Arc::clone(&pool);
            async move {
                // Access 20 different workers (pool size is 10, so eviction will happen)
                for i in 0..20 {
                    let worker_id = format!("evict_{}", i);
                    let pooled = pool.acquire(&worker_id).await;
                    pooled.with_lock_async(|_isolate| async {}).await;
                }
            }
        });
    });
}

/// Benchmark: Pool stats retrieval
fn bench_pool_stats(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let pool = Arc::new(IsolatePool::new(1000, RuntimeLimits::default()));

    // Add some workers to cache
    rt.block_on(async {
        for i in 0..10 {
            let pooled = pool.acquire(&format!("stats_{}", i)).await;
            pooled.with_lock_async(|_isolate| async {}).await;
        }
    });

    c.bench_function("pool_get_stats", |b| {
        b.to_async(&rt).iter(|| {
            let pool = Arc::clone(&pool);
            async move {
                let stats = pool.stats().await;
                black_box(stats);
            }
        });
    });
}

criterion_group!(
    benches,
    bench_cold_start,
    bench_warm_start,
    bench_throughput,
    bench_concurrent_access,
    bench_lru_eviction,
    bench_pool_stats
);

criterion_main!(benches);
