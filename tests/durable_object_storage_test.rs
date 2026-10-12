//! The storage of a Durable Object: values keep their type, the ops of an
//! object run in the order it made them, and a response waits for the writes
//! before it (output gate).

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectStorageOp, ObjectStorageResult,
    OpFuture, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{ObjectEvent, ObjectSpec};
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// The storage of one object, in memory. Writes take `write_delay`, less
/// for each later write, so a host that ran them at once would apply them out
/// of order. A write that puts the key `fail` fails.
#[derive(Default)]
struct Memory {
    entries: Mutex<BTreeMap<String, Vec<u8>>>,
    ops: Mutex<Vec<ObjectStorageOp>>,
    writes: AtomicU64,
    write_delay: Duration,
}

impl OperationsHandler for Memory {
    fn handle_durable_object_storage(
        &self,
        op: ObjectStorageOp,
    ) -> OpFuture<'_, ObjectStorageResult> {
        self.ops.lock().unwrap().push(op.clone());

        Box::pin(async move {
            match op {
                ObjectStorageOp::Get { keys } => {
                    let entries = self.entries.lock().unwrap();

                    ObjectStorageResult::Values(
                        keys.iter().map(|key| entries.get(key).cloned()).collect(),
                    )
                }
                ObjectStorageOp::List {
                    start,
                    start_after,
                    end,
                    prefix,
                    reverse,
                    limit,
                } => {
                    let entries = self.entries.lock().unwrap();
                    let mut list: Vec<(String, Vec<u8>)> = entries
                        .iter()
                        .filter(|(key, _)| start.as_ref().is_none_or(|start| *key >= start))
                        .filter(|(key, _)| start_after.as_ref().is_none_or(|after| *key > after))
                        .filter(|(key, _)| end.as_ref().is_none_or(|end| *key < end))
                        .filter(|(key, _)| prefix.as_ref().is_none_or(|p| key.starts_with(p)))
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();

                    if reverse {
                        list.reverse();
                    }

                    list.truncate(limit.unwrap_or(u64::MAX) as usize);

                    ObjectStorageResult::Entries(list)
                }
                ObjectStorageOp::Write {
                    puts,
                    deletes,
                    delete_all,
                } => {
                    let earlier = self.writes.fetch_add(1, Ordering::SeqCst) as u32;
                    let delay = self
                        .write_delay
                        .saturating_sub(Duration::from_millis(20) * earlier);
                    tokio::time::sleep(delay).await;

                    if puts.iter().any(|(key, _)| key == "fail") {
                        return ObjectStorageResult::Error("the disk is full".to_string());
                    }

                    let mut entries = self.entries.lock().unwrap();

                    if delete_all {
                        entries.clear();
                    }

                    let deleted = deletes
                        .iter()
                        .filter(|key| entries.remove(*key).is_some())
                        .count() as u64;

                    entries.extend(puts);

                    ObjectStorageResult::Written { deleted }
                }
                ObjectStorageOp::GetAlarm | ObjectStorageOp::SetAlarm(_) => {
                    ObjectStorageResult::Error("no alarms in these tests".to_string())
                }
            }
        })
    }
}

/// Runs `body`, the inside of an async fetch method of the object class,
/// once per path in `paths`, and answers the response texts.
async fn run(memory: Arc<Memory>, body: &str, paths: &[&str]) -> Vec<Result<String, String>> {
    let code = format!(
        r#"
        export class Counter {{
            constructor(state) {{
                this.state = state;
                this.storage = state.storage;
            }}

            async fetch(request) {{
                const path = new URL(request.url).pathname;
                const storage = this.storage;
                const answer = await (async () => {{ {body} }})();
                return new Response(String(answer));
            }}
        }}
        "#
    );
    let code = openworkers_transform::parse_worker_code(code.as_bytes(), CodeLanguage::JavaScript)
        .unwrap();

    let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();

    run_in_local(|| async move {
        let (tx, rx) = mpsc::unbounded_channel();
        let spec = ObjectSpec {
            script: Script::new(code),
            class_name: "Counter".to_string(),
            id: "object".to_string(),
            limits: RuntimeLimits::default(),
            ops: memory,
            idle: Duration::from_secs(60),
            drain: Duration::from_secs(30),
        };

        tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

        let mut answers = Vec::new();

        for path in paths {
            answers.push(send(&tx, &path).await);
        }

        answers
    })
    .await
}

async fn send(object: &mpsc::UnboundedSender<ObjectEvent>, path: &str) -> Result<String, String> {
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
        .map_err(|_| "the object failed".to_string())?;

    match response.body {
        ResponseBody::Bytes(bytes) => Ok(String::from_utf8(bytes.to_vec()).unwrap()),
        ResponseBody::None => Ok(String::new()),
        ResponseBody::Stream(mut rx) => {
            let mut text = Vec::new();

            while let Some(chunk) = rx.recv().await {
                text.extend_from_slice(&chunk.unwrap());
            }

            Ok(String::from_utf8(text).unwrap())
        }
    }
}

