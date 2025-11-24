use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};

#[tokio::main]
async fn main() {
    env_logger::init();

    println!("=== Testing setTimeout ===");

    let code = r#"
        addEventListener('fetch', (event) => {
            console.log('[JS] Fetch handler called');

            setTimeout(() => {
                console.log('[JS] setTimeout executed after 100ms!');
            }, 100);

            event.respondWith(new Response('Timer scheduled'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    println!("Worker created with event loop");

    // Create a fetch request to trigger the handler
    use std::collections::HashMap;
    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/test".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, _res_rx) = Task::fetch(req);

    println!("Executing fetch task...");
    let _ = worker.exec(task).await;

    println!("Waiting for timer to execute...");
    // Process callbacks periodically to allow timers to run
    for _i in 0..20 {
        worker.process_callbacks();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }

    println!("=== Done ===");
}
