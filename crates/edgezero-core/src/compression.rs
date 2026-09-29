use async_compression::futures::bufread::{
    BrotliDecoder as AsyncBrotliDecoder, GzipDecoder as AsyncGzipDecoder,
    ZlibDecoder as AsyncZlibDecoder,
};
use async_stream::stream;
use bytes::Bytes;
use compression_codecs::{
    BrotliDecoder as PinnedBrotliCodec, GzipDecoder as PinnedGzipCodec,
    ZlibDecoder as PinnedZlibCodec,
};
use compression_core::util::PartialBuffer;
use futures::io::AsyncReadExt as _;
use futures_util::{StreamExt as _, TryStreamExt as _, stream as futures_stream};
use std::io;

use crate::body::BodyStream;
use crate::error::{BadGatewayDecodeReason, BadGatewayReason, EdgeError, ResponseLimitReason};
use crate::http::HeaderMap;
use crate::http::header::CONTENT_ENCODING;

const BUFFER_SIZE: usize = 8 * 1024;

const _: fn(brotli::BrotliResult) -> brotli_decompressor::BrotliResult = |result| result;
const _: fn(PartialBuffer<&[u8]>) = consume_pinned_compression_core;

/// Conservative non-window charge for the decoder implementation pinned by
/// the workspace lockfiles.
///
/// The source audit in
/// `docs/audits/2026-09-27-brotli-decoder-memory-accounting.md` derives a
/// 3,442,530-byte allocation-payload subtotal for the boxed decoder state,
/// Huffman storage, context maps, and bridge buffer in the pinned decoder
/// graph. The 16 MiB accounting reservation leaves conservative headroom for
/// wrapper objects and allocator overhead that are not claimed as exact heap
/// bounds. The ring allocation is charged separately as `2^WBITS`; dependency
/// upgrades must repeat the audit before changing either pin or charge.
#[expect(
    clippy::decimal_literal_representation,
    reason = "the decimal byte count is the reviewed public contract"
)]
pub const BROTLI_DECODER_FIXED_CHARGE_BYTES: u64 = 16_777_216;

/// Conservative decoder-state charge shared by the pinned gzip and zlib codecs.
///
/// The source audit in
/// `docs/audits/2026-09-29-flate-decoder-memory-accounting.md` derives a
/// 73,728-byte reviewed subtotal. The 1 MiB reservation leaves conservative
/// headroom for wrapper objects, the boxed coroutine, alignment, and allocator
/// variance that are not claimed as exact heap bounds. Dependency upgrades must
/// repeat the audit before changing either the pins or this charge.
pub const FLATE_DECODER_FIXED_CHARGE_BYTES: u64 = 0x0010_0000;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EdgeErrorCarrier(EdgeError);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrotliPrefix {
    NeedSecondByte,
    WindowBits(u8),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentEncoding {
    Brotli,
    Deflate,
    Gzip,
    Identity,
    Passthrough(PassthroughReason),
}

#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PassthroughReason {
    Malformed,
    Stacked,
    Unsupported(String),
}

fn consume_pinned_compression_core(_buffer: PartialBuffer<&[u8]>) {}

#[must_use]
#[inline]
pub fn classify_content_encoding(headers: &HeaderMap) -> ContentEncoding {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let Some(header_value) = values.next() else {
        return ContentEncoding::Identity;
    };
    if values.next().is_some() {
        return ContentEncoding::Passthrough(PassthroughReason::Stacked);
    }
    let raw_encoding = trim_optional_whitespace(header_value.as_bytes());
    let mut codings = raw_encoding.split(|byte| *byte == b',');
    let Some(first_coding) = codings.next() else {
        return ContentEncoding::Passthrough(PassthroughReason::Malformed);
    };
    let first = trim_optional_whitespace(first_coding);
    if !is_content_coding_token(first) {
        return ContentEncoding::Passthrough(PassthroughReason::Malformed);
    }
    let mut stacked = false;
    for coding in codings {
        if !is_content_coding_token(trim_optional_whitespace(coding)) {
            return ContentEncoding::Passthrough(PassthroughReason::Malformed);
        }
        stacked = true;
    }
    if stacked {
        return ContentEncoding::Passthrough(PassthroughReason::Stacked);
    }
    if first.eq_ignore_ascii_case(b"br") {
        ContentEncoding::Brotli
    } else if first.eq_ignore_ascii_case(b"deflate") {
        ContentEncoding::Deflate
    } else if first.eq_ignore_ascii_case(b"gzip") {
        ContentEncoding::Gzip
    } else if first.eq_ignore_ascii_case(b"identity") {
        ContentEncoding::Identity
    } else {
        let token = first
            .iter()
            .map(|byte| char::from(byte.to_ascii_lowercase()))
            .collect();
        ContentEncoding::Passthrough(PassthroughReason::Unsupported(token))
    }
}

