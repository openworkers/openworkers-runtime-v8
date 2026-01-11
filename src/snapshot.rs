use v8;

/// Snapshot output structure
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
        use std::pin::pin;
        let scope = pin!(v8::HandleScope::new(&mut snapshot_creator));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        // Note: setup_console, setup_timers, setup_fetch are NOT included in snapshot
        // because they use native functions (v8::Function::new) which require external references.
        // They will be setup at runtime instead.

        // Setup global aliases (self, global) for browser/Node.js compatibility
        crate::runtime::bindings::setup_global_aliases(scope);

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
