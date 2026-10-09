//! heap_max_mb bounds the JS heap of a worker, and Intl still works within
//! the default limit.

mod common;

use common::run_in_local;
use openworkers_core::{
    Event, HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, TerminationReason,
};
use openworkers_runtime_v8::Worker;
use std::collections::HashMap;

fn get(
    path: &str,
) -> (
    Event,
    tokio::sync::oneshot::Receiver<openworkers_core::HttpResponse>,
) {
    Event::fetch(HttpRequest {
        method: HttpMethod::Get,
        url: format!("http://localhost{path}"),
        headers: HashMap::new(),
        body: RequestBody::None,
    })
}

async fn text(rx: tokio::sync::oneshot::Receiver<openworkers_core::HttpResponse>) -> String {
    let bytes = rx.await.unwrap().body.collect().await.unwrap().unwrap();

    String::from_utf8_lossy(&bytes).into_owned()
}

/// Keeps 1 MiB JS arrays until the limit stops it, then tells on a second
/// request how many it kept.
#[tokio::test(flavor = "current_thread")]
async fn js_objects_stop_at_the_heap_limit() {
    let code = r#"
        globalThis.kept = 0;
        addEventListener('fetch', (e) => {
            if (new URL(e.request.url).pathname === '/kept') {
                e.respondWith(new Response(String(globalThis.kept)));
                return;
            }
            const arrays = [];
            for (;;) {
                arrays.push(new Array(131072).fill(1.5));
                globalThis.kept += 1;
            }
        });
    "#;

    run_in_local(|| async {
        let limits = RuntimeLimits {
            heap_max_mb: 64,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 10_000,
            ..Default::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();

        let (event, _rx) = get("/");
        assert_eq!(
            worker.exec(event).await,
            Err(TerminationReason::MemoryLimit)
        );

        let (event, rx) = get("/kept");
        worker.exec(event).await.unwrap();
        let kept: u64 = text(rx).await.parse().unwrap();

        assert!(
            (8..64).contains(&kept),
            "kept {kept} MiB of arrays under a 64 MiB heap"
        );
    })
    .await;
}

/// The default limit leaves Intl room for its formatters.
#[tokio::test(flavor = "current_thread")]
async fn intl_formatters_fit_in_the_default_heap() {
    let code = r#"
        addEventListener('fetch', (e) => {
            const locales = ['en-US', 'en-GB', 'fr-FR', 'de-DE', 'es-ES', 'it-IT', 'pt-BR', 'ru-RU',
                'ja-JP', 'zh-CN', 'zh-TW', 'ko-KR', 'ar-EG', 'he-IL', 'hi-IN', 'th-TH', 'tr-TR',
                'pl-PL', 'nl-NL', 'sv-SE', 'fi-FI', 'cs-CZ', 'el-GR', 'uk-UA', 'vi-VN', 'id-ID'];
            const styles = [
                { dateStyle: 'full', timeStyle: 'long' },
                { year: 'numeric', month: 'long', day: 'numeric', weekday: 'long' },
                { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false },
                { month: 'short', day: 'numeric', timeZone: 'Asia/Tokyo' },
                { era: 'long', year: 'numeric', calendar: 'islamic' },
                { dateStyle: 'medium', timeZone: 'America/New_York' },
            ];
            const date = new Date(Date.UTC(2026, 9, 9, 12, 30));
            let count = 0;

            for (const locale of locales) {
                for (const style of styles) {
                    new Intl.DateTimeFormat(locale, style).format(date);
                    count++;
                }
                new Intl.NumberFormat(locale, { style: 'currency', currency: 'EUR' }).format(1234.5);
                new Intl.RelativeTimeFormat(locale).format(-3, 'day');
                new Intl.PluralRules(locale).select(2);
                new Intl.Collator(locale).compare('a', 'b');
            }

            e.respondWith(new Response(String(count)));
        });
    "#;

    run_in_local(|| async {
        let limits = RuntimeLimits {
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 20_000,
            ..Default::default()
        };
        let mut worker = Worker::new(Script::new(code), Some(limits)).await.unwrap();

        for _ in 0..3 {
            let (event, rx) = get("/");
            worker.exec(event).await.unwrap();
            assert_eq!(text(rx).await, "156");
        }
    })
    .await;
}
