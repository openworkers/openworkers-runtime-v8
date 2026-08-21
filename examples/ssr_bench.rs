//! SSR cold-start benchmark: wake a worker, render a SvelteKit page, sleep.
//!
//!   cargo run --release --example ssr_bench -- <bundle.js> [--url U] [--dump F]
//!
//! The bundle path also comes from OW_SSR_FIXTURE. Default url is
//! http://localhost/ssr-bench, default dump is <bundle dir>/expected_v8.html.
//!
//! Snapshot on/off is chosen outside this process: an empty or missing file at
//! RUNTIME_SNAPSHOT_PATH means no snapshot. The header line says which was used.

use openworkers_core::{
    BindingInfo, Event, HttpMethod, HttpRequest, HttpResponse, LogLevel, OpFuture,
    OperationsHandle, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
    WorkerCode,
};
use openworkers_runtime_v8::{Worker, create_code_cache, pack_code_cache};
use openworkers_transform::{CodeLanguage, parse_worker_code};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

const WARM_RENDERS: usize = 20;
const COLD_CYCLES: usize = 10;
const RESIDENT_WORKERS: usize = 8;
const USAGE_MARKER: &str = "__API_USAGE__ ";

/// Counts reads of Web API globals and members. URL/URLSearchParams need their
/// instances patched too: this runtime keeps their fields as own data properties.
const INSTRUMENT_JS: &str = r#"
globalThis.__apiUsage = { __proto__: null };
(() => {
  const seen = globalThis.__apiUsage;
  const hit = (n) => { seen[n] = (seen[n] | 0) + 1; };

  const instrumentOwn = (label, obj) => {
    for (const key of Object.keys(obj)) {
      if (key.startsWith('_')) continue;
      const d = Object.getOwnPropertyDescriptor(obj, key);
      if (!d || !d.configurable || typeof d.get === 'function') continue;
      let v = d.value;
      Object.defineProperty(obj, key, {
        configurable: true,
        enumerable: d.enumerable,
        get() { hit(label + '.' + key); return v; },
        set(nv) { v = nv; }
      });
    }
  };

  for (const name of ['URL', 'URLSearchParams']) {
    const Orig = globalThis[name];
    if (typeof Orig !== 'function') continue;
    const Wrapped = function (...a) {
      const o = Reflect.construct(Orig, a, new.target || Orig);
      instrumentOwn(name, o);
      return o;
    };
    Wrapped.prototype = Orig.prototype;
    Object.defineProperty(Wrapped, 'name', { value: name });
    globalThis[name] = Wrapped;
  }

  const wrap = (label, holder, key) => {
    const d = Object.getOwnPropertyDescriptor(holder, key);
    if (!d || !d.configurable) return;
    if (typeof d.value === 'function') {
      const orig = d.value;
      d.value = function (...a) { hit(label); return orig.apply(this, a); };
      Object.defineProperty(holder, key, d);
    } else if (typeof d.get === 'function') {
      const orig = d.get;
      d.get = function () { hit(label); return orig.call(this); };
      Object.defineProperty(holder, key, d);
    }
  };

  const members = (name, obj) => {
    if (!obj) return;
    for (const key of Reflect.ownKeys(obj)) {
      if (key === 'constructor') continue;
      wrap(name + '.' + (typeof key === 'symbol' ? String(key) : key), obj, key);
    }
  };

  for (const name of ['URL', 'URLSearchParams', 'Request', 'Response', 'Headers',
                      'TextEncoder', 'TextDecoder', 'ReadableStream', 'Blob', 'FormData']) {
    const ctor = globalThis[name];
    if (typeof ctor === 'function') members(name + '.prototype', ctor.prototype);
    if (typeof ctor === 'function') members(name, ctor);
  }

  members('crypto', globalThis.crypto);
  if (globalThis.crypto && globalThis.crypto.subtle) members('crypto.subtle', globalThis.crypto.subtle);

  // Global read counters go last, so setup above is not counted as usage.
  const globals = [
    'URL', 'URLSearchParams', 'Request', 'Response', 'Headers', 'TextEncoder',
    'TextDecoder', 'ReadableStream', 'WritableStream', 'TransformStream', 'Blob',
    'File', 'FormData', 'AbortController', 'AbortSignal', 'WebSocket', 'crypto',
    'atob', 'btoa', 'fetch', 'queueMicrotask', 'structuredClone', 'setTimeout',
    'setInterval', 'clearTimeout', 'clearInterval', 'console', 'performance',
    'caches', 'env', 'Event', 'EventTarget', 'addEventListener', 'Intl'
  ];

  for (const name of globals) {
    const d = Object.getOwnPropertyDescriptor(globalThis, name);
    if (!d || !d.configurable || typeof d.get === 'function') continue;
    const value = d.value;
    Object.defineProperty(globalThis, name, {
      configurable: true,
      enumerable: d.enumerable,
      get() { hit(name); return value; },
      set(v) { Object.defineProperty(globalThis, name, { configurable: true, writable: true, enumerable: d.enumerable, value: v }); }
    });
  }

  for (const k of Object.keys(seen)) delete seen[k];
})();
"#;

