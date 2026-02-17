use std::collections::HashMap;
use std::pin::pin;
use v8;

/// Snapshot output structure
#[derive(Debug)]
pub struct SnapshotOutput {
    pub output: Vec<u8>,
}

/// Setup env vars on `globalThis.env` for snapshot creation.
///
/// Only injects plain env vars (key/value strings), not bindings (Storage, KV, etc.)
/// which depend on native functions unavailable during snapshotting.
/// At execution time, `setup_env` replaces this with the full env + bindings object.
fn setup_snapshot_env(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    env: &HashMap<String, String>,
) {
    let env_json = serde_json::to_string(env).unwrap_or_else(|_| "{}".to_string());
    let code_str = format!(
        r#"Object.defineProperty(globalThis, 'env', {{
            value: Object.freeze({}),
            writable: false,
            enumerable: true,
            configurable: true
        }});"#,
        env_json
    );
    let code = v8::String::new(scope, &code_str).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope);
}

/// Setup no-op console stubs for snapshot creation.
///
/// During snapshotting, there's no log callback available, so we install
/// stub console methods that silently discard output. These get replaced
/// by real native-backed console bindings at execution time.
fn setup_stub_console(scope: &mut v8::ContextScope<v8::HandleScope>) {
    let code = v8::String::new(
        scope,
        r#"
        globalThis.console = {
            log() {},
            info() {},
            warn() {},
            error() {},
            debug() {},
            trace() {}
        };
        "#,
    )
    .unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope);
}

/// Create a V8 runtime snapshot with pre-compiled runtime bindings
///
/// This snapshot includes pre-compiled JavaScript for:
/// - console.log/warn/error
/// - URL API
/// - Response constructor
///
/// Note: Timers and Fetch are not included as they require runtime-specific state
pub fn create_runtime_snapshot() -> Result<SnapshotOutput, String> {
    // Get global V8 platform (initialized once, shared across all modules)
    let _platform = crate::platform::get_platform();

    // Create isolate with snapshot creator
    let mut snapshot_creator = v8::Isolate::snapshot_creator(None, None);

    {
        let scope = pin!(v8::HandleScope::new(&mut snapshot_creator));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Note: setup_console, setup_timers, setup_fetch are NOT included in snapshot
        // because they use native functions (v8::Function::new) which require external references.
        // They will be setup at runtime instead.

        // Setup global aliases (self, global) for browser/Node.js compatibility
        crate::runtime::bindings::setup_global_aliases(scope);

        // NOTE: Security restrictions (SharedArrayBuffer, Atomics removal) are NOT in snapshot.
        // Modifying these built-ins in the snapshot context corrupts V8's internal state,
        // breaking context bootstrapping. Applied at context creation time instead.

        // Setup TextEncoder/TextDecoder (pre-compiled in snapshot - pure JS)
        crate::runtime::text_encoding::setup_text_encoding(scope);

        // Setup ReadableStream API (pre-compiled in snapshot - pure JS)
        crate::runtime::streams::setup_readable_stream(scope);

        // Setup Blob/File - pure JS
        crate::runtime::bindings::setup_blob(scope);

        // Setup FormData - pure JS (must be after Blob)
        crate::runtime::bindings::setup_form_data(scope);

        // Setup AbortController/AbortSignal - pure JS
        crate::runtime::bindings::setup_abort_controller(scope);

        // Setup structuredClone - pure JS
        crate::runtime::bindings::setup_structured_clone(scope);

        // Setup Base64 (atob/btoa) - pure JS
        crate::runtime::bindings::setup_base64(scope);

        // Setup URLSearchParams (must be before URL)
        crate::runtime::bindings::setup_url_search_params(scope);

        // Setup URL API (pre-compiled in snapshot - pure JS)
        crate::runtime::bindings::setup_url(scope);

        // Setup Headers API (pre-compiled in snapshot - pure JS)
        crate::runtime::bindings::setup_headers(scope);

        // Setup Request class (pre-compiled in snapshot - pure JS)
        // Note: Request depends on Headers, so Headers must be setup first
        crate::runtime::bindings::setup_request(scope);

        // Setup Response constructor (pre-compiled in snapshot - pure JS)
        // Note: Response depends on Headers, so Headers must be setup first
        crate::runtime::bindings::setup_response(scope);

        // Setup fetch helpers (__normalizeFetchInput, __bufferBody)
        // Note: depends on Request, Headers, URL for instanceof checks
        crate::runtime::bindings::setup_fetch_helpers(scope);

        // Set this context as the default context for the snapshot
        scope.set_default_context(context);
    }

    // Create the snapshot blob
    let snapshot_blob = snapshot_creator
        .create_blob(v8::FunctionCodeHandling::Keep)
        .ok_or("Failed to create snapshot blob")?;

    Ok(SnapshotOutput {
        output: snapshot_blob.to_vec(),
    })
}

