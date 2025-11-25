use openworkers_runtime_v8::{HttpRequest, Script, Task, Worker};
use std::collections::HashMap;

/// Test crypto.getRandomValues
#[tokio::test]
async fn test_get_random_values() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const array = new Uint8Array(16);
            crypto.getRandomValues(array);

            // Check that at least some bytes are non-zero
            const hasNonZero = array.some(b => b !== 0);

            event.respondWith(new Response(hasNonZero ? 'OK' : 'FAIL'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test crypto.subtle.digest with SHA-256
#[tokio::test]
async fn test_digest_sha256() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const data = new TextEncoder().encode('hello world');
            const hash = await crypto.subtle.digest('SHA-256', data);

            // SHA-256 of "hello world" is known
            const hashArray = new Uint8Array(hash);
            const hashHex = Array.from(hashArray)
                .map(b => b.toString(16).padStart(2, '0'))
                .join('');

            // Expected: b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
            const expected = 'b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9';

            event.respondWith(new Response(hashHex === expected ? 'OK' : 'FAIL: ' + hashHex));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test crypto.subtle.digest with SHA-512
#[tokio::test]
async fn test_digest_sha512() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const data = new TextEncoder().encode('test');
            const hash = await crypto.subtle.digest('SHA-512', data);

            // SHA-512 produces 64 bytes
            const hashArray = new Uint8Array(hash);

            event.respondWith(new Response(hashArray.length === 64 ? 'OK' : 'FAIL'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test HMAC sign and verify
#[tokio::test]
async fn test_hmac_sign_verify() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Create a key
            const keyData = new TextEncoder().encode('my-secret-key');
            const key = await crypto.subtle.importKey(
                'raw',
                keyData,
                { name: 'HMAC', hash: 'SHA-256' },
                false,
                ['sign', 'verify']
            );

            // Sign some data
            const data = new TextEncoder().encode('hello world');
            const signature = await crypto.subtle.sign('HMAC', key, data);

            // Verify the signature
            const isValid = await crypto.subtle.verify('HMAC', key, signature, data);

            // Try to verify with wrong data
            const wrongData = new TextEncoder().encode('wrong data');
            const isInvalid = await crypto.subtle.verify('HMAC', key, signature, wrongData);

            const result = isValid && !isInvalid ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}

/// Test HMAC with different hash algorithms
#[tokio::test]
async fn test_hmac_different_algorithms() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const keyData = new TextEncoder().encode('secret');
            const data = new TextEncoder().encode('message');

            // Test SHA-256
            const key256 = await crypto.subtle.importKey(
                'raw', keyData, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']
            );
            const sig256 = await crypto.subtle.sign('HMAC', key256, data);
            const len256 = new Uint8Array(sig256).length;

            // Test SHA-384
            const key384 = await crypto.subtle.importKey(
                'raw', keyData, { name: 'HMAC', hash: 'SHA-384' }, false, ['sign']
            );
            const sig384 = await crypto.subtle.sign('HMAC', key384, data);
            const len384 = new Uint8Array(sig384).length;

            // Test SHA-512
            const key512 = await crypto.subtle.importKey(
                'raw', keyData, { name: 'HMAC', hash: 'SHA-512' }, false, ['sign']
            );
            const sig512 = await crypto.subtle.sign('HMAC', key512, data);
            const len512 = new Uint8Array(sig512).length;

            // SHA-256 = 32 bytes, SHA-384 = 48 bytes, SHA-512 = 64 bytes
            const result = (len256 === 32 && len384 === 48 && len512 === 64) ? 'OK' : 'FAIL';
            event.respondWith(new Response(result));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();

    let body = response.body.as_bytes().unwrap();
    assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
}
