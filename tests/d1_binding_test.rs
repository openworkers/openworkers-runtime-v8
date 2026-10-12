//! The D1 facade of `js/env.js`: the ops it sends and what the guest gets back.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use common::run_in_local;
use openworkers_core::BindingInfo;
use openworkers_core::D1Error;
use openworkers_core::D1ErrorKind;
use openworkers_core::D1Meta;
use openworkers_core::D1Op;
use openworkers_core::D1Result;
use openworkers_core::D1Statement;
use openworkers_core::D1StatementResult;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandle;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_core::SqlPrimitive;

/// Records every op. A query or batch statement answers one row holding its
/// parameters, except for the SQL texts matched in `answer`.
#[derive(Default)]
struct Recorder {
    ops: Mutex<Vec<D1Op>>,
}

fn meta() -> D1Meta {
    D1Meta {
        duration: 0.5,
        changes: 1,
        last_row_id: 7,
        changed_db: true,
        size_after: 8192,
    }
}

fn answer(statement: &D1Statement) -> Result<D1StatementResult, D1Error> {
    let error = |kind, message: &str| D1Error {
        kind,
        message: message.to_string(),
        statement: None,
    };

    let (columns, rows) = match statement.sql.as_str() {
        "SELECT dup" => (
            vec!["id".to_string(), "id".to_string()],
            vec![vec![SqlPrimitive::Int(1), SqlPrimitive::Int(2)]],
        ),
        "SELECT empty" => (vec!["id".to_string()], vec![]),
        "FAIL sql" => {
            return Err(error(D1ErrorKind::Sql, "no such table: t: SQLITE_ERROR"));
        }
        "FAIL range" => return Err(error(D1ErrorKind::Range, "Wrong number of parameters")),
        _ => (
            (0..statement.params.len())
                .map(|i| format!("p{i}"))
                .collect(),
            vec![statement.params.clone()],
        ),
    };

    Ok(D1StatementResult {
        columns,
        rows,
        meta: meta(),
    })
}

impl OperationsHandler for Recorder {
    fn handle_binding_d1(&self, _binding: &str, op: D1Op) -> OpFuture<'_, D1Result> {
        self.ops.lock().unwrap().push(op.clone());

        let result = match op {
            D1Op::Query { statement, .. } => match answer(&statement) {
                Ok(result) => D1Result::Query(result),
                Err(err) => D1Result::Error(err),
            },
            D1Op::Batch { statements } => {
                match statements.iter().map(answer).collect::<Result<Vec<_>, _>>() {
                    Ok(results) => D1Result::Batch { results },
                    Err(err) => D1Result::Error(err),
                }
            }
            D1Op::Exec { .. } => D1Result::Exec {
                count: 2,
                duration: 1.5,
            },
        };

        Box::pin(async move { result })
    }
}

/// Runs `handler` as the body of an async fetch handler with `env.DB` and
/// `env.OTHER` bound to D1, and returns the response text.
async fn run(handler: &str, recorder: Arc<Recorder>) -> String {
    let code = format!(
        r#"
        addEventListener('fetch', (event) => {{
            event.respondWith((async () => {{
                try {{
                    const out = await (async () => {{ {handler} }})();
                    return new Response(JSON.stringify(out));
                }} catch (err) {{
                    return new Response('error: ' + err.constructor.name + ': ' + err.message);
                }}
            }})());
        }});
        "#
    );

    run_in_local(|| async move {
        let script = Script::with_bindings(
            code,
            None,
            vec![BindingInfo::d1("DB"), BindingInfo::d1("OTHER")],
        );

        let ops: OperationsHandle = recorder;
        let mut worker = openworkers_runtime_v8::Worker::new_with_ops(script, None, ops)
            .await
            .unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();

        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();

        String::from_utf8_lossy(&body).into_owned()
    })
    .await
}

