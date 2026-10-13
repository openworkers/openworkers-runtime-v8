//! The natives of Durable Objects: the ids of a namespace binding, which the
//! runner makes, and the storage of the object a context serves.

use openworkers_core::{
    ObjectIdSource, ObjectSocketOp, ObjectStorageOp, OperationsHandle, WebSocketId,
    WebSocketOutgoing,
};
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

    setup_object_sockets(scope);
}

/// The largest attachment of a socket, serialized, as on Cloudflare
const MAX_ATTACHMENT: usize = 2048;

/// Sends an op on the sockets of the object to the scheduler.
fn send_socket_op(scope: &mut v8::PinScope, op: ObjectSocketOp) {
    if let Some(state) = get_state!(scope, FetchState) {
        let _ = state.scheduler_tx.send(SchedulerMessage::ObjectSocket(op));
    }
}

/// The socket id an op was given, or a TypeError thrown in its place.
fn socket_of(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<WebSocketId> {
    if !value.is_number() {
        throw_type_error(scope, "WebSocket: the socket id is not a number");
        return None;
    }

    value.number_value(scope).map(|id| id as WebSocketId)
}

/// Installs the natives of the WebSockets of the object, which the runner
/// holds:
///
///   __nativeObjectSocketId()                    a new socket id
///   __nativeObjectSocketAccept(id, tags, hib)   the object takes the socket
///   __nativeObjectSocketSend(id, data)          a frame to the client
///   __nativeObjectSocketClose(id, code, reason) closes the socket
///   __nativeObjectSocketAttachment(id, value)   sets the attachment
///   __nativeObjectSockets()                     the hibernatable sockets, as
///                                               [{ id, tags, attachment }]
fn setup_object_sockets(scope: &mut v8::PinScope) {
    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, _: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue| {
            let id = super::next_socket_id();
            rv.set(v8::Number::new(scope, id as f64).into());
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSocketId", function);

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(socket) = socket_of(scope, args.get(0)) else {
                return;
            };

            let tags = strings(scope, Some(args.get(1)));
            let hibernatable = args.get(2).is_true();

            send_socket_op(
                scope,
                ObjectSocketOp::Accept {
                    socket,
                    tags,
                    hibernatable,
                },
            );
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSocketAccept", function);

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(socket) = socket_of(scope, args.get(0)) else {
                return;
            };

            let Some(message) = super::outgoing(scope, args.get(1)) else {
                return;
            };

            send_socket_op(scope, ObjectSocketOp::Send { socket, message });
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSocketSend", function);

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(socket) = socket_of(scope, args.get(0)) else {
                return;
            };

            let code = args
                .get(1)
                .number_value(scope)
                .map(|code| code as u16)
                .unwrap_or(1000);
            let reason = args.get(2).to_rust_string_lossy(scope);
            let message = WebSocketOutgoing::Close { code, reason };

            send_socket_op(scope, ObjectSocketOp::Send { socket, message });
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSocketClose", function);

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _: v8::ReturnValue| {
            let Some(socket) = socket_of(scope, args.get(0)) else {
                return;
            };

            let value = args.get(1);

            let value = if value.is_null_or_undefined() {
                None
            } else {
                // None: a DataCloneError is thrown
                let Some(bytes) = serialize(scope, value) else {
                    return;
                };

                if bytes.len() > MAX_ATTACHMENT {
                    let message = format!(
                        "serializeAttachment: the attachment is {} bytes, more than {MAX_ATTACHMENT}",
                        bytes.len()
                    );

                    return throw_type_error(scope, &message);
                }

                Some(bytes)
            };

            send_socket_op(scope, ObjectSocketOp::Attachment { socket, value });
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSocketAttachment", function);

    let function = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope, _: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue| {
            let Some(ids) = get_state!(scope, Ids) else {
                return;
            };

            let sockets = ids.0.durable_object_sockets();
            let list = v8::Array::new(scope, sockets.len() as i32);

            for (index, socket) in sockets.into_iter().enumerate() {
                let entry = v8::Object::new(scope);

                let key = v8::String::new(scope, "id").unwrap();
                let id = v8::Number::new(scope, socket.id as f64);
                entry.set(scope, key.into(), id.into());

                let tags: Vec<v8::Local<v8::Value>> = socket
                    .tags
                    .iter()
                    .map(|tag| v8::String::new(scope, tag).unwrap().into())
                    .collect();
                let key = v8::String::new(scope, "tags").unwrap();
                let tags = v8::Array::new_with_elements(scope, &tags);
                entry.set(scope, key.into(), tags.into());

                let attachment = match socket.attachment {
                    Some(bytes) => match deserialize(scope, &bytes) {
                        Some(value) => value,
                        None => v8::null(scope).into(),
                    },
                    None => v8::null(scope).into(),
                };
                let key = v8::String::new(scope, "attachment").unwrap();
                entry.set(scope, key.into(), attachment);

                list.set_index(scope, index as u32, entry.into());
            }

            rv.set(list.into());
        },
    )
    .unwrap();

    register_fn!(scope, "__nativeObjectSockets", function);
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
