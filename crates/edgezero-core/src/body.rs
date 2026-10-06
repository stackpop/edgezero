use std::fmt;
use std::io;
use std::num::NonZeroUsize;

use bytes::Bytes;
use futures_util::stream::{LocalBoxStream, Stream, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::EdgeError;

pub type BodyStream = LocalBoxStream<'static, Result<Bytes, EdgeError>>;

/// Lightweight HTTP body that can either contain a single `Bytes` buffer or a streaming source of
/// chunks. The streaming variant is implemented with `LocalBoxStream` so it remains compatible with
/// `wasm32` targets that lack thread support.
///
/// Raw matches outside core require a wildcard, even when all current variants are named:
///
/// ```compile_fail
/// use edgezero_core::Body;
/// fn kind(body: Body) -> usize {
///     match body {
///         Body::Once(_) => 0,
///         Body::Stream(_) => 1,
///         Body::Managed(_) => 2,
///     }
/// }
/// ```
///
/// ```
/// use edgezero_core::Body;
/// fn kind(body: Body) -> usize {
///     match body {
///         Body::Once(_) => 0,
///         Body::Stream(_) => 1,
///         Body::Managed(_) => 2,
///         _ => 3,
///     }
/// }
/// ```
#[non_exhaustive]
pub enum Body {
    Managed(ManagedBody),
    Once(Bytes),
    Stream(BodyStream),
}

/// Logical body ownership without ingress policy or cache provenance.
/// This view is exhaustive, so transports need no wildcard:
///
/// ```
/// use edgezero_core::{Body, BodyContent};
/// fn kind(body: Body) -> usize {
///     match body.into_content() {
///         BodyContent::Once(_) => 0,
///         BodyContent::Stream(_) => 1,
///     }
/// }
/// ```
pub enum BodyContent {
    Once(Bytes),
    Stream(BodyStream),
}

/// Opaque ingress policy and transfer provenance. Only core creates cached provenance.
pub struct ManagedBody {
    inner: Box<ManagedBodyState>,
}

struct ManagedBodyState {
    policy: BufferedJsonPolicy,
    transfer: BodyTransfer,
}

#[derive(Clone, Copy)]
pub(crate) struct BufferedJsonPolicy {
    ceiling: NonZeroUsize,
    ingress_is_json: bool,
}

impl BufferedJsonPolicy {
    pub(crate) fn ceiling(self) -> NonZeroUsize {
        self.ceiling
    }
    pub(crate) fn ingress_is_json(self) -> bool {
        self.ingress_is_json
    }
}

pub(crate) enum BodyTransfer {
    Cached(Bytes),
    Initial(BodyContent),
}

impl BodyTransfer {
    fn into_content(self) -> BodyContent {
        match self {
            Self::Initial(content) => content,
            Self::Cached(bytes) => BodyContent::Once(bytes),
        }
    }
}

impl From<BodyContent> for Body {
    #[inline]
    fn from(content: BodyContent) -> Self {
        match content {
            BodyContent::Once(bytes) => Self::Once(bytes),
            BodyContent::Stream(stream) => Self::Stream(stream),
        }
    }
}

#[expect(
    clippy::arbitrary_source_item_ordering,
    reason = "policy transfer helpers precede the existing logical content operations"
)]
impl Body {
    /// Attaches a validated positive ceiling and original ingress classification.
    /// Original policy/provenance win on reattachment. Never polls or copies content.
    #[must_use]
    #[inline]
    pub fn with_buffered_json_policy(self, ceiling: NonZeroUsize, ingress_is_json: bool) -> Self {
        match self {
            Self::Managed(_) => self,
            Self::Once(_) | Self::Stream(_) => Self::from_transfer(
                Some(BufferedJsonPolicy {
                    ceiling,
                    ingress_is_json,
                }),
                BodyTransfer::Initial(self.into_content()),
            ),
        }
    }

    /// Consumes raw content, discarding policy/provenance without polling.
    /// For application-owned operations and transports, not context reconstruction.
    #[must_use]
    #[inline]
    pub fn into_content(self) -> BodyContent {
        let (_, transfer) = self.into_transfer();
        transfer.into_content()
    }