fn is_content_coding_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn trim_optional_whitespace(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !matches!(byte, b' ' | b'\t'))
        .map_or(start, |index| index.saturating_add(1));
    value.get(start..end).unwrap_or_default()
}

/// Return the conservative decoder-state charge for an advertised Brotli window.
///
/// # Errors
/// Returns [`EdgeError::BadRequest`] when `window_bits` is outside the Brotli range accepted by
/// `EdgeZero`, or a typed response-limit error if decoder-memory accounting overflows.
#[inline]
pub fn brotli_decoder_memory_charge(window_bits: u8) -> Result<u64, EdgeError> {
    if !(10..=30).contains(&window_bits) {
        return Err(EdgeError::bad_request(
            "brotli window bits must be between 10 and 30 inclusive",
        ));
    }
    let window = 1_u64.checked_shl(u32::from(window_bits)).ok_or_else(|| {
        EdgeError::response_too_large_with_reason(
            "brotli decoder memory accounting overflow",
            ResponseLimitReason::DecoderMemory,
        )
    })?;
    BROTLI_DECODER_FIXED_CHARGE_BYTES
        .checked_add(window)
        .ok_or_else(|| {
            EdgeError::response_too_large_with_reason(
                "brotli decoder memory accounting overflow",
                ResponseLimitReason::DecoderMemory,
            )
        })
}

/// Decode a stream of gzip-compressed chunks into plain bytes.
#[must_use]
#[inline]
pub fn decode_gzip_stream(stream: BodyStream, max_decoder_bytes: u64) -> BodyStream {
    stream! {
        if let Err(limit_error) = enforce_flate_decoder_memory(max_decoder_bytes, "gzip") {
            yield Err(release_before_terminal(stream, limit_error));
            return;
        }
        let reader = stream.map_err(edge_error_to_io).into_async_read();
        let mut decoder = AsyncGzipDecoder::with_codec(reader, PinnedGzipCodec::new());
        decoder.multiple_members(true);
        let mut buffer = vec![0_u8; BUFFER_SIZE];

        loop {
            let read = match decoder.read(&mut buffer).await {
                Ok(read) => read,
                Err(source_error) => {
                    let error = release_before_terminal(
                        (decoder, buffer),
                        io_to_edge_error(source_error, BadGatewayDecodeReason::Gzip),
                    );
                    yield Err(error);
                    return;
                }
            };
            if read == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..read) else {
                let decoder_error = codec_error(
                    BadGatewayDecodeReason::Gzip,
                    format!("decoder reported {read}-byte read into a {BUFFER_SIZE}-byte buffer"),
                );
                let terminal_error =
                    release_before_terminal((decoder, buffer), decoder_error);
                yield Err(terminal_error);
                return;
            };
            yield Ok(Bytes::copy_from_slice(chunk));
        }
    }
    .boxed_local()
}

/// Decode one RFC 1950 zlib-wrapped DEFLATE stream into plain bytes.
#[must_use]
#[inline]
pub fn decode_deflate_stream(stream: BodyStream, max_decoder_bytes: u64) -> BodyStream {
    stream! {
        if let Err(limit_error) = enforce_flate_decoder_memory(max_decoder_bytes, "deflate") {
            yield Err(release_before_terminal(stream, limit_error));
            return;
        }
        let reader = stream.map_err(edge_error_to_io).into_async_read();
        let mut decoder = AsyncZlibDecoder::with_codec(reader, PinnedZlibCodec::new());
        let mut buffer = vec![0_u8; BUFFER_SIZE];

        loop {
            let read = match decoder.read(&mut buffer).await {
                Ok(read) => read,
                Err(source_error) => {
                    let error = release_before_terminal(
                        (decoder, buffer),
                        io_to_edge_error(source_error, BadGatewayDecodeReason::Deflate),
                    );
                    yield Err(error);
                    return;
                }
            };
            if read == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..read) else {
                let decoder_error = codec_error(
                    BadGatewayDecodeReason::Deflate,
                    format!("decoder reported {read}-byte read into a {BUFFER_SIZE}-byte buffer"),
                );
                let terminal_error =
                    release_before_terminal((decoder, buffer), decoder_error);
                yield Err(terminal_error);
                return;
            };
            yield Ok(Bytes::copy_from_slice(chunk));
        }

        let mut native_reader = decoder.into_inner();
        let mut trailing = [0_u8; 1];
        match native_reader.read(&mut trailing).await {
            Ok(0) => {}
            Ok(_) => {
                let trailing_error = codec_error(
                    BadGatewayDecodeReason::Deflate,
                    "deflate response contains trailing data or a second stream",
                );
                let terminal_error = release_before_terminal(native_reader, trailing_error);
                yield Err(terminal_error);
            }
            Err(source_error) => {
                let terminal_error = release_before_terminal(
                    native_reader,
                    io_to_edge_error(source_error, BadGatewayDecodeReason::Deflate),
                );
                yield Err(terminal_error);
            }
        }
    }
    .boxed_local()
}

