//! The binding objects a worker finds on `env`, each calling the host's
//! handler, and a binding that wins over a variable of the same name. The
//! JavaScript under test is src/js/env.js.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::run_in_local;
use openworkers_core::{
    BindingInfo, BindingType, Event, HttpMethod, HttpRequest, HttpResponse, KvOp, KvResult,
    OpFuture, OperationsHandle, OperationsHandler, RequestBody, ResponseBody, Script, StorageOp,
    StorageResult,
};
use openworkers_runtime_v8::Worker;
use serde_json::json;

/// Keeps KV values and storage objects in memory and records which binding
/// each call named.
#[derive(Default)]
struct Store {
    values: Mutex<HashMap<String, serde_json::Value>>,
    objects: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
}

impl OperationsHandler for Store {
    fn handle_binding_kv(&self, binding: &str, op: KvOp) -> OpFuture<'_, KvResult> {
        self.calls.lock().unwrap().push(format!("kv {binding}"));
        let mut values = self.values.lock().unwrap();

        let result = match op {
            KvOp::Get { key } => KvResult::Value(values.get(&key).cloned()),
            KvOp::Put { key, value, .. } => {
                values.insert(key, value);
                KvResult::Ok
            }
            KvOp::Delete { key } => {
                values.remove(&key);
                KvResult::Ok
            }
            KvOp::List { .. } => KvResult::Keys(values.keys().cloned().collect()),
        };

        Box::pin(async move { result })
    }

    fn handle_binding_storage(&self, binding: &str, op: StorageOp) -> OpFuture<'_, StorageResult> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("storage {binding}"));
        let mut objects = self.objects.lock().unwrap();

        let result = match op {
            StorageOp::Get { key } => StorageResult::Body(objects.get(&key).cloned()),
            StorageOp::Put { key, body } => {
                objects.insert(key, body);
                StorageResult::Body(None)
            }
            StorageOp::Head { key } => match objects.get(&key) {
                Some(body) => StorageResult::Head {
                    size: body.len() as u64,
                    etag: Some("etag-1".to_string()),
                },
                None => StorageResult::Error("not found".to_string()),
            },
            StorageOp::List { .. } => StorageResult::List {
                keys: objects.keys().cloned().collect(),
                truncated: false,
            },
            StorageOp::Delete { key } => {
                objects.remove(&key);
                StorageResult::Body(None)
            }
            // fetch names the object by the URL path, leading slash included
            StorageOp::Fetch { key } => StorageResult::Response(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Bytes(
                    objects
                        .get(key.trim_start_matches('/'))
                        .cloned()
                        .unwrap_or_default()
                        .into(),
                ),
            }),
        };

        Box::pin(async move { result })
    }

    fn handle_binding_fetch(
        &self,
        binding: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        self.calls.lock().unwrap().push(format!("assets {binding}"));
        let body = format!("asset {}", request.url);

        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Bytes(body.into()),
            })
        })
    }

    fn handle_binding_worker(
        &self,
        binding: &str,
        request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        self.calls.lock().unwrap().push(format!("worker {binding}"));
        let sent = match &request.body {
            RequestBody::Bytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => String::new(),
        };
        let body = format!("worker {} {} {sent}", request.method.as_str(), request.url);

        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: ResponseBody::Bytes(body.into()),
            })
        })
    }
}

/// Runs `body`, the inside of an async function that returns a string, in a
/// fetch handler with every binding type declared and two variables.
async fn run(body: &str) -> (String, Vec<String>) {
    let code = format!(
        "addEventListener('fetch', e => e.respondWith((async () => {{ {body} }})().then(text => new Response(String(text)))));"
    );

    run_in_local(|| async move {
        let store = Arc::new(Store::default());
        let ops: OperationsHandle = store.clone();
        let script = Script::with_bindings(
            code,
            Some(HashMap::from([
                ("GREETING".to_string(), "hello".to_string()),
                (
                    "KV".to_string(),
                    "a variable the binding replaces".to_string(),
                ),
            ])),
            vec![
                BindingInfo::new("KV", BindingType::Kv),
                BindingInfo::new("STORAGE", BindingType::Storage),
                BindingInfo::new("ASSETS", BindingType::Assets),
                BindingInfo::new("OTHER", BindingType::Worker),
                BindingInfo::new("IMAGES", BindingType::Images),
            ],
        );

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
        let calls = store.calls.lock().unwrap().clone();

        (String::from_utf8_lossy(&body).into_owned(), calls)
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_binding_wins_over_a_variable_of_the_same_name() {
    let (body, _) =
        run("return [env.GREETING, typeof env.KV.get, Object.isFrozen(env)].join(',');").await;

    assert_eq!(body, "hello,function,true");
}

#[tokio::test(flavor = "current_thread")]
async fn kv_keeps_json_values() {
    let (body, calls) = run(
        "await env.KV.put('session', { user: 7 }, { expiresIn: 60 });
         const session = await env.KV.get('session');
         const keys = await env.KV.list({ prefix: 's' });
         await env.KV.delete('session');
         return JSON.stringify([session, keys, await env.KV.get('session')]);",
    )
    .await;

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        json!([{ "user": 7 }, ["session"], null])
    );
    assert!(calls.iter().all(|call| call == "kv KV"), "{calls:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn storage_keeps_bytes_and_reads_them_as_text() {
    let (body, calls) = run("await env.STORAGE.put('a.txt', 'hello');
         const text = await env.STORAGE.get('a.txt');
         const head = await env.STORAGE.head('a.txt');
         const listing = await env.STORAGE.list({ limit: 10 });
         const served = await (await env.STORAGE.fetch('http://x/a.txt')).text();
         await env.STORAGE.delete('a.txt');
         return JSON.stringify([text, head, listing, served, await env.STORAGE.get('a.txt')]);")
    .await;

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        json!([
            "hello",
            { "size": 5, "etag": "etag-1" },
            { "keys": ["a.txt"], "truncated": false },
            "hello",
            null
        ])
    );
    assert!(
        calls.iter().all(|call| call == "storage STORAGE"),
        "{calls:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn assets_and_worker_bindings_fetch_through_the_host() {
    let (body, calls) = run(
        "const asset = await (await env.ASSETS.fetch('http://x/logo.svg')).text();
         const other = await (await env.OTHER.fetch('http://other/api', { method: 'POST', body: 'x' })).text();
         return asset + ' | ' + other;",
    )
    .await;

    assert_eq!(
        body,
        "asset http://x/logo.svg | worker POST http://other/api x"
    );
    assert_eq!(calls, vec!["assets ASSETS", "worker OTHER"]);
}

#[tokio::test(flavor = "current_thread")]
async fn the_images_binding_says_it_is_not_supported() {
    let (body, _) =
        run("try { env.IMAGES.input(); return 'no throw'; } catch (e) { return e.message; }").await;

    assert_eq!(body, "images binding is not supported by this runtime");
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_binding_response_has_a_status_text() {
    let (body, _) = run("const response = await env.OTHER.fetch('http://other/api');
         return response.status + ' ' + response.statusText;")
    .await;

    assert_eq!(body, "200 OK");
}
