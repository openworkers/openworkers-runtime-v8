use ring::{rand, signature, signature::KeyPair};
use v8;

use super::random::throw_dom_exception;
use super::uint8_array_arg;
use crate::v8_helpers::{create_array_buffer_from_vec, throw_error, throw_type_error};

/// Why an op did not answer: an argument was wrong, the hash is one ring has
/// no P-256 algorithm for, the bytes are not a key, or the signing failed.
enum Failure {
    Argument(String),
    Hash(String),
    Key(&'static str),
    Operation(&'static str),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Argument(message)
    }
}

/// The ring algorithms for ECDSA P-256 with `hash`. Ring pairs P-256 with
/// SHA-256 only, so SHA-384 and SHA-512 are not supported, not silently
/// signed with SHA-256.
fn algorithms(
    hash: &str,
) -> Result<
    (
        &'static signature::EcdsaSigningAlgorithm,
        &'static signature::EcdsaVerificationAlgorithm,
    ),
    Failure,
> {
    match hash.to_uppercase().as_str() {
        "SHA-256" => Ok((
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &signature::ECDSA_P256_SHA256_FIXED,
        )),
        _ => Err(Failure::Hash(format!(
            "ECDSA P-256 with {hash} is not supported, only SHA-256"
        ))),
    }
}

fn hash_arg(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<String, Failure> {
    let Some(hash) = args.get(0).to_string(scope) else {
        return Err(Failure::Argument(
            "ECDSA: the hash name is not a string".into(),
        ));
    };
    Ok(hash.to_rust_string_lossy(scope))
}

/// A fresh P-256 key pair: (PKCS#8, uncompressed public point).
fn generate() -> Result<(Vec<u8>, Vec<u8>), Failure> {
    let rng = rand::SystemRandom::new();
    let algorithm = &signature::ECDSA_P256_SHA256_FIXED_SIGNING;

    let pkcs8 = signature::EcdsaKeyPair::generate_pkcs8(algorithm, &rng)
        .map_err(|_| Failure::Operation("ECDSA: key generation failed"))?;
    let key_pair = signature::EcdsaKeyPair::from_pkcs8(algorithm, pkcs8.as_ref(), &rng)
        .map_err(|_| Failure::Operation("ECDSA: key generation failed"))?;

    Ok((
        pkcs8.as_ref().to_vec(),
        key_pair.public_key().as_ref().to_vec(),
    ))
}

/// The uncompressed public point of the P-256 key `pkcs8` holds.
fn public_key(pkcs8: &[u8]) -> Result<Vec<u8>, Failure> {
    let rng = rand::SystemRandom::new();
    let key_pair = signature::EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        pkcs8,
        &rng,
    )
    .map_err(|_| Failure::Key("ECDSA: the key is not a PKCS#8 P-256 key"))?;

    Ok(key_pair.public_key().as_ref().to_vec())
}

/// Signs (hash, pkcs8, data): the fixed-length r || s the spec wants.
fn sign(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<Vec<u8>, Failure> {
    let (signing, _) = algorithms(&hash_arg(scope, args)?)?;
    let pkcs8 = uint8_array_arg("ECDSA", args, 1)?;
    let data = uint8_array_arg("ECDSA", args, 2)?;

    let rng = rand::SystemRandom::new();
    let key_pair = signature::EcdsaKeyPair::from_pkcs8(signing, &pkcs8, &rng)
        .map_err(|_| Failure::Key("ECDSA: the key is not a PKCS#8 P-256 key"))?;

    let sig = key_pair
        .sign(&rng, &data)
        .map_err(|_| Failure::Operation("ECDSA: signing failed"))?;

    Ok(sig.as_ref().to_vec())
}

/// Verifies (hash, publicPoint, signature, data).
fn verify(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments) -> Result<bool, Failure> {
    let (_, verification) = algorithms(&hash_arg(scope, args)?)?;
    let public_point = uint8_array_arg("ECDSA", args, 1)?;
    let sig = uint8_array_arg("ECDSA", args, 2)?;
    let data = uint8_array_arg("ECDSA", args, 3)?;

    let public_key = signature::UnparsedPublicKey::new(verification, &public_point);

    Ok(public_key.verify(&data, &sig).is_ok())
}

/// Throws what stopped an op.
fn throw(scope: &mut v8::PinScope, failure: Failure) {
    match failure {
        Failure::Argument(message) => throw_type_error(scope, &message),
        Failure::Hash(message) => throw_dom_exception(scope, "NotSupportedError", &message),
        Failure::Key(message) => throw_dom_exception(scope, "DataError", message),
        Failure::Operation(message) => throw_error(scope, message),
    }
}

pub(super) fn setup_ecdsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native ECDSA key generation: __nativeEcdsaGenerateKey() -> { privateKey: ArrayBuffer, publicKey: ArrayBuffer }
    let generate_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         _args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let (pkcs8, public_point) = match generate() {
                Ok(pair) => pair,
                Err(failure) => return throw(scope, failure),
            };

            let result = v8::Object::new(scope);

            let private_buffer = create_array_buffer_from_vec(scope, pkcs8);
            let private_key_str = v8::String::new(scope, "privateKey").unwrap();
            result.set(scope, private_key_str.into(), private_buffer.into());

            let public_buffer = create_array_buffer_from_vec(scope, public_point);
            let public_key_str = v8::String::new(scope, "publicKey").unwrap();
            result.set(scope, public_key_str.into(), public_buffer.into());

            retval.set(result.into());
        },
    )
    .unwrap();

    let generate_key = v8::String::new(scope, "__nativeEcdsaGenerateKey").unwrap();
    subtle_obj.set(scope, generate_key.into(), generate_fn.into());

    // Native ECDSA public key: __nativeEcdsaPublicKey(privateKeyPkcs8) -> ArrayBuffer, the
    // uncompressed point of the public half, or a DataError when the bytes are no key
    let public_key_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let result = uint8_array_arg("ECDSA", &args, 0)
                .map_err(Failure::from)
                .and_then(|pkcs8| public_key(&pkcs8));

            match result {
                Ok(point) => retval.set(create_array_buffer_from_vec(scope, point).into()),
                Err(failure) => throw(scope, failure),
            }
        },
    )
    .unwrap();

    let public_key_key = v8::String::new(scope, "__nativeEcdsaPublicKey").unwrap();
    subtle_obj.set(scope, public_key_key.into(), public_key_fn.into());

    // Native ECDSA sign: __nativeEcdsaSign(hash, privateKeyPkcs8, data) -> ArrayBuffer
    let sign_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| match sign(scope, &args) {
            Ok(sig) => retval.set(create_array_buffer_from_vec(scope, sig).into()),
            Err(failure) => throw(scope, failure),
        },
    )
    .unwrap();

    let sign_key = v8::String::new(scope, "__nativeEcdsaSign").unwrap();
    subtle_obj.set(scope, sign_key.into(), sign_fn.into());

    // Native ECDSA verify: __nativeEcdsaVerify(hash, publicPoint, signature, data) -> boolean
    let verify_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| match verify(scope, &args) {
            Ok(is_valid) => retval.set(v8::Boolean::new(scope, is_valid).into()),
            Err(failure) => throw(scope, failure),
        },
    )
    .unwrap();

    let verify_key = v8::String::new(scope, "__nativeEcdsaVerify").unwrap();
    subtle_obj.set(scope, verify_key.into(), verify_fn.into());
}

