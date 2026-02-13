mod digest;
mod ecdsa;
mod hmac;
mod pbkdf2;
mod random;
mod rsa;

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

    // Define CryptoKey class (must be before importKey implementations)
    setup_crypto_key(scope);

    // crypto.getRandomValues + crypto.randomUUID
    random::setup_get_random_values(scope, crypto_obj);
    random::setup_random_uuid(scope, crypto_obj);

    // crypto.subtle.digest
    digest::setup_digest(scope, subtle_obj);

    // crypto.subtle.sign/verify/importKey — chained: HMAC -> ECDSA -> RSA -> PBKDF2
    hmac::setup_hmac(scope, subtle_obj);
    ecdsa::setup_ecdsa(scope, subtle_obj);
    rsa::setup_rsa(scope, subtle_obj);
    pbkdf2::setup_pbkdf2(scope, subtle_obj);
}

/// Define globalThis.CryptoKey class and a helper to create instances from importKey.
fn setup_crypto_key(scope: &mut v8::PinScope) {
    let code = r#"
        globalThis.CryptoKey = class CryptoKey {
            constructor(type, extractable, algorithm, usages, keyData) {
                this.type = type;
                this.extractable = extractable;
                this.algorithm = algorithm;
                this.usages = Object.freeze([...usages]);
                this.__keyData = keyData;
            }
        };

        // Helper used by all importKey implementations
        globalThis.__createCryptoKey = function(type, extractable, algorithm, usages, keyData) {
            return new CryptoKey(type, extractable, algorithm, usages, keyData);
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
