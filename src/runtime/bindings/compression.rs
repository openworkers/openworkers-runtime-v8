//! Compression Streams, the codecs the surface drives.
//!
//! A codec carries state between chunks, so it lives in the context's slots and
//! the surface holds only its id: an id from one worker cannot name another's
//! codec. `finish` and `drop` both release it, or a worker that abandons a
//! stream leaves its codec behind in a context the pool reuses.
//!
//! Output comes back in pieces of at most [`OUTPUT_BOUND`] bytes. A chunk that
//! inflates a thousandfold reaches the guest a piece at a time, and the input
//! the codec has not consumed yet waits inside it: the host never holds more
//! than one piece and one chunk of input for a stream. An empty piece means the
//! input so far is consumed (`push`), or the stream is complete (`finish`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::v8_helpers::{throw_error, throw_type_error};
use flate2::{Compress, Compression, Crc, Decompress, FlushCompress, FlushDecompress, Status};
use v8;

/// The most bytes one push or finish hands back.
pub const OUTPUT_BOUND: usize = 64 * 1024;

/// The most codecs a context holds at once. Each keeps a window and whatever
/// input it has not consumed, and a guest that opens streams it never ends
/// would otherwise hold them all.
pub const MAX_CODECS: usize = 64;

/// The surface's `CompressionStream` and `DecompressionStream`, over the
/// bounded ops: the one the shared surface ships calls `push` and `finish`
/// once per chunk, which fits a codec that answers with everything at once.
///
/// Evaluated after the shared surface, so these definitions win.
pub(super) const SURFACE: &str = r#"
(() => {
    'use strict';

    const FORMATS = ['deflate', 'deflate-raw', 'gzip'];
    const NOTHING = new Uint8Array(0);

    const transformFor = (format, decompress) => {
        if (!FORMATS.includes(format)) {
            throw new TypeError("Unsupported compression format: '" + format + "'");
        }

        let id = globalThis.__ow.compressionStart(format, decompress);

        return new globalThis.TransformStream({
            transform(chunk, controller) {
                // The host answers a bounded piece at a time, and an empty one
                // once the chunk is consumed.
                try {
                    let piece = globalThis.__ow.compressionPush(id, chunk);

                    while (piece.byteLength > 0) {
                        controller.enqueue(piece);
                        piece = globalThis.__ow.compressionPush(id, NOTHING);
                    }
                } catch (e) {
                    // Corrupt input fails the readable side too, so a reader
                    // learns of it, not only the writer.
                    id = null;
                    controller.error(e);
                    throw e;
                }
            },

            flush(controller) {
                // Likewise, and the empty piece also releases the codec.
                try {
                    let piece = globalThis.__ow.compressionFinish(id);

                    while (piece.byteLength > 0) {
                        controller.enqueue(piece);
                        piece = globalThis.__ow.compressionFinish(id);
                    }
                } catch (e) {
                    id = null;
                    controller.error(e);
                    throw e;
                }

                id = null;
            },

            cancel() {
                if (id !== null) {
                    globalThis.__ow.compressionDrop(id);
                    id = null;
                }
            },
        });
    };

    globalThis.CompressionStream = class CompressionStream {
        constructor(format) {
            const transform = transformFor(String(format), false);

            this.readable = transform.readable;
            this.writable = transform.writable;
        }
    };

    globalThis.DecompressionStream = class DecompressionStream {
        constructor(format) {
            const transform = transformFor(String(format), true);

            this.readable = transform.readable;
            this.writable = transform.writable;
        }
    };
})();
"#;

/// The three formats the standard names. `deflate` is the zlib wrapper,
/// `deflate-raw` the bare stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Zlib,
    Raw,
    Gzip,
}

/// The gzip header, read as it arrives. flate2's rust backend has no gzip
/// mode of its own, so the framing around the raw deflate stream is ours.
struct GzipHeader {
    state: GzipHeaderState,
    /// The bytes of a fixed-size part gathered so far.
    buf: Vec<u8>,
    flags: u8,
    /// Over every header byte, for the optional header checksum.
    crc: Crc,
}

