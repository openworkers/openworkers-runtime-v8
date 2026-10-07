//! How a request or a task reaches the guest's handler and how its answer
//! comes back: both handler styles, their priority, errors, waitUntil and a
//! streamed body. The JavaScript under test is src/js/dispatch.js and
//! src/js/modules.js.

mod common;

use std::collections::HashMap;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, ResponseBody, RuntimeLimits, Script, TaskResult,
};
use openworkers_runtime_v8::Worker;
use serde_json::json;

fn limits() -> RuntimeLimits {
    RuntimeLimits {
        max_wall_clock_time_ms: 3_000,
        ..RuntimeLimits::default()
    }
}

/// Serves one GET and answers the status and the body, streamed or not.
async fn fetch(code: &str) -> (u16, String) {
    let code = code.to_string();

    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        worker.exec(task).await.unwrap();

        let response = rx.await.unwrap();
        let status = response.status;
        let body = response.body.collect().await.unwrap().unwrap();

        (status, String::from_utf8_lossy(&body).into_owned())
    })
    .await
}

/// Runs one invoked task with `payload` and answers its result.
async fn task(code: &str, payload: serde_json::Value) -> TaskResult {
    let code = code.to_string();

    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();
        let (event, rx) = Event::invoke("task-1".to_string(), Some(payload), None);

        worker.exec(event).await.ok();

        rx.await.unwrap()
    })
    .await
}

// fetch, addEventListener

