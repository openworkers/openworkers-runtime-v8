//! A Durable Object settles ready invoke calls inline in each of its turns,
//! as a worker does in each of its requests.

#![cfg(feature = "invoke")]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::run_in_local;
use openworkers_core::BindingInfo;
use openworkers_core::FetchInit;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::HttpResponse;
use openworkers_core::InvokeResult;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::ResponseBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_runtime_v8::{ObjectEvent, ObjectSpec};
use openworkers_transform::CodeLanguage;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// /inline answers whether a ready call settled before three microtasks,
/// that is inline, not through the scheduler; /loop makes 20 ready calls, more
/// than the inline budget of a turn (16).
const OBJECT: &str = r#"
    export class Counter {
        constructor(state, env) {
            this.env = env;
        }

        async fetch(request) {
            const url = new URL(request.url);

            if (url.pathname === '/loop') {
                for (let i = 0; i < 20; i++) {
                    await this.env.RUST.invoke('nothing');
                }

                return new Response('looped');
            }

            let settled = false;
            this.env.RUST.invoke('nothing').then(() => { settled = true; });

            await null;
            await null;
            await null;

            return new Response(String(settled));
        }
    }
"#;

/// Answers every call at once: a ready future.
struct Host;

impl OperationsHandler for Host {
    fn handle_binding_invoke(
        &self,
        _binding: &str,
        _name: &str,
        _args: Value,
    ) -> OpFuture<'_, InvokeResult> {
        Box::pin(async { InvokeResult::Value(Value::Null) })
    }
}

fn start() -> mpsc::UnboundedSender<ObjectEvent> {
    let code =
        openworkers_transform::parse_worker_code(OBJECT.as_bytes(), CodeLanguage::JavaScript)
            .unwrap();
    let spec = ObjectSpec {
        script: Script::with_bindings(code, None, vec![BindingInfo::invoke("RUST")]),
        class_name: "Counter".to_string(),
        id: "0123abcd".to_string(),
        limits: RuntimeLimits::default(),
        ops: Arc::new(Host),
        idle: Duration::from_secs(60),
        drain: Duration::from_millis(300),
    };
    let (tx, rx) = mpsc::unbounded_channel();

    tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

    tx
}

async fn get(object: &mpsc::UnboundedSender<ObjectEvent>, path: &str) -> String {
    let (res_tx, res_rx) = oneshot::channel::<HttpResponse>();
    let request = HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://object{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    object
        .send(ObjectEvent::Fetch(FetchInit::new(request, res_tx)))
        .unwrap();

    let response = timeout(Duration::from_secs(5), res_rx)
        .await
        .expect("the object answers in time")
        .expect("the object answers");

    match response.body {
        ResponseBody::None => String::new(),
        ResponseBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
        ResponseBody::Stream(mut rx) => {
            let mut text = Vec::new();

            while let Some(chunk) = rx.recv().await {
                text.extend_from_slice(&chunk.unwrap());
            }

            String::from_utf8(text).unwrap()
        }
    }
}

// A turn that used the whole inline budget leaves a whole one to the next
// turns: a ready call still settles inline after a loop of 20
#[tokio::test(flavor = "current_thread")]
async fn each_turn_gets_a_whole_inline_budget() {
    run_in_local(|| async {
        let object = start();

        assert_eq!(get(&object, "/inline").await, "true");
        assert_eq!(get(&object, "/loop").await, "looped");
        assert_eq!(get(&object, "/inline").await, "true");
    })
    .await;
}
