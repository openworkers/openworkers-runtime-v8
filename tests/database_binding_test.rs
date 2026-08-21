//! What a database handler's rows look like once they reach the guest.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::run_in_local;
use openworkers_core::BindingInfo;
use openworkers_core::BindingType;
use openworkers_core::DatabaseOp;
use openworkers_core::DatabaseResult;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandle;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_core::SqlPrimitive;

/// Answers every query with one typed row covering each `SqlPrimitive`.
struct TypedRows;

impl OperationsHandler for TypedRows {
    fn handle_binding_database(
        &self,
        _binding: &str,
        _op: DatabaseOp,
    ) -> OpFuture<'_, DatabaseResult> {
        Box::pin(async move {
            DatabaseResult::Table {
                columns: vec![
                    "id".to_string(),
                    "name".to_string(),
                    "score".to_string(),
                    "active".to_string(),
                    "avatar".to_string(),
                    "missing".to_string(),
                ],
                rows: vec![vec![
                    SqlPrimitive::Int(7),
                    SqlPrimitive::String("row".to_string()),
                    SqlPrimitive::Float(1.5),
                    SqlPrimitive::Bool(true),
                    SqlPrimitive::Bytes(vec![0, 127, 255]),
                    SqlPrimitive::Null,
                ]],
            }
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_typed_rows_reach_the_guest() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith(
                    env.DB.query('select 1').then(rows => new Response(JSON.stringify(rows)))
                );
            });
        "#;

        let script = Script::with_bindings(
            code,
            None,
            vec![BindingInfo::new("DB", BindingType::Database)],
        );

        let ops: OperationsHandle = Arc::new(TypedRows);
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

        assert_eq!(
            String::from_utf8_lossy(&body),
            r#"[{"id":7,"name":"row","score":1.5,"active":true,"avatar":[0,127,255],"missing":null}]"#
        );
    })
    .await;
}
