use std::io;

use async_compression::futures::bufread::{BrotliDecoder, GzipDecoder};
use async_stream::try_stream;
use bytes::Bytes;
use futures::io::{AsyncReadExt, BufReader};
use futures::stream::Stream;
use futures::TryStream;
use futures_util::TryStreamExt;

const BUFFER_SIZE: usize = 8 * 1024;

/// Decode a stream of gzip-compressed chunks into plain bytes.
pub fn decode_gzip_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    try_stream! {
        let reader = BufReader::new(stream.into_async_read());
        let mut decoder = GzipDecoder::new(reader);
        let mut buffer = vec![0u8; BUFFER_SIZE];

        loop {
            let read = decoder.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            yield Bytes::copy_from_slice(&buffer[..read]);
        }
    }
}

/// Decode a stream of brotli-compressed chunks into plain bytes.
pub fn decode_brotli_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, io::Error>>
where
    S: TryStream<Ok = Vec<u8>, Error = io::Error> + Unpin,
{
    try_stream! {
        let reader = BufReader::new(stream.into_async_read());
        let mut decoder = BrotliDecoder::new(reader);
        let mut buffer = vec![0u8; BUFFER_SIZE];

        loop {
            let read = decoder.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            yield Bytes::copy_from_slice(&buffer[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::CompressorWriter;
    use flate2::{write::GzEncoder, Compression};
    use futures::executor::block_on;
    use futures_util::{stream, TryStreamExt};
    use std::io::Write;

    #[test]
    fn decode_gzip_stream_yields_plain_bytes() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let compressed = encoder.finish().unwrap();

        let stream = stream::iter(vec![Ok::<Vec<u8>, io::Error>(compressed)]);
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
        let mut brotli_bytes = Vec::new();
        {
            let mut compressor = CompressorWriter::new(&mut brotli_bytes, 4096, 5, 21);
            compressor.write_all(b"hello brotli").unwrap();
        }

        let stream = stream::iter(vec![Ok::<Vec<u8>, io::Error>(brotli_bytes)]);
        let decoded = block_on(async {
            decode_brotli_stream(stream)
                .try_collect::<Vec<Bytes>>()
                .await
                .map(|chunks| chunks.concat())
        })
        .unwrap();

        assert_eq!(decoded, b"hello brotli");
    }
}
