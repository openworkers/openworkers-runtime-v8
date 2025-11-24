use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

#[tokio::main]
async fn main() {
    env_logger::init();

    println!("\n=== Testing setTimeout ===");
    test_set_timeout().await;

    println!("\n=== Testing setInterval ===");
    test_set_interval().await;

    println!("\n=== Testing clearTimeout ===");
    test_clear_timeout().await;

    println!("\n=== Testing clearInterval ===");
    test_clear_interval().await;

    println!("\n=== All timer tests completed! ===");
}

async fn test_set_timeout() {
    let code = r#"
        addEventListener('fetch', (event) => {
            console.log('[setTimeout] Starting test');

            setTimeout(() => {
                console.log('[setTimeout] Timer 1 executed after 50ms');
            }, 50);

            setTimeout(() => {
                console.log('[setTimeout] Timer 2 executed after 150ms');
            }, 150);

            event.respondWith(new Response('setTimeout scheduled'));
        });
    "#;

    let mut worker = create_worker(code).await;
    trigger_fetch(&mut worker).await;

    // Process callbacks for 300ms
    process_for_duration(&mut worker, 300).await;
}

async fn test_set_interval() {
    let code = r#"
        let count = 0;
        addEventListener('fetch', (event) => {
            console.log('[setInterval] Starting test');

            setInterval(() => {
                count++;
                console.log('[setInterval] Interval tick #' + count);
            }, 50);

            event.respondWith(new Response('setInterval scheduled'));
        });
    "#;

    let mut worker = create_worker(code).await;
    trigger_fetch(&mut worker).await;

    // Process callbacks for 250ms (should see ~5 ticks)
    process_for_duration(&mut worker, 250).await;
}

async fn test_clear_timeout() {
    let code = r#"
        addEventListener('fetch', (event) => {
            console.log('[clearTimeout] Starting test');

            const timerId = setTimeout(() => {
                console.log('[clearTimeout] ERROR: This should NOT execute!');
            }, 100);

            setTimeout(() => {
                console.log('[clearTimeout] Clearing timer ' + timerId);
                clearTimeout(timerId);
            }, 20);

            setTimeout(() => {
                console.log('[clearTimeout] Test complete (timer was cleared)');
            }, 200);

            event.respondWith(new Response('clearTimeout test'));
        });
    "#;

    let mut worker = create_worker(code).await;
    trigger_fetch(&mut worker).await;

    process_for_duration(&mut worker, 300).await;
}

async fn test_clear_interval() {
    let code = r#"
        let count = 0;
        let intervalId;

        addEventListener('fetch', (event) => {
            console.log('[clearInterval] Starting test');

            intervalId = setInterval(() => {
                count++;
                console.log('[clearInterval] Interval tick #' + count);

                if (count >= 3) {
                    console.log('[clearInterval] Clearing interval');
                    clearInterval(intervalId);
                }
            }, 50);

            event.respondWith(new Response('clearInterval test'));
        });
    "#;

    let mut worker = create_worker(code).await;
    trigger_fetch(&mut worker).await;

    // Process callbacks for 400ms (should see exactly 3 ticks, then stop)
    process_for_duration(&mut worker, 400).await;
}

// Helper functions

async fn create_worker(code: &str) -> Worker {
    let script = Script::new(code);
    Worker::new(script, None, None).await.unwrap()
}

async fn trigger_fetch(worker: &mut Worker) {
    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/test".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, _res_rx) = Task::fetch(req);
    let _ = worker.exec(task).await;
}

async fn process_for_duration(worker: &mut Worker, ms: u64) {
    let iterations = ms / 10;
    for _ in 0..iterations {
        worker.process_callbacks();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}
