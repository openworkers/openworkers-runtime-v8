//! The one namespace through which the shared surface reaches this host.

use openworkers_wintertc::NATIVE_NAMESPACE;

/// Puts `value` on the namespace, creating it on the first op. Most ops are
/// functions; a few are what the host knows and the surface cannot.
pub fn register_op(scope: &mut v8::PinScope, name: &str, value: v8::Local<v8::Value>) {
    let namespace = namespace(scope);
    let key = v8::String::new(scope, name).unwrap();

    namespace.set(scope, key.into(), value);
}

/// Freezes the namespace, once every op is registered, so a guest cannot answer
/// an op in the host's place.
pub fn seal(scope: &mut v8::PinScope) {
    let namespace = namespace(scope);

    namespace.set_integrity_level(scope, v8::IntegrityLevel::Frozen);
}

fn namespace<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, NATIVE_NAMESPACE).unwrap();

    if let Some(existing) = global.get(scope, key.into())
        && let Ok(existing) = v8::Local::<v8::Object>::try_from(existing)
    {
        return existing;
    }

    let namespace = v8::Object::new(scope);

    // Hidden from enumeration, and neither replaceable nor removable.
    let hidden = v8::PropertyAttribute::DONT_ENUM
        | v8::PropertyAttribute::READ_ONLY
        | v8::PropertyAttribute::DONT_DELETE;

    global.define_own_property(scope, key.into(), namespace.into(), hidden);

    namespace
}