fn enforce_flate_decoder_memory(max_decoder_bytes: u64, coding: &str) -> Result<(), EdgeError> {
    if FLATE_DECODER_FIXED_CHARGE_BYTES > max_decoder_bytes {
        return Err(EdgeError::response_too_large_with_reason(
            format!(
                "{coding} decoder requires {FLATE_DECODER_FIXED_CHARGE_BYTES} bytes; limit is {max_decoder_bytes}"
            ),
            ResponseLimitReason::DecoderMemory,
        ));
    }
    Ok(())
}

/// Decode a stream of brotli-compressed chunks into plain bytes.
#[must_use]
#[inline]
pub fn decode_brotli_stream(
    mut source: BodyStream,
    max_window_bits: u8,
    max_decoder_bytes: u64,
) -> BodyStream {
    stream! {
        let mut prefix = Vec::with_capacity(2);
        let mut initial_chunks = Vec::new();
        let window_bits = loop {
            let Some(item) = source.next().await else {
                let prefix_error = codec_error(
                    BadGatewayDecodeReason::Brotli,
                    "brotli stream ended before its window prefix",
                );
                let terminal_error = release_before_terminal(
                    (source, initial_chunks, prefix),
                    prefix_error,
                );
                yield Err(terminal_error);
                return;
            };
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(source_error) => {
                    let terminal_error =
                        release_before_terminal((source, initial_chunks, prefix), source_error);
                    yield Err(terminal_error);
                    return;
                }
            };
            if chunk.is_empty() {
                continue;
            }
            let prefix_remaining = 2_usize.saturating_sub(prefix.len());
            prefix.extend(chunk.iter().take(prefix_remaining));
            initial_chunks.push(chunk);

            match parse_brotli_prefix(&prefix) {
                Ok(BrotliPrefix::WindowBits(bits)) => break bits,
                Ok(BrotliPrefix::NeedSecondByte) => {}
                Err(prefix_error) => {
                    let terminal_error =
                        release_before_terminal((source, initial_chunks, prefix), prefix_error);
                    yield Err(terminal_error);
                    return;
                }
            }
        };

        if window_bits > max_window_bits {
            let limit_error = EdgeError::response_too_large_with_reason(
                format!(
                    "brotli response advertises a {window_bits}-bit window; limit is {max_window_bits}"
                ),
                ResponseLimitReason::BrotliWindow,
            );
            let terminal_error =
                release_before_terminal((source, initial_chunks, prefix), limit_error);
            yield Err(terminal_error);
            return;
        }
        let charge = match brotli_decoder_memory_charge(window_bits) {
            Ok(charge) => charge,
            Err(charge_error) => {
                let terminal_error =
                    release_before_terminal((source, initial_chunks, prefix), charge_error);
                yield Err(terminal_error);
                return;
            }
        };
        if charge > max_decoder_bytes {
            let limit_error = EdgeError::response_too_large_with_reason(
                format!("brotli decoder requires {charge} bytes; limit is {max_decoder_bytes}"),
                ResponseLimitReason::DecoderMemory,
            );
            let terminal_error =
                release_before_terminal((source, initial_chunks, prefix), limit_error);
            yield Err(terminal_error);
            return;
        }

        drop(prefix);
        let mut decoded = decode_brotli_payload(source, initial_chunks);
        while let Some(item) = decoded.next().await {
            match item {
                Ok(bytes) => yield Ok(bytes),
                Err(error) => {
                    drop(decoded);
                    yield Err(error);
                    return;
                }
            }
        }
    }
    .boxed_local()
}

