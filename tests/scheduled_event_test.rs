//! A scheduled handler sees the event Cloudflare Workers gives it: `type`,
//! `scheduledTime`, `cron` and `noRetry()`, in both handler styles and on both
//! execution paths.

mod common;

use common::run_in_local;
use openworkers_core::{DefaultOps, Event, RuntimeLimits, Script, TaskResult, TaskSource};
use openworkers_runtime_v8::{
    PinnedExecuteRequest, PinnedPoolConfig, Worker, execute_pinned, init_pinned_pool,
};
use std::sync::Arc;
use tokio::sync::oneshot;

const CRON: &str = "*/5 * * * *";
const TIME: u64 = 1_700_000_000_000;

/// Throws unless the event carries every field, so the task result says.
const CHECK: &str = r#"
    function check(event) {
        const problems = [];
        if (event.type !== 'scheduled') problems.push('type=' + event.type);
        if (event.scheduledTime !== 1700000000000) problems.push('scheduledTime=' + event.scheduledTime);
        if (event.cron !== '*/5 * * * *') problems.push('cron=' + event.cron);
        if (typeof event.noRetry !== 'function') problems.push('noRetry=' + typeof event.noRetry);
        else event.noRetry();
        if (problems.length > 0) throw new Error(problems.join(', '));
    }
"#;

fn listener_script() -> Script {
    Script::new(format!(
        "{CHECK}\naddEventListener('scheduled', (event) => check(event));"
    ))
}

fn module_script() -> Script {
    Script::new(format!(
        "{CHECK}\nglobalThis.default = {{ scheduled(controller, env, ctx) {{ check(controller); }} }};"
    ))
}

fn scheduled_event() -> (Event, oneshot::Receiver<TaskResult>) {
    let source = TaskSource::Schedule {
        time: TIME,
        cron: Some(CRON.to_string()),
    };

    Event::task("cron-1".to_string(), None, Some(source), 1)
}

async fn on_worker(script: Script) -> TaskResult {
    let mut worker = Worker::new(script, None).await.unwrap();
    let (task, rx) = scheduled_event();
    worker.exec(task).await.ok();

    rx.await.unwrap()
}

async fn on_pool(script: Script, worker_id: &str) -> TaskResult {
    init_pinned_pool(PinnedPoolConfig {
        max_per_thread: 10,
        max_per_owner: None,
        max_concurrent_per_isolate: 1,
        max_cached_contexts: 10,
        overcommit: true,
        max_context_reuses: 1,
        limits: RuntimeLimits::default(),
    });

    let (task, rx) = scheduled_event();
    execute_pinned(PinnedExecuteRequest {
        owner_id: "scheduled-owner".to_string(),
        worker_id: worker_id.to_string(),
        version: 1,
        script,
        ops: Arc::new(DefaultOps),
        task,
        on_warm_hit: None,
        env_updated_at: None,
        abort: None,
        on_report: None,
    })
    .await
    .ok();

    rx.await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_listener_on_a_worker_sees_the_schedule() {
    let result = run_in_local(|| on_worker(listener_script())).await;
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_on_a_worker_sees_the_schedule() {
    let result = run_in_local(|| on_worker(module_script())).await;
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test(flavor = "current_thread")]
async fn a_listener_on_the_pool_sees_the_schedule() {
    let result = run_in_local(|| on_pool(listener_script(), "scheduled-listener")).await;
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_on_the_pool_sees_the_schedule() {
    let result = run_in_local(|| on_pool(module_script(), "scheduled-module")).await;
    assert!(result.success, "{:?}", result.error);
}

#[tokio::test(flavor = "current_thread")]
async fn a_throwing_listener_fails_the_task() {
    let script = Script::new("addEventListener('scheduled', () => { throw new Error('boom'); });");
    let result = run_in_local(|| on_worker(script)).await;

    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("boom"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_rejecting_module_fails_the_task() {
    let script =
        Script::new("globalThis.default = { async scheduled() { throw new Error('boom'); } };");
    let result = run_in_local(|| on_worker(script)).await;

    assert!(!result.success);
    assert_eq!(result.error.as_deref(), Some("boom"));
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_without_scheduled_fails_at_once() {
    let script = Script::new("globalThis.default = { fetch() { return new Response('x'); } };");
    let start = std::time::Instant::now();
    let result = run_in_local(|| on_worker(script)).await;

    assert!(!result.success);
    assert_eq!(
        result.error.as_deref(),
        Some("Worker does not implement scheduled handler")
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}
