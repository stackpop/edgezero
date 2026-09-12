# Capabilities

Capabilities let an application state which portable runtime behaviors it depends on.
`edgezero build`, `serve`, and `deploy` compare the manifest declaration with the selected
adapter before starting the adapter command.

## Declaration

```toml
[capabilities]
required = ["outbound-deadlines"]
optional = ["outbound-http"]

[capabilities.outbound]
hosts = ["https://api.example.com", "https://*.example.net:*"]
```

A capability cannot appear more than once or in both lists. `required` accepts `Native`
and `BoundedCooperative`; it rejects `BestEffort` and `Unsupported`. `optional` never
blocks an operation, but degraded and unavailable support is logged. A missing or malformed
capability contract fails closed when it contains required entries.

Support levels mean:

- `Native`: fully supported without a documented deviation.
- `BoundedCooperative`: enforced with a documented deterministic bound. No current outbound
  matrix cell uses this level.
- `BestEffort`: implemented, but with a documented behavioral deviation or an unverified
  deployment prerequisite.
- `Unsupported`: unavailable on that adapter.

## Ingress Matrix

| Capability                       | Axum        | Cloudflare  | Fastly      | Spin        |
| -------------------------------- | ----------- | ----------- | ----------- | ----------- |
| `ingress-admission`              | Native      | Native      | Native      | Native      |
| `inbound-read-deadlines`         | Native      | BestEffort  | BestEffort  | BestEffort  |
| `raw-ingress-head-limits`        | Unsupported | Unsupported | Unsupported | Unsupported |
| `raw-ingress-framing-validation` | Unsupported | Unsupported | Unsupported | Unsupported |

All standard adapters resolve the route and run the application admission policy before
middleware, handler dispatch, or body polling. Axum can preempt an asynchronous body read.
Cloudflare and Spin race pending reads against platform timers and enforce absolute checks
around ready results; both require deployed probes before host-side teardown latency can be
bounded. Fastly cannot preempt a synchronous host body read. Those limitations keep the three
deadline cells at `BestEffort`.

All current adapters expose `IngressHeadAccounting::HostManaged` and
`IngressFraming::HostManaged`. They perform defensive checks on the normalized request
information they receive, but those checks do not prove parser-boundary accounting or
ambiguous HTTP/1 framing rejection. Neither raw-ingress capability is currently available on
any adapter.

Admission is body-blind. An application may opt into `ReadBodyBeforeFallback` for pre-resolved
404/405 requests when it needs body-limit precedence: clean EOF at or below the selected cap
retains the canonical 404/405, the first byte over returns the application-selected buffered
`on_exceeded` response first, and expiration of the one absolute read deadline returns the
application-selected buffered `on_timeout` response first. The application-supplied ingress
grant remains live throughout the drain and is released before response conversion, when the
core drain future is dropped, or when the finite read deadline terminates the drain. This path
discards the body and invokes no route middleware, handler, or request context. `Refuse` remains
the zero-read overload path, and ordinary admission retains the immediate, no-poll 404/405
behavior. Axum's current `block_in_place` bridge is not cancellable: aborting the outer Tower
service future does not promptly drop the inner fallback drain. The configured absolute read
deadline still terminates that drain and releases its grant and body source; tests pin this
deadline-bounded, rather than cancellation-bounded, release behavior.

## Config Matrix

| Capability                      | Axum        | Cloudflare  | Fastly      | Spin        |
| ------------------------------- | ----------- | ----------- | ----------- | ----------- |
| `config-read-allocation-bounds` | Unsupported | Unsupported | Unsupported | Unsupported |
| `config-read-deadlines`         | BestEffort  | BestEffort  | BestEffort  | BestEffort  |

Every adapter enforces per-value and cumulative guest-visible config byte caps before EdgeZero
retains or processes a returned value. That does not bound provider-side materialization,
parser allocation, SDK copies, or other work performed before the value reaches guest code, so
`config-read-allocation-bounds` remains `Unsupported` everywhere.

