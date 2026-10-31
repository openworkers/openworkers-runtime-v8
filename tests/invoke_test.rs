//! env.<NAME>.invoke(command, args) reaches the host handler of the invoke
//! binding, and its answer settles the promise.

#![cfg(feature = "invoke")]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::run_in_local;
use openworkers_core::{
    BindingInfo, Event, HttpMethod, HttpRequest, InvokeResult, OpFuture, OperationsHandle,
    OperationsHandler, RequestBody, Script,
};
use openworkers_runtime_v8::Worker;
use serde_json::{Value, json};

type Call = (String, String, Value);

/// Answers three functions and records every call it gets.
#[derive(Default)]
struct Host {
    calls: Mutex<Vec<Call>>,
}

impl OperationsHandler for Host {
    fn handle_binding_invoke(
        &self,
        binding: &str,
        name: &str,
        args: Value,
    ) -> OpFuture<'_, InvokeResult> {
        self.calls
            .lock()
            .unwrap()
            .push((binding.to_string(), name.to_string(), args.clone()));

        let answer = match name {
            "add" => {
                let sum = args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0);

                InvokeResult::Value(json!({ "sum": sum }))
            }
            "nothing" => InvokeResult::Value(Value::Null),
            "fail" => InvokeResult::Error("the disk is full".to_string()),
            // Not ready at the first poll: the scheduler drives it
            "later" => {
                return Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

                    InvokeResult::Value(json!("late"))
                });
            }
            "panic now" => panic!("the host function panics when called"),
            // Not ready at the first poll, then panics in the task of the scheduler
            "panic later" => {
                return Box::pin(async {
                    tokio::task::yield_now().await;
                    panic!("the host function panics at its second poll")
                });
            }
            "forever" => return Box::pin(std::future::pending()),
            "panic in poll" => {
                return Box::pin(async { panic!("the host function panics when polled") });
            }
            // Works 100 ms in Rust, past the CPU limit of 50 ms
            "spin" => {
                let end = std::time::Instant::now() + std::time::Duration::from_millis(100);

                while std::time::Instant::now() < end {}

                InvokeResult::Value(Value::Null)
            }
            other => InvokeResult::Error(format!("no function named '{other}'")),
        };

        Box::pin(async move { answer })
    }
}

