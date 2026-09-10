use std::io;
use std::str;

use async_compression::futures::bufread::{BrotliDecoder, GzipDecoder};
use async_stream::stream;
use bytes::Bytes;
use futures::io::AsyncReadExt as _;
use futures_util::{StreamExt as _, TryStreamExt as _, stream as futures_stream};

use crate::body::BodyStream;
use crate::error::{BadGatewayDecodeReason, BadGatewayReason, EdgeError, ResponseLimitReason};
use crate::http::HeaderMap;
use crate::http::header::CONTENT_ENCODING;

const BUFFER_SIZE: usize = 8 * 1024;

/// Conservative non-window charge for the decoder implementation pinned by
/// the workspace lockfiles.
///
/// The audit covers the boxed decoder state, initial context-map table, block
/// type/length Huffman arrays, literal and distance context maps, context
/// modes, three Huffman tree groups and their index arrays, and decoder bridge
/// buffers in `brotli-decompressor 5.0.1`/`compression-codecs 0.4.38`. Their
/// grammar-bounded maxima fit within 16 MiB. The ring allocation is charged as
/// `2^WBITS`; dependency upgrades must repeat this source audit before changing
/// either pin or charge.
#[expect(
    clippy::decimal_literal_representation,
    reason = "the decimal byte count is the reviewed public contract"
)]
pub const BROTLI_DECODER_FIXED_CHARGE_BYTES: u64 = 16_777_216;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct EdgeErrorCarrier(EdgeError);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BrotliPrefix {
    NeedSecondByte,
    WindowBits(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentEncoding {
    Brotli,
    Gzip,
    Identity,
    Passthrough,
}

#[must_use]
#[inline]
pub fn classify_content_encoding(headers: &HeaderMap) -> ContentEncoding {
    let mut values = headers.get_all(CONTENT_ENCODING).iter();
    let Some(header_value) = values.next() else {
        return ContentEncoding::Identity;
    };
    if values.next().is_some() {
        return ContentEncoding::Passthrough;
    }
    let Ok(raw_encoding) = str::from_utf8(header_value.as_bytes()) else {
        return ContentEncoding::Passthrough;
    };
    let encoding = raw_encoding.trim_matches([' ', '\t']);
    if encoding.eq_ignore_ascii_case("br") {
        ContentEncoding::Brotli
    } else if encoding.eq_ignore_ascii_case("gzip") {
        ContentEncoding::Gzip
    } else if encoding.eq_ignore_ascii_case("identity") {
        ContentEncoding::Identity
    } else {
        ContentEncoding::Passthrough
    }
}

/// Return the conservative decoder-state charge for an advertised Brotli window.
///
/// # Errors
/// Returns a typed response-limit error when `window_bits` is outside the
/// Brotli range accepted by `EdgeZero`.
#[inline]
pub fn brotli_decoder_memory_charge(window_bits: u8) -> Result<u64, EdgeError> {
    if !(10..=30).contains(&window_bits) {
        return Err(EdgeError::response_too_large_with_reason(
            "brotli window bits must be between 10 and 30 inclusive",
            ResponseLimitReason::BrotliWindow,
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
pub fn decode_gzip_stream(stream: BodyStream) -> BodyStream {
    stream! {
        let reader = stream.map_err(edge_error_to_io).into_async_read();
        let mut decoder = GzipDecoder::new(reader);
        decoder.multiple_members(true);
        let mut buffer = vec![0_u8; BUFFER_SIZE];

        loop {
            let read = match decoder.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => {
                    yield Err(io_to_edge_error(error, BadGatewayDecodeReason::Gzip));
                    return;
                }
            };
            if read == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..read) else {
                yield Err(codec_error(
                    BadGatewayDecodeReason::Gzip,
                    format!("decoder reported {read}-byte read into a {BUFFER_SIZE}-byte buffer"),
                ));
                return;
            };
            yield Ok(Bytes::copy_from_slice(chunk));
        }
    }
    .boxed_local()
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
                yield Err(codec_error(
                    BadGatewayDecodeReason::Brotli,
                    "brotli stream ended before its window prefix",
                ));
                return;
            };
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(error) => {
                    yield Err(error);
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
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        };

        if window_bits > max_window_bits {
            yield Err(EdgeError::response_too_large_with_reason(
                format!(
                    "brotli response advertises a {window_bits}-bit window; limit is {max_window_bits}"
                ),
                ResponseLimitReason::BrotliWindow,
            ));
            return;
        }
        let charge = match brotli_decoder_memory_charge(window_bits) {
            Ok(charge) => charge,
            Err(error) => {
                yield Err(error);
                return;
            }
        };
        if charge > max_decoder_bytes {
            yield Err(EdgeError::response_too_large_with_reason(
                format!("brotli decoder requires {charge} bytes; limit is {max_decoder_bytes}"),
                ResponseLimitReason::DecoderMemory,
            ));
            return;
        }

        let replayed_stream = futures_stream::iter(initial_chunks.into_iter().map(Ok)).chain(source);
        let reader = replayed_stream.map_err(edge_error_to_io).into_async_read();
        let mut decoder = BrotliDecoder::new(reader);
        let mut buffer = vec![0_u8; BUFFER_SIZE];

        loop {
            let read = match decoder.read(&mut buffer).await {
                Ok(read) => read,
                Err(error) => {
                    yield Err(io_to_edge_error(error, BadGatewayDecodeReason::Brotli));
                    return;
                }
            };
            if read == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..read) else {
                yield Err(codec_error(
                    BadGatewayDecodeReason::Brotli,
                    format!("decoder reported {read}-byte read into a {BUFFER_SIZE}-byte buffer"),
                ));
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
                yield Err(codec_error(
                    BadGatewayDecodeReason::Brotli,
                    "brotli response contains trailing data or a second stream",
                ));
            }
            Err(error) => {
                yield Err(io_to_edge_error(error, BadGatewayDecodeReason::Brotli));
            }
        }
    }
    .boxed_local()
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
    use brotli::CompressorWriter;
    use bytes::Bytes;
    use flate2::{Compression, write::GzEncoder};
    use futures::executor::block_on;
    use futures_util::stream;
    use std::io::Write as _;

    fn source(items: Vec<Result<Bytes, EdgeError>>) -> BodyStream {
        stream::iter(items).boxed_local()
    }

    fn gzip(input: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
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
        let cases = [
            (None, ContentEncoding::Identity),
            (Some(b"identity".as_slice()), ContentEncoding::Identity),
            (Some(b" GZIP \t".as_slice()), ContentEncoding::Gzip),
            (Some(b"Br".as_slice()), ContentEncoding::Brotli),
            (Some(b"zstd".as_slice()), ContentEncoding::Passthrough),
            (Some(b"gzip, br".as_slice()), ContentEncoding::Passthrough),
            (
                Some(b"gzip; level=1".as_slice()),
                ContentEncoding::Passthrough,
            ),
            (Some(b"".as_slice()), ContentEncoding::Passthrough),
            (Some(b"\xff".as_slice()), ContentEncoding::Passthrough),
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
            ContentEncoding::Passthrough
        );
    }

    #[test]
    fn brotli_decoder_memory_charge_is_pinned_and_checked() {
        assert_eq!(
            brotli_decoder_memory_charge(24_u8).unwrap(),
            BROTLI_DECODER_FIXED_CHARGE_BYTES + (1_u64 << 24_u32)
        );
        brotli_decoder_memory_charge(9).unwrap_err();
        brotli_decoder_memory_charge(31).unwrap_err();
    }

    #[test]
    fn decode_gzip_stream_yields_plain_bytes() {
        let stream = source(vec![Ok(Bytes::from(gzip(b"hello gzip")))]);
        let decoded = block_on(async {
            decode_gzip_stream(stream)
                .try_collect::<Vec<Bytes>>()
                .await
                .map(|chunks| chunks.concat())
        })
        .unwrap();

        assert_eq!(decoded, b"hello gzip");
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
        let result =
            block_on(async { decode_gzip_stream(stream).try_collect::<Vec<Bytes>>().await });
        assert!(matches!(
            result,
            Err(EdgeError::BadGateway {
                reason: BadGatewayReason::Decode(BadGatewayDecodeReason::Gzip),
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
    fn decoder_carrier_restores_exact_edge_error() {
        let expected = BudgetSource::BatchDeadline;
        let stream = source(vec![
            Ok(Bytes::from_static(&[0x1f, 0x8b])),
            Err(EdgeError::gateway_timeout_caused("late", expected)),
        ]);
        let result = block_on(decode_gzip_stream(stream).try_collect::<Vec<_>>());
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
            decode_gzip_stream(source(vec![Ok(Bytes::from(encoded))])).try_collect::<Vec<_>>(),
        )
        .unwrap()
        .concat();
        assert_eq!(decoded, b"onetwo");

        let encoded_member = gzip(b"one");
        let result = block_on(
            decode_gzip_stream(source(vec![
                Ok(Bytes::from(encoded_member)),
                Err(EdgeError::bad_gateway_with_reason(
                    "late transport failure",
                    BadGatewayReason::Transport,
                )),
            ]))
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
