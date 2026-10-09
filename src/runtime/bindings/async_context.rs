//! The current async context frame (wintertc js/async-context.js), kept in
//! V8's continuation-preserved embedder data. V8 saves it when a promise
//! reaction or an await continuation is queued and restores it when that runs.
//! It is one value per isolate, so the host clears it at each entry from Rust.

fn async_context_get(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(scope.get_continuation_preserved_embedder_data());
}

fn async_context_set(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    scope.set_continuation_preserved_embedder_data(args.get(0));
}

/// Registers the two ops. Natives cannot be in a snapshot, so this runs for
/// each context.
pub fn setup_async_context_natives(scope: &mut v8::PinScope) {
    let get = v8::Function::new(scope, async_context_get).unwrap();
    super::native::register_op(scope, "asyncContextGet", get.into());

    let set = v8::Function::new(scope, async_context_set).unwrap();
    super::native::register_op(scope, "asyncContextSet", set.into());
}

/// Starts an entry from Rust with no frame, so that a frame a previous event
/// left current on this isolate does not reach the next one.
pub fn clear_async_context(scope: &mut v8::PinScope) {
    let undefined = v8::undefined(scope).into();
    scope.set_continuation_preserved_embedder_data(undefined);
}
