//! Compression Streams, the codecs the surface drives.
//!
//! A codec carries state between chunks, so it lives in the context's slots and
//! the surface holds only its id: an id from one worker cannot name another's
//! codec. `finish` and `drop` both release it, or a worker that abandons a
//! stream leaves its codec behind in a context the pool reuses.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;

use crate::v8_helpers::throw_type_error;
use flate2::Compression;
use flate2::write::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::write::GzDecoder;
use flate2::write::GzEncoder;
use flate2::write::ZlibDecoder;
use flate2::write::ZlibEncoder;
use v8;

/// The most one chunk may expand to, in bytes.
const MAX_OUTPUT: usize = 32 * 1024 * 1024;

/// The input handed to a codec at a time, so the output is checked often
/// enough to stop a bomb within a few times `MAX_OUTPUT`.
const INPUT_SLICE: usize = 16 * 1024;

enum Codec {
    ZlibEncode(ZlibEncoder<Vec<u8>>),
    RawEncode(DeflateEncoder<Vec<u8>>),
    GzipEncode(GzEncoder<Vec<u8>>),
    ZlibDecode(ZlibDecoder<Vec<u8>>),
    RawDecode(DeflateDecoder<Vec<u8>>),
    GzipDecode(GzDecoder<Vec<u8>>),
}

impl Codec {
    /// The three formats the standard names, in both directions. `deflate` is
    /// the zlib wrapper and `deflate-raw` the bare stream.
    fn new(format: &str, decompress: bool) -> Option<Self> {
        let level = Compression::default();

        Some(match (format, decompress) {
            ("deflate", false) => Self::ZlibEncode(ZlibEncoder::new(Vec::new(), level)),
            ("deflate-raw", false) => Self::RawEncode(DeflateEncoder::new(Vec::new(), level)),
            ("gzip", false) => Self::GzipEncode(GzEncoder::new(Vec::new(), level)),
            ("deflate", true) => Self::ZlibDecode(ZlibDecoder::new(Vec::new())),
            ("deflate-raw", true) => Self::RawDecode(DeflateDecoder::new(Vec::new())),
            ("gzip", true) => Self::GzipDecode(GzDecoder::new(Vec::new())),
            _ => return None,
        })
    }

    /// Whatever the codec can emit from `bytes`, which for a first small chunk
    /// is usually nothing.
    ///
    /// The output lives in Rust memory, outside the caps of the isolate, so the
    /// input goes in by slices and the call fails once the output passes
    /// `MAX_OUTPUT`: a few kilobytes of deflate expand a thousandfold.
    fn push(&mut self, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        macro_rules! push_into {
            ($writer:expr) => {{
                for slice in bytes.chunks(INPUT_SLICE) {
                    $writer.write_all(slice)?;

                    if $writer.get_ref().len() > MAX_OUTPUT {
                        return Err(std::io::Error::other(format!(
                            "the output passes {MAX_OUTPUT} bytes for one chunk"
                        )));
                    }
                }

                Ok(std::mem::take($writer.get_mut()))
            }};
        }

        match self {
            Self::ZlibEncode(w) => push_into!(w),
            Self::RawEncode(w) => push_into!(w),
            Self::GzipEncode(w) => push_into!(w),
            Self::ZlibDecode(w) => push_into!(w),
            Self::RawDecode(w) => push_into!(w),
            Self::GzipDecode(w) => push_into!(w),
        }
    }

    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::ZlibEncode(w) => w.finish(),
            Self::RawEncode(w) => w.finish(),
            Self::GzipEncode(w) => w.finish(),
            Self::ZlibDecode(w) => w.finish(),
            Self::RawDecode(w) => w.finish(),
            Self::GzipDecode(w) => w.finish(),
        }
    }
}

#[derive(Default)]
pub struct CompressionState {
    codecs: RefCell<HashMap<u32, Codec>>,
    next_id: RefCell<u32>,
}

/// The stream id an op was given, or a TypeError thrown in its place.
fn stream_id(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Option<u32> {
    if !value.is_uint32() {
        throw_type_error(scope, "a compression stream op takes a stream id");
        return None;
    }

    Some(value.uint32_value(scope).unwrap())
}

fn bytes_of(value: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(view) = v8::Local::<v8::TypedArray>::try_from(value) {
        let mut bytes = vec![0u8; view.byte_length()];
        view.copy_contents(&mut bytes);

        return Some(bytes);
    }

    if let Ok(buffer) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = buffer.get_backing_store();

        return Some(
            store[..buffer.byte_length()]
                .iter()
                .map(|c| c.get())
                .collect(),
        );
    }

    None
}

