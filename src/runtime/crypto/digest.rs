use ring::digest;
use v8;

pub(super) fn setup_digest(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native digest function: __nativeDigest(algorithm, data) -> ArrayBuffer
    let digest_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 2 {
                retval.set(v8::undefined(scope).into());
                return;
            }

            let algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let data = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let algorithm = match algo.to_uppercase().as_str() {
                "SHA-1" => &digest::SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => &digest::SHA256,
                "SHA-384" => &digest::SHA384,
                "SHA-512" => &digest::SHA512,
                _ => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let result = digest::digest(algorithm, &data);
            let result_bytes = result.as_ref();

            let array_buffer =
                crate::v8_helpers::create_array_buffer_from_vec(scope, result_bytes.to_vec());

            retval.set(array_buffer.into());
        },
    )
    .unwrap();

    let native_key = v8::String::new(scope, "__nativeDigest").unwrap();
    subtle_obj.set(scope, native_key.into(), digest_fn.into());

    // JS wrapper for digest that returns Promise
    let code = r#"
        crypto.subtle.digest = function(algorithm, data) {
            return new Promise((resolve, reject) => {
                try {
                    let bytes;
                    if (data instanceof ArrayBuffer) {
                        bytes = new Uint8Array(data);
                    } else if (data instanceof Uint8Array) {
                        bytes = data;
                    } else {
                        reject(new Error('Data must be ArrayBuffer or Uint8Array'));
                        return;
                    }
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;
                    const result = crypto.subtle.__nativeDigest(algoName, bytes);
                    if (result) {
                        resolve(result);
                    } else {
                        reject(new Error('Unsupported algorithm: ' + algoName));
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
