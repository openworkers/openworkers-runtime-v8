use ring::{rand, rsa, signature};
use v8;

pub(super) fn setup_rsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
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
                    let array_buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, sig);
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
                            resolve(__createCryptoKey(
                                'private', extractable,
                                { name: 'RSASSA-PKCS1-v1_5', hash: { name: hashName } },
                                keyUsages, keyBytes
                            ));
                        } else if (format === 'spki' || format === 'raw') {
                            // SPKI/raw format for public keys
                            resolve(__createCryptoKey(
                                'public', extractable,
                                { name: 'RSASSA-PKCS1-v1_5', hash: { name: hashName } },
                                keyUsages, keyBytes
                            ));
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
