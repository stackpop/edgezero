use std::net::IpAddr;
use std::num::NonZeroU64;
use std::str;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::body::{Body, BodyStream};
use crate::compression::ContentEncoding;
use crate::error::{BadGatewayDecodeReason, BadGatewayReason, EdgeError, ResponseLimitReason};
use crate::http::header::{
    CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST, PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use crate::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri,
    response_builder,
};
use crate::time::Deadline;

pub const DEFAULT_MAX_BROTLI_DECODER_BYTES: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_OUTBOUND_REQUEST_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// Response header identifying the adapter that completed an outbound request.
pub const PROXY_HEADER: &str = "x-edgezero-proxy";

#[derive(Clone, Copy)]
pub(crate) struct BudgetInputs {
    pub deadline: Option<Deadline>,
    pub timeout: Option<Duration>,
}

#[derive(Debug)]
pub struct OutboundRequest {
    body: Body,
    deadline: Option<Deadline>,
    headers: HeaderMap,
    max_brotli_decoder_bytes: u64,
    max_brotli_window_bits: u8,
    max_chunk_bytes: Option<NonZeroU64>,
    max_decoded_response_bytes: Option<u64>,
    max_encoded_response_bytes: Option<u64>,
    max_request_body_bytes: u64,
    max_response_header_bytes: Option<u64>,
    max_response_header_count: Option<u64>,
    method: Method,
    response_mode: ResponseMode,
    timeout: Option<Duration>,
    uri: Uri,
}

#[derive(Debug)]
pub struct OutboundRequestParts {
    pub body: Body,
    pub deadline: Option<Deadline>,
    pub headers: HeaderMap,
    pub max_brotli_decoder_bytes: u64,
    pub max_brotli_window_bits: u8,
    pub max_chunk_bytes: Option<NonZeroU64>,
    pub max_decoded_response_bytes: Option<u64>,
    pub max_encoded_response_bytes: Option<u64>,
    pub max_request_body_bytes: u64,
    pub max_response_header_bytes: Option<u64>,
    pub max_response_header_count: Option<u64>,
    pub method: Method,
    pub response_mode: ResponseMode,
    pub timeout: Option<Duration>,
    pub uri: Uri,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseMode {
    Buffered { max_bytes: u64 },
    Streamed,
}

#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<dyn OutboundHttpClient>,
}

#[async_trait(?Send)]
pub trait OutboundHttpClient: Send + Sync {
    /// Sends one outbound request.
    ///
    /// # Errors
    /// Returns a typed request, transport, deadline, or response-policy failure.
    async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError>;

    async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult>;
}

#[derive(Debug)]
pub struct OutboundResponse {
    body: Body,
    headers: HeaderMap,
    request_method: Method,
    status: StatusCode,
}

#[derive(Debug)]
#[non_exhaustive]
pub struct OutboundSlotResult {
    pub elapsed: Duration,
    pub outcome: Result<OutboundResponse, EdgeError>,
}

impl OutboundSlotResult {
    #[must_use]
    #[inline]
    pub fn new(elapsed: Duration, outcome: Result<OutboundResponse, EdgeError>) -> Self {
        Self { elapsed, outcome }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseBodyDisposition {
    FramingBodyless,
    Payload,
    ResetContent { declared_body: bool },
}

pub struct ResponseHeaderLimiter {
    max_bytes: Option<u64>,
    max_count: Option<u64>,
    observed_bytes: u64,
    observed_count: u64,
}

impl ResponseHeaderLimiter {
    #[must_use]
    #[inline]
    pub fn new(max_bytes: Option<u64>, max_count: Option<u64>) -> Self {
        Self {
            max_bytes,
            max_count,
            observed_bytes: 0,
            observed_count: 0,
        }
    }

    /// Adds one guest-visible field section to the cumulative response-header budget.
    ///
    /// # Errors
    /// Returns a typed response limit error on count, byte, or accounting overflow.
    #[inline]
    pub fn observe(&mut self, headers: &HeaderMap) -> Result<(), EdgeError> {
        for (name, value) in headers {
            let next_count = self
                .observed_count
                .checked_add(1)
                .ok_or_else(|| response_limit_error(ResponseLimitReason::HeaderCount))?;
            self.observed_count = next_count;
            if self.max_count.is_some_and(|max| next_count > max) {
                return Err(response_limit_error(ResponseLimitReason::HeaderCount));
            }

            let name_bytes = u64::try_from(name.as_str().len()).unwrap_or(u64::MAX);
            let value_bytes = u64::try_from(value.as_bytes().len()).unwrap_or(u64::MAX);
            let next_bytes = self
                .observed_bytes
                .checked_add(name_bytes)
                .and_then(|total| total.checked_add(value_bytes))
                .ok_or_else(|| response_limit_error(ResponseLimitReason::HeaderBytes))?;
            self.observed_bytes = next_bytes;
            if self.max_bytes.is_some_and(|max| next_bytes > max) {
                return Err(response_limit_error(ResponseLimitReason::HeaderBytes));
            }
        }
        Ok(())
    }
}

impl HttpClient {
    #[inline]
    pub fn new(client: Arc<dyn OutboundHttpClient>) -> Self {
        Self { inner: client }
    }

    /// Delegates one request to the adapter-owned outbound implementation.
    ///
    /// # Errors
    /// Returns the adapter's typed outbound failure unchanged.
    #[inline]
    pub async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
        self.inner.send(request).await
    }

    #[inline]
    pub async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
        self.inner.send_all(requests).await
    }

    #[inline]
    pub fn with_client<Client>(client: Client) -> Self
    where
        Client: OutboundHttpClient + 'static,
    {
        Self::new(Arc::new(client))
    }
}

impl OutboundResponse {
    #[must_use]
    #[inline]
    pub fn body(&self) -> &Body {
        &self.body
    }

    #[must_use]
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[must_use]
    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[must_use]
    #[inline]
    pub fn into_body(self) -> Body {
        self.body
    }

    /// Collects the body under a final response-buffer limit.
    ///
    /// # Errors
    /// Returns a typed source error or `BufferedBody` response-limit error.
    #[inline]
    pub async fn into_bytes_bounded(self, max: u64) -> Result<Bytes, EdgeError> {
        match self.body {
            Body::Once(bytes) => validate_buffered_response_bytes(bytes, max),
            Body::Stream(stream) => collect_response_stream(stream, max).await,
        }
    }

    /// Collects the body under a final response-buffer limit and cooperative deadline.
    ///
    /// # Errors
    /// Returns 504 when the deadline is observed expired, otherwise a typed source or
    /// `BufferedBody` response-limit error.
    #[inline]
    pub async fn into_bytes_bounded_until(
        self,
        max: u64,
        deadline: Deadline,
    ) -> Result<Bytes, EdgeError> {
        ensure_deadline_live(deadline)?;
        match self.body {
            Body::Once(bytes) => {
                let outcome = validate_buffered_response_bytes(bytes, max);
                ensure_deadline_live(deadline)?;
                outcome
            }
            Body::Stream(stream) => collect_response_stream_until(stream, max, deadline).await,
        }
    }

    #[must_use]
    #[inline]
    pub fn into_parts(self) -> (Method, StatusCode, HeaderMap, Body) {
        (self.request_method, self.status, self.headers, self.body)
    }

    /// Converts the outbound value after one final defensive metadata pass.
    ///
    /// # Errors
    /// Returns a typed protocol error for malformed framing, or an internal error if the
    /// already-validated response cannot be assembled.
    #[inline]
    pub fn into_response(mut self) -> Result<Response, EdgeError> {
        normalize_response_headers(&self.request_method, self.status, &mut self.headers)?;
        let mut builder = response_builder().status(self.status);
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        builder.body(self.body).map_err(EdgeError::internal)
    }

    #[must_use]
    #[inline]
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// Deserializes an already-buffered upstream body as JSON.
    ///
    /// # Errors
    /// Returns protocol 502 for a streamed body and decode 502 for malformed JSON.
    #[inline]
    pub fn json<Value>(&self) -> Result<Value, EdgeError>
    where
        Value: DeserializeOwned,
    {
        let Body::Once(bytes) = &self.body else {
            return Err(EdgeError::bad_gateway_with_reason(
                "response body not buffered; use json_bounded(max) or json_bounded_until(max, deadline)",
                BadGatewayReason::Protocol,
            ));
        };
        decode_json(bytes)
    }

