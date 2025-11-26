use criterion::{Criterion, criterion_group, criterion_main};
use openworkers_runtime_v8::Worker;

openworkers_core::generate_worker_benchmarks!(Worker);

criterion_group!(benches, worker_benchmarks);
criterion_main!(benches);
