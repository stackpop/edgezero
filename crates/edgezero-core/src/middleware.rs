use std::future::Future;
use std::io;
use std::sync::Arc;
use web_time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use futures_util::StreamExt;

use crate::body::Body;
use crate::compression::{
    encode_brotli_stream, encode_deflate_stream, encode_gzip_stream, ContentEncoding,
};
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::handler::DynHandler;
use crate::http::{header, HeaderValue, Response};

pub type BoxMiddleware = Arc<dyn Middleware>;

#[async_trait(?Send)]
pub trait Middleware: Send + Sync + 'static {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError>;
}

pub struct Next<'a> {
    middlewares: &'a [BoxMiddleware],
    handler: &'a dyn DynHandler,
}

impl<'a> Next<'a> {
    pub fn new(middlewares: &'a [BoxMiddleware], handler: &'a dyn DynHandler) -> Self {
        Self {
            middlewares,
            handler,
        }
    }

    pub async fn run(self, ctx: RequestContext) -> Result<Response, EdgeError> {
        if let Some((head, tail)) = self.middlewares.split_first() {
            head.handle(ctx, Next::new(tail, self.handler)).await
        } else {
            self.handler.call(ctx).await
        }
    }
}

pub struct RequestLogger;

#[async_trait(?Send)]
impl Middleware for RequestLogger {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        let method = ctx.request().method().clone();
        let path = ctx.request().uri().path().to_string();
        let start = Instant::now();

        match next.run(ctx).await {
            Ok(response) => {
                let status = response.status();
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(
                    "request method={} path={} status={} elapsed_ms={:.2}",
                    method,
                    path,
                    status.as_u16(),
                    elapsed
                );
                Ok(response)
            }
            Err(err) => {
                let status = err.status();
                let message = err.message();
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                tracing::error!(
                    "request method={} path={} status={} error={} elapsed_ms={:.2}",
                    method,
                    path,
                    status.as_u16(),
                    message,
                    elapsed
                );
                Err(err)
            }
        }
    }
}

pub struct FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    f: F,
}

impl<F> FnMiddleware<F>
where
    F: Send + Sync + 'static,
{
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

#[async_trait(?Send)]
impl<F, Fut> Middleware for FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        (self.f)(ctx, next).await
    }
}

