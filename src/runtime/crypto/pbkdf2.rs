use ring::pbkdf2;
use std::num::NonZeroU32;
use v8;

use super::uint8_array_arg;
use crate::v8_helpers::{create_array_buffer_from_vec, throw_type_error};

/// The most bytes one derivation may ask for. The output is Rust memory, which
/// the caps of the isolate do not see, and real uses want a few dozen bytes.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

struct Derivation {
    algorithm: pbkdf2::Algorithm,
    password: Vec<u8>,
    salt: Vec<u8>,
    iterations: NonZeroU32,
    length_bytes: usize,
}

/// The derivation `(hash, password, salt, iterations, lengthBits)` asks for,
/// or the argument that stops it.
fn derivation(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<Derivation, String> {
    let Some(hash) = args.get(0).to_string(scope) else {
        return Err("PBKDF2: the hash name is not a string".into());
    };
    let hash = hash.to_rust_string_lossy(scope);
    let algorithm = match hash.to_uppercase().as_str() {
        "SHA-1" => pbkdf2::PBKDF2_HMAC_SHA1,
        "SHA-256" => pbkdf2::PBKDF2_HMAC_SHA256,
        "SHA-384" => pbkdf2::PBKDF2_HMAC_SHA384,
        "SHA-512" => pbkdf2::PBKDF2_HMAC_SHA512,
        _ => return Err(format!("PBKDF2: unknown hash \"{hash}\"")),
    };

    let password = uint8_array_arg("PBKDF2", args, 1)?;
    let salt = uint8_array_arg("PBKDF2", args, 2)?;

    let iterations = args.get(3);
    if !iterations.is_number() {
        return Err("PBKDF2: the iteration count is not a number".into());
    }
    let Some(iterations) = NonZeroU32::new(iterations.number_value(scope).unwrap() as u32) else {
        return Err("PBKDF2: the iteration count must be at least 1".into());
    };

    let length = args.get(4);
    if !length.is_number() {
        return Err("PBKDF2: the length is not a number".into());
    }
    let length_bytes = length.number_value(scope).unwrap() as usize / 8;
    if length_bytes > MAX_OUTPUT_BYTES {
        return Err(format!(
            "PBKDF2: the length is over {} bits",
            MAX_OUTPUT_BYTES * 8
        ));
    }

    Ok(Derivation {
        algorithm,
        password,
        salt,
        iterations,
        length_bytes,
    })
}

pub(super) fn setup_pbkdf2(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native PBKDF2: __nativePbkdf2DeriveBits(hashAlgo, password, salt, iterations, lengthBits) -> ArrayBuffer
    let derive_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            let derivation = match derivation(scope, &args) {
                Ok(derivation) => derivation,
                Err(message) => return throw_type_error(scope, &message),
            };

            let mut out = vec![0u8; derivation.length_bytes];
            pbkdf2::derive(
                derivation.algorithm,
                derivation.iterations,
                &derivation.salt,
                &derivation.password,
                &mut out,
            );

            retval.set(create_array_buffer_from_vec(scope, out).into());
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

                    resolve(crypto.subtle.__nativePbkdf2DeriveBits(
                        hashName, baseKey.__keyData, salt, iterations, length
                    ));
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
