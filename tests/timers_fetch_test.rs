use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test]
async fn test_set_timeout() {
    let code = r#"
        let executed = false;
        addEventListener('fetch', (event) => {
            setTimeout(() => {
                executed = true;
            }, 50);
            event.respondWith(new Response('Timer scheduled'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);

    // Process callbacks to allow timer to execute
    for _ in 0..20 {
        worker.process_callbacks();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_set_interval_and_clear() {
    let code = r#"
        let count = 0;
        let intervalId;

        addEventListener('fetch', (event) => {
            intervalId = setInterval(() => {
                count++;
                if (count >= 3) {
                    clearInterval(intervalId);
                }
            }, 20);
            event.respondWith(new Response('Interval scheduled'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    rx.await.unwrap();

    // Process callbacks to allow intervals to execute
    for _ in 0..30 {
        worker.process_callbacks();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_clear_timeout() {
    let code = r#"
        let shouldNotExecute = false;

        addEventListener('fetch', (event) => {
            const timerId = setTimeout(() => {
                shouldNotExecute = true;
            }, 100);

            clearTimeout(timerId);
            event.respondWith(new Response('Timeout cleared'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    rx.await.unwrap();

    // Process callbacks
    for _ in 0..20 {
        worker.process_callbacks();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_async_response() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(
                handleRequest().catch(err => new Response(String(err), { status: 500 }))
            );
        });

        async function handleRequest() {
            // Simulate async work
            await new Promise(resolve => setTimeout(resolve, 10));
            return new Response('Async response!', { status: 200 });
        }
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;
    assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&response.body.collect().await.unwrap()),
        "Async response!"
    );
}

#[tokio::test]
async fn test_fetch_forward() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(handleRequest());
        });

        async function handleRequest() {
            // Try to fetch from httpbin.workers.rocks (dogfooding!)
            try {
                const response = await fetch('https://httpbin.workers.rocks/get');
                return new Response('Fetch completed!', { status: 200 });
            } catch (error) {
                return new Response('Fetch failed but handled: ' + error.message, { status: 200 });
            }
        }
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;
    // Fetch may fail (network issues) but should be handled gracefully
    // We accept both Ok (fetch worked) and Exception (but caught)
    assert!(
        result.is_ok()
            || matches!(
                result,
                Err(openworkers_runtime_v8::TerminationReason::Exception(_))
            ),
        "Expected Ok or Exception, got {:?}",
        result
    );

    // Try to get response with timeout
    if let Ok(Ok(response)) = tokio::time::timeout(tokio::time::Duration::from_secs(10), rx).await {
        assert_eq!(response.status, 200);
        let body_bytes = response.body.collect().await.unwrap();
        let body = String::from_utf8_lossy(&body_bytes);
        // Either fetch succeeded or was handled
        assert!(body.contains("Fetch completed") || body.contains("Fetch failed but handled"));
    }
    // If timeout or channel error, that's also ok (network issue)
}

#[tokio::test]
async fn test_promise_rejection_handling() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(
                Promise.reject(new Error('Test error')).catch(err => {
                    return new Response('Error handled: ' + err.message, { status: 500 });
                })
            );
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.status, 500);
    let body_bytes = response.body.collect().await.unwrap();
    let body = String::from_utf8_lossy(&body_bytes);
    assert!(body.contains("Error handled: Test error"));
}

#[tokio::test]
async fn test_multiple_async_operations() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(handleRequest());
        });

        async function handleRequest() {
            // Multiple async operations
            await new Promise(resolve => setTimeout(resolve, 10));
            await new Promise(resolve => setTimeout(resolve, 10));
            await new Promise(resolve => setTimeout(resolve, 10));

            return new Response('All async ops completed!', { status: 200 });
        }
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&response.body.collect().await.unwrap()),
        "All async ops completed!"
    );
}

#[tokio::test]
async fn test_custom_headers() {
    let code = r#"
        addEventListener('fetch', (event) => {
            event.respondWith(new Response('OK', {
                status: 200,
                headers: {
                    'Content-Type': 'application/json',
                    'X-Custom-Header': 'test-value'
                }
            }));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();

    let response = tokio::time::timeout(tokio::time::Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.status, 200);

    // Check headers
    let content_type = response
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "content-type")
        .map(|(_, v)| v.as_str());
    assert_eq!(content_type, Some("application/json"));

    let custom_header = response
        .headers
        .iter()
        .find(|(k, _)| k.to_lowercase() == "x-custom-header")
        .map(|(_, v)| v.as_str());
    assert_eq!(custom_header, Some("test-value"));
}