pub fn middleware_fn<F, Fut>(f: F) -> FnMiddleware<F>
where
    F: Fn(RequestContext, Next<'_>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response, EdgeError>>,
{
    FnMiddleware::new(f)
}

// ---------------------------------------------------------------------------
// Compression middleware
// ---------------------------------------------------------------------------

/// Minimum body size (bytes) below which we skip compression.
const MIN_COMPRESS_SIZE: usize = 256;

/// Middleware that compresses response bodies based on the request's
/// `Accept-Encoding` header.
///
/// Negotiation priority: br > zstd > gzip > deflate > identity.
///
/// Responses that already carry a `Content-Encoding` header, have an empty
/// body, or are smaller than 256 bytes are left untouched.
pub struct CompressionMiddleware {
    /// Encodings this instance is willing to use (default: all four).
    allowed: Vec<ContentEncoding>,
    /// Minimum response body size to trigger compression.
    min_size: usize,
}

impl Default for CompressionMiddleware {
    fn default() -> Self {
        Self {
            allowed: vec![
                ContentEncoding::Brotli,
                ContentEncoding::Gzip,
                ContentEncoding::Deflate,
            ],
            min_size: MIN_COMPRESS_SIZE,
        }
    }
}

impl CompressionMiddleware {
    pub fn new() -> Self {
        Self::default()
    }

    /// Only offer the listed encodings.
    pub fn with_encodings(mut self, encodings: Vec<ContentEncoding>) -> Self {
        self.allowed = encodings;
        self
    }

    /// Override the minimum body size threshold.
    pub fn with_min_size(mut self, size: usize) -> Self {
        self.min_size = size;
        self
    }
}

#[async_trait(?Send)]
impl Middleware for CompressionMiddleware {
    async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
        // Parse Accept-Encoding from request *before* we pass it to the handler.
        let accepted = parse_accept_encoding(
            ctx.request()
                .headers()
                .get(header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
        );

        let chosen = negotiate(&accepted, &self.allowed);

        let response = next.run(ctx).await?;

        // Skip if there's already encoding, or no usable encoding was negotiated.
        let chosen = match chosen {
            Some(enc) if enc != ContentEncoding::Identity => enc,
            _ => return Ok(response),
        };

        // Skip if already encoded.
        if response.headers().contains_key(header::CONTENT_ENCODING) {
            return Ok(response);
        }

        let (parts, body) = response.into_parts();

        // For buffered bodies, skip small payloads.
        if let Body::Once(ref bytes) = body {
            if bytes.len() < self.min_size {
                return Ok(Response::from_parts(parts, body));
            }
        }

        // Convert body into a Vec<u8> stream for the encoder, then pin it
        // via boxed_local() since the async-compression streams are !Unpin.
        use futures_util::stream::LocalBoxStream;

        let raw_stream: LocalBoxStream<'static, Result<Vec<u8>, io::Error>> = match body {
            Body::Once(bytes) => stream::once(async move { Ok(bytes.to_vec()) }).boxed_local(),
            Body::Stream(s) => s
                .map(|res| match res {
                    Ok(bytes) => Ok(bytes.to_vec()),
                    Err(err) => Err(io::Error::other(err.to_string())),
                })
                .boxed_local(),
        };

        let encoded_stream: LocalBoxStream<'static, Result<Bytes, io::Error>> = match chosen {
            ContentEncoding::Gzip => encode_gzip_stream(raw_stream).boxed_local(),
            ContentEncoding::Brotli => encode_brotli_stream(raw_stream).boxed_local(),
            ContentEncoding::Deflate => encode_deflate_stream(raw_stream).boxed_local(),
            ContentEncoding::Identity => unreachable!(),
        };

        let compressed_body = Body::from_stream(encoded_stream);

        let mut response = Response::from_parts(parts, compressed_body);
        response.headers_mut().insert(
            header::CONTENT_ENCODING,
            HeaderValue::from_static(chosen.as_str()),
        );
        // Length is unknown for streaming compressed output.
        response.headers_mut().remove(header::CONTENT_LENGTH);

        // Indicate that the response varies by Accept-Encoding.
        response
            .headers_mut()
            .append(header::VARY, HeaderValue::from_static("Accept-Encoding"));

        Ok(response)
    }
}

/// Parsed entry from an `Accept-Encoding` header value.
#[derive(Debug, Clone)]
struct AcceptEntry {
    encoding: ContentEncoding,
    quality: f32,
}

