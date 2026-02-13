use ring::rand;
use v8;

pub(super) fn setup_get_random_values(scope: &mut v8::PinScope, crypto_obj: v8::Local<v8::Object>) {
    let get_random_values_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 1 {
                return;
            }

            let array = args.get(0);

            if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(array) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];

                // Fill with random bytes using ring
                let rng = rand::SystemRandom::new();

                if rand::SecureRandom::fill(&rng, &mut bytes).is_ok() {
                    // Copy back to the typed array
                    if let Some(buffer) = uint8_array.buffer(scope) {
                        let backing_store = buffer.get_backing_store();
                        let offset = uint8_array.byte_offset();

                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                bytes.as_ptr(),
                                (backing_store.data().unwrap().as_ptr() as *mut u8).add(offset),
                                len,
                            );
                        }
                    }
                }

                retval.set(array);
            }
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
