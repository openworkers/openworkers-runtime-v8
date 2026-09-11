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

/// The surface is evaluated as a classic script in the guest's own global, so a
/// top-level declaration of its own would collide with a guest that names the
/// same thing.
#[tokio::test(flavor = "current_thread")]
async fn a_guest_may_declare_any_name_at_top_level() {
    run_in_local(|| async {
        let answer = answer_of(
            r#"
            const BASE64_CHARS = 'mine';
            const B64_CODES = 'mine';
            const CHARS = 'mine';
            const CODES = 'mine';
            let i = 0;

            addEventListener('fetch', (event) => {
                event.respondWith(new Response(BASE64_CHARS + btoa('hi')));
            });
        "#,
        )
        .await;

        assert_eq!(answer, "mineaGk=");
    })
    .await;
}