Config reads use one absolute extraction deadline with pre-read and post-ready checks, but
current provider operations are not proved cancellable within a finite wall-clock interval.
They therefore report `BestEffort` for `config-read-deadlines`. The
[configuration design](https://github.com/stackpop/edgezero/blob/main/docs/superpowers/specs/2026-06-16-blob-app-config.md#632-bounded-cancellable-extraction-reads)
defines the normative accounting, cancellation, error, and promotion requirements.

## Response Egress Matrix

| Capability                     | Axum        | Cloudflare  | Fastly      | Spin        |
| ------------------------------ | ----------- | ----------- | ----------- | ----------- |
| `response-egress-abort`        | Unsupported | Unsupported | Unsupported | Unsupported |
| `response-egress-backpressure` | Unsupported | Unsupported | Unsupported | Unsupported |
| `response-egress-completion`   | Unsupported | Unsupported | Unsupported | Unsupported |
| `response-write-deadlines`     | Unsupported | Unsupported | Unsupported | Unsupported |

These cells describe transport-observable client-response delivery, not response conversion.
Axum currently emits `ResponseReturned` with zero written bytes after conversion and before
returning the response to Hyper. It does not observe Hyper acceptance, socket transmission,
disconnect, abort, or client completion. The other adapters likewise expose no proved
transport completion boundary. Fastly's `StreamingBody` and Spin/WASI's `BodyWriter` provide
lower-level send APIs, but the current EdgeZero entrypoints do not own those lifetimes.
Converter caps and deadline checks remain useful, but they do not satisfy these capabilities;
whole-response-lifetime certification remains blocked.

## Outbound Matrix

| Capability                              | Axum                              | Cloudflare                        | Fastly                            | Spin                              |
| --------------------------------------- | --------------------------------- | --------------------------------- | --------------------------------- | --------------------------------- |
| `outbound-http`                         | Native                            | Native                            | BestEffort⁹                       | Native                            |
| `outbound-complete-resource-accounting` | Unsupported[^resource-accounting] | Unsupported[^resource-accounting] | Unsupported[^resource-accounting] | Unsupported[^resource-accounting] |
| `outbound-header-fidelity`              | Native                            | BestEffort⁸                       | Native                            | Native                            |
| `outbound-deadlines`                    | Native                            | Native                            | BestEffort¹                       | BestEffort⁸                       |
| `outbound-flexible-phase-budget`        | Native                            | Native                            | BestEffort⁵                       | BestEffort⁵                       |
| `send-all-slot-isolation`               | Native                            | Native                            | BestEffort⁴                       | Native                            |
| `streamed-upload-deadlines`             | Native                            | Native                            | BestEffort²                       | BestEffort⁸                       |
| `lazy-streamed-response-passthrough`    | BestEffort³                       | Native                            | BestEffort⁶                       | BestEffort⁷                       |

[^resource-accounting]:
    Complete accounting includes provider parsing and field-section
    materialization, informational responses, final headers, trailers, native receive chunks,
    guest buffers and spare capacity, decoder state, allocator metadata, and runtime copies.
    Current provider APIs do not expose or bound every term, so all adapters report
    `Unsupported`. The narrower EdgeZero-owned limits below still apply.

¹ Fastly cannot preempt cold dynamic-backend registration or guest-to-origin writes. Its
receive timers and absolute checks still cover the documented warm, response-read portions,
but they are not one end-to-end wall-clock guarantee.

² Fastly checks the absolute deadline between streamed upload chunks, but it cannot preempt a
stalled source pull or host write.

³ Axum collects a portable non-`Send` response stream before passing it to Hyper. Collection
is capped at 16 MiB by `AXUM_RESPONSE_STREAM_BUFFER_BYTES`.

⁴ Fastly dispatches slots sequentially and harvests response bodies in input order. Cold
backend registration can delay a later dispatch; unresolved uploads or an earlier body drain
can delay a sibling's terminal observation.

⁵ Fastly divides a total budget among provider phase timers. Spin's host may reject phase
timer settings and retain opaque defaults. Neither can promise one fully elastic pool.

⁶ The standard `#[fastly::main]` entrypoint cannot use Fastly's manual
`stream_to_client` response lifetime. EdgeZero therefore buffers the stream under
`FASTLY_RESPONSE_STREAM_BUFFER_BYTES`, currently 16 MiB.

⁷ Spin's current `SpinFullResponse` boundary is buffered. EdgeZero collects the stream under
`SPIN_RESPONSE_STREAM_BUFFER_BYTES`, currently 16 MiB.

⁸ Spin cancellation is cooperative until host teardown is observable within a documented
bound. Cloudflare exposes normalized header strings rather than original raw octets and field
boundaries, although it preserves the visible header semantics used by the client.

⁹ Fastly outbound HTTP uses dynamic backends, which must be enabled on the deployed service.
The CLI cannot currently prove that entitlement. If disabled, dispatch returns a typed 502
with the documented enablement diagnostic.

## Host Declarations

`[capabilities.outbound].hosts` configures platform host plumbing; it is not an application
authorization policy. Enforce user-controlled destination policy in application code.

- Omitting `hosts` defaults to `https://*:*`. Cleartext is not granted implicitly.
- `"*"` explicitly expands to both `http://*:*` and `https://*:*`.
- A bare host or wildcard subdomain defaults to HTTPS and port 443.
- An explicit scheme may be `http` or `https`; ports are `1..=65535` or `*`.
- Paths, queries, fragments, user information, whitespace, non-ASCII names, and malformed
  wildcards are rejected.

Spin receives the canonicalized entries through `allowed_outbound_hosts`. Cloudflare does not
need a build-time destination list. Fastly creates deterministic dynamic backends from the
canonical target. Axum applies the same manifest validation even though the native client does
not require provider host registration.

## Limits And Accounting

Each `OutboundRequest` owns independent limits:

| Control                      | Scope                                                   | Default |
| ---------------------------- | ------------------------------------------------------- | ------- |
| `max_request_body_bytes`     | Buffered or streamed request bytes                      | 8 MiB   |
| `max_encoded_response_bytes` | Upstream transport bytes before decoding                | Unset   |
| `max_decoded_response_bytes` | Identity or EdgeZero-decoded gzip/Brotli output         | Unset   |
| `max_response_bytes`         | Final buffered response, including raw passthrough      | 1 MiB   |
| `max_response_header_bytes`  | Cumulative guest-visible header name/value bytes        | Unset   |
| `max_response_header_count`  | Cumulative guest-visible header fields                  | Unset   |
| `max_brotli_window_bits`     | Brotli stream header checked before decoder allocation  | 24      |
| `max_brotli_decoder_bytes`   | Pinned policy charge for Brotli decoder state           | 32 MiB  |
| `max_chunk_bytes`            | Maximum emitted item size after decoding or passthrough | Unset   |

The encoded counter applies to every response path. The decoded counter applies to identity
and gzip/Brotli data decoded by EdgeZero, but not to unknown, stacked, parameterized, or other
raw passthrough encodings. The final buffered cap is independent of both. This separation lets
an application permit a larger raw body while keeping decoded expansion small.

`max_chunk_bytes` is an item-shape guarantee, not a source-allocation limit: splitting a
`Bytes` value may retain its original allocation, and a provider may have already materialized
the source chunk. Header controls cover guest-visible fields, not opaque parser allocations,
informational blocks that the SDK hides, or provider-owned trailers. These exclusions are why
`outbound-complete-resource-accounting` is `Unsupported` on every current adapter.

For `send_all`, bound both the number of requests and every per-slot cap. Core-retained payload
is approximately the sum of buffered request bodies, configured final response caps, and one
current chunk per actively draining slot. Adapter staging and host/runtime copies are outside
that formula.

## Encoding Boundaries

An application-provided request body is sent with its declared `Content-Encoding`; EdgeZero
does not silently recompress it. Response decoding is automatic only for a single bare `gzip`
or `br` coding. Multi-member gzip is drained through the final member and transport EOF.
Unknown or compound codings remain encoded and retain `Content-Encoding`.

Cloudflare has two separate manual-encoding controls. The outbound fetch requests raw encoded
response bytes so EdgeZero can enforce transport and decode limits. When those bytes are later
passed through to the downstream response, the converter also selects manual response-body
encoding so Workers does not encode them again. The latter does not promise an exact streamed
wire `Content-Length`, because Workers owns final framing.

## Batch Timing

`HttpClient::send_all` returns one `OutboundSlotResult` per input in the same order. Each slot
contains its own `elapsed` and `outcome`; one failure does not erase sibling outcomes.
`elapsed` starts at the single method-entry monotonic snapshot and ends when that slot becomes
terminal, including preflight validation, adapter setup, provider queueing, upload, headers,
buffered body drain, and any delayed guest observation. It is not pure transport RTT.
Preflight failures are timed from the same batch start, and a same-tick result may be zero.

Standard adapter wiring clones the application's monotonic clock into its outbound client, so
ingress timing, dispatch budgets, slot elapsed values, error precedence, and deferred body
streams remain in one clock domain. Explicit low-level outbound constructors use the default
clock. A backwards injected clock cannot enlarge the method-entry budget; backwards elapsed
sampling fails closed as an internal slot outcome with zero elapsed.

Axum, Cloudflare, and Spin drive complete eligible exchanges concurrently. Fastly records each
slot when it is observed during sequential dispatch/harvest, so the value can include sibling
delay; the `send-all-slot-isolation` matrix row exposes that distinction.

`send_all` accepts buffered request bodies and buffered response mode only. Use `send` for a
streamed upload or response, and consume a streamed response body inside the same concurrent
task that issued it.

## Missing Client

Adapters inject `HttpClient` into request extensions. A handler obtains it with
`RequestContext::http_client()`. The accessor returns `None` when custom/manual wiring omitted
the client; handlers should return an explicit 501 or another application-selected fallback.
The generated project demonstrates the 501 behavior.
