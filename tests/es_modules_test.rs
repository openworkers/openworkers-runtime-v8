//! ES Modules style handler tests

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script, WorkerCode};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_fetch() {
    run_in_local(|| async {
        let code = r#"
            globalThis.default = {
                async fetch(request, env, ctx) {
                    return new Response('Hello from ES Modules!', {
                        status: 200,
                        headers: { 'Content-Type': 'text/plain' }
                    });
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        assert_eq!(response.status, 200);
        let body = response.body.collect().await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "Hello from ES Modules!"
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_with_env() {
    run_in_local(|| async {
        let code = r#"
            globalThis.default = {
                async fetch(request, env, ctx) {
                    const value = env.TEST_VAR || 'not set';
                    return new Response('Value: ' + value);
                }
            };
        "#;

        let mut env = HashMap::new();
        env.insert("TEST_VAR".to_string(), "hello".to_string());

        let script = Script {
            code: WorkerCode::JavaScript(code.to_string()),
            env: Some(env),
            bindings: vec![],
        };

        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "Value: hello");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_priority_over_addeventlistener() {
    run_in_local(|| async {
        // Both styles defined - ES Modules should take priority
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith(new Response('From addEventListener'));
            });

            globalThis.default = {
                async fetch(request, env, ctx) {
                    return new Response('From ES Modules');
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "From ES Modules");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_wait_until() {
    run_in_local(|| async {
        // Test that waitUntil promises are awaited before handler completes
        let code = r#"
            globalThis.waitUntilCompleted = false;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    // Schedule background work via waitUntil
                    ctx.waitUntil(
                        new Promise(resolve => {
                            setTimeout(() => {
                                globalThis.waitUntilCompleted = true;
                                resolve();
                            }, 50);
                        })
                    );

                    return new Response('Response sent');
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "Response sent");

        // Verify waitUntil completed by checking the global variable
        worker
            .evaluate(
                "if (!globalThis.waitUntilCompleted) throw new Error('waitUntil not completed');",
            )
            .expect("waitUntil should have completed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_multiple_wait_until() {
    run_in_local(|| async {
        // Test multiple waitUntil calls
        let code = r#"
            globalThis.waitUntilCount = 0;

            globalThis.default = {
                async fetch(request, env, ctx) {
                    // Schedule multiple background tasks
                    ctx.waitUntil(
                        new Promise(resolve => {
                            setTimeout(() => {
                                globalThis.waitUntilCount++;
                                resolve();
                            }, 20);
                        })
                    );

                    ctx.waitUntil(
                        new Promise(resolve => {
                            setTimeout(() => {
                                globalThis.waitUntilCount++;
                                resolve();
                            }, 40);
                        })
                    );

                    ctx.waitUntil(
                        new Promise(resolve => {
                            setTimeout(() => {
                                globalThis.waitUntilCount++;
                                resolve();
                            }, 60);
                        })
                    );

                    return new Response('Response sent');
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let _response = rx.await.unwrap();

        // Verify all waitUntil callbacks completed
        worker
            .evaluate("if (globalThis.waitUntilCount !== 3) throw new Error('Expected 3, got ' + globalThis.waitUntilCount);")
            .expect("All 3 waitUntil promises should have completed");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_service_worker_wait_until() {
    run_in_local(|| async {
        // Test waitUntil with addEventListener style
        let code = r#"
            globalThis.waitUntilCompleted = false;

            addEventListener('fetch', (event) => {
                event.waitUntil(
                    new Promise(resolve => {
                        setTimeout(() => {
                            globalThis.waitUntilCompleted = true;
                            resolve();
                        }, 50);
                    })
                );

                event.respondWith(new Response('From Service Worker'));
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

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = response.body.collect().await.unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "From Service Worker");

        // Verify waitUntil completed
        worker
            .evaluate(
                "if (!globalThis.waitUntilCompleted) throw new Error('waitUntil not completed');",
            )
            .expect("waitUntil should have completed");
    })
    .await;
}

/// Test ES modules with SSE streaming response (the /slow pattern)
#[tokio::test(flavor = "current_thread")]
async fn test_es_modules_sse_streaming() {
    run_in_local(|| async {
        let code = r#"
            globalThis.default = {
                async fetch(request, env, ctx) {
                    const url = new URL(request.url);
                    const target = parseInt(url.searchParams.get('target') || '3') || 3;

                    const stream = new ReadableStream({
                        async start(controller) {
                            for (let i = 1; i <= target; i++) {
                                await new Promise(resolve => setTimeout(resolve, 50));
                                controller.enqueue(`data: ${JSON.stringify({ elapsed: i, target })}\n\n`);
                            }

                            controller.enqueue(`data: ${JSON.stringify({ done: true, target })}\n\n`);
                            controller.close();
                        }
                    });

                    return new Response(stream, {
                        headers: {
                            'Content-Type': 'text/event-stream',
                            'Cache-Control': 'no-cache',
                            'Connection': 'keep-alive'
                        }
                    });
                }
            };
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/slow?target=3".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);

        // Should complete within 5 seconds (3 events * 50ms = 150ms minimum)
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), worker.exec(task)).await;

        assert!(result.is_ok(), "Should complete within timeout");
        result.unwrap().unwrap();

        let response = rx.await.unwrap();
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.unwrap();
        let body_str = String::from_utf8_lossy(&body);

        // Verify SSE events
        assert!(
            body_str.contains("\"elapsed\":1") || body_str.contains("\"elapsed\": 1"),
            "Should have elapsed 1: {}",
            body_str
        );
        assert!(
            body_str.contains("\"elapsed\":3") || body_str.contains("\"elapsed\": 3"),
            "Should have elapsed 3: {}",
            body_str
        );
        assert!(
            body_str.contains("\"done\":true") || body_str.contains("\"done\": true"),
            "Should have done event: {}",
            body_str
        );
    })
    .await;
}