fn decode_brotli_payload(source: BodyStream, initial_chunks: Vec<Bytes>) -> BodyStream {
    stream! {
        let replayed_stream = futures_stream::iter(initial_chunks.into_iter().map(Ok)).chain(source);
        let reader = replayed_stream.map_err(edge_error_to_io).into_async_read();
        let mut decoder =
            AsyncBrotliDecoder::with_codec(reader, PinnedBrotliCodec::new());
        let mut buffer = vec![0_u8; BUFFER_SIZE];

        loop {
            let read = match decoder.read(&mut buffer).await {
                Ok(read) => read,
                Err(source_error) => {
                    let terminal_error = release_before_terminal(
                        (decoder, buffer),
                        io_to_edge_error(source_error, BadGatewayDecodeReason::Brotli),
                    );
                    yield Err(terminal_error);
                    return;
                }
            };
            if read == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..read) else {
                let decoder_error = codec_error(
                    BadGatewayDecodeReason::Brotli,
                    format!("decoder reported {read}-byte read into a {BUFFER_SIZE}-byte buffer"),
                );
                let terminal_error =
                    release_before_terminal((decoder, buffer), decoder_error);
                yield Err(terminal_error);
                return;
            };
            yield Ok(Bytes::copy_from_slice(chunk));
        }

        // Brotli has no multi-member HTTP representation. Recover any input
        // read ahead by the decoder, then poll the native source once more so
        // trailing bytes and late transport errors cannot be hidden by codec EOF.
        let mut native_reader = decoder.into_inner();
        let mut trailing = [0_u8; 1];
        match native_reader.read(&mut trailing).await {
            Ok(0) => {}
            Ok(_) => {
                let trailing_error = codec_error(
                    BadGatewayDecodeReason::Brotli,
                    "brotli response contains trailing data or a second stream",
                );
                let terminal_error = release_before_terminal(native_reader, trailing_error);
                yield Err(terminal_error);
            }
            Err(source_error) => {
                let terminal_error = release_before_terminal(
                    native_reader,
                    io_to_edge_error(source_error, BadGatewayDecodeReason::Brotli),
                );
                yield Err(terminal_error);
            }
        }
    }
    .boxed_local()
}

fn release_before_terminal<Owned>(owned: Owned, error: EdgeError) -> EdgeError {
    drop(owned);
    error
}

fn codec_error(reason: BadGatewayDecodeReason, message: impl Into<String>) -> EdgeError {
    EdgeError::bad_gateway_with_reason(message, BadGatewayReason::Decode(reason))
}

fn edge_error_to_io(error: EdgeError) -> io::Error {
    io::Error::other(EdgeErrorCarrier(error))
}

fn io_to_edge_error(error: io::Error, reason: BadGatewayDecodeReason) -> EdgeError {
    let diagnostic = error.to_string();
    if let Some(source) = error.into_inner()
        && let Ok(carrier) = source.downcast::<EdgeErrorCarrier>()
    {
        return carrier.0;
    }
    codec_error(
        reason,
        format!("upstream response decode failed: {diagnostic}"),
    )
}

