use ring::{digest, hmac, rand, rsa, signature, signature::KeyPair};
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

    // Setup crypto.randomUUID()
    setup_random_uuid(scope, crypto_obj);

    // Setup crypto.subtle.digest(algorithm, data)
    setup_digest(scope, subtle_obj);

    // Setup crypto.subtle.sign/verify/importKey for HMAC
    setup_hmac(scope, subtle_obj);

    // Setup crypto.subtle for ECDSA (ES256)
    setup_ecdsa(scope, subtle_obj);

    // Setup crypto.subtle for RSA (RS256, RS384, RS512)
    setup_rsa(scope, subtle_obj);
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

fn setup_random_uuid(scope: &mut v8::PinScope, crypto_obj: v8::Local<v8::Object>) {
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

fn setup_ecdsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native ECDSA key generation: __nativeEcdsaGenerateKey() -> { privateKey: ArrayBuffer, publicKey: ArrayBuffer }
    let generate_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         _args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let rng = rand::SystemRandom::new();

            // Generate ECDSA P-256 key pair
            let pkcs8_bytes = match signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &rng,
            ) {
                Ok(bytes) => bytes,
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            // Parse the key pair to get the public key
            let key_pair = match signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                pkcs8_bytes.as_ref(),
                &rng,
            ) {
                Ok(kp) => kp,
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let public_key_bytes = key_pair.public_key().as_ref();

            // Create result object with privateKey and publicKey
            let result = v8::Object::new(scope);

            // Private key (PKCS#8 format)
            let private_backing =
                v8::ArrayBuffer::new_backing_store_from_vec(pkcs8_bytes.as_ref().to_vec());
            let private_buffer =
                v8::ArrayBuffer::with_backing_store(scope, &private_backing.make_shared());
            let private_key_str = v8::String::new(scope, "privateKey").unwrap();
            result.set(scope, private_key_str.into(), private_buffer.into());

            // Public key (uncompressed point format)
            let public_backing =
                v8::ArrayBuffer::new_backing_store_from_vec(public_key_bytes.to_vec());
            let public_buffer =
                v8::ArrayBuffer::with_backing_store(scope, &public_backing.make_shared());
            let public_key_str = v8::String::new(scope, "publicKey").unwrap();
            result.set(scope, public_key_str.into(), public_buffer.into());

            retval.set(result.into());
        },
    )
    .unwrap();

    let generate_key = v8::String::new(scope, "__nativeEcdsaGenerateKey").unwrap();
    subtle_obj.set(scope, generate_key.into(), generate_fn.into());

    // Native ECDSA sign: __nativeEcdsaSign(privateKeyPkcs8, data) -> ArrayBuffer
    let sign_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 2 {
                retval.set(v8::undefined(scope).into());
                return;
            }

            let private_key_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(0)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
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

            let rng = rand::SystemRandom::new();

            // Load the key pair from PKCS#8
            let key_pair = match signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &private_key_data,
                &rng,
            ) {
                Ok(kp) => kp,
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            // Sign the data
            let sig = match key_pair.sign(&rng, &data) {
                Ok(s) => s,
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(sig.as_ref().to_vec());
            let array_buffer =
                v8::ArrayBuffer::with_backing_store(scope, &backing_store.make_shared());

            retval.set(array_buffer.into());
        },
    )
    .unwrap();

    let sign_key = v8::String::new(scope, "__nativeEcdsaSign").unwrap();
    subtle_obj.set(scope, sign_key.into(), sign_fn.into());

    // Native ECDSA verify: __nativeEcdsaVerify(publicKey, signature, data) -> boolean
    let verify_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 3 {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            }

            let public_key_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(0)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                };

            let sig_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                };

            let data = if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(2)) {
                let len = uint8_array.byte_length();
                let mut bytes = vec![0u8; len];
                uint8_array.copy_contents(&mut bytes);
                bytes
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            };

            // Verify using UnparsedPublicKey
            let public_key = signature::UnparsedPublicKey::new(
                &signature::ECDSA_P256_SHA256_FIXED,
                &public_key_data,
            );

            let is_valid = public_key.verify(&data, &sig_data).is_ok();
            retval.set(v8::Boolean::new(scope, is_valid).into());
        },
    )
    .unwrap();

    let verify_key = v8::String::new(scope, "__nativeEcdsaVerify").unwrap();
    subtle_obj.set(scope, verify_key.into(), verify_fn.into());

    // JS wrappers for ECDSA - extends the existing functions
    let code = r#"
        // Extend generateKey to support ECDSA
        crypto.subtle.generateKey = function(algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        const namedCurve = algorithm.namedCurve || 'P-256';
                        if (namedCurve !== 'P-256') {
                            reject(new Error('Only P-256 curve is supported'));
                            return;
                        }

                        const result = crypto.subtle.__nativeEcdsaGenerateKey();
                        if (!result) {
                            reject(new Error('Key generation failed'));
                            return;
                        }

                        const keyPair = {
                            privateKey: {
                                type: 'private',
                                extractable: extractable,
                                algorithm: { name: 'ECDSA', namedCurve: 'P-256' },
                                usages: keyUsages.filter(u => u === 'sign'),
                                __keyData: new Uint8Array(result.privateKey),
                                __publicKeyData: new Uint8Array(result.publicKey)
                            },
                            publicKey: {
                                type: 'public',
                                extractable: true,
                                algorithm: { name: 'ECDSA', namedCurve: 'P-256' },
                                usages: keyUsages.filter(u => u === 'verify'),
                                __keyData: new Uint8Array(result.publicKey)
                            }
                        };

                        resolve(keyPair);
                    } else {
                        reject(new Error('Only ECDSA algorithm is supported for generateKey'));
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // Store original importKey for HMAC
        const __originalImportKey = crypto.subtle.importKey;

        // Extend importKey to support ECDSA
        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        let keyBytes;
                        if (keyData instanceof ArrayBuffer) {
                            keyBytes = new Uint8Array(keyData);
                        } else if (keyData instanceof Uint8Array) {
                            keyBytes = keyData;
                        } else {
                            reject(new Error('Key data must be ArrayBuffer or Uint8Array'));
                            return;
                        }

                        const namedCurve = algorithm.namedCurve || 'P-256';
                        if (namedCurve !== 'P-256') {
                            reject(new Error('Only P-256 curve is supported'));
                            return;
                        }

                        if (format === 'raw') {
                            // Raw format is for public keys (uncompressed point)
                            const key = {
                                type: 'public',
                                extractable: extractable,
                                algorithm: { name: 'ECDSA', namedCurve: 'P-256' },
                                usages: keyUsages,
                                __keyData: keyBytes
                            };
                            resolve(key);
                        } else if (format === 'pkcs8') {
                            // PKCS#8 format is for private keys
                            const key = {
                                type: 'private',
                                extractable: extractable,
                                algorithm: { name: 'ECDSA', namedCurve: 'P-256' },
                                usages: keyUsages,
                                __keyData: keyBytes
                            };
                            resolve(key);
                        } else {
                            reject(new Error('Only "raw" and "pkcs8" formats are supported for ECDSA'));
                        }
                    } else {
                        // Fall back to original for HMAC
                        __originalImportKey.call(crypto.subtle, format, keyData, algorithm, extractable, keyUsages)
                            .then(resolve)
                            .catch(reject);
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // Store original sign for HMAC
        const __originalSign = crypto.subtle.sign;

        // Extend sign to support ECDSA
        crypto.subtle.sign = function(algorithm, key, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        if (key.type !== 'private' || key.algorithm.name !== 'ECDSA') {
                            reject(new Error('Invalid key for ECDSA signing'));
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

                        const result = crypto.subtle.__nativeEcdsaSign(key.__keyData, dataBytes);
                        if (result) {
                            resolve(result);
                        } else {
                            reject(new Error('ECDSA sign failed'));
                        }
                    } else {
                        // Fall back to original for HMAC
                        __originalSign.call(crypto.subtle, algorithm, key, data)
                            .then(resolve)
                            .catch(reject);
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // Store original verify for HMAC
        const __originalVerify = crypto.subtle.verify;

        // Extend verify to support ECDSA
        crypto.subtle.verify = function(algorithm, key, signature, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        if (key.algorithm.name !== 'ECDSA') {
                            reject(new Error('Invalid key for ECDSA verification'));
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

                        // For private keys, use the public key data
                        const publicKeyData = key.type === 'private' ? key.__publicKeyData : key.__keyData;
                        const isValid = crypto.subtle.__nativeEcdsaVerify(publicKeyData, sigBytes, dataBytes);
                        resolve(isValid);
                    } else {
                        // Fall back to original for HMAC
                        __originalVerify.call(crypto.subtle, algorithm, key, signature, data)
                            .then(resolve)
                            .catch(reject);
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

fn setup_rsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native RSA sign: __nativeRsaSign(hashAlgo, privateKeyDer, data) -> ArrayBuffer
    let sign_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 3 {
                retval.set(v8::undefined(scope).into());
                return;
            }

            let hash_algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::undefined(scope).into());
                return;
            };

            let private_key_data =
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

            // Select padding/encoding based on hash algorithm
            let padding = match hash_algo.to_uppercase().as_str() {
                "SHA-256" => &signature::RSA_PKCS1_SHA256,
                "SHA-384" => &signature::RSA_PKCS1_SHA384,
                "SHA-512" => &signature::RSA_PKCS1_SHA512,
                _ => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            // Load RSA key pair from DER
            let key_pair = match rsa::KeyPair::from_der(&private_key_data) {
                Ok(kp) => kp,
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                    return;
                }
            };

            let rng = rand::SystemRandom::new();
            let mut sig = vec![0u8; key_pair.public().modulus_len()];

            match key_pair.sign(padding, &rng, &data, &mut sig) {
                Ok(_) => {
                    let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(sig);
                    let array_buffer =
                        v8::ArrayBuffer::with_backing_store(scope, &backing_store.make_shared());
                    retval.set(array_buffer.into());
                }
                Err(_) => {
                    retval.set(v8::undefined(scope).into());
                }
            }
        },
    )
    .unwrap();

    let sign_key = v8::String::new(scope, "__nativeRsaSign").unwrap();
    subtle_obj.set(scope, sign_key.into(), sign_fn.into());

    // Native RSA verify: __nativeRsaVerify(hashAlgo, publicKeyDer, signature, data) -> boolean
    let verify_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            if args.length() < 4 {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            }

            let hash_algo = if let Some(algo_str) = args.get(0).to_string(scope) {
                algo_str.to_rust_string_lossy(scope)
            } else {
                retval.set(v8::Boolean::new(scope, false).into());
                return;
            };

            let public_key_data =
                if let Ok(uint8_array) = v8::Local::<v8::Uint8Array>::try_from(args.get(1)) {
                    let len = uint8_array.byte_length();
                    let mut bytes = vec![0u8; len];
                    uint8_array.copy_contents(&mut bytes);
                    bytes
                } else {
                    retval.set(v8::Boolean::new(scope, false).into());
                    return;
                };

            let sig_data =
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

            // Select verification algorithm based on hash
            let algorithm: &dyn signature::VerificationAlgorithm =
                match hash_algo.to_uppercase().as_str() {
                    "SHA-256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                    "SHA-384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                    "SHA-512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                    _ => {
                        retval.set(v8::Boolean::new(scope, false).into());
                        return;
                    }
                };

            let public_key = signature::UnparsedPublicKey::new(algorithm, &public_key_data);
            let is_valid = public_key.verify(&data, &sig_data).is_ok();

            retval.set(v8::Boolean::new(scope, is_valid).into());
        },
    )
    .unwrap();

    let verify_key = v8::String::new(scope, "__nativeRsaVerify").unwrap();
    subtle_obj.set(scope, verify_key.into(), verify_fn.into());

    // JS wrappers for RSA - extends the existing functions
    let code = r#"
        // Store original functions
        const __ecdsaImportKey = crypto.subtle.importKey;
        const __ecdsaSign = crypto.subtle.sign;
        const __ecdsaVerify = crypto.subtle.verify;

        // Extend importKey to support RSASSA-PKCS1-v1_5
        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'RSASSA-PKCS1-v1_5') {
                        let keyBytes;
                        if (keyData instanceof ArrayBuffer) {
                            keyBytes = new Uint8Array(keyData);
                        } else if (keyData instanceof Uint8Array) {
                            keyBytes = keyData;
                        } else {
                            reject(new Error('Key data must be ArrayBuffer or Uint8Array'));
                            return;
                        }

                        const hashName = typeof algorithm === 'object' && algorithm.hash
                            ? (typeof algorithm.hash === 'string' ? algorithm.hash : algorithm.hash.name)
                            : 'SHA-256';

                        if (format === 'pkcs8') {
                            // PKCS#8 format for private keys
                            const key = {
                                type: 'private',
                                extractable: extractable,
                                algorithm: { name: 'RSASSA-PKCS1-v1_5', hash: { name: hashName } },
                                usages: keyUsages,
                                __keyData: keyBytes
                            };
                            resolve(key);
                        } else if (format === 'spki' || format === 'raw') {
                            // SPKI/raw format for public keys
                            const key = {
                                type: 'public',
                                extractable: extractable,
                                algorithm: { name: 'RSASSA-PKCS1-v1_5', hash: { name: hashName } },
                                usages: keyUsages,
                                __keyData: keyBytes
                            };
                            resolve(key);
                        } else {
                            reject(new Error('Only "pkcs8" and "spki" formats are supported for RSA'));
                        }
                    } else {
                        // Fall back to ECDSA/HMAC handler
                        __ecdsaImportKey.call(crypto.subtle, format, keyData, algorithm, extractable, keyUsages)
                            .then(resolve)
                            .catch(reject);
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // Extend sign to support RSASSA-PKCS1-v1_5
        crypto.subtle.sign = function(algorithm, key, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'RSASSA-PKCS1-v1_5') {
                        if (key.type !== 'private' || key.algorithm.name !== 'RSASSA-PKCS1-v1_5') {
                            reject(new Error('Invalid key for RSA signing'));
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
                        const result = crypto.subtle.__nativeRsaSign(hashName, key.__keyData, dataBytes);

                        if (result) {
                            resolve(result);
                        } else {
                            reject(new Error('RSA sign failed'));
                        }
                    } else {
                        // Fall back to ECDSA/HMAC handler
                        __ecdsaSign.call(crypto.subtle, algorithm, key, data)
                            .then(resolve)
                            .catch(reject);
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // Extend verify to support RSASSA-PKCS1-v1_5
        crypto.subtle.verify = function(algorithm, key, signature, data) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'RSASSA-PKCS1-v1_5') {
                        if (key.algorithm.name !== 'RSASSA-PKCS1-v1_5') {
                            reject(new Error('Invalid key for RSA verification'));
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
                        const isValid = crypto.subtle.__nativeRsaVerify(hashName, key.__keyData, sigBytes, dataBytes);
                        resolve(isValid);
                    } else {
                        // Fall back to ECDSA/HMAC handler
                        __ecdsaVerify.call(crypto.subtle, algorithm, key, signature, data)
                            .then(resolve)
                            .catch(reject);
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
