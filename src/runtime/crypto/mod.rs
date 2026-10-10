mod aes;
mod digest;
mod ecdsa;
mod hmac;
mod pbkdf2;
mod random;
mod rsa;

/// Copies a Uint8Array argument out of the V8 heap, or names the argument
/// that is not one.
pub(super) fn uint8_array_arg(
    op: &str,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Result<Vec<u8>, String> {
    let Ok(array) = v8::Local::<v8::Uint8Array>::try_from(args.get(index)) else {
        return Err(format!("{op}: argument {index} is not a Uint8Array"));
    };
    let mut bytes = vec![0u8; array.byte_length()];
    array.copy_contents(&mut bytes);

    Ok(bytes)
}

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

    // crypto.getRandomValues + crypto.randomUUID
    random::setup_get_random_values(scope, crypto_obj);
    random::setup_random_uuid(scope, crypto_obj);

    // The native ops, on crypto.subtle
    digest::setup_digest(scope, subtle_obj);
    hmac::setup_hmac(scope, subtle_obj);
    ecdsa::setup_ecdsa(scope, subtle_obj);
    rsa::setup_rsa(scope, subtle_obj);
    pbkdf2::setup_pbkdf2(scope, subtle_obj);
    aes::setup_aes(scope, subtle_obj);

    setup_subtle(scope);
}

/// Installs the crypto classes and every subtle wrapper from one closure.
/// The key material lives in a WeakMap only that closure reaches, so a guest
/// can neither read the bytes of a key nor make a CryptoKey of its own.
///
/// The wrappers chain: HMAC installs importKey, sign and verify, and ECDSA,
/// RSA, PBKDF2 and AES each wrap what the one before installed.
fn setup_subtle(scope: &mut v8::PinScope) {
    let wrappers = [
        KEYS_JS,
        digest::JS,
        hmac::JS,
        ecdsa::JS,
        rsa::JS,
        pbkdf2::JS,
        aes::JS,
    ]
    .join("\n");
    let code = format!("(function () {{\n{wrappers}\n}})();");

    let code_str = v8::String::new(scope, &code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}

/// The classes, the private key store and the helpers the wrappers share.
const KEYS_JS: &str = r#"
        globalThis.Crypto = class Crypto {
            constructor() {
                throw new TypeError('Illegal constructor');
            }
        };

        globalThis.SubtleCrypto = class SubtleCrypto {
            constructor() {
                throw new TypeError('Illegal constructor');
            }
        };

        // crypto and crypto.subtle are native plain objects, so instanceof only
        // answers once they carry the matching prototype.
        Object.setPrototypeOf(crypto, Crypto.prototype);
        Object.setPrototypeOf(crypto.subtle, SubtleCrypto.prototype);

        // What a CryptoKey is, out of the guest's reach: its attributes and its bytes
        const __material = new WeakMap();
        const __token = Symbol('CryptoKey');

        const __record = (key) => {
            const record = __material.get(key);
            if (!record) {
                throw new TypeError('Not a CryptoKey');
            }
            return record;
        };

        const __freeze = (value) => {
            if (value !== null && typeof value === 'object') {
                for (const inner of Object.values(value)) {
                    __freeze(inner);
                }
                Object.freeze(value);
            }
            return value;
        };

        // The attributes are getters on the prototype, as the spec has them: a key
        // has no own properties, so it cannot be enumerated or serialized into
        // its bytes, and nothing can be written over them.
        globalThis.CryptoKey = class CryptoKey {
            constructor(token, record) {
                if (token !== __token) {
                    throw new TypeError('Illegal constructor');
                }
                __material.set(this, record);
            }
            get type() { return __record(this).type; }
            get extractable() { return __record(this).extractable; }
            get algorithm() { return __record(this).algorithm; }
            get usages() { return __record(this).usages; }
        };

        // Shared by every importKey and generateKey: data is the key's bytes,
        // publicData those of the public half a private key carries
        const __createCryptoKey = (type, extractable, algorithm, usages, data, publicData) =>
            new CryptoKey(__token, {
                type,
                extractable: Boolean(extractable),
                algorithm: __freeze(algorithm),
                usages: Object.freeze([...usages]),
                data,
                publicData,
            });

        const __isCryptoKey = (key) => __material.has(key);
        const __keyData = (key) => __record(key).data;
        const __publicKeyData = (key) => __record(key).publicData;

        // A BufferSource as the bytes it holds: the ArrayBuffer, or the part of
        // one a view points at
        const __bufferSource = (value) => {
            if (value instanceof ArrayBuffer) {
                return new Uint8Array(value);
            }
            if (ArrayBuffer.isView(value)) {
                return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
            }
            throw new TypeError('Expected an ArrayBuffer or a view');
        };

        // Key material is copied out of the caller's buffer: a key keeps the
        // bytes it was imported with, whatever the caller writes there next
        const __copyBufferSource = (value) => __bufferSource(value).slice();

        const __domException = (name, message) => typeof DOMException === 'function'
            ? new DOMException(message, name)
            : Object.assign(new Error(message), { name });

        // What every op checks of its key before it touches the bytes: a CryptoKey
        // of the algorithm the op is for, imported for this use
        const __checkKey = (key, algoName, usage) => {
            if (!__isCryptoKey(key)) {
                throw new TypeError('The key is not a CryptoKey');
            }
            if (key.algorithm.name !== algoName) {
                throw __domException('InvalidAccessError',
                    'The key is for ' + key.algorithm.name + ', not ' + algoName);
            }
            if (!key.usages.includes(usage)) {
                throw __domException('InvalidAccessError', 'The key does not allow ' + usage);
            }
        };

        // What importKey and generateKey check of the usages they are given
        const __checkUsages = (usages, allowed) => {
            for (const usage of usages) {
                if (!allowed.includes(usage)) {
                    throw __domException('SyntaxError', 'The usage ' + usage + ' is not one of ' + allowed.join(', '));
                }
            }
        };
"#;