fn parse_brotli_prefix(prefix: &[u8]) -> Result<BrotliPrefix, EdgeError> {
    let Some(&first) = prefix.first() else {
        return Ok(BrotliPrefix::NeedSecondByte);
    };
    if first & 1 == 0 {
        return Ok(BrotliPrefix::WindowBits(16));
    }
    let short_code = match first & 0x0f {
        0x03 => Some(18),
        0x05 => Some(19),
        0x07 => Some(20),
        0x09 => Some(21),
        0x0b => Some(22),
        0x0d => Some(23),
        0x0f => Some(24),
        _ => None,
    };
    if let Some(bits) = short_code {
        return Ok(BrotliPrefix::WindowBits(bits));
    }
    let long_code = match first & 0x7f {
        0x71 => Some(15),
        0x61 => Some(14),
        0x51 => Some(13),
        0x41 => Some(12),
        0x31 => Some(11),
        0x21 => Some(10),
        0x01 => Some(17),
        _ => None,
    };
    if let Some(bits) = long_code {
        return Ok(BrotliPrefix::WindowBits(bits));
    }
    if first != 0x11 {
        return Err(codec_error(
            BadGatewayDecodeReason::Brotli,
            "invalid brotli window prefix",
        ));
    }
    let Some(&second) = prefix.get(1) else {
        return Ok(BrotliPrefix::NeedSecondByte);
    };
    let bits = second & 0x3f;
    if !(10..=30).contains(&bits) {
        return Err(codec_error(
            BadGatewayDecodeReason::Brotli,
            "invalid brotli large-window prefix",
        ));
    }
    Ok(BrotliPrefix::WindowBits(bits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BudgetSource;
    use crate::http::{HeaderMap, HeaderValue};
    use brotli::{BrotliState, CompressorWriter, HuffmanCode, enc::StandardAlloc};
    use bytes::Bytes;
    use flate2::{
        Compression,
        write::{DeflateEncoder, GzEncoder, ZlibEncoder},
    };
    use futures::executor::block_on;
    use futures_util::FutureExt as _;
    use futures_util::stream::{self, poll_fn};
    use std::cell::Cell;
    use std::io::Write as _;
    use std::mem::size_of;
    use std::rc::Rc;
    use std::task::Poll;

    struct DropSignal(Rc<Cell<usize>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.set(self.0.get().saturating_add(1));
        }
    }

    fn source(items: Vec<Result<Bytes, EdgeError>>) -> BodyStream {
        stream::iter(items).boxed_local()
    }

    fn tracked_source(first_item: Result<Bytes, EdgeError>, drops: &Rc<Cell<usize>>) -> BodyStream {
        let signal = DropSignal(Rc::clone(drops));
        let mut pending_item = Some(first_item);
        poll_fn(move |_context| {
            let _keep_signal_alive = &signal;
            Poll::Ready(pending_item.take())
        })
        .boxed_local()
    }

    fn gzip(input: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn raw_deflate(input: &[u8]) -> Vec<u8> {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(input: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli(input: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut compressor = CompressorWriter::new(&mut encoded, 4096, 5, 21);
        compressor.write_all(input).unwrap();
        drop(compressor);
        encoded
    }

    #[test]
    fn content_encoding_classifier_covers_every_visible_shape() {
        let cases = vec![
            (None, ContentEncoding::Identity),
            (Some(b"identity".as_slice()), ContentEncoding::Identity),
            (Some(b" GZIP \t".as_slice()), ContentEncoding::Gzip),
            (Some(b"Br".as_slice()), ContentEncoding::Brotli),
            (Some(b" DeFlAtE \t".as_slice()), ContentEncoding::Deflate),
            (
                Some(b"zstd".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Unsupported(String::from("zstd"))),
            ),
            (
                Some(b" X_Custom ".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Unsupported(String::from(
                    "x_custom",
                ))),
            ),
            (
                Some(b"gzip, deflate".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Stacked),
            ),
            (
                Some(b"gzip; level=1".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
            (
                Some(b"gzip,,deflate".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
            (
                Some(b"gzip,".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
            (
                Some(b"".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
            (
                Some(b" \t".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
            (
                Some(b"\xff".as_slice()),
                ContentEncoding::Passthrough(PassthroughReason::Malformed),
            ),
        ];

        for (value, expected) in cases {
            let mut headers = HeaderMap::new();
            if let Some(raw_value) = value {
                headers.append(
                    "content-encoding",
                    HeaderValue::from_bytes(raw_value).expect("header"),
                );
            }
            assert_eq!(classify_content_encoding(&headers), expected);
        }

        let mut repeated = HeaderMap::new();
        repeated.append("content-encoding", HeaderValue::from_static("gzip"));
        repeated.append("content-encoding", HeaderValue::from_static("gzip"));
        assert_eq!(
            classify_content_encoding(&repeated),
            ContentEncoding::Passthrough(PassthroughReason::Stacked)
        );

        let mut repeated_with_malformed_value = HeaderMap::new();
        repeated_with_malformed_value.append(
            "content-encoding",
            HeaderValue::from_bytes(b"\xff").expect("header"),
        );
        repeated_with_malformed_value.append("content-encoding", HeaderValue::from_static("gzip"));
        assert_eq!(
            classify_content_encoding(&repeated_with_malformed_value),
            ContentEncoding::Passthrough(PassthroughReason::Stacked)
        );
    }

    #[test]
    fn brotli_decoder_memory_charge_is_pinned_and_checked() {
        assert_eq!(brotli_decoder_memory_charge(24_u8).unwrap(), 0x0200_0000);
        assert!(matches!(
            brotli_decoder_memory_charge(9),
            Err(EdgeError::BadRequest { .. })
        ));
        assert!(matches!(
            brotli_decoder_memory_charge(31),
            Err(EdgeError::BadRequest { .. })
        ));
    }

    #[test]
    fn brotli_fixed_charge_covers_source_audited_payload_subtotal() {
        type DecoderState = BrotliState<StandardAlloc, StandardAlloc, StandardAlloc>;

        const HUFFMAN_CODE_BYTES: u64 = 4;
        const MAX_BLOCK_TYPES_OR_TREES: u64 = 256;
        const MAX_HUFFMAN_TABLE_ENTRIES: u64 = 1_080;
        const DECODER_STATE_RESERVE_BYTES: u64 = 0x0001_0000;
        const RING_FIXED_SLACK_BYTES: u64 = 42 + 24;

        let decode_buffer_bytes = u64::try_from(BUFFER_SIZE).expect("buffer size fits u64");

        assert_eq!(
            u64::try_from(size_of::<HuffmanCode>()).expect("HuffmanCode size fits u64"),
            HUFFMAN_CODE_BYTES
        );
        assert!(
            u64::try_from(size_of::<DecoderState>()).expect("decoder state size fits u64")
                <= DECODER_STATE_RESERVE_BYTES
        );

        let context_map_table = MAX_HUFFMAN_TABLE_ENTRIES * HUFFMAN_CODE_BYTES;
        let block_trees = 2 * 3 * MAX_HUFFMAN_TABLE_ENTRIES * HUFFMAN_CODE_BYTES;
        let context_modes = MAX_BLOCK_TYPES_OR_TREES;
        let context_maps = MAX_BLOCK_TYPES_OR_TREES * 64 + MAX_BLOCK_TYPES_OR_TREES * 4;
        let huffman_group_indices = 3 * MAX_BLOCK_TYPES_OR_TREES * 4;
        let huffman_group_codes =
            3 * MAX_BLOCK_TYPES_OR_TREES * MAX_HUFFMAN_TABLE_ENTRIES * HUFFMAN_CODE_BYTES;
        let audited_payload_bytes = DECODER_STATE_RESERVE_BYTES
            + context_map_table
            + block_trees
            + context_modes
            + context_maps
            + huffman_group_indices
            + huffman_group_codes
            + decode_buffer_bytes
            + RING_FIXED_SLACK_BYTES;

        assert_eq!(audited_payload_bytes, 3_442_530);
        assert!(audited_payload_bytes <= BROTLI_DECODER_FIXED_CHARGE_BYTES);
    }

    #[test]
    fn flate_fixed_charge_covers_source_audited_payload_subtotal() {
        use miniz_oxide::inflate::stream::InflateState;

        const INFLATE_STATE_RESERVE_BYTES: u64 = 0x0001_0000;
        const CODEC_WRAPPER_RESERVE_BYTES: u64 = 4_096;
        const PUBLIC_CHARGE_BYTES: u64 = 0x0010_0000;

        let decode_buffer_bytes = u64::try_from(BUFFER_SIZE).expect("buffer size fits u64");
        let inflate_state_bytes =
            u64::try_from(size_of::<InflateState>()).expect("inflate state size fits u64");
        let gzip_codec_bytes =
            u64::try_from(size_of::<PinnedGzipCodec>()).expect("gzip codec size fits u64");
        let zlib_codec_bytes =
            u64::try_from(size_of::<PinnedZlibCodec>()).expect("zlib codec size fits u64");

        assert!(inflate_state_bytes <= INFLATE_STATE_RESERVE_BYTES);
        assert!(gzip_codec_bytes <= CODEC_WRAPPER_RESERVE_BYTES);
        assert!(zlib_codec_bytes <= CODEC_WRAPPER_RESERVE_BYTES);
        assert_eq!(decode_buffer_bytes, 8_192);
        assert_eq!(FLATE_DECODER_FIXED_CHARGE_BYTES, PUBLIC_CHARGE_BYTES);

        let audited_payload_bytes = INFLATE_STATE_RESERVE_BYTES + decode_buffer_bytes;
        assert_eq!(audited_payload_bytes, 73_728);
        assert!(audited_payload_bytes <= FLATE_DECODER_FIXED_CHARGE_BYTES);
    }

    #[test]
    fn decode_gzip_stream_yields_plain_bytes() {
        let stream = source(vec![Ok(Bytes::from(gzip(b"hello gzip")))]);
        let decoded = block_on(async {
            decode_gzip_stream(stream, FLATE_DECODER_FIXED_CHARGE_BYTES)
                .try_collect::<Vec<Bytes>>()
                .await
                .map(|chunks| chunks.concat())
        })
        .unwrap();

        assert_eq!(decoded, b"hello gzip");
    }

    #[test]
    fn decode_deflate_stream_yields_plain_bytes_from_zlib_framing() {
        let stream = source(vec![Ok(Bytes::from(zlib(b"hello deflate")))]);
        let decoded = block_on(async {
            decode_deflate_stream(stream, FLATE_DECODER_FIXED_CHARGE_BYTES)
                .try_collect::<Vec<Bytes>>()
                .await
                .map(|chunks| chunks.concat())
        })
        .unwrap();

        assert_eq!(decoded, b"hello deflate");
    }

    #[test]
    fn decode_deflate_stream_rejects_raw_rfc1951_framing() {
        let stream = source(vec![Ok(Bytes::from(raw_deflate(b"raw deflate")))]);
        let result = block_on(
            decode_deflate_stream(stream, FLATE_DECODER_FIXED_CHARGE_BYTES)
                .try_collect::<Vec<Bytes>>(),
        );

        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Deflate),
                ..
            })
        ));
    }

    #[test]
    fn decode_brotli_stream_yields_plain_bytes() {
        let stream = source(vec![Ok(Bytes::from(brotli(b"hello brotli")))]);
        let decoded = block_on(async {
            decode_brotli_stream(stream, 24, 1_u64 << 25)
                .try_collect::<Vec<Bytes>>()
                .await
                .map(|chunks| chunks.concat())
        })
        .unwrap();

        assert_eq!(decoded, b"hello brotli");
    }

    #[test]
    fn decode_gzip_stream_surfaces_error_on_invalid_input() {
        let garbage = Bytes::from_static(b"this is definitely not a gzip member");
        let stream = source(vec![Ok(garbage)]);
        let result = block_on(async {
            decode_gzip_stream(stream, FLATE_DECODER_FIXED_CHARGE_BYTES)
                .try_collect::<Vec<Bytes>>()
                .await
        });
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Gzip),
                ..
            })
        ));
    }

    #[test]
    fn decode_deflate_stream_surfaces_error_on_invalid_input() {
        let garbage = Bytes::from_static(b"this is not a zlib-wrapped deflate stream");
        let result = block_on(
            decode_deflate_stream(source(vec![Ok(garbage)]), FLATE_DECODER_FIXED_CHARGE_BYTES)
                .try_collect::<Vec<Bytes>>(),
        );
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Deflate),
                ..
            })
        ));
    }

    #[test]
    fn decode_brotli_stream_surfaces_error_on_invalid_input() {
        // A high-bit-set lead byte is not a valid brotli stream prefix.
        let garbage = Bytes::from(vec![0xFF_u8; 64]);
        let stream = source(vec![Ok(garbage)]);
        let result = block_on(async {
            decode_brotli_stream(stream, 24, 1_u64 << 25)
                .try_collect::<Vec<Bytes>>()
                .await
        });
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Brotli),
                ..
            })
        ));
    }

    #[test]
    fn terminal_stream_error_releases_source_before_yield() {
        let gzip_drops = Rc::new(Cell::new(0_usize));
        let mut gzip_stream = decode_gzip_stream(
            tracked_source(Ok(Bytes::from_static(b"not a gzip stream")), &gzip_drops),
            FLATE_DECODER_FIXED_CHARGE_BYTES,
        );
        block_on(gzip_stream.next())
            .expect("gzip result")
            .expect_err("gzip decode error");
        assert_eq!(gzip_drops.get(), 1_usize);

        let deflate_drops = Rc::new(Cell::new(0_usize));
        let mut deflate_stream = decode_deflate_stream(
            tracked_source(
                Ok(Bytes::from_static(b"not a deflate stream")),
                &deflate_drops,
            ),
            FLATE_DECODER_FIXED_CHARGE_BYTES,
        );
        block_on(deflate_stream.next())
            .expect("deflate result")
            .expect_err("deflate decode error");
        assert_eq!(deflate_drops.get(), 1_usize);

        let brotli_drops = Rc::new(Cell::new(0_usize));
        let mut brotli_stream = decode_brotli_stream(
            tracked_source(Ok(Bytes::from_static(&[0xff])), &brotli_drops),
            24,
            1_u64 << 25,
        );
        block_on(brotli_stream.next())
            .expect("brotli result")
            .expect_err("brotli prefix error");
        assert_eq!(brotli_drops.get(), 1_usize);
    }

    #[test]
    fn decoder_carrier_restores_exact_edge_error() {
        let expected = BudgetSource::RequestDeadline;
        let stream = source(vec![
            Ok(Bytes::from_static(&[0x1f, 0x8b])),
            Err(EdgeError::gateway_timeout_caused("late", expected)),
        ]);
        let result = block_on(
            decode_gzip_stream(stream, FLATE_DECODER_FIXED_CHARGE_BYTES).try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            result,
            Err(EdgeError::GatewayTimeout { cause, .. }) if cause == expected
        ));
    }

    #[test]
    fn decode_gzip_drains_every_member_to_native_eof() {
        let mut encoded = gzip(b"one");
        encoded.extend(gzip(b"two"));
        let decoded = block_on(
            decode_gzip_stream(
                source(vec![Ok(Bytes::from(encoded))]),
                FLATE_DECODER_FIXED_CHARGE_BYTES,
            )
            .try_collect::<Vec<_>>(),
        )
        .unwrap()
        .concat();
        assert_eq!(decoded, b"onetwo");

        let encoded_member = gzip(b"one");
        let result = block_on(
            decode_gzip_stream(
                source(vec![
                    Ok(Bytes::from(encoded_member)),
                    Err(EdgeError::bad_gateway_with_reason(
                        "late transport failure",
                        BadGatewayReason::Transport,
                    )),
                ]),
                FLATE_DECODER_FIXED_CHARGE_BYTES,
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Transport,
                ..
            })
        ));
    }

    #[test]
    fn decode_deflate_requires_native_eof_and_preserves_late_source_errors() {
        let mut with_trailing = zlib(b"one");
        with_trailing.extend_from_slice(b"trailing");
        let trailing_result = block_on(
            decode_deflate_stream(
                source(vec![Ok(Bytes::from(with_trailing))]),
                FLATE_DECODER_FIXED_CHARGE_BYTES,
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            trailing_result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Deflate),
                ..
            })
        ));

        let late_error_result = block_on(
            decode_deflate_stream(
                source(vec![
                    Ok(Bytes::from(zlib(b"one"))),
                    Err(EdgeError::bad_gateway_with_reason(
                        "late transport failure",
                        BadGatewayReason::Transport,
                    )),
                ]),
                FLATE_DECODER_FIXED_CHARGE_BYTES,
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            late_error_result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Transport,
                ..
            })
        ));
    }

    #[test]
    fn decode_deflate_rejects_a_second_zlib_stream() {
        let mut encoded = zlib(b"one");
        encoded.extend(zlib(b"two"));

        let result = block_on(
            decode_deflate_stream(
                source(vec![Ok(Bytes::from(encoded))]),
                FLATE_DECODER_FIXED_CHARGE_BYTES,
            )
            .try_collect::<Vec<_>>(),
        );

        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Deflate),
                ..
            })
        ));
    }

    #[test]
    fn decode_deflate_waits_for_native_eof_after_codec_eof() {
        let mut encoded = Some(Bytes::from(zlib(b"one")));
        let native = poll_fn(move |_context| {
            if let Some(chunk) = encoded.take() {
                Poll::Ready(Some(Ok(chunk)))
            } else {
                Poll::Pending
            }
        })
        .boxed_local();
        let mut decoded = decode_deflate_stream(native, FLATE_DECODER_FIXED_CHARGE_BYTES);

        let first = block_on(decoded.next())
            .expect("decoded item")
            .expect("decoded bytes");
        assert_eq!(first, Bytes::from_static(b"one"));
        assert!(decoded.next().now_or_never().is_none());
    }

    #[test]
    fn flate_memory_limit_rejects_before_source_poll() {
        let gzip_polls = Rc::new(Cell::new(0_usize));
        let observed_gzip_polls = Rc::clone(&gzip_polls);
        let gzip_source = poll_fn(move |_context| {
            observed_gzip_polls.set(observed_gzip_polls.get().saturating_add(1));
            Poll::Ready(None)
        })
        .boxed_local();
        let gzip_error = block_on(
            decode_gzip_stream(
                gzip_source,
                FLATE_DECODER_FIXED_CHARGE_BYTES.saturating_sub(1),
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            gzip_error,
            Err(EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::DecoderMemory,
                ..
            })
        ));
        assert_eq!(gzip_polls.get(), 0_usize);

        let deflate_polls = Rc::new(Cell::new(0_usize));
        let observed_deflate_polls = Rc::clone(&deflate_polls);
        let deflate_source = poll_fn(move |_context| {
            observed_deflate_polls.set(observed_deflate_polls.get().saturating_add(1));
            Poll::Ready(None)
        })
        .boxed_local();
        let deflate_error = block_on(
            decode_deflate_stream(
                deflate_source,
                FLATE_DECODER_FIXED_CHARGE_BYTES.saturating_sub(1),
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            deflate_error,
            Err(EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::DecoderMemory,
                ..
            })
        ));
        assert_eq!(deflate_polls.get(), 0_usize);
    }

    #[test]
    fn decode_brotli_rejects_trailing_data() {
        let mut encoded = brotli(b"one");
        encoded.extend_from_slice(b"trailing");
        let result = block_on(
            decode_brotli_stream(source(vec![Ok(Bytes::from(encoded))]), 24, 1_u64 << 25)
                .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Brotli),
                ..
            })
        ));
    }

    #[test]
    fn brotli_window_parser_covers_standard_and_large_forms() {
        let standard = [
            (0x21, 10),
            (0x31, 11),
            (0x41, 12),
            (0x51, 13),
            (0x61, 14),
            (0x71, 15),
            (0x00, 16),
            (0x01, 17),
            (0x03, 18),
            (0x05, 19),
            (0x07, 20),
            (0x09, 21),
            (0x0b, 22),
            (0x0d, 23),
            (0x0f, 24),
        ];
        for (byte, bits) in standard {
            assert_eq!(
                parse_brotli_prefix(&[byte]).unwrap(),
                BrotliPrefix::WindowBits(bits)
            );
        }
        for bits in 10_u8..=30 {
            assert_eq!(
                parse_brotli_prefix(&[0x11, bits]).unwrap(),
                BrotliPrefix::WindowBits(bits)
            );
        }
        assert_eq!(
            parse_brotli_prefix(&[0x11]).unwrap(),
            BrotliPrefix::NeedSecondByte
        );
        parse_brotli_prefix(&[0x91, 24]).unwrap_err();
        parse_brotli_prefix(&[0x11, 9]).unwrap_err();
        parse_brotli_prefix(&[0x11, 31]).unwrap_err();
    }

    #[test]
    fn brotli_window_rejects_before_decoder_allocation() {
        let window_error = block_on(
            decode_brotli_stream(source(vec![Ok(Bytes::from_static(&[0x0f]))]), 23, u64::MAX)
                .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            window_error,
            Err(EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BrotliWindow,
                ..
            })
        ));

        let memory_error = block_on(
            decode_brotli_stream(
                source(vec![Ok(Bytes::from_static(&[0x0f]))]),
                24,
                brotli_decoder_memory_charge(24).unwrap() - 1,
            )
            .try_collect::<Vec<_>>(),
        );
        assert!(matches!(
            memory_error,
            Err(EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::DecoderMemory,
                ..
            })
        ));
    }
}
