#[cfg(feature = "unsafe-worker-snapshot")]
use std::collections::HashMap;
use std::pin::pin;
use v8;

/// Magic header identifying a code cache bundle.
/// "CODECA5E" in little-endian.
const CODE_CACHE_MAGIC: u32 = 0xC0DE_CA5E;

/// Snapshot output structure
#[derive(Debug)]
pub struct SnapshotOutput {
    pub output: Vec<u8>,
}

#[cfg(feature = "unsafe-worker-snapshot")]
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

#[cfg(feature = "unsafe-worker-snapshot")]
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
/// **WARNING**: Concurrent loading of different worker snapshots crashes due to
/// V8's `SharedHeapDeserializer::DeserializeStringTable` not being thread-safe.
/// Use `create_code_cache` instead for production workloads.
///
/// Creates a standalone snapshot. The resulting blob is fully self-contained —
/// V8's `create_blob()` re-serializes the entire heap.
///
/// At execution time, loading this snapshot skips transform + compile + eval,
/// giving near-instant cold starts.
///
/// Uses `FunctionCodeHandling::Clear` to strip compiled bytecode from the blob.
/// This avoids internal reference issues and produces smaller snapshots.
/// V8 re-compiles from source on first execution (negligible cost).
///
/// Stub console bindings (no-ops) are installed so top-level `console.log()` calls
/// don't crash during snapshotting. Real bindings replace them at execution time.
#[cfg(feature = "unsafe-worker-snapshot")]
pub fn create_worker_snapshot(
    js_code: &str,
    env: Option<&HashMap<String, String>>,
) -> Result<SnapshotOutput, String> {
    let _platform = crate::platform::get_platform();

    // Always create standalone snapshots (not layered).
    // Layered snapshots (snapshot_creator_from_existing_snapshot) corrupt V8's
    // StringForwardingTable when multiple snapshots are created from the same
    // base snapshot, causing "Check failed: index < size()" crashes on load.
    let mut snapshot_creator = v8::Isolate::snapshot_creator(None, None);

    // Track errors without early return — V8 requires create_blob() before dropping
    // a snapshot creator, so we must never return early once it's created.
    let mut eval_error: Option<String> = None;

    {
        let scope = pin!(v8::HandleScope::new(&mut snapshot_creator));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Standalone snapshot — set up all pure JS APIs from scratch
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

    // Force a full GC to resolve any string forwarding indices before serialization.
    // During execution, V8's GC may externalize strings, replacing their hash with a
    // forwarding index into the StringForwardingTable. If these forwarding indices
    // survive into the snapshot, the loading isolate (with an empty forwarding table)
    // will crash with "Check failed: index < size()" in GetRawHash.
    snapshot_creator.low_memory_notification();

    // CRITICAL: Always call create_blob before dropping a snapshot creator.
    // V8 panics if a snapshot creator is dropped without this call.
    let snapshot_blob = snapshot_creator
        .create_blob(v8::FunctionCodeHandling::Clear)
        .ok_or("Failed to create worker snapshot blob")?;

    // Now check if there was an eval error
    if let Some(err) = eval_error {
        return Err(err);
    }

    Ok(SnapshotOutput {
        output: snapshot_blob.to_vec(),
    })
}

/// Create a V8 code cache (compiled bytecode) for the given JavaScript source.
///
/// This does NOT run the code — it only parses
/// and compiles it with `EagerCompile`, then serializes the bytecode via
/// `UnboundScript::create_code_cache()`.
///
/// The result is thread-safe: no shared heap objects, no string table interaction.
/// Multiple code caches for different scripts can be loaded concurrently.
///
/// At execution time, V8 skips parse+compile (~80-90% of cold start cost)
/// but still runs the code (eval). This is slightly slower than a full snapshot
/// (which skips eval too) but eliminates the concurrency crash.
pub fn create_code_cache(js_code: &str) -> Result<Vec<u8>, String> {
    let _platform = crate::platform::get_platform();

    // Use the runtime snapshot (if available) so that APIs like Response, Headers etc.
    // are available during compilation. This doesn't affect the code cache output —
    // it only ensures the compilation context has the right globals for type feedback.
    let snapshot_ref = crate::platform::get_snapshot();

    let mut params = v8::CreateParams::default();

    if let Some(snapshot_data) = snapshot_ref {
        params = params.snapshot_blob((*snapshot_data).into());
    }

    let mut isolate = v8::Isolate::new(params);

    let scope = pin!(v8::HandleScope::new(&mut isolate));
    let mut scope = scope.init();
    let context = v8::Context::new(&scope, Default::default());
    let scope = &mut v8::ContextScope::new(&mut scope, context);

    let code_str =
        v8::String::new(scope, js_code).ok_or("Failed to create V8 string from source")?;

    let mut source = v8::script_compiler::Source::new(code_str, None);

    let unbound = v8::script_compiler::compile_unbound_script(
        scope,
        &mut source,
        v8::script_compiler::CompileOptions::EagerCompile,
        v8::script_compiler::NoCacheReason::NoReason,
    )
    .ok_or("Failed to compile script for code cache")?;

    let cache = unbound
        .create_code_cache()
        .ok_or("Failed to create code cache from compiled script")?;

    Ok(cache.to_vec())
}

/// Pack source code and its code cache into a single byte buffer.
///
/// Wire format:
/// ```text
/// [4 bytes: MAGIC 0xC0DECA5E LE]
/// [4 bytes: SOURCE_LEN u32 LE]
/// [SOURCE_LEN bytes: transpiled JS source UTF-8]
/// [remaining bytes: V8 code cache]
/// ```
pub fn pack_code_cache(source: &str, cache: &[u8]) -> Vec<u8> {
    let source_bytes = source.as_bytes();
    let source_len = source_bytes.len() as u32;

    let mut buf = Vec::with_capacity(4 + 4 + source_bytes.len() + cache.len());
    buf.extend_from_slice(&CODE_CACHE_MAGIC.to_le_bytes());
    buf.extend_from_slice(&source_len.to_le_bytes());
    buf.extend_from_slice(source_bytes);
    buf.extend_from_slice(cache);
    buf
}

/// Unpack source code and code cache from a packed buffer.
///
/// Returns `None` if the buffer doesn't start with the code cache magic header
/// or is too short.
pub fn unpack_code_cache(data: &[u8]) -> Option<(&str, &[u8])> {
    if !is_code_cache(data) {
        return None;
    }

    let source_len = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let source_end = 8 + source_len;

    if data.len() < source_end {
        return None;
    }

    let source = std::str::from_utf8(&data[8..source_end]).ok()?;
    let cache = &data[source_end..];
    Some((source, cache))
}

/// Check whether a byte buffer contains a valid packed code cache.
pub fn is_code_cache(data: &[u8]) -> bool {
    if data.len() < 8 {
        return false;
    }

    let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
    magic == CODE_CACHE_MAGIC
}
