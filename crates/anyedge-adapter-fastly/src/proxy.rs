use anyedge_core::{
    decode_brotli_stream, decode_gzip_stream, header, Body, EdgeError, HeaderMap, HeaderValue,
    Method, ProxyClient, ProxyRequest, ProxyResponse, Uri,
};
use async_stream::try_stream;
use async_trait::async_trait;
use bytes::Bytes;
use fastly::{
    http::body::StreamingBody, Backend, Request as FastlyRequest, Response as FastlyResponse,
};
use futures_util::stream::{BoxStream, StreamExt};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};

const BACKEND_PREFIX: &str = "anyedge-dynamic-";

pub struct FastlyProxyClient;

#[async_trait(?Send)]
impl ProxyClient for FastlyProxyClient {
    async fn send(&self, request: ProxyRequest) -> Result<ProxyResponse, EdgeError> {
        let (method, uri, headers, body, _ext) = request.into_parts();
        let backend = ensure_backend(&uri)?;
        let fastly_request = build_fastly_request(method, &uri, headers)?;
        let (mut streaming_body, pending_request) = fastly_request
            .send_async_streaming(backend.name())
            .map_err(|err| EdgeError::internal(err))?;
        forward_request_body(body, &mut streaming_body).await?;
        streaming_body
            .finish()
            .map_err(|err| EdgeError::internal(err))?;
        let mut fastly_response = pending_request
            .wait()
            .map_err(|err| EdgeError::internal(err))?;

        let mut proxy_response = convert_response(&mut fastly_response)?;
        proxy_response
            .headers_mut()
            .insert("x-anyedge-proxy", HeaderValue::from_static("fastly"));
        Ok(proxy_response)
    }
}

fn build_fastly_request(
    method: Method,
    uri: &Uri,
    headers: HeaderMap,
) -> Result<FastlyRequest, EdgeError> {
    let mut fastly_request = FastlyRequest::new(
        method.clone(),
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/"),
    );
    fastly_request.set_method(method);

    for (name, value) in headers.iter() {
        if name.as_str().eq_ignore_ascii_case("host") {
            continue;
        }
        fastly_request.set_header(name.as_str(), value.clone());
    }

    if let Some(host) = uri.host() {
        fastly_request.set_header("Host", host);
    }

    Ok(fastly_request)
}

async fn forward_request_body(
    body: Body,
    streaming_body: &mut StreamingBody,
) -> Result<(), EdgeError> {
    match body {
        Body::Once(bytes) => {
            if !bytes.is_empty() {
                streaming_body
                    .write_all(bytes.as_ref())
                    .map_err(|err| EdgeError::internal(err))?;
            }
        }
        Body::Stream(mut stream) => {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(EdgeError::internal)?;
                streaming_body
                    .write_all(&chunk)
                    .map_err(|err| EdgeError::internal(err))?;
            }
        }
    }

    streaming_body
        .flush()
        .map_err(|err| EdgeError::internal(err))?;

    Ok(())
}

fn ensure_backend(uri: &Uri) -> Result<Backend, EdgeError> {
    let host = uri
        .host()
        .ok_or_else(|| EdgeError::bad_request("proxy target must include host"))?;
    let port = uri.port_u16();
    let target = match port {
        Some(port) => format!("{}:{}", host, port),
        None => host.to_string(),
    };

    let name = backend_name(&target, uri.scheme_str());

    match Backend::from_name(&name) {
        Ok(backend) => Ok(backend),
        Err(_) => {
            let mut builder = Backend::builder(&name, &target);
            if uri.scheme_str() == Some("https") {
                builder = builder.enable_ssl();
            }
            builder.finish().map_err(|err| EdgeError::internal(err))
        }
    }
}

fn backend_name(target: &str, scheme: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    scheme.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{}{:016x}", BACKEND_PREFIX, hash)
}

fn convert_response(fastly_response: &mut FastlyResponse) -> Result<ProxyResponse, EdgeError> {
    let status = fastly_response.get_status();
    let mut proxy_response = ProxyResponse::new(status, Body::empty());

    for header in fastly_response.get_header_names() {
        if let Some(value) = fastly_response.get_header(header) {
            proxy_response.headers_mut().insert(header, value.clone());
        }
    }

    let encoding = proxy_response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase());

    let body = fastly_response.take_body();

    let chunk_stream = fastly_body_stream(body);
    let body_stream = transform_stream(chunk_stream, encoding.as_deref());
    *proxy_response.body_mut() = Body::from_stream(body_stream);
    if encoding.as_deref() == Some("gzip") || encoding.as_deref() == Some("br") {
        proxy_response
            .headers_mut()
            .remove(header::CONTENT_ENCODING);
        proxy_response.headers_mut().remove(header::CONTENT_LENGTH);
    }

    Ok(proxy_response)
}

type ChunkStream = BoxStream<'static, Result<Vec<u8>, io::Error>>;

fn fastly_body_stream(mut body: fastly::Body) -> ChunkStream {
    try_stream! {
        for chunk in body.read_chunks(8 * 1024) {
            let chunk = chunk?;
            yield chunk;
        }
    }
    .boxed()
}

fn transform_stream(
    stream: ChunkStream,
    encoding: Option<&str>,
) -> BoxStream<'static, Result<Bytes, io::Error>> {
    match encoding {
        Some("gzip") => decode_gzip_stream(stream).boxed(),
        Some("br") => decode_brotli_stream(stream).boxed(),
        _ => stream.map(|res| res.map(Bytes::from)).boxed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brotli::CompressorWriter;
    use flate2::{write::GzEncoder, Compression};
    use futures::executor::block_on;
    use std::io::Write;

    #[test]
    fn stream_handles_identity_and_gzip() {
        let mut plain = fastly::Body::new();
        plain.write_all(b"plain").unwrap();
        let body = Body::from_stream(transform_stream(fastly_body_stream(plain), None));
        let collected = collect_body(body);
        assert_eq!(collected, b"plain");

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello gzip").unwrap();
        let compressed = encoder.finish().unwrap();
        let mut gz_body = fastly::Body::new();
        gz_body.write_all(&compressed).unwrap();
        let body = Body::from_stream(transform_stream(fastly_body_stream(gz_body), Some("gzip")));
        let collected = collect_body(body);
        assert_eq!(collected, b"hello gzip");
    }

    #[test]
    fn stream_handles_brotli() {
        let mut compressed = Vec::new();
        {
            let mut compressor = CompressorWriter::new(&mut compressed, 4096, 5, 21);
            compressor.write_all(b"hello brotli").unwrap();
        }

        let mut br_body = fastly::Body::new();
        br_body.write_all(&compressed).unwrap();
        let body = Body::from_stream(transform_stream(fastly_body_stream(br_body), Some("br")));
        let collected = collect_body(body);
        assert_eq!(collected, b"hello brotli");
    }

    fn collect_body(body: Body) -> Vec<u8> {
        match body {
            Body::Once(bytes) => bytes.to_vec(),
            Body::Stream(mut stream) => block_on(async {
                let mut out = Vec::new();
                while let Some(chunk) = stream.next().await {
                    out.extend_from_slice(&chunk.expect("chunk"));
                }
                out
            }),
        }
    }
}
