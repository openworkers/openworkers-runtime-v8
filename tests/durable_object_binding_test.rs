//! A worker reaches its Durable Objects through a namespace binding: the
//! host makes the ids and routes each request to the object of its id.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::run_in_local;
use openworkers_core::{
    BindingInfo, Event, FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectIdSource, OpFuture,
    OperationsHandle, OperationsHandler, RequestBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{ObjectSpec, Worker};
use tokio::sync::{mpsc, oneshot};

/// The host of one namespace, COUNTER: ids are `name-<name>` and
/// `unique-<n>`, and each request goes to the object of its id.
struct Host {
    router: mpsc::UnboundedSender<(String, FetchInit)>,
    unique: AtomicU64,
}

impl OperationsHandler for Host {
    fn durable_object_id(&self, binding: &str, source: ObjectIdSource) -> Result<String, String> {
        if binding != "COUNTER" {
            return Err(format!("no namespace for '{binding}'"));
        }

        let id = match source {
            ObjectIdSource::Name(name) => format!("name-{name}"),
            ObjectIdSource::Text(text) if text.starts_with("name-") => text,
            ObjectIdSource::Text(_) => return Err("Invalid Durable Object ID".to_string()),
            ObjectIdSource::Unique => {
                format!("unique-{}", self.unique.fetch_add(1, Ordering::SeqCst))
            }
        };

        Ok(id)
    }

    fn handle_binding_durable_object(
        &self,
        _binding: &str,
        id: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        let (res_tx, res_rx) = oneshot::channel();
        let _ = self
            .router
            .send((id.to_string(), FetchInit::new(request, res_tx)));

        Box::pin(async move { res_rx.await.map_err(|_| "the object failed".to_string()) })
    }
}

const WORKER: &str = r#"
    export class Counter {
        constructor(state, env) {
            this.state = state;
            this.env = env;
            this.count = 0;
        }

        async fetch(request) {
            this.count += 1;
            const body = await request.text();

            if (body === 'spin') {
                while (true) {}
            }

            if (body.startsWith('relay:')) {
                const other = this.env.COUNTER.getByName(body.slice('relay:'.length));
                const text = await (await other.fetch('http://do/')).text();

                return new Response(`${this.state.id} relays ${text}`);
            }

            return new Response(`${this.state.id} ${this.count} ${request.method} ${body}`);
        }
    }

    export default {
        async fetch(request, env) {
            const url = new URL(request.url);
            const body = await (async () => { BODY })();
            return new Response(String(body));
        },
    };
"#;

/// Runs `body`, the inside of an async function, in the fetch handler of a
/// worker with a COUNTER binding, and answers what it returns.
async fn run(body: &str) -> String {
    let code = WORKER.replace("BODY", body);
    let code = openworkers_transform::parse_worker_code(
        code.as_bytes(),
        openworkers_transform::CodeLanguage::JavaScript,
    )
    .unwrap();

    run_in_local(|| async move {
        let script =
            Script::with_bindings(code, None, vec![BindingInfo::durable_object("COUNTER")]);

        let (router, mut routes) = mpsc::unbounded_channel::<(String, FetchInit)>();
        let host = Arc::new(Host {
            router,
            unique: AtomicU64::new(0),
        });
        let ops: OperationsHandle = host.clone();

        // The objects run on this thread, each with its own mailbox
        let objects_script = script.clone();
        let objects_ops = ops.clone();
        tokio::task::spawn_local(async move {
            let mut mailboxes: HashMap<String, mpsc::UnboundedSender<FetchInit>> = HashMap::new();

            while let Some((id, init)) = routes.recv().await {
                let mailbox = mailboxes.entry(id.clone()).or_insert_with(|| {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let spec = ObjectSpec {
                        script: objects_script.clone(),
                        class_name: "Counter".to_string(),
                        id,
                        limits: RuntimeLimits {
                            max_wall_clock_time_ms: 300,
                            ..Default::default()
                        },
                        ops: objects_ops.clone(),
                        idle: Duration::from_secs(60),
                        drain: Duration::from_secs(30),
                    };

                    tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));
                    tx
                });

                let _ = mailbox.send(init);
            }
        });

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

        String::from_utf8_lossy(&body).into_owned()
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_stub_reaches_the_object_of_its_id() {
    let body = r#"
        const a = env.COUNTER.getByName('a');
        const b = env.COUNTER.get(env.COUNTER.idFromName('b'));
        const texts = [];

        texts.push(await (await a.fetch('http://do/')).text());
        texts.push(await (await a.fetch('http://do/', { method: 'POST', body: 'hi' })).text());
        texts.push(await (await b.fetch(new Request('http://do/'))).text());

        return texts.join('|');
    "#;

    assert_eq!(
        run(body).await,
        "name-a 1 GET |name-a 2 POST hi|name-b 1 GET "
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ids_come_from_the_host() {
    let body = r#"
        const ns = env.COUNTER;
        const a = ns.idFromName('a');
        const again = ns.idFromString(String(a));
        const unique = [ns.newUniqueId(), ns.newUniqueId()].map(String);

        return JSON.stringify({
            text: String(a),
            name: a.name,
            same: a.equals(ns.idFromName('a')),
            other: a.equals(ns.idFromName('b')),
            again: again.equals(a),
            againName: again.name ?? null,
            unique,
            stub: ns.get(a).name,
            frozen: Object.isFrozen(a),
        });
    "#;

    let result: serde_json::Value = serde_json::from_str(&run(body).await).unwrap();

    assert_eq!(
        result,
        serde_json::json!({
            "text": "name-a",
            "name": "a",
            "same": true,
            "other": false,
            "again": true,
            "againName": null,
            "unique": ["unique-0", "unique-1"],
            "stub": "a",
            "frozen": true,
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn the_namespace_refuses_what_it_does_not_serve() {
    let body = r#"
        const ns = env.COUNTER;
        const errors = [];
        const attempt = (f) => {
            try {
                f();
                errors.push('none');
            } catch (e) {
                errors.push(`${e.name}: ${e.message}`);
            }
        };

        attempt(() => ns.idFromString('elsewhere'));
        attempt(() => ns.idFromName(42));
        attempt(() => ns.get({ toString: () => 'name-a' }));
        attempt(() => ns.newUniqueId({ jurisdiction: 'eu' }));
        attempt(() => ns.jurisdiction('eu'));

        return errors.join('\n');
    "#;

    assert_eq!(
        run(body).await,
        [
            "TypeError: Invalid Durable Object ID",
            "TypeError: Durable Objects: idFromName() needs a string",
            "TypeError: Durable Objects: get() takes an id of this namespace",
            "TypeError: Durable Objects: jurisdictions are not supported",
            "TypeError: Durable Objects: jurisdictions are not supported",
        ]
        .join("\n")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_object_reaches_another_object() {
    let body = r#"
        const relay = env.COUNTER.getByName('relay');
        const init = { method: 'POST', body: 'relay:target' };

        return (await relay.fetch('http://do/', init)).text();
    "#;

    assert_eq!(run(body).await, "name-relay relays name-target 1 GET ");
}

#[tokio::test(flavor = "current_thread")]
async fn a_stopped_object_rejects_the_fetch_of_the_stub() {
    let body = r#"
        const stub = env.COUNTER.getByName('a');

        try {
            await stub.fetch('http://do/', { method: 'PUT', body: 'spin' });
            return 'answered';
        } catch (e) {
            // The next request builds the object again
            return `${e.message}|${await (await stub.fetch('http://do/')).text()}`;
        }
    "#;

    assert_eq!(run(body).await, "the object failed|name-a 1 GET ");
}
