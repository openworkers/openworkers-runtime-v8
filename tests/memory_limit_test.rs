use openworkers_runtime_v8::{HttpRequest, RuntimeLimits, Script, Task, TerminationReason, Worker};
use std::collections::HashMap;

#[tokio::test]
async fn test_memory_limit_arraybuffer_allocation() {
    // Set a very low memory limit (1MB)
    let limits = RuntimeLimits {
        heap_initial_mb: 1,
        heap_max_mb: 1,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 0,
    };

    // Try to allocate 10MB - should fail
    let code = r#"
        addEventListener('fetch', (event) => {
            try {
                // Try to allocate 10MB ArrayBuffer - should exceed 1MB limit
                const buffer = new ArrayBuffer(10 * 1024 * 1024);
                event.respondWith(new Response('Allocation succeeded (unexpected)', { status: 200 }));
            } catch (e) {
                // Expected: RangeError due to allocation failure
                event.respondWith(new Response('Memory limit hit: ' + e.name, { status: 507 }));
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
    let result = worker.exec(task).await;

    // exec() should return MemoryLimit termination reason
    assert_eq!(
        result,
        Err(TerminationReason::MemoryLimit),
        "Expected Err(TerminationReason::MemoryLimit), got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_memory_limit_uint8array_allocation() {
    // Set a very low memory limit (2MB)
    let limits = RuntimeLimits {
        heap_initial_mb: 1,
        heap_max_mb: 2,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 0,
    };

    // Try to allocate 20MB via Uint8Array - should fail
    let code = r#"
        addEventListener('fetch', (event) => {
            try {
                // Try to allocate 20MB Uint8Array - should exceed 2MB limit
                const arr = new Uint8Array(20 * 1024 * 1024);
                event.respondWith(new Response('Allocation succeeded (unexpected)', { status: 200 }));
            } catch (e) {
                event.respondWith(new Response('Memory limit hit: ' + e.name, { status: 507 }));
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
    let result = worker.exec(task).await;

    // exec() should return MemoryLimit termination reason
    assert_eq!(
        result,
        Err(TerminationReason::MemoryLimit),
        "Expected Err(TerminationReason::MemoryLimit), got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_memory_limit_incremental_allocation() {
    // Set 4MB limit
    let limits = RuntimeLimits {
        heap_initial_mb: 1,
        heap_max_mb: 4,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 0,
    };

    // Allocate incrementally until we hit the limit
    let code = r#"
        addEventListener('fetch', (event) => {
            const allocations = [];
            let totalMB = 0;

            try {
                // Allocate 1MB chunks until failure
                for (let i = 0; i < 10; i++) {
                    allocations.push(new Uint8Array(1024 * 1024)); // 1MB each
                    totalMB++;
                }
                event.respondWith(new Response('Allocated ' + totalMB + 'MB without limit', { status: 200 }));
            } catch (e) {
                event.respondWith(new Response('Hit limit after ' + totalMB + 'MB: ' + e.name, { status: 507 }));
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
    let result = worker.exec(task).await;

    // exec() should return MemoryLimit termination reason
    assert_eq!(
        result,
        Err(TerminationReason::MemoryLimit),
        "Expected Err(TerminationReason::MemoryLimit), got: {:?}",
        result
    );
}

#[tokio::test]
async fn test_memory_within_limit() {
    // Set generous 64MB limit
    let limits = RuntimeLimits {
        heap_initial_mb: 1,
        heap_max_mb: 64,
        max_cpu_time_ms: 0,
        max_wall_clock_time_ms: 0,
    };

    // Allocate small buffer - should succeed
    let code = r#"
        addEventListener('fetch', (event) => {
            try {
                // Allocate 1MB - well within 64MB limit
                const buffer = new ArrayBuffer(1024 * 1024);
                const view = new Uint8Array(buffer);
                view[0] = 42;
                event.respondWith(new Response('Allocated 1MB successfully, first byte: ' + view[0], { status: 200 }));
            } catch (e) {
                event.respondWith(new Response('Unexpected error: ' + e.message, { status: 500 }));
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

    let (task, rx) = Task::fetch(req);
    let result = worker.exec(task).await;

    // exec() should return Ok - no memory limit hit
    assert!(result.is_ok(), "Expected Ok(), got: {:?}", result);

    // Also verify the response is correct
    let response = rx.await.unwrap();
    assert_eq!(response.status, 200);
    let body = String::from_utf8_lossy(response.body.as_bytes().unwrap());
    assert!(
        body.contains("Allocated 1MB successfully"),
        "Expected successful allocation, got: {}",
        body
    );
}