fn ops(recorder: &Recorder) -> String {
    serde_json::to_string(&*recorder.ops.lock().unwrap()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn bind_sends_typed_values() {
    let recorder = Arc::new(Recorder::default());

    let out = run(
        r#"
        const stmt = env.DB.prepare('SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8');
        const bytes = new Uint8Array([0, 255]);
        const { results } = await stmt
            .bind(1, -2, 1.5, 'text', true, null, bytes, bytes.buffer)
            .all();
        return results;
        "#,
        recorder.clone(),
    )
    .await;

    assert_eq!(
        out,
        r#"[{"p0":1,"p1":-2,"p2":1.5,"p3":"text","p4":true,"p5":null,"p6":[0,255],"p7":[0,255]}]"#
    );
    assert_eq!(
        ops(&recorder),
        r#"[{"kind":"query","statement":{"sql":"SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8","params":[1,-2,1.5,"text",true,null,{"$bytes":"AP8="},{"$bytes":"AP8="}]},"rows":"all"}]"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bind_rejects_unsupported_values() {
    for value in ["undefined", "{}", "10n", "() => 1"] {
        let recorder = Arc::new(Recorder::default());

        let out = run(
            &format!("return env.DB.prepare('SELECT ?').bind({value});"),
            recorder.clone(),
        )
        .await;

        assert!(
            out.starts_with("error: Error: D1_TYPE_ERROR: Type '"),
            "{value}: {out}"
        );
        assert_eq!(ops(&recorder), "[]");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn result_has_d1_shape() {
    let out = run(
        "return await env.DB.prepare('SELECT ?').bind(1).run();",
        Arc::default(),
    )
    .await;

    assert_eq!(
        out,
        r#"{"success":true,"meta":{"duration":0.5,"changes":1,"last_row_id":7,"changed_db":true,"size_after":8192,"timings":{"sql_duration_ms":0.5},"served_by_primary":true,"total_attempts":1},"results":[{"p0":1}]}"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_keeps_duplicate_columns() {
    let out = run(
        r#"
        const stmt = env.DB.prepare('SELECT dup');
        return [await stmt.raw(), await stmt.raw({ columnNames: true }), (await stmt.all()).results];
        "#,
        Arc::default(),
    )
    .await;

    assert_eq!(out, r#"[[[1,2]],[["id","id"],[1,2]],[{"id":2}]]"#);
}

#[tokio::test(flavor = "current_thread")]
async fn first_returns_a_row_a_value_or_null() {
    let recorder = Arc::new(Recorder::default());

    let out = run(
        r#"
        return [
            await env.DB.prepare('SELECT ?').bind(5).first(),
            await env.DB.prepare('SELECT ?').bind(5).first('p0'),
            await env.DB.prepare('SELECT empty').first(),
            await env.DB.prepare('SELECT empty').first('id'),
            await env.DB.prepare('SELECT empty').raw({ columnNames: true }),
        ];
        "#,
        recorder.clone(),
    )
    .await;

    assert_eq!(out, r#"[{"p0":5},5,null,null,[["id"]]]"#);
    assert!(ops(&recorder).contains(r#""rows":"first""#));
}

#[tokio::test(flavor = "current_thread")]
async fn first_with_a_missing_column_throws() {
    let out = run(
        "return await env.DB.prepare('SELECT ?').bind(5).first('nope');",
        Arc::default(),
    )
    .await;

    assert_eq!(out, "error: Error: D1_ERROR: Column not found: nope");
}

#[tokio::test(flavor = "current_thread")]
async fn bind_returns_a_new_statement() {
    let recorder = Arc::new(Recorder::default());

    let out = run(
        r#"
        const insert = env.DB.prepare('INSERT INTO t VALUES (?1)');
        const results = await env.DB.batch([insert.bind(1), insert.bind(2), insert]);
        return results.map((r) => r.results);
        "#,
        recorder.clone(),
    )
    .await;

    assert_eq!(out, r#"[[{"p0":1}],[{"p0":2}],[{}]]"#);
    assert_eq!(
        ops(&recorder),
        r#"[{"kind":"batch","statements":[{"sql":"INSERT INTO t VALUES (?1)","params":[1]},{"sql":"INSERT INTO t VALUES (?1)","params":[2]},{"sql":"INSERT INTO t VALUES (?1)","params":[]}]}]"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn batch_checks_its_statements() {
    let recorder = Arc::new(Recorder::default());

    let out = run(
        r#"
        const errors = [];
        for (const list of [[env.OTHER.prepare('SELECT 1')], [{ sql: 'SELECT 1' }], 'SELECT 1']) {
            try {
                await env.DB.batch(list);
            } catch (err) {
                errors.push(err.constructor.name);
            }
        }
        return [errors, await env.DB.batch([])];
        "#,
        recorder.clone(),
    )
    .await;

    assert_eq!(out, r#"[["TypeError","TypeError","TypeError"],[]]"#);
    assert_eq!(ops(&recorder), "[]");
}

#[tokio::test(flavor = "current_thread")]
async fn errors_carry_the_d1_prefix() {
    let out = run(
        r#"
        const errors = [];
        for (const sql of ['FAIL sql', 'FAIL range']) {
            try {
                await env.DB.prepare(sql).all();
            } catch (err) {
                errors.push(err.message);
            }
        }
        try {
            await env.DB.batch([env.DB.prepare('SELECT 1'), env.DB.prepare('FAIL sql')]);
        } catch (err) {
            errors.push(err.message);
        }
        return errors;
        "#,
        Arc::default(),
    )
    .await;

    assert_eq!(
        out,
        r#"["D1_ERROR: no such table: t: SQLITE_ERROR","D1_RANGE_ERROR: Wrong number of parameters","D1_ERROR: no such table: t: SQLITE_ERROR"]"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exec_returns_count_and_duration() {
    let recorder = Arc::new(Recorder::default());

    let out = run(
        "return await env.DB.exec('CREATE TABLE t (a);\\nINSERT INTO t VALUES (1);');",
        recorder.clone(),
    )
    .await;

    assert_eq!(out, r#"{"count":2,"duration":1.5}"#);
    assert_eq!(
        ops(&recorder),
        r#"[{"kind":"exec","sql":"CREATE TABLE t (a);\nINSERT INTO t VALUES (1);"}]"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unsupported_methods_fail() {
    let out = run(
        r#"
        const errors = [];
        try { env.DB.withSession(); } catch (err) { errors.push(err.message); }
        try { await env.DB.dump(); } catch (err) { errors.push(err.message); }
        return [errors, typeof env.DB.query];
        "#,
        Arc::default(),
    )
    .await;

    assert_eq!(
        out,
        r#"[["D1_ERROR: withSession() is not supported","D1_ERROR: dump() is not supported"],"undefined"]"#
    );
}
