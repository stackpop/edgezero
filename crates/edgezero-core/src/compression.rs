use std::io;

use async_compression::futures::bufread::{
    BrotliDecoder, BrotliEncoder, DeflateDecoder, DeflateEncoder, GzipDecoder, GzipEncoder,
};
use async_stream::try_stream;
use bytes::Bytes;
use futures::io::{AsyncReadExt, BufReader};
use futures::stream::Stream;
use futures::TryStream;
use futures_util::TryStreamExt;

const BUFFER_SIZE: usize = 8 * 1024;

// ---------------------------------------------------------------------------
// Decoders
// ---------------------------------------------------------------------------

/// Decode a stream of gzip-compressed chunks into plain bytes.
pub fn decode_gzip_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(GzipDecoder::new(BufReader::new(stream.into_async_read())))
}

/// Decode a stream of brotli-compressed chunks into plain bytes.
pub fn decode_brotli_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(BrotliDecoder::new(BufReader::new(stream.into_async_read())))
}

/// Decode a stream of deflate-compressed chunks into plain bytes.
pub fn decode_deflate_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(DeflateDecoder::new(BufReader::new(
        stream.into_async_read(),
    )))
}

// ---------------------------------------------------------------------------
// Encoders
// ---------------------------------------------------------------------------

/// Compress a stream of plain bytes using gzip.
pub fn encode_gzip_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(GzipEncoder::new(BufReader::new(stream.into_async_read())))
}

/// Compress a stream of plain bytes using brotli.
pub fn encode_brotli_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(BrotliEncoder::new(BufReader::new(stream.into_async_read())))
}

/// Compress a stream of plain bytes using deflate.
pub fn encode_deflate_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    decode_stream(DeflateEncoder::new(BufReader::new(
        stream.into_async_read(),
    )))
}

// ---------------------------------------------------------------------------
// Shared reader drain
// ---------------------------------------------------------------------------

fn decode_stream<R>(reader: R) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    R: futures::io::AsyncRead + Unpin,
{
    try_stream! {
        let mut reader = reader;
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            yield Bytes::copy_from_slice(&buffer[..read]);
        }
    }
}

// ---------------------------------------------------------------------------
// ContentEncoding enum
// ---------------------------------------------------------------------------

/// Recognized `Content-Encoding` / `Accept-Encoding` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    Gzip,
    Brotli,
    Deflate,
    Identity,
}

impl ContentEncoding {
    /// Parse an encoding token (case-insensitive).
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gzip" | "x-gzip" => Some(Self::Gzip),
            "br" => Some(Self::Brotli),
            "deflate" => Some(Self::Deflate),
            "identity" => Some(Self::Identity),
            _ => None,
        }
    }

    /// The canonical token to use in `Content-Encoding` headers.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Brotli => "br",
            Self::Deflate => "deflate",
            Self::Identity => "identity",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::CompressorWriter;
    use flate2::{
        write::{DeflateEncoder as FlateDeflateEncoder, GzEncoder},
        Compression,
    };
    use futures::executor::block_on;
    use futures_util::{stream, TryStreamExt};
    use std::io::Write;

    // Helper: collect a decoded stream to Vec<u8>
    async fn collect_stream(
        s: impl Stream<Item = Result<Bytes, io::Error>>,
    ) -> Result<Vec<u8>, io::Error> {
        use futures_util::pin_mut;
        pin_mut!(s);
        let chunks: Vec<Bytes> = s.try_collect().await?;
        Ok(chunks.concat())
    }

    // ------- Decode tests -------

    #[test]
    fn decode_gzip_stream_yields_plain_bytes() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let compressed = encoder.finish().unwrap();

        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
        let decoded = block_on(collect_stream(decode_gzip_stream(s))).unwrap();
        assert_eq!(decoded, b"hello gzip");
    }

    #[test]
    fn decode_brotli_stream_yields_plain_bytes() {
        let mut brotli_bytes = Vec::new();
        {
            let mut compressor = CompressorWriter::new(&mut brotli_bytes, 4096, 5, 21);
            compressor.write_all(b"hello brotli").unwrap();
        }

        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(brotli_bytes)]);
        let decoded = block_on(collect_stream(decode_brotli_stream(s))).unwrap();
        assert_eq!(decoded, b"hello brotli");
    }

    #[test]
    fn decode_deflate_stream_yields_plain_bytes() {
        let mut encoder = FlateDeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello deflate").unwrap();
        let compressed = encoder.finish().unwrap();

        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
        let decoded = block_on(collect_stream(decode_deflate_stream(s))).unwrap();
        assert_eq!(decoded, b"hello deflate");
    }

    // ------- Encode tests (round-trip) -------

    #[test]
    fn encode_gzip_roundtrip() {
        let plain = b"round-trip gzip test data".to_vec();
        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(plain.clone())]);
        let compressed = block_on(collect_stream(encode_gzip_stream(s))).unwrap();
        assert_ne!(compressed, plain);

        let s2 = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
        let decoded = block_on(collect_stream(decode_gzip_stream(s2))).unwrap();
        assert_eq!(decoded, plain);
    }

    #[test]
    fn encode_brotli_roundtrip() {
        let plain = b"round-trip brotli test data".to_vec();
        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(plain.clone())]);
        let compressed = block_on(collect_stream(encode_brotli_stream(s))).unwrap();
        assert_ne!(compressed, plain);

        let s2 = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
        let decoded = block_on(collect_stream(decode_brotli_stream(s2))).unwrap();
        assert_eq!(decoded, plain);
    }

    #[test]
    fn encode_deflate_roundtrip() {
        let plain = b"round-trip deflate test data".to_vec();
        let s = stream::iter(vec![Ok::<Vec<u8>, io::Error>(plain.clone())]);
        let compressed = block_on(collect_stream(encode_deflate_stream(s))).unwrap();
        assert_ne!(compressed, plain);

        let s2 = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
        let decoded = block_on(collect_stream(decode_deflate_stream(s2))).unwrap();
        assert_eq!(decoded, plain);
    }

    // ------- ContentEncoding tests -------

    #[test]
    fn content_encoding_parse_variants() {
        assert_eq!(ContentEncoding::parse("gzip"), Some(ContentEncoding::Gzip));
        assert_eq!(
            ContentEncoding::parse("x-gzip"),
            Some(ContentEncoding::Gzip)
        );
        assert_eq!(ContentEncoding::parse("br"), Some(ContentEncoding::Brotli));
        assert_eq!(
            ContentEncoding::parse("deflate"),
            Some(ContentEncoding::Deflate)
        );
        assert_eq!(
            ContentEncoding::parse("identity"),
            Some(ContentEncoding::Identity)
        );
        assert_eq!(
            ContentEncoding::parse(" GZIP "),
            Some(ContentEncoding::Gzip)
        );
        assert_eq!(ContentEncoding::parse("unknown"), None);
    }

    #[test]
    fn content_encoding_as_str() {
        assert_eq!(ContentEncoding::Gzip.as_str(), "gzip");
        assert_eq!(ContentEncoding::Brotli.as_str(), "br");
        assert_eq!(ContentEncoding::Deflate.as_str(), "deflate");
        assert_eq!(ContentEncoding::Identity.as_str(), "identity");
    }
}
