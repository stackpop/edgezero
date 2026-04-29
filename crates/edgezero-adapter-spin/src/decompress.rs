// Used by proxy.rs (wasm32-gated) and tests; not reachable on native non-test builds.
#![allow(
    dead_code,
    reason = "wasm32-gated callers; native non-test build has no consumer"
)]

use edgezero_core::error::EdgeError;
use flate2::read::GzDecoder;
use std::io::Read as _;

/// Maximum decompressed body size (64 MiB). Prevents zip-bomb attacks
/// where a small compressed payload expands to exhaust WASI memory.
///
/// This is intentionally larger than `MAX_BODY_SIZE` (16 MiB) in the response
/// module: proxy responses are untrusted external data that may legitimately
/// decompress to a larger size, while response streams originate from the
/// app's own handlers.
const MAX_DECOMPRESSED_SIZE: usize = 64 * 1024 * 1024;
/// Same value as [`MAX_DECOMPRESSED_SIZE`] expressed as `u64` for the
/// `Read::take` API. Defined as a sibling constant so neither callsite
/// needs a numeric conversion.
const MAX_DECOMPRESSED_SIZE_U64: u64 = 64 * 1024 * 1024;

/// Decompress a buffered body based on the `Content-Encoding` value.
///
/// Since Spin bodies are already fully buffered, we use synchronous
/// decompression (`flate2`, `brotli`) directly on the byte slice. This
/// avoids wrapping in an async stream and calling `block_on` inside
/// the WASI single-threaded async executor, which risks deadlock.
///
/// The output is capped at [`MAX_DECOMPRESSED_SIZE`] to guard against
/// zip-bomb payloads.
pub(crate) fn decompress_body(body: Vec<u8>, encoding: Option<&str>) -> Result<Vec<u8>, EdgeError> {
    match encoding {
        Some("gzip") => {
            let mut decoder = GzDecoder::new(body.as_slice());
            let mut output = Vec::with_capacity(body.len().min(MAX_DECOMPRESSED_SIZE));
            decoder
                .by_ref()
                .take(MAX_DECOMPRESSED_SIZE_U64.saturating_add(1))
                .read_to_end(&mut output)
                .map_err(|e| {
                    EdgeError::internal(anyhow::anyhow!("gzip decompression failed: {e}"))
                })?;
            if output.len() > MAX_DECOMPRESSED_SIZE {
                return Err(EdgeError::internal(anyhow::anyhow!(
                    "decompressed body exceeds maximum size of {MAX_DECOMPRESSED_SIZE} bytes"
                )));
            }
            Ok(output)
        }
        Some("br") => {
            let mut decoder = brotli::Decompressor::new(body.as_slice(), 8192);
            let mut output = Vec::with_capacity(body.len().min(MAX_DECOMPRESSED_SIZE));
            decoder
                .by_ref()
                .take(MAX_DECOMPRESSED_SIZE_U64.saturating_add(1))
                .read_to_end(&mut output)
                .map_err(|e| {
                    EdgeError::internal(anyhow::anyhow!("brotli decompression failed: {e}"))
                })?;
            if output.len() > MAX_DECOMPRESSED_SIZE {
                return Err(EdgeError::internal(anyhow::anyhow!(
                    "decompressed body exceeds maximum size of {MAX_DECOMPRESSED_SIZE} bytes"
                )));
            }
            Ok(output)
        }
        _ => Ok(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write as _;

    #[test]
    fn decompress_body_handles_identity() {
        let plain = b"hello plain".to_vec();
        let none_encoding = decompress_body(plain.clone(), None).unwrap();
        assert_eq!(none_encoding, plain);

        let identity_encoding = decompress_body(plain.clone(), Some("identity")).unwrap();
        assert_eq!(identity_encoding, plain);
    }

    #[test]
    fn decompress_body_handles_gzip() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let compressed = encoder.finish().unwrap();

        let result = decompress_body(compressed, Some("gzip")).unwrap();
        assert_eq!(result, b"hello gzip");
    }

    #[test]
    fn decompress_body_handles_brotli() {
        let mut compressed = Vec::new();
        let mut compressor = brotli::CompressorWriter::new(&mut compressed, 4096, 5, 21);
        compressor.write_all(b"hello brotli").unwrap();
        drop(compressor);

        let result = decompress_body(compressed, Some("br")).unwrap();
        assert_eq!(result, b"hello brotli");
    }

    #[test]
    fn decompress_body_rejects_zip_bomb() {
        // Create a gzip payload that decompresses to more than MAX_DECOMPRESSED_SIZE.
        // We compress a stream of zeros which compresses extremely well.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        let zeros = vec![0_u8; 1024 * 1024]; // 1 MiB chunk
        for _ in 0_i32..65_i32 {
            encoder.write_all(&zeros).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        let result = decompress_body(compressed, Some("gzip"));
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("exceeds maximum size"),
            "expected zip-bomb error, got: {err_msg}"
        );
    }
}
