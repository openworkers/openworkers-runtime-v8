mod common;

use std::collections::HashMap;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_runtime_v8::Worker;

async fn answer_of(code: &str) -> String {
    let mut worker = Worker::new(Script::new(code), None).await.unwrap();

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

    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn the_namespace_does_not_show_up_in_the_guest_globals() {
    run_in_local(|| async {
        let answer = answer_of(
            r#"
            addEventListener('fetch', (event) => {
                const listed = Object.keys(globalThis).includes('__ow');
                event.respondWith(new Response(String(listed)));
            });
        "#,
        )
        .await;

        assert_eq!(answer, "false");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_guest_cannot_replace_the_namespace() {
    run_in_local(|| async {
        let answer = answer_of(
            r#"
            addEventListener('fetch', (event) => {
                globalThis.__ow = { urlParse: () => null };
                event.respondWith(new Response(new URL('https://example.com/a').pathname));
            });
        "#,
        )
        .await;

        assert_eq!(answer, "/a");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_guest_cannot_replace_an_op() {
    run_in_local(|| async {
        let answer = answer_of(
            r#"
            addEventListener('fetch', (event) => {
                globalThis.__ow.urlParse = () => null;
                event.respondWith(new Response(new URL('https://example.com/a').pathname));
            });
        "#,
        )
        .await;

        assert_eq!(answer, "/a");
    })
    .await;
}