#[derive(Clone, Copy)]
enum GzipHeaderState {
    Fixed,
    ExtraLen,
    Extra(usize),
    Name,
    Comment,
    HeaderCrc,
    Done,
}

const GZIP_FHCRC: u8 = 1 << 1;
const GZIP_FEXTRA: u8 = 1 << 2;
const GZIP_FNAME: u8 = 1 << 3;
const GZIP_FCOMMENT: u8 = 1 << 4;

/// The optional fields, in the order the format lays them out.
const GZIP_OPTIONAL: [(u8, GzipHeaderState); 4] = [
    (GZIP_FEXTRA, GzipHeaderState::ExtraLen),
    (GZIP_FNAME, GzipHeaderState::Name),
    (GZIP_FCOMMENT, GzipHeaderState::Comment),
    (GZIP_FHCRC, GzipHeaderState::HeaderCrc),
];

/// The header the encoder writes: deflate, no mtime, unknown OS.
const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];

impl GzipHeader {
    fn new() -> Self {
        Self {
            state: GzipHeaderState::Fixed,
            buf: Vec::with_capacity(10),
            flags: 0,
            crc: Crc::new(),
        }
    }

    fn done(&self) -> bool {
        matches!(self.state, GzipHeaderState::Done)
    }

    /// The first optional field from `from` on that the flags announce.
    fn advance(&mut self, from: usize) {
        self.state = GZIP_OPTIONAL[from..]
            .iter()
            .find(|(flag, _)| self.flags & flag != 0)
            .map(|(_, state)| *state)
            .unwrap_or(GzipHeaderState::Done);
    }

    /// Reads as much of the header as `input` holds; how many bytes it took.
    fn feed(&mut self, input: &[u8]) -> Result<usize, String> {
        let mut pos = 0;

        while pos < input.len() {
            let rest = &input[pos..];

            match self.state {
                GzipHeaderState::Done => break,
                GzipHeaderState::Fixed => {
                    let take = (10 - self.buf.len()).min(rest.len());
                    self.buf.extend_from_slice(&rest[..take]);
                    pos += take;

                    if self.buf.len() < 10 {
                        break;
                    }

                    if self.buf[..2] != [0x1f, 0x8b] {
                        return Err("the input is not a gzip stream".into());
                    }
                    if self.buf[2] != 8 {
                        return Err("the gzip stream uses an unknown compression method".into());
                    }
                    self.flags = self.buf[3];
                    if self.flags & 0xe0 != 0 {
                        return Err("the gzip header sets reserved flags".into());
                    }

                    self.crc.update(&self.buf);
                    self.buf.clear();
                    self.advance(0);
                }
                GzipHeaderState::ExtraLen => {
                    let take = (2 - self.buf.len()).min(rest.len());
                    self.buf.extend_from_slice(&rest[..take]);
                    pos += take;

                    if self.buf.len() < 2 {
                        break;
                    }

                    let len = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
                    self.crc.update(&self.buf);
                    self.buf.clear();

                    if len == 0 {
                        self.advance(1);
                    } else {
                        self.state = GzipHeaderState::Extra(len);
                    }
                }
                GzipHeaderState::Extra(left) => {
                    let take = left.min(rest.len());
                    self.crc.update(&rest[..take]);
                    pos += take;

                    if take == left {
                        self.advance(1);
                    } else {
                        self.state = GzipHeaderState::Extra(left - take);
                    }
                }
                GzipHeaderState::Name | GzipHeaderState::Comment => {
                    let end = rest.iter().position(|b| *b == 0);
                    let take = end.map_or(rest.len(), |end| end + 1);
                    self.crc.update(&rest[..take]);
                    pos += take;

                    if end.is_none() {
                        break;
                    }

                    let from = match self.state {
                        GzipHeaderState::Name => 2,
                        _ => 3,
                    };
                    self.advance(from);
                }
                GzipHeaderState::HeaderCrc => {
                    let take = (2 - self.buf.len()).min(rest.len());
                    self.buf.extend_from_slice(&rest[..take]);
                    pos += take;

                    if self.buf.len() < 2 {
                        break;
                    }

                    let expected = u16::from_le_bytes([self.buf[0], self.buf[1]]);
                    if expected != self.crc.sum() as u16 {
                        return Err("the gzip header checksum does not match".into());
                    }

                    self.buf.clear();
                    self.state = GzipHeaderState::Done;
                }
            }
        }

        Ok(pos)
    }
}

