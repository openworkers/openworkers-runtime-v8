//! The natives of Durable Objects: the ids of a namespace binding, which the
//! runner makes, and the storage of the object a context serves.

use openworkers_core::{ObjectIdSource, ObjectStorageOp, OperationsHandle};
use v8::{ValueDeserializerHelper, ValueSerializerHelper};

use super::fetch::{register_callbacks_with_error, resolve_reject};
use super::state::FetchState;
use crate::runtime::SchedulerMessage;
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

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(state) = get_state!(scope, FetchState) else {
                return;
            };

            // None: the op threw, a DataCloneError or a TypeError
            let Some(op) = parse_storage_op(scope, args.get(0)) else {
                return;
            };

            let Some((resolve, reject)) = resolve_reject(scope, args.get(1), args.get(2)) else {
                return;
            };

            let callback_id = register_callbacks_with_error(&state, scope, resolve, reject);

            let _ = state
                .scheduler_tx
                .send(SchedulerMessage::ObjectStorage(callback_id, op));
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectStorage", function);
}

struct Serializer;

impl v8::ValueSerializerImpl for Serializer {
    fn throw_data_clone_error<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        message: v8::Local<'s, v8::String>,
    ) {
        let error = v8::Exception::error(scope, message);
        scope.throw_exception(error);
    }
}

struct Deserializer;

impl v8::ValueDeserializerImpl for Deserializer {}

/// The bytes of `value` in the structured clone format of V8, or `None`
/// with a DataCloneError thrown.
fn serialize(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    let context = scope.get_current_context();
    let serializer = v8::ValueSerializer::new(scope, Box::new(Serializer));

    serializer.write_header();
    serializer.write_value(context, value)?;

    Some(serializer.release())
}

/// The value `serialize` wrote into `bytes`.
pub fn deserialize<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> Option<v8::Local<'s, v8::Value>> {
    let context = scope.get_current_context();
    let deserializer = v8::ValueDeserializer::new(scope, Box::new(Deserializer), bytes);

    deserializer.read_header(context)?;
    deserializer.read_value(context)
}

fn field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, name).unwrap();

    object
        .get(scope, key.into())
        .filter(|value| !value.is_null_or_undefined())
}

fn string_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
) -> Option<String> {
    field(scope, object, name).map(|value| value.to_rust_string_lossy(scope))
}

fn strings(scope: &mut v8::PinScope, value: Option<v8::Local<v8::Value>>) -> Vec<String> {
    let Some(array) = value.and_then(|value| v8::Local::<v8::Array>::try_from(value).ok()) else {
        return Vec::new();
    };

    (0..array.length())
        .filter_map(|index| array.get_index(scope, index))
        .map(|value| value.to_rust_string_lossy(scope))
        .collect()
}

/// The op the storage glue of durable_object.js describes, with its values
/// serialized. Throws and answers `None` for a value that cannot be cloned.
fn parse_storage_op(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Option<ObjectStorageOp> {
    let Some(op) = value.to_object(scope) else {
        throw_type_error(scope, "durable object storage: the op is not an object");
        return None;
    };

    let kind = string_field(scope, op, "kind").unwrap_or_default();

    match kind.as_str() {
        "get" => {
            let keys = field(scope, op, "keys");

            Some(ObjectStorageOp::Get {
                keys: strings(scope, keys),
            })
        }
        "list" => {
            let reverse = field(scope, op, "reverse").is_some_and(|value| value.is_true());
            let limit = field(scope, op, "limit")
                .and_then(|value| value.number_value(scope))
                .map(|limit| limit as u64);

            Some(ObjectStorageOp::List {
                start: string_field(scope, op, "start"),
                start_after: string_field(scope, op, "startAfter"),
                end: string_field(scope, op, "end"),
                prefix: string_field(scope, op, "prefix"),
                reverse,
                limit,
            })
        }
        "write" => {
            let mut puts = Vec::new();

            if let Some(entries) = field(scope, op, "puts")
                .and_then(|value| v8::Local::<v8::Array>::try_from(value).ok())
            {
                for index in 0..entries.length() {
                    let entry = entries
                        .get_index(scope, index)
                        .and_then(|entry| v8::Local::<v8::Array>::try_from(entry).ok())?;
                    let key = entry.get_index(scope, 0)?.to_rust_string_lossy(scope);
                    let value = entry.get_index(scope, 1)?;

                    puts.push((key, serialize(scope, value)?));
                }
            }

            let deletes = field(scope, op, "deletes");
            let delete_all = field(scope, op, "deleteAll").is_some_and(|value| value.is_true());

            Some(ObjectStorageOp::Write {
                puts,
                deletes: strings(scope, deletes),
                delete_all,
            })
        }
        "getAlarm" => Some(ObjectStorageOp::GetAlarm),
        "setAlarm" => {
            let at = field(scope, op, "at")
                .filter(|value| value.is_number())
                .and_then(|value| value.integer_value(scope));

            Some(ObjectStorageOp::SetAlarm(at))
        }
        _ => {
            throw_type_error(scope, "durable object storage: unknown op");
            None
        }
    }
}