    /// Collects and deserializes an upstream body as JSON.
    ///
    /// # Errors
    /// Returns typed source/limit failures or decode 502 for malformed JSON.
    #[inline]
    pub async fn json_bounded<Value>(self, max: u64) -> Result<Value, EdgeError>
    where
        Value: DeserializeOwned,
    {
        let bytes = self.into_bytes_bounded(max).await?;
        decode_json(&bytes)
    }

    /// Collects and deserializes an upstream body under a cooperative deadline.
    ///
    /// # Errors
    /// Returns 504 on observed expiry, typed source/limit failures, or decode 502 for malformed
    /// JSON.
    #[inline]
    pub async fn json_bounded_until<Value>(
        self,
        max: u64,
        deadline: Deadline,
    ) -> Result<Value, EdgeError>
    where
        Value: DeserializeOwned,
    {
        let bytes = self.into_bytes_bounded_until(max, deadline).await?;
        ensure_deadline_live(deadline)?;
        decode_json(&bytes)
    }

    #[must_use]
    #[inline]
    pub fn new(request_method: Method, status: StatusCode, headers: HeaderMap, body: Body) -> Self {
        Self {
            body,
            headers,
            request_method,
            status,
        }
    }

    #[must_use]
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl OutboundRequest {
    #[must_use]
    #[inline]
    pub fn backend_target(&self) -> String {
        let host = bracket_ipv6(self.host_name());
        format!("{host}:{}", self.resolved_port())
    }

    #[must_use]
    #[inline]
    pub fn body<BodyValue>(mut self, body: BodyValue) -> Self
    where
        BodyValue: Into<Body>,
    {
        self.body = body.into();
        self
    }

    pub(crate) fn budget_inputs(&self) -> BudgetInputs {
        BudgetInputs {
            deadline: self.deadline,
            timeout: self.timeout,
        }
    }

    #[must_use]
    #[inline]
    pub fn cert_host(&self) -> Option<&str> {
        (self.uri.scheme_str() == Some("https")).then(|| self.host_name())
    }

    #[must_use]
    #[inline]
    pub fn deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Reassembles a request and revalidates its target.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when `parts.uri` is not a canonicalizable HTTP(S)
    /// target.
    #[inline]
    pub fn from_parts(parts: OutboundRequestParts) -> Result<Self, EdgeError> {
        let canonical_uri = canonicalize_typed_uri(&parts.uri)?;
        Ok(Self {
            body: parts.body,
            deadline: parts.deadline,
            headers: parts.headers,
            max_brotli_decoder_bytes: parts.max_brotli_decoder_bytes,
            max_brotli_window_bits: parts.max_brotli_window_bits,
            max_chunk_bytes: parts.max_chunk_bytes,
            max_decoded_response_bytes: parts.max_decoded_response_bytes,
            max_encoded_response_bytes: parts.max_encoded_response_bytes,
            max_request_body_bytes: parts.max_request_body_bytes,
            max_response_header_bytes: parts.max_response_header_bytes,
            max_response_header_count: parts.max_response_header_count,
            method: parts.method,
            response_mode: parts.response_mode,
            timeout: parts.timeout,
            uri: canonical_uri,
        })
    }

    fn from_raw(method: Method, raw: &str) -> Result<Self, EdgeError> {
        reject_ambiguous_raw_target(raw)?;
        let uri = canonicalize_url(raw)?;
        Ok(Self::with_canonical_uri(method, uri))
    }

    /// Creates an outbound request from an inbound request while removing framing and
    /// hop-by-hop metadata.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when the target is invalid or a `connection`
    /// nomination is malformed.
    #[inline]
    pub fn from_request(request: Request, target: Uri) -> Result<Self, EdgeError> {
        let (parts, body) = request.into_parts();
        let mut outbound = Self::new(parts.method, target)?;
        outbound.body = body;
        outbound.headers = parts.headers;
        normalize_request_headers(&mut outbound.headers)?;
        Ok(outbound)
    }

    /// Creates a canonical GET request from a raw URL.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when the URL is not an absolute HTTP(S) target.
    #[inline]
    pub fn get<Target>(uri: Target) -> Result<Self, EdgeError>
    where
        Target: AsRef<str>,
    {
        Self::from_raw(Method::GET, uri.as_ref())
    }

    /// Appends one validated header value.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when the name is invalid, the value is not UTF-8,
    /// or the value contains bytes forbidden by HTTP header syntax.
    #[inline]
    pub fn header<N, V>(mut self, name: N, value: V) -> Result<Self, EdgeError>
    where
        N: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let header_name = HeaderName::from_bytes(name.as_ref())
            .map_err(|_error| EdgeError::bad_request("invalid outbound header name"))?;
        if str::from_utf8(value.as_ref()).is_err() {
            return Err(EdgeError::bad_request(format!(
                "header value is not valid UTF-8: {header_name}"
            )));
        }
        let header_value = HeaderValue::from_bytes(value.as_ref()).map_err(|_error| {
            EdgeError::bad_request(format!(
                "header value contains forbidden bytes: {header_name}"
            ))
        })?;
        self.headers.append(header_name, header_value);
        Ok(self)
    }

    #[must_use]
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[must_use]
    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    #[must_use]
    #[inline]
    pub fn host_authority(&self) -> String {
        self.uri
            .authority()
            .map_or_else(String::new, |authority| authority.as_str().to_owned())
    }

    #[must_use]
    #[inline]
    pub fn host_name(&self) -> &str {
        let host = self.uri.host().unwrap_or_default();
        host.strip_prefix('[')
            .and_then(|unbracketed| unbracketed.strip_suffix(']'))
            .unwrap_or(host)
    }

    #[must_use]
    #[inline]
    pub fn into_parts(self) -> OutboundRequestParts {
        OutboundRequestParts {
            body: self.body,
            deadline: self.deadline,
            headers: self.headers,
            max_brotli_decoder_bytes: self.max_brotli_decoder_bytes,
            max_brotli_window_bits: self.max_brotli_window_bits,
            max_chunk_bytes: self.max_chunk_bytes,
            max_decoded_response_bytes: self.max_decoded_response_bytes,
            max_encoded_response_bytes: self.max_encoded_response_bytes,
            max_request_body_bytes: self.max_request_body_bytes,
            max_response_header_bytes: self.max_response_header_bytes,
            max_response_header_count: self.max_response_header_count,
            method: self.method,
            response_mode: self.response_mode,
            timeout: self.timeout,
            uri: self.uri,
        }
    }

    #[must_use]
    #[inline]
    pub fn is_stream_body(&self) -> bool {
        self.body.is_stream()
    }

    #[must_use]
    #[inline]
    pub fn is_stream_response(&self) -> bool {
        self.response_mode == ResponseMode::Streamed
    }

