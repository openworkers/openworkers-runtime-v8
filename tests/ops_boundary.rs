//! A native op either does what it was asked or throws a named error. It never
//! substitutes a default, and it never returns without settling the promise
//! it was given.

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Runs `expr` in a fetch handler and answers what it threw, or `no throw`.
async fn thrown_by(expr: &str) -> String {
    let code = format!(
        r#"
        addEventListener('fetch', async (event) => {{
            let out;
            try {{
                await ({expr});
                out = 'no throw';
            }} catch (e) {{
                out = e.name + ': ' + e.message;
            }}
            event.respondWith(new Response(out));
        }});
        "#
    );

    run_in_local(|| async move {
        let mut worker = Worker::new(Script::new(code), None).await.unwrap();
        let (task, rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();
        let body = response.body.collect().await.unwrap().unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    })
    .await
}

macro_rules! throws {
    ($name:ident, $expr:expr, $expected:expr) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $name() {
            let out = thrown_by($expr).await;
            assert!(out.starts_with($expected), "got: {out}");
        }
    };
}

// fetch

throws!(
    fetch_unknown_method,
    "__nativeBindingFetch('ASSETS', { url: 'http://x/', method: 'BREW' }, () => {}, () => {})",
    "TypeError: fetch: unknown method \"BREW\""
);

throws!(
    fetch_options_not_an_object,
    "__nativeFetchStreaming(42, () => {}, () => {})",
    "TypeError: fetch: the options are not a request object"
);

throws!(
    fetch_without_callbacks,
    "__nativeFetchStreaming({ url: 'http://x/' })",
    "TypeError: the resolve and reject callbacks are not functions"
);

throws!(
    binding_fetch_name_not_a_string,
    "__nativeBindingFetch(1, { url: 'http://x/' }, () => {}, () => {})",
    "TypeError: binding fetch: the binding name is not a string"
);

// storage

throws!(
    storage_put_without_body,
    "__nativeBindingStorage('S', 'put', { key: 'k' }, () => {}, () => {})",
    "TypeError: storage put: takes a body"
);

throws!(
    storage_unknown_operation,
    "__nativeBindingStorage('S', 'frob', { key: 'k' }, () => {}, () => {})",
    "TypeError: storage: unknown operation \"frob\""
);

throws!(
    storage_bad_parameters,
    "__nativeBindingStorage('S', 'get', 'nope', () => {}, () => {})",
    "TypeError: storage: bad parameters"
);

throws!(
    storage_without_callbacks,
    "__nativeBindingStorage('S', 'get', { key: 'k' })",
    "TypeError: the resolve and reject callbacks are not functions"
);

// kv

throws!(
    kv_put_without_value,
    "__nativeBindingKv('K', 'put', { key: 'k' }, () => {}, () => {})",
    "TypeError: kv put: takes a value"
);

throws!(
    kv_unknown_operation,
    "__nativeBindingKv('K', 'frob', { key: 'k' }, () => {}, () => {})",
    "TypeError: kv: unknown operation \"frob\""
);

// database

throws!(
    database_without_callbacks,
    "__nativeBindingDatabase('DB', 'query', { sql: 'SELECT 1' })",
    "TypeError: the resolve and reject callbacks are not functions"
);

throws!(
    database_without_sql,
    "__nativeBindingDatabase('DB', 'query', { params: [] }, () => {}, () => {})",
    "TypeError: database: bad parameters"
);

throws!(
    database_unknown_operation,
    "__nativeBindingDatabase('DB', 'frob', { sql: 'SELECT 1' }, () => {}, () => {})",
    "TypeError: database: unknown operation \"frob\""
);

throws!(
    database_name_not_a_string,
    "__nativeBindingDatabase(1, 'query', { sql: 'SELECT 1' }, () => {}, () => {})",
    "TypeError: database: the binding name is not a string"
);

// websocket

throws!(
    websocket_headers_not_an_object,
    "__nativeWebSocketConnect('ws://x/', 'nope', () => {}, () => {})",
    "TypeError: WebSocket: the headers are not a name/value object"
);

throws!(
    websocket_headers_absent_are_empty,
    "__nativeWebSocketConnect('ws://x/', null, () => {}, () => {})",
    "no throw"
);

