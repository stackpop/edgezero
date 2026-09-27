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
3. Spin and Axum collect the stream into a buffer, and Fastly writes each chunk
   into a host-side body that is only sent once complete (Spin rejects streamed
   bodies over 16 MiB), so on all three the client receives the whole body at
   once

On Cloudflare, streaming also keeps memory flat, because each chunk is forwarded as
it is produced. On the buffering adapters a streamed body is collected in full
before the response goes out: Axum into an unbounded buffer, Spin into a buffer
capped at 16 MiB (larger bodies fail), and Fastly into a host-side body that is
only sent once complete. There `Body::stream` is an API-composability convenience,
not a memory saving, and a very large streamed response can exhaust memory or be
rejected. Only rely on progressive delivery (SSE, long-lived chunked responses) on
Cloudflare today.

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

On Fastly, Cloudflare, and Spin, EdgeZero automatically decompresses gzip and brotli
responses from upstream services (the Axum proxy client has no decode path):

```rust
// Proxied response with Content-Encoding: gzip is automatically decoded
let response = proxy.forward(request).await?;
// response.body is now decompressed
```

This happens transparently in the adapter layer using shared decoders from `edgezero-core`.

## Memory Considerations

On Cloudflare, the one adapter that forwards chunks as they are produced (see
[How Streaming Works](#how-streaming-works)), streaming is what makes these
workloads fit:

- Large file downloads
- Video/audio content
- Real-time data feeds
- Responses larger than available memory

::: warning Platform Limits
Edge platforms have memory constraints. A Fastly Compute instance has ~128MB by default. On Cloudflare, stream large responses rather than buffering. On Fastly, Spin, and Axum a streamed body is still collected in full before it is sent, so keep responses within the instance's memory regardless of how you build the body. Spin additionally caps streamed-body collection at 16 MiB; an already-buffered `Body::Once` bypasses that cap.
:::

## Chunked Transfer

When the response size is unknown, `Body::stream` lets the adapter send without a
`Content-Length`. Cloudflare delivers this as chunked transfer; the buffering
adapters compute the length once the body is collected:

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
- Explore adapter-specific streaming in the [Fastly](/guide/adapters/fastly), [Cloudflare](/guide/adapters/cloudflare), and [Spin](/guide/adapters/spin) guides