/// Parse `Accept-Encoding` header value into entries sorted by quality descending.
fn parse_accept_encoding(header: &str) -> Vec<AcceptEntry> {
    let mut entries: Vec<AcceptEntry> = header
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let mut split = part.splitn(2, ";q=");
            let token = split.next()?.trim();
            let quality: f32 = split
                .next()
                .and_then(|q| q.trim().parse().ok())
                .unwrap_or(1.0);

            let encoding = ContentEncoding::parse(token)?;
            Some(AcceptEntry { encoding, quality })
        })
        .collect();

    // Stable sort by descending quality.
    entries.sort_by(|a, b| {
        b.quality
            .partial_cmp(&a.quality)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    entries
}

/// Pick the best encoding that the client accepts and we're willing to produce.
fn negotiate(accepted: &[AcceptEntry], allowed: &[ContentEncoding]) -> Option<ContentEncoding> {
    for entry in accepted {
        if entry.quality <= 0.0 {
            continue;
        }
        if allowed.contains(&entry.encoding) {
            return Some(entry.encoding);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::handler::IntoHandler;
    use crate::http::{request_builder, Method, Response, StatusCode};
    use crate::params::PathParams;
    use crate::response::response_with_body;
    use futures::executor::block_on;
    use std::sync::{Arc, Mutex};

    struct RecordingMiddleware {
        log: Arc<Mutex<Vec<String>>>,
        name: &'static str,
    }

    #[async_trait(?Send)]
    impl Middleware for RecordingMiddleware {
        async fn handle(&self, ctx: RequestContext, next: Next<'_>) -> Result<Response, EdgeError> {
            {
                let mut entries = self.log.lock().unwrap();
                entries.push(self.name.to_string());
            }
            next.run(ctx).await
        }
    }

    struct ShortCircuit;

    #[async_trait(?Send)]
    impl Middleware for ShortCircuit {
        async fn handle(
            &self,
            _ctx: RequestContext,
            _next: Next<'_>,
        ) -> Result<Response, EdgeError> {
            Ok(response_with_body(StatusCode::UNAUTHORIZED, Body::empty()))
        }
    }

    fn empty_context() -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    async fn ok_handler(_ctx: RequestContext) -> Result<Response, EdgeError> {
        Ok(response_with_body(StatusCode::OK, Body::empty()))
    }

    #[test]
    fn middleware_chain_runs_in_order() {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let first = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "first",
        };
        let second = RecordingMiddleware {
            log: Arc::clone(&log),
            name: "second",
        };

        let handler = (|_ctx: RequestContext| async move {
            Ok::<Response, EdgeError>(response_with_body(StatusCode::OK, Body::empty()))
        })
        .into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![
            Arc::new(first) as BoxMiddleware,
            Arc::new(second) as BoxMiddleware,
        ];

        let result = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(result.status(), StatusCode::OK);

        let calls = log.lock().unwrap().clone();
        assert_eq!(calls, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn middleware_can_short_circuit() {
        let handler = ok_handler.into_handler();

        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(ShortCircuit) as BoxMiddleware];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn next_runs_handler_without_middlewares() {
        let handler = ok_handler.into_handler();
        let response =
            block_on(Next::new(&[], handler.as_ref()).run(empty_context())).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn request_logger_passes_through_success() {
        let handler = ok_handler.into_handler();
        let response =
            block_on(RequestLogger.handle(empty_context(), Next::new(&[], handler.as_ref())))
                .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn request_logger_propagates_error() {
        let handler = (|_ctx: RequestContext| async move {
            Err::<Response, EdgeError>(EdgeError::bad_request("boom"))
        })
        .into_handler();
        let err = block_on(RequestLogger.handle(empty_context(), Next::new(&[], handler.as_ref())))
            .expect_err("error");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn middleware_fn_executes_closure() {
        let called = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&called);
        let middleware = middleware_fn(move |_ctx, _next| {
            let flag = Arc::clone(&flag);
            async move {
                *flag.lock().unwrap() = true;
                Ok(response_with_body(StatusCode::OK, Body::empty()))
            }
        });

        let handler = ok_handler.into_handler();
        let middlewares: Vec<BoxMiddleware> = vec![Arc::new(middleware) as BoxMiddleware];
        let response = block_on(Next::new(&middlewares, handler.as_ref()).run(empty_context()))
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(*called.lock().unwrap());
    }

    // ------- Compression middleware tests -------

    fn ctx_with_accept_encoding(encoding: &str) -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri("/test")
            .header("accept-encoding", encoding)
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    fn large_body_handler(
        body: &'static str,
    ) -> impl Fn(
        RequestContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Response, EdgeError>> + 'static>,
    > + Send
           + Sync
           + 'static {
        move |_ctx: RequestContext| {
            Box::pin(async move {
                Ok::<Response, EdgeError>(response_with_body(StatusCode::OK, Body::from(body)))
            })
        }
    }

    /// A body large enough to trigger compression (> MIN_COMPRESS_SIZE).
    const LARGE_BODY: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
        Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
        Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris \
        nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in \
        reprehenderit in voluptate velit esse cillum dolore eu fugiat.";

    #[test]
    fn compression_middleware_compresses_gzip() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = ctx_with_accept_encoding("gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
        assert!(response.body().is_stream());

        // Verify round-trip: collect compressed bytes, then decompress
        let compressed = block_on(response.into_body().collect()).expect("collect");
        let decompressed = decompress_gzip(&compressed);
        assert_eq!(decompressed, LARGE_BODY.as_bytes());
    }

    #[test]
    fn compression_middleware_compresses_brotli() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = ctx_with_accept_encoding("br");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("br")
        );
    }

    #[test]
    fn compression_middleware_skips_small_bodies() {
        let handler = (|_ctx: RequestContext| async move {
            Ok::<Response, EdgeError>(response_with_body(StatusCode::OK, Body::from("tiny")))
        })
        .into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = ctx_with_accept_encoding("gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        // No compression for small body
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(!response.body().is_stream());
    }

    #[test]
    fn compression_middleware_skips_if_already_encoded() {
        let handler = (|_ctx: RequestContext| async move {
            use crate::http::response_builder;
            let response = response_builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_ENCODING, "br")
                .body(Body::from(LARGE_BODY))
                .unwrap();
            Ok::<Response, EdgeError>(response)
        })
        .into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = ctx_with_accept_encoding("gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        // Should keep the original "br" encoding, not re-compress
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("br")
        );
    }

    #[test]
    fn compression_middleware_skips_without_accept_encoding() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = empty_context(); // no accept-encoding

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
    }

    #[test]
    fn compression_middleware_respects_quality_values() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        let mw = CompressionMiddleware::new();
        // gzip preferred over br via quality values
        let ctx = ctx_with_accept_encoding("br;q=0.5, gzip;q=1.0");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn compression_middleware_adds_vary_header() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        let mw = CompressionMiddleware::new();
        let ctx = ctx_with_accept_encoding("gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert_eq!(
            response
                .headers()
                .get(header::VARY)
                .and_then(|v| v.to_str().ok()),
            Some("Accept-Encoding")
        );
    }

    #[test]
    fn compression_middleware_custom_encodings() {
        let handler = large_body_handler(LARGE_BODY).into_handler();
        // Only allow gzip
        let mw = CompressionMiddleware::new().with_encodings(vec![ContentEncoding::Gzip]);
        let ctx = ctx_with_accept_encoding("br, gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        // Should pick gzip since br is not allowed
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn compression_middleware_custom_min_size() {
        let handler = (|_ctx: RequestContext| async move {
            Ok::<Response, EdgeError>(response_with_body(StatusCode::OK, Body::from("tiny")))
        })
        .into_handler();
        // Set min_size to 1 so even small bodies are compressed
        let mw = CompressionMiddleware::new().with_min_size(1);
        let ctx = ctx_with_accept_encoding("gzip");

        let response =
            block_on(mw.handle(ctx, Next::new(&[], handler.as_ref()))).expect("response");

        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn parse_accept_encoding_handles_various_formats() {
        let entries = parse_accept_encoding("gzip, deflate, br;q=0.9");
        assert_eq!(entries.len(), 3);
        // gzip and deflate have q=1.0, br has q=0.9
        assert_eq!(entries[0].quality, 1.0);
        assert_eq!(entries[2].quality, 0.9);
    }

    #[test]
    fn parse_accept_encoding_skips_unknown() {
        let entries = parse_accept_encoding("gzip, unknown, br");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn negotiate_picks_first_match() {
        let accepted = parse_accept_encoding("br, gzip");
        let allowed = vec![ContentEncoding::Gzip, ContentEncoding::Brotli];
        assert_eq!(
            negotiate(&accepted, &allowed),
            Some(ContentEncoding::Brotli)
        );
    }

    #[test]
    fn negotiate_skips_zero_quality() {
        let accepted = parse_accept_encoding("gzip;q=0, br");
        let allowed = vec![ContentEncoding::Gzip, ContentEncoding::Brotli];
        assert_eq!(
            negotiate(&accepted, &allowed),
            Some(ContentEncoding::Brotli)
        );
    }

    #[test]
    fn negotiate_returns_none_when_no_match() {
        let accepted = parse_accept_encoding("zstd");
        let allowed = vec![ContentEncoding::Gzip];
        assert_eq!(negotiate(&accepted, &allowed), None);
    }

    // Helper to decompress gzip for round-trip verification
    fn decompress_gzip(data: &[u8]) -> Vec<u8> {
        use flate2::read::GzDecoder;
        use std::io::Read;
        let mut decoder = GzDecoder::new(data);
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).expect("gzip decompress");
        result
    }
}
