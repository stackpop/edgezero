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
use edgezero_core::outbound::OutboundRequest;
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
        .timeout(Duration::from_secs(2))
        .max_request_body_bytes(256 * 1024)
        .max_encoded_response_bytes(2 * 1024 * 1024)
        .max_decoded_response_bytes(4 * 1024 * 1024)
        .max_response_bytes(4 * 1024 * 1024);
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
use edgezero_core::outbound::OutboundRequest;

let request = OutboundRequest::post("https://api.example.com/events")?
    .header("accept", "application/json")?
    .json(&payload)?;
let response = client.send(request).await?;
let decoded: ApiResponse = response.json()?;
```

Buffered response mode is the default. Call `stream_response()` for a streamed response and
consume `response.into_body()` while its absolute request deadline is still active. Use
`into_bytes_bounded`, `into_bytes_bounded_until`, or `json_bounded_until` when application code
performs an additional collection step.

## Batch Requests

`send_all` accepts buffered request bodies and buffered response mode only. It returns an
index-aligned `Vec<OutboundSlotResult>` rather than failing the whole batch:

```rust
let slots = client.send_all(requests).await;
for slot in slots {
    match slot.outcome {
        Ok(response) => record_success(slot.elapsed, response.status()),
        Err(error) => record_failure(slot.elapsed, error),
    }
}
```

Every slot is timed from the batch method-entry snapshot through its own terminal observation,
including preflight and body buffering. It is not pure network RTT. Bound the request count and
every per-request body limit; EdgeZero intentionally has no global batch-concurrency or memory
cap.

## Platform Behavior

The API is portable, but not every runtime can provide identical timing, header, or lazy-stream
semantics. See [Capabilities](/guide/capabilities) for the exact support matrix, Fastly's dynamic
backend prerequisite, the 16 MiB Axum/Fastly/Spin response-conversion fallback, Cloudflare's
manual encoding boundaries, and complete resource-accounting exclusions.
