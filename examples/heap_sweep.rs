//! GC accounting for the SSR benchmark, one heap configuration per process.
//!
//!   OW_SSR_FIXTURE=<bundle.js> cargo run --release --example heap_sweep
//!
//! V8 flags are process-global and can only be set before initialization, so a
//! sweep runs this binary once per configuration:
//!
//!   OW_LABEL      row label in the ROW line
//!   OW_HEAP_INITIAL_MB / OW_HEAP_MAX_MB   RuntimeLimits overrides
//!   OW_V8_FLAGS   forwarded to v8::V8::set_flags_from_string by the runtime
//!   OW_URL        route to render, default http://localhost/ssr-bench

use openworkers_core::{
    BindingInfo, Event, HttpMethod, HttpRequest, HttpResponse, LogLevel, OpFuture,
    OperationsHandle, OperationsHandler, RequestBody, ResponseBody, RuntimeLimits, Script,
    WorkerCode,
};
use openworkers_runtime_v8::v8_helpers::worker_create_params;
use openworkers_runtime_v8::{Worker, create_code_cache, pack_code_cache};
use openworkers_transform::{CodeLanguage, parse_worker_code};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::task::LocalSet;

const WARM_RENDERS: usize = 2000;
const COLD_CYCLES: usize = 15;
const RESIDENT_WORKERS: usize = 64;

static BASE: OnceLock<Instant> = OnceLock::new();
static GC_START_NS: AtomicU64 = AtomicU64::new(0);
static GC_NS: AtomicU64 = AtomicU64::new(0);
static SCAVENGE: AtomicU64 = AtomicU64::new(0);
static MINOR_MS: AtomicU64 = AtomicU64::new(0);
static MAJOR: AtomicU64 = AtomicU64::new(0);
static INCREMENTAL: AtomicU64 = AtomicU64::new(0);

fn now_ns() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

unsafe extern "C" fn gc_prologue(
    _isolate: v8::UnsafeRawIsolatePtr,
    _kind: v8::GCType,
    _flags: v8::GCCallbackFlags,
    _data: *mut std::ffi::c_void,
) {
    GC_START_NS.store(now_ns(), Ordering::Relaxed);
}

unsafe extern "C" fn gc_epilogue(
    _isolate: v8::UnsafeRawIsolatePtr,
    kind: v8::GCType,
    _flags: v8::GCCallbackFlags,
    _data: *mut std::ffi::c_void,
) {
    let start = GC_START_NS.load(Ordering::Relaxed);
    GC_NS.fetch_add(now_ns().saturating_sub(start), Ordering::Relaxed);

    let counter = if kind.0 & v8::GCType::kGCTypeScavenge.0 != 0 {
        &SCAVENGE
    } else if kind.0 & v8::GCType::kGCTypeMinorMarkSweep.0 != 0 {
        &MINOR_MS
    } else if kind.0 & v8::GCType::kGCTypeMarkSweepCompact.0 != 0 {
        &MAJOR
    } else if kind.0 & v8::GCType::kGCTypeIncrementalMarking.0 != 0 {
        &INCREMENTAL
    } else {
        return;
    };

    counter.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy)]
struct Gc {
    ns: u64,
    scavenge: u64,
    minor_ms: u64,
    major: u64,
    incremental: u64,
}

impl Gc {
    fn read() -> Self {
        Self {
            ns: GC_NS.load(Ordering::Relaxed),
            scavenge: SCAVENGE.load(Ordering::Relaxed),
            minor_ms: MINOR_MS.load(Ordering::Relaxed),
            major: MAJOR.load(Ordering::Relaxed),
            incremental: INCREMENTAL.load(Ordering::Relaxed),
        }
    }

    fn since(self) -> Self {
        let now = Self::read();
        Self {
            ns: now.ns - self.ns,
            scavenge: now.scavenge - self.scavenge,
            minor_ms: now.minor_ms - self.minor_ms,
            major: now.major - self.major,
            incremental: now.incremental - self.incremental,
        }
    }

    fn ms(self) -> f64 {
        self.ns as f64 / 1e6
    }
}

struct BenchOps;

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

    fn handle_log(&self, _level: LogLevel, _message: String) {}
}

/// Size and physical footprint of the new space, over all its heap spaces.
fn new_space(isolate: &mut v8::Isolate) -> (usize, usize) {
    let mut size = 0;
    let mut physical = 0;

    for i in 0..isolate.number_of_heap_spaces() {
        let Some(s) = isolate.get_heap_space_statistics(i) else {
            continue;
        };

        if s.space_name().to_string_lossy().starts_with("new_") {
            size += s.space_size();
            physical += s.physical_space_size();
        }
    }

    (size, physical)
}

