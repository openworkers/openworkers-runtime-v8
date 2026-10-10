//! Compression Streams: the codecs answer a bounded piece at a time, so a chunk
//! that inflates a thousandfold never lands in host memory at once.

mod common;

use common::run_in_local;
use openworkers_core::{Event, HttpMethod, HttpRequest, RequestBody, Script};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;
use std::io::Write;

/// Runs `body` as an async fetch handler and answers what it responded with.
async fn answer_of(body: &str) -> String {
    let code = format!(
        r#"
        globalThis.hex = (text) => {{
            const bytes = new Uint8Array(text.length / 2);
            for (let i = 0; i < bytes.length; i++) {{
                bytes[i] = parseInt(text.substr(i * 2, 2), 16);
            }}
            return bytes;
        }};

        globalThis.unhex = (bytes) =>
            Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');

        // Feeds `chunks` through `stream` and answers the pieces that come out.
        globalThis.through = async (stream, chunks) => {{
            const writer = stream.writable.getWriter();
            const reader = stream.readable.getReader();
            const pieces = [];

            const writing = (async () => {{
                for (const chunk of chunks) {{
                    await writer.write(chunk);
                }}
                await writer.close();
            }})();
            // A failed write is reported below, by the reader or the final await.
            writing.catch(() => {{}});

            for (;;) {{
                const {{ done, value }} = await reader.read();
                if (done) break;
                pieces.push(value);
            }}

            await writing;
            return pieces;
        }};

        globalThis.concat = (pieces) => {{
            const total = pieces.reduce((n, piece) => n + piece.byteLength, 0);
            const out = new Uint8Array(total);
            let at = 0;
            for (const piece of pieces) {{
                out.set(piece, at);
                at += piece.byteLength;
            }}
            return out;
        }};

        addEventListener('fetch', async (event) => {{
            let out;
            try {{
                out = await (async () => {{ {body} }})();
            }} catch (e) {{
                out = 'threw ' + e.name + ': ' + e.message;
            }}
            event.respondWith(new Response(String(out)));
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

/// One megabyte that deflates to a few kilobytes.
const PATTERN: &str = "0123456789abcdef";
const REPEATS: usize = 65536;

fn compressible() -> Vec<u8> {
    PATTERN.repeat(REPEATS).into_bytes()
}

fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

/// A single compressed chunk that inflates to 1 MiB comes out in pieces no
/// larger than the bound, and whole.
#[tokio::test(flavor = "current_thread")]
async fn one_chunk_inflates_in_bounded_pieces() {
    let compressed = hex(&deflate_raw(&compressible()));
    let body = format!(
        r#"
        const pieces = await through(new DecompressionStream('deflate-raw'), [hex('{compressed}')]);
        const largest = Math.max(...pieces.map((piece) => piece.byteLength));
        const text = new TextDecoder().decode(concat(pieces));
        const whole = text === '{PATTERN}'.repeat({REPEATS});
        return 'pieces=' + pieces.length + ' largest=' + largest + ' whole=' + whole;
        "#
    );

    let out = answer_of(&body).await;
    let bound = openworkers_runtime_v8::runtime::bindings::OUTPUT_BOUND;

    let number = |key: &str| -> usize {
        out.split(key)
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("got: {out}"))
    };

    assert!(number("largest=") <= bound, "got: {out}");
    assert!(number("pieces=") > 1, "got: {out}");
    assert!(out.ends_with("whole=true"), "got: {out}");
}

/// Each format round-trips through its own pair of streams, with the input
/// split into chunks of odd sizes.
#[tokio::test(flavor = "current_thread")]
async fn each_format_round_trips() {
    let body = r#"
        const results = [];
        for (const format of ['deflate', 'deflate-raw', 'gzip']) {
            const text = 'the quick brown fox jumps over the lazy dog, '.repeat(5000);
            const bytes = new TextEncoder().encode(text);
            const chunks = [bytes.subarray(0, 7), bytes.subarray(7, 70001), bytes.subarray(70001)];

            const compressed = concat(await through(new CompressionStream(format), chunks));
            const smaller = compressed.byteLength < bytes.byteLength / 10;

            const back = [compressed.subarray(0, 3), compressed.subarray(3, 5), compressed.subarray(5)];
            const inflated = concat(await through(new DecompressionStream(format), back));
            const same = new TextDecoder().decode(inflated) === text;

            results.push(format + ':' + smaller + ':' + same);
        }
        return results.join(' ');
    "#;

    assert_eq!(
        answer_of(body).await,
        "deflate:true:true deflate-raw:true:true gzip:true:true"
    );
}

/// What the guest compresses as gzip, flate2 reads as gzip, and the other way
/// round: through a header with a file name and a header checksum.
#[tokio::test(flavor = "current_thread")]
async fn gzip_interoperates_with_flate2() {
    let data = compressible();
    let mut encoder = flate2::GzBuilder::new()
        .filename("pattern.txt")
        .comment("sixteen hex digits, many times")
        .write(Vec::new(), flate2::Compression::default());
    encoder.write_all(&data).unwrap();
    let mut gz = encoder.finish().unwrap();

    // flate2 does not write the header checksum; set the flag and append it.
    gz[3] |= 0x02;
    let header_end = 10 + "pattern.txt".len() + 1 + "sixteen hex digits, many times".len() + 1;
    let mut crc = flate2::Crc::new();
    crc.update(&gz[..header_end]);
    let sum = (crc.sum() as u16).to_le_bytes();
    gz.splice(header_end..header_end, sum);

    let gz_hex = hex(&gz);
    let body = format!(
        r#"
        const inflated = concat(await through(new DecompressionStream('gzip'), [hex('{gz_hex}')]));
        const whole = new TextDecoder().decode(inflated) === '{PATTERN}'.repeat({REPEATS});

        const deflated = concat(await through(new CompressionStream('gzip'), [new TextEncoder().encode('{PATTERN}'.repeat({REPEATS}))]));
        return 'whole=' + whole + ' gz=' + unhex(deflated);
        "#
    );

    let out = answer_of(&body).await;
    let (whole, gz) = out
        .split_once(" gz=")
        .unwrap_or_else(|| panic!("got: {out}"));
    assert_eq!(whole, "whole=true");

    let gz = unhex(gz);
    let mut decoder = flate2::read::GzDecoder::new(gz.as_slice());
    let mut back = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut back).unwrap();
    assert_eq!(back, data);
}

/// Bytes past the end of the compressed stream are an error, not ignored.
#[tokio::test(flavor = "current_thread")]
async fn trailing_junk_rejects() {
    let mut data = deflate_raw(b"hello");
    data.extend_from_slice(b"junk");
    let data = hex(&data);

    let body = format!(
        r#"
        await through(new DecompressionStream('deflate-raw'), [hex('{data}')]);
        return 'accepted';
        "#
    );

    let out = answer_of(&body).await;
    assert!(out.starts_with("threw TypeError:"), "got: {out}");
}

/// A gzip stream whose trailer disagrees with the data is corrupt.
#[tokio::test(flavor = "current_thread")]
async fn corrupt_gzip_trailer_rejects() {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"hello").unwrap();
    let mut gz = encoder.finish().unwrap();
    let last = gz.len() - 1;
    gz[last] ^= 0xff;
    let data = hex(&gz);

    let body = format!(
        r#"
        await through(new DecompressionStream('gzip'), [hex('{data}')]);
        return 'accepted';
        "#
    );

    let out = answer_of(&body).await;
    assert!(out.starts_with("threw TypeError:"), "got: {out}");
}

/// A context holds a bounded number of codecs: the one past the limit is
/// refused with a plain Error, and releasing one makes room again.
#[tokio::test(flavor = "current_thread")]
async fn open_codecs_are_capped() {
    let limit = openworkers_runtime_v8::runtime::bindings::MAX_CODECS;
    let body = format!(
        r#"
        const streams = [];
        for (let i = 0; i < {limit}; i++) {{
            streams.push(new CompressionStream('gzip'));
        }}

        let refused;
        try {{
            new CompressionStream('gzip');
            refused = 'none';
        }} catch (e) {{
            refused = e.name + ': ' + e.message;
        }}

        // Ending one stream releases its codec.
        const pieces = await through(streams[0], [new TextEncoder().encode('hello')]);
        const again = concat(await through(new CompressionStream('gzip'), [new TextEncoder().encode('again')]));

        return refused + ' / first=' + (pieces.length > 0) + ' again=' + (again.byteLength > 0);
        "#
    );

    let out = answer_of(&body).await;
    assert_eq!(
        out,
        format!(
            "Error: too many compression streams are open at once (the limit is {limit}) / first=true again=true"
        )
    );
}

/// A codec the previous request left open is released when the context is
/// reset for the next one, so the guest can no longer reach it.
#[tokio::test(flavor = "current_thread")]
async fn a_reset_releases_every_codec() {
    run_in_local(|| async {
        let code = r#"
            globalThis.outcome = 'pending';
            globalThis.probe = () => {
                try {
                    __ow.compressionPush(globalThis.id, new Uint8Array(1));
                    globalThis.outcome = 'open';
                } catch (e) {
                    globalThis.outcome = e.name + ': ' + e.message;
                }
            };
            addEventListener('fetch', (event) => {
                globalThis.id ??= __ow.compressionStart('deflate-raw', false);
                event.respondWith(new Response('OK'));
            });
        "#;

        let mut worker = Worker::new(Script::new(code), None).await.unwrap();

        // The codec a request opens is still there after it, and gone after
        // the reset that starts the next
        let (task, _rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".into(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });
        worker.exec(task).await.unwrap();
        worker.evaluate("probe();").unwrap();

        let (task, _rx) = Event::fetch(HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".into(),
            headers: HashMap::new(),
            body: RequestBody::None,
        });
        worker.exec(task).await.unwrap();
        worker
            .evaluate("globalThis.before = outcome; probe();")
            .unwrap();

        let outcome = worker.with_isolate(|isolate, context| {
            use std::pin::pin;

            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);
            let key = v8::String::new(scope, "outcome").unwrap();
            let value = global.get(scope, key.into()).unwrap();
            value.to_rust_string_lossy(scope)
        });

        let before = worker.with_isolate(|isolate, context| {
            use std::pin::pin;

            let scope = pin!(v8::HandleScope::new(isolate));
            let mut scope = scope.init();
            let context = v8::Local::new(&scope, context);
            let scope = &mut v8::ContextScope::new(&mut scope, context);
            let global = context.global(scope);
            let key = v8::String::new(scope, "before").unwrap();
            let value = global.get(scope, key.into()).unwrap();
            value.to_rust_string_lossy(scope)
        });

        assert_eq!(before, "open");
        assert_eq!(
            outcome,
            "TypeError: the compression stream is already closed"
        );
    })
    .await;
}

/// Input cut before the end of the deflate stream is an error at the end,
/// for the formats without a trailer too.
#[tokio::test(flavor = "current_thread")]
async fn truncated_input_rejects() {
    let compressed = deflate_raw(&compressible());
    let cut = hex(&compressed[..compressed.len() / 2]);

    for format in ["deflate-raw", "deflate"] {
        let data = match format {
            "deflate" => {
                let mut encoder =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(b"hello hello hello hello").unwrap();
                let zlib = encoder.finish().unwrap();
                hex(&zlib[..zlib.len() - 6])
            }
            _ => cut.clone(),
        };

        let body = format!(
            r#"
            const pieces = await through(new DecompressionStream('{format}'), [hex('{data}')]);
            return 'accepted ' + pieces.length + ' pieces';
            "#
        );

        let out = answer_of(&body).await;
        assert!(out.starts_with("threw TypeError:"), "{format} got: {out}");
    }
}
