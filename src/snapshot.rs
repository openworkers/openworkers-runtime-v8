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

        // The whole shared surface, in one script: the snapshot pays the same
        // compile the runtime would, and fourteen of them cost more than one.
        crate::runtime::bindings::setup_surface(scope);

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

    // new_isolate starts from the runtime snapshot, so Response, Headers and the
    // other APIs exist during compilation. The code cache output does not change;
    // the context only has the right globals for type feedback.
    let mut isolate = crate::v8_helpers::new_isolate(v8::CreateParams::default());

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
