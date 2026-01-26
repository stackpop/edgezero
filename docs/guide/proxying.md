# Proxying

EdgeZero provides built-in helpers for forwarding requests to upstream services while staying provider-agnostic.

## Proxy Primitives

The core proxy types live in `edgezero_core::proxy`:

- **`ProxyRequest`** - Represents a request to forward upstream
- **`ProxyResponse`** - The response from the upstream service
- **`ProxyService<C>`** - Executes proxy requests using a provider-specific client

## Basic Proxying

```rust
use edgezero_core::action;
use edgezero_core::context::RequestContext;
use edgezero_core::http::{Response, Uri};
use edgezero_core::proxy::{ProxyRequest, ProxyService};
use edgezero_core::body::Body;

#[action]
async fn proxy_to_api(RequestContext(ctx): RequestContext) -> Response<Body> {
    let target: Uri = "https://api.example.com/v1".parse().unwrap();
    
    // Build proxy request from incoming request
    let proxy_request = ProxyRequest::from_request(ctx.into_request(), target);
    
    // Forward using the adapter's proxy client
    let client = get_proxy_client(); // Platform-specific
    let response = ProxyService::new(client)
        .forward(proxy_request)
        .await
        .unwrap();
    
    response.into_response()
}
```

## Adapter-Specific Clients

Each adapter provides its own proxy client implementation:

### Fastly

```rust
use edgezero_adapter_fastly::FastlyProxyClient;

let client = FastlyProxyClient::new("backend-name");
let response = ProxyService::new(client).forward(request).await?;
```

The backend name must be configured in `fastly.toml`:

```toml
[local_server.backends.backend-name]
url = "https://api.example.com"
```

### Cloudflare

```rust
use edgezero_adapter_cloudflare::CloudflareProxyClient;

let client = CloudflareProxyClient::new();
let response = ProxyService::new(client).forward(request).await?;
```

Cloudflare Workers use the global `fetch` API.

### Axum (Development)

```rust
use edgezero_adapter_axum::AxumProxyClient;

let client = AxumProxyClient::new();
let response = ProxyService::new(client).forward(request).await?;
```

## Request Modification

Modify requests before forwarding:

```rust
#[action]
async fn proxy_with_auth(RequestContext(ctx): RequestContext) -> Response<Body> {
    let target: Uri = "https://api.example.com".parse().unwrap();
    
    let mut proxy_request = ProxyRequest::from_request(ctx.into_request(), target);
    
    // Add authentication header
    proxy_request.headers_mut().insert(
        "authorization",
        "Bearer secret-token".parse().unwrap(),
    );
    
    // Remove sensitive headers
    proxy_request.headers_mut().remove("cookie");
    
    let client = get_proxy_client();
    ProxyService::new(client).forward(proxy_request).await?.into_response()
}
```

## Response Processing

Process upstream responses before returning:

```rust
#[action]
async fn proxy_with_transform(RequestContext(ctx): RequestContext) -> Response<Body> {
    let target: Uri = "https://api.example.com".parse().unwrap();
    let proxy_request = ProxyRequest::from_request(ctx.into_request(), target);
    
    let client = get_proxy_client();
    let mut response = ProxyService::new(client).forward(proxy_request).await?;
    
    // Add cache headers
    response.headers_mut().insert(
        "cache-control",
        "public, max-age=3600".parse().unwrap(),
    );
    
    // Add diagnostic header
    response.headers_mut().insert(
        "x-proxy-by",
        "edgezero".parse().unwrap(),
    );
    
    response.into_response()
}
```

## Streaming Proxies

Proxy requests preserve streaming bodies without buffering:

```rust
// Large uploads/downloads stream through without loading into memory
let proxy_request = ProxyRequest::from_request(request, target);
let response = proxy.forward(proxy_request).await?;
// response.body is still streaming
```

## Transparent Decompression

Proxied responses are automatically decompressed:

| Content-Encoding | Handling |
|------------------|----------|
| `gzip` | Automatically decoded |
| `br` (brotli) | Automatically decoded |
| `identity` | Passed through |

This allows you to process response bodies without manual decompression:

```rust
let response = proxy.forward(request).await?;
let body = response.body().bytes().await?;
// body is already decompressed, ready for transformation
```

## Error Handling

Proxy operations can fail for various reasons:

```rust
use edgezero_core::error::EdgeError;

#[action]
async fn safe_proxy(RequestContext(ctx): RequestContext) -> Response<Body> {
    let target: Uri = "https://api.example.com".parse().unwrap();
    let proxy_request = ProxyRequest::from_request(ctx.into_request(), target);
    
    let client = get_proxy_client();
    match ProxyService::new(client).forward(proxy_request).await {
        Ok(response) => response.into_response(),
        Err(e) => {
            log::error!("Proxy failed: {}", e);
            Response::builder()
                .status(502)
                .body(Body::from("Bad Gateway"))
                .unwrap()
        }
    }
}
```

## Common Use Cases

### API Gateway

```rust
#[action]
async fn api_gateway(
    Path(service): Path<String>,
    RequestContext(ctx): RequestContext,
) -> Response<Body> {
    let target = match service.as_str() {
        "users" => "https://users-api.internal",
        "orders" => "https://orders-api.internal",
        _ => return Response::builder()
            .status(404)
            .body(Body::from("Service not found"))
            .unwrap(),
    };
    
    let uri: Uri = target.parse().unwrap();
    let request = ProxyRequest::from_request(ctx.into_request(), uri);
    
    get_proxy_client()
        .forward(request)
        .await
        .map(|r| r.into_response())
        .unwrap_or_else(|_| bad_gateway())
}
```

### Caching Proxy

```rust
#[action]
async fn caching_proxy(RequestContext(ctx): RequestContext) -> Response<Body> {
    // Check cache first
    if let Some(cached) = cache.get(ctx.uri().path()) {
        return cached;
    }
    
    // Proxy to origin
    let response = proxy.forward(request).await?;
    
    // Cache successful responses
    if response.status().is_success() {
        cache.set(ctx.uri().path(), response.clone());
    }
    
    response.into_response()
}
```

## Next Steps

- Configure backends in [Fastly](/guide/adapters/fastly) adapter guide
- Learn about [Cloudflare](/guide/adapters/cloudflare) fetch integration
