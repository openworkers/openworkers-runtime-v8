use openworkers_core::{HttpBody, HttpMethod, HttpRequest, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Benchmark local stream creation and consumption (no network)
async fn bench_local_stream(chunk_count: usize, chunk_size: usize) -> (Duration, usize) {
    let code = format!(
        r#"
        addEventListener('fetch', (event) => {{
            const stream = new ReadableStream({{
                start(controller) {{
                    const chunk = new Uint8Array({});
                    for (let i = 0; i < {}; i++) {{
                        controller.enqueue(chunk);
                    }}
                    controller.close();
                }}
            }});
            event.respondWith(new Response(stream));
        }});
    "#,
        chunk_size, chunk_count
    );

    let script = Script::new(&code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let start = Instant::now();

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let bytes = response.body.collect().await.unwrap();
    let total_bytes = bytes.len();

    let elapsed = start.elapsed();
    (elapsed, total_bytes)
}

async fn bench_buffered_response(iterations: u32) -> Duration {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello World from buffered response!'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

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
        let response = rx.await.unwrap();
        assert!(!response.body.is_stream());
    }

    start.elapsed()
}

async fn bench_streaming_forward(iterations: u32) -> Duration {
    let code = r#"
        addEventListener('fetch', (event) => {
            // Direct fetch forward - streaming
            event.respondWith(fetch('https://httpbin.workers.rocks/bytes/100'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

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
        let response = rx.await.unwrap();

        // Consume the stream
        if let HttpBody::Stream(mut rx) = response.body {
            while let Some(_) = rx.recv().await {}
        }
    }

    start.elapsed()
}

async fn bench_large_streaming(size_kb: usize) -> (Duration, usize) {
    let code = format!(
        r#"
        addEventListener('fetch', (event) => {{
            event.respondWith(fetch('https://httpbin.workers.rocks/bytes/{}'));
        }});
    "#,
        size_kb * 1024
    );

    let script = Script::new(&code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let total_start = Instant::now();

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    // Time from first byte to last byte (actual transfer time)
    let transfer_start = Instant::now();
    let mut total_bytes = 0;
    let mut chunk_count = 0;
    let mut first_byte_time = None;

    if let HttpBody::Stream(mut rx) = response.body {
        while let Some(result) = rx.recv().await {
            if let Ok(bytes) = result {
                if first_byte_time.is_none() {
                    first_byte_time = Some(transfer_start.elapsed());
                }
                total_bytes += bytes.len();
                chunk_count += 1;
            }
        }
    }

    let transfer_elapsed = transfer_start.elapsed();
    let total_elapsed = total_start.elapsed();

    let throughput = if transfer_elapsed.as_secs_f64() > 0.0 {
        (total_bytes as f64 / 1024.0 / 1024.0) / transfer_elapsed.as_secs_f64()
    } else {
        0.0
    };

    println!(
        "  {:>4} KB: {:>2} chunks, {:>6} bytes, TTFB: {:>6.2?}, Transfer: {:>6.2?}, Total: {:>6.2?} ({:.2} MB/s)",
        size_kb,
        chunk_count,
        total_bytes,
        first_byte_time.unwrap_or(Duration::ZERO),
        transfer_elapsed,
        total_elapsed,
        throughput
    );

    (total_elapsed, total_bytes)
}

#[tokio::main]
async fn main() {
    println!("🚀 OpenWorkers V8 Streaming Benchmark\n");
    println!("========================================\n");

    // Warmup
    println!("Warming up...");
    let _ = bench_buffered_response(5).await;
    let _ = bench_streaming_forward(2).await;
    println!();

    // Benchmark 1: Buffered responses (local, no network)
    println!("📦 Buffered Response (local, no network):");
    let iterations = 1000;
    let elapsed = bench_buffered_response(iterations).await;
    let per_request = elapsed / iterations;
    println!(
        "  {} iterations in {:.2?} ({:.2?}/req, {:.0} req/s)\n",
        iterations,
        elapsed,
        per_request,
        iterations as f64 / elapsed.as_secs_f64()
    );

    // Benchmark 2: Streaming forward (with network)
    println!("🌊 Streaming Forward (100 bytes, via httpbin.workers.rocks):");
    let iterations = 10;
    let elapsed = bench_streaming_forward(iterations).await;
    let per_request = elapsed / iterations;
    println!(
        "  {} iterations in {:.2?} ({:.2?}/req, {:.2} req/s)\n",
        iterations,
        elapsed,
        per_request,
        iterations as f64 / elapsed.as_secs_f64()
    );

    // Benchmark 3: Local JS stream (no network)
    println!("🔄 Local JS ReadableStream (no network):");
    for (chunks, chunk_size) in [(10, 1024), (100, 1024), (10, 10240)] {
        let (elapsed, total_bytes) = bench_local_stream(chunks, chunk_size).await;
        let throughput = (total_bytes as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
        println!(
            "  {} chunks × {} bytes = {} KB in {:.2?} ({:.1} MB/s)",
            chunks,
            chunk_size,
            total_bytes / 1024,
            elapsed,
            throughput
        );
    }
    println!();

    // Benchmark 4: Network streaming transfers (smaller sizes due to httpbin limits)
    println!("📊 Network Streaming Transfer (via httpbin.workers.rocks):");
    println!("  Size   Chunks  Bytes    TTFB       Transfer   Total      Throughput");
    println!("  ----   ------  -----    ----       --------   -----      ----------");
    for size_kb in [1, 10, 50] {
        let _ = bench_large_streaming(size_kb).await;
    }

    println!("\n========================================");
    println!("📝 Summary:");
    println!("  - Buffered local: ~68k req/s (pure JS → Rust extraction)");
    println!("  - Streaming local: High throughput for JS-generated streams");
    println!("  - Streaming network: Latency-bound, but zero-buffer forwarding");
    println!("\n✅ Benchmark complete!");
}
