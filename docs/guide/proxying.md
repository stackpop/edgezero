# Outbound HTTP

EdgeZero exposes one provider-neutral HTTP client to handlers. Adapters inject an `HttpClient`
into request extensions, while application code builds `OutboundRequest` values and receives
typed `OutboundResponse` or `EdgeError` outcomes.

## Forward An Inbound Request

This example preserves the inbound method and body, replaces the target URI, removes a private
header, and converts the outbound response back into a core response.

```rust
use edgezero_core::action;
use edgezero_core::context::RequestContext;
use edgezero_core::error::EdgeError;
use edgezero_core::http::{HeaderValue, Response, Uri};
use edgezero_core::outbound::{OutboundCachePolicy, OutboundRequest};
use std::num::NonZeroU64;
use std::time::Duration;

#[action]
async fn forward_with_auth(
    RequestContext(ctx): RequestContext,
) -> Result<Response, EdgeError> {
    let client = ctx
        .http_client()
        .ok_or_else(|| EdgeError::not_implemented("outbound HTTP client not available"))?;
    let target: Uri = "https://api.example.com/v1/data"
        .parse()
        .map_err(|_| EdgeError::bad_request("invalid upstream URI"))?;

    let mut request = OutboundRequest::from_request(ctx.into_request(), target)?
        .cache_policy(OutboundCachePolicy::Bypass)
        .timeout(Duration::from_secs(2))
        .max_request_body_bytes(256 * 1024)
        .max_encoded_response_bytes(2 * 1024 * 1024)
        .max_decoded_response_bytes(4 * 1024 * 1024)
        .max_response_bytes(4 * 1024 * 1024)
        .max_response_header_bytes(64 * 1024)
        .max_response_header_count(100)
        .max_chunk_bytes(NonZeroU64::new(64 * 1024).unwrap_or(NonZeroU64::MIN));
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_static("Bearer secret-token"),
    );
    request.headers_mut().remove("cookie");

    let mut response = client.send(request).await?;
    response
        .headers_mut()
        .insert("x-forwarded-by", HeaderValue::from_static("edgezero"));
    response.into_response()
}
```

Outbound failures remain typed. Deadline expiry maps to `GatewayTimeout { cause }`; transport,
protocol, and codec failures map to inspectable `BadGatewayReason` values; resource limits use
`ResponseLimitReason`. Applications do not need to classify errors by matching messages.

## Construct A Request

Use `OutboundRequest::get`, `post`, or `new` for a new request. `from_request` is intended for
forwarding an inbound request and performs the same target validation and hop-by-hop header
normalization immediately.

```rust
use edgezero_core::outbound::{OutboundCachePolicy, OutboundRequest};
use std::num::NonZeroU64;
use std::time::Duration;

let request = OutboundRequest::post("https://api.example.com/events")?
    .cache_policy(OutboundCachePolicy::Bypass)
    .timeout(Duration::from_secs(2))
    .max_request_body_bytes(256 * 1024)
    .max_encoded_response_bytes(2 * 1024 * 1024)
    .max_decoded_response_bytes(4 * 1024 * 1024)
    .max_response_bytes(4 * 1024 * 1024)
    .max_response_header_bytes(64 * 1024)
    .max_response_header_count(100)
    .max_chunk_bytes(NonZeroU64::new(64 * 1024).unwrap_or(NonZeroU64::MIN))
    .max_brotli_window_bits(24)
    .max_brotli_decoder_bytes(32 * 1024 * 1024)
    .header("accept", "application/json")?
    .json(&payload)?;
let response = client.send(request).await?;
let decoded: ApiResponse = response.json()?;
```

`host_authority_override("virtual.example:8443")` changes only the outgoing HTTP authority. The
request URI still selects the connection destination, TLS SNI, and certificate identity. Axum and
Fastly support that separation, Cloudflare is BestEffort pending a deployed wire probe, and Spin
rejects the override during preflight.

Buffered response mode is the default. Call `stream_response()` for a streamed response and
consume `response.into_body()` while its absolute request deadline is still active. Use
`into_bytes_bounded`, `into_bytes_bounded_until`, or `json_bounded_until` when application code
performs an additional collection step.

`max_chunk_bytes` opt-in rechunks guest-visible response items before they reach the application.
It bounds emitted item shape, not allocation: split `Bytes` values can retain their original
backing allocation, and a provider SDK may allocate the source chunk before EdgeZero receives it.

## Batch Requests

`start_batch_until` accepts buffered request bodies and buffered response mode only. It yields
terminal slots in observed completion order while retaining each original input index. This lets
an application keep completed results when its own cutoff wins:

```rust
use edgezero_core::time::Deadline;

let mut batch = client.start_batch_until(requests, Deadline::after(Duration::from_secs(2)));
while let Some(item) = batch.next().await {
    match item.result.outcome {
        Ok(response) => record_success(item.index, item.result.elapsed, response.status()),
        Err(error) => record_failure(item.index, item.result.elapsed, error),
    }
}
```

`send_all_until` collects the same driver into an index-aligned
`Vec<Option<OutboundSlotResult>>`; `None` means that slot was unresolved at cutoff. Every terminal
slot is timed from the batch method-entry snapshot through its own terminal observation, including
preflight and body buffering. It is not pure network RTT. Bound the request count and every
per-request body limit; EdgeZero intentionally has no global batch-concurrency or memory cap.

## Platform Behavior

The API is portable, but not every runtime can provide identical timing, header, or lazy-stream
semantics. See [Capabilities](/guide/capabilities) for the exact support matrix, Fastly's dynamic
backend prerequisite, each adapter's lazy downstream handoff boundary, Cloudflare's manual
encoding boundaries, and complete resource-accounting exclusions.
