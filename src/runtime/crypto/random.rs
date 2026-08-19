use ring::rand;
use v8;

/// Largest buffer `crypto.getRandomValues` will fill, per the Web Crypto spec.
const RANDOM_VALUES_QUOTA: usize = 65536;

/// Falls back to a plain `Error` when the context has no `DOMException`.
fn throw_dom_exception(scope: &mut v8::PinScope, name: &str, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "DOMException").unwrap();

    let ctor = global
        .get(scope, key.into())
        .and_then(|value| v8::Local::<v8::Function>::try_from(value).ok());

    let exception = match ctor {
        Some(ctor) => {
            let name = v8::String::new(scope, name).unwrap();
            ctor.new_instance(scope, &[message.into(), name.into()])
                .map(|e| e.into())
                .unwrap_or_else(|| v8::Exception::error(scope, message))
        }
        None => v8::Exception::error(scope, message),
    };

    scope.throw_exception(exception);
}

/// The views the spec allows; float views and DataView are deliberately absent.
fn is_integer_view(value: v8::Local<v8::Value>) -> bool {
    value.is_int8_array()
        || value.is_uint8_array()
        || value.is_uint8_clamped_array()
        || value.is_int16_array()
        || value.is_uint16_array()
        || value.is_int32_array()
        || value.is_uint32_array()
        || value.is_big_int64_array()
        || value.is_big_uint64_array()
}

pub(super) fn setup_get_random_values(scope: &mut v8::PinScope, crypto_obj: v8::Local<v8::Object>) {
    let get_random_values_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let array = args.get(0);

            if !is_integer_view(array) {
                throw_dom_exception(
                    scope,
                    "TypeMismatchError",
                    "crypto.getRandomValues expects an integer TypedArray",
                );
                return;
            }

            let view = v8::Local::<v8::TypedArray>::try_from(array).unwrap();
            let len = view.byte_length();

            if len > RANDOM_VALUES_QUOTA {
                throw_dom_exception(
                    scope,
                    "QuotaExceededError",
                    &format!("crypto.getRandomValues accepts at most {RANDOM_VALUES_QUOTA} bytes"),
                );
                return;
            }

            let mut bytes = vec![0u8; len];

            // Throw rather than leave the caller with a buffer we did not randomize
            if rand::SecureRandom::fill(&rand::SystemRandom::new(), &mut bytes).is_err() {
                throw_dom_exception(scope, "OperationError", "the system RNG failed");
                return;
            }

            // A detached or empty view has nothing to fill: byte_length is 0 for both
            if let Some(buffer) = view.buffer(scope)
                && let Some(data) = buffer.get_backing_store().data()
            {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        (data.as_ptr() as *mut u8).add(view.byte_offset()),
                        len,
                    );
                }
            }

            retval.set(array);
        },
    )
    .unwrap();

    let key = v8::String::new(scope, "getRandomValues").unwrap();
    crypto_obj.set(scope, key.into(), get_random_values_fn.into());
}

pub(super) fn setup_random_uuid(scope: &mut v8::PinScope, crypto_obj: v8::Local<v8::Object>) {
    let random_uuid_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         _args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let uuid = uuid::Uuid::new_v4().to_string();
            let uuid_str = v8::String::new(scope, &uuid).unwrap();
            retval.set(uuid_str.into());
        },
    )
    .unwrap();

    let key = v8::String::new(scope, "randomUUID").unwrap();
    crypto_obj.set(scope, key.into(), random_uuid_fn.into());
}
