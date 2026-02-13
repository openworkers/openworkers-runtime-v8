use ring::pbkdf2;
use v8;

pub(super) fn setup_pbkdf2(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native PBKDF2: __nativePbkdf2DeriveBits(hashAlgo, password, salt, iterations, lengthBits) -> ArrayBuffer
    let derive_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 5 {
                retval.set(v8::undefined(scope).into());
                return;
            }

            let hash_algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let password =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::undefined(scope).into());
                    return;
                };

            let salt = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(2)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let iterations = if let Some(n) = args.get(3).number_value(scope) {
                n as u32
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let length_bits = if let Some(n) = args.get(4).number_value(scope) {
                n as usize
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let length_bytes = length_bits / 8;

            let algorithm = match hash_algo.to_uppercase().as_str() {
                "SHA-1" => pbkdf2::PBKDF2_HMAC_SHA1,
                "SHA-256" => pbkdf2::PBKDF2_HMAC_SHA256,
                "SHA-384" => pbkdf2::PBKDF2_HMAC_SHA384,
                "SHA-512" => pbkdf2::PBKDF2_HMAC_SHA512,
                _ => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let iterations = std::num::NonZeroU32::new(iterations)
                .unwrap_or(std::num::NonZeroU32::new(1).unwrap());

            let mut out = vec![0u8; length_bytes];
            pbkdf2::derive(algorithm, iterations, &salt, &password, &mut out);

            let array_buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, out);

            retval.set(array_buffer.into());
        },
    )
    .unwrap();

    let derive_key = v8::String::new(scope, "__nativePbkdf2DeriveBits").unwrap();
    subtle_obj.set(scope, derive_key.into(), derive_fn.into());

    // JS wrappers: extend importKey for PBKDF2 and add deriveBits
    let code = r#"
        const __rsaImportKey = crypto.subtle.importKey;

        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'PBKDF2') {
                        if (format !== 'raw') {
                            reject(new Error('Only "raw" format is supported for PBKDF2'));
                            return;
                        }

                        let keyBytes;
                        if (keyData instanceof ArrayBuffer) {
                            keyBytes = new Uint8Array(keyData);
                        } else if (keyData instanceof Uint8Array) {
                            keyBytes = keyData;
                        } else if (typeof keyData === 'string') {
                            keyBytes = new TextEncoder().encode(keyData);
                        } else {
                            reject(new Error('Key data must be ArrayBuffer, Uint8Array, or string'));
                            return;
                        }

                        resolve(__createCryptoKey(
                            'secret', extractable,
                            { name: 'PBKDF2' },
                            keyUsages, keyBytes
                        ));
                    } else {
                        __rsaImportKey.call(crypto.subtle, format, keyData, algorithm, extractable, keyUsages)
                            .then(resolve)
                            .catch(reject);
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.deriveBits = function(algorithm, baseKey, length) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName !== 'PBKDF2') {
                        reject(new Error('Only PBKDF2 algorithm is supported for deriveBits'));
                        return;
                    }

                    if (!baseKey.__keyData || baseKey.algorithm.name !== 'PBKDF2') {
                        reject(new Error('Invalid key for PBKDF2'));
                        return;
                    }

                    let salt;
                    if (algorithm.salt instanceof ArrayBuffer) {
                        salt = new Uint8Array(algorithm.salt);
                    } else if (algorithm.salt instanceof Uint8Array) {
                        salt = algorithm.salt;
                    } else {
                        reject(new Error('Salt must be ArrayBuffer or Uint8Array'));
                        return;
                    }

                    const iterations = algorithm.iterations;
                    if (!iterations || iterations < 1) {
                        reject(new Error('Iterations must be a positive number'));
                        return;
                    }

                    const hashName = typeof algorithm.hash === 'string'
                        ? algorithm.hash
                        : algorithm.hash.name;

                    const result = crypto.subtle.__nativePbkdf2DeriveBits(
                        hashName, baseKey.__keyData, salt, iterations, length
                    );

                    if (result) {
                        resolve(result);
                    } else {
                        reject(new Error('PBKDF2 deriveBits failed'));
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
