use ring::aead;
use v8;

/// Copies a Uint8Array argument out of the V8 heap.
fn bytes_arg(args: &v8::FunctionCallbackArguments, index: i32) -> Option<Vec<u8>> {
    let array = v8::Local::<v8::Uint8Array>::try_from(args.get(index)).ok()?;
    let mut bytes = vec![0u8; array.byte_length()];
    array.copy_contents(&mut bytes);

    Some(bytes)
}

/// Ring has no AES-192, so WebCrypto's middle key size is rejected here.
fn gcm_key(key: &[u8]) -> Option<aead::LessSafeKey> {
    let algorithm = match key.len() {
        16 => &aead::AES_128_GCM,
        32 => &aead::AES_256_GCM,
        _ => return None,
    };

    Some(aead::LessSafeKey::new(
        aead::UnboundKey::new(algorithm, key).ok()?,
    ))
}

/// Seals (key, nonce, plaintext, aad) into ciphertext with the tag appended,
/// which is the layout WebCrypto hands back from encrypt().
fn seal(args: &v8::FunctionCallbackArguments) -> Option<Vec<u8>> {
    let key = gcm_key(&bytes_arg(args, 0)?)?;
    let nonce = aead::Nonce::try_assume_unique_for_key(&bytes_arg(args, 1)?).ok()?;
    let aad = bytes_arg(args, 3)?;
    let mut data = bytes_arg(args, 2)?;

    key.seal_in_place_append_tag(nonce, aead::Aad::from(&aad), &mut data)
        .ok()?;

    Some(data)
}

fn open(args: &v8::FunctionCallbackArguments) -> Option<Vec<u8>> {
    let key = gcm_key(&bytes_arg(args, 0)?)?;
    let nonce = aead::Nonce::try_assume_unique_for_key(&bytes_arg(args, 1)?).ok()?;
    let aad = bytes_arg(args, 3)?;
    let mut data = bytes_arg(args, 2)?;

    let plain = key
        .open_in_place(nonce, aead::Aad::from(&aad), &mut data)
        .ok()?;

    Some(plain.to_vec())
}

pub(super) fn setup_aes(scope: &mut v8::PinScope, subtle_obj: v8::Local<v8::Object>) {
    // Native AES-GCM: __nativeAesGcmSeal/Open(key, iv, data, aad) -> ArrayBuffer
    let seal_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            match seal(&args) {
                Some(out) => {
                    let buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, out);
                    retval.set(buffer.into());
                }
                None => retval.set(v8::undefined(scope).into()),
            }
        },
    )
    .unwrap();

    let seal_key = v8::String::new(scope, "__nativeAesGcmSeal").unwrap();
    subtle_obj.set(scope, seal_key.into(), seal_fn.into());

    let open_fn = v8::Function::new(
        scope,
        |scope: &mut v8::PinScope,
         args: v8::FunctionCallbackArguments,
         mut retval: v8::ReturnValue| {
            match open(&args) {
                Some(out) => {
                    let buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, out);
                    retval.set(buffer.into());
                }
                None => retval.set(v8::undefined(scope).into()),
            }
        },
    )
    .unwrap();

    let open_key = v8::String::new(scope, "__nativeAesGcmOpen").unwrap();
    subtle_obj.set(scope, open_key.into(), open_fn.into());

    // JS wrappers: AES-GCM generateKey/importKey, plus exportKey and encrypt/decrypt
    let code = r#"
        const __aesBytes = (value) => {
            if (value instanceof ArrayBuffer) {
                return new Uint8Array(value);
            }

            if (ArrayBuffer.isView(value)) {
                return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
            }

            throw new TypeError('Expected an ArrayBuffer or a view');
        };

        // The nonce and the extra authenticated data ride on the algorithm object.
        const __aesGcmArgs = (algorithm, key, data) => {
            if (algorithm.name !== 'AES-GCM' || key.algorithm.name !== 'AES-GCM') {
                throw new Error('Only AES-GCM is supported for encrypt and decrypt');
            }

            if (algorithm.tagLength !== undefined && algorithm.tagLength !== 128) {
                throw new Error('Only a 128 bit AES-GCM tag is supported');
            }

            const aad = algorithm.additionalData === undefined
                ? new Uint8Array(0)
                : __aesBytes(algorithm.additionalData);

            return [key.__keyData, __aesBytes(algorithm.iv), __aesBytes(data), aad];
        };

        const __generateKeyFallback = crypto.subtle.generateKey;

        crypto.subtle.generateKey = function(algorithm, extractable, keyUsages) {
            const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

            if (algoName !== 'AES-GCM') {
                return __generateKeyFallback.call(crypto.subtle, algorithm, extractable, keyUsages);
            }

            return new Promise((resolve, reject) => {
                try {
                    const length = algorithm.length === undefined ? 256 : algorithm.length;

                    if (length !== 128 && length !== 256) {
                        throw new Error('AES-GCM supports 128 and 256 bit keys');
                    }

                    resolve(__createCryptoKey(
                        'secret', extractable,
                        { name: 'AES-GCM', length },
                        keyUsages, crypto.getRandomValues(new Uint8Array(length / 8))
                    ));
                } catch (e) {
                    reject(e);
                }
            });
        };

        const __importKeyFallback = crypto.subtle.importKey;

        crypto.subtle.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
            const algoName = typeof algorithm === 'string' ? algorithm : algorithm.name;

            if (algoName !== 'AES-GCM') {
                return __importKeyFallback.call(crypto.subtle, format, keyData, algorithm, extractable, keyUsages);
            }

            return new Promise((resolve, reject) => {
                try {
                    if (format !== 'raw') {
                        throw new Error('Only "raw" format is supported for AES-GCM');
                    }

                    const bytes = __aesBytes(keyData);

                    if (bytes.byteLength !== 16 && bytes.byteLength !== 32) {
                        throw new Error('AES-GCM supports 128 and 256 bit keys');
                    }

                    resolve(__createCryptoKey(
                        'secret', extractable,
                        { name: 'AES-GCM', length: bytes.byteLength * 8 },
                        keyUsages, bytes
                    ));
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.exportKey = function(format, key) {
            return new Promise((resolve, reject) => {
                try {
                    if (format !== 'raw') {
                        throw new Error('Only "raw" format is supported for exportKey');
                    }

                    if (!key.extractable) {
                        throw new Error('Key is not extractable');
                    }

                    const bytes = __aesBytes(key.__keyData);

                    resolve(bytes.slice().buffer);
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.encrypt = function(algorithm, key, data) {
            return new Promise((resolve, reject) => {
                try {
                    const result = crypto.subtle.__nativeAesGcmSeal(...__aesGcmArgs(algorithm, key, data));

                    if (!result) {
                        throw new Error('AES-GCM encrypt failed');
                    }

                    resolve(result);
                } catch (e) {
                    reject(e);
                }
            });
        };

        crypto.subtle.decrypt = function(algorithm, key, data) {
            return new Promise((resolve, reject) => {
                try {
                    const result = crypto.subtle.__nativeAesGcmOpen(...__aesGcmArgs(algorithm, key, data));

                    if (!result) {
                        throw new Error('AES-GCM decrypt failed');
                    }

                    resolve(result);
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
