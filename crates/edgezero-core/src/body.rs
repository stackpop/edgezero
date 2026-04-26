use std::fmt;
use std::io;

use bytes::Bytes;
use futures_util::stream::{LocalBoxStream, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::EdgeError;

/// Lightweight HTTP body that can either contain a single `Bytes` buffer or a streaming source of
/// chunks. The streaming variant is implemented with `LocalBoxStream` so it remains compatible with
/// `wasm32` targets that lack thread support.
pub enum Body {
    Once(Bytes),
    Stream(LocalBoxStream<'static, Result<Bytes, anyhow::Error>>),
}

impl Body {
    #[must_use]
    pub fn empty() -> Self {
        Self::from_bytes(Bytes::new())
    }

    pub fn from_bytes<B>(bytes: B) -> Self
    where
        B: Into<Bytes>,
    {
        Self::Once(bytes.into())
    }

    pub fn from_stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + 'static,
        anyhow::Error: From<E>,
    {
        Self::Stream(
            stream
                .map(|res| res.map_err(anyhow::Error::from))
                .boxed_local(),
        )
    }

    pub fn stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Bytes> + 'static,
    {
        Self::Stream(stream.map(Ok::<Bytes, anyhow::Error>).boxed_local())
    }

    /// Returns the in-memory bytes for a buffered body, or `None` if this is
    /// a streaming body. To consume a streaming body into bytes, use
    /// [`Body::into_bytes_bounded`].
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Once(bytes) => Some(bytes.as_ref()),
            Body::Stream(_) => None,
        }
    }

    /// Consume a buffered body and return its bytes, or `None` if this is a
    /// streaming body. To collect a streaming body, use
    /// [`Body::into_bytes_bounded`].
    pub fn into_bytes(self) -> Option<Bytes> {
        match self {
            Body::Once(bytes) => Some(bytes),
            Body::Stream(_) => None,
        }
    }

    pub fn into_stream(self) -> Option<LocalBoxStream<'static, Result<Bytes, anyhow::Error>>> {
        match self {
            Body::Once(_) => None,
            Body::Stream(stream) => Some(stream),
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self, Body::Stream(_))
    }

    /// Drain the body into a single `Bytes` buffer, enforcing `max_size`.
    ///
    /// Works for both buffered and streaming variants.
    ///
    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the body exceeds `max_size` bytes; or [`EdgeError::internal`] if the upstream stream errors.
    pub async fn into_bytes_bounded(self, max_size: usize) -> Result<Bytes, EdgeError> {
        match self {
            Body::Once(bytes) => {
                if bytes.len() > max_size {
                    return Err(EdgeError::bad_request("request body too large"));
                }
                Ok(bytes)
            }
            Body::Stream(mut stream) => {
                let mut buf = Vec::new();
                while let Some(result) = StreamExt::next(&mut stream).await {
                    let chunk = result.map_err(EdgeError::internal)?;
                    buf.extend_from_slice(&chunk);
                    if buf.len() > max_size {
                        return Err(EdgeError::bad_request("request body too large"));
                    }
                }
                Ok(Bytes::from(buf))
            }
        }
    }

    pub fn text<S>(text: S) -> Self
    where
        S: Into<String>,
    {
        Self::from_bytes(text.into().into_bytes())
    }

    /// # Errors
    /// Returns the underlying [`serde_json::Error`] if `value` cannot be serialized.
    pub fn json<T>(value: &T) -> Result<Self, serde_json::Error>
    where
        T: Serialize,
    {
        serde_json::to_vec(value).map(Self::from_bytes)
    }

    /// # Errors
    /// Returns [`serde_json::Error`] if the body is streaming or its bytes are not valid JSON for `T`.
    pub fn to_json<T>(&self) -> Result<T, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        match self {
            Body::Once(bytes) => serde_json::from_slice(bytes.as_ref()),
            Body::Stream(_) => Err(serde_json::Error::io(io::Error::other(
                "streaming body cannot be materialised as JSON",
            ))),
        }
    }
}

impl Default for Body {
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Body::Once(bytes) => f
                .debug_struct("Body::Once")
                .field("len", &bytes.len())
                .finish(),
            Body::Stream(_) => f.debug_tuple("Body::Stream").finish(),
        }
    }
}

impl From<Vec<u8>> for Body {
    fn from(value: Vec<u8>) -> Self {
        Body::from_bytes(value)
    }
}

