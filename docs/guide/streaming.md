# Streaming

EdgeZero preserves lazy streaming response bodies on every adapter. Each adapter owns body
delivery through its strongest platform boundary; no standard entrypoint collects the complete
downstream response before handoff.

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

The router keeps `Body::Stream` intact until the adapter response coordinator:

1. Your handler returns `Body::stream(...)` with a `Stream` of chunks
2. Core validates response framing and wraps declared-length enforcement around the source
3. The adapter polls one chunk at a time under its host's demand or write readiness
4. Accepted bytes, deadline/abort events, and one terminal outcome are recorded by that adapter

Axum uses a connection-local Hyper body, Cloudflare a JavaScript stream writer, Fastly
`stream_to_client`, and Spin a WASI body writer. See [Capabilities](/guide/capabilities) for the
exact acceptance and completion boundary on each target.

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

::: warning Completion semantics
All adapters preserve progressive production, but their response-egress capability cells remain
BestEffort because host acceptance or close does not prove end-client receipt. Fastly source polls
and hostcalls are synchronous and cannot be preempted.
:::

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

## Outbound Response Decompression

The outbound client decodes a single bare `gzip` or `br` content coding through shared
`edgezero-core` decoders. Unknown, parameterized, or stacked codings pass through unchanged.
Encoded transport bytes, decoded output, and final buffered bytes have independent limits; see
[Capabilities](/guide/capabilities#limits-and-accounting).

```rust
let request = OutboundRequest::get("https://api.example.com/data")?
    .max_encoded_response_bytes(2 * 1024 * 1024)
    .max_decoded_response_bytes(8 * 1024 * 1024)
    .stream_response();
let body = client.send(request).await?.into_body();
```

Multi-member gzip streams are decoded through every member and drained to transport EOF.
For Brotli, the stream window is checked before decoder allocation and decoder state is charged
against the configured policy limit.

## Memory Considerations

Lazy streaming is useful for:

- Large file downloads
- Video/audio content
- Real-time data feeds
- Responses larger than available memory

::: warning Platform limits
Edge platforms have finite memory. Lazy response delivery removes whole-body collection but does
not bound provider queues, SDK allocations, source chunk size, or transport buffers. Bound source
chunks and application work, and use the capability matrix when certifying a deployment.
:::

## Chunked Transfer

Applications may omit `Content-Length` when the response size is unknown:

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

The provider owns the final wire framing. EdgeZero does not guarantee a particular HTTP/1 transfer
coding; it normalizes framing and enforces the application's declared payload length before the
adapter writes body bytes.

## Next Steps

- Learn about [Outbound HTTP](/guide/proxying) for upstream requests
- Explore adapter-specific streaming in [Fastly](/guide/adapters/fastly) and [Cloudflare](/guide/adapters/cloudflare) guides
