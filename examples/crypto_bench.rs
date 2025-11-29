// Quick crypto benchmark
use openworkers_core::{HttpBody, HttpMethod, HttpRequest, Script, Task};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

#[tokio::main]
async fn main() {
    println!("=== Crypto Benchmark ===\n");

    // Benchmark SHA-256
    benchmark_sha256().await;

    // Benchmark HMAC
    benchmark_hmac().await;

    // Benchmark ECDSA
    benchmark_ecdsa().await;

    // Benchmark RSA
    benchmark_rsa().await;

    println!();
}

async fn benchmark_hmac() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const iterations = 1000;
            const keyData = new TextEncoder().encode('secret-key-for-hmac-benchmark');
            const key = await crypto.subtle.importKey(
                'raw', keyData, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']
            );
            
            const data = new TextEncoder().encode('hello world benchmark data');
            
            const start = Date.now();
            for (let i = 0; i < iterations; i++) {
                await crypto.subtle.sign('HMAC', key, data);
            }
            const elapsed = Date.now() - start;
            
            const opsPerSec = Math.round(iterations / (elapsed / 1000));
            event.respondWith(new Response(`${opsPerSec}`));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();
    let body = response.body.collect().await.unwrap();
    let ops_per_sec: u64 = std::str::from_utf8(&body).unwrap().parse().unwrap();

    println!("HMAC-SHA256:  {:>8} ops/sec", ops_per_sec);
}

async fn benchmark_ecdsa() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const iterations = 100;
            
            // Generate key pair once
            const keyPair = await crypto.subtle.generateKey(
                { name: 'ECDSA', namedCurve: 'P-256' },
                true,
                ['sign', 'verify']
            );
            
            const data = new TextEncoder().encode('hello world benchmark data');
            
            // Benchmark signing
            const startSign = Date.now();
            let signature;
            for (let i = 0; i < iterations; i++) {
                signature = await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, keyPair.privateKey, data);
            }
            const signElapsed = Date.now() - startSign;
            const signOps = Math.round(iterations / (signElapsed / 1000));
            
            // Benchmark verification
            const startVerify = Date.now();
            for (let i = 0; i < iterations; i++) {
                await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, keyPair.publicKey, signature, data);
            }
            const verifyElapsed = Date.now() - startVerify;
            const verifyOps = Math.round(iterations / (verifyElapsed / 1000));
            
            event.respondWith(new Response(`${signOps},${verifyOps}`));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();
    let body = response.body.collect().await.unwrap();
    let parts: Vec<&str> = std::str::from_utf8(&body).unwrap().split(',').collect();
    let sign_ops: u64 = parts[0].parse().unwrap();
    let verify_ops: u64 = parts[1].parse().unwrap();

    println!("ECDSA P-256 sign:   {:>5} ops/sec", sign_ops);
    println!("ECDSA P-256 verify: {:>5} ops/sec", verify_ops);
}

async fn benchmark_sha256() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            const iterations = 10000;
            const data = new TextEncoder().encode('hello world benchmark data for sha256 hashing');
            
            const start = Date.now();
            for (let i = 0; i < iterations; i++) {
                await crypto.subtle.digest('SHA-256', data);
            }
            const elapsed = Date.now() - start;
            
            const opsPerSec = Math.round(iterations / (elapsed / 1000));
            event.respondWith(new Response(`${opsPerSec}`));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();
    let body = response.body.collect().await.unwrap();
    let ops_per_sec: u64 = std::str::from_utf8(&body).unwrap().parse().unwrap();

    println!("SHA-256:      {:>8} ops/sec", ops_per_sec);
}

