//! The marks the dispatch leaves when a fetch listener calls respondWith
//! after it returned, read through Worker and through the pool's callback.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::{
    ListenerMarks, PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};

const IN_TIME: &str = "addEventListener('fetch', e => e.respondWith(new Response('ok')));";

const AFTER_AWAIT: &str = r#"
    addEventListener('fetch', async (e) => {
        await Promise.resolve();
        e.respondWith(new Response('ok'));
    });
"#;

const AFTER_SETTLE: &str = r#"
    addEventListener('fetch', async (e) => {
        new Promise((resolve) => setTimeout(resolve, 5))
            .then(() => e.respondWith(new Response('ok')));
    });
"#;

fn get() -> Event {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
    .0
}

async fn worker_marks(code: &str) -> ListenerMarks {
    let code = code.to_string();

    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), None).await.unwrap();
        worker.exec(get()).await.unwrap();

        worker.listener_marks()
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn a_worker_reports_how_its_listener_answered() {
    assert_eq!(worker_marks(IN_TIME).await, ListenerMarks::default());

    assert_eq!(
        worker_marks(AFTER_AWAIT).await,
        ListenerMarks {
            late: true,
            after_settle: false
        }
    );

    assert_eq!(
        worker_marks(AFTER_SETTLE).await,
        ListenerMarks {
            late: true,
            after_settle: true
        }
    );
}

/// What the pool's callback received for one request, None if it was not
/// called.
async fn pool_marks(worker_id: &str, code: &str) -> Option<ListenerMarks> {
    let seen = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&seen);

    execute_pinned(PinnedExecuteRequest {
        owner_id: "owner".to_string(),
        worker_id: worker_id.to_string(),
        version: 1,
        script: Script::new(code),
        ops: Arc::new(DefaultOps),
        task: get(),
        on_warm_hit: None,
        env_updated_at: None,
        abort: None,
        on_marks: Some(Box::new(move |marks| {
            *sink.lock().unwrap() = Some(marks);
        })),
    })
    .await
    .unwrap();

    *seen.lock().unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn the_pool_calls_back_only_for_a_marked_listener() {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 1,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: false,
        max_context_reuses: 100,
        limits: Default::default(),
    });

    run_in_local(|| async {
        assert_eq!(pool_marks("in-time", IN_TIME).await, None);

        assert_eq!(
            pool_marks("after-await", AFTER_AWAIT).await,
            Some(ListenerMarks {
                late: true,
                after_settle: false
            })
        );

        // A warm hit on the same worker reports again
        assert_eq!(
            pool_marks("after-await", AFTER_AWAIT).await,
            Some(ListenerMarks {
                late: true,
                after_settle: false
            })
        );
    })
    .await;
}
