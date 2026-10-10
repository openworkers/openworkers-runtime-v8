use ring::pbkdf2;
use std::num::NonZeroU32;
use v8;

use super::uint8_array_arg;
use crate::v8_helpers::{create_array_buffer_from_vec, throw_type_error};

/// Longest output `deriveBits` hands back. The spec sets no limit, but the
/// output is allocated before the derivation runs, and 2**53 bits must not
/// reserve a petabyte.
const MAX_LENGTH_BYTES: usize = 64 * 1024;

/// The byte count a `deriveBits` length asks for, or why it cannot: the spec
/// wants a whole, positive multiple of 8 bits, and this runtime a bounded one.
fn length_bytes(bits: f64) -> Result<usize, String> {
    if !bits.is_finite() || bits.fract() != 0.0 {
        return Err("PBKDF2: the length is not a whole number of bits".into());
    }
    if bits <= 0.0 {
        return Err("PBKDF2: the length must be at least 8 bits".into());
    }
    if bits % 8.0 != 0.0 {
        return Err("PBKDF2: the length is not a multiple of 8 bits".into());
    }
    if bits > (MAX_LENGTH_BYTES * 8) as f64 {
        return Err(format!(
            "PBKDF2: the length is above {} bits",
            MAX_LENGTH_BYTES * 8
        ));
    }

    Ok(bits as usize / 8)
}

/// Most iterations one derivation may run. Each is a synchronous HMAC on
/// the isolate thread, out of reach of the CPU limit, so the count has to be
/// bounded here: this is a generous bound, with 600 000 the OWASP advice.
const MAX_ITERATIONS: u32 = 10_000_000;

/// The iteration count a derivation asks for, or why it cannot run it: the
/// spec wants a positive integer, and this runtime a bounded one.
fn iteration_count(count: f64) -> Result<NonZeroU32, String> {
    if !count.is_finite() || count.fract() != 0.0 {
        return Err("PBKDF2: the iteration count is not a whole number".into());
    }
    if count < 1.0 {
        return Err("PBKDF2: the iteration count must be at least 1".into());
    }
    if count > MAX_ITERATIONS as f64 {
        return Err(format!(
            "PBKDF2: the iteration count is above {MAX_ITERATIONS}"
        ));
    }

    Ok(NonZeroU32::new(count as u32).unwrap())
}

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
    let iterations = iteration_count(iterations.number_value(scope).unwrap())?;

    let length = args.get(4);
    if !length.is_number() {
        return Err("PBKDF2: the length is not a number".into());
    }
    let length_bytes = length_bytes(length.number_value(scope).unwrap())?;

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
}

/// The PBKDF2 importKey and deriveBits wrappers, extending the RSA ones.
/// Installed by mod.rs.
pub(super) const JS: &str = r#"
        const __rsaImportKey = crypto.subtle.importKey;

        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            return new Promise((resolve, reject) => {
                try {
                    const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

                    if (algoName === 'PBKDF2') {
                        if (format !== 'raw') {
                            reject(__domException('NotSupportedError', 'Only "raw" format is supported for PBKDF2'));
                            return;
                        }

                        __checkUsages(keyUsages, ['deriveKey', 'deriveBits']);

                        const keyBytes = typeof keyData === 'string'
                            ? new TextEncoder().encode(keyData)
                            : __copyBufferSource(keyData);

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
                        reject(__domException('NotSupportedError', 'Only PBKDF2 algorithm is supported for deriveBits'));
                        return;
                    }

                    __checkKey(baseKey, 'PBKDF2', 'deriveBits');

                    const salt = __bufferSource(algorithm.salt);

                    // A count the derivation cannot run is the native op's TypeError;
                    // the spec names zero an OperationError
                    const iterations = algorithm.iterations;
                    if (typeof iterations === 'number' && iterations < 1) {
                        reject(__domException('OperationError', 'Iterations must be a positive number'));
                        return;
                    }

                    const hashName = typeof algorithm.hash === 'string'
                        ? algorithm.hash
                        : algorithm.hash.name;

                    resolve(crypto.subtle.__nativePbkdf2DeriveBits(
                        hashName, __keyData(baseKey), salt, iterations, length
                    ));
                } catch (e) {
                    reject(e);
                }
            });
        };
"#;
