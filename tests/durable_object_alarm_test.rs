//! The alarm of a Durable Object: storage.getAlarm, setAlarm and deleteAlarm
//! reach the host, and an alarm event runs the alarm() handler behind the
//! input gate and answers after the writes it made (output gate).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::run_in_local;
use openworkers_core::{
    FetchInit, HttpMethod, HttpRequest, HttpResponse, ObjectStorageOp, ObjectStorageResult,
    OpFuture, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
};
use openworkers_runtime_v8::{AlarmInit, ObjectEvent, ObjectSpec};
use openworkers_transform::CodeLanguage;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

/// The alarm of one object, in memory. A write takes WRITE_DELAY.
#[derive(Default)]
struct Memory {
    alarm: Mutex<Option<i64>>,
}

const WRITE_DELAY: Duration = Duration::from_millis(200);

impl OperationsHandler for Memory {
    fn handle_durable_object_storage(
        &self,
        op: ObjectStorageOp,
    ) -> OpFuture<'_, ObjectStorageResult> {
        Box::pin(async move {
            match op {
                ObjectStorageOp::GetAlarm => {
                    ObjectStorageResult::Alarm(*self.alarm.lock().unwrap())
                }
                ObjectStorageOp::SetAlarm(at) => {
                    tokio::time::sleep(WRITE_DELAY).await;
                    *self.alarm.lock().unwrap() = at;

                    ObjectStorageResult::Alarm(at)
                }
                ObjectStorageOp::Write { .. } => {
                    tokio::time::sleep(WRITE_DELAY).await;

                    ObjectStorageResult::Written { deleted: 0 }
                }
                _ => ObjectStorageResult::Error("not in these tests".to_string()),
            }
        })
    }
}

/// `fetch` runs the body of its path in `routes`; `alarm` keeps what it got
/// in `this.alarms`, and throws when `this.fail` is set.
fn object(memory: Arc<Memory>, class: &str) -> mpsc::UnboundedSender<ObjectEvent> {
    let code = openworkers_transform::parse_worker_code(class.as_bytes(), CodeLanguage::JavaScript)
        .unwrap();
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

    tx
}

const ALARMED: &str = r#"
    export class Counter {
        constructor(state) {
            this.state = state;
            this.alarms = [];
            this.fail = false;
        }

        async fetch(request) {
            const storage = this.state.storage;

            switch (new URL(request.url).pathname) {
                case '/api': {
                    const answers = [await storage.getAlarm()];
                    await storage.setAlarm(new Date(5000));
                    answers.push(await storage.getAlarm());
                    await storage.setAlarm(7000.9);
                    answers.push(await storage.getAlarm());
                    await storage.deleteAlarm();
                    answers.push(await storage.getAlarm());
                    answers.push(await storage.setAlarm('soon').catch((e) => e.name));
                    answers.push(await storage.transaction((txn) => txn.setAlarm(1).catch((e) => e.message)));
                    return new Response(JSON.stringify(answers));
                }
                case '/fail':
                    this.fail = true;
                    return new Response('failing');
                case '/alarms':
                    return new Response(JSON.stringify(this.alarms));
            }
        }

        async alarm(info) {
            if (this.fail) {
                throw new Error('the alarm failed');
            }

            this.alarms.push(info);
            this.state.storage.put('seen', this.alarms.length);
        }
    }
"#;

async fn fetch(object: &mpsc::UnboundedSender<ObjectEvent>, path: &str) -> String {
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
        ResponseBody::Bytes(bytes) => String::from_utf8(bytes.to_vec()).unwrap(),
        _ => panic!("a plain body"),
    }
}

async fn alarm(
    object: &mpsc::UnboundedSender<ObjectEvent>,
    retry_count: u32,
) -> Result<(), String> {
    let (done, outcome) = oneshot::channel();

    object
        .send(ObjectEvent::Alarm(AlarmInit { retry_count, done }))
        .unwrap();

    timeout(Duration::from_secs(5), outcome)
        .await
        .expect("the alarm ends in time")
        .expect("the object answers the alarm")
}

