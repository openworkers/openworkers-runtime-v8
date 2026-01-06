use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let local = LocalSet::new();

    local
        .run_until(async {
            let code = r#"
               addEventListener('fetch', (event) => {
                    event.respondWith(new Response('Hello from V8!', { status: 200 }));
                });
            "#;

            let script = Script::new(code);
            let mut worker = Worker::new(script, None).await.unwrap();

            let req = HttpRequest {
                method: HttpMethod::Get,
                url: "http://localhost/test".to_string(),
                headers: HashMap::new(),
                body: RequestBody::None,
            };

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();

            let response = rx.await.unwrap();
            println!("Status: {}", response.status);

            if let Some(body) = response.body.collect().await {
                println!("Body: {}", String::from_utf8_lossy(&body));
            }
        })
        .await;
}