    /// Serializes a request body as JSON.
    ///
    /// # Errors
    /// Returns [`EdgeError::Internal`] when serialization fails.
    #[inline]
    pub fn json<T: Serialize>(mut self, value: &T) -> Result<Self, EdgeError> {
        self.body = Body::json(value).map_err(EdgeError::internal)?;
        if !self.headers.contains_key(CONTENT_TYPE) {
            self.headers
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
        Ok(self)
    }

    #[must_use]
    #[inline]
    pub fn max_brotli_decoder_bytes(mut self, bytes: u64) -> Self {
        self.max_brotli_decoder_bytes = bytes;
        self
    }

    #[must_use]
    #[inline]
    pub fn max_brotli_window_bits(mut self, bits: u8) -> Self {
        self.max_brotli_window_bits = bits;
        self
    }

    #[must_use]
    #[inline]
    pub fn max_chunk_bytes(mut self, bytes: NonZeroU64) -> Self {
        self.max_chunk_bytes = Some(bytes);
        self
    }

    #[must_use]
    #[inline]
    pub fn max_decoded_response_bytes(mut self, bytes: u64) -> Self {
        self.max_decoded_response_bytes = Some(bytes);
        self
    }

    #[must_use]
    #[inline]
    pub fn max_encoded_response_bytes(mut self, bytes: u64) -> Self {
        self.max_encoded_response_bytes = Some(bytes);
        self
    }

    #[must_use]
    #[inline]
    pub fn max_request_body_bytes(mut self, bytes: u64) -> Self {
        self.max_request_body_bytes = bytes;
        self
    }

    #[must_use]
    #[inline]
    pub fn max_response_bytes(mut self, bytes: u64) -> Self {
        self.response_mode = ResponseMode::Buffered { max_bytes: bytes };
        self
    }

    #[must_use]
    #[inline]
    pub fn max_response_header_bytes(mut self, bytes: u64) -> Self {
        self.max_response_header_bytes = Some(bytes);
        self
    }

    #[must_use]
    #[inline]
    pub fn max_response_header_count(mut self, count: u64) -> Self {
        self.max_response_header_count = Some(count);
        self
    }

    #[must_use]
    #[inline]
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Creates an outbound request from a typed URI.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when the URI is not an absolute HTTP(S) target.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the public request constructor takes ownership consistently with http::Request"
    )]
    #[inline]
    pub fn new(method: Method, uri: Uri) -> Result<Self, EdgeError> {
        let canonical_uri = canonicalize_typed_uri(&uri)?;
        Ok(Self::with_canonical_uri(method, canonical_uri))
    }

    /// Creates a canonical POST request from a raw URL.
    ///
    /// # Errors
    /// Returns [`EdgeError::BadRequest`] when the URL is not an absolute HTTP(S) target.
    #[inline]
    pub fn post<Target>(uri: Target) -> Result<Self, EdgeError>
    where
        Target: AsRef<str>,
    {
        Self::from_raw(Method::POST, uri.as_ref())
    }

    fn resolved_port(&self) -> u16 {
        self.uri.port_u16().unwrap_or_else(|| {
            if self.uri.scheme_str() == Some("https") {
                443
            } else {
                80
            }
        })
    }

    #[must_use]
    #[inline]
    pub fn sni_hostname(&self) -> Option<&str> {
        let host = self.host_name();
        (self.uri.scheme_str() == Some("https") && host.parse::<IpAddr>().is_err()).then_some(host)
    }

    #[must_use]
    #[inline]
    pub fn stream_response(mut self) -> Self {
        self.response_mode = ResponseMode::Streamed;
        self
    }

    #[must_use]
    #[inline]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    #[inline]
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    fn with_canonical_uri(method: Method, uri: Uri) -> Self {
        Self {
            body: Body::empty(),
            deadline: None,
            headers: HeaderMap::new(),
            max_brotli_decoder_bytes: DEFAULT_MAX_BROTLI_DECODER_BYTES,
            max_brotli_window_bits: 24,
            max_chunk_bytes: None,
            max_decoded_response_bytes: None,
            max_encoded_response_bytes: None,
            max_request_body_bytes: DEFAULT_OUTBOUND_REQUEST_BODY_BYTES,
            max_response_header_bytes: None,
            max_response_header_count: None,
            method,
            response_mode: ResponseMode::Buffered {
                max_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            },
            timeout: None,
            uri,
        }
    }
}

/// Collects a response stream under the final buffered-body cap.
///
/// # Errors
/// Returns a typed source error unchanged or a `BufferedBody` response-limit error.
#[inline]
pub async fn collect_response_stream(stream: BodyStream, max: u64) -> Result<Bytes, EdgeError> {
    collect_response_stream_inner(stream, max, None).await
}

/// Applies sound pre-poll checks to a normalized payload `content-length`.
///
/// # Errors
/// Returns a protocol 502 for malformed length metadata or a typed response-limit 502 when
/// the declared wire/final size exceeds a comparable configured cap.
#[inline]
pub fn enforce_payload_content_length(
    headers: &HeaderMap,
    encoding: ContentEncoding,
    max_buffered_bytes: Option<u64>,
    max_decoded_bytes: Option<u64>,
    max_encoded_bytes: Option<u64>,
) -> Result<(), EdgeError> {
    let Some(length) = parse_content_length(headers)? else {
        return Ok(());
    };
    if max_encoded_bytes.is_some_and(|max| length > max) {
        return Err(response_limit_error(ResponseLimitReason::EncodedBody));
    }
    match encoding {
        ContentEncoding::Identity => {
            if max_decoded_bytes.is_some_and(|max| length > max) {
                return Err(response_limit_error(ResponseLimitReason::DecodedBody));
            }
            if max_buffered_bytes.is_some_and(|max| length > max) {
                return Err(response_limit_error(ResponseLimitReason::BufferedBody));
            }
        }
        ContentEncoding::Passthrough => {
            if max_buffered_bytes.is_some_and(|max| length > max) {
                return Err(response_limit_error(ResponseLimitReason::BufferedBody));
            }
        }
        ContentEncoding::Brotli | ContentEncoding::Gzip => {}
    }
    Ok(())
}

#[must_use]
#[inline]
pub fn limit_decoded_stream(stream: BodyStream, max: Option<u64>) -> BodyStream {
    limit_response_stream(stream, max, ResponseLimitReason::DecodedBody)
}

#[must_use]
#[inline]
pub fn limit_encoded_stream(stream: BodyStream, max: Option<u64>) -> BodyStream {
    limit_response_stream(stream, max, ResponseLimitReason::EncodedBody)
}

/// Reapplies request header normalization immediately before platform conversion.
///
/// # Errors
/// Returns [`EdgeError::BadRequest`] when a `connection` nomination is malformed.
#[inline]
pub fn normalize_for_dispatch(request: &mut OutboundRequest) -> Result<(), EdgeError> {
    normalize_request_headers(&mut request.headers)
}

/// Normalizes raw upstream metadata before response-body policy is selected.
///
/// # Errors
/// Returns a protocol-classified 502 when `connection` or payload framing is malformed.
#[inline]
pub fn normalize_response_headers(
    request_method: &Method,
    status: StatusCode,
    headers: &mut HeaderMap,
) -> Result<ResponseBodyDisposition, EdgeError> {
    let nominated = connection_nominations(headers, true)?;
    retain_utf8_header_values(headers, true);
    strip_hop_by_hop(headers, nominated, false);

    if request_method == Method::HEAD
        || status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        if status.is_informational() || status == StatusCode::NO_CONTENT {
            headers.remove(CONTENT_LENGTH);
        }
        return Ok(ResponseBodyDisposition::FramingBodyless);
    }

    let content_length = parse_content_length(headers)?;
    if status == StatusCode::RESET_CONTENT {
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
        return Ok(ResponseBodyDisposition::ResetContent {
            declared_body: content_length.is_some_and(|length| length > 0),
        });
    }
    Ok(ResponseBodyDisposition::Payload)
}