async fn spawn(code: &WorkerCode, limits: RuntimeLimits) -> Worker {
    let script = Script::with_bindings(code.clone(), None, vec![BindingInfo::assets("ASSETS")]);

    let ops: OperationsHandle = Arc::new(BenchOps);
    let mut worker = Worker::new_with_ops(script, Some(limits), ops)
        .await
        .expect("worker creation failed");

    worker.with_runtime(|rt| {
        let null = std::ptr::null_mut();
        rt.isolate
            .add_gc_prologue_callback(gc_prologue, null, v8::GCType::kGCTypeAll);
        rt.isolate
            .add_gc_epilogue_callback(gc_epilogue, null, v8::GCType::kGCTypeAll);
    });

    worker
}

async fn render(worker: &mut Worker, url: &str) -> usize {
    let req = HttpRequest {
        method: HttpMethod::Get,
        url: url.to_string(),
        headers: HashMap::new(),
        body: RequestBody::None,
    };

    let (task, rx) = Event::fetch(req);
    worker.exec(task).await.expect("exec failed");
    let res = rx.await.expect("no response");

    res.body.collect().await.unwrap_or_default().len()
}

fn quantiles(mut samples: Vec<Duration>) -> (f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;

    (ms(samples[0]), ms(samples[samples.len() / 2]))
}

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

