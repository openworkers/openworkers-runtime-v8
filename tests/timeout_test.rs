use openworkers_runtime_v8::{HttpRequest, RuntimeLimits, Script, Task, TerminationReason, Worker};
use std::collections::HashMap;

// Wall-clock timeout tests are Linux-only because they spin CPU waiting for timeout.
// On macOS without CPU enforcement, these tests would burn CPU unnecessarily.

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_wall_clock_timeout_infinite_loop() {
    // Set a short wall-clock timeout (500ms)
    let limits = RuntimeLimits {
        heap_initial_mb: 16,
        heap_max_mb: 64,
        max_cpu_time_ms: 0, // Disabled
        max_wall_clock_time_ms: 500,
    };

    // Infinite loop - will be terminated by wall-clock timeout
    let code = r#"
        addEventListener('fetch', (event) => {
            // Infinite loop - should be terminated by wall-clock timeout
            while (true) {
                // Busy loop
            }
            event.respondWith(new Response('Should never reach here'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, _rx) = Task::fetch(req);
    let result = worker.exec(task).await.unwrap();

    // exec() should return WallClockTimeout termination reason
    assert_eq!(
        result,
        TerminationReason::WallClockTimeout,
        "Expected TerminationReason::WallClockTimeout, got: {:?}",
        result
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_wall_clock_timeout_async_loop() {
    // Set a short wall-clock timeout (500ms)
    let limits = RuntimeLimits {
        heap_initial_mb: 16,
        heap_max_mb: 64,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 500,
    };

    // Loop that includes promises - still should be terminated
    let code = r#"
        addEventListener('fetch', async (event) => {
            // Create a promise that resolves after a long time
            const neverResolves = new Promise(() => {});

            // This will hang forever waiting
            await neverResolves;

            event.respondWith(new Response('Should never reach here'));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, _rx) = Task::fetch(req);
    let result = worker.exec(task).await.unwrap();

    // Should be wall-clock timeout since it's waiting (not computing)
    assert_eq!(
        result,
        TerminationReason::WallClockTimeout,
        "Expected TerminationReason::WallClockTimeout, got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_fast_execution_no_timeout() {
    // Set a wall-clock timeout of 5 seconds
    let limits = RuntimeLimits {
        heap_initial_mb: 16,
        heap_max_mb: 64,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 5000,
    };

    // Fast code - should complete well before timeout
    let code = r#"
        addEventListener('fetch', (event) => {
            // Simple computation
            let sum = 0;
            for (let i = 0; i < 1000; i++) {
                sum += i;
            }
            event.respondWith(new Response('Sum: ' + sum, { status: 200 }));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await.unwrap();

    // Should succeed - no timeout
    assert_eq!(
        result,
        TerminationReason::Success,
        "Expected TerminationReason::Success, got: {:?}",
        result
    );

    // Verify response
    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);
    let body = String::from_utf8_lossy(response.body.as_bytes().unwrap());
    assert!(body.contains("Sum: 499500"), "Got: {}", body);
}

#[tokio::test]
async fn test_disabled_timeout_allows_long_execution() {
    // Timeout = 0 means disabled
    let limits = RuntimeLimits {
        heap_initial_mb: 16,
        heap_max_mb: 64,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 0, // Disabled
    };

    // Long-running but finite computation
    let code = r#"
        addEventListener('fetch', (event) => {
            // Compute something that takes a moment but finishes
            let sum = 0;
            for (let i = 0; i < 100000; i++) {
                sum += Math.sqrt(i);
            }
            event.respondWith(new Response('Completed: ' + sum.toFixed(2), { status: 200 }));
        });
    "#;

    let script = Script::new(code);
    let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

    let req = HttpRequest {
        method: "GET".to_string(),
        url: "http://localhost/".to_string(),
        headers: HashMap::new(),
        body: None,
    };

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await.unwrap();

    // Should succeed since timeout is disabled
    assert_eq!(
        result,
        TerminationReason::Success,
        "Expected TerminationReason::Success, got: {:?}",
        result
    );

    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);
}

// CPU time tests (Linux only)
#[cfg(target_os = "linux")]
mod cpu_tests {
    use super::*;

    #[tokio::test]
    async fn test_cpu_time_limit_infinite_loop() {
        // Set CPU time limit of 500ms
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 500,
            max_wall_clock_time_ms: 0, // Disabled
        };

        // CPU-intensive infinite loop
        let code = r#"
            addEventListener('fetch', (event) => {
                // CPU-intensive infinite loop
                while (true) {
                    // Busy computation to burn CPU
                    Math.sqrt(Math.random());
                }
                event.respondWith(new Response('Should never reach here'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: "GET".to_string(),
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: None,
        };

        let (task, _rx) = Task::fetch(req);
        let result = worker.exec(task).await.unwrap();

        // exec() should return CpuTimeLimit termination reason
        assert_eq!(
            result,
            TerminationReason::CpuTimeLimit,
            "Expected TerminationReason::CpuTimeLimit, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_cpu_time_limit_expensive_regex() {
        // Set CPU time limit of 500ms
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 500,
            max_wall_clock_time_ms: 10000, // Wall clock higher to ensure CPU is the trigger
        };

        // CPU-intensive regex (catastrophic backtracking)
        let code = r#"
            addEventListener('fetch', (event) => {
                // Exponential backtracking regex - very CPU intensive
                const regex = /^(a+)+$/;
                const input = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaab';
                regex.test(input); // This will take a very long time
                event.respondWith(new Response('Should never reach here'));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: "GET".to_string(),
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: None,
        };

        let (task, _rx) = Task::fetch(req);
        let result = worker.exec(task).await.unwrap();

        // Should be terminated by CPU limit
        assert_eq!(
            result,
            TerminationReason::CpuTimeLimit,
            "Expected TerminationReason::CpuTimeLimit, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_cpu_time_not_charged_during_sleep() {
        // Set CPU time limit of 100ms (very short)
        // but wall clock timeout of 2000ms
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 100,
            max_wall_clock_time_ms: 2000,
        };

        // Sleep for 500ms (wall clock) but use minimal CPU
        // This should NOT trigger CPU limit since sleep doesn't use CPU
        let code = r#"
            addEventListener('fetch', async (event) => {
                // Sleep for 500ms - this uses wall time but NOT CPU time
                await new Promise(resolve => setTimeout(resolve, 500));
                event.respondWith(new Response('Woke up after sleep', { status: 200 }));
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: "GET".to_string(),
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: None,
        };

        let (task, rx) = Task::fetch(req);
        let result = worker.exec(task).await.unwrap();

        // Should succeed - sleep doesn't count as CPU time
        assert_eq!(
            result,
            TerminationReason::Success,
            "Expected TerminationReason::Success, got: {:?}",
            result
        );

        let response = rx.await.unwrap();
        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn test_cpu_limit_priority_over_wall_clock() {
        // Both limits set, CPU is hit first
        let limits = RuntimeLimits {
            heap_initial_mb: 16,
            heap_max_mb: 64,
            max_cpu_time_ms: 200,
            max_wall_clock_time_ms: 5000, // 5 seconds
        };

        // CPU-intensive code that will hit CPU limit before wall clock
        let code = r#"
            addEventListener('fetch', (event) => {
                while (true) {
                    Math.sqrt(Math.random());
                }
            });
        "#;

        let script = Script::new(code);
        let mut worker = Worker::new(script, None, Some(limits)).await.unwrap();

        let req = HttpRequest {
            method: "GET".to_string(),
            url: "http://localhost/".to_string(),
            headers: HashMap::new(),
            body: None,
        };

        let (task, _rx) = Task::fetch(req);
        let result = worker.exec(task).await.unwrap();

        // CPU limit should be hit first since it's 200ms
        assert_eq!(
            result,
            TerminationReason::CpuTimeLimit,
            "Expected TerminationReason::CpuTimeLimit (priority), got: {:?}",
            result
        );
    }
}