#[tokio::test(flavor = "current_thread")]
async fn the_storage_reads_sets_and_deletes_the_alarm() {
    run_in_local(|| async {
        let memory = Arc::new(Memory::default());
        let object = object(memory.clone(), ALARMED);

        assert_eq!(
            fetch(&object, "/api").await,
            r#"[null,5000,7000,null,"TypeError","Durable Object storage: setAlarm() is not supported in a transaction"]"#
        );
        assert_eq!(*memory.alarm.lock().unwrap(), None);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_class_without_an_alarm_handler_cannot_set_an_alarm() {
    run_in_local(|| async {
        let class = r#"
            export class Counter {
                constructor(state) {
                    this.state = state;
                }

                async fetch() {
                    const error = await this.state.storage.setAlarm(Date.now()).catch((e) => e.message);
                    return new Response(error);
                }
            }
        "#;
        let object = object(Arc::new(Memory::default()), class);

        assert_eq!(
            fetch(&object, "/").await,
            "the Durable Object class has no alarm() handler"
        );
        assert_eq!(
            alarm(&object, 0).await,
            Err("the Durable Object class has no alarm() handler".to_string())
        );
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alarm_runs_the_handler_with_its_retry_count() {
    run_in_local(|| async {
        let object = object(Arc::new(Memory::default()), ALARMED);

        assert_eq!(alarm(&object, 0).await, Ok(()));
        assert_eq!(alarm(&object, 2).await, Ok(()));
        assert_eq!(
            fetch(&object, "/alarms").await,
            r#"[{"retryCount":0,"isRetry":false},{"retryCount":2,"isRetry":true}]"#
        );

        fetch(&object, "/fail").await;
        assert_eq!(alarm(&object, 0).await, Err("the alarm failed".to_string()));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alarm_ends_after_the_writes_of_its_handler() {
    run_in_local(|| async {
        let object = object(Arc::new(Memory::default()), ALARMED);
        let started = Instant::now();

        assert_eq!(alarm(&object, 0).await, Ok(()));
        assert!(started.elapsed() >= WRITE_DELAY);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alarm_waits_for_block_concurrency_while() {
    run_in_local(|| async {
        let class = r#"
            export class Counter {
                constructor(state) {
                    state.blockConcurrencyWhile(() => new Promise((resolve) => setTimeout(resolve, 300)));
                }

                async alarm() {}
            }
        "#;
        let object = object(Arc::new(Memory::default()), class);
        let started = Instant::now();

        assert_eq!(alarm(&object, 0).await, Ok(()));
        assert!(started.elapsed() >= Duration::from_millis(300));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn an_alarm_the_host_stopped_waiting_for_does_not_keep_the_object() {
    run_in_local(|| async {
        let class = r#"
            export class Counter {
                async alarm() {
                    await new Promise(() => {});
                }
            }
        "#;
        let code =
            openworkers_transform::parse_worker_code(class.as_bytes(), CodeLanguage::JavaScript)
                .unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        let spec = ObjectSpec {
            script: Script::new(code),
            class_name: "Counter".to_string(),
            id: "object".to_string(),
            limits: RuntimeLimits::default(),
            ops: Arc::new(Memory::default()),
            idle: Duration::from_millis(100),
            drain: Duration::from_secs(30),
        };
        let run = tokio::task::spawn_local(openworkers_runtime_v8::durable_object::run(spec, rx));

        let (done, outcome) = oneshot::channel();
        tx.send(ObjectEvent::Alarm(AlarmInit {
            retry_count: 0,
            done,
        }))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(outcome);
        drop(tx);

        timeout(Duration::from_secs(5), run)
            .await
            .expect("the object ends")
            .unwrap();
    })
    .await;
}
