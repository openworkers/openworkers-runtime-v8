use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;
use std::time::Instant;

#[tokio::main]
async fn main() {
    env_logger::init();

    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from V8!'));
        });
    "#;

    println!("=== V8 Full Worker Benchmark (with exec) ===\n");

    let iterations = 5;
    let mut creation_times = Vec::new();
    let mut exec_times = Vec::new();
    let mut total_times = Vec::new();

    for i in 1..=iterations {
        println!("Iteration {}/{}:", i, iterations);

        let total_start = Instant::now();

        // Measure Worker::new
        let worker_start = Instant::now();
        let script = Script::with_env(code.to_string(), HashMap::new());
        let mut worker = match Worker::new(script, None, None).await {
            Ok(w) => w,
            Err(e) => {
                eprintln!("  Worker creation error: {}", e);
                continue;
            }
        };
        let worker_time = worker_start.elapsed();
        creation_times.push(worker_time.as_micros());
        println!("  Worker::new(): {:?}", worker_time);

        // Measure exec
        let exec_start = Instant::now();
        let req = HttpRequest {
            method: "GET".to_string(),
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: None,
        };

        let (task, _rx) = Task::fetch(req);

        match worker.exec(task).await {
            Ok(reason) => {
                let exec_time = exec_start.elapsed();
                exec_times.push(exec_time.as_micros());
                println!("  exec():        {:?} (reason: {:?})", exec_time, reason);
            }
            Err(e) => {
                eprintln!("  Exec error: {}", e);
                continue;
            }
        }

        let total_time = total_start.elapsed();
        total_times.push(total_time.as_micros());
        println!("  Total:         {:?}", total_time);
        println!();
    }

    if !total_times.is_empty() {
        println!("=== Summary ===");

        let avg_creation = creation_times.iter().sum::<u128>() / creation_times.len() as u128;
        let avg_exec = exec_times.iter().sum::<u128>() / exec_times.len() as u128;
        let avg_total = total_times.iter().sum::<u128>() / total_times.len() as u128;

        println!(
            "Worker::new(): avg={}µs, min={}µs, max={}µs",
            avg_creation,
            creation_times.iter().min().unwrap(),
            creation_times.iter().max().unwrap()
        );
        println!(
            "exec():        avg={}µs, min={}µs, max={}µs",
            avg_exec,
            exec_times.iter().min().unwrap(),
            exec_times.iter().max().unwrap()
        );
        println!(
            "Total:         avg={}µs, min={}µs, max={}µs",
            avg_total,
            total_times.iter().min().unwrap(),
            total_times.iter().max().unwrap()
        );
    }
}
