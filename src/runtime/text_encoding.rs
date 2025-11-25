use v8;

/// Setup TextEncoder and TextDecoder APIs
/// These are essential for converting between strings and bytes
pub fn setup_text_encoding(scope: &mut v8::PinScope) {
    let code = r#"
        // TextEncoder - encode strings to UTF-8 bytes
        globalThis.TextEncoder = class TextEncoder {
            constructor() {
                this.encoding = 'utf-8';
            }

            encode(input) {
                const str = String(input || '');
                const bytes = [];

                // UTF-8 encoding with proper surrogate pair handling
                for (let i = 0; i < str.length; i++) {
                    let code = str.codePointAt(i);

                    // Skip low surrogate (already processed with high surrogate)
                    if (code > 0xFFFF) i++;

                    if (code < 0x80) {
                        bytes.push(code);
                    } else if (code < 0x800) {
                        bytes.push(0xC0 | (code >> 6));
                        bytes.push(0x80 | (code & 0x3F));
                    } else if (code < 0x10000) {
                        bytes.push(0xE0 | (code >> 12));
                        bytes.push(0x80 | ((code >> 6) & 0x3F));
                        bytes.push(0x80 | (code & 0x3F));
                    } else {
                        bytes.push(0xF0 | (code >> 18));
                        bytes.push(0x80 | ((code >> 12) & 0x3F));
                        bytes.push(0x80 | ((code >> 6) & 0x3F));
                        bytes.push(0x80 | (code & 0x3F));
                    }
                }

                return new Uint8Array(bytes);
            }
        };

        // TextDecoder - decode UTF-8 bytes to strings
        globalThis.TextDecoder = class TextDecoder {
            constructor(encoding = 'utf-8') {
                this.encoding = encoding.toLowerCase();
                if (this.encoding !== 'utf-8' && this.encoding !== 'utf8') {
                    throw new RangeError('Only UTF-8 encoding is supported');
                }
            }

            decode(input) {
                if (!input) return '';

                // Convert to Uint8Array if needed
                const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
                const chars = [];

                // Simple UTF-8 decoding
                let i = 0;
                while (i < bytes.length) {
                    const byte1 = bytes[i++];

                    if (byte1 < 0x80) {
                        // 1-byte character (ASCII)
                        chars.push(String.fromCharCode(byte1));
                    } else if ((byte1 & 0xE0) === 0xC0) {
                        // 2-byte character
                        const byte2 = bytes[i++];
                        const code = ((byte1 & 0x1F) << 6) | (byte2 & 0x3F);
                        chars.push(String.fromCharCode(code));
                    } else if ((byte1 & 0xF0) === 0xE0) {
                        // 3-byte character
                        const byte2 = bytes[i++];
                        const byte3 = bytes[i++];
                        const code = ((byte1 & 0x0F) << 12) | ((byte2 & 0x3F) << 6) | (byte3 & 0x3F);
                        chars.push(String.fromCharCode(code));
                    } else if ((byte1 & 0xF8) === 0xF0) {
                        // 4-byte character (emojis, etc.)
                        const byte2 = bytes[i++];
                        const byte3 = bytes[i++];
                        const byte4 = bytes[i++];
                        const code = ((byte1 & 0x07) << 18) | ((byte2 & 0x3F) << 12) |
                                    ((byte3 & 0x3F) << 6) | (byte4 & 0x3F);
                        chars.push(String.fromCodePoint(code));
                    } else {
                        // Invalid UTF-8, skip
                        chars.push('\uFFFD'); // Replacement character
                    }
                }

                return chars.join('');
            }
        };
    "#;

    let code_str = v8::String::new(scope, code).unwrap();
    let script = v8::Script::compile(scope, code_str, None).unwrap();
    script.run(scope).unwrap();
}
