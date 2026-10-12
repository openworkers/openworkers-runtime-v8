//! The ids of the Durable Object binding, which the runner makes.

use openworkers_core::{ObjectIdSource, OperationsHandle};

use crate::v8_helpers::throw_type_error;

/// The runner, which knows the namespace of each binding.
struct Ids(OperationsHandle);

/// Installs `__nativeDurableObjectId(binding, kind, value)`, which answers
/// the id as a string or throws a TypeError. `kind` is `name`, `text` or
/// `unique`.
pub fn setup_durable_object_ids(scope: &mut v8::PinScope, ops: OperationsHandle) {
    store_state!(scope, Ids(ops));

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let Some(ids) = get_state!(scope, Ids) else {
                return;
            };

            let Ok(binding) = serde_v8::from_v8_any::<String>(scope, args.get(0)) else {
                return throw_type_error(scope, "durable object: the binding name is not a string");
            };

            let Ok(kind) = serde_v8::from_v8_any::<String>(scope, args.get(1)) else {
                return throw_type_error(scope, "durable object: the id kind is not a string");
            };

            let value = serde_v8::from_v8_any::<String>(scope, args.get(2));

            let source = match (kind.as_str(), value) {
                ("name", Ok(name)) => ObjectIdSource::Name(name),
                ("text", Ok(text)) => ObjectIdSource::Text(text),
                ("unique", _) => ObjectIdSource::Unique,
                _ => return throw_type_error(scope, "durable object: not an id source"),
            };

            match ids.0.durable_object_id(&binding, source) {
                Ok(id) => {
                    let id = v8::String::new(scope, &id).unwrap();
                    retval.set(id.into());
                }
                Err(message) => throw_type_error(scope, &message),
            }
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeDurableObjectId", function);
}