impl From<&[u8]> for Body {
    fn from(value: &[u8]) -> Self {
        Body::from_bytes(Bytes::copy_from_slice(value))
    }
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        Body::text(value)
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Body::text(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures_util::stream;
    use std::io;

    #[test]
    fn collect_stream_body() {
        let body = Body::stream(stream::iter(vec![
            Bytes::from_static(b"a"),
            Bytes::from_static(b"b"),
        ]));
        assert!(body.is_stream());
        let mut stream = body.into_stream().expect("stream");
        let collected = block_on(async {
            let mut data = Vec::new();
            while let Some(result) = stream.next().await {
                let chunk = result.expect("chunk");
                data.extend_from_slice(&chunk);
            }
            data
        });
        assert_eq!(collected, b"ab");
    }

    #[test]
    fn from_stream_maps_errors() {
        let source = stream::iter(vec![
            Ok(Bytes::from_static(b"ok")),
            Err(io::Error::other("boom")),
        ]);
        let body = Body::from_stream(source);
        let mut chunks = body.into_stream().expect("stream");
        let (first, second) = block_on(async {
            let first = chunks.next().await.expect("first").expect("ok");
            let second = chunks.next().await.expect("second");
            (first, second)
        });
        assert_eq!(first, Bytes::from_static(b"ok"));
        let err = second.expect_err("error");
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn to_json_fails_for_streaming_body() {
        let body = Body::stream(stream::iter(vec![
            Bytes::from_static(b"{"),
            Bytes::from_static(b"}"),
        ]));
        body.to_json::<serde_json::Value>()
            .expect_err("streaming body cannot deserialize as JSON");
    }

    #[test]
    fn into_bytes_returns_none_for_stream() {
        let body = Body::stream(stream::iter(vec![Bytes::from_static(b"data")]));
        assert!(body.into_bytes().is_none());
    }

    #[test]
    fn as_bytes_returns_none_for_stream() {
        let body = Body::stream(stream::iter(vec![Bytes::from_static(b"data")]));
        assert!(body.as_bytes().is_none());
    }

    #[test]
    fn into_stream_returns_none_for_buffered_body() {
        let body = Body::from("payload");
        assert!(body.into_stream().is_none());
    }

    #[test]
    fn is_stream_returns_false_for_buffered_body() {
        let body = Body::from("payload");
        assert!(!body.is_stream());
    }

    #[test]
    fn default_body_is_empty() {
        let body = Body::default();
        assert!(body.as_bytes().expect("buffered").is_empty());
    }

    #[test]
    fn debug_formats_both_body_variants() {
        let buffered = Body::from("payload");
        let buffered_debug = format!("{buffered:?}");
        assert!(buffered_debug.contains("Body::Once"));

        let stream = Body::stream(stream::iter(vec![Bytes::from_static(b"chunk")]));
        let stream_debug = format!("{stream:?}");
        assert!(stream_debug.contains("Body::Stream"));
    }

    #[test]
    fn from_vec_u8_builds_buffered_body() {
        let body = Body::from(vec![1_u8, 2_u8, 3_u8]);
        assert_eq!(body.as_bytes().expect("buffered"), &[1_u8, 2_u8, 3_u8]);
    }

    #[test]
    fn into_bytes_bounded_buffered_ok() {
        let body = Body::from("hello");
        let result = block_on(body.into_bytes_bounded(100));
        assert_eq!(result.unwrap(), Bytes::from("hello"));
    }

    #[test]
    fn into_bytes_bounded_buffered_too_large() {
        let body = Body::from("hello");
        block_on(body.into_bytes_bounded(3)).expect_err("body exceeds max_size");
    }

    #[test]
    fn into_bytes_bounded_stream_ok() {
        let body = Body::stream(stream::iter(vec![
            Bytes::from_static(b"ab"),
            Bytes::from_static(b"cd"),
        ]));
        let result = block_on(body.into_bytes_bounded(100));
        assert_eq!(result.unwrap(), Bytes::from("abcd"));
    }

    #[test]
    fn into_bytes_bounded_stream_too_large() {
        let body = Body::stream(stream::iter(vec![
            Bytes::from_static(b"ab"),
            Bytes::from_static(b"cd"),
        ]));
        block_on(body.into_bytes_bounded(3)).expect_err("stream exceeds max_size");
    }
}
