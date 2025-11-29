use openworkers_core::{HttpBody, HttpMethod, HttpRequest, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::Instant;

/// Benchmark: Simple response (no async, no timers)
#[tokio::test]
async fn bench_simple_response() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello World'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let iterations = 100;
    let start = Instant::now();

    for _ in 0..iterations {
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: HttpBody::None,
        };

        let (task, rx) = Task::fetch(req);
        worker.exec(task).await.unwrap();
        rx.await.unwrap();
    }

    let elapsed = start.elapsed();
    let avg = elapsed / iterations;

    println!("\n📊 Benchmark: Simple Response");
    println!("   Iterations: {}", iterations);
    println!("   Total time: {:?}", elapsed);
    println!("   Average: {:?}", avg);
    println!(
        "   Throughput: {:.2} req/s",
        iterations as f64 / elapsed.as_secs_f64()
    );
}

/// Benchmark: Async response with Promise
#[tokio::test]
async fn bench_async_response() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(handleRequest());
        });

        async function handleRequest() {
            await new Promise(resolve => setTimeout(resolve, 1));
            return new Response('Async response');
        }
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let iterations = 50;
    let start = Instant::now();

    for _ in 0..iterations {
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: HttpBody::None,
        };

        let (task, rx) = Task::fetch(req);
        worker.exec(task).await.unwrap();
        rx.await.unwrap();
    }

    let elapsed = start.elapsed();
    let avg = elapsed / iterations;

    println!("\n📊 Benchmark: Async Response");
    println!("   Iterations: {}", iterations);
    println!("   Total time: {:?}", elapsed);
    println!("   Average: {:?}", avg);
    println!(
        "   Throughput: {:.2} req/s",
        iterations as f64 / elapsed.as_secs_f64()
    );
}

/// Benchmark: Worker creation
#[tokio::test]
async fn bench_worker_creation() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('OK'));
        });
    "#;

    let iterations = 10;
    let start = Instant::now();

    for _ in 0..iterations {
        let script = Script::new(code);
        let _worker = Worker::new(script, None, None).await.unwrap();
    }

    let elapsed = start.elapsed();
    let avg = elapsed / iterations;

    println!("\n📊 Benchmark: Worker Creation");
    println!("   Iterations: {}", iterations);
    println!("   Total time: {:?}", elapsed);
    println!("   Average: {:?}", avg);
    println!(
        "   Creation rate: {:.2} workers/s",
        iterations as f64 / elapsed.as_secs_f64()
    );
}

/// Benchmark: Complex scenario (timers + fetch simulation)
#[tokio::test]
async fn bench_complex_scenario() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(handleRequest());
        });

        async function handleRequest() {
            // Simulate some complex work
            let result = '';

            // Multiple setTimeout calls
            await new Promise(resolve => setTimeout(resolve, 5));
            result += 'A';

            await new Promise(resolve => setTimeout(resolve, 5));
            result += 'B';

            // Return response
            return new Response(result, {
                status: 200,
                headers: { 'Content-Type': 'text/plain' }
            });
        }
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let iterations = 20;
    let start = Instant::now();

    for _ in 0..iterations {
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: HttpBody::None,
        };

        let (task, rx) = Task::fetch(req);
        worker.exec(task).await.unwrap();
        rx.await.unwrap();
    }

    let elapsed = start.elapsed();
    let avg = elapsed / iterations;

    println!("\n📊 Benchmark: Complex Scenario (timers + async)");
    println!("   Iterations: {}", iterations);
    println!("   Total time: {:?}", elapsed);
    println!("   Average: {:?}", avg);
    println!(
        "   Throughput: {:.2} req/s",
        iterations as f64 / elapsed.as_secs_f64()
    );
}

/// Performance comparison summary
#[tokio::test]
async fn bench_summary() {
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║        OpenWorkers Runtime V8 - Benchmark Summary       ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("Run with: cargo test --test benchmark_test -- --nocapture");
    println!();
    println!("Individual benchmarks:");
    println!("  • bench_simple_response      - Baseline throughput");
    println!("  • bench_async_response       - Async/await performance");
    println!("  • bench_worker_creation      - Worker initialization cost");
    println!("  • bench_complex_scenario     - Real-world scenario");
    println!();
}