#[tokio::test(flavor = "current_thread")]
async fn respond_with_a_response() {
    let code =
        "addEventListener('fetch', e => e.respondWith(new Response('ok', { status: 201 })));";

    assert_eq!(fetch(code).await, (201, "ok".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_a_promise() {
    let code =
        "addEventListener('fetch', e => e.respondWith(Promise.resolve(new Response('later'))));";

    assert_eq!(fetch(code).await, (200, "later".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_returned_response_answers() {
    let code = "addEventListener('fetch', () => new Response('returned'));";

    assert_eq!(fetch(code).await, (200, "returned".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_returned_promise_answers() {
    let code = "addEventListener('fetch', async () => new Response('async'));";

    assert_eq!(fetch(code).await, (200, "async".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_wins_over_a_returned_response() {
    let code = r#"
        addEventListener('fetch', e => {
            e.respondWith(new Response('respondWith'));
            return new Response('returned');
        });
    "#;

    assert_eq!(fetch(code).await, (200, "respondWith".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_throwing_listener_answers_500() {
    let code = "addEventListener('fetch', () => { throw new Error('boom'); });";

    assert_eq!(fetch(code).await, (500, "Handler exception: boom".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejected_respond_with_answers_500() {
    let code = "addEventListener('fetch', e => e.respondWith(Promise.reject(new Error('boom'))));";

    assert_eq!(fetch(code).await, (500, "Handler exception: boom".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejected_returned_promise_answers_500() {
    let code = "addEventListener('fetch', async () => { throw new Error('boom'); });";

    assert_eq!(fetch(code).await, (500, "Handler exception: boom".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn wait_until_finishes_before_exec_returns() {
    let code = r#"
        globalThis.later = 0;
        addEventListener('fetch', e => {
            e.waitUntil(new Promise(r => setTimeout(() => { globalThis.later = 1; r(); }, 30)));
            e.respondWith(new Response('now'));
        });
    "#
    .to_string();

    let later = run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        worker.exec(task).await.unwrap();
        assert_eq!(rx.await.unwrap().status, 200);

        worker.get_global_u32("later")
    })
    .await;

    assert_eq!(later, Some(1));
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejected_wait_until_keeps_the_response() {
    let code = r#"
        addEventListener('fetch', e => {
            e.waitUntil(Promise.reject(new Error('background')));
            e.respondWith(new Response('kept'));
        });
    "#;

    assert_eq!(fetch(code).await, (200, "kept".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn no_handler_answers_501() {
    assert_eq!(
        fetch("globalThis.nothing = 1;").await,
        (501, "Worker does not implement fetch handler".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_streamed_body_arrives_whole() {
    let code = r#"
        addEventListener('fetch', e => {
            const body = new ReadableStream({
                start(controller) {
                    controller.enqueue(new TextEncoder().encode('one,'));
                    controller.enqueue(new TextEncoder().encode('two'));
                    controller.close();
                },
            });
            e.respondWith(new Response(body));
        });
    "#
    .to_string();

    let (streamed, text) = run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), Some(limits()))
            .await
            .unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        worker.exec(task).await.unwrap();

        let response = rx.await.unwrap();
        let streamed = matches!(response.body, ResponseBody::Stream(_));
        let body = response.body.collect().await.unwrap().unwrap();

        (streamed, String::from_utf8_lossy(&body).into_owned())
    })
    .await;

    assert!(streamed, "a ReadableStream body goes out as a stream");
    assert_eq!(text, "one,two");
}

// fetch, export default

#[tokio::test(flavor = "current_thread")]
async fn a_module_fetch_gets_env_and_ctx() {
    let code = r#"
        globalThis.default = {
            fetch(request, env, ctx) {
                ctx.passThroughOnException();
                const shapes = [typeof request.url, typeof env, typeof ctx.waitUntil].join(',');
                return new Response(shapes);
            },
        };
    "#;

    assert_eq!(fetch(code).await, (200, "string,object,function".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_throwing_module_fetch_answers_500() {
    let code = "globalThis.default = { async fetch() { throw new Error('boom'); } };";

    assert_eq!(fetch(code).await, (500, "Handler exception: boom".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_without_fetch_answers_501() {
    let code = "globalThis.default = { scheduled() {} };";

    assert_eq!(
        fetch(code).await,
        (501, "Worker does not implement fetch handler".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_default_that_is_not_an_object_answers_501() {
    assert_eq!(
        fetch("globalThis.default = 42;").await,
        (501, "Worker does not implement fetch handler".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_fetch_wins_over_a_listener() {
    let code = r#"
        addEventListener('fetch', e => e.respondWith(new Response('listener')));
        globalThis.default = { fetch: () => new Response('module') };
    "#;

    assert_eq!(fetch(code).await, (200, "module".into()));
}

// task, addEventListener

#[tokio::test(flavor = "current_thread")]
async fn a_task_listener_answers_with_respond_with() {
    let code =
        "addEventListener('task', e => e.respondWith({ success: true, data: e.payload.n * 2 }));";
    let result = task(code, json!({ "n": 21 })).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.data, Some(json!(42)));
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_listener_answers_with_its_value() {
    let code = "addEventListener('task', e => e.payload.n + 1);";
    let result = task(code, json!({ "n": 1 })).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.data, Some(json!(2)));
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_listener_can_report_a_failure() {
    let code = "addEventListener('task', () => ({ success: false, error: 'refused' }));";
    let result = task(code, json!(null)).await;

    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("refused"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_throwing_task_listener_fails_the_task() {
    let code = "addEventListener('task', () => { throw new Error('boom'); });";
    let result = task(code, json!(null)).await;

    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("boom"));
}

// task, export default

#[tokio::test(flavor = "current_thread")]
async fn a_module_task_answers_with_its_value() {
    let code =
        "globalThis.default = { async task(event, env, ctx) { return event.payload.n * 3; } };";
    let result = task(code, json!({ "n": 2 })).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.data, Some(json!(6)));
}

#[tokio::test(flavor = "current_thread")]
async fn a_throwing_module_task_fails_the_task() {
    let code = "globalThis.default = { task() { throw new Error('boom'); } };";
    let result = task(code, json!(null)).await;

    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("boom"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_with_scheduled_alone_serves_an_invoked_task() {
    let code = "globalThis.default = { scheduled() { globalThis.ran = 1; } };";
    let result = task(code, json!(null)).await;

    assert!(result.success, "{:?}", result.error);
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_from_a_timer_answers() {
    let code = "addEventListener('fetch', e => { setTimeout(() => e.respondWith(new Response('late')), 20); });";

    assert_eq!(fetch(code).await, (200, "late".into()));
}

// what the dispatch refuses, at once rather than at the wall clock

#[tokio::test(flavor = "current_thread")]
async fn a_module_fetch_that_returns_nothing_answers_500() {
    assert_eq!(
        fetch("globalThis.default = { fetch() {} };").await,
        (
            500,
            "Handler exception: the fetch handler did not respond".into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn respond_with_something_else_than_a_response_answers_500() {
    assert_eq!(
        fetch("addEventListener('fetch', e => e.respondWith('text'));").await,
        (
            500,
            "Handler exception: the fetch handler did not answer with a Response".into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_respond_with_keeps_the_first_response() {
    let code = r#"
        addEventListener('fetch', e => {
            e.respondWith(new Response('first'));
            e.respondWith(new Response('second'));
        });
    "#;

    assert_eq!(fetch(code).await, (200, "first".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_without_a_task_handler_fails_at_once() {
    let started = std::time::Instant::now();
    let code = "addEventListener('fetch', e => e.respondWith(new Response('x')));";
    let result = task(code, json!(null)).await;

    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("Worker does not implement task handler")
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_with_fetch_alone_refuses_a_task() {
    let code = "globalThis.default = { fetch: () => new Response('x') };";
    let result = task(code, json!(null)).await;

    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("Worker does not implement task handler")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_respond_with_keeps_plain_data() {
    let code = "addEventListener('task', e => e.respondWith({ rows: 3 }));";
    let result = task(code, json!(null)).await;

    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.data, Some(json!({ "rows": 3 })));
}

// what the guest can no longer reach

#[tokio::test(flavor = "current_thread")]
async fn the_guest_sees_no_dispatch_internals() {
    let code = r#"
        addEventListener('fetch', e => {
            const internals = [
                '__triggerFetch', '__taskHandler', '__scheduledHandler', '__lastResponse',
                '__requestComplete', '__taskResult', '__activeResponseStreams',
                '__lastResponseStreamId', '__signalClientDisconnect', '__streamResponseBody',
            ];
            e.respondWith(new Response(internals.filter(name => name in globalThis).join(',') || 'none'));
        });
    "#;

    assert_eq!(fetch(code).await, (200, "none".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn globals_the_guest_sets_do_not_answer_for_it() {
    let code = r#"
        addEventListener('fetch', e => {
            globalThis.__lastResponse = new Response('fake', { status: 418 });
            globalThis.__requestComplete = true;
            setTimeout(() => e.respondWith(new Response('real')), 20);
        });
    "#;

    assert_eq!(fetch(code).await, (200, "real".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_replaced_text_encoder_does_not_break_a_streamed_body() {
    let code = r#"
        addEventListener('fetch', e => {
            globalThis.TextEncoder = class { encode() { throw new Error('replaced'); } };
            const body = new ReadableStream({
                start(controller) {
                    controller.enqueue('text chunk');
                    controller.close();
                },
            });
            e.respondWith(new Response(body));
        });
    "#;

    assert_eq!(fetch(code).await, (200, "text chunk".into()));
}
