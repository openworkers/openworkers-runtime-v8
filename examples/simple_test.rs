use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

#[tokio::main]
async fn main() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('Hello from V8!', { status: 200 }));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/test".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = rx.await.unwrap();
    println!("Status: {}", response.status);
    if let Some(body) = response.body.as_bytes() {
        println!("Body: {}", String::from_utf8_lossy(body));
    }
}