enum Engine {
    Encode(Compress),
    Decode(Decompress),
}

/// Where the stream stands. An encoder goes from `Body` to `Done`; a decoder
/// passes `Header` and `Trailer` too when its format frames the stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Header,
    Body,
    Trailer,
    Done,
}

struct Codec {
    engine: Engine,
    format: Format,
    phase: Phase,
    /// Input not consumed yet, from `pending_pos` on.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Framing bytes that did not fit in the piece they were made for.
    held: Vec<u8>,
    header: GzipHeader,
    /// The gzip trailer bytes gathered so far.
    trailer: Vec<u8>,
    /// Over the uncompressed bytes, for the gzip trailer.
    crc: Crc,
}

impl Codec {
    /// The three formats the standard names, in both directions.
    fn new(format: &str, decompress: bool) -> Option<Self> {
        let format = match format {
            "deflate" => Format::Zlib,
            "deflate-raw" => Format::Raw,
            "gzip" => Format::Gzip,
            _ => return None,
        };
        let zlib_header = format == Format::Zlib;
        let gzip = format == Format::Gzip;

        let engine = if decompress {
            Engine::Decode(Decompress::new(zlib_header))
        } else {
            Engine::Encode(Compress::new(Compression::default(), zlib_header))
        };

        Some(Self {
            engine,
            format,
            phase: if decompress && gzip {
                Phase::Header
            } else {
                Phase::Body
            },
            pending: Vec::new(),
            pending_pos: 0,
            held: if decompress || !gzip {
                Vec::new()
            } else {
                GZIP_HEADER.to_vec()
            },
            header: GzipHeader::new(),
            trailer: Vec::new(),
            crc: Crc::new(),
        })
    }

    fn append_input(&mut self, bytes: &[u8]) {
        if self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        } else if self.pending_pos > 0 {
            self.pending.drain(..self.pending_pos);
            self.pending_pos = 0;
        }

