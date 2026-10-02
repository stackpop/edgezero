# Proxying

EdgeZero provides helpers for forwarding requests to upstream services while staying
provider-agnostic.

## End-to-End Example

This example forwards the incoming request upstream, adjusts headers on the way in and out, and
returns a friendly 502 on proxy errors. It uses the adapter-provided proxy handle inserted by each
adapter.

```rust
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{Response, StatusCode, Uri};
use edgezero_core::proxy::ProxyRequest;

#[action]
async fn proxy_with_auth(RequestContext(ctx): RequestContext) -> Result<Response, EdgeError> {
    let target: Uri = "https://api.example.com".parse().unwrap();

    let handle = ctx
        .proxy_handle()
        .ok_or_else(|| EdgeError::internal("proxy client not configured"))?;

    let mut proxy_request = ProxyRequest::from_request(ctx.into_request(), target);
    proxy_request.headers_mut().insert(
        "authorization",
        "Bearer secret-token".parse().unwrap(),
    );
    proxy_request.headers_mut().remove("cookie");

    match handle.forward(proxy_request).await {
        Ok(mut response) => {
            response
                .headers_mut()
                .insert("x-proxy-by", "edgezero".parse().unwrap());
            Ok(response)
        }
        Err(err) => {
            tracing::error!("proxy failed: {}", err);
            let response = Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("Bad Gateway"))
                .map_err(EdgeError::internal)?;
            Ok(response)
        }
    }
}
```

## Notes

- Fastly and Cloudflare stream outbound request bodies; Axum buffers them before sending, and Spin buffers them too, capping streamed-body collection at 16 MiB (larger streams error) while an already-buffered `Body::Once` bypasses that cap.
- Fastly, Cloudflare, and Spin automatically decode `gzip`/`br` responses for you.
- If you need a direct client (for tests or custom wiring), use the adapter clients
  (`FastlyProxyClient`, `CloudflareProxyClient`, `SpinProxyClient`, `AxumProxyClient::try_new()?`).

## Next Steps

- Learn about [Fastly](/guide/adapters/fastly), [Cloudflare](/guide/adapters/cloudflare), and [Spin](/guide/adapters/spin) adapter specifics