fn ok(answers: Vec<Result<String, String>>) -> Vec<String> {
    answers.into_iter().map(Result::unwrap).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn values_keep_their_type() {
    let body = r#"
        const date = new Date(86400000);
        await storage.put({
            map: new Map([['a', 1]]),
            date,
            bytes: new Uint8Array([1, 2, 3]),
            nested: { list: [1, 'two', null], big: 10n },
        });

        const read = await storage.get(['map', 'date', 'bytes', 'nested', 'missing']);

        return JSON.stringify({
            keys: [...read.keys()],
            map: read.get('map') instanceof Map && read.get('map').get('a'),
            date: read.get('date') instanceof Date && read.get('date').getTime(),
            bytes: read.get('bytes') instanceof Uint8Array && [...read.get('bytes')],
            nested: read.get('nested').list,
            big: typeof read.get('nested').big,
            one: await storage.get('missing'),
        });
    "#;

    let answers = ok(run(Arc::new(Memory::default()), body, &["/"]).await);

    assert_eq!(
        answers[0],
        r#"{"keys":["map","date","bytes","nested"],"map":1,"date":86400000,"bytes":[1,2,3],"nested":[1,"two",null],"big":"bigint"}"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn ops_run_in_the_order_the_object_made_them() {
    let memory = Arc::new(Memory {
        write_delay: Duration::from_millis(100),
        ..Default::default()
    });

    // The first write takes the longest, and none is awaited
    let body = r#"
        storage.put('k', 1);
        storage.put('k', 2);
        storage.delete('gone');
        storage.put('k', 3);
        return storage.get('k');
    "#;

    assert_eq!(ok(run(memory, body, &["/"]).await), ["3"]);
}

#[tokio::test(flavor = "current_thread")]
async fn list_delete_and_delete_all() {
    let body = r#"
        await storage.put({ 'a:1': 1, 'a:2': 2, 'a:3': 3, b: 4 });

        const keys = async (options) => [...(await storage.list(options)).keys()].join(',');
        const lists = [
            await keys(),
            await keys({ prefix: 'a:' }),
            await keys({ start: 'a:2', end: 'b' }),
            await keys({ startAfter: 'a:1', limit: 2 }),
            await keys({ reverse: true, limit: 2 }),
        ];

        const one = await storage.delete('a:1');
        const none = await storage.delete('a:1');
        const count = await storage.delete(['a:2', 'a:3', 'zz']);
        await storage.deleteAll();

        return [...lists, one, none, count, (await storage.list()).size].join('|');
    "#;

    assert_eq!(
        ok(run(Arc::new(Memory::default()), body, &["/"]).await),
        ["a:1,a:2,a:3,b|a:1,a:2,a:3|a:2,a:3|a:2,a:3|b,a:3|true|false|2|0"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_transaction_writes_once_or_not_at_all() {
    let memory = Arc::new(Memory::default());
    let body = r#"
        await storage.put('kept', 1);

        const seen = await storage.transaction(async (txn) => {
            await txn.put('a', 1);
            await txn.put('b', { two: 2 });
            await txn.delete('kept');
            return [await txn.get('a'), await txn.get('kept')].join(',');
        });

        await storage.transaction(async (txn) => {
            await txn.put('rolled', 1);
            txn.rollback();
        });

        const thrown = await storage.transaction(async (txn) => {
            await txn.put('thrown', 1);
            throw new Error('no');
        }).catch((error) => error.message);

        const all = [...(await storage.list()).keys()].join(',');

        return [seen, thrown, all].join('|');
    "#;

    assert_eq!(
        ok(run(Arc::clone(&memory), body, &["/"]).await),
        ["1,|no|a,b"]
    );

    // The put before, then one write for the transaction
    let writes: Vec<ObjectStorageOp> = memory
        .ops
        .lock()
        .unwrap()
        .iter()
        .filter(|op| matches!(op, ObjectStorageOp::Write { .. }))
        .cloned()
        .collect();

    assert_eq!(writes.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn a_response_waits_for_the_writes_before_it() {
    let memory = Arc::new(Memory {
        write_delay: Duration::from_millis(300),
        ..Default::default()
    });
    let body = r#"
        if (path === '/write') {
            storage.put('k', 1);
        }

        return 'ok';
    "#;

    let started = Instant::now();
    let answers = ok(run(memory, body, &["/read", "/write"]).await);

    assert_eq!(answers, ["ok", "ok"]);
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test(flavor = "current_thread")]
async fn a_response_without_writes_does_not_wait() {
    let memory = Arc::new(Memory {
        write_delay: Duration::from_millis(1000),
        ..Default::default()
    });

    let started = Instant::now();
    let answers = ok(run(memory, "return 'ok';", &["/"]).await);

    assert_eq!(answers, ["ok"]);
    assert!(started.elapsed() < Duration::from_millis(1000));
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_write_drops_the_object_and_its_response() {
    let body = r#"
        this.served = (this.served ?? 0) + 1;

        if (path === '/fail') {
            storage.put('fail', 1);
            return 'sent';
        }

        await storage.put('k', 1);
        return `served ${this.served}`;
    "#;

    let answers = run(Arc::new(Memory::default()), body, &["/", "/fail", "/"]).await;

    // The third request finds a new instance
    assert_eq!(answers[0], Ok("served 1".to_string()));
    assert_eq!(answers[1], Err("the object failed".to_string()));
    assert_eq!(answers[2], Ok("served 1".to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn a_value_that_cannot_be_cloned_rejects_the_put_only() {
    let body = r#"
        const error = await storage.put('f', () => 1).catch((error) => error.message);
        await storage.put('k', 1);

        return `${error.includes('could not be cloned')} ${await storage.get('k')}`;
    "#;

    assert_eq!(
        ok(run(Arc::new(Memory::default()), body, &["/"]).await),
        ["true 1"]
    );
}