throws!(
    websocket_accept_id_not_a_number,
    "__nativeWebSocketAccept('abc', () => {})",
    "TypeError: WebSocket: the socket id is not a number"
);

throws!(
    websocket_accept_dispatcher_not_a_function,
    "__nativeWebSocketAccept(1, 'x')",
    "TypeError: WebSocket: the dispatcher is not a function"
);

throws!(
    websocket_send_id_not_a_number,
    "__nativeWebSocketSend(undefined, 'hi')",
    "TypeError: WebSocket: the socket id is not a number"
);

throws!(
    websocket_send_rejects_an_object,
    "__nativeWebSocketSend(1, {})",
    "TypeError: WebSocket: send takes a string, an ArrayBuffer or a view"
);

throws!(
    websocket_close_id_not_a_number,
    "__nativeWebSocketClose('1', 1000, '')",
    "TypeError: WebSocket: the socket id is not a number"
);

throws!(
    websocket_close_code_not_a_number,
    "__nativeWebSocketClose(1, 'abc', '')",
    "TypeError: WebSocket: the close code is not a number"
);

// compression streams

throws!(
    compression_push_id_not_a_stream_id,
    "__ow.compressionPush('x', new Uint8Array(0))",
    "TypeError: a compression stream op takes a stream id"
);

throws!(
    compression_finish_id_not_a_stream_id,
    "__ow.compressionFinish(-1)",
    "TypeError: a compression stream op takes a stream id"
);

throws!(
    compression_drop_id_not_a_stream_id,
    "__ow.compressionDrop(1.5)",
    "TypeError: a compression stream op takes a stream id"
);

// AES-GCM

throws!(
    aes_gcm_key_of_24_bytes,
    "crypto.subtle.__nativeAesGcmSeal(new Uint8Array(24), new Uint8Array(12), new Uint8Array(1), new Uint8Array(0))",
    "TypeError: AES-GCM: the key is 24 bytes, not 16 or 32"
);

throws!(
    aes_gcm_iv_of_11_bytes,
    "crypto.subtle.__nativeAesGcmSeal(new Uint8Array(16), new Uint8Array(11), new Uint8Array(1), new Uint8Array(0))",
    "TypeError: AES-GCM: the iv is 11 bytes, not 12"
);

throws!(
    aes_gcm_key_not_a_uint8array,
    "crypto.subtle.__nativeAesGcmOpen('key', new Uint8Array(12), new Uint8Array(16), new Uint8Array(0))",
    "TypeError: AES-GCM: argument 0 is not a Uint8Array"
);

throws!(
    aes_gcm_decrypt_of_a_tampered_ciphertext,
    r#"(async () => {
        const key = await crypto.subtle.importKey('raw', new Uint8Array(16), { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
        const iv = new Uint8Array(12);
        const sealed = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, key, new Uint8Array([1, 2, 3])));
        sealed[0] ^= 1;
        await crypto.subtle.decrypt({ name: 'AES-GCM', iv }, key, sealed);
    })()"#,
    "Error: AES-GCM: decryption failed"
);

// PBKDF2

throws!(
    pbkdf2_zero_iterations,
    "crypto.subtle.__nativePbkdf2DeriveBits('SHA-256', new Uint8Array(1), new Uint8Array(1), 0, 256)",
    "TypeError: PBKDF2: the iteration count must be at least 1"
);

throws!(
    pbkdf2_unknown_hash,
    "crypto.subtle.__nativePbkdf2DeriveBits('MD5', new Uint8Array(1), new Uint8Array(1), 1, 256)",
    "TypeError: PBKDF2: unknown hash \"MD5\""
);

throws!(
    pbkdf2_iterations_not_a_number,
    "crypto.subtle.__nativePbkdf2DeriveBits('SHA-256', new Uint8Array(1), new Uint8Array(1), 'x', 256)",
    "TypeError: PBKDF2: the iteration count is not a number"
);

throws!(
    pbkdf2_salt_not_a_uint8array,
    "crypto.subtle.__nativePbkdf2DeriveBits('SHA-256', new Uint8Array(1), 'salt', 1, 256)",
    "TypeError: PBKDF2: argument 2 is not a Uint8Array"
);