fn env_mb(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    BASE.get_or_init(Instant::now);

    let fixture = std::env::var("OW_SSR_FIXTURE").expect("OW_SSR_FIXTURE required");
    let url = std::env::var("OW_URL").unwrap_or_else(|_| "http://localhost/ssr-bench".to_string());
    let label = std::env::var("OW_LABEL").unwrap_or_else(|_| "default".to_string());
    let flags = std::env::var("OW_V8_FLAGS").unwrap_or_default();

    let mut limits = RuntimeLimits::default();
    limits.heap_initial_mb = env_mb("OW_HEAP_INITIAL_MB", limits.heap_initial_mb);
    limits.heap_max_mb = env_mb("OW_HEAP_MAX_MB", limits.heap_max_mb);
    // The warm leg keeps one worker past the 50 ms per-request CPU budget.
    limits.max_cpu_time_ms = 0;

    println!(
        "label {} initial {} MB max {} MB flags \"{}\"",
        label, limits.heap_initial_mb, limits.heap_max_mb, flags
    );

    // What heap_limits(initial, max) derives on its own, next to what the runtime
    // actually asks V8 for.
    let raw = v8::CreateParams::default().heap_limits(
        limits.heap_initial_mb * 1024 * 1024,
        limits.heap_max_mb * 1024 * 1024,
    );
    let used = worker_create_params(&limits, &Arc::new(AtomicBool::new(false)));
    println!(
        "derived   young init {} KB max {} KB (runtime asks {} KB) / old init {} KB max {} KB",
        raw.initial_young_generation_size_in_bytes() / 1024,
        raw.max_young_generation_size_in_bytes() / 1024,
        used.max_young_generation_size_in_bytes() / 1024,
        raw.initial_old_generation_size_in_bytes() / 1024,
        raw.max_old_generation_size_in_bytes() / 1024,
    );

    let source = std::fs::read(&fixture).expect("cannot read fixture");
    let lowered = parse_worker_code(&source, CodeLanguage::JavaScript).expect("transform failed");
    let cache = create_code_cache(&lowered).expect("code cache failed");
    let cached = WorkerCode::Snapshot(pack_code_cache(&lowered, &cache));

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    LocalSet::new().block_on(&rt, async move {
        // Cold: create a worker, render once, drop it.
        let mut cold = Vec::new();
        let cold_gc = Gc::read();

        for _ in 0..COLD_CYCLES {
            let t = Instant::now();
            let mut worker = spawn(&cached, limits.clone()).await;
            render(&mut worker, &url).await;
            cold.push(t.elapsed());
        }

        let cold_gc = cold_gc.since();
        let (cold_min, cold_med) = quantiles(cold);

        // Warm: one worker, many renders.
        let mut warm_worker = spawn(&cached, limits.clone()).await;
        let bytes = render(&mut warm_worker, &url).await;

        let warm_gc = Gc::read();
        let warm_start = Instant::now();
        let mut warm = Vec::new();

        for _ in 0..WARM_RENDERS {
            let t = Instant::now();
            render(&mut warm_worker, &url).await;
            warm.push(t.elapsed());
        }

        let warm_wall = warm_start.elapsed();
        let warm_gc = warm_gc.since();
        let (warm_min, warm_med) = quantiles(warm);

        let (heap_limit, used, allocated, warm_new_space, warm_physical) =
            warm_worker.with_runtime(|rt| {
                let stats = rt.isolate.get_heap_statistics();

                (
                    stats.heap_size_limit(),
                    stats.used_heap_size(),
                    stats.total_allocated_bytes(),
                    new_space(&mut rt.isolate).0,
                    stats.total_physical_size(),
                )
            });

        // What the young space costs once the worker stops being saturated.
        let warm_idle_physical = warm_worker.with_runtime(|rt| {
            rt.isolate.low_memory_notification();
            rt.isolate.get_heap_statistics().total_physical_size()
        });

        drop(warm_worker);

        // Density: RSS cost of an idle worker that has served one request. The
        // first workers reuse pages the earlier phases left in V8's pool, so the
        // second half is the honest marginal cost.
        let before = rss_bytes();
        let mut half = 0;
        let mut resident = Vec::new();

        for i in 0..RESIDENT_WORKERS {
            let mut w = spawn(&cached, limits.clone()).await;
            render(&mut w, &url).await;
            resident.push(w);

            if i + 1 == RESIDENT_WORKERS / 2 {
                half = rss_bytes();
            }
        }

        let after = rss_bytes();
        let per_worker = (after.saturating_sub(before)) as f64 / RESIDENT_WORKERS as f64;
        let per_worker_tail =
            (after.saturating_sub(half)) as f64 / (RESIDENT_WORKERS - RESIDENT_WORKERS / 2) as f64;

        // Per-isolate view of the same cost, immune to the page pool reuse that
        // makes the process-wide RSS delta hard to attribute.
        let (resident_physical, resident_new_space) = resident
            .last_mut()
            .expect("no resident worker")
            .with_runtime(|rt| {
                let physical = rt.isolate.get_heap_statistics().total_physical_size();
                (physical, new_space(&mut rt.isolate).1)
            });

        // V8 requires isolates to be dropped in reverse creation order.
        while resident.pop().is_some() {}

        let warm_share = warm_gc.ms() / (warm_wall.as_secs_f64() * 1000.0) * 100.0;
        let n = WARM_RENDERS as f64;

        println!("body      {} bytes", bytes);
        println!(
            "heap      limit {} MB, hot worker used {} KB, physical {} KB ({} KB once idle), new space {} KB, allocated {:.1} MB total",
            heap_limit / 1024 / 1024,
            used / 1024,
            warm_physical / 1024,
            warm_idle_physical / 1024,
            warm_new_space / 1024,
            allocated as f64 / 1e6,
        );
        println!(
            "cold      {:.2} min / {:.2} med ms over {} cycles, gc {:.3} ms total, {} scavenge {} minorms {} major",
            cold_min,
            cold_med,
            COLD_CYCLES,
            cold_gc.ms(),
            cold_gc.scavenge,
            cold_gc.minor_ms,
            cold_gc.major,
        );
        // Only one render in ~25 pays a GC pause, so the median hides it: the mean
        // is the statistic that carries the GC cost.
        let warm_mean = warm_wall.as_secs_f64() * 1000.0 / WARM_RENDERS as f64;

        println!(
            "warm      {:.3} min / {:.3} med / {:.3} mean ms over {} renders, gc {:.2} ms = {:.1}% of wall, {} scavenge {} minorms {} major {} incr",
            warm_min,
            warm_med,
            warm_mean,
            WARM_RENDERS,
            warm_gc.ms(),
            warm_share,
            warm_gc.scavenge,
            warm_gc.minor_ms,
            warm_gc.major,
            warm_gc.incremental,
        );
        println!(
            "rss       {:.2} MB per idle worker ({:.2} MB over the second half), isolate physical {} KB of which new space {} KB",
            per_worker / 1e6,
            per_worker_tail / 1e6,
            resident_physical / 1024,
            resident_new_space / 1024,
        );

        println!(
            "ROW\t{}\t{:.2}\t{:.2}\t{:.3}\t{:.3}\t{:.3}\t{:.1}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}",
            label,
            cold_min,
            cold_med,
            warm_min,
            warm_med,
            warm_mean,
            warm_share,
            warm_gc.scavenge as f64 / n,
            warm_gc.major as f64 / n,
            per_worker / 1e6,
            allocated as f64 / 1e6 / n,
            resident_physical as f64 / 1e6,
            per_worker_tail / 1e6,
            warm_physical as f64 / 1e6,
            warm_idle_physical as f64 / 1e6,
        );
    });
}