fn as_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> v8::Local<'s, v8::Uint8Array> {
    let len = bytes.len();
    let buffer = crate::v8_helpers::create_array_buffer_from_vec(scope, bytes);

    v8::Uint8Array::new(scope, buffer, 0, len).unwrap()
}

/// The codec table of the running context, created on first use.
fn state(scope: &mut v8::PinScope) -> Rc<CompressionState> {
    if let Some(state) = crate::context_slots::get::<CompressionState>(scope) {
        return state;
    }

    let state = Rc::new(CompressionState::default());
    crate::context_slots::set(scope, state.clone());

    state
}

/// `compressionStart(format, decompress) -> id`
fn compression_start(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let format = match args.get(0).to_string(scope) {
        Some(value) => value.to_rust_string_lossy(scope),
        None => String::new(),
    };

    let decompress = args.get(1).boolean_value(scope);

    // The namespace is reachable from guest code, so the format is checked here
    // and not only in the constructor that the standard makes throw.
    let Some(codec) = Codec::new(&format, decompress) else {
        throw_type_error(
            scope,
            &format!("Unsupported compression format: '{}'", format),
        );
        return;
    };

    let state = state(scope);
    let id = {
        let mut next = state.next_id.borrow_mut();
        *next += 1;
        *next
    };

    state.codecs.borrow_mut().insert(id, codec);
    rv.set_uint32(id);
}

/// `compressionPush(id, bytes) -> bytes`
fn compression_push(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(id) = stream_id(scope, args.get(0)) else {
        return;
    };

    let Some(bytes) = bytes_of(args.get(1)) else {
        throw_type_error(scope, "a compression stream takes a BufferSource");
        return;
    };

    let state = state(scope);
    let mut codecs = state.codecs.borrow_mut();

    let Some(codec) = codecs.get_mut(&id) else {
        throw_type_error(scope, "the compression stream is already closed");
        return;
    };

    match codec.push(&bytes) {
        Ok(out) => {
            drop(codecs);
            rv.set(as_uint8array(scope, out).into());
        }
        Err(err) => {
            codecs.remove(&id);
            drop(codecs);
            throw_type_error(scope, &format!("compression failed: {}", err));
        }
    }
}

/// `compressionFinish(id) -> bytes`, which also releases the codec.
fn compression_finish(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(id) = stream_id(scope, args.get(0)) else {
        return;
    };
    let codec = state(scope).codecs.borrow_mut().remove(&id);

    let Some(codec) = codec else {
        throw_type_error(scope, "the compression stream is already closed");
        return;
    };

    match codec.finish() {
        Ok(out) => rv.set(as_uint8array(scope, out).into()),
        Err(err) => throw_type_error(scope, &format!("compression failed: {}", err)),
    }
}

/// `compressionDrop(id)`, for a stream nobody will finish.
fn compression_drop(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(id) = stream_id(scope, args.get(0)) else {
        return;
    };
    state(scope).codecs.borrow_mut().remove(&id);
}

/// Register the ops the compression streams call.
pub fn setup_compression_natives(scope: &mut v8::PinScope) {
    let start = v8::Function::new(scope, compression_start).unwrap();
    super::native::register_op(scope, "compressionStart", start.into());

    let push = v8::Function::new(scope, compression_push).unwrap();
    super::native::register_op(scope, "compressionPush", push.into());

    let finish = v8::Function::new(scope, compression_finish).unwrap();
    super::native::register_op(scope, "compressionFinish", finish.into());

    let release = v8::Function::new(scope, compression_drop).unwrap();
    super::native::register_op(scope, "compressionDrop", release.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_that_expands_past_the_cap_is_refused() {
        let mut encoder = Codec::new("gzip", false).unwrap();
        let zeros = vec![0u8; 1024 * 1024];
        let mut packed = Vec::new();

        for _ in 0..(MAX_OUTPUT / zeros.len() + 8) {
            packed.extend(encoder.push(&zeros).unwrap());
        }
        packed.extend(encoder.finish().unwrap());
        assert!(packed.len() < 1024 * 1024);

        let mut decoder = Codec::new("gzip", true).unwrap();
        assert!(decoder.push(&packed).is_err());
    }

    #[test]
    fn a_small_chunk_round_trips() {
        let mut encoder = Codec::new("deflate", false).unwrap();
        let mut packed = encoder.push(b"hello hello hello").unwrap();
        packed.extend(encoder.finish().unwrap());

        let mut decoder = Codec::new("deflate", true).unwrap();
        let mut out = decoder.push(&packed).unwrap();
        out.extend(decoder.finish().unwrap());

        assert_eq!(out, b"hello hello hello");
    }
}
