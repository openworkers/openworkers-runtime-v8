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
