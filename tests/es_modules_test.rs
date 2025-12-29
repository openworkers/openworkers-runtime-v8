//! ES Modules style handler tests

use openworkers_runtime_v8::{HttpMethod, HttpRequest, RequestBody, Script, Task, Worker};
use std::collections::HashMap;

#[tokio::test]
async fn test_es_modules_fetch() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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

            let (task, rx) = Task::fetch(req);
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

#[tokio::test]
async fn test_es_modules_with_env() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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
                code: code.to_string(),
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

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "Value: hello");
        })
        .await;
}

#[tokio::test]
async fn test_es_modules_priority_over_addeventlistener() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "From ES Modules");
        })
        .await;
}

#[tokio::test]
async fn test_es_modules_wait_until() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "Response sent");

            // Verify waitUntil completed by checking the global variable
            worker
                .evaluate("if (!globalThis.waitUntilCompleted) throw new Error('waitUntil not completed');")
                .expect("waitUntil should have completed");
        })
        .await;
}

#[tokio::test]
async fn test_es_modules_multiple_wait_until() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();
            let _response = rx.await.unwrap();

            // Verify all waitUntil callbacks completed
            worker
                .evaluate("if (globalThis.waitUntilCount !== 3) throw new Error('Expected 3, got ' + globalThis.waitUntilCount);")
                .expect("All 3 waitUntil promises should have completed");
        })
        .await;
}

#[tokio::test]
async fn test_service_worker_wait_until() {
    let local = tokio::task::LocalSet::new();

    local
        .run_until(async {
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

            let (task, rx) = Task::fetch(req);
            worker.exec(task).await.unwrap();
            let response = rx.await.unwrap();

            let body = response.body.collect().await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), "From Service Worker");

            // Verify waitUntil completed
            worker
                .evaluate("if (!globalThis.waitUntilCompleted) throw new Error('waitUntil not completed');")
                .expect("waitUntil should have completed");
        })
        .await;
}
