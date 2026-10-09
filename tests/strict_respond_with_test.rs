//! With set_strict_respond_with(true), respondWith follows the Service Worker
//! spec. The switch is for the whole process, so these tests have a file of
//! their own; every other test runs in the default, lax, mode.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script};
use openworkers_runtime_v8::{Worker, set_strict_respond_with};

/// Serves one GET and answers the status, the body and how long it took.
async fn fetch(code: &str) -> (u16, String, Duration) {
    set_strict_respond_with(true);
    let code = code.to_string();

    run_in_local(|| async move {
        let limits = RuntimeLimits {
            max_wall_clock_time_ms: 5_000,
            ..RuntimeLimits::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();
        let (event, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        let started = Instant::now();
        worker.exec(event).await.unwrap();
        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();

        (
            response.status,
            String::from_utf8_lossy(&body).into_owned(),
            started.elapsed(),
        )
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_during_the_listener_answers() {
    let (status, body, _) =
        fetch("addEventListener('fetch', e => e.respondWith(new Response('now')));").await;

    assert_eq!((status, body.as_str()), (200, "now"));
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_after_an_await_fails_at_once() {
    let code = r#"
        addEventListener('fetch', async (e) => {
            setTimeout(() => {}, 3000);
            await null;
            e.respondWith(new Response('late'));
        });
    "#;
    let (status, body, took) = fetch(code).await;

    assert_eq!(
        (status, body.as_str()),
        (
            500,
            "Handler exception: the fetch listener did not call respondWith"
        )
    );
    assert!(took < Duration::from_secs(1), "took {took:?}");
}
