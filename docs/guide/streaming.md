# Streaming

EdgeZero supports streaming responses for large payloads, real-time data, and server-sent events.

## Streaming Responses

Use `Body::stream` to yield response chunks progressively:

```rust
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::http::Response;
use bytes::Bytes;
use futures::stream;

#[action]
async fn stream_data() -> Response {
    let chunks = vec![
        Bytes::from_static(b"Hello"),
        Bytes::from_static(b" "),
        Bytes::from_static(b"World"),
    ];

    let body = Body::stream(stream::iter(chunks));

    Response::builder()
        .status(200)
        .header("content-type", "text/plain")
        .body(body)
        .unwrap()
}
```

## How Streaming Works

The core router passes `Body::Stream` through untouched; whether the client sees
chunks progressively depends on the adapter:

1. Your handler returns `Body::stream(...)` with a `Stream` of chunks
2. Cloudflare wraps the stream in a `ReadableStream` (`Response::from_stream`), so
   chunks reach the client as they are produced
3. Fastly, Spin, and Axum drain the stream into a buffer before writing the
   provider response (Spin rejects streamed bodies over 16 MiB), so the client
   receives the whole body at once

Use streaming for its memory and composability benefits everywhere, but only rely
on progressive delivery (SSE, long-lived chunked responses) on Cloudflare today.

## Server-Sent Events

Stream events to clients with SSE:

```rust
use edgezero_core::action;
use edgezero_core::body::Body;
use edgezero_core::http::Response;
use bytes::Bytes;

#[action]
async fn events() -> Response {
    let events = async_stream::stream! {
        for i in 0..10 {
            let payload = format!("data: Event {}\n\n", i);
            yield Bytes::from(payload);
        }
    };

    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Body::stream(events))
        .unwrap()
}
```

## Body Modes

Routes can specify their body handling mode in the manifest. This is parsed today and reserved
for future enforcement by adapters and router helpers:

```toml
[[triggers.http]]
path = "/upload"
methods = ["POST"]
handler = "my_app::handlers::upload"
body-mode = "buffered"  # or "stream"
```

| Mode       | Behavior                                              |
| ---------- | ----------------------------------------------------- |
| `buffered` | Body is fully read into memory before handler runs    |
| `stream`   | Body is passed as a stream for progressive processing |

## Transparent Decompression

EdgeZero automatically decompresses gzip and brotli responses from upstream services:

```rust
// Proxied response with Content-Encoding: gzip is automatically decoded
let response = proxy.forward(request).await?;
// response.body is now decompressed
```

This happens transparently in the adapter layer using shared decoders from `edgezero-core`.

## Memory Considerations

Streaming is essential for:

- Large file downloads
- Video/audio content
- Real-time data feeds
- Responses larger than available memory

::: warning Platform Limits
Edge platforms have memory constraints. A Fastly Compute instance has ~128MB by default. Always stream large responses rather than buffering.
:::

## Chunked Transfer

When the response size is unknown, EdgeZero uses chunked transfer encoding:

```rust
#[action]
async fn dynamic_content() -> Response {
    let stream = generate_content_stream();

    // No Content-Length header needed
    Response::builder()
        .status(200)
        .header("content-type", "application/octet-stream")
        .body(Body::stream(stream))
        .unwrap()
}
```

## Next Steps

- Learn about [Proxying](/guide/proxying) for forwarding requests upstream
- Explore adapter-specific streaming in [Fastly](/guide/adapters/fastly) and [Cloudflare](/guide/adapters/cloudflare) guides
