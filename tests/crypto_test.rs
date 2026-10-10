mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

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

/// Test RSA PKCS#1 v1.5 sign and verify
#[tokio::test(flavor = "current_thread")]
async fn test_rsa_sign_verify() {
    run_in_local(|| async {
        // Test RSA key pair (2048-bit, generated with openssl)
        // Private key: DER format
        // Public key: RSAPublicKey format (not SPKI)
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Base64 decoder (atob not available in this runtime)
                function base64ToBytes(base64) {
                    const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
                    let bufferLength = base64.length * 0.75;
                    if (base64[base64.length - 1] === '=') bufferLength--;
                    if (base64[base64.length - 2] === '=') bufferLength--;

                    const bytes = new Uint8Array(bufferLength);
                    let p = 0;

                    for (let i = 0; i < base64.length; i += 4) {
                        const encoded1 = chars.indexOf(base64[i]);
                        const encoded2 = chars.indexOf(base64[i + 1]);
                        const encoded3 = chars.indexOf(base64[i + 2]);
                        const encoded4 = chars.indexOf(base64[i + 3]);

                        bytes[p++] = (encoded1 << 2) | (encoded2 >> 4);
                        if (encoded3 !== -1 && base64[i + 2] !== '=') {
                            bytes[p++] = ((encoded2 & 15) << 4) | (encoded3 >> 2);
                        }
                        if (encoded4 !== -1 && base64[i + 3] !== '=') {
                            bytes[p++] = ((encoded3 & 3) << 6) | encoded4;
                        }
                    }
                    return bytes;
                }

                // Base64 encoded RSA keys (2048-bit)
                const privateKeyBase64 = 'MIIEpAIBAAKCAQEA5EmDGTHoMj6bosn6lbZMJkZNnDlfoon7eMBrVQYSkQDLZCnJHDAxAD8ODlIWlRHDD9NWqyEBdTGqlUDTrjKvLBzktSMWeIG0TrXVQ0Yw3Ibu8EvSn8tGVEq/Epa05uNh7JGVjxmIRVyGn6ic9b1S85JzfcSJgUoxSvW0KmTOh/TaaHdAkGS/4wpdfjSexogWapyKNms17jHehmtkUq0Vhh4YYr8t72bb+FJtHqwsEYbC3jXXEQ+u6zCmc9fDuAvbv5kvjglBZu0aEGap5fmbqSWexWqJcdvln7TMQ2A6b1fmZ1t76+WtKH7WwGf4SGkJ2PLFxCZaJ8oE0Ci+Rm/amwIDAQABAoIBABBogj5A2o4l9tzMBLFXEYEcw35Ll2ag4UzME8rgLVxzwKq54CUhB5yba6C24L2lMa6FA7E4JZktUTP6HVzjcrjKeNvWIkrWE8YmhqYXuPJY1nq6EHEA1NTBLJui7my8AjFVQ3kuHh/SJzD5lxKIoZo1OAzdn/6FfSaEo4b6iOe3nGj2q00WUf4t5OjQyWkgZHb3D+QFimnrw0q0ct/N28MxiHohJs+8NgDhDnjthF1fwi5mpso9mm+ysw2/ss5W1y6mczWcEwXvTh0svD6BkdGHfdpbkaXguHFCyFk1WG80MYiq61yZOwPMj2GFh/o3dPdIo5x3ScKBzDen2zuevLkCgYEA+N30zLXWM67jpqxFTokbEiImmUiLHGPx4CtCu1Cf3MNWz7W2p8/eCi3vtL8dCeD687yuHDPcft6KH88jXHriyTaK3zez8BTetmGNM+3YM1QmFynD1qYuqaDyobZBFwpxka902SQFcWAIDsimaJeNsVd2Kxr2lb3AYZyu66vQAtkCgYEA6tSNeHWS+PqF0OUivYI2Vsn+moplxNfEElSK6ifrK39YaEv9hZzwLIR3Iq0cxbKQWBNvssWzaLd0Z5ZKBEWFmLDNph7Giq7V1spUc6V6tWrbGL+92Yw0+ZWjx83InFAT+B6Cjgvptfrd0AipphhrAFC0c3iiIKbSPv0EOPec+JMCgYEAx0rneO+9A1JwV87pCYVeOl1Cz8l6LVgUIFJEdECSZHXBlUCNb0FVLI2wweux03FpRbq5KziUwLxxnBuC09JMvpmBCFRRMldkKmVgcE9trV0by7zUaZZXE9whsUKESXFBlUsOpbzk5u/iRASGzodfHr9NkCNdiHiWERUqNuw1/bECgYASDVDqx68KsMeErXikNNRUi6ak3qrAHQ4XkqQzJ+puJ5X2PpE4qj3UTkKSSdiCYh2yh5v4lDYcgK3UILuD5IxGlqDYelks5A/QOTGQylHKjHJXTrYbeSnBXf1/KJSZX5aJZl8G6GeI88YFbgUMnafsGEgm8EkWVXyoFu8yKebJPQKBgQDsltWFU9zmXXA6mMaKi5A7J7Va3s74pEqlyQk+Xb0iRcZLKCIdB3MepaIPXi0QPjRwXY6vIVIV2AvTToup1c4pZKH98YM/HFZfLgQsNw0YGW39VzyR4i39j44AvAmLB0y8x8GKD7NUk8cVJGLL+R5qyRe2LGOJtHb4UoBsmTCIWg==';
                const publicKeyBase64 = 'MIIBCgKCAQEA5EmDGTHoMj6bosn6lbZMJkZNnDlfoon7eMBrVQYSkQDLZCnJHDAxAD8ODlIWlRHDD9NWqyEBdTGqlUDTrjKvLBzktSMWeIG0TrXVQ0Yw3Ibu8EvSn8tGVEq/Epa05uNh7JGVjxmIRVyGn6ic9b1S85JzfcSJgUoxSvW0KmTOh/TaaHdAkGS/4wpdfjSexogWapyKNms17jHehmtkUq0Vhh4YYr8t72bb+FJtHqwsEYbC3jXXEQ+u6zCmc9fDuAvbv5kvjglBZu0aEGap5fmbqSWexWqJcdvln7TMQ2A6b1fmZ1t76+WtKH7WwGf4SGkJ2PLFxCZaJ8oE0Ci+Rm/amwIDAQAB';

                const privateKeyData = base64ToBytes(privateKeyBase64);
                const publicKeyData = base64ToBytes(publicKeyBase64);

                // Import keys
                const privateKey = await crypto.subtle.importKey(
                    'pkcs8',
                    privateKeyData,
                    { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' },
                    false,
                    ['sign']
                );

                const publicKey = await crypto.subtle.importKey(
                    'spki',
                    publicKeyData,
                    { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' },
                    false,
                    ['verify']
                );

                // Sign data
                const data = new TextEncoder().encode('hello world');
                const signature = await crypto.subtle.sign(
                    'RSASSA-PKCS1-v1_5',
                    privateKey,
                    data
                );

                // Verify signature
                const isValid = await crypto.subtle.verify(
                    'RSASSA-PKCS1-v1_5',
                    publicKey,
                    signature,
                    data
                );

                // Verify with wrong data fails
                const wrongData = new TextEncoder().encode('wrong data');
                const isInvalid = await crypto.subtle.verify(
                    'RSASSA-PKCS1-v1_5',
                    publicKey,
                    signature,
                    wrongData
                );

                // RSA-2048 signature is 256 bytes
                const sigLen = new Uint8Array(signature).length;

                const result = isValid && !isInvalid && sigLen === 256 ? 'OK' : `FAIL: isValid=${isValid}, isInvalid=${isInvalid}, sigLen=${sigLen}`;
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

/// The output of a native op is Rust memory the isolate caps do not see
#[tokio::test(flavor = "current_thread")]
async fn test_pbkdf2_refuses_a_huge_output() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith((async () => {
                    const key = await crypto.subtle.importKey('raw', new Uint8Array([1]), { name: 'PBKDF2' }, false, ['deriveBits']);
                    const algorithm = { name: 'PBKDF2', salt: new Uint8Array([1]), iterations: 1, hash: 'SHA-256' };

                    try {
                        await crypto.subtle.deriveBits(algorithm, key, 8 * 1024 * 1024 * 400);
                        return new Response('FAIL: derived');
                    } catch (e) {
                        const small = await crypto.subtle.deriveBits(algorithm, key, 256);
                        return new Response(small.byteLength === 32 ? 'OK' : 'FAIL: small');
                    }
                })());
            });
        "#;

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
        let body = &response.body.collect().await.unwrap().unwrap();

        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}

/// Keys as OpenSSL and WebCrypto write them: PKCS#8 private, SPKI public
#[tokio::test(flavor = "current_thread")]
async fn test_rsa_spki_and_pkcs8_keys() {
    run_in_local(|| async {
        let code = r#"
            addEventListener('fetch', (event) => {
                event.respondWith((async () => {
                    const bytes = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
                    const algorithm = { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' };
                    const priv = await crypto.subtle.importKey('pkcs8', bytes('MIIEpQIBAAKCAQEAqvpKGORObkDn7VLZTlrad4nbV4fWwXkwMd1N6KM9S6CVT/RD6OgVm1GnepjwFswv61KSqFxZ9vH4qy7kq4M9OCUL8CgYKMuWeMAwHuauBet9Dvn2ovstLD2WCwCOxDI5KCBdcVPTj4E0KN5WGHwOxtxB+A7WBo69LFy+u3lm7ckyDyCCNE59EH9oySDxduehl9uNo5Cur8uowos/j0A1DQsuiNzr6e3WdLO2Yd9OhsBYLYHlMgZYvYiWmh6QJfzja3xV0HcVuE9dHxcM8dE8U3jpZPvM7U+8Dwv2yB2+VJds2VW16NskO+rhdunXwYat1+L1WtxXBV759142r3bI1QIDAQABAoIBAAzVl+1Raf/FuIMslmpW0JJrkz75T+obBj6f/aKqakX8imjDjbt0fHa5xOgjhdY4QpqYCrE/qXMri76R2RF02woVYdWHtPSO/78VsicHquV/3VXb9qMaVrQ89T/jLVRV7stvzoPcxoM9sCQnOHBDE7riusL7nh5E5bdoSNr6zHqp4byV/4H+VUte6pda0/Xt7wg9bmajqnky6ap0SSUGHYFDdC6jjgT6rbigemS9c+W0vw7zGGgr+Rsxa/V2/R4oAhh+muGh4SrFUBBpLO/09h0iyoHZaNtnj9KR0WPCSyowPjjw+pULbcvr5gwpav2wtWKhGCDXBKlT0rjwQYYGLMECgYEA2ZQ8DDLePLWOwF5AhvYy0mkA9iMs4E53mS1+jOevAjgXZu1Sym0BNkAN6ow9E0br64/paxwMSJW6VnsCxq8/O+0kSQXuCVpv2uWD/zrGOlQW8hZOMiGmt5LFsRnUYeI6DF0VixDe+tDIn3Vw8bTjWp/SP8zDBalbWRAX2AUmdDUCgYEAyStrIUMyT7jHf3nDfhuM305Ctk3iOeW37a+/XVKMxhrrU471ye6/KQyYG/YdAevipJJzlu1KG9r4a1GdLLfpYQIX3icmKTAUsy5Lt7c0UngQNQr7NVTctIxsyEQEDPxmW6mKAf4ndyu3MRB2AKk8QqCcPDTSBqQjrmTD4w+YViECgYEAqd8Y7rE8X4ukkz5DBNvtG+fNT15xKAM7TwV8+0fblFD0vHBnphFq0884zjmFaaqCgRyPsgdo87aqj+Bkb3jdVs0z+is+CGFqWS2+W6OopluGuqV9kZhCUKqv3DB9Z5q3lXWLX1LhtFMTf6OydZOzucpz3UnhrWbnIeb1pruGpU0CgYEApJVjNnl1hgfVIBQMvvXnUSMELYaW2Wt6CXpKBB3vknyfn2NM8ALmXr0xDV9T6CiG6sHu08IbaaLCr3q8LsPgqj8+K8C31ebCaL4tsIawxe/4wozTbZSaZRSmQ0pyTfWKAOA6StsWisc3P2sKQAw1gwVIDXHhixFrJ9jE8tXlekECgYEAniAeyGicGrs0etC0BCPNcEFBsa4937RhUlO3dXwSygGYvHwOugRhfNEL2iHc+JYEcyTLByf6I2gRDJQbiWYtWRmpfvBJP8PxYBaXTCXUxHJvU6EmdIAZMIZ5nVEPu6mO1mKeRRCIdS6sHYcLghnZi1XZyFvx4dzlc3/E6keohHI='), algorithm, false, ['sign']);
                    const pub = await crypto.subtle.importKey('spki', bytes('MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqvpKGORObkDn7VLZTlrad4nbV4fWwXkwMd1N6KM9S6CVT/RD6OgVm1GnepjwFswv61KSqFxZ9vH4qy7kq4M9OCUL8CgYKMuWeMAwHuauBet9Dvn2ovstLD2WCwCOxDI5KCBdcVPTj4E0KN5WGHwOxtxB+A7WBo69LFy+u3lm7ckyDyCCNE59EH9oySDxduehl9uNo5Cur8uowos/j0A1DQsuiNzr6e3WdLO2Yd9OhsBYLYHlMgZYvYiWmh6QJfzja3xV0HcVuE9dHxcM8dE8U3jpZPvM7U+8Dwv2yB2+VJds2VW16NskO+rhdunXwYat1+L1WtxXBV759142r3bI1QIDAQAB'), algorithm, true, ['verify']);
                    const data = new TextEncoder().encode('hi');
                    const sig = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', priv, data);
                    const good = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', pub, sig, data);
                    const bad = await crypto.subtle.verify('RSASSA-PKCS1-v1_5', pub, sig, new TextEncoder().encode('no'));
                    return new Response(good && !bad ? 'OK' : 'FAIL ' + good + ' ' + bad);
                })());
            });
        "#;

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
        let body = &response.body.collect().await.unwrap().unwrap();

        assert_eq!(std::str::from_utf8(body).unwrap(), "OK");
    })
    .await;
}