#[must_use]
#[inline]
pub fn rechunk_stream(mut source: BodyStream, optional_maximum: Option<NonZeroU64>) -> BodyStream {
    let Some(maximum) = optional_maximum else {
        return source;
    };
    let maximum_chunk = usize::try_from(maximum.get()).unwrap_or(usize::MAX);
    stream! {
        while let Some(item) = source.next().await {
            match item {
                Ok(mut bytes) => {
                    if bytes.is_empty() {
                        yield Ok(Bytes::new());
                        continue;
                    }
                    while bytes.len() > maximum_chunk {
                        yield Ok(bytes.split_to(maximum_chunk));
                    }
                    if !bytes.is_empty() {
                        yield Ok(bytes);
                    }
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
    .boxed_local()
}

/// Validates the portable outbound method, body, and response policy contract.
///
/// # Errors
/// Returns [`EdgeError::BadRequest`] when the request cannot be represented consistently on
/// every adapter.
#[inline]
pub fn validate_for_dispatch(request: &OutboundRequest) -> Result<(), EdgeError> {
    if !matches!(
        request.method,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
    ) {
        return Err(EdgeError::bad_request(format!(
            "method {} is not portable; supported: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS",
            request.method
        )));
    }

    if matches!(request.method, Method::GET | Method::HEAD) {
        match &request.body {
            Body::Once(bytes) if bytes.is_empty() => {}
            Body::Once(_) => {
                return Err(EdgeError::bad_request(
                    "GET/HEAD request must not carry a body",
                ));
            }
            Body::Stream(_) => {
                return Err(EdgeError::bad_request(
                    "GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream",
                ));
            }
        }
    }

    if !(10..=30).contains(&request.max_brotli_window_bits) {
        return Err(EdgeError::bad_request(
            "max_brotli_window_bits must be between 10 and 30 inclusive",
        ));
    }
    Ok(())
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn canonicalize_typed_uri(uri: &Uri) -> Result<Uri, EdgeError> {
    canonicalize_url(&uri.to_string())
}

fn canonicalize_url(raw: &str) -> Result<Uri, EdgeError> {
    let mut parsed = Url::parse(raw)
        .map_err(|_error| EdgeError::bad_request("outbound URI must be absolute with authority"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(EdgeError::bad_request(
            "outbound URI scheme must be http or https",
        ));
    }
    if parsed.host_str().is_none() {
        return Err(EdgeError::bad_request(
            "outbound URI must be absolute with authority",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EdgeError::bad_request(
            "outbound URI must not contain userinfo; pass credentials via the `authorization` header",
        ));
    }
    if parsed.fragment().is_some() {
        return Err(EdgeError::bad_request(
            "outbound URI must not contain a fragment",
        ));
    }
    let default_port = if parsed.scheme() == "http" { 80 } else { 443 };
    if parsed.port() == Some(default_port) {
        parsed
            .set_port(None)
            .map_err(|()| EdgeError::bad_request("outbound URI contains an invalid port"))?;
    }
    parsed
        .as_str()
        .parse()
        .map_err(|_error| EdgeError::bad_request("outbound URI cannot be represented as HTTP URI"))
}

async fn collect_response_stream_inner(
    mut source: BodyStream,
    max: u64,
    optional_deadline: Option<Deadline>,
) -> Result<Bytes, EdgeError> {
    let mut collected = Vec::new();
    let mut total = 0_u64;
    loop {
        if let Some(active_deadline) = optional_deadline {
            ensure_deadline_live(active_deadline)?;
        }
        let next_item = source.next().await;
        if let Some(active_deadline) = optional_deadline {
            ensure_deadline_live(active_deadline)?;
        }
        let Some(item) = next_item else {
            return Ok(Bytes::from(collected));
        };
        let bytes = item?;
        let chunk_len = u64::try_from(bytes.len())
            .map_err(|_error| response_limit_error(ResponseLimitReason::BufferedBody))?;
        let next_total = total
            .checked_add(chunk_len)
            .ok_or_else(|| response_limit_error(ResponseLimitReason::BufferedBody))?;
        if next_total > max {
            if let Some(active_deadline) = optional_deadline {
                ensure_deadline_live(active_deadline)?;
            }
            return Err(response_limit_error(ResponseLimitReason::BufferedBody));
        }
        collected.extend_from_slice(&bytes);
        total = next_total;
    }
}

async fn collect_response_stream_until(
    stream: BodyStream,
    max: u64,
    deadline: Deadline,
) -> Result<Bytes, EdgeError> {
    collect_response_stream_inner(stream, max, Some(deadline)).await
}

fn decode_json<Value>(bytes: &[u8]) -> Result<Value, EdgeError>
where
    Value: DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|error| {
        EdgeError::bad_gateway_with_reason(
            format!("failed to decode upstream JSON: {error}"),
            BadGatewayReason::Decode(BadGatewayDecodeReason::Json),
        )
    })
}

fn ensure_deadline_live(deadline: Deadline) -> Result<(), EdgeError> {
    if deadline.is_expired() {
        Err(EdgeError::gateway_timeout("response body deadline expired"))
    } else {
        Ok(())
    }
}

fn normalize_request_headers(headers: &mut HeaderMap) -> Result<(), EdgeError> {
    let nominated = connection_nominations(headers, false)?;
    retain_utf8_header_values(headers, false);
    strip_hop_by_hop(headers, nominated, true);
    Ok(())
}

fn connection_nominations(
    headers: &HeaderMap,
    response: bool,
) -> Result<Vec<HeaderName>, EdgeError> {
    let mut nominated = Vec::new();
    for header_value in headers.get_all(CONNECTION) {
        let raw_value = str::from_utf8(header_value.as_bytes()).map_err(|_error| {
            malformed_connection(response, "connection header value is not valid UTF-8")
        })?;
        for raw_token in raw_value.split(',') {
            let token = raw_token.trim();
            if token.is_empty() {
                return Err(malformed_connection(
                    response,
                    "connection header contains an empty nomination",
                ));
            }
            let name = HeaderName::from_bytes(token.as_bytes()).map_err(|_error| {
                malformed_connection(response, "connection header contains an invalid nomination")
            })?;
            nominated.push(name);
        }
    }
    Ok(nominated)
}

fn malformed_connection(response: bool, message: &'static str) -> EdgeError {
    if response {
        EdgeError::bad_gateway_with_reason(message, BadGatewayReason::Protocol)
    } else {
        EdgeError::bad_request(message)
    }
}

fn limit_response_stream(
    mut source: BodyStream,
    optional_maximum: Option<u64>,
    reason: ResponseLimitReason,
) -> BodyStream {
    let Some(maximum) = optional_maximum else {
        return source;
    };
    stream! {
        let mut total = 0_u64;
        while let Some(item) = source.next().await {
            match item {
                Ok(bytes) => {
                    let Ok(chunk_len) = u64::try_from(bytes.len()) else {
                        yield Err(response_limit_error(reason));
                        return;
                    };
                    let Some(next_total) = total.checked_add(chunk_len) else {
                        yield Err(response_limit_error(reason));
                        return;
                    };
                    if next_total > maximum {
                        yield Err(response_limit_error(reason));
                        return;
                    }
                    total = next_total;
                    yield Ok(bytes);
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
    }
    .boxed_local()
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, EdgeError> {
    let mut parsed = None;
    for value in headers.get_all(CONTENT_LENGTH) {
        let raw = str::from_utf8(value.as_bytes()).map_err(|_error| protocol_content_length())?;
        if raw.contains(',') {
            return Err(protocol_content_length());
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(protocol_content_length());
        }
        let length = trimmed
            .parse::<u64>()
            .map_err(|_error| protocol_content_length())?;
        if parsed.is_some_and(|previous| previous != length) {
            return Err(protocol_content_length());
        }
        parsed = Some(length);
    }
    Ok(parsed)
}

fn protocol_content_length() -> EdgeError {
    EdgeError::bad_gateway_with_reason(
        "upstream response has malformed or conflicting content-length",
        BadGatewayReason::Protocol,
    )
}

fn response_limit_error(reason: ResponseLimitReason) -> EdgeError {
    EdgeError::response_too_large_with_reason("upstream response exceeded configured limit", reason)
}

fn retain_utf8_header_values(headers: &mut HeaderMap, response: bool) {
    let mut retained = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        if name != CONNECTION
            && name != CONTENT_ENCODING
            && str::from_utf8(value.as_bytes()).is_err()
        {
            if response {
                log::warn!("dropping non-UTF-8 outbound response header: {name}");
            } else {
                log::warn!("dropping non-UTF-8 outbound request header: {name}");
            }
            continue;
        }
        retained.append(name.clone(), value.clone());
    }
    *headers = retained;
}

fn strip_hop_by_hop(
    headers: &mut HeaderMap,
    nominated: Vec<HeaderName>,
    strip_request_framing: bool,
) {
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        CONNECTION,
        HeaderName::from_static("keep-alive"),
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(name);
    }
    if strip_request_framing {
        headers.remove(HOST);
        headers.remove(CONTENT_LENGTH);
    }
}

fn validate_buffered_response_bytes(bytes: Bytes, max: u64) -> Result<Bytes, EdgeError> {
    let length = u64::try_from(bytes.len())
        .map_err(|_error| response_limit_error(ResponseLimitReason::BufferedBody))?;
    if length > max {
        Err(response_limit_error(ResponseLimitReason::BufferedBody))
    } else {
        Ok(bytes)
    }
}

fn reject_ambiguous_raw_target(raw: &str) -> Result<(), EdgeError> {
    if raw.as_bytes().contains(&b'#') {
        return Err(EdgeError::bad_request(
            "outbound URI must not contain a fragment",
        ));
    }
    if raw.as_bytes().contains(&b'\\') {
        return Err(EdgeError::bad_request(
            "outbound URI must not contain a backslash",
        ));
    }
    let Some((_scheme, authority_tail)) = raw.split_once("://") else {
        return Err(EdgeError::bad_request(
            "outbound URI must be absolute with authority",
        ));
    };
    let authority = authority_tail
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return Err(EdgeError::bad_request(
            "outbound URI must be absolute with authority",
        ));
    }
    if authority.as_bytes().contains(&b'@') {
        return Err(EdgeError::bad_request(
            "outbound URI must not contain userinfo; pass credentials via the `authorization` header",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::executor::block_on;
    use futures_util::{StreamExt as _, stream};

    use crate::body::Body;
    use crate::compression::{ContentEncoding, classify_content_encoding};
    use crate::error::{BudgetSource, EdgeError, ResponseLimitReason};
    use crate::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, request_builder};
    use crate::time::{
        DEADLINE_FAR_FUTURE, DEFAULT_NO_DEADLINE_BUDGET, Deadline, MonotonicInstant,
        dispatch_budget,
    };

    use super::{
        DEFAULT_MAX_BROTLI_DECODER_BYTES, DEFAULT_MAX_RESPONSE_BYTES,
        DEFAULT_OUTBOUND_REQUEST_BODY_BYTES, HttpClient, OutboundHttpClient, OutboundRequest,
        OutboundResponse, OutboundSlotResult, ResponseBodyDisposition, ResponseHeaderLimiter,
        ResponseMode, collect_response_stream, enforce_payload_content_length,
        limit_decoded_stream, limit_encoded_stream, normalize_for_dispatch,
        normalize_response_headers, rechunk_stream, validate_for_dispatch,
    };

    struct MockClient {
        batch_calls: AtomicUsize,
        send_calls: AtomicUsize,
    }

    #[async_trait(?Send)]
    impl OutboundHttpClient for MockClient {
        async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError> {
            self.send_calls.fetch_add(1, Ordering::Relaxed);
            Ok(OutboundResponse::new(
                request.method().clone(),
                StatusCode::CREATED,
                HeaderMap::new(),
                Body::from("single"),
            ))
        }

        async fn send_all(&self, requests: Vec<OutboundRequest>) -> Vec<OutboundSlotResult> {
            self.batch_calls.fetch_add(1, Ordering::Relaxed);
            requests
                .into_iter()
                .enumerate()
                .map(|(index, request)| OutboundSlotResult {
                    elapsed: Duration::from_millis(
                        u64::try_from(index)
                            .expect("index")
                            .checked_add(1)
                            .expect("elapsed"),
                    ),
                    outcome: if request.method() == Method::DELETE {
                        Err(EdgeError::bad_gateway("mock failure"))
                    } else {
                        Ok(OutboundResponse::new(
                            request.method().clone(),
                            StatusCode::OK,
                            HeaderMap::new(),
                            Body::empty(),
                        ))
                    },
                })
                .collect()
        }
    }

    fn bad_request_message(error: EdgeError) -> String {
        match error {
            EdgeError::BadRequest { message } => message,
            other => panic!("expected bad request, got {other:?}"),
        }
    }

    fn response_with_body(body: Body) -> OutboundResponse {
        OutboundResponse::new(Method::GET, StatusCode::OK, HeaderMap::new(), body)
    }

    #[test]
    fn buffered_collection_limit_is_independent() {
        let source = Body::from_stream(stream::iter([
            Ok(Bytes::from_static(b"ab")),
            Ok(Bytes::from_static(b"cd")),
        ]))
        .into_stream()
        .expect("stream");
        let bytes = block_on(collect_response_stream(source, 4)).expect("exact cap");
        assert_eq!(bytes, Bytes::from_static(b"abcd"));

        let error =
            block_on(response_with_body(Body::from("raw-passthrough")).into_bytes_bounded(3))
                .expect_err("final cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BufferedBody,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_budget_rejects_expired_or_zero() {
        let now = MonotonicInstant::now();
        let expired = OutboundRequest::get("https://example.com")
            .expect("request")
            .deadline(Deadline::at_instant(now));
        let error = dispatch_budget(&expired, now).expect_err("expired");
        assert!(matches!(
            error,
            EdgeError::GatewayTimeout {
                cause: BudgetSource::BatchDeadline,
                ..
            }
        ));

        let tied = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::ZERO)
            .deadline(Deadline::at_instant(now));
        let error = dispatch_budget(&tied, now).expect_err("zero tie");
        assert!(matches!(
            error,
            EdgeError::GatewayTimeout {
                cause: BudgetSource::PerCallTimeout,
                ..
            }
        ));
    }

    #[test]
    fn dispatch_budget_selects_and_attributes_minimum() {
        let now = MonotonicInstant::now();

        let default = OutboundRequest::get("https://example.com").expect("request");
        let budget = dispatch_budget(&default, now).expect("default budget");
        assert_eq!(budget.cause, BudgetSource::Default);
        assert_eq!(budget.duration, DEFAULT_NO_DEADLINE_BUDGET);
        assert_eq!(
            budget.deadline.instant(),
            now.checked_add(DEFAULT_NO_DEADLINE_BUDGET)
                .expect("no overflow")
        );

        let timeout = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::from_secs(3));
        let budget = dispatch_budget(&timeout, now).expect("timeout budget");
        assert_eq!(budget.cause, BudgetSource::PerCallTimeout);
        assert_eq!(budget.duration, Duration::from_secs(3));

        let deadline = Deadline::at_instant(
            now.checked_add(Duration::from_secs(4))
                .expect("no overflow"),
        );
        let request = OutboundRequest::get("https://example.com")
            .expect("request")
            .deadline(deadline);
        let budget = dispatch_budget(&request, now).expect("deadline budget");
        assert_eq!(budget.cause, BudgetSource::BatchDeadline);
        assert_eq!(budget.duration, Duration::from_secs(4));
        assert_eq!(budget.deadline.instant(), deadline.instant());

        let timeout_wins = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::from_secs(2))
            .deadline(deadline);
        let budget = dispatch_budget(&timeout_wins, now).expect("timeout wins");
        assert_eq!(budget.cause, BudgetSource::PerCallTimeout);
        assert_eq!(budget.duration, Duration::from_secs(2));

        let deadline_wins = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::from_secs(5))
            .deadline(deadline);
        let budget = dispatch_budget(&deadline_wins, now).expect("deadline wins");
        assert_eq!(budget.cause, BudgetSource::BatchDeadline);
        assert_eq!(budget.duration, Duration::from_secs(4));

        let tied_deadline = Deadline::at_instant(
            now.checked_add(Duration::from_secs(5))
                .expect("no overflow"),
        );
        let tied = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::from_secs(5))
            .deadline(tied_deadline);
        let budget = dispatch_budget(&tied, now).expect("equal budget");
        assert_eq!(budget.cause, BudgetSource::PerCallTimeout);

        let huge_timeout = OutboundRequest::get("https://example.com")
            .expect("request")
            .timeout(Duration::MAX);
        let budget = dispatch_budget(&huge_timeout, now).expect("clamped timeout");
        assert_eq!(budget.duration, DEADLINE_FAR_FUTURE);

        let far_deadline = Deadline::at_instant(
            now.checked_add(Duration::from_hours(8_760))
                .expect("no overflow"),
        );
        let far = OutboundRequest::get("https://example.com")
            .expect("request")
            .deadline(far_deadline);
        let budget = dispatch_budget(&far, now).expect("clamped deadline");
        assert_eq!(budget.duration, DEADLINE_FAR_FUTURE);
        assert_eq!(budget.cause, BudgetSource::BatchDeadline);
    }

    #[test]
    fn encoded_limit_stops_before_decoder() {
        let polls = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let observed = std::rc::Rc::clone(&polls);
        let source = Body::from_stream(
            stream::iter([
                Ok(Bytes::from_static(b"four")),
                Ok(Bytes::from_static(b"later")),
            ])
            .inspect(move |_| observed.set(observed.get().saturating_add(1))),
        )
        .into_stream()
        .expect("stream");
        let mut limited = limit_encoded_stream(source, Some(3));
        let error = block_on(limited.next())
            .expect("limit result")
            .expect_err("over limit");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::EncodedBody,
                ..
            }
        ));
        assert_eq!(polls.get(), 1);
        assert!(block_on(limited.next()).is_none());
    }

    #[test]
    fn http_client_delegates_send_and_send_all() {
        let client = HttpClient::with_client(MockClient {
            batch_calls: AtomicUsize::new(0),
            send_calls: AtomicUsize::new(0),
        });

        let response = block_on(
            client.send(
                OutboundRequest::post("https://example.com")
                    .expect("request")
                    .body("request"),
            ),
        )
        .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.body().as_bytes(), Some(b"single".as_slice()));

        let empty = block_on(client.send_all(Vec::new()));
        assert!(empty.is_empty());
    }

    #[test]
    fn http_client_preserves_per_slot_elapsed() {
        let client = HttpClient::with_client(MockClient {
            batch_calls: AtomicUsize::new(0),
            send_calls: AtomicUsize::new(0),
        });
        let requests = vec![
            OutboundRequest::get("https://example.com/one").expect("one"),
            OutboundRequest::new(Method::DELETE, Uri::from_static("https://example.com/two"))
                .expect("two"),
            OutboundRequest::post("https://example.com/three").expect("three"),
        ];

        let results = block_on(client.send_all(requests));
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].elapsed, Duration::from_millis(1));
        assert_eq!(results[1].elapsed, Duration::from_millis(2));
        assert_eq!(results[2].elapsed, Duration::from_millis(3));
        assert!(results[0].outcome.is_ok());
        assert!(matches!(
            results[1].outcome,
            Err(EdgeError::BadGateway { .. })
        ));
        assert!(results[2].outcome.is_ok());
    }

    #[test]
    fn outbound_response_parts_preserve_method_headers_and_body() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        let mut response = OutboundResponse::new(
            Method::HEAD,
            StatusCode::CREATED,
            headers,
            Body::from("payload"),
        );
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(response.is_success());
        assert_eq!(response.body().as_bytes(), Some(b"payload".as_slice()));
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        response
            .headers_mut()
            .append("x-adapter", HeaderValue::from_static("yes"));

        let (method, status, headers, body) = response.into_parts();
        assert_eq!(method, Method::HEAD);
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            headers.get("x-adapter"),
            Some(&HeaderValue::from_static("yes"))
        );
        assert_eq!(body.as_bytes(), Some(b"payload".as_slice()));

        let body = OutboundResponse::new(
            Method::GET,
            StatusCode::OK,
            HeaderMap::new(),
            Body::from("owned"),
        )
        .into_body();
        assert_eq!(body.as_bytes(), Some(b"owned".as_slice()));
    }

    #[test]
    fn payload_content_length_explicit_identity_rejects_before_body_poll() {
        let mut headers = HeaderMap::new();
        headers.append("content-encoding", HeaderValue::from_static("identity"));
        headers.append("content-length", HeaderValue::from_static("11"));
        let error = enforce_payload_content_length(
            &headers,
            ContentEncoding::Identity,
            Some(20),
            Some(10),
            Some(20),
        )
        .expect_err("decoded cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::DecodedBody,
                ..
            }
        ));
    }

    #[test]
    fn payload_content_length_passthrough_uses_buffered_not_decoded_cap() {
        let mut headers = HeaderMap::new();
        headers.append("content-length", HeaderValue::from_static("15"));
        enforce_payload_content_length(
            &headers,
            ContentEncoding::Passthrough,
            Some(20),
            Some(10),
            Some(20),
        )
        .expect("decoded cap bypassed");

        let error = enforce_payload_content_length(
            &headers,
            ContentEncoding::Passthrough,
            Some(14),
            Some(100),
            Some(20),
        )
        .expect_err("buffered cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::BufferedBody,
                ..
            }
        ));
    }

    #[test]
    fn payload_content_length_rejects_before_body_poll() {
        let mut headers = HeaderMap::new();
        headers.append("content-length", HeaderValue::from_static("21"));
        let error = enforce_payload_content_length(
            &headers,
            ContentEncoding::Identity,
            Some(30),
            Some(30),
            Some(20),
        )
        .expect_err("encoded cap");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::EncodedBody,
                ..
            }
        ));

        for value in ["", "+1", "-1", "bad", "1, 1"] {
            let mut headers = HeaderMap::new();
            headers.append(
                "content-length",
                HeaderValue::from_bytes(value.as_bytes()).expect("header"),
            );
            assert!(
                enforce_payload_content_length(
                    &headers,
                    ContentEncoding::Identity,
                    None,
                    None,
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn payload_content_length_skips_output_caps_for_compressed_body() {
        let mut headers = HeaderMap::new();
        headers.append("content-length", HeaderValue::from_static("100"));
        for encoding in [ContentEncoding::Brotli, ContentEncoding::Gzip] {
            enforce_payload_content_length(&headers, encoding, Some(1), Some(1), Some(100))
                .expect("wire length is not output length");
        }
    }

    #[test]
    fn rechunk_stream_is_lazy_and_ordered() {
        let source = Body::from_stream(stream::iter([
            Ok(Bytes::from_static(b"abcde")),
            Ok(Bytes::new()),
            Err(EdgeError::bad_gateway("source")),
        ]))
        .into_stream()
        .expect("stream");
        let mut chunks = rechunk_stream(source, NonZeroU64::new(2));
        let values = block_on(async {
            let mut values = Vec::new();
            while let Some(item) = chunks.next().await {
                match item {
                    Ok(bytes) => values.push(bytes),
                    Err(error) => {
                        assert!(matches!(error, EdgeError::BadGateway { .. }));
                        break;
                    }
                }
            }
            values
        });
        assert_eq!(
            values,
            vec![
                Bytes::from_static(b"ab"),
                Bytes::from_static(b"cd"),
                Bytes::from_static(b"e"),
                Bytes::new(),
            ]
        );
    }

    #[test]
    fn response_header_limiter_accumulates_field_sections() {
        let mut limiter = ResponseHeaderLimiter::new(Some(12), Some(3));
        let mut first = HeaderMap::new();
        first.append("a", HeaderValue::from_static("123"));
        limiter.observe(&first).expect("first section");

        let mut second = HeaderMap::new();
        second.append("b", HeaderValue::from_static("456"));
        limiter.observe(&second).expect("second section");

        let mut third = HeaderMap::new();
        third.append("c", HeaderValue::from_static("789"));
        limiter.observe(&third).expect("third section");

        let mut fourth = HeaderMap::new();
        fourth.append("d", HeaderValue::from_static("0"));
        let error = limiter.observe(&fourth).expect_err("cumulative count");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::HeaderCount,
                ..
            }
        ));
    }

    #[test]
    fn response_resource_header_limit_count_wins_tie() {
        let mut limiter = ResponseHeaderLimiter::new(Some(0), Some(0));
        let mut headers = HeaderMap::new();
        headers.append("x", HeaderValue::from_static("y"));
        let error = limiter.observe(&headers).expect_err("both limits");
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::HeaderCount,
                ..
            }
        ));
    }

    #[test]
    fn decoded_limit_bypasses_raw_passthrough() {
        let source = Body::from_stream(stream::iter([
            Ok(Bytes::from_static(b"ab")),
            Ok(Bytes::from_static(b"cd")),
        ]))
        .into_stream()
        .expect("stream");
        let error = block_on(async {
            let mut limited = limit_decoded_stream(source, Some(3));
            assert_eq!(limited.next().await.expect("first").expect("bytes"), "ab");
            limited.next().await.expect("second").expect_err("over cap")
        });
        assert!(matches!(
            error,
            EdgeError::ResponseTooLarge {
                reason: ResponseLimitReason::DecodedBody,
                ..
            }
        ));

        let raw = Body::from_stream(stream::iter([Ok(Bytes::from_static(b"raw-body"))]))
            .into_stream()
            .expect("raw passthrough");
        let bytes = block_on(async { raw.collect::<Vec<_>>().await });
        assert_eq!(bytes[0].as_ref().expect("raw").as_ref(), b"raw-body");
    }

    #[test]
    fn outbound_response_into_response_reapplies_normalization() {
        let mut headers = HeaderMap::new();
        headers.append("connection", HeaderValue::from_static("x-private"));
        headers.append("x-private", HeaderValue::from_static("secret"));
        headers.append("set-cookie", HeaderValue::from_static("a=1"));
        headers.append("set-cookie", HeaderValue::from_static("b=2"));
        let body = Body::stream(stream::iter([Bytes::from_static(b"lazy")]));
        let response = OutboundResponse::new(Method::GET, StatusCode::OK, headers, body)
            .into_response()
            .expect("response");

        assert!(response.headers().get("connection").is_none());
        assert!(response.headers().get("x-private").is_none());
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        assert!(response.into_body().is_stream());
    }

    #[test]
    fn outbound_response_json_error_classification() {
        let response = response_with_body(Body::from(r#"{"ok":true}"#));
        let value: serde_json::Value = response.json().expect("json");
        assert_eq!(value["ok"], true);

        let malformed = response_with_body(Body::from("not-json"));
        let error = malformed
            .json::<serde_json::Value>()
            .expect_err("malformed JSON");
        assert!(matches!(
            error,
            EdgeError::BadGateway {
                reason: crate::error::BadGatewayReason::Decode(
                    crate::error::BadGatewayDecodeReason::Json
                ),
                ..
            }
        ));

        let streamed = response_with_body(Body::stream(stream::iter([Bytes::from_static(b"{}")])))
            .json::<serde_json::Value>()
            .expect_err("stream requires bounded helper");
        assert!(matches!(
            streamed,
            EdgeError::BadGateway {
                reason: crate::error::BadGatewayReason::Protocol,
                ..
            }
        ));

        let streamed = response_with_body(Body::stream(stream::iter([Bytes::from_static(b"{}")])));
        let value: serde_json::Value = block_on(streamed.json_bounded(2)).expect("bounded json");
        assert_eq!(value, serde_json::json!({}));
    }

    #[test]
    fn outbound_response_until_deadline_wins_ready_result() {
        let deadline = Deadline::at_instant(MonotonicInstant::now());
        let response = response_with_body(Body::from_stream(stream::iter([Err(
            EdgeError::bad_gateway("ready source error"),
        )])));
        let error = block_on(response.into_bytes_bounded_until(100, deadline))
            .expect_err("expired deadline");
        assert!(matches!(
            error,
            EdgeError::GatewayTimeout {
                cause: BudgetSource::Unspecified,
                ..
            }
        ));
    }

    #[test]
    fn response_normalization_precedence_table() {
        let mut nominated = HeaderMap::new();
        nominated.append(
            "connection",
            HeaderValue::from_static("content-encoding, content-length"),
        );
        nominated.append("content-encoding", HeaderValue::from_static("gzip"));
        nominated.append("content-length", HeaderValue::from_static("malformed"));
        nominated.append("keep-alive", HeaderValue::from_static("timeout=5"));
        assert_eq!(
            normalize_response_headers(&Method::GET, StatusCode::OK, &mut nominated)
                .expect("nominations precede semantics"),
            ResponseBodyDisposition::Payload
        );
        assert!(nominated.get("content-encoding").is_none());
        assert!(nominated.get("content-length").is_none());
        assert!(nominated.get("keep-alive").is_none());

        let mut ambiguous_encoding = HeaderMap::new();
        ambiguous_encoding.append("content-encoding", HeaderValue::from_static("gzip"));
        ambiguous_encoding.append(
            "content-encoding",
            HeaderValue::from_bytes(&[0xff]).expect("opaque header value"),
        );
        normalize_response_headers(&Method::GET, StatusCode::OK, &mut ambiguous_encoding)
            .expect("invalid encoding sibling must remain visible to classification");
        assert_eq!(
            classify_content_encoding(&ambiguous_encoding),
            ContentEncoding::Passthrough
        );
        assert_eq!(
            ambiguous_encoding
                .get_all("content-encoding")
                .iter()
                .count(),
            2
        );

        for malformed in ["x-good,,x-other", "bad name", ",x-good", "x-good,"] {
            let mut headers = HeaderMap::new();
            headers.append(
                "connection",
                HeaderValue::from_bytes(malformed.as_bytes()).expect("header"),
            );
            let error = normalize_response_headers(&Method::GET, StatusCode::OK, &mut headers)
                .expect_err("malformed connection");
            assert!(matches!(
                error,
                EdgeError::BadGateway {
                    reason: crate::error::BadGatewayReason::Protocol,
                    ..
                }
            ));
        }

        let mut head = HeaderMap::new();
        head.append("content-length", HeaderValue::from_static("not-a-number"));
        head.append("content-encoding", HeaderValue::from_static("gzip"));
        assert_eq!(
            normalize_response_headers(&Method::HEAD, StatusCode::OK, &mut head)
                .expect("HEAD metadata"),
            ResponseBodyDisposition::FramingBodyless
        );
        assert_eq!(head.get("content-length").expect("length"), "not-a-number");

        for status in [
            StatusCode::CONTINUE,
            StatusCode::NO_CONTENT,
            StatusCode::NOT_MODIFIED,
        ] {
            let mut headers = HeaderMap::new();
            headers.append("content-length", HeaderValue::from_static("7"));
            assert_eq!(
                normalize_response_headers(&Method::GET, status, &mut headers).expect("bodyless"),
                ResponseBodyDisposition::FramingBodyless
            );
            if status == StatusCode::NOT_MODIFIED {
                assert_eq!(headers.get("content-length").expect("metadata"), "7");
            } else {
                assert!(headers.get("content-length").is_none());
            }
        }

        for (value, declared_body) in [(None, false), (Some("0"), false), (Some("7"), true)] {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.append(
                    "content-length",
                    HeaderValue::from_bytes(value.as_bytes()).expect("length"),
                );
            }
            assert_eq!(
                normalize_response_headers(&Method::GET, StatusCode::RESET_CONTENT, &mut headers)
                    .expect("205"),
                ResponseBodyDisposition::ResetContent { declared_body }
            );
            assert_eq!(headers.get("content-length").expect("normalized"), "0");
        }

        let mut conflicting = HeaderMap::new();
        conflicting.append("content-length", HeaderValue::from_static("7"));
        conflicting.append("content-length", HeaderValue::from_static("8"));
        assert!(
            normalize_response_headers(&Method::GET, StatusCode::OK, &mut conflicting).is_err()
        );

        let mut idempotent = HeaderMap::new();
        idempotent.append("x-keep", HeaderValue::from_static("yes"));
        normalize_response_headers(&Method::GET, StatusCode::OK, &mut idempotent).expect("first");
        let once = idempotent.clone();
        normalize_response_headers(&Method::GET, StatusCode::OK, &mut idempotent).expect("second");
        assert_eq!(idempotent, once);
    }

    #[test]
    fn dispatch_validation_precedence_table() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            let request =
                OutboundRequest::new(method, Uri::from_static("https://example.com/resource"))
                    .expect("portable request");
            validate_for_dispatch(&request).expect("portable method");
        }

        let request = OutboundRequest::new(
            Method::CONNECT,
            Uri::from_static("https://example.com/resource"),
        )
        .expect("request");
        assert_eq!(
            bad_request_message(validate_for_dispatch(&request).expect_err("custom method")),
            "method CONNECT is not portable; supported: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"
        );

        let request = OutboundRequest::get("https://example.com")
            .expect("request")
            .body("not empty");
        assert_eq!(
            bad_request_message(validate_for_dispatch(&request).expect_err("GET body")),
            "GET/HEAD request must not carry a body"
        );

        let request = OutboundRequest::get("https://example.com")
            .expect("request")
            .body(Body::stream(stream::iter([Bytes::new()])));
        assert!(request.is_stream_body());
        assert_eq!(
            bad_request_message(validate_for_dispatch(&request).expect_err("GET stream")),
            "GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream"
        );

        let request = OutboundRequest::post("https://example.com")
            .expect("request")
            .body(Body::stream(stream::iter([Bytes::from_static(b"data")])))
            .stream_response();
        assert!(request.is_stream_body());
        assert!(request.is_stream_response());
        validate_for_dispatch(&request).expect("POST stream");

        for bits in [9, 31] {
            let request = OutboundRequest::get("https://example.com")
                .expect("request")
                .max_brotli_window_bits(bits);
            assert!(validate_for_dispatch(&request).is_err(), "accepted {bits}");
        }
        for bits in [10, 30] {
            let request = OutboundRequest::get("https://example.com")
                .expect("request")
                .max_brotli_window_bits(bits);
            validate_for_dispatch(&request).expect("valid Brotli window");
        }
    }

    #[test]
    fn request_normalization_is_idempotent() {
        let mut request = OutboundRequest::post("https://example.com")
            .expect("request")
            .header("connection", "x-first, x-second")
            .expect("connection")
            .header("x-first", "one")
            .expect("first")
            .header("x-second", "two")
            .expect("second")
            .header("x-keep", "yes")
            .expect("keep");

        normalize_for_dispatch(&mut request).expect("first pass");
        let once = request.headers().clone();
        normalize_for_dispatch(&mut request).expect("second pass");
        assert_eq!(request.headers(), &once);
        assert_eq!(
            request.headers().get("x-keep"),
            Some(&HeaderValue::from_static("yes"))
        );
    }

    #[test]
    fn request_normalization_strips_connection_nominations() {
        let mut request = OutboundRequest::post("https://example.com")
            .expect("request")
            .header("connection", "x-first")
            .expect("connection")
            .header("connection", "x-second, X-Third")
            .expect("connection")
            .header("x-first", "one")
            .expect("first")
            .header("x-second", "two")
            .expect("second")
            .header("x-third", "three")
            .expect("third")
            .header("keep-alive", "timeout=5")
            .expect("keep-alive")
            .header("proxy-authenticate", "challenge")
            .expect("proxy-authenticate")
            .header("proxy-authorization", "secret")
            .expect("proxy-authorization")
            .header("te", "trailers")
            .expect("te")
            .header("trailer", "x-checksum")
            .expect("trailer")
            .header("transfer-encoding", "chunked")
            .expect("transfer-encoding")
            .header("upgrade", "websocket")
            .expect("upgrade")
            .header("host", "wrong.example")
            .expect("host")
            .header("content-length", "100")
            .expect("content-length")
            .header("x-keep", "yes")
            .expect("keep");

        normalize_for_dispatch(&mut request).expect("normalize");
        for name in [
            "connection",
            "x-first",
            "x-second",
            "x-third",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
        ] {
            assert!(request.headers().get(name).is_none(), "retained {name}");
        }
        assert_eq!(
            request.headers().get("x-keep"),
            Some(&HeaderValue::from_static("yes"))
        );
    }

    #[test]
    fn request_normalization_rejects_malformed_connection() {
        for value in ["x-valid,,x-other", "bad name", ",x-valid", "x-valid,"] {
            let mut request = OutboundRequest::get("https://example.com").expect("request");
            request.headers_mut().append(
                "connection",
                HeaderValue::from_str(value).expect("header value"),
            );
            assert!(
                normalize_for_dispatch(&mut request).is_err(),
                "accepted {value}"
            );
        }

        let mut request = OutboundRequest::get("https://example.com").expect("request");
        request.headers_mut().append(
            "connection",
            HeaderValue::from_bytes(b"x-private,\xff").expect("opaque header"),
        );
        assert!(normalize_for_dispatch(&mut request).is_err());
    }

    #[test]
    fn outbound_request_defaults_and_parts_round_trip() {
        let deadline = Deadline::after(Duration::from_secs(9));
        let request = OutboundRequest::post("https://example.com/items")
            .expect("request")
            .body("payload")
            .deadline(deadline)
            .header("x-test", "one")
            .expect("header")
            .max_brotli_decoder_bytes(40 * 1024 * 1024)
            .max_brotli_window_bits(23)
            .max_chunk_bytes(NonZeroU64::new(4096).expect("nonzero"))
            .max_decoded_response_bytes(2_000_000)
            .max_encoded_response_bytes(1_000_000)
            .max_request_body_bytes(3_000_000)
            .max_response_bytes(4_000_000)
            .max_response_header_bytes(32_000)
            .max_response_header_count(80)
            .timeout(Duration::from_secs(8));

        let parts = request.into_parts();
        assert_eq!(parts.method, Method::POST);
        assert_eq!(parts.uri, Uri::from_static("https://example.com/items"));
        assert_eq!(parts.body.as_bytes(), Some(b"payload".as_slice()));
        assert_eq!(
            parts.deadline.expect("deadline").instant(),
            deadline.instant()
        );
        assert_eq!(
            parts.headers.get("x-test"),
            Some(&HeaderValue::from_static("one"))
        );
        assert_eq!(parts.max_brotli_decoder_bytes, 40 * 1024 * 1024);
        assert_eq!(parts.max_brotli_window_bits, 23);
        assert_eq!(parts.max_chunk_bytes.map(NonZeroU64::get), Some(4096));
        assert_eq!(parts.max_decoded_response_bytes, Some(2_000_000));
        assert_eq!(parts.max_encoded_response_bytes, Some(1_000_000));
        assert_eq!(parts.max_request_body_bytes, 3_000_000);
        assert_eq!(parts.max_response_header_bytes, Some(32_000));
        assert_eq!(parts.max_response_header_count, Some(80));
        assert_eq!(
            parts.response_mode,
            ResponseMode::Buffered {
                max_bytes: 4_000_000
            }
        );
        assert_eq!(parts.timeout, Some(Duration::from_secs(8)));

        let request = OutboundRequest::from_parts(parts).expect("round trip");
        assert_eq!(request.method(), &Method::POST);
        assert_eq!(
            request.uri(),
            &Uri::from_static("https://example.com/items")
        );

        let defaults = OutboundRequest::get("https://example.com")
            .expect("defaults")
            .into_parts();
        assert_eq!(
            defaults.max_brotli_decoder_bytes,
            DEFAULT_MAX_BROTLI_DECODER_BYTES
        );
        assert_eq!(defaults.max_brotli_window_bits, 24);
        assert_eq!(
            defaults.max_request_body_bytes,
            DEFAULT_OUTBOUND_REQUEST_BODY_BYTES
        );
        assert_eq!(
            defaults.response_mode,
            ResponseMode::Buffered {
                max_bytes: DEFAULT_MAX_RESPONSE_BYTES
            }
        );
        assert!(defaults.deadline.is_none());
        assert!(defaults.max_decoded_response_bytes.is_none());
        assert!(defaults.max_encoded_response_bytes.is_none());
        assert!(defaults.max_response_header_bytes.is_none());
        assert!(defaults.max_response_header_count.is_none());
        assert!(defaults.timeout.is_none());
    }

    #[test]
    fn outbound_request_canonicalizes_url_table() {
        let cases = [
            ("HTTPS://EXAMPLE.com:443/a/../b", "https://example.com/b"),
            ("http://example.com:80", "http://example.com/"),
            ("https://example.com:8443", "https://example.com:8443/"),
            ("https://127.0.0.1", "https://127.0.0.1/"),
            ("https://[::1]:443/a", "https://[::1]/a"),
            ("https://caf%C3%A9.example/", "https://xn--caf-dma.example/"),
        ];

        for (input, expected) in cases {
            let request = OutboundRequest::get(input).expect(input);
            assert_eq!(request.uri().to_string(), expected, "{input}");
        }

        let dns = OutboundRequest::get("https://example.com").expect("dns");
        assert_eq!(dns.backend_target(), "example.com:443");
        assert_eq!(dns.cert_host(), Some("example.com"));
        assert_eq!(dns.host_authority(), "example.com");
        assert_eq!(dns.host_name(), "example.com");
        assert_eq!(dns.sni_hostname(), Some("example.com"));

        let ip = OutboundRequest::get("https://[::1]:8443").expect("ip");
        assert_eq!(ip.backend_target(), "[::1]:8443");
        assert_eq!(ip.cert_host(), Some("::1"));
        assert_eq!(ip.host_authority(), "[::1]:8443");
        assert_eq!(ip.host_name(), "::1");
        assert_eq!(ip.sni_hostname(), None);
    }

    #[test]
    fn outbound_request_rejects_invalid_target_table() {
        for target in [
            "ftp://example.com",
            "/relative",
            "https:///missing",
            "https://user:pass@example.com",
            "https://example.com/path#fragment",
        ] {
            assert!(OutboundRequest::get(target).is_err(), "accepted {target}");
        }
    }

    #[test]
    fn outbound_request_rejects_empty_userinfo() {
        assert!(OutboundRequest::get("https://@example.com/").is_err());
    }

    #[test]
    fn outbound_request_rejects_backslash_authority_forms() {
        for target in [
            r"https:\\@example.com/",
            r"https:/\@example.com/",
            r"https:\/\@example.com/",
            r"https://example.com\path",
        ] {
            assert!(OutboundRequest::get(target).is_err(), "accepted {target}");
        }
    }

    #[test]
    fn outbound_request_preserves_percent_encoded_hash() {
        let request = OutboundRequest::get("https://example.com/a%23b?q=%40user")
            .expect("encoded delimiters");
        assert_eq!(
            request.uri().to_string(),
            "https://example.com/a%23b?q=%40user"
        );
    }

    #[test]
    fn outbound_request_from_request_normalizes_immediately() {
        let request = request_builder()
            .method(Method::POST)
            .uri("/local")
            .header("connection", "x-remove")
            .header("x-remove", "gone")
            .header("host", "local.invalid")
            .header("content-length", "7")
            .header("transfer-encoding", "chunked")
            .header("x-keep", "yes")
            .body(Body::from("payload"))
            .expect("inbound request");

        let outbound =
            OutboundRequest::from_request(request, Uri::from_static("https://example.com/target"))
                .expect("outbound request");
        assert_eq!(outbound.method(), &Method::POST);
        assert_eq!(
            outbound.into_parts().body.as_bytes(),
            Some(b"payload".as_slice())
        );

        let request = request_builder()
            .method(Method::GET)
            .uri("/local")
            .header("x-keep", "yes")
            .body(Body::empty())
            .expect("inbound request");
        let outbound =
            OutboundRequest::from_request(request, Uri::from_static("https://example.com/target"))
                .expect("outbound request");
        assert_eq!(
            outbound.headers().get("x-keep"),
            Some(&HeaderValue::from_static("yes"))
        );
    }

    #[test]
    fn outbound_request_rejects_invalid_header_bytes() {
        let request = OutboundRequest::get("https://example.com").expect("request");
        assert!(request.header(b"bad name", b"value").is_err());

        let request = OutboundRequest::get("https://example.com").expect("request");
        assert!(request.header(b"x-test", [0xff]).is_err());

        let request = OutboundRequest::get("https://example.com").expect("request");
        assert!(request.header(b"x-test", b"line\nbreak").is_err());
    }
}
