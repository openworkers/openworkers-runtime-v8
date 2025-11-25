use ring::{digest, hmac, rand};
use v8;

/// Setup crypto global object with getRandomValues and subtle
pub fn setup_crypto(scope: &mut v8::PinScope) {
    let context = scope.get_current_context();
    let global = context.global(scope);

    // Create crypto object and add it to global FIRST
    let crypto_obj = v8::Object::new(scope);
    let crypto_key = v8::String::new(scope, "crypto").unwrap();
    global.set(scope, crypto_key.into(), crypto_obj.into());

    // Create crypto.subtle object
    let subtle_obj = v8::Object::new(scope);
    let subtle_key = v8::String::new(scope, "subtle").unwrap();
    crypto_obj.set(scope, subtle_key.into(), subtle_obj.into());

    // Setup crypto.getRandomValues(typedArray)
    setup_get_random_values(scope, crypto_obj);

    // Setup crypto.subtle.digest(algorithm, data)
    setup_digest(scope, subtle_obj);

    // Setup crypto.subtle.sign/verify/importKey
    setup_hmac(scope, subtle_obj);
}

fn setup_get_random_values(scope: &mut v8::PinScope, crypto_obj: v8::Local<v8::Object>) {
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

fn setup_digest(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
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

            // Get algorithm name
            let algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            // Get data as Uint8Array
            let data = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            // Select algorithm
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

            // Compute digest
            let result = digest::digest(algorithm, &data);
            let result_bytes = result.as_ref();

            // Create ArrayBuffer with result
            let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(result_bytes.to_vec());
            let array_buffer =
                v8::ArrayBuffer::with_backing_store(scope, &backing_store.make_shared());

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

fn setup_hmac(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native HMAC sign: __nativeHmacSign(algorithm, keyData, data) -> ArrayBuffer
    let sign_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 3 {
                retval.set(v8::undefined(scope).into());
                return;
            }

            let algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let key_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::undefined(scope).into());
                    return;
                };

            let data = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(2)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let algorithm = match algo.to_uppercase().as_str() {
                "SHA-1" => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => hmac::HMAC_SHA256,
                "SHA-384" => hmac::HMAC_SHA384,
                "SHA-512" => hmac::HMAC_SHA512,
                _ => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let key = hmac::Key::new(algorithm, &key_data);
            let tag = hmac::sign(&key, &data);
            let result_bytes = tag.as_ref();

            let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(result_bytes.to_vec());
            let array_buffer =
                v8::ArrayBuffer::with_backing_store(scope, &backing_store.make_shared());

            retval.set(array_buffer.into());
        },
    )
    .unwrap();

    let sign_key = v8::String::new(scope, "__nativeHmacSign").unwrap();
    subtle_obj.set(scope, sign_key.into(), sign_fn.into());

    // Native HMAC verify: __nativeHmacVerify(algorithm, keyData, signature, data) -> boolean
    let verify_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 4 {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            }

            let algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            };

            let key_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                };

            let signature =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(2)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                };

            let data = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(3)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            };

            let algorithm = match algo.to_uppercase().as_str() {
                "SHA-1" => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => hmac::HMAC_SHA256,
                "SHA-384" => hmac::HMAC_SHA384,
                "SHA-512" => hmac::HMAC_SHA512,
                _ => {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                }
            };

            let key = hmac::Key::new(algorithm, &key_data);
            let is_valid = hmac::verify(&key, &data, &signature).is_ok();

            retval.set(v8::Boolean::new(scope, is_valid).into());
        },
    )
    .unwrap();

    let verify_key = v8::String::new(scope, "__nativeHmacVerify").unwrap();
    subtle_obj.set(scope, verify_key.into(), verify_fn.into());

    // JS wrappers for sign/verify with key management
    let code = r#"
        // Simple key storage (per-isolate)
        const __cryptoKeys = new Map();
        let __nextKeyId = 1;

        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    if (format !== 'raw') {
                        reject(new Error('Only "raw" format is supported'));
                        return;
                    }

                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;
                    const hashName = typeof algorithm === 'object' && algorithm.hash
                        ? (typeof algorithm.hash === 'string' ? algorithm.hash : algorithm.hash.name)
                        : 'SHA-256';

                    if (algoName !== 'HMAC') {
                        reject(new Error('Only HMAC algorithm is supported'));
                        return;
                    }

                    let keyBytes;
                    if (keyData instanceof ArrayBuffer) {
                        keyBytes = new Uint8Array(keyData);
                    } else if (keyData instanceof Uint8Array) {
                        keyBytes = keyData;
                    } else {
                        reject(new Error('Key data must be ArrayBuffer or Uint8Array'));
                        return;
                    }

                    const keyId = __nextKeyId++;
                    const key = {
                        type: 'secret',
                        extractable: extractable,
                        algorithm: { name: 'HMAC', hash: { name: hashName } },
                        usages: keyUsages,
                        __keyId: keyId,
                        __keyData: keyBytes
                    };

                    __cryptoKeys.set(keyId, key);
                    resolve(key);
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.sign = function(algorithm, key, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName !== 'HMAC') {
                        reject(new Error('Only HMAC algorithm is supported'));
                        return;
                    }

                    if (!key.__keyData) {
                        reject(new Error('Invalid key'));
                        return;
                    }

                    let dataBytes;
                    if (data instanceof ArrayBuffer) {
                        dataBytes = new Uint8Array(data);
                    } else if (data instanceof Uint8Array) {
                        dataBytes = data;
                    } else {
                        reject(new Error('Data must be ArrayBuffer or Uint8Array'));
                        return;
                    }

                    const hashName = key.algorithm.hash.name;
                    const result = crypto.subtle.__nativeHmacSign(hashName, key.__keyData, dataBytes);

                    if (result) {
                        resolve(result);
                    } else {
                        reject(new Error('Sign failed'));
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.verify = function(algorithm, key, signature, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName !== 'HMAC') {
                        reject(new Error('Only HMAC algorithm is supported'));
                        return;
                    }

                    if (!key.__keyData) {
                        reject(new Error('Invalid key'));
                        return;
                    }

                    let dataBytes, sigBytes;
                    if (data instanceof ArrayBuffer) {
                        dataBytes = new Uint8Array(data);
                    } else if (data instanceof Uint8Array) {
                        dataBytes = data;
                    } else {
                        reject(new Error('Data must be ArrayBuffer or Uint8Array'));
                        return;
                    }

                    if (signature instanceof ArrayBuffer) {
                        sigBytes = new Uint8Array(signature);
                    } else if (signature instanceof Uint8Array) {
                        sigBytes = signature;
                    } else {
                        reject(new Error('Signature must be ArrayBuffer or Uint8Array'));
                        return;
                    }

                    const hashName = key.algorithm.hash.name;
                    const isValid = crypto.subtle.__nativeHmacVerify(hashName, key.__keyData, sigBytes, dataBytes);

                    resolve(isValid);
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
