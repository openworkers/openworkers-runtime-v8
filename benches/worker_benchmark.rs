use criterion::{Criterion, criterion_group, criterion_main};
use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

fn bench_worker_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("Worker");

    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from V8!'));
        });
    "#;

    group.bench_function("new", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();

        b.iter(|| {
            rt.block_on(async {
                let script = Script::new(code);
                let worker = Worker::new(script, None, None).await.unwrap();
                drop(worker);
            });
        });
    });

    group.finish();
}

fn bench_worker_exec(c: &mut Criterion) {
    let mut group = c.benchmark_group("Worker");

    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from V8!'));
        });
    "#;

    group.bench_function("exec_simple_response", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();

        b.iter(|| {
            rt.block_on(async {
                let script = Script::new(code);
                let mut worker = Worker::new(script, None, None).await.unwrap();

                let req = HttpRequest {
                    method: "GET".to_string(),
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: None,
                };

                let (task, _rx) = Task::fetch(req);
                let _ = worker.exec(task).await;
            });
        });
    });

    group.finish();
}

fn bench_worker_json_response(c: &mut Criterion) {
    let mut group = c.benchmark_group("Worker");

    let code = r#"
        addEventListener('fetch', (event) => {
            const data = { message: 'Hello', timestamp: Date.now(), items: [1, 2, 3, 4, 5] };
            event.respondWith(new Response(JSON.stringify(data), {
                headers: { 'Content-Type': 'application/json' }
            }));
        });
    "#;

    group.bench_function("exec_json_response", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();

        b.iter(|| {
            rt.block_on(async {
                let script = Script::new(code);
                let mut worker = Worker::new(script, None, None).await.unwrap();

                let req = HttpRequest {
                    method: "GET".to_string(),
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: None,
                };

                let (task, _rx) = Task::fetch(req);
                let _ = worker.exec(task).await;
            });
        });
    });

    group.finish();
}

fn bench_worker_with_headers(c: &mut Criterion) {
    let mut group = c.benchmark_group("Worker");

    let code = r#"
        addEventListener('fetch', (event) => {
            const headers = new Headers();
            headers.set('Content-Type', 'text/html');
            headers.set('X-Custom-Header', 'custom-value');
            headers.set('Cache-Control', 'no-cache');
            event.respondWith(new Response('<h1>Hello</h1>', {
                status: 200,
                headers: headers
            }));
        });
    "#;

    group.bench_function("exec_with_headers", |b| {
        let rt = tokio::runtime::Runtime::new().unwrap();

        b.iter(|| {
            rt.block_on(async {
                let script = Script::new(code);
                let mut worker = Worker::new(script, None, None).await.unwrap();

                let req = HttpRequest {
                    method: "GET".to_string(),
                    url: "http://localhost/".to_string(),
                    headers: HashMap::new(),
                    body: None,
                };

                let (task, _rx) = Task::fetch(req);
                let _ = worker.exec(task).await;
            });
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_worker_creation,
    bench_worker_exec,
    bench_worker_json_response,
    bench_worker_with_headers
);
criterion_main!(benches);