/// Reports the usage table once the handler has produced its response.
const REPORT_JS: &str = r#"
(() => {
  const handler = globalThis.default;
  const orig = handler.fetch.bind(handler);
  handler.fetch = async (...a) => {
    const res = await orig(...a);
    console.log('__API_USAGE__ ' + JSON.stringify(globalThis.__apiUsage));
    return res;
  };
})();
"#;

/// Serves a 404 for every binding call: the fixture has no static assets here.
struct BenchOps {
    logs: Mutex<Vec<String>>,
}

impl BenchOps {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            logs: Mutex::new(Vec::new()),
        })
    }
}

impl OperationsHandler for BenchOps {
    fn handle_binding_fetch(
        &self,
        _binding: &str,
        _request: HttpRequest,
    ) -> OpFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async {
            Ok(HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: ResponseBody::None,
            })
        })
    }

    fn handle_log(&self, _level: LogLevel, message: String) {
        self.logs.lock().unwrap().push(message);
    }
}

async fn spawn(code: &WorkerCode, ops: OperationsHandle) -> Worker {
    let script = Script::with_bindings(code.clone(), None, vec![BindingInfo::assets("ASSETS")]);

    Worker::new_with_ops(script, Some(RuntimeLimits::default()), ops)
        .await
        .expect("worker creation failed")
}

async fn render(worker: &mut Worker, url: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(req);
    worker.exec(task).await.expect("exec failed");
    let res = rx.await.expect("no response");
    let headers = res.headers;
    let body = res
        .body
        .collect()
        .await
        .expect("body stream failed")
        .unwrap_or_default();

    (res.status, headers, body.to_vec())
}

enum Cycle {
    Create,
    CreateAndRender,
}

async fn cold_samples(code: &WorkerCode, url: &str, cycle: Cycle) -> Vec<Duration> {
    let mut samples = Vec::new();

    for _ in 0..COLD_CYCLES {
        let t = Instant::now();
        let mut worker = spawn(code, BenchOps::new()).await;

        if matches!(cycle, Cycle::CreateAndRender) {
            render(&mut worker, url).await;
        }

        samples.push(t.elapsed());
    }

    samples
}

fn row(label: &str, mut samples: Vec<Duration>) {
    samples.sort();

    println!(
        "{:<44} {:>9.2} {:>9.2}",
        label,
        samples[0].as_secs_f64() * 1000.0,
        samples[samples.len() / 2].as_secs_f64() * 1000.0
    );
}

/// Current RSS in bytes, via ps. Returns 0 when unavailable.
fn rss_bytes() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output();

    match out {
        Ok(o) => {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                * 1024
        }
        Err(_) => 0,
    }
}

fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let fixture = std::env::args()
        .nth(1)
        .filter(|a| !a.starts_with("--"))
        .or_else(|| std::env::var("OW_SSR_FIXTURE").ok())
        .expect("fixture path required: argv[1] or OW_SSR_FIXTURE");

    let url = arg_value("--url").unwrap_or_else(|| "http://localhost/ssr-bench".to_string());

    let dump = arg_value("--dump").unwrap_or_else(|| {
        let dir = std::path::Path::new(&fixture)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        dir.join("expected_v8.html").to_string_lossy().to_string()
    });

    let source = std::fs::read(&fixture).expect("cannot read fixture");
    let snapshot = openworkers_runtime_v8::platform::get_snapshot();

    println!("fixture   {} ({} bytes ESM)", fixture, source.len());
    println!(
        "snapshot  {}",
        match snapshot {
            Some(s) => format!("present, {} bytes", s.len()),
            None => "absent".to_string(),
        }
    );

    let t = Instant::now();
    let lowered = parse_worker_code(&source, CodeLanguage::JavaScript).expect("transform failed");
    let transform_ms = t.elapsed();
    println!(
        "lowered   {} bytes classic script in {:.2} ms",
        lowered.len(),
        transform_ms.as_secs_f64() * 1000.0
    );

    let t = Instant::now();
    let cache = create_code_cache(&lowered).expect("code cache failed");
    let cache_ms = t.elapsed();
    let packed = pack_code_cache(&lowered, &cache);
    println!(
        "codecache {} bytes bytecode in {:.2} ms (packed {} bytes)",
        cache.len(),
        cache_ms.as_secs_f64() * 1000.0,
        packed.len()
    );

    let plain = WorkerCode::JavaScript(lowered.clone());
    let cached = WorkerCode::Snapshot(packed);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    LocalSet::new().block_on(&rt, async move {
        // Proof leg: correctness of the render, not its cost.
        let mut worker = spawn(&plain, BenchOps::new()).await;

        let (root_status, _, root_body) = render(&mut worker, "http://localhost/").await;
        println!("\n--- GET / ---");
        println!(
            "status {} ({} bytes) - \"/\" is prerendered, so this is an ASSETS lookup, not SSR",
            root_status,
            root_body.len()
        );

        let (status, headers, body) = render(&mut worker, &url).await;
        println!("\n--- GET {} ---", url);
        println!("status {} ({} bytes)", status, body.len());

        for (k, v) in &headers {
            println!("header {}: {}", k, v);
        }

        println!(
            "{}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        );

        assert!(
            String::from_utf8_lossy(&body).contains("<html"),
            "no <html> in the rendered body"
        );

        std::fs::write(&dump, &body).expect("cannot write dump");
        println!("\ndumped    {}", dump);

        let mut fresh = spawn(&plain, BenchOps::new()).await;
        let (_, _, again) = render(&mut fresh, &url).await;
        println!(
            "determinism  fresh worker render {}",
            if again == body {
                "identical".to_string()
            } else {
                format!("DIFFERS ({} vs {} bytes)", again.len(), body.len())
            }
        );
        drop(fresh);

        let instrumented =
            WorkerCode::JavaScript(format!("{INSTRUMENT_JS}\n{lowered}\n{REPORT_JS}"));
        let probe_ops = BenchOps::new();
        let mut probe = spawn(&instrumented, probe_ops.clone()).await;
        let (probe_status, _, probe_body) = render(&mut probe, &url).await;
        drop(probe);

        println!(
            "\n--- API surface exercised (status {}, {} bytes, render {}) ---",
            probe_status,
            probe_body.len(),
            if probe_body == body {
                "unchanged"
            } else {
                "PERTURBED by instrumentation"
            }
        );
        print_usage(&probe_ops.logs.lock().unwrap());

        drop(worker);

        println!("\n{:<44} {:>9} {:>9}", "phase", "min ms", "med ms");

        row(
            "worker creation, source",
            cold_samples(&plain, &url, Cycle::Create).await,
        );
        row(
            "worker creation, code cache",
            cold_samples(&cached, &url, Cycle::Create).await,
        );
        row(
            "cold cycle, source (create + render)",
            cold_samples(&plain, &url, Cycle::CreateAndRender).await,
        );
        row(
            "cold cycle, code cache (create + render)",
            cold_samples(&cached, &url, Cycle::CreateAndRender).await,
        );

        let mut warm = spawn(&plain, BenchOps::new()).await;
        let t = Instant::now();
        render(&mut warm, &url).await;
        row("first render on a fresh worker", vec![t.elapsed()]);

        let mut samples = Vec::new();

        for _ in 0..WARM_RENDERS {
            let t = Instant::now();
            render(&mut warm, &url).await;
            samples.push(t.elapsed());
        }

        row("warm render", samples);
        drop(warm);

        let before = rss_bytes();
        let mut resident = Vec::new();

        for _ in 0..RESIDENT_WORKERS {
            let mut w = spawn(&cached, BenchOps::new()).await;
            render(&mut w, &url).await;
            resident.push(w);
        }

        let after = rss_bytes();
        println!(
            "\nRSS {:.1} MB -> {:.1} MB for {} idle workers = {:.1} MB each",
            before as f64 / 1e6,
            after as f64 / 1e6,
            RESIDENT_WORKERS,
            (after.saturating_sub(before)) as f64 / 1e6 / RESIDENT_WORKERS as f64
        );

        // V8 requires isolates to be dropped in reverse creation order.
        while resident.pop().is_some() {}
    });
}

fn print_usage(logs: &[String]) {
    let Some((_, json)) = logs.iter().find_map(|l| l.split_once(USAGE_MARKER)) else {
        println!("no usage report, logs: {:?}", logs);
        return;
    };

    let map: HashMap<String, u64> = serde_json::from_str(json.trim()).unwrap_or_default();
    let mut entries: Vec<_> = map.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    for (name, count) in entries {
        println!("{:>6}  {}", count, name);
    }
}
