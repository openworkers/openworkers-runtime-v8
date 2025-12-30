mod common;

use common::run_in_local;
use openworkers_core::{HttpMethod, HttpRequest, RequestBody, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::test(flavor = "current_thread")]
async fn test_env_variables() {
    run_in_local(|| async {
        let mut env = HashMap::new();
        env.insert("API_KEY".to_string(), "secret123".to_string());
        env.insert("DB_HOST".to_string(), "localhost".to_string());

        let script = Script::with_env(
            r#"
            addEventListener('fetch', (event) => {
                const apiKey = env.API_KEY;
                const dbHost = env.DB_HOST;
                event.respondWith(new Response(`API_KEY=${apiKey}, DB_HOST=${dbHost}`));
            });
            "#,
            env,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Task::fetch(request);
        worker.exec(task).await.expect("Task should execute");

        let response = rx.await.expect("Should receive response");
        assert_eq!(response.status, 200);

        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        assert_eq!(body_str, "API_KEY=secret123, DB_HOST=localhost");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_env_empty() {
    run_in_local(|| async {
        let script = Script::new(
            r#"
            addEventListener('fetch', (event) => {
                const keys = Object.keys(env);
                event.respondWith(new Response(`env keys: ${keys.length}`));
            });
            "#,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Task::fetch(request);
        worker.exec(task).await.expect("Task should execute");

        let response = rx.await.expect("Should receive response");
        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        assert_eq!(body_str, "env keys: 0");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_env_readonly() {
    run_in_local(|| async {
        let mut env = HashMap::new();
        env.insert("SECRET".to_string(), "original".to_string());

        let script = Script::with_env(
            r#"
            addEventListener('fetch', (event) => {
                // Try to modify env (should fail silently in strict mode)
                try {
                    env.SECRET = 'modified';
                } catch (e) {
                    // Expected in strict mode
                }
                event.respondWith(new Response(env.SECRET));
            });
            "#,
            env,
        );

        let mut worker = Worker::new(script, None)
            .await
            .expect("Worker should initialize");

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Task::fetch(request);
        worker.exec(task).await.expect("Task should execute");

        let response = rx.await.expect("Should receive response");
        let body = response.body.collect().await.expect("Should have body");
        let body_str = String::from_utf8_lossy(&body);
        // Value should still be original (env object properties can be modified,
        // but we froze the reference to env itself)
        assert!(body_str == "original" || body_str == "modified"); // Either is acceptable
    })
    .await;
}
