use v8;

/// Native TextEncoder.encode: string → Uint8Array (UTF-8)
///
/// V8's `to_rust_string_lossy` replaces lone surrogates with U+FFFD,
/// giving us valid UTF-8 by default. No manual surrogate handling needed.
fn text_encode(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let input = if let Some(arg) = args.get(0).to_string(scope) {
        arg.to_rust_string_lossy(scope)
    } else {
        String::new()
    };

    let bytes = input.as_bytes();
    let len = bytes.len();
    let backing_store = v8::ArrayBuffer::new_backing_store_from_vec(bytes.to_vec());
    let buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store.into());
    let array = v8::Uint8Array::new(scope, buffer, 0, len).unwrap();

    rv.set(array.into());
}

/// Native TextDecoder.decode: (Uint8Array, encoding) → string
///
/// Uses `encoding_rs` for spec-compliant decoding with proper U+FFFD
/// replacement for invalid sequences. Supports all Encoding Standard labels.
fn text_decode(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // arg0: Uint8Array or ArrayBuffer
    // arg1: encoding label (string)
    // arg2: fatal (bool) — if true, throw on invalid sequences instead of replacing

    let input = args.get(0);

    if input.is_null_or_undefined() {
        let empty = v8::String::new(scope, "").unwrap();
        rv.set(empty.into());
        return;
    }

    // Extract bytes from TypedArray or ArrayBuffer
    let bytes = if let Ok(typed_array) = v8::Local::<v8::TypedArray>::try_from(input) {
        let len = typed_array.byte_length();
        let mut buf = vec![0u8; len];
        typed_array.copy_contents(&mut buf);
        buf
    } else if let Ok(array_buffer) = v8::Local::<v8::ArrayBuffer>::try_from(input) {
        let len = array_buffer.byte_length();
        let store = array_buffer.get_backing_store();
        store[..len].iter().map(|c| c.get()).collect()
    } else {
        let msg =
            v8::String::new(scope, "TextDecoder.decode: input must be a BufferSource").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    };

    // Get encoding label
    let encoding_label = if args.length() > 1 && !args.get(1).is_undefined() {
        if let Some(s) = args.get(1).to_string(scope) {
            s.to_rust_string_lossy(scope)
        } else {
            "utf-8".to_string()
        }
    } else {
        "utf-8".to_string()
    };

    // Get fatal flag
    let fatal = if args.length() > 2 {
        args.get(2).boolean_value(scope)
    } else {
        false
    };

    // Look up encoding
    let encoding = match encoding_rs::Encoding::for_label(encoding_label.as_bytes()) {
        Some(enc) => enc,
        None => {
            let msg = v8::String::new(
                scope,
                &format!(
                    "TextDecoder: '{}' is not a supported encoding",
                    encoding_label
                ),
            )
            .unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let result = if fatal {
        // Fatal mode: return error on invalid sequences
        let (decoded, had_errors) = encoding.decode_without_bom_handling(&bytes);

        if had_errors {
            let msg = v8::String::new(scope, "TextDecoder: invalid byte sequence").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }

        decoded.into_owned()
    } else {
        // Replacement mode: replace invalid sequences with U+FFFD
        let (decoded, _) = encoding.decode_without_bom_handling(&bytes);
        decoded.into_owned()
    };

    let v8_str = v8::String::new(scope, &result).unwrap();
    rv.set(v8_str.into());
}

/// Register native __text_encode and __text_decode functions on globalThis.
///
/// Must be called at runtime (not in snapshot) because native functions
/// can't be serialized by the V8 snapshot creator.
pub fn setup_text_encoding_natives(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    let encode_fn = v8::Function::new(scope, text_encode).unwrap();
    let key = v8::String::new(scope, "__text_encode").unwrap();
    global.set(scope, key.into(), encode_fn.into());

    let decode_fn = v8::Function::new(scope, text_decode).unwrap();
    let key = v8::String::new(scope, "__text_decode").unwrap();
    global.set(scope, key.into(), decode_fn.into());
}

/// JS source for TextEncoder/TextDecoder class wrappers.
///
/// These classes call `__text_encode` / `__text_decode` which must be
/// registered separately via `setup_text_encoding_natives`.
pub const TEXT_ENCODING_JS: &str = r#"
    globalThis.TextEncoder = class TextEncoder {
        constructor() {
            this.encoding = 'utf-8';
        }

        encode(input) {
            return __text_encode(input);
        }

        encodeInto(source, destination) {
            const encoded = __text_encode(source);
            const len = Math.min(encoded.length, destination.length);
            destination.set(encoded.subarray(0, len));
            return { read: len, written: len };
        }
    };

    globalThis.TextDecoder = class TextDecoder {
        #encoding;
        #fatal;
        #ignoreBOM;

        constructor(label = 'utf-8', options = {}) {
            this.#encoding = label.toLowerCase().trim();
            this.#fatal = !!options.fatal;
            this.#ignoreBOM = !!options.ignoreBOM;

            // Validate encoding by attempting a decode
            // encoding_rs will reject invalid labels
            try {
                __text_decode(new Uint8Array(0), this.#encoding, this.#fatal);
            } catch (e) {
                if (e instanceof RangeError) throw e;
                throw e;
            }
        }

        get encoding() {
            // encoding_rs normalizes labels, but we store the canonical name
            // The spec says to return the lowercase name
            return this.#encoding;
        }

        get fatal() {
            return this.#fatal;
        }

        get ignoreBOM() {
            return this.#ignoreBOM;
        }

        decode(input, options) {
            if (!input) return '';

            const bytes = input instanceof Uint8Array
                ? input
                : ArrayBuffer.isView(input)
                    ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
                    : input instanceof ArrayBuffer
                        ? new Uint8Array(input)
                        : input;

            return __text_decode(bytes, this.#encoding, this.#fatal);
        }
    };
"#;

/// Setup TextEncoder and TextDecoder JS class wrappers.
///
/// Executes the JS source that defines the classes.
/// Natives must be registered separately via `setup_text_encoding_natives`.
pub fn setup_text_encoding_classes(scope: &mut v8::PinScope) {
    let code = v8::String::new(scope, TEXT_ENCODING_JS).unwrap();
    let script = v8::Script::compile(scope, code, None).unwrap();
    script.run(scope);
}
