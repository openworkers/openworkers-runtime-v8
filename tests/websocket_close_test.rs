//! An outgoing WebSocket tells the guest when it closes, whoever closes it.

mod common;

use common::run_in_local;
use openworkers_core::Event;
use openworkers_core::HttpMethod;
use openworkers_core::HttpRequest;
use openworkers_core::OpFuture;
use openworkers_core::OperationsHandler;
use openworkers_core::RequestBody;
use openworkers_core::Script;
use openworkers_core::WebSocketConnection;
use openworkers_core::WebSocketIncoming;
use openworkers_core::WebSocketOutgoing;
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// A server that keeps its ends, or drops them at once to stand for a
/// connection that goes away without a close frame.
struct Server {
    hang_up: bool,
    ends: Mutex<
        Vec<(
            mpsc::UnboundedReceiver<WebSocketOutgoing>,
            mpsc::UnboundedSender<WebSocketIncoming>,
        )>,
    >,
}

impl OperationsHandler for Server {
    fn handle_websocket_connect(
        &self,
        _url: &str,
        _headers: HashMap<String, String>,
    ) -> OpFuture<'_, Result<WebSocketConnection, String>> {
        let (send_tx, send_rx) = mpsc::unbounded_channel();
        let (recv_tx, recv_rx) = mpsc::unbounded_channel();

        if !self.hang_up {
            self.ends.lock().unwrap().push((send_rx, recv_tx));
        }

        Box::pin(async move { Ok(WebSocketConnection { send_tx, recv_rx }) })
    }
}

async fn close_event(script: &str, hang_up: bool) -> String {
    let ops = Arc::new(Server {
        hang_up,
        ends: Mutex::new(Vec::new()),
    });
    let mut worker = Worker::new_with_ops(Script::new(script), None, ops)
        .await
        .unwrap();

    let (task, rx) = Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    });

    tokio::time::timeout(Duration::from_secs(5), worker.exec(task))
        .await
        .expect("the close event settles the response")
        .unwrap();

    let body = rx.await.unwrap().body.collect().await.unwrap().unwrap();

    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn closing_the_socket_fires_close() {
    run_in_local(|| async {
        let script = r#"
            addEventListener('fetch', (event) => {
                const ws = new WebSocket('wss://upstream/socket');
                ws.addEventListener('open', () => ws.close(1000, 'done'));

                event.respondWith(new Promise((resolve) => {
                    ws.addEventListener('close', (e) => {
                        resolve(new Response(e.code + ':' + e.reason + ':' + ws.readyState));
                    });
                }));
            });
        "#;

        assert_eq!(close_event(script, false).await, "1000:done:3");
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_goes_away_fires_close() {
    run_in_local(|| async {
        let script = r#"
            addEventListener('fetch', (event) => {
                const ws = new WebSocket('wss://upstream/socket');

                event.respondWith(new Promise((resolve) => {
                    ws.addEventListener('close', (e) => resolve(new Response(String(e.code))));
                }));
            });
        "#;

        assert_eq!(close_event(script, true).await, "1006");
    })
    .await;
}