/// The ECDSA wrappers, extending the HMAC ones. Installed by mod.rs.
pub(super) const JS: &str = r#"
        // Extend generateKey to support ECDSA
        crypto.subtle.generateKey = function(algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        const namedCurve = algorithm.namedCurve || 'P-256';
                        if (namedCurve !== 'P-256') {
                            reject(__domException('NotSupportedError', 'Only P-256 curve is supported'));
                            return;
                        }

                        __checkUsages(keyUsages, ['sign', 'verify']);

                        const result = crypto.subtle.__nativeEcdsaGenerateKey();
                        if (!result) {
                            reject(__domException('OperationError', 'Key generation failed'));
                            return;
                        }

                        // A private key carries its public half, so it can verify too
                        const privKey = __createCryptoKey(
                            'private', extractable,
                            { name: 'ECDSA', namedCurve: 'P-256' },
                            keyUsages.filter(u => u === 'sign'),
                            new Uint8Array(result.privateKey),
                            new Uint8Array(result.publicKey)
                        );

                        const pubKey = __createCryptoKey(
                            'public', true,
                            { name: 'ECDSA', namedCurve: 'P-256' },
                            keyUsages.filter(u => u === 'verify'),
                            new Uint8Array(result.publicKey)
                        );

                        const keyPair = { privateKey: privKey, publicKey: pubKey };

                        resolve(keyPair);
                    } else {
                        reject(__domException('NotSupportedError', 'Only ECDSA algorithm is supported for generateKey'));
                    }
                } catch (e) {
                    reject(e);
                }
            });
        };

        // The hash sign and verify are asked for; the native op answers whether
        // ring has it for the curve
        const __ecdsaHash = (algorithm) => {
            const hash = typeof algorithm === 'object' && algorithm !== null ? algorithm.hash : undefined;
            const name = typeof hash === 'object' && hash !== null ? hash.name : hash;
            if (typeof name !== 'string') {
                throw new TypeError('ECDSA sign and verify need an algorithm.hash');
            }
            return name;
        };

        // Store original importKey for HMAC
        const __originalImportKey = crypto.subtle.importKey;

        // Extend importKey to support ECDSA
        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'ECDSA') {
                        const keyBytes = __copyBufferSource(keyData);

                        const namedCurve = algorithm.namedCurve || 'P-256';
                        if (namedCurve !== 'P-256') {
                            reject(__domException('NotSupportedError', 'Only P-256 curve is supported'));
                            return;
                        }

                        if (format === 'raw') {
                            // Raw format is for public keys (uncompressed point)
                            __checkUsages(keyUsages, ['verify']);
                            if (keyBytes.length !== 65 || keyBytes[0] !== 4) {
                                throw __domException('DataError', 'ECDSA: the key is not an uncompressed P-256 point');
                            }
                            resolve(__createCryptoKey(
                                'public', extractable,
                                { name: 'ECDSA', namedCurve: 'P-256' },
                                keyUsages, keyBytes
                            ));
                        } else if (format === 'pkcs8') {
                            // PKCS#8 format is for private keys; the key is parsed now, and
                            // keeps its public half so it can verify too
                            __checkUsages(keyUsages, ['sign']);
                            const publicBytes = new Uint8Array(crypto.subtle.__nativeEcdsaPublicKey(keyBytes));
                            resolve(__createCryptoKey(
                                'private', extractable,
                                { name: 'ECDSA', namedCurve: 'P-256' },
                                keyUsages, keyBytes, publicBytes
                            ));
                        } else {
                            reject(__domException('NotSupportedError', 'Only "raw" and "pkcs8" formats are supported for ECDSA'));
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
                        __checkKey(key, 'ECDSA', 'sign');
                        if (key.type !== 'private') {
                            throw __domException('InvalidAccessError', 'Only a private key signs');
                        }

                        const dataBytes = __bufferSource(data);

                        resolve(crypto.subtle.__nativeEcdsaSign(__ecdsaHash(algorithm), __keyData(key), dataBytes));
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
                        // A private key verifies with the public half it carries: an
                        // extension of the spec, under its sign usage
                        __checkKey(key, 'ECDSA', key.type === 'private' ? 'sign' : 'verify');

                        const dataBytes = __bufferSource(data);
                        const sigBytes = __bufferSource(signature);

                        // For private keys, use the public key data
                        const publicKeyData = key.type === 'private' ? __publicKeyData(key) : __keyData(key);
                        const isValid = crypto.subtle.__nativeEcdsaVerify(__ecdsaHash(algorithm), publicKeyData, sigBytes, dataBytes);
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
