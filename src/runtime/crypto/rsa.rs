use ring::{rand, rsa, signature};
use v8;

use super::random::throw_dom_exception;
use super::uint8_array_arg;
use crate::v8_helpers::{create_array_buffer_from_vec, throw_type_error};

/// The DER tags the key envelopes are made of.
const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OBJECT_IDENTIFIER: u8 = 0x06;

/// rsaEncryption, 1.2.840.113549.1.1.1: the AlgorithmIdentifier of a
/// SubjectPublicKeyInfo that holds an RSAPublicKey.
const RSA_ENCRYPTION_OID: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];

/// The modulus sizes ring's RSA_PKCS1_2048_8192_* verifiers accept.
const MIN_MODULUS_BITS: usize = 2048;
const MAX_MODULUS_BITS: usize = 8192;

/// Splits one DER element off `input`: its tag, its contents and what
/// follows it. Only definite lengths of up to four bytes, which is all a key
/// needs.
fn der_element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;

    let (length, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0usize, |length, &byte| (length << 8) | byte as usize);
        (length, &rest[count..])
    };

    if rest.len() < length {
        return None;
    }

    let (contents, rest) = rest.split_at(length);
    Some((tag, contents, rest))
}

/// The contents of the one element of `tag` that `input` is, and nothing else.
fn der_only(input: &[u8], tag: u8) -> Option<&[u8]> {
    match der_element(input)? {
        (found, contents, []) if found == tag => Some(contents),
        _ => None,
    }
}

/// Checks `key` is an RSAPublicKey, `SEQUENCE { INTEGER n, INTEGER e }`, with
/// a modulus the verifiers accept: ring parses it only when it verifies, and
/// importKey has to reject a key that can never verify.
fn check_rsa_public_key(key: &[u8]) -> Result<(), String> {
    let malformed = || "RSA: the key is not a DER RSAPublicKey".to_string();

    let contents = der_only(key, SEQUENCE).ok_or_else(malformed)?;
    let (tag, modulus, rest) = der_element(contents).ok_or_else(malformed)?;
    if tag != INTEGER {
        return Err(malformed());
    }
    if der_only(rest, INTEGER).is_none() {
        return Err(malformed());
    }

    // A positive INTEGER with its top bit set carries a leading zero byte
    let modulus = match modulus {
        [0, rest @ ..] => rest,
        other => other,
    };
    let bits = match modulus.first() {
        Some(&first) if first != 0 => modulus.len() * 8 - first.leading_zeros() as usize,
        _ => return Err(malformed()),
    };

    if !(MIN_MODULUS_BITS..=MAX_MODULUS_BITS).contains(&bits) {
        return Err(format!(
            "RSA: the modulus is {bits} bits, not {MIN_MODULUS_BITS} to {MAX_MODULUS_BITS}"
        ));
    }

    Ok(())
}

/// The RSAPublicKey inside a SubjectPublicKeyInfo:
/// `SEQUENCE { AlgorithmIdentifier { rsaEncryption, NULL }, BIT STRING { RSAPublicKey } }`.
fn rsa_public_key_from_spki(spki: &[u8]) -> Result<&[u8], String> {
    let malformed = || "RSA: the key is not a DER SubjectPublicKeyInfo".to_string();

    let contents = der_only(spki, SEQUENCE).ok_or_else(malformed)?;
    let (tag, algorithm, rest) = der_element(contents).ok_or_else(malformed)?;
    if tag != SEQUENCE {
        return Err(malformed());
    }

    match der_element(algorithm) {
        Some((OBJECT_IDENTIFIER, RSA_ENCRYPTION_OID, _)) => {}
        Some((OBJECT_IDENTIFIER, _, _)) => {
            return Err("RSA: the key is not an rsaEncryption key".into());
        }
        _ => return Err(malformed()),
    }

    // The first byte of a BIT STRING counts the unused bits of its last byte
    match der_only(rest, BIT_STRING).ok_or_else(malformed)? {
        [0, key @ ..] => Ok(key),
        _ => Err(malformed()),
    }
}

/// What importKey keeps for a key of `format`: the PKCS#8 bytes of a private
/// key, checked by ring, or the bare RSAPublicKey of a public one.
fn import_key(format: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    match format {
        "pkcs8" => {
            rsa::KeyPair::from_pkcs8(bytes)
                .map_err(|e| format!("RSA: the key is not a usable PKCS#8 RSA key: {e}"))?;
            Ok(bytes.to_vec())
        }
        "spki" => {
            let key = rsa_public_key_from_spki(bytes)?;
            check_rsa_public_key(key)?;
            Ok(key.to_vec())
        }
        "raw" => {
            check_rsa_public_key(bytes)?;
            Ok(bytes.to_vec())
        }
        other => Err(format!("RSA: unsupported key format \"{other}\"")),
    }
}

pub(super) fn setup_rsa(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native RSA import: __nativeRsaImportKey(format, keyData) -> ArrayBuffer, the
    // key material to keep, or a DataError when the bytes are not that key
    let import_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let Some(format) = args.get(0).to_string(scope) else {
                return throw_type_error(scope, "RSA: the key format is not a string");
            };
            let format = format.to_rust_string_lossy(scope);

            let bytes = match uint8_array_arg("RSA", &args, 1) {
                Ok(bytes) => bytes,
                Err(message) => return throw_type_error(scope, &message),
            };

            match import_key(&format, &bytes) {
                Ok(material) => retval.set(create_array_buffer_from_vec(scope, material).into()),
                Err(message) => throw_dom_exception(scope, "DataError", &message),
            }
        },
    )
    .unwrap();

    let import_key = v8::String::new(scope, "__nativeRsaImportKey").unwrap();
    subtle_obj.set(scope, import_key.into(), import_fn.into());

    // Native RSA sign: __nativeRsaSign(hashAlgo, privateKeyPkcs8, data) -> ArrayBuffer
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

            // Load the RSA key pair from its PKCS#8 envelope
            let key_pair = match rsa::KeyPair::from_pkcs8(&private_key_data) {
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

    // Native RSA verify: __nativeRsaVerify(hashAlgo, rsaPublicKeyDer, signature, data) -> boolean
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
}

/// The RSASSA-PKCS1-v1_5 wrappers, extending the ECDSA ones. Installed by mod.rs.
pub(super) const JS: &str = r#"
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

                        if (format !== 'pkcs8' && format !== 'spki' && format !== 'raw') {
                            reject(new Error('Only "pkcs8" and "spki" formats are supported for RSA'));
                            return;
                        }

                        // Parsed now, so a key that cannot sign or verify rejects here:
                        // the PKCS#8 of a private key, the RSAPublicKey out of the
                        // SubjectPublicKeyInfo of a public one ("raw" takes it bare)
                        const material = new Uint8Array(crypto.subtle.__nativeRsaImportKey(format, keyBytes));

                        resolve(__createCryptoKey(
                            format === 'pkcs8' ? 'private' : 'public', extractable,
                            { name: 'RSASSA-PKCS1-v1_5', hash: { name: hashName } },
                            keyUsages, material
                        ));
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
                        const result = crypto.subtle.__nativeRsaSign(hashName, __keyData(key), dataBytes);

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
                        const isValid = crypto.subtle.__nativeRsaVerify(hashName, __keyData(key), sigBytes, dataBytes);
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