/// Create a V8 snapshot with worker code already evaluated.
///
/// This creates a **standalone** snapshot that includes all runtime APIs plus
/// the worker's evaluated code (e.g., `globalThis.default = { fetch() { ... } }`).
///
/// At execution time, loading this snapshot skips transform + compile + eval,
/// giving near-instant cold starts.
///
/// Note: We always create standalone snapshots (not layered on the runtime snapshot)
/// because V8 layered snapshots have internal string table references that become
/// invalid when loaded via `v8::Isolate::new()`. Standalone snapshots are fully
/// self-contained and load reliably.
///
/// Stub console bindings (no-ops) are installed so top-level `console.log()` calls
/// don't crash during snapshotting. Real bindings replace them at execution time.
pub fn create_worker_snapshot(
    js_code: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<SnapshotOutput, String> {
    let _platform = crate::platform::get_platform();

    // Always create a standalone snapshot (not layered on runtime snapshot).
    // Layered snapshots (via snapshot_creator_from_existing_snapshot) crash when
    // loaded into a regular isolate due to StringForwardingTable deserialization errors.
    let mut snapshot_creator = v8::Isolate::snapshot_creator(None, None);

    // Track errors without early return — V8 requires create_blob() before dropping
    // a snapshot creator, so we must never return early once it's created.
    let mut eval_error: Option<String> = None;

    {
        let scope = pin!(v8::HandleScope::new(&mut snapshot_creator));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Set up all pure JS APIs from scratch (standalone snapshot)
        crate::runtime::bindings::setup_global_aliases(scope);
        crate::runtime::text_encoding::setup_text_encoding(scope);
        crate::runtime::streams::setup_readable_stream(scope);
        crate::runtime::bindings::setup_blob(scope);
        crate::runtime::bindings::setup_form_data(scope);
        crate::runtime::bindings::setup_abort_controller(scope);
        crate::runtime::bindings::setup_structured_clone(scope);
        crate::runtime::bindings::setup_base64(scope);
        crate::runtime::bindings::setup_url_search_params(scope);
        crate::runtime::bindings::setup_url(scope);
        crate::runtime::bindings::setup_headers(scope);
        crate::runtime::bindings::setup_request(scope);
        crate::runtime::bindings::setup_response(scope);
        crate::runtime::bindings::setup_fetch_helpers(scope);

        // Install no-op console stubs so top-level console.log() doesn't crash
        setup_stub_console(scope);

        // Inject env vars so top-level code can read globalThis.env
        if let Some(env) = env {
            setup_snapshot_env(scope, env);
        }

        // Compile and run the worker's code.
        // Use TryCatch to capture JS exceptions without aborting.
        let tc_scope = pin!(v8::TryCatch::new(scope));
        let mut tc_scope = tc_scope.init();

        if let Some(code_str) = v8::String::new(&tc_scope, js_code) {
            if let Some(script) = v8::Script::compile(&tc_scope, code_str, None) {
                if script.run(&tc_scope).is_none() {
                    // Runtime error (throw, ReferenceError, etc.)
                    let msg = tc_scope
                        .exception()
                        .and_then(|e| e.to_string(&tc_scope))
                        .map(|s| s.to_rust_string_lossy(&tc_scope))
                        .unwrap_or_else(|| "Unknown error".to_string());
                    eval_error = Some(format!("Worker code threw during snapshotting: {}", msg));
                }
            } else {
                let msg = tc_scope
                    .exception()
                    .and_then(|e| e.to_string(&tc_scope))
                    .map(|s| s.to_rust_string_lossy(&tc_scope))
                    .unwrap_or_else(|| "Unknown error".to_string());
                eval_error = Some(format!("Failed to compile worker code: {}", msg));
            }
        } else {
            eval_error = Some("Failed to create V8 string from worker code".to_string());
        }

        // Must always set default context before create_blob
        tc_scope.set_default_context(context);
    }

    // CRITICAL: Always call create_blob before dropping a snapshot creator.
    // V8 panics if a snapshot creator is dropped without this call.
    let snapshot_blob = snapshot_creator
        .create_blob(v8::FunctionCodeHandling::Keep)
        .ok_or("Failed to create worker snapshot blob")?;

    // Now check if there was an eval error
    if let Some(err) = eval_error {
        return Err(err);
    }

    Ok(SnapshotOutput {
        output: snapshot_blob.to_vec(),
    })
}