/// Runs `body`, the inside of an async function that returns a string, in a
/// fetch handler with one invoke binding named RUST.
async fn run(body: &str) -> (String, Vec<Call>) {
    let code = format!(
        "addEventListener('fetch', e => e.respondWith((async () => {{ {body} }})().then(text => new Response(String(text)))));"
    );

    run_in_local(|| async move {
        let host = Arc::new(Host::default());
        let ops: OperationsHandle = host.clone();
        let script = Script::with_bindings(code, None, vec![BindingInfo::invoke("RUST")]);

        let mut worker = Worker::new_with_ops(script, None, ops).await.unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        worker.exec(task).await.unwrap();

        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();
        let calls = host.calls.lock().unwrap().clone();

        (String::from_utf8_lossy(&body).into_owned(), calls)
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn invoke_returns_what_the_host_answers() {
    let (body, calls) =
        run("return JSON.stringify(await env.RUST.invoke('add', { a: 2, b: 3 }));").await;

    assert_eq!(body, r#"{"sum":5}"#);
    assert_eq!(
        calls,
        vec![(
            "RUST".to_string(),
            "add".to_string(),
            json!({ "a": 2, "b": 3 })
        )]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_error_rejects_with_its_message() {
    let (body, _) =
        run("try { await env.RUST.invoke('fail'); } catch (error) { return error.message; }").await;

    assert_eq!(body, "the disk is full");
}

#[tokio::test(flavor = "current_thread")]
async fn no_arguments_reach_the_host_as_null() {
    let (body, calls) = run("return String(await env.RUST.invoke('nothing'));").await;

    assert_eq!(body, "null");
    assert_eq!(calls[0].2, Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn a_command_that_is_not_a_string_is_a_type_error() {
    let (body, calls) = run(
        "try { await env.RUST.invoke(42); } catch (error) { return `${error.name}: ${error.message}`; }",
    )
    .await;

    assert_eq!(body, "TypeError: invoke: the command is not a string");
    assert!(calls.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_bigint_argument_is_a_type_error() {
    let (body, calls) = run(
        "try { await env.RUST.invoke('add', { a: 1n }); return 'sent'; } catch (error) { return error.name; }",
    )
    .await;

    assert_eq!(body, "TypeError");
    assert!(calls.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn an_argument_that_holds_itself_is_a_type_error() {
    let (body, calls) = run(
        "const a = {}; a.self = a;
         try { await env.RUST.invoke('add', a); return 'sent'; } catch (error) { return error.name; }",
    )
    .await;

    assert_eq!(body, "TypeError");
    assert!(calls.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn a_host_function_that_waits_settles_later() {
    let (body, _) = run("return await env.RUST.invoke('later');").await;

    assert_eq!(body, "late");
}

// A ready call settles before a timer of 0 ms set before it.
#[tokio::test(flavor = "current_thread")]
async fn a_ready_call_settles_before_an_earlier_timer() {
    let (body, _) = run("const order = [];
         setTimeout(() => order.push('timer'), 0);
         await env.RUST.invoke('nothing');
         order.push('invoke');
         await new Promise((resolve) => setTimeout(resolve, 5));
         return order.join(',');")
    .await;

    assert_eq!(body, "invoke,timer");
}

// A ready call is settled when invoke returns, as an async function of Node.js
// would be: its reaction runs before a microtask queued after the call.
#[tokio::test(flavor = "current_thread")]
async fn a_ready_call_settles_before_a_later_microtask() {
    let (body, _) = run("const order = [];
         const call = env.RUST.invoke('nothing').then(() => order.push('invoke'));
         Promise.resolve().then(() => order.push('microtask'));
         await call;
         await new Promise((resolve) => setTimeout(resolve, 5));
         return order.join(',');")
    .await;

    assert_eq!(body, "invoke,microtask");
}

/// Runs two requests on one worker: the first with `body`, the second calls
/// `add`. Returns the body of each, or the error of the first.
async fn run_twice(body: &str) -> (Result<String, String>, String) {
    let code = format!(
        "addEventListener('fetch', e => e.respondWith((async () => {{
            if (new URL(e.request.url).pathname === '/second') {{
                return new Response(JSON.stringify(await env.RUST.invoke('add', {{ a: 1, b: 1 }})));
            }}

            return new Response(String(await (async () => {{ {body} }})()));
        }})()));"
    );

    run_in_local(|| async move {
        let ops: OperationsHandle = Arc::new(Host::default());
        let script = Script::with_bindings(code, None, vec![BindingInfo::invoke("RUST")]);
        let mut worker = Worker::new_with_ops(script, None, ops).await.unwrap();

        let mut answers = Vec::new();

        for path in ["/first", "/second"] {
            let (task, rx) = Event::fetch(HttpRequest {
                method: HttpMethod::Get,
                url: format!("http://localhost{path}"),
                headers: HashMap::new(),
                body: RequestBody::None,
            });

            match worker.exec(task).await {
                Ok(()) => {
                    let response = rx.await.unwrap();
                    let body = response.body.collect().await.unwrap().unwrap();

                    answers.push(Ok(String::from_utf8_lossy(&body).into_owned()));
                }
                Err(reason) => answers.push(Err(format!("{reason:?}"))),
            }
        }

        let second = answers.pop().unwrap().unwrap();

        (answers.pop().unwrap(), second)
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_host_function_that_panics_rejects_and_the_next_request_works() {
    for name in ["panic now", "panic in poll"] {
        let (first, second) = run_twice(&format!(
            "try {{ await env.RUST.invoke('{name}'); return 'settled'; }} catch (error) {{ return error.message; }}"
        ))
        .await;

        assert_eq!(
            first.unwrap(),
            "invoke: the host function panicked",
            "{name}"
        );
        assert_eq!(second, r#"{"sum":2}"#, "{name}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_host_function_that_panics_after_it_waits_rejects() {
    let (first, second) = run_twice(
        "try { await env.RUST.invoke('panic later'); return 'settled'; } catch (error) { return error.message; }",
    )
    .await;

    assert_eq!(first.unwrap(), "invoke: the host function panicked");
    assert_eq!(second, r#"{"sum":2}"#);
}

// A call that waits is cancelled with its request, and its promise rejects
#[tokio::test(flavor = "current_thread")]
async fn a_call_that_waits_is_cancelled_with_its_request() {
    use openworkers_core::RuntimeLimits;
    use openworkers_runtime_v8::{ExecutionContext, LockerManagedIsolate};
    use tokio_util::sync::CancellationToken;

    let code = "addEventListener('fetch', e => e.respondWith((async () => {
        try { await env.RUST.invoke('forever'); return new Response('settled'); }
        catch (error) { return new Response(error.message); }
    })()));";

    let body = run_in_local(|| async move {
        let ops: OperationsHandle = Arc::new(Host::default());
        let script = Script::with_bindings(code, None, vec![BindingInfo::invoke("RUST")]);
        let limits = RuntimeLimits::default();
        let isolate = Arc::new(LockerManagedIsolate::new(limits.clone()));
        let mut context = {
            let (mut locker, _js_lock) = isolate.lock();

            ExecutionContext::new_with_pooled_isolate(
                &mut locker,
                Arc::clone(&isolate),
                isolate.use_snapshot,
                isolate.platform,
                limits,
                script,
                ops,
            )
            .unwrap()
        };
        let cancel = CancellationToken::new();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });

        context.begin_request(Some(cancel.clone()));

        let (_, response) = tokio::join!(context.exec(task), async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();

            rx.await.unwrap()
        });
        let body = response.body.collect().await.unwrap().unwrap();

        String::from_utf8_lossy(&body).into_owned()
    })
    .await;

    assert_eq!(body, "Operation cancelled");
}

// A loop of ready calls gives the thread back, so a timer set before it fires
#[tokio::test(flavor = "current_thread")]
async fn a_loop_of_ready_calls_lets_a_timer_fire() {
    let (body, _) = run("let fired = false;
         setTimeout(() => { fired = true; }, 0);
         const start = Date.now();
         while (!fired && Date.now() - start < 200) { await env.RUST.invoke('nothing'); }
         return fired;")
    .await;

    assert_eq!(body, "true");
}

// The same with a host function that panics: each call counts in the budget
#[tokio::test(flavor = "current_thread")]
async fn a_loop_of_panicking_calls_lets_a_timer_fire() {
    let (body, _) = run("let fired = false;
         setTimeout(() => { fired = true; }, 0);
         const start = Date.now();
         while (!fired && Date.now() - start < 200) {
             try { await env.RUST.invoke('panic now'); } catch {}
         }
         return fired;")
    .await;

    assert_eq!(body, "true");
}

// A loop of ready calls past the inline budget: its answers stay right
#[tokio::test(flavor = "current_thread")]
async fn a_long_loop_of_ready_calls_stays_correct() {
    let (body, calls) = run(
        "let sum = 0;
         for (let i = 0; i < 100; i++) { sum += (await env.RUST.invoke('add', { a: i, b: 0 })).sum; }
         return sum;",
    )
    .await;

    assert_eq!(body, "4950");
    assert_eq!(calls.len(), 100);
}

// The host function runs inside the turn of JavaScript, so its time counts in
// the CPU limit of the request
#[tokio::test(flavor = "current_thread")]
async fn a_host_function_past_the_cpu_limit_ends_the_request() {
    let (first, _) = run_twice("await env.RUST.invoke('spin'); return 'spun';").await;

    assert_eq!(first.unwrap_err(), "CpuTimeLimit");
}
