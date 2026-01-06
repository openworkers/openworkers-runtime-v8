mod common;

use common::run_in_local;
use openworkers_core::{HttpMethod, HttpRequest, RequestBody, RuntimeLimits, Script, Task};
use openworkers_runtime_v8::Worker;
use openworkers_runtime_v8::security::{CpuTimer, get_thread_cpu_time};
use std::collections::HashMap;
use std::time::Duration;

#[test]
fn test_get_thread_cpu_time_works() {
    let time = get_thread_cpu_time();
    assert!(time.is_some(), "Should be able to get CPU time on Unix");
    assert!(time.unwrap().as_nanos() > 0, "CPU time should be non-zero");
}

#[test]
fn test_cpu_timer_measures_computation() {
    let timer = CpuTimer::start();

    // Do some CPU-intensive work that can't be optimized away
    let mut sum = 0u64;

    for i in 0..10_000_000 {
        sum = sum.wrapping_add((i as f64).sqrt() as u64);
    }

    let elapsed = timer.elapsed();

    // Prevent optimization with black_box-like pattern
    std::hint::black_box(sum);

    // Should have measured some CPU time (at least 1 nanosecond)
    assert!(
        elapsed.as_nanos() > 0,
        "Should measure CPU time for computation, got {:?}",
        elapsed
    );
}

#[test]
fn test_cpu_timer_ignores_sleep() {
    let timer = CpuTimer::start();

    // Sleep doesn't consume CPU time
    std::thread::sleep(Duration::from_millis(10));

    let elapsed = timer.elapsed();

    // CPU time should be very small (< 5ms) despite 10ms sleep
    assert!(
        elapsed.as_millis() < 5,
        "Sleep should not count as CPU time, got {:?}",
        elapsed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_exec_cpu_time_measurement() {
    run_in_local(|| async {
        // Use generous limits so we don't hit them
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 0,
            stream_buffer_size: 16,
        };

        // Code that does some computation
        let code = r#"
            addEventListener('fetch', (event) => {
                // Do some work
                let sum = 0;
                for (let i = 0; i < 100000; i++) {
                    sum += Math.sqrt(i);
                }
                event.respondWith(new Response('Sum: ' + sum.toFixed(2), { status: 200 }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        // Measure CPU time around exec
        let timer = CpuTimer::start();

        let (task, rx) = Task::fetch(req);
        let result = worker.exec(task).await;

        let cpu_time = timer.elapsed();

        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        // Verify response
        let response = rx.await.unwrap();
        assert_eq!(response.status, 200);

        // Should have used some CPU time
        assert!(
            cpu_time.as_micros() > 0,
            "exec() should consume CPU time, got {:?}",
            cpu_time
        );

        println!("exec() CPU time: {:?}", cpu_time);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_exec_cpu_time_excludes_async_wait() {
    run_in_local(|| async {
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 5000,
            stream_buffer_size: 16,
        };

        // Code that sleeps (async wait, not CPU)
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Sleep for 100ms - this is wall time, not CPU time
                await new Promise(resolve => setTimeout(resolve, 100));
                event.respondWith(new Response('Slept!', { status: 200 }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };

        let timer = CpuTimer::start();

        let (task, rx) = Task::fetch(req);
        let result = worker.exec(task).await;

        let cpu_time = timer.elapsed();

        assert!(result.is_ok(), "Expected Ok, got: {:?}", result);

        let response = rx.await.unwrap();
        assert_eq!(response.status, 200);

        // CPU time should be much less than wall time (100ms sleep)
        // Allow up to 50ms for setup/teardown overhead
        assert!(
            cpu_time.as_millis() < 50,
            "Async sleep should not count as CPU time, got {:?}",
            cpu_time
        );

        println!("exec() with 100ms sleep - CPU time: {:?}", cpu_time);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_exec_cpu_intensive_uses_more_cpu_time() {
    run_in_local(|| async {
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 0,
            max_wall_clock_time_ms: 5000,
            stream_buffer_size: 16,
        };

        // Light computation
        let light_code = r#"
            addEventListener('fetch', (event) => {
                let sum = 0;
                for (let i = 0; i < 1000; i++) {
                    sum += i;
                }
                event.respondWith(new Response('Light: ' + sum));
            });
        "#;

        // Heavy computation
        let heavy_code = r#"
            addEventListener('fetch', (event) => {
                let sum = 0;
                for (let i = 0; i < 1000000; i++) {
                    sum += Math.sqrt(i) * Math.sin(i);
                }
                event.respondWith(new Response('Heavy: ' + sum));
            });
        "#;

        // Measure light
        let script = Script::new(light_code);
        let mut worker = Worker::new(script, Some(limits.clone())).await.unwrap();
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };
        let timer = CpuTimer::start();
        let (task, _rx) = Task::fetch(req);
        let _ = worker.exec(task).await.unwrap();
        let light_cpu = timer.elapsed();

        // Measure heavy
        let script = Script::new(heavy_code);
        let mut worker = Worker::new(script, Some(limits)).await.unwrap();
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: RequestBody::None,
        };
        let timer = CpuTimer::start();
        let (task, _rx) = Task::fetch(req);
        let _ = worker.exec(task).await.unwrap();
        let heavy_cpu = timer.elapsed();

        println!("Light computation CPU time: {:?}", light_cpu);
        println!("Heavy computation CPU time: {:?}", heavy_cpu);

        // Heavy should use significantly more CPU time
        assert!(
            heavy_cpu > light_cpu,
            "Heavy computation ({:?}) should use more CPU than light ({:?})",
            heavy_cpu,
            light_cpu
        );
    })
    .await;
}
