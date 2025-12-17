use edgezero_core::body::Body;
use edgezero_core::compression::{decode_brotli_stream, decode_gzip_stream};
use edgezero_core::error::EdgeError;
use edgezero_core::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use edgezero_core::proxy::{ProxyClient, ProxyRequest, ProxyResponse};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::{self, LocalBoxStream, StreamExt};
use futures_util::TryStreamExt;
use std::io;
use worker::{
    wasm_bindgen::JsValue, Body as WorkerBody, Fetch, Headers, Method as CfMethod,
    Request as CfRequest, RequestInit, Response as CfResponse,
};

pub struct CloudflareProxyClient;

#[async_trait(?Send)]
impl ProxyClient for CloudflareProxyClient {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _ext) = request.into_parts();
        let cf_request = build_cf_request(method, &uri, headers, body).await?;
        let mut cf_response = Fetch::Request(cf_request)
            .send()
            .await
            .map_err(EdgeError::internal)?;

        let mut proxy_response = convert_response(&mut cf_response).await?;
        proxy_response
            .headers_mut()
            .insert("x-edgezero-proxy", HeaderValue::from_static("cloudflare"));
        Ok(proxy_response)
    }
}

async fn build_cf_request(
    method: Method,
    uri: &Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<CfRequest, EdgeError> {
    let mut init = RequestInit::new();
    init.with_method(http_method_to_cf(method.clone()));

    let cf_headers = Headers::from(&headers);
    init.with_headers(cf_headers);

    attach_body(&mut init, body)?;

    let request = CfRequest::new_with_init(&uri.to_string(), &init).map_err(EdgeError::internal)?;
    Ok(request)
}

fn attach_body(init: &mut RequestInit, body: Body) -> Result<(), EdgeError> {
    match body {
        Body::Once(bytes) => {
            if bytes.is_empty() {
                return Ok(());
            }
            let chunk = bytes.to_vec();
            let stream = stream::once(async move { Ok::<Vec<u8>, JsValue>(chunk) }).boxed_local();
            let worker_body = WorkerBody::from_stream(stream).map_err(EdgeError::internal)?;
            if let Some(readable) = worker_body.into_inner() {
                init.with_body(Some(JsValue::from(readable)));
            }
        }
        Body::Stream(stream) => {
            let mapped = stream
                .map(|res| match res {
                    Ok(bytes) => Ok::<Vec<u8>, JsValue>(bytes.to_vec()),
                    Err(err) => Err(JsValue::from_str(&err.to_string())),
                })
                .boxed_local();
            let worker_body = WorkerBody::from_stream(mapped).map_err(EdgeError::internal)?;
            if let Some(readable) = worker_body.into_inner() {
                init.with_body(Some(JsValue::from(readable)));
            }
        }
    }

    Ok(())
}

async fn convert_response(cf_response: &mut CfResponse) -> Result<ProxyResponse, EdgeError> {
    let status = StatusCode::from_u16(cf_response.status_code()).map_err(EdgeError::internal)?;
    let mut proxy_response = ProxyResponse::new(status, Body::empty());

    let mut encoding = None;
    for (name, value) in cf_response.headers().entries() {
        if name.eq_ignore_ascii_case(header::CONTENT_ENCODING.as_str()) {
            encoding = Some(value.to_ascii_lowercase());
        }
        if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
            if let Ok(header_value) = HeaderValue::from_str(&value) {
                proxy_response
                    .headers_mut()
                    .insert(header_name, header_value);
            }
        }
    }

    let worker_stream = cf_response.stream().map_err(EdgeError::internal)?;

    let chunk_stream: ChunkStream = worker_stream.map_err(worker_error_to_io).boxed_local();
    let body_stream = transform_stream(chunk_stream, encoding.as_deref());
    *proxy_response.body_mut() = Body::from_stream(body_stream);

    if encoding.is_some() {
        proxy_response
            .headers_mut()
            .remove(header::CONTENT_ENCODING);
        proxy_response.headers_mut().remove(header::CONTENT_LENGTH);
    }

    Ok(proxy_response)
}

fn http_method_to_cf(method: Method) -> CfMethod {
    match method {
        Method::GET => CfMethod::Get,
        Method::POST => CfMethod::Post,
        Method::PUT => CfMethod::Put,
        Method::PATCH => CfMethod::Patch,
        Method::DELETE => CfMethod::Delete,
        Method::HEAD => CfMethod::Head,
        Method::OPTIONS => CfMethod::Options,
        Method::CONNECT => CfMethod::Connect,
        Method::TRACE => CfMethod::Trace,
        _ => CfMethod::Get,
    }
}

type ChunkStream = LocalBoxStream<'static, Result<Vec<u8>, io::Error>>;

fn worker_error_to_io(err: worker::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}

fn transform_stream(
    stream: ChunkStream,
    encoding: Option<&str>,
) -> LocalBoxStream<'static, Result<Bytes, io::Error>> {
    match encoding {
        Some("gzip") => decode_gzip_stream(stream).boxed_local(),
        Some("br") => decode_brotli_stream(stream).boxed_local(),
        _ => stream.map(|res| res.map(Bytes::from)).boxed_local(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::CompressorWriter;
    use flate2::{write::GzEncoder, Compression};
    use futures::executor::block_on;
    use futures_util::stream;
    use std::io::Write;

    fn collect_body(body: Body) -> Vec<u8> {
        match body {
            Body::Once(bytes) => bytes.to_vec(),
            Body::Stream(mut stream) => block_on(async {
                let mut out = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.expect("chunk");
                    out.extend_from_slice(&chunk);
                }
                out
            }),
        }
    }

    #[test]
    fn streaming_identity_preserves_body() {
        let chunks = vec![
            Ok::<Vec<u8>, io::Error>(b"hello".to_vec()),
            Ok(b" world".to_vec()),
        ];
        let chunk_stream: ChunkStream = Box::pin(stream::iter(chunks));
        let body = Body::from_stream(transform_stream(chunk_stream, None));
        assert_eq!(collect_body(body), b"hello world");
    }

    #[test]
    fn streaming_handles_gzip_and_brotli() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"gzip payload").unwrap();
        let gzip = encoder.finish().unwrap();
        let gzip_stream: ChunkStream = Box::pin(stream::iter(vec![Ok::<Vec<u8>, io::Error>(gzip)]));
        let body = Body::from_stream(transform_stream(gzip_stream, Some("gzip")));
        assert_eq!(collect_body(body), b"gzip payload");

        let mut brotli_data = Vec::new();
        {
            let mut compressor = CompressorWriter::new(&mut brotli_data, 4096, 5, 21);
            compressor.write_all(b"brotli payload").unwrap();
        }
        let brotli_stream: ChunkStream =
            Box::pin(stream::iter(vec![Ok::<Vec<u8>, io::Error>(brotli_data)]));
        let body = Body::from_stream(transform_stream(brotli_stream, Some("br")));
        assert_eq!(collect_body(body), b"brotli payload");
    }
}