    pub(crate) fn into_transfer(self) -> (Option<BufferedJsonPolicy>, BodyTransfer) {
        match self {
            Self::Once(bytes) => (None, BodyTransfer::Initial(BodyContent::Once(bytes))),
            Self::Stream(stream) => (None, BodyTransfer::Initial(BodyContent::Stream(stream))),
            Self::Managed(managed) => {
                let state = *managed.inner;
                (Some(state.policy), state.transfer)
            }
        }
    }

    pub(crate) fn from_transfer(
        policy: Option<BufferedJsonPolicy>,
        transfer: BodyTransfer,
    ) -> Self {
        match policy {
            Some(selected) => Self::Managed(ManagedBody {
                inner: Box::new(ManagedBodyState {
                    policy: selected,
                    transfer,
                }),
            }),
            None => transfer.into_content().into(),
        }
    }

    fn buffered_bytes(&self) -> Option<&Bytes> {
        match self {
            Self::Once(bytes) => Some(bytes),
            Self::Stream(_) => None,
            Self::Managed(managed) => match &managed.inner.transfer {
                BodyTransfer::Initial(BodyContent::Once(bytes)) | BodyTransfer::Cached(bytes) => {
                    Some(bytes)
                }
                BodyTransfer::Initial(BodyContent::Stream(_)) => None,
            },
        }
    }
    /// Returns the in-memory bytes for a buffered body, or `None` if this is
    /// a streaming body. To consume a streaming body into bytes, use
    /// [`Body::into_bytes_bounded`].
    #[inline]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.buffered_bytes().map(Bytes::as_ref)
    }

    #[must_use]
    #[inline]
    pub fn empty() -> Self {
        Self::from_bytes(Bytes::new())
    }

    #[inline]
    pub fn from_bytes<B>(bytes: B) -> Self
    where
        B: Into<Bytes>,
    {
        Self::Once(bytes.into())
    }

    #[inline]
    pub fn from_external_stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + 'static,
        anyhow::Error: From<E>,
    {
        Self::Stream(
            stream
                .map(|result| {
                    result.map_err(|error| EdgeError::internal(anyhow::Error::from(error)))
                })
                .boxed_local(),
        )
    }

    #[inline]
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, EdgeError>> + 'static,
    {
        Self::Stream(stream.boxed_local())
    }

    /// Consume a buffered body and return its bytes, or `None` if this is a
    /// streaming body. To collect a streaming body, use
    /// [`Body::into_bytes_bounded`].
    #[inline]
    pub fn into_bytes(self) -> Option<Bytes> {
        match self.into_content() {
            BodyContent::Once(bytes) => Some(bytes),
            BodyContent::Stream(_) => None,
        }
    }

    /// Drain the body into a single `Bytes` buffer, enforcing `max_size`.
    ///
    /// Works for both buffered and streaming variants.
    ///
    /// # Errors
    /// Returns [`EdgeError::bad_request`] if the body exceeds `max_size` bytes; or [`EdgeError::internal`] if the upstream stream errors.
    #[inline]
    pub async fn into_bytes_bounded(self, max_size: usize) -> Result<Bytes, EdgeError> {
        match self.into_content() {
            BodyContent::Once(bytes) => {
                if bytes.len() > max_size {
                    return Err(EdgeError::bad_request("request body too large"));
                }
                Ok(bytes)
            }
            BodyContent::Stream(mut stream) => {
                let mut buf = Vec::new();
                while let Some(result) = StreamExt::next(&mut stream).await {
                    let chunk = result?;
                    let next_len = buf.len().checked_add(chunk.len()).ok_or_else(|| {
                        EdgeError::bad_request("request body size accounting overflow")
                    })?;
                    if next_len > max_size {
                        return Err(EdgeError::bad_request("request body too large"));
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(Bytes::from(buf))
            }
        }
    }

    #[inline]
    pub fn into_stream(self) -> Option<BodyStream> {
        match self.into_content() {
            BodyContent::Once(_) => None,
            BodyContent::Stream(stream) => Some(stream),
        }
    }

    #[inline]
    pub fn is_stream(&self) -> bool {
        self.buffered_bytes().is_none()
    }

    /// # Errors
    /// Returns the underlying [`serde_json::Error`] if `value` cannot be serialized.
    #[inline]
    pub fn json<T>(value: &T) -> Result<Self, serde_json::Error>
    where
        T: Serialize,
    {
        serde_json::to_vec(value).map(Self::from_bytes)
    }

    #[inline]
    pub fn stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Bytes> + 'static,
    {
        Self::Stream(stream.map(Ok::<Bytes, EdgeError>).boxed_local())
    }

    #[inline]
    pub fn text<S>(text: S) -> Self
    where
        S: Into<String>,
    {
        Self::from_bytes(text.into().into_bytes())
    }

    /// # Errors
    /// Returns [`serde_json::Error`] if the body is streaming or its bytes are not valid JSON for `T`.
    #[inline]
    pub fn to_json<T>(&self) -> Result<T, serde_json::Error>
    where
        T: DeserializeOwned,
    {
        match self.buffered_bytes() {
            Some(bytes) => serde_json::from_slice(bytes.as_ref()),
            None => Err(serde_json::Error::io(io::Error::other(
                "streaming body cannot be materialised as JSON",
            ))),
        }
    }
}

impl Default for Body {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Debug for Body {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.buffered_bytes() {
            Some(bytes) => f
                .debug_struct("Body::Once")
                .field("len", &bytes.len())
                .finish(),
            None => f.debug_tuple("Body::Stream").finish(),
        }
    }
}

impl From<Bytes> for Body {
    #[inline]
    fn from(value: Bytes) -> Self {
        Body::Once(value)
    }
}

impl From<Vec<u8>> for Body {
    #[inline]
    fn from(value: Vec<u8>) -> Self {
        Body::from_bytes(value)
    }
}

impl From<&[u8]> for Body {
    #[inline]
    fn from(value: &[u8]) -> Self {
        Body::from_bytes(Bytes::copy_from_slice(value))
    }
}

impl From<&str> for Body {
    #[inline]
    fn from(value: &str) -> Self {
        Body::text(value)
    }
}

impl From<String> for Body {
    #[inline]
    fn from(value: String) -> Self {
        Body::text(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ResponseLimitReason;
    use crate::http::StatusCode;
    use futures::executor::block_on;
    use futures_util::stream;
    use std::cell::Cell;
    use std::io;
    use std::rc::Rc;
    use std::task::Poll;

    #[test]
    fn managed_content_accessors_and_manual_collection_ignore_ingress_ceiling() {
        let cap = NonZeroUsize::new(1).expect("positive cap");
        let bytes = Bytes::from_static(b"\"value\"");
        let buffered = Body::from_bytes(bytes.clone()).with_buffered_json_policy(cap, true);
        assert_eq!(buffered.as_bytes(), Some(bytes.as_ref()));
        assert!(!buffered.is_stream());
        assert_eq!(buffered.to_json::<String>().expect("raw JSON"), "value");
        assert!(format!("{buffered:?}").contains("Body::Once"));
        let moved = buffered.into_bytes().expect("bytes");
        assert_eq!(moved.as_ptr(), bytes.as_ptr());
        for make in [false, true] {
            let raw = || {
                if make {
                    Body::stream(stream::iter([bytes.clone()]))
                } else {
                    Body::from_bytes(bytes.clone())
                }
            };
            assert_eq!(
                block_on(
                    raw()
                        .with_buffered_json_policy(cap, true)
                        .into_bytes_bounded(7)
                )
                .expect("caller cap"),
                bytes
            );
            assert_eq!(
                block_on(
                    raw()
                        .with_buffered_json_policy(cap, true)
                        .into_bytes_bounded(6)
                )
                .expect_err("manual overflow")
                .status(),
                StatusCode::BAD_REQUEST
            );
        }
        let polls = Rc::new(Cell::new(0_usize));
        let observed = Rc::clone(&polls);
        let lazy = Body::from_stream(stream::poll_fn(move |_| {
            observed.set(observed.get().saturating_add(1));
            Poll::Ready(None)
        }))
        .with_buffered_json_policy(cap, true);
        assert!(lazy.is_stream());
        assert!(lazy.as_bytes().is_none());
        assert!(matches!(lazy.into_content(), BodyContent::Stream(_)));
        assert_eq!(polls.get(), 0);
    }

    #[test]
    fn as_bytes_returns_none_for_stream() {
        let body = Body::stream(stream::iter(vec![Bytes::from_static(b"data")]));
        assert!(body.as_bytes().is_none());
    }

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
    fn debug_formats_both_body_variants() {
        let buffered = Body::from("payload");
        let buffered_debug = format!("{buffered:?}");
        assert!(buffered_debug.contains("Body::Once"));

        let stream = Body::stream(stream::iter(vec![Bytes::from_static(b"chunk")]));
        let stream_debug = format!("{stream:?}");
        assert!(stream_debug.contains("Body::Stream"));
    }

    #[test]
    fn default_body_is_empty() {
        let body = Body::default();
        assert!(body.as_bytes().expect("buffered").is_empty());
    }

    #[test]
    fn body_from_external_stream_maps_to_internal() {
        let source = stream::iter(vec![
            Ok(Bytes::from_static(b"ok")),
            Err(io::Error::other("boom")),
        ]);
        let body = Body::from_external_stream(source);
        let mut chunks = body.into_stream().expect("stream");
        let (first, second) = block_on(async {
            let first = chunks.next().await.expect("first").expect("ok");
            let second = chunks.next().await.expect("second");
            (first, second)
        });
        assert_eq!(first, Bytes::from_static(b"ok"));
        let err = second.expect_err("error");
        assert!(matches!(err, EdgeError::Internal { .. }));
        assert!(err.to_string().contains("boom"));
    }

    #[test]
    fn body_from_stream_preserves_edge_error() {
        let source = stream::iter([Err(EdgeError::response_too_large_with_reason(
            "encoded cap",
            ResponseLimitReason::EncodedBody,
        ))]);
        let body = Body::from_stream(source);
        let mut chunks = body.into_stream().expect("stream");
        let error = block_on(chunks.next())
            .expect("item")
            .expect_err("typed error");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::EncodedBody,
                ..
            }
        ));
    }

    #[test]
    fn body_into_bytes_bounded_preserves_edge_error() {
        let source = stream::iter([Err(EdgeError::gateway_timeout("expired"))]);
        let error =
            block_on(Body::from_stream(source).into_bytes_bounded(100)).expect_err("typed error");
        assert!(matches!(error, EdgeError::GatewayTimeout { .. }));
    }

    #[test]
    fn body_stream_accepts_infallible_bytes() {
        let body = Body::stream(stream::iter([
            Bytes::from_static(b"one"),
            Bytes::from_static(b"two"),
        ]));
        let bytes = block_on(body.into_bytes_bounded(6)).expect("body");
        assert_eq!(bytes, Bytes::from_static(b"onetwo"));
        assert_eq!(
            Body::from(Bytes::from_static(b"bytes")).as_bytes(),
            Some(b"bytes".as_slice())
        );
    }

    #[test]
    fn body_bounded_checks_before_append() {
        let polls = Rc::new(Cell::new(0_usize));
        let observed = Rc::clone(&polls);
        let source = stream::iter([
            Ok(Bytes::from_static(b"too-large")),
            Ok(Bytes::from_static(b"must-not-poll")),
        ])
        .inspect(move |_| observed.set(observed.get().saturating_add(1)));
        let error =
            block_on(Body::from_stream(source).into_bytes_bounded(3)).expect_err("over limit");
        assert!(matches!(error, EdgeError::BadRequest { .. }));
        assert_eq!(polls.get(), 1);
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

    #[test]
    fn into_bytes_returns_none_for_stream() {
        let body = Body::stream(stream::iter(vec![Bytes::from_static(b"data")]));
        assert!(body.into_bytes().is_none());
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
    fn to_json_fails_for_streaming_body() {
        let body = Body::stream(stream::iter(vec![
            Bytes::from_static(b"{"),
            Bytes::from_static(b"}"),
        ]));
        body.to_json::<serde_json::Value>()
            .expect_err("streaming body cannot deserialize as JSON");
    }
}
