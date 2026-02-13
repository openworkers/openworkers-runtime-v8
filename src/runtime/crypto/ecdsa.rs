use ring::{rand, signature, signature::KeyPair};
use v8;

pub(super) fn setup_ecdsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
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
            let private_buffer = crate::v8_helpers::create_array_buffer_from_vec(
                scope,
                pkcs8_bytes.as_ref().to_vec(),
            );
            let private_key_str = v8::String::new(scope, "privateKey").unwrap();
            result.set(scope, private_key_str.into(), private_buffer.into());

            // Public key (uncompressed point format)
            let public_buffer =
                crate::v8_helpers::create_array_buffer_from_vec(scope, public_key_bytes.to_vec());
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

            let array_buffer =
                crate::v8_helpers::create_array_buffer_from_vec(scope, sig.as_ref().to_vec());

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

                        const privKey = __createCryptoKey(
                            'private', extractable,
                            { name: 'ECDSA', namedCurve: 'P-256' },
                            keyUsages.filter(u => u === 'sign'),
                            new Uint8Array(result.privateKey)
                        );
                        privKey.__publicKeyData = new Uint8Array(result.publicKey);

                        const pubKey = __createCryptoKey(
                            'public', true,
                            { name: 'ECDSA', namedCurve: 'P-256' },
                            keyUsages.filter(u => u === 'verify'),
                            new Uint8Array(result.publicKey)
                        );

                        const keyPair = { privateKey: privKey, publicKey: pubKey };

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
                            resolve(__createCryptoKey(
                                'public', extractable,
                                { name: 'ECDSA', namedCurve: 'P-256' },
                                keyUsages, keyBytes
                            ));
                        } else if (format === 'pkcs8') {
                            // PKCS#8 format is for private keys
                            resolve(__createCryptoKey(
                                'private', extractable,
                                { name: 'ECDSA', namedCurve: 'P-256' },
                                keyUsages, keyBytes
                            ));
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
