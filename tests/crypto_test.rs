mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

/// Runs `body` as the body of an async function inside a fetch listener and
/// returns what it returns, as text; a throw answers with its message.
async fn run_fetch(body: &str) -> String {
    let code = format!(
        r#"
            addEventListener('fetch', async (event) => {{
                let result;
                try {{
                    result = await (async () => {{ {body} }})();
                }} catch (e) {{
                    result = 'THREW: ' + e;
                }}
                event.respondWith(new Response(String(result)));
            }});
        "#
    );

    run_in_local(|| async move {
        let script = Script::new(code.as_str());
        let mut worker = Worker::new(script, None).await.unwrap();

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
    })
    .await
}

/// Test crypto.getRandomValues
#[tokio::test(flavor = "current_thread")]
async fn test_get_random_values() {
    run_in_local(|| async {
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
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test crypto.subtle.digest with SHA-256
#[tokio::test(flavor = "current_thread")]
async fn test_digest_sha256() {
    run_in_local(|| async {
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
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test crypto.subtle.digest with SHA-512
#[tokio::test(flavor = "current_thread")]
async fn test_digest_sha512() {
    run_in_local(|| async {
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
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test HMAC sign and verify
#[tokio::test(flavor = "current_thread")]
async fn test_hmac_sign_verify() {
    run_in_local(|| async {
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
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test HMAC with different hash algorithms
#[tokio::test(flavor = "current_thread")]
async fn test_hmac_different_algorithms() {
    run_in_local(|| async {
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
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test ECDSA P-256 key generation, sign and verify
#[tokio::test(flavor = "current_thread")]
async fn test_ecdsa_sign_verify() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Generate an ECDSA P-256 key pair
                const keyPair = await crypto.subtle.generateKey(
                    { name: 'ECDSA', namedCurve: 'P-256' },
                    true,
                    ['sign', 'verify']
                );

                // Sign some data
                const data = new TextEncoder().encode('hello world');
                const signature = await crypto.subtle.sign(
                    { name: 'ECDSA', hash: 'SHA-256' },
                    keyPair.privateKey,
                    data
                );

                // Verify with public key
                const isValid = await crypto.subtle.verify(
                    { name: 'ECDSA', hash: 'SHA-256' },
                    keyPair.publicKey,
                    signature,
                    data
                );

                // Try to verify with wrong data
                const wrongData = new TextEncoder().encode('wrong data');
                const isInvalid = await crypto.subtle.verify(
                    { name: 'ECDSA', hash: 'SHA-256' },
                    keyPair.publicKey,
                    signature,
                    wrongData
                );

                // ECDSA P-256 signature is 64 bytes (r||s, each 32 bytes)
                const sigLen = new Uint8Array(signature).length;

                const result = isValid && !isInvalid && sigLen === 64 ? 'OK' : `FAIL: isValid=${isValid}, isInvalid=${isInvalid}, sigLen=${sigLen}`;
                event.respondWith(new Response(result));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Test ECDSA verify with private key (should use embedded public key)
#[tokio::test(flavor = "current_thread")]
async fn test_ecdsa_verify_with_private_key() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Generate key pair
                const keyPair = await crypto.subtle.generateKey(
                    { name: 'ECDSA', namedCurve: 'P-256' },
                    true,
                    ['sign', 'verify']
                );

                // Sign data
                const data = new TextEncoder().encode('test message');
                const signature = await crypto.subtle.sign(
                    { name: 'ECDSA', hash: 'SHA-256' },
                    keyPair.privateKey,
                    data
                );

                // Verify with private key (should work, uses embedded public key)
                const isValid = await crypto.subtle.verify(
                    { name: 'ECDSA', hash: 'SHA-256' },
                    keyPair.privateKey,
                    signature,
                    data
                );

                event.respondWith(new Response(isValid ? 'OK' : 'FAIL'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// The RSA-2048 test key, as `openssl genpkey -algorithm RSA` made it:
/// the private key as PKCS#8 (`openssl pkcs8 -topk8 -nocrypt -outform DER`),
/// the public key as SubjectPublicKeyInfo (`openssl pkey -pubout -outform DER`)
/// and bare (`openssl rsa -RSAPublicKey_out -outform DER`).
const RSA_PKCS8_BASE64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCny/+Fpl7ZcM0tQ/AmbtOY4QJij6UKeA58LNvjTi6usgHoTQaIsAtJTAIdZg3M2v2hhmQMRvOXAoox4MYfzPSbSMK9Z7kun0zYw5ziijjwq3y7B/pOUfRWYHy1dgZDeHeKu6Y2CYZeAEmCq1Ljtq8PPQAQO3VmE4SHRZq1rtcABpTul8YTgpGFmk+wnPByWsOHQvsO6BTbYoLTlBf9Ff5kUnblNJhmcfwjQGcTIFKEAQggJsr/tPLq3bMSdxYl05WjLuFHLonojioKpGNcI2LI8+kj8wQ0lXZm2rWSzcgR4KWLS2NYcQQcYp+ZTT5dap/o9x/HVNB48AhHldabgMV9AgMBAAECggEAHac3uCkDZJZicBAsScJ2qvMCvqPHhR7b4nZ0AorPxagoHaM1FyVTTUf9LLBbGnuN7IRpPGEyjZqRjQh9wuNvy9xzK9E/gN1+kWUaXc+TCfcoUw4xHjOuBDDHgTMDHtvUdmQ8lpqe0BBpbUn1G1BuxfjgAL5dPCWRW22Bzn9AOBzn2xcbhJIyHvt67AjH4z1gTxiWrwVAT0dQTY0hmBbHXpgjQdKm84c9o9PmQMOVKryJ5GeKtFnJvKH0NYYab3+QG8Yh3ABArvfm5clXolsDOnX4OTBv1qHQ6+rOaDBfVrLsJnkC+gtP8GAmDbNaMPyQ7E5Zf7RUkPRxX/Y0ufKaQQKBgQDqFvoIjEA+eFv4Ux0Ah7dSkclrBWccC07eIejJuCUSbaIMY4YLdgSftwn87OhRs6gAOQJvYUNgrO52OsLuVwRfti8MO3JGehK4v3uA8lCSfbKKvrlt7vv+IrQuQM+rTSf4jYb0gJGcUYuIdWLFdkm8qJMRbDi3KVCgHcIpi0B8QQKBgQC3gJP2a2BCeyWILO2GEoTQwGfx/50ixq4O6qHIBdweFMWsDPFkR+duRRw0Ds2w9QCV3vcQniajPGZNjx0ZdGqMQAEu4MY4+5AeN9G0GJxxLqHUtpXUsPneWOD3a2ULGbcuejDz0XCLZEyLs8IRq78hYH2Rb3cushP8XCqVcqCqPQKBgQDmPFcLVTZSuvpqEQTzYohyE6VxN00kjhKx89QLoqwDpgS9/pz2ZMtDcznFpBUTVookPe4hMh6c1Tls23qiBL/uizdW5pkMrEABqYOFXc7VZf/W6qNidq0uVV+2JlSafTaVBk336QROJP4B5sKQyDjZ70tG1ZQqwd3kvaAcUDPKgQKBgCCOLDH8rNA+ntMA/YbaxDtw10Ak1FD2JK06zUb6WynvD37NsQnUg+eZVT6bHbz2SotMSlLla/9r2M6LxGLet4R4Wn1hnWlAoDnsN0UXVLHzzvw5BG3+k+XxqL/cismkX05cmVC4aJoiSj5Cvvx5lugqAT0LJH7hUxBjnZ5z/rMVAoGBAM6K7oQynzQLCeGIcV97QMSswCNAQgNtOZQfb47LQtSMZQ5SV4IZTD4bIMwnABn4205QfsWt5OcfcCypP/w0F6GeUofCmw9+IDbFNpDj9QldzjPq30ziXv/U2HSQtTiPKm+fNrOV0djB/aBSL4IeD0CsDCYHXe7ckvxzt3UxukXi";
const RSA_SPKI_BASE64: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAp8v/haZe2XDNLUPwJm7TmOECYo+lCngOfCzb404urrIB6E0GiLALSUwCHWYNzNr9oYZkDEbzlwKKMeDGH8z0m0jCvWe5Lp9M2MOc4oo48Kt8uwf6TlH0VmB8tXYGQ3h3irumNgmGXgBJgqtS47avDz0AEDt1ZhOEh0Wata7XAAaU7pfGE4KRhZpPsJzwclrDh0L7DugU22KC05QX/RX+ZFJ25TSYZnH8I0BnEyBShAEIICbK/7Ty6t2zEncWJdOVoy7hRy6J6I4qCqRjXCNiyPPpI/MENJV2Ztq1ks3IEeCli0tjWHEEHGKfmU0+XWqf6Pcfx1TQePAIR5XWm4DFfQIDAQAB";
const RSA_PUBLIC_KEY_BASE64: &str = "MIIBCgKCAQEAp8v/haZe2XDNLUPwJm7TmOECYo+lCngOfCzb404urrIB6E0GiLALSUwCHWYNzNr9oYZkDEbzlwKKMeDGH8z0m0jCvWe5Lp9M2MOc4oo48Kt8uwf6TlH0VmB8tXYGQ3h3irumNgmGXgBJgqtS47avDz0AEDt1ZhOEh0Wata7XAAaU7pfGE4KRhZpPsJzwclrDh0L7DugU22KC05QX/RX+ZFJ25TSYZnH8I0BnEyBShAEIICbK/7Ty6t2zEncWJdOVoy7hRy6J6I4qCqRjXCNiyPPpI/MENJV2Ztq1ks3IEeCli0tjWHEEHGKfmU0+XWqf6Pcfx1TQePAIR5XWm4DFfQIDAQAB";

/// The JS that binds the RSA test key to `rsaPkcs8`, `rsaSpki` and `rsaPublicKey`.
fn rsa_test_keys() -> String {
    format!(
        r#"
        const fromBase64 = (text) => Uint8Array.from(atob(text), (c) => c.charCodeAt(0));
        const rsaPkcs8 = fromBase64('{RSA_PKCS8_BASE64}');
        const rsaSpki = fromBase64('{RSA_SPKI_BASE64}');
        const rsaPublicKey = fromBase64('{RSA_PUBLIC_KEY_BASE64}');
        "#
    )
}

/// Test RSA PKCS#1 v1.5 sign and verify with real PKCS#8 and SPKI keys
#[tokio::test(flavor = "current_thread")]
async fn test_rsa_sign_verify() {
    let body = rsa_test_keys()
        + r#"
        const privateKey = await crypto.subtle.importKey(
            'pkcs8', rsaPkcs8, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign']
        );
        const publicKey = await crypto.subtle.importKey(
            'spki', rsaSpki, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['verify']
        );

        const data = new TextEncoder().encode('hello world');
        const signature = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', privateKey, data);

        const isValid = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', publicKey, signature, data);

        const wrongData = new TextEncoder().encode('wrong data');
        const isInvalid = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', publicKey, signature, wrongData);

        // RSA-2048 signature is 256 bytes
        const sigLen = new Uint8Array(signature).length;

        return isValid && !isInvalid && sigLen === 256
            ? 'OK'
            : `FAIL: isValid=${isValid}, isInvalid=${isInvalid}, sigLen=${sigLen}`;
    "#;

    assert_eq!(run_fetch(&body).await, "OK");
}

/// importKey parses the key: bare DER under "pkcs8" or "spki", and bytes that
/// are no key, reject with a DataError there, not at sign or verify. The bare
/// RSAPublicKey is still taken as "raw".
#[tokio::test(flavor = "current_thread")]
async fn test_rsa_import_key_rejects_what_is_not_that_format() {
    let body = rsa_test_keys()
        + r#"
        const algorithm = { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' };
        const rsaPrivateKey = rsaPkcs8.slice(26); // the RSAPrivateKey inside the PKCS#8

        const accepted = [];
        const attempts = [
            ['pkcs8', rsaPrivateKey, ['sign']],
            ['pkcs8', rsaSpki, ['sign']],
            ['pkcs8', new Uint8Array(64), ['sign']],
            ['spki', rsaPublicKey, ['verify']],
            ['spki', rsaPkcs8, ['verify']],
            ['spki', new Uint8Array(64), ['verify']],
            ['raw', rsaSpki, ['verify']],
        ];
        for (const [format, bytes, usages] of attempts) {
            try {
                await crypto.subtle.importKey(format, bytes, algorithm, false, usages);
                accepted.push(format + ' ' + bytes.length);
            } catch (e) {
                if (!(e instanceof DOMException) || e.name !== 'DataError') {
                    accepted.push(format + ' ' + bytes.length + ': ' + e);
                }
            }
        }

        const privateKey = await crypto.subtle.importKey('pkcs8', rsaPkcs8, algorithm, false, ['sign']);
        const rawKey = await crypto.subtle.importKey('raw', rsaPublicKey, algorithm, false, ['verify']);
        const data = new TextEncoder().encode('hello world');
        const signature = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', privateKey, data);
        const rawVerifies = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', rawKey, signature, data);

        return accepted.length === 0 && rawVerifies
            ? 'OK'
            : 'FAIL: accepted ' + accepted.join(', ') + ', rawVerifies=' + rawVerifies;
    "#;

    assert_eq!(run_fetch(&body).await, "OK");
}

/// A view into part of a buffer must be filled where it points, not at offset 0
#[tokio::test(flavor = "current_thread")]
async fn test_get_random_values_honours_byte_offset() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                const buffer = new Uint8Array(16);
                const window = buffer.subarray(4, 12);

                crypto.getRandomValues(window);

                const untouched = buffer.slice(0, 4).every(b => b === 0)
                    && buffer.slice(12).every(b => b === 0);
                const filled = window.some(b => b !== 0);

                event.respondWith(new Response(untouched && filled ? 'OK' : 'FAIL'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// An imported AES-GCM key must decrypt what it encrypted, additional data included
#[tokio::test(flavor = "current_thread")]
async fn test_aes_gcm_imported_key_round_trip() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const raw = new Uint8Array(32).fill(7);
                const key = await crypto.subtle.importKey(
                    'raw', raw, { name: 'AES-GCM' }, true, ['encrypt', 'decrypt']
                );
                const algorithm = {
                    name: 'AES-GCM',
                    iv: new Uint8Array(12).fill(3),
                    additionalData: new TextEncoder().encode('header')
                };

                const cipher = await crypto.subtle.encrypt(
                    algorithm, key, new TextEncoder().encode('secret')
                );
                const plain = await crypto.subtle.decrypt(algorithm, key, cipher);

                const exported = new Uint8Array(await crypto.subtle.exportKey('raw', key));
                const sameKey = exported.length === 32 && exported.every(b => b === 7);

                // The tag is appended, so the ciphertext is 16 bytes longer than the input
                const tagged = cipher.byteLength === 6 + 16;

                const decoded = new TextDecoder().decode(plain);

                event.respondWith(new Response(
                    sameKey && tagged && decoded === 'secret' ? 'OK' : 'FAIL'
                ));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Decryption must fail when the additional data does not match
#[tokio::test(flavor = "current_thread")]
async fn test_aes_gcm_rejects_wrong_additional_data() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', async (event) => {
                const key = await crypto.subtle.generateKey(
                    { name: 'AES-GCM', length: 128 }, true, ['encrypt', 'decrypt']
                );
                const iv = crypto.getRandomValues(new Uint8Array(12));
                const encode = (text) => new TextEncoder().encode(text);

                const cipher = await crypto.subtle.encrypt(
                    { name: 'AES-GCM', iv, additionalData: encode('a') }, key, encode('secret')
                );

                let rejected = false;

                try {
                    await crypto.subtle.decrypt(
                        { name: 'AES-GCM', iv, additionalData: encode('b') }, key, cipher
                    );
                } catch (e) {
                    rejected = true;
                }

                event.respondWith(new Response(rejected ? 'OK' : 'FAIL'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let (task, rx) = Event::fetch(req);
        worker.exec(task).await.unwrap();
        let response = rx.await.unwrap();

        let body = &response.body.collect().await.unwrap().unwrap();
        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// deriveBits allocates its output before deriving, so the length must be a
/// bounded, whole, positive multiple of 8; Infinity used to abort the process
#[tokio::test(flavor = "current_thread")]
async fn test_pbkdf2_derive_bits_rejects_unusable_lengths() {
    let body = r#"
        const key = await crypto.subtle.importKey(
            'raw', new TextEncoder().encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']
        );
        const params = { name: 'PBKDF2', salt: new Uint8Array(8), iterations: 10, hash: 'SHA-256' };

        const accepted = [];
        for (const length of [Infinity, 0, 100, 2 ** 40, -8, NaN]) {
            try {
                await crypto.subtle.deriveBits(params, key, length);
                accepted.push(length);
            } catch (e) {
                if (!(e instanceof TypeError)) accepted.push(length + ': ' + e);
            }
        }

        const bits = await crypto.subtle.deriveBits(params, key, 256);

        return accepted.length === 0 && bits.byteLength === 32
            ? 'OK'
            : 'FAIL: accepted ' + accepted.join(', ') + ', got ' + bits.byteLength + ' bytes';
    "#;

    assert_eq!(run_fetch(body).await, "OK");
}

/// The iteration count is run synchronously on the isolate thread, so it is
/// bounded; `as u32` used to saturate 1e12 into four billion iterations
#[tokio::test(flavor = "current_thread")]
async fn test_pbkdf2_derive_bits_bounds_iterations() {
    let body = r#"
        const key = await crypto.subtle.importKey(
            'raw', new TextEncoder().encode('password'), { name: 'PBKDF2' }, false, ['deriveBits']
        );
        const derive = (iterations) => crypto.subtle.deriveBits(
            { name: 'PBKDF2', salt: new Uint8Array(8), iterations, hash: 'SHA-256' }, key, 256
        );

        const accepted = [];
        for (const iterations of [1e12, 10000001, 2 ** 32, 1.5, Infinity]) {
            try {
                await derive(iterations);
                accepted.push(iterations);
            } catch (e) {
                if (!(e instanceof TypeError)) accepted.push(iterations + ': ' + e);
            }
        }

        const bits = await derive(1000);

        return accepted.length === 0 && bits.byteLength === 32
            ? 'OK'
            : 'FAIL: accepted ' + accepted.join(', ');
    "#;

    assert_eq!(run_fetch(body).await, "OK");
}

/// A key's bytes are not on the key object, so a non-extractable key cannot be
/// read out, serialized or forged: no own property holds them, the attributes
/// cannot be written over, and the helper that made keys is no longer global
#[tokio::test(flavor = "current_thread")]
async fn test_crypto_key_hides_its_material() {
    let body = r#"
        const secret = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        const key = await crypto.subtle.importKey(
            'raw', secret, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']
        );
        const pair = await crypto.subtle.generateKey(
            { name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']
        );

        const failures = [];
        const expect = (name, ok) => { if (!ok) failures.push(name); };

        expect('__keyData', key.__keyData === undefined);
        expect('__publicKeyData', pair.privateKey.__publicKeyData === undefined);
        expect('own keys', !Object.keys(key).includes('__keyData'));
        expect('json', !JSON.stringify(key).includes('1,2,3'));
        expect('json private', JSON.stringify(pair.privateKey) === '{}');
        expect('__createCryptoKey', globalThis.__createCryptoKey === undefined);
        expect('instanceof', key instanceof CryptoKey);
        expect('type', key.type === 'secret');
        expect('extractable', key.extractable === false);
        expect('usages', key.usages.length === 1 && key.usages[0] === 'sign');
        expect('algorithm', key.algorithm.name === 'HMAC' && key.algorithm.hash.name === 'SHA-256');

        // The attributes are read-only: a guest cannot make a key extractable
        key.extractable = true;
        key.algorithm.name = 'AES-GCM';
        expect('extractable stays', key.extractable === false);
        expect('algorithm stays', key.algorithm.name === 'HMAC');

        let constructed = 'no';
        try { new CryptoKey('secret', true, { name: 'HMAC' }, ['sign'], secret); constructed = 'yes'; }
        catch (e) { constructed = e instanceof TypeError ? 'no' : 'other: ' + e; }
        expect('constructor ' + constructed, constructed === 'no');

        // And the key still signs
        const signature = await crypto.subtle.sign('HMAC', key, new Uint8Array([42]));
        expect('signs', signature.byteLength === 32);

        return failures.length === 0 ? 'OK' : 'FAIL: ' + failures.join(', ');
    "#;

    assert_eq!(run_fetch(body).await, "OK");
}

/// Every op checks its key: a key imported for one use cannot do another, a
/// key of one algorithm cannot be used under another (InvalidAccessError),
/// and importKey takes only the usages its algorithm has (SyntaxError)
#[tokio::test(flavor = "current_thread")]
async fn test_key_usages_and_algorithm_are_enforced() {
    let body = rsa_test_keys()
        + r#"
        const data = new Uint8Array([1, 2, 3]);
        const raw = new Uint8Array(16).fill(9);

        const verifyOnly = await crypto.subtle.importKey(
            'raw', raw, { name: 'HMAC', hash: 'SHA-256' }, false, ['verify']
        );
        const rsaPrivate = await crypto.subtle.importKey(
            'pkcs8', rsaPkcs8, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign']
        );
        const encryptOnly = await crypto.subtle.importKey(
            'raw', raw, { name: 'AES-GCM' }, false, ['encrypt']
        );
        const deriveKeyOnly = await crypto.subtle.importKey(
            'raw', raw, { name: 'PBKDF2' }, false, ['deriveKey']
        );
        const pair = await crypto.subtle.generateKey(
            { name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign', 'verify']
        );

        const iv = new Uint8Array(12);
        const sealed = await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, encryptOnly, data);

        const refused = async (name, expected, op) => {
            try {
                await op();
                return name + ' went through';
            } catch (e) {
                return e instanceof DOMException && e.name === expected ? null : name + ': ' + e;
            }
        };

        const failures = (await Promise.all([
            refused('verify-only HMAC signs', 'InvalidAccessError',
                () => crypto.subtle.sign('HMAC', verifyOnly, data)),
            refused('RSA key under HMAC', 'InvalidAccessError',
                () => crypto.subtle.sign('HMAC', rsaPrivate, data)),
            refused('HMAC key under RSA', 'InvalidAccessError',
                () => crypto.subtle.sign('RSASSA-PKCS1-v1_5', verifyOnly, data)),
            refused('HMAC key under ECDSA', 'InvalidAccessError',
                () => crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, verifyOnly, data)),
            refused('public ECDSA key signs', 'InvalidAccessError',
                () => crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, pair.publicKey, data)),
            refused('encrypt-only AES decrypts', 'InvalidAccessError',
                () => crypto.subtle.decrypt({ name: 'AES-GCM', iv }, encryptOnly, sealed)),
            refused('deriveKey-only PBKDF2 derives bits', 'InvalidAccessError',
                () => crypto.subtle.deriveBits(
                    { name: 'PBKDF2', salt: iv, iterations: 1, hash: 'SHA-256' }, deriveKeyOnly, 256
                )),
            refused('non-extractable key exports', 'InvalidAccessError',
                () => crypto.subtle.exportKey('raw', encryptOnly)),
            refused('HMAC imported to encrypt', 'SyntaxError',
                () => crypto.subtle.importKey('raw', raw, { name: 'HMAC', hash: 'SHA-256' }, false, ['encrypt'])),
            refused('ECDSA private imported to verify', 'SyntaxError',
                () => crypto.subtle.importKey('pkcs8', raw, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify'])),
            refused('RSA public imported to sign', 'SyntaxError',
                () => crypto.subtle.importKey('spki', rsaSpki, { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign'])),
        ])).filter((failure) => failure !== null);

        // The allowed uses still work
        const signature = await crypto.subtle.sign('HMAC', await crypto.subtle.importKey(
            'raw', raw, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']
        ), data);
        if (!(await crypto.subtle.verify('HMAC', verifyOnly, signature, data))) {
            failures.push('verify-only HMAC does not verify');
        }

        return failures.length === 0 ? 'OK' : 'FAIL: ' + failures.join('; ');
    "#;

    assert_eq!(run_fetch(&body).await, "OK");
}