        self.pending.extend_from_slice(bytes);
    }

    /// The next piece of output for `bytes`: empty once the input so far is
    /// consumed.
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.append_input(bytes);

        let mut out = Vec::with_capacity(OUTPUT_BOUND);
        self.drain(&mut out, false)?;

        Ok(out)
    }

    /// The next piece of output with no input to come: empty once the stream
    /// is complete, which is when the codec can go.
    fn finish(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(OUTPUT_BOUND);
        self.drain(&mut out, true)?;

        // A decoder that never saw the final block, or the trailer after it,
        // was cut short: the standard makes that an error, not a shorter file.
        if out.is_empty() && self.phase != Phase::Done {
            return Err("the compressed input ended early".into());
        }

        Ok(out)
    }

    /// Fills `out` up to its capacity from the held bytes and the pending
    /// input, and stops short of that when the codec needs more input (or, when
    /// `finishing`, has nothing left to say).
    fn drain(&mut self, out: &mut Vec<u8>, finishing: bool) -> Result<(), String> {
        let gzip = self.format == Format::Gzip;

        loop {
            if !self.held.is_empty() {
                let take = (OUTPUT_BOUND - out.len()).min(self.held.len());
                out.extend(self.held.drain(..take));
            }

            if out.len() >= OUTPUT_BOUND {
                return Ok(());
            }

            let input = &self.pending[self.pending_pos..];

            match self.phase {
                Phase::Header => {
                    let took = self.header.feed(input)?;
                    self.pending_pos += took;

                    if !self.header.done() {
                        return Ok(());
                    }

                    self.phase = Phase::Body;
                }
                Phase::Body => match &mut self.engine {
                    Engine::Decode(decoder) => {
                        if input.is_empty() {
                            return Ok(());
                        }

                        let in_before = decoder.total_in();
                        let out_before = out.len();
                        let status = decoder
                            .decompress_vec(input, out, FlushDecompress::None)
                            .map_err(|e| e.to_string())?;
                        let consumed = (decoder.total_in() - in_before) as usize;
                        self.pending_pos += consumed;

                        if gzip {
                            self.crc.update(&out[out_before..]);
                        }

                        if status == Status::StreamEnd {
                            self.phase = if gzip { Phase::Trailer } else { Phase::Done };
                        } else if consumed == 0 && out.len() == out_before {
                            return Ok(());
                        }
                    }
                    Engine::Encode(encoder) => {
                        let flush = if !input.is_empty() {
                            FlushCompress::None
                        } else if finishing {
                            FlushCompress::Finish
                        } else {
                            return Ok(());
                        };

                        let in_before = encoder.total_in();
                        let out_before = out.len();
                        let status = encoder
                            .compress_vec(input, out, flush)
                            .map_err(|e| e.to_string())?;
                        let consumed = (encoder.total_in() - in_before) as usize;

                        if gzip {
                            self.crc.update(&input[..consumed]);
                        }
                        self.pending_pos += consumed;

                        if status == Status::StreamEnd {
                            self.phase = Phase::Done;

                            if gzip {
                                self.held.extend_from_slice(&self.crc.sum().to_le_bytes());
                                self.held
                                    .extend_from_slice(&self.crc.amount().to_le_bytes());
                            }
                        } else if consumed == 0 && out.len() == out_before {
                            return Ok(());
                        }
                    }
                },
                Phase::Trailer => {
                    let take = (8 - self.trailer.len()).min(input.len());
                    self.trailer.extend_from_slice(&input[..take]);
                    self.pending_pos += take;

                    if self.trailer.len() < 8 {
                        return Ok(());
                    }

                    let sum = u32::from_le_bytes(self.trailer[..4].try_into().unwrap());
                    let amount = u32::from_le_bytes(self.trailer[4..].try_into().unwrap());

                    if sum != self.crc.sum() || amount != self.crc.amount() {
                        return Err("the gzip trailer does not match the data".into());
                    }

                    self.phase = Phase::Done;
                }
                Phase::Done => {
                    if !input.is_empty() {
                        return Err(
                            "the input continues past the end of the compressed stream".into()
                        );
                    }

                    return Ok(());
                }
            }
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

/// Releases every codec of the running context.
///
/// For the reset between the requests of a pooled context: a stream the last
/// request left open keeps its codec, which the next request can neither
/// reach nor release.
pub fn clear_compression_state(scope: &mut v8::PinScope) {
    if let Some(state) = crate::context_slots::get::<CompressionState>(scope) {
        state.codecs.borrow_mut().clear();
    }
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
    let mut codecs = state.codecs.borrow_mut();

    if codecs.len() >= MAX_CODECS {
        drop(codecs);
        throw_error(
            scope,
            &format!("too many compression streams are open at once (the limit is {MAX_CODECS})"),
        );
        return;
    }

    let id = {
        let mut next = state.next_id.borrow_mut();
        *next += 1;
        *next
    };

    codecs.insert(id, codec);
    rv.set_uint32(id);
}

/// `compressionPush(id, bytes) -> bytes`: the next piece of output, empty once
/// the input so far is consumed.
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

/// `compressionFinish(id) -> bytes`: the next piece of output with no input to
/// come. The empty piece that ends the stream also releases the codec.
fn compression_finish(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let Some(id) = stream_id(scope, args.get(0)) else {
        return;
    };

    let state = state(scope);
    let mut codecs = state.codecs.borrow_mut();

    let Some(codec) = codecs.get_mut(&id) else {
        throw_type_error(scope, "the compression stream is already closed");
        return;
    };

    match codec.finish() {
        Ok(out) => {
            if out.is_empty() {
                codecs.remove(&id);
            }
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
