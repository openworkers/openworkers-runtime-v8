//! Cancelling a request must close the WebSocket its guest is waiting on.
//!
//! The command loop used to leave on cancellation without a word, and a guest
//! awaiting 'close' stayed pending for the life of the context.

mod common;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::RuntimeLimits;
use openworkers_core::Script;
use openworkers_core::WebSocketConnection;
use openworkers_core::WebSocketIncoming;
use openworkers_core::WebSocketOutgoing;
use openworkers_runtime_v8::PinnedExecuteRequest;
use openworkers_runtime_v8::PinnedPoolConfig;
use openworkers_runtime_v8::execute_pinned;
use openworkers_runtime_v8::init_pinned_pool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Connects at once and then says nothing, holding its ends open.
#[derive(Default)]
struct SilentServer {
    ends: Mutex<
        Vec<(
            mpsc::UnboundedReceiver<WebSocketOutgoing>,
            mpsc::UnboundedSender<WebSocketIncoming>,
        )>,
    >,
}

impl OperationsHandler for SilentServer {
    fn handle_websocket_connect(
        &self,
        _url: &str,
        _headers: HashMap<String, String>,
    ) -> OpFuture<'_, Result<WebSocketConnection, String>> {
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();
        self.ends.lock().unwrap().push((send_rx, recv_tx));

        Box::pin(async move { Ok(WebSocketConnection { send_tx, recv_rx }) })
    }
}

const SCRIPT: &str = r#"
    addEventListener('fetch', (event) => {
        const ws = new WebSocket('wss://upstream/socket');

        event.respondWith(new Promise((resolve) => {
            ws.addEventListener('close', (e) => {
                resolve(new Response('closed:' + e.code + ':' + e.reason));
            });
        }));
    });
"#;

#[tokio::test(flavor = "current_thread")]
async fn cancelling_the_request_closes_the_socket_the_guest_awaits() {
    run_in_local(|| async {
        init_pinned_pool(PinnedPoolConfig {
            max_per_thread: 10,
            max_per_owner: None,
            max_concurrent_per_isolate: 20,
            max_cached_contexts: 10,
            limits: RuntimeLimits::default(),
        });

        let abort = CancellationToken::new();

        tokio::spawn({
            let abort = abort.clone();

            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                abort.cancel();
            }
        });

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };
        let (task, rx) = Event::fetch(request);

        let finished = tokio::time::timeout(
            Duration::from_secs(5),
            execute_pinned(PinnedExecuteRequest {
                owner_id: "ws-cancel".to_string(),
                worker_id: "ws-cancel-worker".to_string(),
                version: 1,
                script: Script::new(SCRIPT),
                ops: Arc::new(SilentServer::default()),
                task,
                on_warm_hit: None,
                env_updated_at: None,
                abort: Some(abort),
            }),
        )
        .await;

        finished
            .expect("a cancelled request must not leave the guest waiting on 'close'")
            .unwrap();

        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();

        // A failed connect also reports 1006, with an empty reason or the
        // connect error; the reason is what tells the cancellation apart.
        assert_eq!(
            String::from_utf8_lossy(&body),
            "closed:1006:Operation cancelled"
        );
    })
    .await;
}