async fn benchmark_rsa() {
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Base64 decoder
            function base64ToBytes(base64) {
                const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
                let bufferLength = base64.length * 0.75;
                if (base64[base64.length - 1] === '=') bufferLength--;
                if (base64[base64.length - 2] === '=') bufferLength--;
                const bytes = new Uint8Array(bufferLength);
                let p = 0;
                for (let i = 0; i < base64.length; i += 4) {
                    const e1 = chars.indexOf(base64[i]);
                    const e2 = chars.indexOf(base64[i + 1]);
                    const e3 = chars.indexOf(base64[i + 2]);
                    const e4 = chars.indexOf(base64[i + 3]);
                    bytes[p++] = (e1 << 2) | (e2 >> 4);
                    if (e3 !== -1 && base64[i + 2] !== '=') bytes[p++] = ((e2 & 15) << 4) | (e3 >> 2);
                    if (e4 !== -1 && base64[i + 3] !== '=') bytes[p++] = ((e3 & 3) << 6) | e4;
                }
                return bytes;
            }

            const iterations = 100;

            // RSA 2048-bit test keys
            const privateKeyBase64 = 'MIIEpAIBAAKCAQEA5EmDGTHoMj6bosn6lbZMJkZNnDlfoon7eMBrVQYSkQDLZCnJHDAxAD8ODlIWlRHDD9NWqyEBdTGqlUDTrjKvLBzktSMWeIG0TrXVQ0Yw3Ibu8EvSn8tGVEq/Epa05uNh7JGVjxmIRVyGn6ic9b1S85JzfcSJgUoxSvW0KmTOh/TaaHdAkGS/4wpdfjSexogWapyKNms17jHehmtkUq0Vhh4YYr8t72bb+FJtHqwsEYbC3jXXEQ+u6zCmc9fDuAvbv5kvjglBZu0aEGap5fmbqSWexWqJcdvln7TMQ2A6b1fmZ1t76+WtKH7WwGf4SGkJ2PLFxCZaJ8oE0Ci+Rm/amwIDAQABAoIBABBogj5A2o4l9tzMBLFXEYEcw35Ll2ag4UzME8rgLVxzwKq54CUhB5yba6C24L2lMa6FA7E4JZktUTP6HVzjcrjKeNvWIkrWE8YmhqYXuPJY1nq6EHEA1NTBLJui7my8AjFVQ3kuHh/SJzD5lxKIoZo1OAzdn/6FfSaEo4b6iOe3nGj2q00WUf4t5OjQyWkgZHb3D+QFimnrw0q0ct/N28MxiHohJs+8NgDhDnjthF1fwi5mpso9mm+ysw2/ss5W1y6mczWcEwXvTh0svD6BkdGHfdpbkaXguHFCyFk1WG80MYiq61yZOwPMj2GFh/o3dPdIo5x3ScKBzDen2zuevLkCgYEA+N30zLXWM67jpqxFTokbEiImmUiLHGPx4CtCu1Cf3MNWz7W2p8/eCi3vtL8dCeD687yuHDPcft6KH88jXHriyTaK3zez8BTetmGNM+3YM1QmFynD1qYuqaDyobZBFwpxka902SQFcWAIDsimaJeNsVd2Kxr2lb3AYZyu66vQAtkCgYEA6tSNeHWS+PqF0OUivYI2Vsn+moplxNfEElSK6ifrK39YaEv9hZzwLIR3Iq0cxbKQWBNvssWzaLd0Z5ZKBEWFmLDNph7Giq7V1spUc6V6tWrbGL+92Yw0+ZWjx83InFAT+B6Cjgvptfrd0AipphhrAFC0c3iiIKbSPv0EOPec+JMCgYEAx0rneO+9A1JwV87pCYVeOl1Cz8l6LVgUIFJEdECSZHXBlUCNb0FVLI2wweux03FpRbq5KziUwLxxnBuC09JMvpmBCFRRMldkKmVgcE9trV0by7zUaZZXE9whsUKESXFBlUsOpbzk5u/iRASGzodfHr9NkCNdiHiWERUqNuw1/bECgYASDVDqx68KsMeErXikNNRUi6ak3qrAHQ4XkqQzJ+puJ5X2PpE4qj3UTkKSSdiCYh2yh5v4lDYcgK3UILuD5IxGlqDYelks5A/QOTGQylHKjHJXTrYbeSnBXf1/KJSZX5aJZl8G6GeI88YFbgUMnafsGEgm8EkWVXyoFu8yKebJPQKBgQDsltWFU9zmXXA6mMaKi5A7J7Va3s74pEqlyQk+Xb0iRcZLKCIdB3MepaIPXi0QPjRwXY6vIVIV2AvTToup1c4pZKH98YM/HFZfLgQsNw0YGW39VzyR4i39j44AvAmLB0y8x8GKD7NUk8cVJGLL+R5qyRe2LGOJtHb4UoBsmTCIWg==';
            const publicKeyBase64 = 'MIIBCgKCAQEA5EmDGTHoMj6bosn6lbZMJkZNnDlfoon7eMBrVQYSkQDLZCnJHDAxAD8ODlIWlRHDD9NWqyEBdTGqlUDTrjKvLBzktSMWeIG0TrXVQ0Yw3Ibu8EvSn8tGVEq/Epa05uNh7JGVjxmIRVyGn6ic9b1S85JzfcSJgUoxSvW0KmTOh/TaaHdAkGS/4wpdfjSexogWapyKNms17jHehmtkUq0Vhh4YYr8t72bb+FJtHqwsEYbC3jXXEQ+u6zCmc9fDuAvbv5kvjglBZu0aEGap5fmbqSWexWqJcdvln7TMQ2A6b1fmZ1t76+WtKH7WwGf4SGkJ2PLFxCZaJ8oE0Ci+Rm/amwIDAQAB';

            const privateKey = await crypto.subtle.importKey(
                'pkcs8', base64ToBytes(privateKeyBase64),
                { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['sign']
            );
            const publicKey = await crypto.subtle.importKey(
                'spki', base64ToBytes(publicKeyBase64),
                { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' }, false, ['verify']
            );

            const data = new TextEncoder().encode('hello world benchmark data');

            // Benchmark signing
            const startSign = Date.now();
            let signature;
            for (let i = 0; i < iterations; i++) {
                signature = await crypto.subtle.sign('RSASSA-PKCS1-v1_5', privateKey, data);
            }
            const signElapsed = Date.now() - startSign;
            const signOps = Math.round(iterations / (signElapsed / 1000));

            // Benchmark verification
            const startVerify = Date.now();
            for (let i = 0; i < iterations; i++) {
                await crypto.subtle.verify('RSASSA-PKCS1-v1_5', publicKey, signature, data);
            }
            const verifyElapsed = Date.now() - startVerify;
            const verifyOps = Math.round(iterations / (verifyElapsed / 1000));

            event.respondWith(new Response(`${signOps},${verifyOps}`));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, None).await.unwrap();

    let req = HttpRequest {
        method: HttpMethod::Get,
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: HttpBody::None,
    };

    let (task, rx) = Task::fetch(req);
    worker.exec(task).await.unwrap();
    let response = rx.await.unwrap();
    let body = response.body.collect().await.unwrap();
    let parts: Vec<&str> = std::str::from_utf8(&body).unwrap().split(',').collect();
    let sign_ops: u64 = parts[0].parse().unwrap();
    let verify_ops: u64 = parts[1].parse().unwrap();

    println!("RSA-2048 sign:      {:>5} ops/sec", sign_ops);
    println!("RSA-2048 verify:    {:>5} ops/sec", verify_ops);
}
