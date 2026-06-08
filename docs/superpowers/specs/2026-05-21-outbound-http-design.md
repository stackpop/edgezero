# EdgeZero Outbound HTTP — Design Spec

> **Status:** Draft, revised through review rounds 1–49 (round 49 = URI canonicalization split into request-URI vs manifest-host-entry rule sets, `OutboundDeadlines` enum scope updated to include `send_all` active body-drain, Fastly streamed-upload response timeout corrected to first-byte phase, appendix bookkeeping refreshed) · **Date:** 2026-06-06
> **Branch:** `docs/outbound-http-spec` · **Audience:** EdgeZero maintainers
> **Driving pattern:** fan-out HTTP workloads — N concurrent outbound requests under a shared wall-clock deadline, results harvested in input order. The spec is written against this pattern as a portable substrate; it deliberately does not name a specific consumer.
> **Target codebase baseline:** [`stackpop/edgezero` PR #269](https://github.com/stackpop/edgezero/pull/269) (`feature/extensible-cli`, rev `b4c80e9`) — **not yet merged into `main`**. PR #269 introduces the multi-store manifest (`ManifestStores { config, kv, secrets }`), the `edgezero_cli::adapter::execute(..)` shell-or-registry dispatcher, the expanded `AdapterAction` (`AuthLogin` / `AuthLogout` / `AuthStatus` / `Build` / `Deploy` / `Serve`), separate `Adapter::provision(..)` and config-validation hooks, Spin SDK 6 / wasip2, the contributor-only `demo` command replacing `dev`, and the new `examples/app-demo/crates/app-demo-cli` integration crate.
> **Current checkout (pre-#269):** `crates/edgezero-cli/src/args.rs` still has `Command::{Build, Deploy, Dev, New, Serve}`; `crates/edgezero-adapter/src/registry.rs` still has `AdapterAction::{Build, Deploy, Serve}`; `main.rs` still handles `Command::Dev`. **The CLI rows in §3.5.3 / §5.4 / §7 / Appendix AR are contingent on PR #269 landing.** If PR #269 ships in a different shape, the affected rows must be re-rebased; if it never lands, the spec's CLI surface degrades to the current `build` / `serve` / `deploy` / `dev` set plus the `ensure_capabilities` gate applied at each of those four call sites (the round-1–43 wording). Spec §1 / §3.1 / §3.2 / §3.3 / §3.4 / §4 (the outbound HTTP design itself) is independent of PR #269 and lands either way.
> **Where rebase claims live (authoritative surfaces):** §3.5.3 build-enforcement, §3.5.2 `Adapter` trait shape (showing both the pre-#269 and PR-#269 forms), §5.4 capability test rows mentioning `demo` / `auth` / `provision` / `config push|validate`, and the §7 `edgezero-cli` migration bullet. Earlier appendices that quote `handle_build` / `handle_serve` / `handle_deploy` / `handle_dev` / `edgezero dev` are the round-1–43 historical resolution journal and remain accurate against the current checkout. **Appendix AR is the round-44 rebase snapshot and is now superseded by Appendices AS / AT / AU / AV / AW / AX** (rounds 44–49): AR still describes the gate as "a single `Adapter::execute` dispatch point" — that wording was corrected to "four pre-dispatch gates" in AS, then to "five gate sites" in AU. Treat AR as round-44 history; the §3.5.3 + §7 active text is authoritative.

## 1. Overview

### 1.1 Goal

Make EdgeZero a production-safe substrate for **outbound HTTP fan-out**: an app must be
able to issue many independent target requests concurrently, enforce per-request and
whole-fan-out batch deadlines, keep memory predictable, and run the *same handler source*
unchanged on Axum, Cloudflare Workers, Fastly Compute, and Spin.

"Predictable memory" here means: a documented, bounded cost per buffered response and
per inbound body, plus an explicit batch-level memory model the app controls (§3.4.4).
It does **not** mean EdgeZero imposes a global allocation ceiling.

### 1.2 Context

Applications today proxy a single outbound request through the current
`ProxyClient` / `ProxyHandle`. What is missing:

- A first-class, **independently constructed** outbound request type.
- **True concurrent fan-out.** Today's Fastly client calls `pending_request.wait()`
  inside a single `send()`, so any `join_all` of `send()` calls runs strictly serially.
- A **portable deadline** primitive.
- **Bounded buffering** helpers with clean error mapping.
- A way for an app to **declare required capabilities** and fail the build early.

### 1.3 Non-goals

- No consumer-specific target logic in EdgeZero.
- EdgeZero does not own privacy, the external batch protocol, or target allowlists. It exposes
  `OutboundRequest::uri()` so apps enforce their own allowlist; it never blocks a
  request itself.
- No new direct dependency on `tokio`, `reqwest`, `fastly`, `worker`, or `spin-sdk` in
  application/library crates or in `edgezero-core`. Those stay inside adapter crates.
- No general-purpose "timeout any future" combinator in this spec — see §3.3.5.

### 1.4 Decisions locked before / during review

- **No backward compatibility.** `ProxyClient` is renamed and reshaped in place;
  `app-demo`, scaffolding templates, docsare migrated. No deprecated
  aliases.
- **One portable buffered fan-out primitive.** `send_all` is the only fan-out API
  for buffered request bodies + buffered responses. Its **input/output contract**
  is identical on every adapter (preflight, index alignment, per-slot Ok/Err
  shape — see §3.1.1 / §3.2). **Cross-slot timing is not uniform** — on
  Axum/CF/Spin `join_all` fans out body drains concurrently, on Fastly buffered
  body drains run serially in harvest order (§3.3.4); the
  `send-all-slot-isolation` capability (§3.5.1 footnote 4) lets apps require
  the stricter guarantee and fail closed on Fastly. **Streamed-response fan-out
  is explicitly non-portable** — Fastly's dispatch-all-then-harvest model and
  lack of a concurrent body-drain primitive (§3.3.4 / §3.2 / §8 risk 8) make
  it unsafe to expose as a portable primitive. Apps that need streamed-response
  concurrency use single `send` per request and orchestrate themselves; that is
  reactor-bearing only (Axum/CF/Spin), as is any concurrent body consumption.
  `futures::future::join_all` is an internal adapter detail for `send_all`'s
  implementation on the three reactor-bearing adapters, never app-facing.
- **Unified body.** Outbound request and response bodies use the existing core `Body`
  type and may be **buffered (default)** or **streamed (opt-in)**. Streaming
  proxy-forwarding is preserved — it is not dropped (review finding / residual risk).
- **Deliverable:** this spec only. Implementation plan and code are follow-ups.

## 2. Current state (summary)

| Concern | Today | File |
| --- | --- | --- |
| Outbound trait | `ProxyClient::send(ProxyRequest) -> Result<ProxyResponse, EdgeError>` | `crates/edgezero-core/src/proxy.rs:16` |
| Handle | `ProxyHandle` (`Arc<dyn ProxyClient>`), `RequestContext::proxy_handle()` | `proxy.rs:21`, `context.rs:97` |
| Request type | `ProxyRequest::new(method, uri)`; `ProxyRequest::from_request` (streaming) | `proxy.rs:138`, `proxy.rs:100` |
| Body | `Body { Once(Bytes), Stream(..) }`; `Body::into_bytes_bounded(max)` exists | `body.rs:14`, `body.rs:76` |
| Errors | `EdgeError`: 400/422/404/405/503/500. No 502/504. `#[non_exhaustive]` | `error.rs:14` |
| Deadlines | None. `web_time::Instant` used only by `RequestLogger` | `middleware.rs:1` |
| Fastly send | `send_async_streaming()` then `pending_request.wait()` — serializes | `crates/edgezero-adapter-fastly/src/proxy.rs:30` |
| Fastly backend name | host with only `.`/`:` sanitized | `crates/edgezero-adapter-fastly/src/proxy.rs:110` |
| Manifest | `Manifest { adapters, app, environment, logging, stores, triggers }` | `manifest.rs:89` |
| Adapter trait | `Adapter { execute, name }` — no capability metadata | `crates/edgezero-adapter/src/registry.rs` |
| Contract tests | exist for Cloudflare/Fastly/Spin; **Axum has none** | `crates/edgezero-adapter-*/tests/contract.rs` |
| Scaffold templates | emit proxy code | `crates/edgezero-cli/.../handlers.rs.hbs`, `spin.toml.hbs:13` |
| Public docs | document `ProxyService`/`ProxyRequest` | `docs/guide/proxying.md`, `docs/guide/handlers.md`, `docs/guide/architecture.md`, `docs/guide/what-is-edgezero.md`, `docs/guide/adapters/*` |

## 3. Design

### 3.1 Outbound HTTP client abstraction

`crates/edgezero-core/src/proxy.rs` is renamed to `crates/edgezero-core/src/outbound.rs`.
Bodies use the **existing core `Body`** type (`Once(Bytes)` | `Stream(..)`), so a request
or response may be buffered or streamed. Buffered is the default;
streaming is an explicit opt-in that preserves proxy-forwarding.

#### 3.1.1 Adapter-facing trait — two required methods

```rust
// crates/edgezero-core/src/outbound.rs

#[async_trait(?Send)]
pub trait OutboundHttpClient: Send + Sync {
    /// Send a single request. Accepts streamed request bodies — this is the API
    /// for streaming proxy-forwarding (one inbound → one outbound).
    ///
    /// **`Buffered` mode:** `Ok(resp)` means the full exchange completed —
    /// headers AND the response body buffered within the deadline and the
    /// decompressed-byte cap. `Err(_)` is returned for transport failure
    /// (DNS/TLS/connect), deadline expiry, or over-cap.
    ///
    /// **`Streamed` mode:** `Ok(resp)` means headers completed. Body-phase
    /// failures surface later, when the caller consumes `resp.body`:
    /// - **Read errors / decompression failures / deadline expiry** during
    ///   chunk reads come from the deadline-aware stream wrapper (§3.3.3,
    ///   §4.3 "Streamed-response wrapping") as `Err(EdgeError::..)` chunks.
    /// - **Over-cap** only fires when the consumer uses a bounded helper
    ///   (`OutboundResponse::into_bytes_bounded(max)`, `into_bytes_bounded_until`,
    ///   `json_bounded[_until]`) — the streaming decoder itself does **not**
    ///   count bytes (§3.4.1 "Cap ownership"). Raw `into_response()` passthrough
    ///   carries no EdgeZero cap; the platform downstream wire is the budget.
    ///   Axum's response converter is the exception: it buffers, with its own
    ///   `AXUM_RESPONSE_STREAM_BUFFER_BYTES` cap → 502 on overflow (§4.1).
    /// If the caller has *already started writing the downstream response
    /// headers* (e.g. a proxy-forward via `into_response()` that the platform
    /// converter has begun sending), HTTP no longer allows a status change.
    /// The adapter response converter then **aborts the downstream body** (TCP
    /// close on HTTP/1.1, RST_STREAM on HTTP/2) and logs the originating
    /// `EdgeError`; clients observe an early close, not a synthetic 502/504.
    /// See §5.4 for the cross-adapter contract test.
    async fn send(&self, req: OutboundRequest) -> Result<OutboundResponse, EdgeError>;

    /// Issue every request concurrently, then collect every result.
    ///
    /// The returned vec is index-aligned with `reqs`: `out[i]` is the result of
    /// `reqs[i]`. **Input handling is isolated per slot**: a `bad_request` for
    /// one preflight failure never changes another slot's input shape, and one
    /// slot's `Ok`/`Err` type never mutates another's. Cross-slot *timing* is
    /// **not uniformly isolated** — see the `send-all-slot-isolation` capability
    /// (§3.5.1 footnote 4): on Axum/CF/Spin it's `Native` (concurrent body
    /// drains), but on Fastly it's `BestEffort` because buffered-body drains
    /// run in harvest order (§3.3.4), so a slot whose own budget would have
    /// covered it can still return `gateway_timeout` because an earlier slot
    /// monopolized harvest. Apps that require the stricter cross-slot timing
    /// guarantee declare the capability required and get a hard build failure
    /// on Fastly. `send_all(vec![])` returns `vec![]`.
    ///
    /// **Memory model:** worst-case **persistent collected buffer** memory for
    /// one `send_all` is `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ`
    /// (per-slot caps). Transient overhead during a buffered drain adds up to
    /// one in-flight chunk per actively-draining slot (the
    /// `sizeof(current_chunk)` term from §3.4.1); the full bound is therefore
    /// `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ + Σⱼ
    /// sizeof(current_chunkⱼ)` where j ranges over slots currently in a drain
    /// step (§3.4.4). EdgeZero does NOT impose a global cap on N — apps are
    /// responsible for bounding the number of requests passed in. On Fastly all
    /// requests are in-flight at the host simultaneously to make fan-out work,
    /// so a `max_concurrency` knob would defeat the feature; instead, bound N
    /// at the application layer (typically the fan-out batch's target count).
    ///
    /// **Request bodies MUST be buffered (`Body::Once`).** A `Body::Stream`
    /// request body yields `out[i] = Err(EdgeError::bad_request("send_all
    /// requires buffered request bodies; use send() for a streamed upload"))`,
    /// identically on every adapter. This rule prevents Fastly's
    /// dispatch-all-then-harvest fan-out from serializing on slow request
    /// uploads.
    ///
    /// **Response mode MUST be Buffered.** A request whose `response_mode`
    /// is `Streamed` (via `stream_response()`) yields `out[i] =
    /// Err(EdgeError::bad_request("send_all requires buffered responses;
    /// use send() for a streamed response"))`, identically on every adapter.
    /// Reason: `send_all` returns its `Vec` only after every slot has reached
    /// headers, so a fast slot's deadline-aware streamed body wrapper has
    /// already been running while later siblings were still in headers phase
    /// — by the time the consumer gets the Vec, the fast slot's body may
    /// already be at-or-past its deadline. There is no concurrent
    /// body-consumption primitive in `send_all` to fix this (Fastly has no
    /// guest reactor, §3.3.5; even on Axum/CF/Spin a consumer iterating
    /// `out[i].body()` serially can't outrun the wrapper deadlines that have
    /// been ticking since headers). Apps that want streamed responses use
    /// single `send` and orchestrate concurrency themselves on the three
    /// reactor-bearing adapters. This rule keeps `send-all-slot-isolation`'s
    /// `Native` claim on Axum/CF/Spin honest — the cross-slot body-lifetime
    /// problem is removed by construction rather than papered over.
    ///
    /// **"Identical" scope.** The trait contract guarantees identical
    /// **input handling**: same preflight, same index alignment, same
    /// per-slot Ok/Err shape. The *cross-slot timing behaviour* is **not**
    /// uniform — see the `send-all-slot-isolation` capability (§3.5.1).
    /// On Axum/CF/Spin `join_all` fans out body drains concurrently and a
    /// slot's result reflects what it would have produced in isolation.
    /// On Fastly buffered-body drains run in harvest order (§3.3.4), so a
    /// slot can return `gateway_timeout` because an earlier slot
    /// monopolised harvest — even when its own `budget.deadline` would
    /// have covered its body in isolation. Apps that require cross-slot
    /// isolation declare the capability required and get a hard build
    /// failure on Fastly per §3.5.3.
    ///
    /// Per-slot `Ok`/`Err` semantics: since preflight rejects streamed bodies AND
    /// streamed responses, every surviving slot is Buffered on both sides, so the
    /// per-slot result shape matches `send`'s **Buffered-mode** semantics — `Ok(resp)`
    /// means the full exchange completed within the deadline and the body fits
    /// within `max_response_bytes`; `Err(_)` is transport / deadline / over-cap.
    /// Streamed-mode `Ok`-means-headers-only does not apply here because there are
    /// no streamed slots.
    async fn send_all(
        &self,
        reqs: Vec<OutboundRequest>,
    ) -> Vec<Result<OutboundResponse, EdgeError>>;
}
```

Both `send` and `send_all` are required on the trait. Each adapter implements both; in
practice they share an internal helper for buffered-body single sends, so the
single-request and batch paths cannot drift.

#### 3.1.2 App-facing handle

```rust
/// Cloneable handle stored in request extensions and handed to handlers.
/// This is the only outbound *client/handle* type application code touches;
/// handlers also build `OutboundRequest` and read `OutboundResponse`.
#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<dyn OutboundHttpClient>,
}

impl HttpClient {
    pub fn new(client: Arc<dyn OutboundHttpClient>) -> Self;
    pub fn with_client<C: OutboundHttpClient + 'static>(client: C) -> Self;

    pub async fn send(&self, req: OutboundRequest) -> Result<OutboundResponse, EdgeError>;
    pub async fn send_all(
        &self,
        reqs: Vec<OutboundRequest>,
    ) -> Vec<Result<OutboundResponse, EdgeError>>;
}
```

Obtained from the context:

```rust
// crates/edgezero-core/src/context.rs — replaces proxy_handle()
// After the round-6 restructure (§3.4.5), the context exposes `parts` rather than
// a `Request`. The `HttpClient` handle is stored in request extensions during
// adapter setup and retrieved via parts.extensions.
impl RequestContext {
    pub fn http_client(&self) -> Option<HttpClient> {
        self.parts.extensions.get::<HttpClient>().cloned()
    }
}
```

#### 3.1.3 Request and response types

```rust
pub struct OutboundRequest {
    method: Method,
    uri: Uri,                            // validated + canonicalized; see below
    headers: HeaderMap,
    body: Body,                          // buffered or streamed
    timeout: Option<Duration>,           // per-request budget
    deadline: Option<Deadline>,          // shared absolute cap; copy one value into every target request, do not recompute per request (see §3.3.2)
    response_mode: ResponseMode,         // Buffered { max_bytes } (default) | Streamed
    max_request_body_bytes: usize,       // cap when `body` is Body::Stream (default 8 MiB)
}

/// How the adapter delivers the response body. Default is `Buffered`.
pub enum ResponseMode {
    /// Adapter reads the full body within the deadline, enforcing a decompressed
    /// byte cap. `OutboundResponse.body` is `Body::Once`.
    Buffered { max_bytes: usize },   // default max_bytes = DEFAULT_MAX_RESPONSE_BYTES
    /// Adapter returns headers; `OutboundResponse.body` is `Body::Stream`. The
    /// caller buffers later (e.g. `into_bytes_bounded`) or passes the body through.
    Streamed,
}

impl OutboundRequest {
    /// Constructors validate **and canonicalize** the URI:
    ///
    /// - Scheme must be `http` or `https` (plain `http` is permitted —
    ///   required for loopback contract tests). Other schemes →
    ///   `Err(EdgeError::bad_request("outbound URI scheme must be http or
    ///   https"))`.
    /// - An authority must be present. Missing authority →
    ///   `Err(EdgeError::bad_request("outbound URI must be absolute with
    ///   authority"))`.
    /// - **Userinfo is rejected.** `https://user:pass@example.com` →
    ///   `Err(EdgeError::bad_request("outbound URI must not contain
    ///   userinfo; pass credentials via the `authorization` header"))`.
    ///   This keeps the Fastly backend Host override (§4.3) unambiguous and
    ///   stops accidental credential leakage.
    /// - **Fragments are rejected at the string-input boundary.**
    ///   `OutboundRequest::get("https://x/p#anchor")` and `::post(..)` parse
    ///   the input as a string *first* (they take `impl AsRef<str>` — see
    ///   below) and reject a `#` before `http::Uri` ever sees it, with
    ///   `Err(EdgeError::bad_request("outbound URI must not contain a
    ///   fragment"))`. `http::Uri` truncates at `#`, so a Uri-typed input
    ///   has already lost the fragment by the time we receive it.
    ///   `OutboundRequest::new(method, uri)` and `OutboundRequest::from_parts`
    ///   therefore cannot detect fragments — the caller built a `Uri`, which
    ///   means whatever was after `#` is gone. Documented asymmetry, not a
    ///   silent surprise: when constructing from a raw string use
    ///   `get`/`post` and you get fragment rejection for free; when you
    ///   already hold a `Uri`, fragments are not an issue because they were
    ///   stripped during `Uri` parsing.
    /// - **Default ports are normalized away.** A `Uri` parsed from
    ///   `https://example.com:443` is rewritten so `uri.port()` returns
    ///   `None`; `http://example.com:80` likewise. This means
    ///   `https://example.com` and `https://example.com:443` produce
    ///   identical `OutboundRequest`s — same `resolved_port` in the §4.3
    ///   Fastly identity, same Host override, one dynamic backend. Explicit
    ///   non-default ports (`:8443`, `:3000`) are preserved verbatim.
    /// - **Scheme and host are lowercased.** Per RFC 3986 §3.1 (scheme) and
    ///   §3.2.2 (host) both are case-insensitive, so `https://EXAMPLE.com`,
    ///   `HTTPS://example.com`, and `https://example.com` are the same
    ///   origin. The canonicalization rewrites the stored URI to lowercase
    ///   so `OutboundRequest::uri()` always reports the lowercase form,
    ///   and downstream consumers (Fastly backend identity in §4.3,
    ///   app-level allowlist checks, Spin `allowed_outbound_hosts`
    ///   matching) compare against one canonical spelling. Userinfo and
    ///   fragments are already rejected above; path and query are passed
    ///   through verbatim (case-sensitive per RFC 3986 §3.3 / §3.4).
    ///
    /// These canonicalizations run inside the constructors before the URI
    /// is stored, so every downstream consumer (Fastly backend identity, Host
    /// override, allowlist checks) sees a single canonical form.
    pub fn new(method: Method, uri: Uri) -> Result<Self, EdgeError>;
    /// `get` and `post` take `impl AsRef<str>` (not `TryInto<Uri>`) so the raw
    /// string is available for fragment detection *before* `http::Uri`
    /// truncates at `#`. The impl checks for `#` in the input bytes, then
    /// parses with `Uri::try_from(&str)`, then runs the rest of §3.1.3
    /// canonicalization. `&str`, `String`, and any `AsRef<str>` work; an
    /// already-built `Uri` goes through `OutboundRequest::new` (which cannot
    /// detect fragments because the `Uri` has already lost them — see
    /// "Fragments are rejected at the string-input boundary" above).
    pub fn get(uri: impl AsRef<str>) -> Result<Self, EdgeError>;
    pub fn post(uri: impl AsRef<str>) -> Result<Self, EdgeError>;

    /// Forward an inbound request to a new target. Preserves method and body
    /// (which may stream). Headers are normalized for proxy forwarding —
    /// the rules live in core so adapters cannot diverge:
    ///
    /// - hop-by-hop headers are stripped: `connection`, `keep-alive`,
    ///   `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`,
    ///   `transfer-encoding`, `upgrade` (RFC 7230 §6.1), plus every header
    ///   named in the inbound `connection` header value;
    /// - `host` is **dropped** from the headers. The adapter sets the final
    ///   `Host` value (or platform SDK equivalent) from
    ///   `req.host_authority()` at SDK-construction time — the same
    ///   canonical accessor every adapter uses (§3.1.4). The accessor
    ///   already encodes the rules: explicit port preserved when the URI
    ///   carries a non-default port (`https://example.com:8443` →
    ///   `Host: example.com:8443`); port stripped when default
    ///   (`https://example.com` → `Host: example.com`); IPv6 hosts
    ///   bracketed. **Adapters MUST NOT read `req.uri()` for the Host
    ///   value** — `host_authority()` is the single source of truth, so the
    ///   Fastly identity hash, the Cloudflare `set_header("host", ..)` arg,
    ///   the Axum reqwest Host setter, and the Spin outgoing-request Host
    ///   field all observe the same string. No part of the pipeline reads
    ///   `host` from `req.headers()`. `normalize_for_dispatch` re-strips
    ///   `host` defensively as a safety net for callers that reached past
    ///   `header(..)` via `headers_mut()`;
    /// - `content-length` is dropped — the adapter sets it from the new body
    ///   for `Body::Once`, or omits it (relying on chunked transfer) for
    ///   `Body::Stream`.
    ///
    /// All other headers are preserved verbatim. Validates `uri` per `new`.
    pub fn from_request(request: Request, uri: Uri) -> Result<Self, EdgeError>;

    /// Fallible: header name/value construction from arbitrary inputs can
    /// fail. The signature takes `impl AsRef<[u8]>` for both name and value
    /// — **not** `TryInto<HeaderName>` / `TryInto<HeaderValue>`. The standard
    /// `TryFrom<&str> for HeaderValue` path is built on
    /// `HeaderValue::from_str`, which rejects every byte outside visible
    /// ASCII and would refuse a valid non-ASCII UTF-8 header
    /// (`x-app-display-name: café`) before EdgeZero's own UTF-8 rule could
    /// run. By taking bytes directly:
    ///
    /// 1. `HeaderName::from_bytes(name.as_ref())` — strict name check (HTTP
    ///    grammar).
    /// 2. `std::str::from_utf8(value.as_ref()).is_err()` → reject with
    ///    `EdgeError::bad_request("header value is not valid UTF-8: <name>")`
    ///    (the EdgeZero rule per §3.1.4).
    /// 3. `HeaderValue::from_bytes(value.as_ref())` — applies the **HTTP
    ///    header-value byte rule** (visible ASCII + obs-text; rejects
    ///    control bytes like `\n`, `\0` that would enable header injection).
    ///    Combined with step 2, the values that survive are exactly the ones
    ///    that are **both** valid UTF-8 **and** valid HTTP header bytes — a
    ///    valid-UTF-8 string containing a forbidden control byte is still
    ///    rejected, which is intended security behaviour. Two distinct error
    ///    messages distinguish the cause (forbidden-bytes vs invalid-UTF-8).
    ///
    /// Works for `&str`, `String`, `&[u8]`, `Vec<u8>`, and `HeaderName` /
    /// `HeaderValue` (both `AsRef<[u8]>`).
    pub fn header<N, V>(self, name: N, value: V) -> Result<Self, EdgeError>
    where
        N: AsRef<[u8]>,
        V: AsRef<[u8]>;
    /// Escape hatch for callers holding already-validated
    /// `HeaderName`/`HeaderValue` (or building from `from_request`). The
    /// returned `HeaderMap` is not validated here — non-UTF-8 values and
    /// stray hop-by-hop / framing headers (`host`, `content-length`,
    /// `transfer-encoding`) are caught by the adapter's
    /// `normalize_for_dispatch` sweep before the request is issued (§3.1.4).
    pub fn headers_mut(&mut self) -> &mut HeaderMap;

    pub fn body(self, body: impl Into<Body>) -> Self;       // Bytes or a stream
    /// Serialize `value` as JSON and set the request body to the resulting
    /// bytes. Sets `content-type: application/json` only if the request has
    /// no `content-type` yet — a caller-set value is preserved unchanged.
    /// `content-length` is left to the adapter (it is recomputed from the
    /// serialized body for `Body::Once` and omitted for `Body::Stream`).
    /// Serialization failure yields `Err(EdgeError::internal(..))`.
    pub fn json<T: Serialize>(self, value: &T) -> Result<Self, EdgeError>;

    pub fn timeout(self, d: Duration) -> Self;
    pub fn deadline(self, d: Deadline) -> Self;
    pub fn max_response_bytes(self, n: usize) -> Self;      // sets Buffered { n }
    pub fn stream_response(self) -> Self;                   // sets Streamed

    /// Cap on the **request** body when it is a `Body::Stream` — see
    /// §4.1/§4.2/§4.3/§4.4. EdgeZero's core `Body::Stream` is `LocalBoxStream`
    /// (WASM-friendly, not `Send + 'static`), so adapters cannot hand it
    /// directly to a SDK that requires `Send` streams (notably reqwest
    /// without its `stream` feature). The contract is therefore: streamed
    /// request bodies are **bounded** by this cap on every adapter; adapters
    /// MAY pass the stream through to the platform natively (Fastly's
    /// `send_async_streaming`, Spin's WASI outgoing body) or buffer to
    /// `Bytes` within the cap before dispatch (Axum, Cloudflare). Over-cap
    /// during drain → `bad_request` (400) — a client-side misuse.
    /// Default `DEFAULT_OUTBOUND_REQUEST_BODY_BYTES = 8 MiB`.
    pub fn max_request_body_bytes(self, n: usize) -> Self;

    pub fn method(&self) -> &Method;
    pub fn uri(&self) -> &Uri;          // apps inspect this for their own allowlist
    pub fn headers(&self) -> &HeaderMap;

    // ---- Canonicalized URI accessors (adapter-facing, non-consuming) ----
    //
    // These four accessors are the **single canonical source** of the
    // host/port/SNI/cert-host split that every adapter needs. They are
    // derived from `self.uri()` after the §3.1.3 canonicalization rules
    // have rejected **userinfo and fragments**, validated the port, and
    // lower-cased scheme + host. **Path and query are preserved verbatim**
    // (per §3.1.3 — case-sensitive per RFC 3986 §3.3 / §3.4); they do not
    // appear in these accessors because none of them are host/port/SNI/cert
    // values, but they remain accessible via `self.uri()` for the wire-level
    // request line. **Adapters MUST consume these accessors rather than
    // re-deriving from `uri()`** for the host/port/SNI/cert split — both to
    // share the canonicalization logic and so the Fastly identity hash
    // sees a single canonical form (§4.3). They are also the values
    // tested by the Tier 1 half of the §5.4 four-value row.
    //
    // **Manifest `[capabilities.outbound].hosts` entries are a separate
    // grammar** (§3.5.4) — those entries are host-authority-only
    // declarations, so the manifest-host validator **rejects** path / query
    // / fragment / userinfo on the manifest side. That validator and the
    // request-URI canonicalization rules above share the userinfo / fragment
    // reject and the lowercase-scheme/host pass, but diverge on path/query:
    // request URIs pass them through; manifest host entries reject them. The
    // two rule sets must not be conflated.

    /// Connection target — always `"<host>:<port>"`, with the port resolved
    /// (default ports filled in: `http` → 80, `https` → 443). IPv6 hosts
    /// are bracketed (`[::1]:443`). This is what Fastly's
    /// `Backend::builder(name, ..)` expects and what Spin uses for its
    /// `allowed_outbound_hosts` rendering when the source had no explicit
    /// port. Stable across canonicalization (same value whether the input
    /// was `https://example.com` or `https://example.com:443`).
    pub fn backend_target(&self) -> String;

    /// Authority for the outgoing `Host` header. Carries the explicit port
    /// **only when it is non-default** for the scheme:
    /// `https://example.com:8443` → `"example.com:8443"`;
    /// `https://example.com` → `"example.com"`. IPv6 hosts are bracketed.
    /// This is what Fastly's `.override_host(..)` and Cloudflare's
    /// outbound `Request::set_header("host", ..)` consume; Axum / Spin pick
    /// it up the same way.
    pub fn host_authority(&self) -> String;

    /// SNI hostname — what an HTTPS adapter passes to its TLS stack's
    /// SNI setter (Fastly's `.sni_hostname(..)`, Spin/CF's underlying
    /// TLS config, etc.). Port-stripped, bracket-stripped for IPv6.
    /// **Returns `None` for IP-literal hosts** (IPv4 and IPv6) per
    /// RFC 6066 §3, which forbids SNI for IP literals. Adapters call
    /// the TLS-stack SNI setter only when this returns `Some`; for `None`
    /// the SNI extension is omitted from the ClientHello. **Adapters
    /// MUST NOT fall back to `uri().host()` for SNI** — `None` here
    /// means "send no SNI," not "derive it yourself." The cert verification
    /// host is `cert_host()` below, not this accessor.
    pub fn sni_hostname(&self) -> Option<&str>;

    /// Certificate-verification host — what an HTTPS adapter passes to
    /// its TLS stack's certificate-verification setter (Fastly's
    /// `.check_certificate(..)`, Spin/CF's underlying TLS verifier).
    /// **Always present for HTTPS, always port-stripped, always
    /// bracket-stripped for IPv6.** Unlike SNI, certificate verification
    /// is meaningful for IP literals too — verification will check the
    /// presented certificate's SAN against the IP literal (e.g. `127.0.0.1`,
    /// `::1`). Returns `None` only for non-HTTPS schemes (i.e. `http`),
    /// where the accessor is not used by the adapter. **This is the
    /// single canonical source for `.check_certificate(..)` arguments
    /// across every adapter**; adapters MUST NOT call `uri().host()` and
    /// post-process — they call `cert_host()` and pass it through.
    ///
    /// Concrete examples:
    /// - `https://example.com` / `https://example.com:443` → `Some("example.com")`
    /// - `https://example.com:8443` → `Some("example.com")` (port stripped — cert is not port-qualified)
    /// - `https://127.0.0.1` → `Some("127.0.0.1")`
    /// - `https://[::1]` / `https://[::1]:443` → `Some("::1")` (brackets stripped)
    /// - `http://example.com` → `None`
    pub fn cert_host(&self) -> Option<&str>;

    // ---- Adapter-facing inspection (non-consuming) ----
    /// Cheap non-consuming check used by `send_all` preflight (§3.1.1 /
    /// §4.1–§4.4): if `true`, the slot is rejected with `bad_request`
    /// *before* `send_one` is invoked, so the streamed-upload path is never
    /// reached from `send_all`. `send` (single-request) handles `Body::Stream`
    /// directly per its trait contract.
    pub fn is_stream_body(&self) -> bool;

    /// Cheap non-consuming check used by `send_all` preflight: if `true`
    /// (i.e. `response_mode == Streamed`), the slot is rejected with
    /// `bad_request` before `send_one` is invoked. `send` (single-request)
    /// handles streamed responses directly.
    pub fn is_stream_response(&self) -> bool;

    // ---- Adapter-facing disassembly / reassembly ----
    /// Consume the request into its constituent parts. Adapters call this
    /// inside `send` / `send_all` after `normalize_for_dispatch` has run,
    /// to hand the components to the platform SDK.
    pub fn into_parts(self) -> OutboundRequestParts;
    /// Round-trip constructor for adapters that need to destructure, mutate
    /// a single field, and reassemble (rare — most adapter paths consume).
    /// All fields are pub on `OutboundRequestParts`, so this is just a
    /// disciplined re-wrap and applies the same invariants as
    /// `new`/`get`/`post` (URI validation re-runs).
    pub fn from_parts(parts: OutboundRequestParts) -> Result<Self, EdgeError>;
}

/// Disassembled form of an `OutboundRequest`. Adapter-facing only.
pub struct OutboundRequestParts {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Body,
    pub timeout: Option<Duration>,
    pub deadline: Option<Deadline>,
    pub response_mode: ResponseMode,
    pub max_request_body_bytes: usize,    // applies when `body` is Body::Stream
}

pub struct OutboundResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,                     // Once in Buffered mode, Stream in Streamed mode
}

impl OutboundResponse {
    /// Adapter-facing constructor. Adapters build the response from the
    /// platform SDK's reply: status, normalized headers (decompression
    /// strips `content-encoding`/`content-length` per §3.4.1; non-UTF-8
    /// values are dropped per §3.1.4), and the body (`Body::Once` in
    /// `Buffered` mode after the adapter has drained and capped, or a
    /// `Body::Stream` wrapped with the deadline-aware wrapper described
    /// in `into_bytes_bounded_until` for `Streamed` mode).
    pub fn new(status: StatusCode, headers: HeaderMap, body: Body) -> Self;

    /// Adapter-facing destructure. Mirrors `OutboundRequest::into_parts`.
    pub fn into_parts(self) -> (StatusCode, HeaderMap, Body);

    /// Adapter-facing mutation point — used during construction (e.g. to
    /// strip `content-encoding` after decompression). App code uses the
    /// immutable `headers()` accessor instead.
    pub fn headers_mut(&mut self) -> &mut HeaderMap;

    // ---- App-facing accessors ----
    pub fn status(&self) -> StatusCode;
    pub fn is_success(&self) -> bool;       // 2xx
    pub fn headers(&self) -> &HeaderMap;
    pub fn body(&self) -> &Body;

    /// Buffer the body with a decompressed-byte cap. Works for both `Once`
    /// and `Stream`. Over-cap yields `Err(EdgeError::bad_gateway(..))` (502).
    ///
    /// This is NOT a thin wrapper over `Body::into_bytes_bounded` — that
    /// helper maps over-limit to `bad_request` (400), correct for inbound
    /// bodies but wrong for an over-large upstream response. This method
    /// performs its own bounded drain (pre-append checked accounting per
    /// §3.4.1) and maps to `bad_gateway` (502). On adapters that decompress
    /// (§3.4.1), the cap is enforced against decompressed output here too.
    ///
    /// **Effective-budget deadline is already honoured on a streamed body.**
    /// Per §3.3.3, adapters with platform timers (Axum/CF/Spin) wrap
    /// `Streamed` response bodies with a deadline-aware stream bounded by
    /// `dispatch_budget(req).deadline` — which is non-`None` even for
    /// timeout-only and no-deadline requests (the synthetic 30 s ceiling) —
    /// so a stalled upstream yields a `gateway_timeout` error chunk and
    /// this drain returns 504. Fastly's bounded-cooperative body check
    /// (§3.3.4) achieves the same end with a documented overshoot bound.
    /// There is no need to thread the deadline through manually — call
    /// `into_bytes_bounded_until(max, deadline)` only when you want to
    /// **cooperatively narrow** the failure timing on top of the request
    /// budget (see the precise bound and caveat below).
    pub async fn into_bytes_bounded(self, max: usize) -> Result<Bytes, EdgeError>;

    /// As `into_bytes_bounded`, but additionally bounded by a `Deadline`
    /// that the caller passes per drain. **The helper is a *cooperative*
    /// post-read / EOF validator, not a timer-backed race.** The bound it
    /// provides is *exactly* "the first `is_expired()` check that observes
    /// expiry returns `gateway_timeout`," where the check sites are
    /// enumerated below. A read that is already blocked when the deadline
    /// passes does **not** get preempted by this helper — it returns when
    /// the underlying source returns (chunk, EOF, or wrapper-emitted error
    /// chunk past the request budget), and the helper's *next* check (or
    /// post-return check for `Body::Once`) is what fires. Real-time
    /// preemption is the *wrapper's* job (the adapter installs a
    /// deadline-aware stream bounded by `dispatch_budget(req).deadline` at
    /// response construction time, per §3.3.3); the helper only catches the
    /// **tighter `until`** case at yield boundaries.
    ///
    /// Concretely, if the wrapper still has 500 ms and the caller passes
    /// `until_deadline = now + 100 ms`, and a body read happens to block
    /// for the full 500 ms, the helper does **not** return at 100 ms — it
    /// observes the expired `until` at the 500 ms post-read check and
    /// returns `gateway_timeout`. The bound the helper provides is "first
    /// expiry check at or after `until_deadline`," not wall-clock = `until`.
    /// Apps that need wall-clock preemption tighter than the request budget
    /// must either lower `dispatch_budget(req).deadline` (set
    /// `.deadline(min(req_deadline, app_inner_deadline))` on the builder)
    /// or split the work into a smaller request.
    ///
    /// Works on both `Body::Once` and `Body::Stream`:
    ///
    /// - **`Body::Once` (already buffered)**: the helper checks
    ///   `until_deadline.is_expired()` **at entry**, before doing anything
    ///   else, and returns `gateway_timeout` if expired. Otherwise it
    ///   checks the buffered length against `max` — under cap → `Ok(bytes)`;
    ///   over cap → `bad_gateway`. **Precedence: expired deadline beats
    ///   over-cap** (an over-cap error after the deadline has expired is
    ///   masked by the deadline check, since the caller's `until` rolled
    ///   the result regardless of cap behaviour). This entry-time check
    ///   makes single `send` + `Body::Once` callers see consistent
    ///   `gateway_timeout` semantics whether their response arrived
    ///   already-buffered or streamed.
    /// - **`Body::Stream`**: the helper checks `until_deadline.is_expired()`
    ///   **both before issuing each blocking body read and again after it
    ///   returns** — including the EOF read. Returns
    ///   `Err(EdgeError::gateway_timeout(..))` (504) on the first expired
    ///   check.
    ///
    /// **Enforcement composes layer-wise without sharing state.** The
    /// adapter wrapper installed at response construction time enforces
    /// the request's `dispatch_budget(req).deadline` by yielding
    /// `Err(EdgeError::gateway_timeout(..))` chunks past *that* deadline
    /// (§3.3.3); this helper enforces `until_deadline` cooperatively at
    /// the four check sites enumerated above (entry for `Body::Once`;
    /// before and after each underlying read including EOF for
    /// `Body::Stream`). **"Whichever fires first" is at yield boundaries
    /// only**: the wrapper's error chunk arrives in real time (timer-backed
    /// on Axum / CF / Spin; bounded-cooperative on Fastly per §3.3.4); the
    /// helper's `until_deadline` fires at the next check site. If the
    /// caller's `until_deadline` is tighter and the next underlying read
    /// returns promptly, the helper fires first; if the next underlying
    /// read blocks past `until` but within the wrapper's budget, the helper
    /// still fires (post-read check) and the helper's bound is "read
    /// latency + at most one extra check," not zero. There is no shared
    /// "effective deadline" stored on `OutboundResponse` (which carries
    /// only status / headers / body), and no `min(..)` computation in the
    /// helper. Apps that need a single combined check with **timer-backed
    /// preemption** of the tighter deadline pass
    /// `min(req_deadline, app_inner_deadline)` to `.deadline(..)` on the
    /// `OutboundRequest` builder instead of layering here — that pushes
    /// the tighter deadline into the wrapper, which is the only layer with
    /// real-time enforcement on Axum / CF / Spin.
    ///
    /// **Enforcement is layered.** The helper itself is cooperative on every
    /// adapter — its before-and-after-read `is_expired()` check cannot
    /// preempt a read in progress. Real-time enforcement of the request
    /// budget comes from the adapter wrapping streamed response bodies at
    /// construction time:
    ///
    /// - **Axum, Cloudflare, Spin** — the adapter wraps the response body
    ///   with a deadline-aware stream using its platform timer (tokio /
    ///   `worker::Delay` / wasi monotonic-clock), bounded by
    ///   `dispatch_budget(req).deadline`. That deadline is non-`None` for
    ///   every request (synthetic 30 s ceiling when `req.deadline` was
    ///   absent), so the wrapping is unconditional — *not* "only when
    ///   `req.deadline.is_some()`." Each chunk read is bounded by the
    ///   request's effective deadline, so a peer that stalls mid-stream
    ///   produces an error chunk at that deadline rather than blocking.
    ///   `into_bytes_bounded_until`'s helper-side `is_expired()` check on
    ///   the caller-supplied `until_deadline` is what catches the
    ///   *tighter* `until` case (e.g. the wrapper has 500 ms left but the
    ///   caller passed a 100 ms `until`) **at the next yield boundary**,
    ///   not in real time. If a read happens to block for the full 500 ms,
    ///   the helper returns at 500 ms with `gateway_timeout` (post-read
    ///   check observed expiry), not at 100 ms. Use
    ///   `min(req_deadline, app_inner_deadline)` on the builder for
    ///   timer-backed preemption.
    /// - **Fastly** — no guest async timer (§3.3.5), but the adapter still
    ///   wraps the streamed response body with a **cooperative
    ///   deadline-aware stream** that checks `budget.deadline.is_expired()`
    ///   **both before issuing the underlying body read and again after it
    ///   returns** (including the read that discovers EOF, per §3.3.4) and
    ///   emits a `gateway_timeout` error chunk past the deadline instead
    ///   of `Ok(chunk)` or stream-end. This makes `into_bytes_bounded`,
    ///   `into_response()` passthrough, and any other consumer of the
    ///   wrapped body honour the deadline uniformly — the deadline does
    ///   not depend on whether the caller chose this helper specifically.
    ///   Bounded-cooperative semantics apply: a stream that yields one
    ///   chunk and then stalls returns control on the host's
    ///   between-bytes-timeout (§3.3.4), so worst-case overshoot per chunk
    ///   gap is one between-bytes-timeout interval — never unbounded.
    ///
    /// The real-vs-bounded distinction matches the `outbound-deadlines`
    /// capability matrix in §3.5.2. Decompression-cap and 502-mapping
    /// behaviour matches `into_bytes_bounded`.
    pub async fn into_bytes_bounded_until(
        self,
        max: usize,
        deadline: Deadline,
    ) -> Result<Bytes, EdgeError>;
    /// JSON-decode the already-buffered body. Requires `Body::Once`; on a
    /// `Body::Stream` returns `Err(EdgeError::bad_gateway("response body
    /// not buffered; use json_bounded(max) or json_bounded_until(max,
    /// deadline)"))`. Malformed JSON yields `Err(EdgeError::bad_gateway(..))` —
    /// an upstream returning unparseable JSON is a 502 outcome, not a 400.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, EdgeError>;

    /// Buffer (with a decompressed-byte cap) then JSON-decode in one step.
    /// Consuming convenience for the `Streamed` mode: equivalent to
    /// `into_bytes_bounded(max).await` + `serde_json::from_slice`, with
    /// malformed JSON mapping to `bad_gateway` (502).
    pub async fn json_bounded<T: DeserializeOwned>(self, max: usize)
        -> Result<T, EdgeError>;

    /// As `json_bounded`, additionally bounded by a caller-supplied
    /// `Deadline`. **The caller-supplied deadline is enforced
    /// cooperatively by `into_bytes_bounded_until`** — that is, at the
    /// yield boundaries enumerated in that helper's rustdoc (entry for
    /// `Body::Once`; before and after each underlying read including EOF
    /// for `Body::Stream`). A read already blocked when `deadline` passes
    /// does **not** get preempted by this helper; it returns when the
    /// underlying source returns, and the next check fires. **Real-time
    /// enforcement is the wrapper's job** — adapters with platform timers
    /// (Axum / CF / Spin) install a deadline-aware stream bounded by
    /// `dispatch_budget(req).deadline` at response construction time
    /// (§3.3.3), so the **request budget** is enforced in real time on
    /// those three; Fastly is `BoundedCooperative` on the request budget
    /// (§3.3.4). The `deadline` argument here only adds the cooperative
    /// post-read tighten; it does not get its own wrapper. Apps that need
    /// timer-backed preemption of a deadline tighter than the request
    /// budget set `.deadline(min(req_deadline, app_inner_deadline))` on
    /// the `OutboundRequest` builder so the tighter deadline lands in the
    /// wrapper. Malformed JSON maps to `bad_gateway` (502).
    pub async fn json_bounded_until<T: DeserializeOwned>(
        self,
        max: usize,
        deadline: Deadline,
    ) -> Result<T, EdgeError>;
    /// Pass the response through as a core `Response` (keeps a streamed body lazy).
    pub fn into_response(self) -> Result<Response, EdgeError>;
}
```

The complete builder surface — `new`/`get`/`post`/`from_request`/`header`/`headers_mut`/
`body`/`json`/`timeout`/`deadline`/`max_response_bytes`/`max_request_body_bytes`/`stream_response`. Every fallible
step returns `EdgeError`, so handler code uses `?` uniformly.

#### 3.1.4 Adapter behaviour contract — redirects and header encoding

These rules apply identically on every adapter so handler code is portable.

**Redirects: not followed automatically.** A 3xx upstream response is delivered to the
app as `Ok(OutboundResponse)` with the 3xx status and the `Location` header preserved.
EdgeZero never silently follows a redirect on the app's behalf. This is a security
property: an app that allowlists `https://trusted.example` and checks `req.uri()` before
sending can never be diverted to `https://attacker.example` by an upstream 302, because
following the redirect requires the app to issue a fresh `OutboundRequest` — at which
point its allowlist runs again. Per-adapter mechanics:

| Adapter | How to disable auto-redirect |
| --- | --- |
| Axum | `reqwest::ClientBuilder::redirect(reqwest::redirect::Policy::none())` |
| Cloudflare | `worker::RequestInit { redirect: "manual", .. }` |
| Spin (WASI) | `spin_sdk::http::send` does not auto-follow — no opt-out needed |
| Fastly | `fastly` does not auto-follow — no opt-out needed |

Apps that want to follow a redirect read `resp.headers().get("location")`, run their
allowlist against the new URI, and issue a new request.

**Header value encoding: UTF-8.** EdgeZero requires every outbound and inbound-of-outbound
header value to be valid UTF-8. Spin/WASI cannot represent non-UTF-8 header values, so
portability mandates this rule everywhere — uniform behaviour beats per-adapter
lossiness for headers that matter.

- *Outbound request headers.* `OutboundRequest::header(..)` constructs the
  `HeaderValue` via `HeaderValue::from_bytes(value.as_ref())`, **not**
  `HeaderValue::from_str` — the latter rejects every byte outside visible ASCII and
  would refuse a perfectly valid non-ASCII UTF-8 header like
  `x-app-display-name: café` before EdgeZero's UTF-8 rule runs. The builder's
  `V: AsRef<[u8]>` bound means `value.as_ref() -> &[u8]` works uniformly for `&str`,
  `String`, `&[u8]`, `Vec<u8>`, `HeaderName`, and `HeaderValue`.
  `HeaderValue::from_bytes` accepts the **HTTP header-value byte set** (visible
  ASCII + obs-text, with control bytes like `\n`/`\0` rejected to prevent header
  injection); EdgeZero then layers its own UTF-8 check via
  `std::str::from_utf8(value.as_ref()).is_ok()`. The accepted set is therefore
  **valid UTF-8 *and* valid HTTP header-value bytes**, not "all valid UTF-8" — an
  HTTP-invalid byte (`\n`, `\0`) inside a UTF-8-valid string still rejects, and
  that's intended security behaviour. Two distinct failure messages:
  `Err(EdgeError::bad_request("header value contains forbidden bytes: <name>"))`
  for the HTTP-validity reject, `Err(EdgeError::bad_request("header value is not
  valid UTF-8: <name>"))` for the UTF-8 reject. Loud and at construction time.
- *Outbound response headers.* If an upstream response carries non-UTF-8 header values,
  **each individual value** is checked (`std::str::from_utf8` on the raw byte slice from
  the platform SDK) — invalid values are dropped, valid sibling values for the same
  header name are preserved. Multi-value headers like `set-cookie` therefore keep
  every valid entry even if one duplicate is invalid. The adapter emits a `log::warn!`
  naming each dropped header. The rest of the response is delivered normally so a
  malformed exotic header cannot poison an otherwise valid fan-out batch response.

*Implementation guardrail.* The UTF-8 check uses `std::str::from_utf8(value.as_bytes())`,
**not** `HeaderValue::to_str()`. `to_str()` is stricter than UTF-8 — it rejects any
byte outside visible ASCII — and would incorrectly drop valid non-ASCII UTF-8 headers
(e.g. an `x-app-display-name: café` style header). Adapters and the core
`normalize_for_dispatch` helper both use `str::from_utf8(value.as_bytes()).is_ok()`.
§5.4 has a test that asserts a valid non-ASCII UTF-8 request and response header survive
round-trip on every adapter, plus one that asserts a header containing a `\x80` byte is
dropped (response) or rejected (request).

Headers that matter for security, tracing, caching, and content negotiation
(`authorization`, `traceparent`, `cookie`, `cache-control`, `accept`, `content-type`,
…) are ASCII-only by spec and are unaffected by this rule. The trade-off only restricts
exotic non-UTF-8 custom headers; apps requiring fidelity for those must not use
EdgeZero outbound for that case.

**Final normalization at dispatch (`outbound::normalize_for_dispatch`).** Two surfaces
bypass the construction-time `header(..)` check — `headers_mut()` exposes raw
`HeaderMap`, and `from_request(..)` carries inbound headers in. Adapters MUST call a
core helper `outbound::normalize_for_dispatch(&mut OutboundRequest)` immediately before
handing the request to the platform SDK. The helper is idempotent and runs the same
rules end-to-end:

1. Drop any header value that is not valid UTF-8 (drop + `log::warn!` naming the
   header) — same lossy semantics as the response side. This applies **only** to
   values that arrived via `headers_mut()` or `from_request(..)` (which carries
   inbound headers verbatim). `OutboundRequest::header(..)` already rejects invalid
   UTF-8 at construction with `bad_request` (§3.1.3), so a non-UTF-8 value can only
   reach this stage by bypassing the checked builder. The policy split is
   deliberate: construction is loud (caller error → 400); proxy-forward and
   pre-validated-map paths are lossy (don't fail an otherwise-good forward over an
   exotic header). The `warn!` makes the drop observable in either case.
2. Strip hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`,
   `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`, plus every
   header named in any `connection` header value). Idempotent for `from_request`
   output; mandatory for manually built requests.
3. Remove `host` — `normalize_for_dispatch` is the single source of truth for stripping
   it from the request; the adapter then sets the final `Host` header (or platform
   SDK equivalent) from `req.host_authority()` at SDK-construction time — the canonical
   accessor (§3.1.4) — and does **not** re-read whatever was in `req.headers()` nor
   reconstruct it from `req.uri()` directly. `from_request` (§3.1.3) also drops `host`
   so the two sites agree end-to-end: the request structure carries no `host` from the
   moment it leaves the core builders; the value on the wire comes from
   `host_authority()`, which itself is derived from the canonicalized URI. One
   accessor, one canonical string, every adapter consumes the same value.
4. Remove `content-length` — the adapter sets it from the body (length for
   `Body::Once`; omitted for `Body::Stream`).
5. Remove `transfer-encoding` — the adapter sets it per body type and HTTP version.

Apps can therefore use `headers_mut()` and `from_request` freely; portability and
framing safety are guaranteed by this final sweep, not by individual callers
remembering to sanitize.

**Multi-value headers preserved.** `HeaderMap` permits repeated names — `set-cookie`,
`warning`, custom tracing headers, etc. EdgeZero adapters MUST preserve every entry for
a repeated header on both request and response: use `HeaderMap::append` (never
`insert`) when building, and read with `get_all` (never `get`) when serializing to the
platform SDK or deserializing platform responses. Per-adapter mechanics (the spots
current code uses single-value APIs that collapse):

| Adapter | Request side (build platform request) | Response side (read platform response) |
| --- | --- | --- |
| Axum | `reqwest::RequestBuilder::header` (calls `HeaderMap::append`) | iterate `reqwest::Response::headers()` which is already a `HeaderMap` — preserve as-is |
| Cloudflare | `worker::Headers::append(name, value)` — **not** `set` (collapses) | iterate `worker::Headers` entries; `set-cookie` is enumerated separately by the worker runtime, handled explicitly |
| Fastly | `fastly::Request::append_header(name, value)` — **not** `set_header` | `fastly::Response::get_header_all(name)` per name, **not** `get_header` (returns first only) |
| Spin | `spin_sdk::http::Headers::append` — uses WASI HTTP `fields` which natively support multi-value | iterate WASI `fields` per name |

Contract tests in §5.4 exercise repeated `set-cookie` response headers and repeated
outbound request headers, so any regression to collapsing duplicates is caught at CI
time. If a future SDK update breaks multi-value round-tripping on one adapter, the
spec downgrades the contract for that adapter and documents the limitation rather than
silently dropping headers.

### 3.2 Concurrent fan-out

`HttpClient::send_all` is the single concurrency API. It is truly concurrent on all four
platforms, and its **input/output contract** is identical (preflight, index alignment,
per-slot Ok/Err shape). Cross-slot timing **is not uniform** — see the
`send-all-slot-isolation` capability and §3.3.4 for Fastly's buffered-body
harvest-order caveat. App code never calls `futures::future::join_all`.

| Adapter | `send_all` mechanism | Concurrency source |
| --- | --- | --- |
| Axum | `futures::future::join_all` of per-request `reqwest` sends | tokio reactor |
| Cloudflare | `futures::future::join_all` of `worker::Fetch` sends | Workers JS event loop |
| Spin | `futures::future::join_all` of `spin_sdk::http::send` | wasi async reactor |
| Fastly | dispatch every request with `send_async`, **then** harvest | Fastly host (parallel) |

**Why a batch API and not `join_all` in app code.** Axum/Cloudflare/Spin have an async
reactor, so `join_all` of independent futures fans out. Fastly Compute has no guest
reactor: a future wrapping Fastly's poll-based `PendingRequest` would return `Pending`
with no waker, and `block_on` would deadlock. Fastly fan-out therefore *must* be
structured as "dispatch all, then harvest" — a shape that cannot be decomposed into N
independent futures. Making `send_all` the one primitive hides this entirely.

**Where "identical" stops being identical: Fastly buffered body drain.** Adapter
contracts for the *headers* phase are identical across all four. The body-drain
phase is not: Fastly's buffered-body drain runs in harvest order rather than
concurrently with sibling drains (§3.3.4 "Buffered body drain runs in harvest
order"). For small bodies (fan-out batches, JSON) the wall-clock difference is negligible;
for large bodies on Fastly, EdgeZero has no API that delivers concurrent large-body
fan-out — `Streamed` mode defers drain but does not let the app consume chunks
concurrently across slots either (no guest reactor; §3.2). This is a known
limitation, not a recommendation.

**Partial failure.** `send_all` returns `Vec<Result<OutboundResponse, EdgeError>>`
index-aligned with the input. A single target timing out or returning a 502 yields
`out[i] = Err(..)` or `out[i] = Ok(non-2xx)` without changing the *type* of any
other slot's result. Cross-slot **timing** is governed by `send-all-slot-isolation`
(§3.5.1 footnote 4): `Native` on Axum/CF/Spin, `BestEffort` on Fastly because
serial harvest-order body drain can cause a slot to return `gateway_timeout` even
when its own budget would have covered it (§3.3.4). Apps that need the stricter
timing guarantee declare the capability required and get a hard build failure on
Fastly.

### 3.3 Portable deadline

#### 3.3.1 `Deadline` — portable value type, in core

```rust
// crates/edgezero-core/src/time.rs  (new module)

/// An absolute monotonic instant after which work should stop. A pure value type
/// — arithmetic over `web_time::Instant`, identical on every target, with no
/// runtime dependency. `time.rs` contains `Deadline`, `DispatchBudget`,
/// `dispatch_budget`, and the public timing constants (§7); the deliberate
/// constraint per §3.3.5 is that core carries **no runtime / timer / platform
/// dependency** — none of those types reaches outside the value-level
/// arithmetic and the trait surface adapters implement.
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    at: web_time::Instant,
}

impl Deadline {
    /// `now + min(d, DEADLINE_FAR_FUTURE)`, where `DEADLINE_FAR_FUTURE` is a
    /// **defined constant** clamp (7 days, see below). Bounded far-future clamping,
    /// not "saturate to whatever Instant::MAX happens to be" — `std::time::Instant`
    /// has no `MAX` and platform overflow behaviour differs. The clamp is
    /// finite and well above any realistic fan-out batch/proxy budget, so this never
    /// truncates a legitimate caller and never panics. Adapter boundaries must
    /// not crash the host.
    pub fn after(d: Duration) -> Self;
    pub fn at_instant(instant: web_time::Instant) -> Self;  // construct from absolute instant
    pub fn instant(&self) -> web_time::Instant;    // accessor for the absolute instant
    pub fn remaining(&self) -> Option<Duration>;   // None once passed
    pub fn is_expired(&self) -> bool;
}

/// Hard upper bound on any caller-supplied duration. The clamp exists so
/// `Deadline::after` and `dispatch_budget` cannot panic on a pathological
/// `Duration::MAX` input. Set to **7 days** rather than something larger so the
/// ceiling fits inside every supported platform's per-request timeout range — in
/// particular Fastly's backend timeouts are `u32` milliseconds (≈ 49.7 days max
/// per Fastly 0.12.1), so the EdgeZero clamp must stay well below that. 7 days
/// is still orders of magnitude above any realistic outbound budget; nobody hits
/// it legitimately.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
```

#### 3.3.2 Mapping an external batch deadline to EdgeZero deadlines

| External concept | EdgeZero mechanism |
| --- | --- |
| External batch deadline (whole fan-out) | Compute `let batch_deadline = Deadline::after(Duration::from_millis(batch_deadline_ms))` **once** at handler entry, then pass that absolute value into every target request via `.deadline(batch_deadline)`. `Deadline` is `Copy` and absolute, so all targets share the same wall-clock cap. Do **not** call `Deadline::after(..)` per target — that re-anchors `now` per call and lets later targets drift past the batch deadline. |
| Per-target request timeout | `OutboundRequest::timeout(per_target)` |
| Effective per-request budget | computed by `dispatch_budget` — see below |

**Effective budget rule (`dispatch_budget(req)`).** Returns a `DispatchBudget` struct
carrying **both** the duration to feed to platform SDK timeouts AND the absolute
`Deadline` to use for cooperative body-phase `is_expired()` checks. The implementation
computes a single set of candidate **absolute** deadlines from one monotonic `now`
snapshot and takes the minimum — so the effective deadline can never extend an
original `req.deadline`, and "no deadline" never gets conflated with "expired
deadline" via an `Option<Duration>` round-trip.

```rust
pub struct DispatchBudget {
    pub duration: Duration,    // SDK timeout setting
    pub deadline: Deadline,    // effective absolute deadline
}

/// `now` is passed in (not snapshotted internally) so a single `send_all` can use
/// **one** `now` snapshot across every slot. Without that, sequential per-slot
/// `Instant::now()` calls produce slightly different `duration` values for the same
/// shared `Deadline`, which on Fastly would produce different `budget_ms` values
/// and therefore different dynamic-backend identities for the same host under one
/// batch deadline (§4.3). `send` (single request) just passes
/// `web_time::Instant::now()`.
pub fn dispatch_budget(
    req: &OutboundRequest,
    now: web_time::Instant,
) -> Result<DispatchBudget, EdgeError> {
    // (1) Expired-deadline check using the *single* now snapshot — no remaining()
    //     round-trip that could lose the distinction between "no deadline" and
    //     "deadline expired" (both produce None from remaining()).
    if let Some(dl) = req.deadline {
        if dl.instant() <= now {
            return Err(EdgeError::gateway_timeout("deadline expired before dispatch"));
        }
    }

    // (2) Candidate absolute deadlines. Use checked_add throughout — a caller-
    //     supplied Duration::MAX must not panic the adapter. The same clamp as
    //     Deadline::after (§3.3.1): cap the duration at DEADLINE_FAR_FUTURE
    //     *before* the add, so the addition itself never overflows in practice
    //     (now + 7 days is well within Instant range). checked_add on the
    //     clamped value is belt-and-suspenders.
    let saturating = |dur: Duration| -> Deadline {
        let clamped = dur.min(DEADLINE_FAR_FUTURE);
        let inst = now.checked_add(clamped).unwrap_or(now);   // last-resort: now (immediate)
        Deadline::at_instant(inst)
    };
    let from_timeout      = req.timeout.map(&saturating);
    // `Deadline::at_instant` is public (§3.3.1), so a caller could construct a
    // Deadline well past DEADLINE_FAR_FUTURE and bypass Deadline::after's clamp.
    // Re-clamp `from_caller` here: the caller's deadline is never honoured beyond
    // `now + DEADLINE_FAR_FUTURE`. This only tightens; a caller's deadline closer
    // than that is unaffected.
    let from_caller       = req.deadline.map(|d| {
        let far = now.checked_add(DEADLINE_FAR_FUTURE).unwrap_or(now);
        Deadline::at_instant(d.instant().min(far))
    });
    let from_default_only =
        (req.timeout.is_none() && req.deadline.is_none())
            .then(|| saturating(DEFAULT_NO_DEADLINE_BUDGET));

    // (3) Effective deadline = min of the candidates (always at least one).
    let deadline = [from_timeout, from_caller, from_default_only]
        .into_iter()
        .flatten()
        .min_by_key(|d| d.instant())
        .expect("at least one candidate by construction");

    // (4) Duration is derived from the chosen deadline and the same now snapshot
    //     — never `Deadline::after(duration)`, which would re-anchor to a *later*
    //     now and could extend the absolute deadline past the caller's intent.
    let duration = deadline.instant().saturating_duration_since(now);
    if duration.is_zero() {
        return Err(EdgeError::gateway_timeout("effective budget is zero"));
    }

    Ok(DispatchBudget { duration, deadline })
}
```

Behaviour table (the implementation gives these directly; listed here for clarity):

All `now + t` entries in this table are shorthand for `now + min(t,
DEADLINE_FAR_FUTURE)` (§3.3.1) — the clamp is universal, not a special case for
`Duration::MAX`.

Below, `clamped(d)` denotes `Deadline::at_instant(d.instant().min(now +
DEADLINE_FAR_FUTURE))` — the re-clamp of a caller's `req.deadline` performed by
`dispatch_budget` so a `Deadline::at_instant` constructed past the 7-day clamp
cannot escape the bound (§3.3.2 step 2 / round 16). For brevity the table writes
`clamped(d)` rather than the full expression.

| `req.timeout` | `req.deadline` | `duration` | `deadline` (absolute) |
| --- | --- | --- | --- |
| `None` | `None` | `30 s` | `now + 30 s` |
| `Some(t)` | `None` | `min(t, DEADLINE_FAR_FUTURE)` | `now + min(t, DEADLINE_FAR_FUTURE)` |
| `None` | `Some(d)` | `clamped(d).instant() - now` | `clamped(d)` |
| `Some(t)` | `Some(d)` with `now + min(t, …) < clamped(d).instant()` | `min(t, …)` | `now + min(t, …)` (tighter) |
| `Some(t)` | `Some(d)` with `now + min(t, …) ≥ clamped(d).instant()` | `clamped(d).instant() - now` | `clamped(d)` (tighter) |
| any | expired (`d.instant() <= now`) | — | `Err(gateway_timeout)` |
| any | duration ends up zero | — | `Err(gateway_timeout)` |
| `Some(Duration::MAX)` | `None` | `DEADLINE_FAR_FUTURE` (7 d) | `now + DEADLINE_FAR_FUTURE` |
| `None` | `Some(d)` 100 years out via `at_instant` | `DEADLINE_FAR_FUTURE` (7 d) | `now + DEADLINE_FAR_FUTURE` |

`.timeout(50ms)` with no batch deadline therefore yields `duration = 50ms` and
`deadline = now + 50ms`, **not** 30 s. The single absolute `deadline` is what Fastly's
between-chunk checks (§3.3.4) and the streamed-body wrappers in §4.1/§4.2/§4.4 use, so
per-request `timeout` is honoured across the entire exchange — including the streamed
body phase — whether or not an batch deadline was provided.

"No deadline configured" therefore differs from "deadline configured and expired" —
the former is bounded by the synthetic 30 s ceiling; the latter is a hard fail at
dispatch with `gateway_timeout`.

The same rule governs the dispatch+headers phase in `Streamed` mode. The body phase is
**also** governed by `dispatch_budget(req).deadline` (see §3.3.3) — the spec
deliberately does
not split the deadline into "before headers" and "after headers" pieces.

#### 3.3.3 What the deadline covers

The deadline on `OutboundRequest` covers the **entire exchange end-to-end** in both
modes. The mechanism differs:

- **`Buffered` (default):** the adapter buffers the body *inside* the deadline-bounded
  region, so a slow body counts against the budget. `Ok(resp)` from `send`/`send_all`
  means the full exchange completed within the deadline.
- **`Streamed`:** `Ok(resp)` is returned once headers arrive — earliest possible
  delivery — but the **body stream returned in `resp` is adapter-wrapped to honour
  `dispatch_budget(req).deadline`.** That deadline is the *effective* one computed by
  the budget rule (§3.3.2), which is non-`None` even for timeout-only and no-deadline
  requests — adapters wrap the body stream in every case, not only when
  `req.deadline.is_some()`. Axum/CF/Spin wrap with a platform-timer-aware stream
  (real preemption per chunk); Fastly is bounded-cooperative per §3.3.4. So a stalled
  upstream cannot exceed the effective budget silently in either mode.

What this means in practice:

- `OutboundResponse::into_bytes_bounded(max)` on a streamed body already honours the
  effective-budget deadline through the wrapped stream — body chunks past the
  deadline yield `gateway_timeout`.
- `OutboundResponse::into_bytes_bounded_until(max, deadline)` is for tightening the
  bound below the effective-budget deadline (e.g. an inner budget for body-only) —
  not for re-applying the same deadline, which is automatic.
- If the caller dropped the `Deadline` value but still wants the same effective
  ceiling, passing `Deadline::after(remaining_budget_from_some_source)` works; or
  just call `into_bytes_bounded` and trust the wrapped stream.

This is one contract for everyone: handlers never have to remember "Streamed cuts the
deadline at headers." Adapter notes (§4.1–§4.4) implement this end-to-end.

#### 3.3.4 Per-adapter enforcement (`Buffered` mode)

| Adapter | Mechanism | Strength |
| --- | --- | --- |
| Axum | `reqwest::RequestBuilder::timeout(effective)` — reqwest applies it through response-body read | Real, whole-operation |
| Cloudflare | race the entire `send_one` future (fetch **and** body drain) against `worker::Delay(effective)`; drop on expiry | Real, whole-operation |
| Spin | race the entire `send_one` future (send **and** body collect) against a wasi monotonic-clock timer; drop on expiry | Real, whole-operation |
| Fastly | host phase timers split per §4.3 (`connect = budget/4`, `first_byte = 3*budget/4`, `between_bytes = budget`); during body drain, `budget.deadline.is_expired()` is checked **after every blocking body read returns, including the EOF read** (the synthetic 30 s deadline applies when no caller deadline was set); the host between-bytes timeout bounds each gap | Real for connect+headers with a documented phase split (see §4.3 — a connect that itself takes longer than `budget/4` fails even if the rest of the budget would have sufficed); **bounded-cooperative** for the body phase |

**Fastly precision, stated honestly.** Fastly has no guest wall-clock primitive to
preempt a chunk read in progress. At dispatch the adapter computes `let budget =
dispatch_budget(req, now)?` (§3.3.2, `now` snapshotted inline for single `send`,
passed in as `batch_now` for `send_all` — round 23. `DEFAULT_NO_DEADLINE_BUDGET = 30 s`
and the synthetic absolute deadline both apply when no deadline is set, identical to
every other adapter) and derives the host timeouts via the named helper:

```rust
fn fastly_timeout_ms(budget: &DispatchBudget) -> u64 {
    // True ceil-to-ms — never floor a sub-ms remainder away (round 20).
    // The DEADLINE_FAR_FUTURE clamp keeps this below Fastly's 2^32 ms ceiling
    // (round 24); we still assert it explicitly because a bug elsewhere
    // shouldn't crash the host.
    let ms = ((budget.duration.as_nanos() + 999_999) / 1_000_000).max(1);
    debug_assert!(ms < (u32::MAX as u128), "fastly_timeout_ms exceeds u32::MAX ms");
    ms.min(u32::MAX as u128 - 1) as u64
}

// `dispatch_budget` always takes an explicit `now` (round 23). Single `send`
// snapshots inline; `send_all` snapshots once into `batch_now` and reuses it
// across slots so the dynamic-backend identity stays consistent for a shared
// caller Deadline.
let now = web_time::Instant::now();             // single `send`; `send_all` passes batch_now
let budget = dispatch_budget(req, now)?;

// Fastly 0.12.1 exposes the timeout setters on BackendBuilder, NOT on Request — see
// https://docs.rs/fastly/0.12.1/fastly/backend/struct.BackendBuilder.html.
// IMPORTANT: connect_timeout and first_byte_timeout are *separate* phase timers
// per Fastly's docs — connect bounds DNS+TCP+TLS setup; first_byte bounds the gap
// from "request sent" until headers are received. Setting both to the same `t`
// would make the dispatch+headers worst case ~2*t, breaking the absolute-deadline
// bound. We therefore SPLIT the budget across the two phases (and the third,
// between-bytes, which only applies once chunks are flowing during body drain),
// keeping the sum exactly equal to total_ms:
//   total_ms      = ceil-to-ms(budget.duration)
//   connect_ms    = total_ms / 4              [floor; most connects take <100ms]
//   first_byte_ms = total_ms - connect_ms     [remainder; sum invariant]
//   between_ms    = total_ms                  [body-phase ceiling unchanged]
// Sub-4 ms degenerate case: both = total_ms (sum = 2*total_ms, documented).
// SSL configuration also lives on BackendBuilder: `use_ssl` defaults to false, so
// HTTPS targets MUST opt in explicitly with .enable_ssl() and configure SNI +
// certificate verification (per the existing pattern at
// crates/edgezero-adapter-fastly/src/proxy.rs:120). HTTP targets opt out via
// .disable_ssl().
//
// Four canonicalized values come from the OutboundRequest accessors (§3.1.4 —
// adapters MUST consume these, never re-derive from `req.uri()`):
//   - `req.backend_target()`         — connection target `"host:port"` with the
//                                       resolved port; passed as the
//                                       BackendBuilder's `target` arg.
//                                       (current adapter precedent:
//                                       `host_with_port` at
//                                       crates/edgezero-adapter-fastly/src/proxy.rs:108)
//   - `req.host_authority()`         — authority for `.override_host(..)`
//                                       (carries the explicit port only when
//                                       non-default; preserves §3.1.3 Host
//                                       semantics).
//   - `req.sni_hostname()` — `Option<&str>`. `Some(host)` for DNS-name HTTPS
//                            targets; `None` for IP-literal HTTPS (RFC 6066 §3
//                            forbids SNI for IP literals). When `None`, the
//                            adapter omits `.sni_hostname(..)` entirely; it
//                            does NOT fall back to `req.uri().host()`.
//   - `req.cert_host()`    — `Option<&str>`. `Some(host)` for any HTTPS target
//                            (DNS name OR IP literal — port-stripped,
//                            bracket-stripped); `None` for non-HTTPS schemes.
//                            Passed to `.check_certificate(..)` verbatim; the
//                            adapter does NOT bracket-trim, parse, or
//                            post-process.
// Phase split. The documented semantics: connect gets a *floor quarter* of the
// already-ceiled total; first_byte gets the remainder; between_bytes gets the full
// budget. Invariant we want: connect_ms + first_byte_ms == total_ms exactly, so
// the worst-case dispatch+headers wall-clock is bounded by `budget.duration`
// (modulo ms rounding). Using `total_ms / 4` (floor) keeps the sum exact; the
// earlier "ceil-to-ms of budget * 1/4" framing was a misnomer — that would have
// made the sum exceed total_ms by up to 1 ms for some inputs. For tiny budgets
// where the 1/4 share would round to 0, we degenerate to "both = total_ms" —
// the absolute-deadline bound becomes 2*total_ms but at sub-4 ms scale this is
// negligible (and the ceil-to-ms rounding already dominates).
let total_ms = fastly_timeout_ms(&budget);                 // ceil-to-ms of budget.duration
let (connect_ms, first_byte_ms) = if total_ms < 4 {
    (total_ms, total_ms)                                   // sum = 2*total_ms; documented
} else {
    let connect    = total_ms / 4;                         // floor — keeps sum exact
    let first_byte = total_ms - connect;                   // sum = total_ms exactly
    (connect, first_byte)
};
let between_ms = total_ms;
let mut builder = Backend::builder(&backend_name, &req.backend_target())
    .connect_timeout(Duration::from_millis(connect_ms))
    .first_byte_timeout(Duration::from_millis(first_byte_ms))
    .between_bytes_timeout(Duration::from_millis(between_ms))
    .override_host(req.host_authority());
// TLS handling — the §3.1.4 accessors carry the canonicalized split. We do NOT
// inspect `req.uri()` directly: `cert_host()` returns `Some` iff the scheme is
// HTTPS (the adapter-local "is TLS?" question), and `sni_hostname()` carries
// the DNS-vs-IP-literal distinction (`None` for IP literals per RFC 6066 §3).
builder = match req.cert_host() {
    Some(cert) => {
        // HTTPS: always set .check_certificate(..). Pass req.cert_host()
        // through unmodified — bracket-stripping for IPv6 is already done in
        // the accessor; we never call .trim_start_matches('[').
        let mut b = builder.enable_ssl().check_certificate(cert);
        // SNI: only when the accessor returns Some (DNS-name host).
        // For IP literals (`None`), .sni_hostname() is omitted entirely.
        if let Some(sni) = req.sni_hostname() {
            b = b.sni_hostname(sni);
        }
        b
    }
    None => builder.disable_ssl(),    // HTTP
};
let backend = builder.finish()?;
// Fastly's Request public API has no `with_backend`. The backend is passed as
// the argument to `send` / `send_async` / `send_async_streaming` at send time
// (each accepts `impl ToBackend`). `Backend` implements `ToBackend`.
// Buffered request body (send_all only — preflight rejected streams):
let pending = fastly_req.send_async(&backend)?;
// Streamed request body (single `send` only):
// let (streaming_body, pending) = fastly_req.send_async_streaming(&backend)?;
```

The dynamic-backend identity tuple (§4.3) is `scheme + ":" + host + ":" +
resolved_port + ":" + tls_mode + ":" + budget_ms`, where `tls_mode` is derived from
`req.uri().scheme_str()` and `budget_ms = ceil-to-ms(budget.duration)` — the same
`total_ms` that drives the `connect_ms / first_byte_ms / between_ms` deterministic
phase split above. The cached `Backend` and a freshly-requested one therefore always
carry identical timeouts AND identical SSL configuration because both are
deterministic functions of the same tuple. Existing in-tree precedent for
the SSL setters lives at `crates/edgezero-adapter-fastly/src/proxy.rs:120`; the
migration generalises that pattern to every dynamic backend. The budget is set once
before `send_async` and not mutated afterwards — the Fastly SDK does not expose
dynamic per-chunk timeout updates. During body drain the adapter checks
`budget.deadline.is_expired()` **after every blocking body read returns, including
the EOF read** (per the §3.3.4 rule — the earlier "between chunks" wording was
incomplete because a final EOF read can itself cross the deadline). Because
`dispatch_budget` always returns a concrete `Deadline` (synthetic if the request
had none), this cooperative check works uniformly whether or not the caller
supplied a deadline.
`connect-timeout` and `first-byte-timeout` together bound the dispatch+headers phase
at `budget.duration` (their sum, by the §4.3 split) **when `total_ms ≥ 4`**; for
`total_ms < 4` the code degenerates to `connect = first_byte = total_ms` and the
sum is `2 * total_ms`. The absolute-deadline guarantee in the sub-4 ms branch is
therefore "≤ `total_ms + BATCH_DISPATCH_SLACK_MAX + ms_rounding` past deadline"
(strict upper bound: `BATCH_DISPATCH_SLACK_MAX + total_ms + ms_rounding`
which is `25 + (≤ 3) + (≤ 1) < 29` ms), not the common-case "≤ 26 ms" — see
the two explicit
branches in §4.3 "Net guarantee." Sub-4 ms outbound budgets are degenerate inputs
where ms-rounding already dominates, not a normal operating point. The documented trade-off (§4.3) is that a request
spending more than `budget/4` on connect-phase work (DNS+TCP+TLS) fails at the
connect timer even if the remaining budget would have sufficed for headers; that
is captured by the separate `outbound-flexible-phase-budget` capability (§3.5.1).
During body drain (post-`wait()`), the adapter checks `budget.deadline.is_expired()`
**after every blocking body read returns, including the EOF read** (not "between
chunks" — the EOF read can itself block past the deadline and would otherwise
slip through with `Ok(resp)`). On the first expired check the slot is aborted
with `gateway_timeout`; each individual chunk-gap (including the gap before EOF)
is bounded by the host `between-bytes-timeout`. So the Buffered `Ok(resp)`
contract — "headers AND body completed within the deadline" — holds end-to-end:
either every read (including EOF) observed `!is_expired()`, or the slot returned
`gateway_timeout`.

**Slot-level vs. wall-clock-observed completion.** The bound above is on
**host-side** enforcement per slot: the Fastly host stops each request when its own
configured timeouts elapse. The host runs all dispatched requests in parallel, so
fast-budget slots complete (success or host-timeout) at host-time independent of how
long the guest blocks on earlier slots' `wait()`. What the guest **observes**, though,
is gated by harvest order — a slot with a 50 ms effective budget sitting behind a
3 s `wait()` on slot 0 has already completed at the host (either successfully or as a
host-timeout error) at t ≈ 50 ms, but the guest does not see the result until slot 0's
`wait()` returns. So:

- **Per-slot result correctness (headers phase):** each slot's connect / first-byte /
  between-bytes timeouts are configured from its own `budget.duration`, and the host
  enforces them independently. A 50 ms slot that fails to receive headers in time
  errors at 50 ms host-side, not 3 s — the headers phase is genuinely per-slot.
  *This holds only for the headers phase.* Buffered body drain in `send_all` is
  bounded by the same host timeouts on a per-chunk-gap basis but is **scheduled
  sequentially in harvest order** — see the next bullet for the wall-clock
  consequence.
- **Per-slot wall-clock-observed delivery:** bounded by
  `max_over_remaining_slots(effective_at_dispatch)` in the worst case (harvest-order
  delay). When all slots in one fan-out batch share the same effective
  deadline the bounds coincide; in heterogeneous-budget scenarios
  apps should be aware that observed completion can be later than per-slot
  completion. The opportunistic `poll()` of later slots after each `wait()`
  (Phase 2 above) reduces this gap in practice but does not eliminate it.
- **Buffered body drain runs in harvest order, not concurrently.** `harvest()` does
  `pending.wait()` *and then* drains the response body (Buffered mode) *and then*
  moves to the next slot. On Axum/CF/Spin `join_all` polls all `send_one` futures
  concurrently, so two slow body drains complete in parallel; on Fastly they are
  sequential. Wall-clock for the entire `send_all` is therefore
  `max(header_arrivals) + Σ buffered_body_drain_times` on Fastly versus
  `max(header_arrivals + buffered_body_drain_times)` elsewhere. **A slot can therefore
  return `gateway_timeout` even though its host-side headers + body would have
  completed within `budget.deadline` in isolation** — its body-drain phase started
  late because an earlier slot's drain monopolised harvest, and the inter-chunk
  `is_expired()` check fires once `budget.deadline` is crossed. The
  "per-slot result correctness" bullet above applies only to the *headers* phase;
  for the body phase, results genuinely depend on harvest order. The `send_all`
  contract on Fastly therefore *admits* harvest-order-induced 504s in Buffered mode,
  and the §5.4 test row asserts this explicitly. Concrete contract:
  - For typical small JSON bodies (fan-out batches, the external batch protocol, sub-100 KiB responses) the
    drain times are on the order of a few hostcalls (≤ low single-digit ms) and the
    summed term is well within any realistic fan-out batch deadline.
  - For large body responses, Fastly `send_all` is **simply suboptimal** compared
    to the other three adapters and there is no current EdgeZero API that recovers
    parallel large-body fan-out on Fastly. `Streamed` mode defers each slot's drain
    to the consumer, but the consumer has no concurrent body-drain primitive
    either — Fastly's body reads are synchronous host calls with no guest reactor
    (§3.2 / §3.3.5), so iterating `Stream::next` on `out[0].body()` and
    `out[1].body()` still serializes at the guest. Apps that fan out to large-body
    upstreams on Fastly should either (a) target a different adapter for that
    workload, (b) issue requests in a topology that doesn't require parallel
    large-body drains, or (c) wait for the interleaved-drain follow-up in §8 risk 8.
    typical small-body fan-outs are unaffected (response bodies under a few KiB).

The worst-case post-deadline overshoot per slot **once that slot is actively draining**
is therefore **one between-bytes-timeout interval, which is ≤ `effective_at_dispatch`**.
Note: that bound is on the host timeout set at dispatch and does *not* shrink while a
slot waits behind earlier harvest work. **Total wall-clock observed by the caller**
is *not* bounded by one between-bytes-timeout — it also includes the harvest delay
described above: the sum of preceding slots' drain times before this slot's drain
phase begins. Concretely, in a Buffered-mode `send_all` of N homogeneous-budget slots
on Fastly with sequential body drains, slot `k`'s observed completion can be as late
as `Σᵢ<ₖ drain_timeᵢ + (effective_at_dispatch for slot k)` — and once slot `k`'s drain
*begins*, the inter-chunk `is_expired()` check fires within one between-bytes-timeout
of `budget.deadline` for that slot.

Apps reasoning about precise wall-clock should treat `effective_at_dispatch` as the
maximum per-slot *active-drain* overshoot — i.e., the original batch budget is the
bound on each slot's drain phase **in isolation**, not the bound on its observed
completion time across the whole `send_all`. The `send-all-slot-isolation` capability
(§3.5.1 footnote 4) is what scopes the cross-slot half: declaring it required gives
the hard build failure on Fastly, signalling that an app needs isolation guarantees
the harvest order does not provide. This is what `BoundedCooperative` means at the
single-slot level (§3.5.1); the cross-slot harvest-order weakening is the separate
`BestEffort` `send-all-slot-isolation` story. A peer dribbling bytes still cannot
blow past the batch deadline indefinitely *on its own slot*, but a fan-out batch observing total
wall-clock should also account for harvest serialization.

#### 3.3.5 No general-purpose timeout combinator (deliberate)

An earlier draft put a `timeout(deadline, future)` combinator for *arbitrary* futures in
`edgezero-core`. That is **removed**: a real timer future needs a platform runtime
(`tokio` / `worker` / `spin-sdk`), which core may not depend on (§1.3). Core therefore
ships only the `Deadline` value type; outbound-deadline enforcement lives entirely inside
adapters (§3.3.4). A general arbitrary-future timeout would require an adapter-injected
`Timer` trait and a dedicated capability; it is **out of scope** here because the fan-out pattern's
timing needs are fully met by the outbound path. Noted as possible future work.

### 3.4 Bounded buffering & error mapping

#### 3.4.1 Outbound responses

In `Buffered` mode, `max_response_bytes` (default `DEFAULT_MAX_RESPONSE_BYTES = 1 MiB`)
caps the body. The cap is measured in **decompressed, app-visible bytes**, not
compressed wire bytes. Every adapter that transparently decompresses gzip/br
**must enforce the cap incrementally during decompression** and abort as soon as the
decompressed output exceeds the cap — this closes the decompression-bomb gap so a
small compressed body cannot expand past the limit. Over-cap →
`Err(EdgeError::bad_gateway("response body exceeded N bytes"))`.

**Pre-append check is mandatory.** Both inbound (`RequestContext::body_bytes`) and
outbound (`OutboundResponse::into_bytes_bounded` / `_until`) bounded drains MUST check
`collected.len().checked_add(chunk.len()).map_or(true, |n| n > max)` (equivalently
`chunk.len() > max.saturating_sub(collected.len())`) **before** extending the buffer
— never extend
then check. A single oversized chunk on a small cap would otherwise allocate past the
limit before erroring. The existing `Body::into_bytes_bounded` helper at
`crates/edgezero-core/src/body.rs:84` extends then checks; the migration updates it
to pre-append checked length accounting. Both helpers therefore guarantee that the
**persistent collected buffer** is bounded by `max` — pre-append checking aborts before
ever extending past `max`.

Worst-case **transient** resident memory during a drain is `max + sizeof(current_chunk)`:
the in-flight chunk briefly co-exists with the collected buffer during the check, then
is dropped (over-cap) or appended (under-cap). **`sizeof(current_chunk)` is
source-controlled, not bounded by this spec.** The `8–64 KiB` figure typical sources
yield (`tokio::io` 8 KiB, `hyper` 16 KiB, WASI body reads 64 KiB) is descriptive of the
adapters' incoming stream chunking, not a contract. Three concrete consequences readers
must internalise:

- **An upstream that yields one large `Bytes` exceeds the typical figure.** A peer
  returning a 4 MiB response in a single chunk produces a single 4 MiB in-flight
  `Bytes` while the over-cap check runs; if the cap is 1 MiB, the persistent buffer
  never grows past 1 MiB but resident memory transiently includes the full 4 MiB
  chunk. The check still aborts before any append, but the host did receive 4 MiB.
- **The spec does not rechunk.** EdgeZero's `Body::Stream` forwards chunks verbatim;
  there is no `chunk_size_cap` configuration knob on `OutboundRequest`/`OutboundResponse`.
  Adding one would require either every adapter to rechunk on the inbound side (a
  non-trivial perf cost) or a core wrapper around every adapter-emitted stream (which
  defeats lazy passthrough on CF/Fastly/Spin). **Deferred** — tracked in §8 risk 11.
- **The batch model in §3.4.4 inherits the same property.** `Σⱼ sizeof(current_chunkⱼ)`
  for actively-draining slots is bounded by what each source yields, not by EdgeZero.
  Apps that need a hard per-batch ceiling against adversarial chunking must either
  size the request fan-out (N) conservatively against the **upstream's** advertised
  maximum chunk size, or wait for the §8 risk 11 follow-up.

This is a per-call drain bound, **not** a whole-process memory ceiling; the batch-level
bound is `Σ persistent buffers + Σ in-flight chunks` per §3.4.4, with the same
source-controlled caveat on the in-flight term.

Decompression-cap responsibility per adapter:

- **Cloudflare, Fastly, Spin** — already decompress gzip/br explicitly today; the cap
  obligation applies in-line in their existing decode paths.
- **Axum** — the workspace `reqwest` dependency is currently
  `default-features = false` and does not enable gzip/brotli decoding. This migration
  enables the `gzip` and `brotli` features on `reqwest` so behaviour matches the other
  three adapters; reqwest then performs decoding and the byte cap is enforced
  incrementally while the adapter drains the response. The Cargo.toml change is part of
  the file-by-file summary (§7).

Whenever an adapter decompresses, the `OutboundResponse.headers` it returns MUST have
both `content-encoding` and `content-length` removed — the original values describe
compressed wire bytes and no longer match the app-visible body. This applies in both
`Buffered` and `Streamed` modes: callers must never see decoded bytes alongside stale
compressed metadata. Existing Cloudflare and Fastly proxy code already does this and
the contract codifies it.

**Streaming-decompressor design (Streamed mode).** Lazy
`lazy-streamed-response-passthrough` on CF/Fastly/Spin coexists with the cap
obligation because each adapter wraps the raw compressed byte stream with a
**streaming decoder** that emits decompressed chunks as they arrive, never buffering
the full body. The decoder's *only* responsibilities are decoding bytes, stripping
the two compressed-only headers, and surfacing decoder errors — it deliberately does
**not** enforce a byte cap, because `ResponseMode::Streamed` carries no `max_bytes`
(§3.1.3) and the cap lives with the consumer:

1. Pull a raw compressed chunk from the platform stream.
2. Feed it into the decoder; emit whatever decompressed output is currently available
   (zero, one, or many output chunks per input chunk).
3. Yield each decompressed chunk verbatim. **No byte counting in the wrapper.**
4. Stop on raw EOF, decoder error (→ `Err(EdgeError::bad_gateway(..))` chunk).
5. `content-encoding` and `content-length` are stripped from
   `OutboundResponse.headers` at construction time — the wrapper's output bytes are
   the new ground truth.

Cap ownership is then unambiguous:

- **Buffered mode:** the adapter drains the decompressed stream inside the
  buffered-drain helper with `max_response_bytes` (per-append-checked, §3.4.1).
  Cap fires inside the adapter.
- **Streamed mode + `into_bytes_bounded(max)` / `into_bytes_bounded_until(max,
  deadline)`:** the helper's own pre-append check enforces `max` against the
  decompressed chunks it pulls from the wrapped stream. Cap fires in the helper.
- **Streamed mode + `into_response()` passthrough (proxy-forward):** there is
  **deliberately no EdgeZero cap** — the platform's downstream response wire is
  the budget, and inserting an EdgeZero cap on a transparent proxy stream would
  silently truncate a perfectly valid streamed proxy response. Apps that want to
  cap proxied bodies do `into_bytes_bounded` first, then re-emit.

**Implementation hooks (don't rewrite what already exists).** The async stream
decoders for gzip and brotli **already live in `edgezero-core` at
`compression.rs:15` and `compression.rs:41`** — they are core helpers, not
adapter-local code. (Spin's `decompress.rs` is a separate **buffered slice**
decoder — not the async helper.) The existing helpers' chunk error type is
**`io::Error`** (not `anyhow::Error`); the migration **evolves them in place** to
yield `EdgeError` chunks per the round-15 `Body::Stream` change in §7 — wrap each
`io::Error` with `EdgeError::bad_gateway(..)` (a decode-side IO failure is a 502
outcome, distinct from EdgeError-typed `gateway_timeout` chunks the wrapper might
inject). No lift or relocation needed. CF/Fastly/Spin response converters call
into these existing core helpers; Axum keeps its buffered path (a non-streaming
decoder is fine there, since the response converter buffers anyway — §4.1).

In `Streamed` mode no cap is pre-enforced; the caller applies one via
`OutboundResponse::into_bytes_bounded(max)`. That method does **not** delegate to
`Body::into_bytes_bounded` directly — `Body::into_bytes_bounded` maps over-limit to
`bad_request` (400), correct for the inbound body case but wrong for an over-large
upstream response. `OutboundResponse::into_bytes_bounded` performs its own bounded
drain and maps to `bad_gateway` (502). On adapters that decompress, the cap is enforced
against decompressed output here too.

#### 3.4.2 Inbound request bodies

Wrap the existing `Body::into_bytes_bounded` with context-level helpers:

```rust
// crates/edgezero-core/src/context.rs
impl RequestContext {
    /// Read the inbound request body into `Bytes`, bounded by `max`.
    /// Over-limit yields `Err(EdgeError::bad_request(..))` (400).
    ///
    /// **Takes `&self`** — `RequestContext` carries an internal body cache
    /// (an `unsync::OnceCell<Bytes>` style cell; single-threaded per
    /// request, no `tokio` dep). This is deliberate so that existing
    /// `FromRequest` extractors that take `&RequestContext` (e.g. `Json`,
    /// `ValidatedJson`) can call it without a trait-signature breaking
    /// change. The first call drains the underlying `Body::Stream` into
    /// the cell; later calls return a cheap clone. The cached size is
    /// re-validated against `max` on every call, so a later, stricter cap
    /// is still enforced after buffering. The network body is read at most
    /// once.
    pub async fn body_bytes(&self, max: usize) -> Result<Bytes, EdgeError>;

    /// Call `body_bytes(max)` then deserialize as JSON. Malformed inbound
    /// JSON yields `Err(EdgeError::bad_request(..))` (a client bug → 400,
    /// in contrast to outbound `OutboundResponse::json` which maps to 502).
    /// Same `&self` cache semantics as `body_bytes`.
    pub async fn json_within<T: DeserializeOwned>(&self, max: usize)
        -> Result<T, EdgeError>;

    /// Call `body_bytes(max)` then deserialize as `application/x-www-form-urlencoded`.
    /// Default cap from extractors: `DEFAULT_INBOUND_FORM_BYTES = 1 MiB`
    /// (forms are typically small). Malformed form data → `bad_request` (400).
    /// Same `&self` cache semantics as `body_bytes`.
    pub async fn form_within<T: DeserializeOwned>(&self, max: usize)
        -> Result<T, EdgeError>;
}
```

#### 3.4.3 New `EdgeError` variants & mapping

`EdgeError` is `#[non_exhaustive]`, so this is additive.

```rust
// crates/edgezero-core/src/error.rs — add two variants + constructors
EdgeError::BadGateway { message: String }      // -> 502
EdgeError::GatewayTimeout { message: String }  // -> 504

pub fn bad_gateway(message: impl Into<String>) -> Self;
pub fn gateway_timeout(message: impl Into<String>) -> Self;
```

`EdgeError::status()` gains `BadGateway => 502`, `GatewayTimeout => 504`.

| Condition | `EdgeError` | HTTP status |
| --- | --- | --- |
| Inbound request body over limit / not valid JSON | `bad_request` | 400 |
| Invalid outbound URI (relative / no authority / bad scheme) | `bad_request` | 400 |
| Outbound transport failure (DNS / TLS / connect) | `bad_gateway` | 502 |
| Outbound response over `max_response_bytes` (decompressed) | `bad_gateway` | 502 |
| Outbound response body not valid JSON / `json::<T>` called on a streamed body | `bad_gateway` | 502 |
| Outbound per-request timeout or batch deadline exceeded | `gateway_timeout` | 504 |
| Outbound completed with a non-2xx status | **not an error** — `Ok(OutboundResponse)` | app decides |

The non-2xx rule is load-bearing: a target returning 204/400/500 is a normal fan-out batch
outcome, not a transport error.

#### 3.4.4 Batch memory model (explicit)

`send_all` does not impose a global allocation ceiling. The bound comes in two parts —
a **persistent collected buffer** term that holds the request payloads and the
buffered response payloads, plus a **transient in-flight chunk** term that
briefly co-exists with the collected buffer per actively-draining slot (per
§3.4.1's pre-append checked accounting, the in-flight chunk is held during the
overflow check before being appended or dropped):

```
persistent collected buffer  =  Σᵢ request_bodyᵢ.len()
                              + Σᵢ max_response_bytesᵢ      (send_all is buffered-only)

transient in-flight chunks   =  Σⱼ sizeof(current_chunkⱼ)
                                                            // j ranges over slots
                                                            // currently inside a drain
                                                            // step; typically 8-64 KiB
                                                            // per active slot

worst-case resident memory   =  persistent + transient

// Equivalently, when all slots share the same response cap, the persistent term is:
//     Σᵢ request_bodyᵢ.len()  +  N × max_response_bytes
// — but the precise sum is over the per-slot caps, not a single N × max.
// Heterogeneous caps (mix of `.max_response_bytes(small)` and unset slots) bound
// the persistent term by Σᵢ instead of N × max(capᵢ).
```

`send_all` rejects streamed request bodies and streamed responses in preflight
(§3.1.1), so a Streamed-mode batch memory model does not exist. Single `send`
with `Streamed` is the path for lazy bodies, where memory is bounded by the
streaming chunk buffer plus whatever the consumer chooses to buffer via
`into_bytes_bounded`.

EdgeZero's contract — **persistent** (post-append, retained) vs **transient**
(in-flight, dropped after the cap check):

- **Per-response (Buffered).** *Persistent* memory — the collected buffer — is bounded
  by `max_response_bytes`. *Transient* worst-case resident memory during a drain is
  `max_response_bytes + sizeof(current_chunk)`, where `sizeof(current_chunk)` is
  source-controlled (§3.4.1). The post-check buffer never exceeds `max_response_bytes`.
- **Per-inbound-body.** *Persistent* memory — the cached `Bytes` after a successful
  drain — is bounded by the `max` passed to `body_bytes(max)` / `json_within(max)` /
  `form_within(max)`. *Transient* worst-case during the drain is the same shape:
  `max + sizeof(current_chunk)`, with the in-flight chunk source-controlled
  (§3.4.1 / §3.4.5).
- **Batch (N)** memory is the app's responsibility: the app must bound the number of
  requests passed to `send_all`. Both terms add up — *persistent* is
  `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ` (`request_bodyᵢ` and
  `max_response_bytesᵢ` denote slot `i`'s buffered request body length and its
  per-request response cap respectively); *transient* adds
  `Σⱼ sizeof(current_chunkⱼ)` over actively-draining slots, source-controlled.
  For typical fan-out workloads this is intrinsic — `N` is the fixed, configured target count and
  target responses are small JSON. The spec deliberately does **not** add a
  `max_concurrency` knob: on Fastly all requests must be in-flight at once for
  fan-out to work, so throttling concurrency would defeat the feature. This
  requirement is documented in the `send_all` rustdoc and in `docs/`. See §8 risk 11
  for the deferred per-batch transient-chunk cap.

#### 3.4.5 Inbound body migration

The body-bound guarantee in §3.4.4 only holds if the adapter does not pre-buffer the
inbound request body before core can apply a cap. Today every adapter pre-buffers
(`crates/edgezero-adapter-axum/src/request.rs:24` buffers JSON with `usize::MAX`;
`crates/edgezero-adapter-cloudflare/src/request.rs:60` calls `req.bytes()`;
the Fastly and Spin paths fully materialize the body too). This migration changes that:

- **Adapter request conversion** stops pre-buffering. Inbound `Request` is exposed to
  core with a `Body::Stream` (or `Body::Once` only when the platform genuinely owns
  the bytes already — e.g. an in-process Axum body that arrived buffered). Each
  adapter's `request.rs` is updated to wrap the platform body as a stream rather than
  drain it eagerly.
- **`RequestContext` is restructured** — today it holds a plain `Request`, which cannot
  be safely mutated through `&self`. The new shape:

  ```rust
  pub struct RequestContext {
      path_params: PathParams,
      parts: http::request::Parts,   // method, uri, version, headers, extensions
      body: BodyCell,                // interior-mutable
  }

  struct BodyCell(/* unsync */ RefCell<BodyState>);

  enum BodyState {
      Initial(Body),                 // never read; the platform body is still owned
      Draining,                      // body taken out, drain in progress
      Cached(Bytes),                 // body drained successfully
      Poisoned(StoredError),         // drain failed (over-cap, stream error, drop)
      Taken,                         // body consumed via take_body / into_request
  }

  /// Non-consuming snapshot of cell state for app inspection.
  pub enum BodyKind {
      Initial,
      Draining,
      Cached { len: usize },
      Poisoned,
      Taken,
  }
  ```

  `RefCell` (unsync) is fine because a `RequestContext` is owned per-request and
  EdgeZero's async traits already use `?Send`. No `tokio` dependency in core.

  **Async drain protocol.** A naive "borrow_mut across .await" implementation would
  panic on reentrant access or hold the borrow indefinitely if the future is dropped
  mid-drain. The implementation is therefore:

  1. Briefly borrow the cell, `mem::replace` the state with `Draining` while taking
     ownership of the `Body`, drop the borrow. (No borrow held across any `.await`.)
  2. Drive the async drain on the owned `Body`. A drop guard wraps the drain such
     that, on success, the cell is set to `Cached(bytes)`; on stream error or cap
     overflow, the cell is set to `Poisoned(stored_err)`; on **future-cancellation**
     (the drain future is dropped), the guard's `Drop` sets the cell to
     `Poisoned(StoredError::cancelled())`. The network body is partially consumed and
     unrecoverable in every failure case — poison is sticky.
  3. While the cell is in `Draining`, any reentrant `body_bytes` / `json_within` call
     observes that state and returns `Err(EdgeError::internal("body read already in
     progress"))` rather than panicking; this would only occur in programmer-error
     scenarios but must not crash the host.

  Tested in §5.4: drop-mid-drain → next call yields `cancelled` poison;
  reentrant-during-drain → `internal` (no panic); successful drain → reentrant call
  during drain is impossible because Phase 1 is non-async, so the test exercises the
  paths a real async runtime can produce.

- **Public methods become coherent with the cache.** Their post-cache behaviour is
  explicit so middleware → handler → proxy-forward chains compose:

  | Method | Behaviour |
  | --- | --- |
  | `method()` / `uri()` / `headers()` / `extensions()` | from `parts` — unaffected by body state |
  | `headers_mut()` / `extensions_mut()` | mutates `parts` — unaffected by body state |
  | `parts() -> &http::request::Parts` / `parts_mut() -> &mut http::request::Parts` | direct access to the underlying `Parts` for middleware that needs the full snapshot; same body-state-irrelevance as the granular accessors above. These are the migration target for call sites currently doing `ctx.request()` / `ctx.request_mut()` (§6 sweep). |
  | `body_kind() -> BodyKind` | a non-consuming snapshot of the cell state — variants enumerated above (`Initial \| Draining \| Cached { len } \| Poisoned \| Taken`). There is **no** `body() -> &Body` / `body() -> Body` accessor — a `&Body` reference cannot span the cell's interior mutability, and a value-returning getter would either consume the stream (single-shot) or require a tee. Callers either buffer via `body_bytes`/`json_within` or consume via `take_body`/`into_request`. |
  | `take_body() -> Result<Body, EdgeError>` | consume the body out of the context: `Initial` → `Ok(Body::Stream(..))`, set state to `Taken`; `Cached(bytes)` → `Ok(Body::Once(bytes))`, set state to `Taken`; `Draining` → `Err(EdgeError::internal("body read in progress"))` (programmer error); `Poisoned(err)` → `Err(err.clone_as_edge_error())`; `Taken` → `Ok(Body::empty())`. After a successful `take_body`, the body cannot be re-read or buffered. |
  | `body_bytes(max)` / `json_within(max)` / `form_within(max)` | from `Initial`: drains → `Cached`, returns clone (or → `Poisoned(err)` on drain failure, then returns that error). From `Cached`: re-validates `max` and returns a clone. From `Poisoned`: returns a fresh `EdgeError` reproduced from the stored error. From `Draining`: `Err(EdgeError::internal("body read in progress"))` — programmer error. From `Taken`: `Err(EdgeError::internal("body already consumed via take_body"))` — buffered helpers cannot resurrect a body that was handed out. |
  | `into_request() -> Result<Request, EdgeError>` | reassembles a `Request` from `parts` + the cell's body via the same rules as `take_body`: `Cached` → `Ok(Body::Once(bytes))`, `Initial` → `Ok(Body::Stream(..))`, `Draining` → `Err(EdgeError::internal("body read in progress"))` (programmer error), `Poisoned(err)` → `Err(err.clone_as_edge_error())` — **not** `Body::empty()`, because a poisoned read silently turning into an empty proxy-forward would violate the "poison is sticky" rule below, `Taken` → `Ok(Body::empty())` (the caller consumed via `take_body`, the empty is intentional). This is what `OutboundRequest::from_request(ctx.into_request()?, uri)?` uses, so streaming proxy-forward still works **even after middleware has buffered the body** (the cached `Bytes` flow through), and a permissive proxy-forward cannot mask a stricter middleware's poisoned read. |

  The legacy `request()` / `request_mut()` accessors are removed (they leaked the
  whole `Request` and made the body cell incoherent); call sites switch to
  `parts()` / `parts_mut()` for headers/method/uri/extensions, `body_kind()` for
  state inspection, `body_bytes(max)` / `json_within(max)` for buffered consumption,
  `take_body()` for one-shot consumption, and `into_request()` for proxy-forward
  reassembly.

- **Poison semantics on failed body reads.** If `body_bytes` fails mid-drain — the cap
  is exceeded, the stream errors, or a future cancellation interrupts the drain — the
  network body has already been partially consumed and cannot satisfy any later call.
  The body cell transitions to `Poisoned(stored_err)`, where `stored_err` is enough
  metadata to reproduce a fresh `EdgeError` on every subsequent call (since `EdgeError`
  is not `Clone`). All later `body_bytes`/`json_within` calls return that error;
  `body_kind()` reports `Poisoned`; `take_body()` and `into_request()` both return
  `Err(stored)` — the latter explicitly fallible so a poisoned read cannot silently
  become an empty proxy-forward. The network body is **not**
  retried. This is the most defensible contract: silently re-reading is impossible, and
  silently succeeding with a larger-cap call would let a permissive extractor mask a
  stricter middleware's enforcement. The poisoned error variant matches the first
  failure (e.g. an over-cap drain returns `bad_request` on call N+1 too).

- **Existing extractors.** All extractors that consume the inbound body are migrated to
  the bounded helpers:

  | Extractor (today) | After migration |
  | --- | --- |
  | `Json<T>` (uses `ctx.json()`, assumes buffered body) | delegates to `ctx.json_within(DEFAULT_INBOUND_JSON_BYTES)` — `DEFAULT_INBOUND_JSON_BYTES = 8 MiB` |
  | `ValidatedJson<T>` | as above + `validator` pass; sibling `ValidatedJsonWithin<T, MAX>` for explicit caps |
  | `Form<T>` (uses `ctx.form()`, also rejects streams today — `crates/edgezero-core/src/extractor.rs:375`, `crates/edgezero-core/src/context.rs:31`) | delegates to a new `ctx.form_within(max)` helper, default `DEFAULT_INBOUND_FORM_BYTES = 1 MiB` (forms are typically small) |
  | `ValidatedForm<T>` | as above + `validator` pass; sibling `ValidatedFormWithin<T, MAX>` for explicit caps |

  The legacy `RequestContext::json()` and `RequestContext::form()` are removed; both
  required `Body::Once` and would break once adapters stop pre-buffering.

- **Extractor trait.** No change required — `FromRequest::from_request(&RequestContext,
  ..)` continues to take `&RequestContext`, which works because `body_bytes` is now
  `&self`-callable through the cache.

Net effect: per-inbound-body memory is bounded at the boundary of the bounded helper
that actually reads the body; failed reads are sticky so a permissive caller cannot
silently bypass a stricter one; streaming proxy-forward works whether or not middleware
already buffered the body.

### 3.5 Capability declaration

#### 3.5.1 Manifest section

```toml
# edgezero.toml
[capabilities]
required = ["outbound-http", "outbound-deadlines"]
optional = ["config-store"]

[capabilities.outbound]
hosts = ["*"]   # optional plumbing; default ["*"]
```

```rust
// crates/edgezero-core/src/capability.rs  (new module)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    OutboundHttp,                       // can issue outbound HTTP at all
    OutboundDeadlines,                  // wall-clock budget on a *single* outbound
                                        // exchange: connect + headers + buffered
                                        // response body AND chunk-yield of a streamed
                                        // response body (§3.3.3). For `send_all`,
                                        // this covers both the headers phase and the
                                        // **active body-drain phase** of each slot —
                                        // a slot's active drain still honours the
                                        // single-slot bound (≤ one between-bytes-
                                        // timeout overshoot per gap on Fastly per
                                        // §3.3.4). The **cross-slot harvest delay**
                                        // (slot k waiting behind earlier slots'
                                        // drains in Fastly Buffered mode) is *not*
                                        // covered here — that is the separate
                                        // `SendAllSlotIsolation` capability below,
                                        // so each label means exactly one thing.
    OutboundFlexiblePhaseBudget,        // the entire request budget is one elastic
                                        // pool — a slow connect followed by a fast
                                        // headers + body that would together fit
                                        // inside the total budget actually succeeds.
                                        // Native on Axum/CF/Spin (single total
                                        // timeout, no per-phase split); BestEffort on
                                        // Fastly (rigid 1/4 connect + 3/4 first-byte
                                        // split — §4.3 documented deviation). Apps
                                        // with slow-connect-but-fast-rest workloads
                                        // require this and get a hard fail on Fastly.
    SendAllSlotIsolation,               // in `send_all`, each slot's result reflects
                                        // what it would have produced in isolation —
                                        // sibling-slot timing cannot turn a slot that
                                        // would have completed within its own
                                        // `budget.deadline` into a 504. Native on
                                        // Axum/CF/Spin; BestEffort on Fastly
                                        // (harvest-order false 504s in Buffered mode,
                                        // §3.3.4).
    StreamedUploadDeadlines,            // can preempt a stalled `stream.next().await`
                                        // while feeding a streamed REQUEST body
                                        // (Fastly = BestEffort)
    LazyStreamedResponsePassthrough,    // `into_response()` on a streamed body
                                        // delivers chunks without first collecting
                                        // the whole body (Axum = BestEffort,
                                        // see §3.5.2 footnote 3)
    ConfigStore,
    KvStore,
    SecretStore,
}

impl Capability {
    pub fn as_str(&self) -> &'static str;   // kebab-case, for messages
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitySupport {
    /// Fully supported with no documented caveats.
    Native,
    /// Real enforcement with a precisely documented, deterministic bound on any
    /// deviation. Used for timing-related degradations (e.g. Fastly
    /// outbound-deadlines body phase — overshoot ≤ one between-bytes-timeout
    /// interval, §3.3.4).
    BoundedCooperative,
    /// Available but with a documented limitation that the matrix footnotes
    /// describe. The limitation can be timing-related (unbounded cooperative
    /// enforcement, e.g. Fastly source-stream-stall in
    /// `streamed-upload-deadlines`) **or functional** (deterministic behaviour
    /// differs from `Native`, e.g. Axum `lazy-streamed-response-passthrough`
    /// buffers rather than streaming). `BestEffort` therefore means
    /// "supported, with a real-world deviation you need to read the footnote
    /// to understand" — not specifically "unbounded cooperative timing."
    BestEffort,
    /// Not available.
    Unsupported,
}
```

The capability is named **`outbound-deadlines`**, not `timers`, and is defined precisely:
"the platform can enforce a wall-clock budget on an outbound HTTP request." It makes no
claim about timing arbitrary guest computation (which EdgeZero does not offer — §3.3.5),
so an app declaring it gets exactly what the name says on every adapter.

```rust
// crates/edgezero-core/src/manifest.rs — new field on Manifest
#[derive(Debug, Default, Deserialize, Validate)]
pub struct ManifestCapabilities {
    #[serde(default)]
    pub required: Vec<Capability>,
    #[serde(default)]
    pub optional: Vec<Capability>,
    #[serde(default)]
    #[validate(nested)]
    pub outbound: ManifestOutboundCapability,
}

#[derive(Debug, Deserialize, Validate)]
pub struct ManifestOutboundCapability {
    /// Outbound host plumbing. `["*"]` (the default) means "any host".
    /// `length(min = 1)` enforces at least one entry; per-entry validation
    /// is the `validate_outbound_hosts` custom validator below, which checks
    /// every entry against §3.5.4's accepted forms (wildcard, scheme-prefixed,
    /// host:port, bare host, wildcard subdomain).
    #[serde(default = "default_outbound_hosts")]
    #[validate(length(min = 1), custom(function = "validate_outbound_hosts"))]
    pub hosts: Vec<String>,
}

fn default_outbound_hosts() -> Vec<String> { vec!["*".to_owned()] }

impl Default for ManifestOutboundCapability {
    fn default() -> Self { Self { hosts: default_outbound_hosts() } }
}

/// Per-entry validation for `[capabilities.outbound].hosts` (§3.5.4). This is
/// **host-authority-only plumbing**, not a URI field — the same rationale as
/// `OutboundRequest`'s userinfo rejection (§3.1.3 — credentials must not leak
/// through the manifest into `allowed_outbound_hosts`).
///
/// Each entry MUST be one of:
/// - `"*"` (the wildcard).
/// - `scheme://host[:port]` where:
///   - `scheme ∈ {http, https}`, case-**insensitive** at the validator
///     (RFC 3986 §3.1) — `HTTPS`, `https`, `Https` all accepted. The
///     §3.5.4 Spin renderer then canonicalizes to lowercase before emitting
///     `spin.toml`, so the rendered manifest carries one canonical
///     spelling. Other schemes → rejected at the validator.
///   - `host` is a DNS label, IPv4 literal, IPv6 literal in brackets, or
///     `*` / `*.domain.tld` wildcard form.
///   - `port`, if present, is a decimal integer in `1..=65535`.
///   - **NO userinfo, NO path, NO query, NO fragment.** `https://user:pass@x`,
///     `https://x/p`, `https://x?q`, `https://x#f` all reject.
/// - `host[:port]` (no scheme) — same host/port rules as above.
///
/// Empty entries, schemes other than `http`/`https`, ports outside
/// `1..=65535` or non-numeric, any userinfo / path / query / fragment, and
/// authorities the `Uri` parser rejects all yield a `ValidationError`. `"*"`
/// mixed with specific hosts is allowed; the wildcard renders both schemes
/// (§3.5.4) and specific hosts render alongside.
///
/// §5.4 has a Tier 1 test row exercising every accept and reject case:
/// empty string, bad scheme (`ftp://x`), missing authority (`https://`),
/// userinfo (`https://u:p@x`), path (`https://x/p`), query (`https://x?q`),
/// fragment (`https://x#f`), out-of-range port (`https://x:0`,
/// `https://x:70000`), non-numeric port (`https://x:abc`), wildcard,
/// wildcard subdomain (`*.example.com`), bare host with port (`x:8443`),
/// IPv6 (`https://[::1]`), and mixed `"*"` + host.
fn validate_outbound_hosts(hosts: &Vec<String>) -> Result<(), ValidationError>;

// Manifest gains:  #[serde(default)] #[validate(nested)]
//                  pub capabilities: ManifestCapabilities,
```

Every field is `#[serde(default)]`, so existing manifests parse unchanged.

#### 3.5.2 Adapter capability metadata

The registry `Adapter` trait gains one method (`capability`). The exact shape
depends on whether the codebase is **today's pre-#269 checkout** or **PR-#269**:

```rust
// crates/edgezero-adapter/src/registry.rs — pre-#269 (today's checkout)
pub trait Adapter: Sync + Send {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;
    fn name(&self) -> &'static str;
    fn capability(&self, capability: Capability) -> CapabilitySupport;   // new
}
```

```rust
// crates/edgezero-adapter/src/registry.rs — PR-#269 target baseline
pub trait Adapter: Sync + Send {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;
    fn name(&self) -> &'static str;
    fn capability(&self, capability: Capability) -> CapabilitySupport;   // new

    // The following methods are PR-#269 surface (not in today's checkout):
    fn provision(&self, args: &ProvisionArgs) -> Result<(), String>;
    fn push_config_entries(&self, args: &ConfigPushArgs) -> Result<(), String>;
    fn validate_config(&self, args: &ConfigValidateArgs) -> Result<(), String>;
    // …other PR-#269 validation hooks elided here; see crates/edgezero-adapter/src/registry.rs
    // in PR-#269 for the full set. They do **not** affect capability metadata —
    // `capability(..)` is the only method `ensure_capabilities` consults.
}
```

This spec only adds `capability(..)`. Everything else in the trait is owned by
PR #269 (or by today's pre-#269 baseline, accordingly) and is shown above purely
so readers don't misread the `Adapter` reference in §3.5.3 as an exhaustive
declaration. The `Adapter::provision(..)` and config-validation hooks referenced
in §3.5.3 / §6 / §7 are the PR-#269 methods listed in the second block; they are
called from the **sibling pre-dispatch gates** on `run_provision` /
`run_config_push` / `run_config_validate`, not from `Adapter::execute`. On
today's checkout there is no `provision` / `config` surface at all — the
sibling-gate wording in §3.5.3 only applies once PR #269 lands.

Capability matrix (all four adapters):

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `outbound-http` | Native | Native | Native | Native |
| `outbound-deadlines` | Native | Native | BoundedCooperative¹ | Native |
| `outbound-flexible-phase-budget` | Native | Native | BestEffort⁵ | Native |
| `send-all-slot-isolation` | Native | Native | BestEffort⁴ | Native |
| `streamed-upload-deadlines` | Native | Native | BestEffort² | Native |
| `lazy-streamed-response-passthrough` | BestEffort³ | Native | Native | Native |
| `config-store` | Native | Native | Native | Native |
| `kv-store` | Native | Native | Native | Native |
| `secret-store` | Native | Native | Native | Native |

¹ Fastly enforcement has **two documented, deterministic overshoot bounds** —
`BoundedCooperative` means real enforcement with a known finite ceiling, not zero
overshoot. All bounds below assume the common-case `total_ms ≥ 4` phase split; the
sub-4 ms degenerate branch adds `total_ms` to each (see §4.3 "Net guarantee" for
both branches explicitly):
- **Single `send`** — `now` is snapshotted inline so there is no batch drift,
  but the **same `BATCH_DISPATCH_SLACK_MAX` guard** applies to the gap between
  `dispatch_budget(req, now)` and `send_async` (backend lookup, possible
  `Backend::builder().finish()`, SDK request construction; see §4.3). Worst-case
  dispatch+headers overshoot is `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (the
  same bound as `send_all`); the window is typically narrower because there's
  no per-slot harvest loop. Body phase overshoot ≤ one between-bytes-timeout
  interval (§3.3.4).
- **`send_all`** — `batch_now` is shared across slots so dispatch+headers carries
  `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (≈ 26 ms when `total_ms ≥ 4`, §4.3
  "Dispatch-overhead slack, hard-bounded"); body phase **once a slot is actively
  draining** is still ≤ one between-bytes-timeout — but the slot's **observed
  completion** can additionally be delayed by the harvest-order serialization
  (preceding slots' drain times). The harvest delay is what the separate
  `send-all-slot-isolation` capability owns (footnote 4); the
  `outbound-deadlines` bound here is on the active-drain phase only, not on
  total observed wall-clock across the batch.

Both are hard adapter constants, not "scales with preflight." `Native` is reserved for
adapters with no such caveat — this rubric lets future adapters be judged consistently
without quiet downgrading. A new adapter unable to honour a capability declares
`Unsupported` and is caught at build time. The `send_all` *buffered-body* cross-slot
caveat (harvest-order false 504s) is **not** within this capability — that one is
`send-all-slot-isolation` (footnote 4), so each label means exactly one thing.

² Fastly has no guest primitive to preempt a stalled `stream.next().await` while feeding
a streamed REQUEST body via `send_async_streaming` (§4.3). Once chunks start flowing,
the host's `between-bytes-timeout` still bounds inter-chunk gaps, but a source stream
that never yields the next chunk is unbounded on the guest side. This is `BestEffort` —
no documented preemption bound — and is exposed as the separate
`streamed-upload-deadlines` capability so apps that need real-time enforcement on this
specific path declare it required and get a hard build failure on Fastly per §3.5.3.
Apps that buffer their request bodies before calling `send` are unaffected — buffered
uploads use `Body::Once`, no `stream.next().await`, and fall under `outbound-deadlines`
(BoundedCooperative on Fastly).

⁵ `outbound-flexible-phase-budget` captures whether the adapter treats the request
budget as one elastic pool. On Axum/CF/Spin there is a single total SDK timeout
(reqwest's `.timeout(..)`, `worker::Delay`, the wasi timer); a slow connect followed
by a fast headers+body inside the total budget succeeds. On Fastly the budget is
**rigidly split** (§4.3 — `connect = budget/4`, `first_byte = 3*budget/4`,
`between_bytes = budget`); a request that takes more than `budget/4` on connect-phase
work fails at the connect timer even though the rest of the budget would have
sufficed. This is a documented `BestEffort` deviation — the platform-level cause is
that Fastly's `BackendBuilder` exposes per-phase timers and no total-budget timer.
Apps that need elastic budget allocation (slow-connect workloads, mixed-latency
upstreams) declare this capability required and get the hard build failure on
Fastly per §3.5.3.

⁴ `send-all-slot-isolation` is `BestEffort` on Fastly because Fastly's `send_all`
buffered-body drain runs in harvest order (§3.3.4). A slot whose own
`budget.deadline` would have covered its body in isolation can still return
`gateway_timeout` because an earlier slot's body drain monopolised harvest. The
*headers* phase remains correct per-slot (host enforces independently) — only the
body phase loses isolation. Apps that need cross-slot result isolation declare this
capability required and get a hard build failure on Fastly per the round-5
"required + BestEffort = hard fail" rule (§3.5.3); on Axum/CF/Spin where `join_all`
fans out body drains concurrently, isolation is `Native`. **typical small-body fan-outs are unaffected
because its fan-out response bodies are expected to be small** (the external batch protocol JSON, on
the order of a few KiB) — drain times are sub-millisecond hostcalls, so the
serial-drain wall-clock is negligibly different from concurrent drain and no slot
is starved of its budget. Sharing the same effective deadline across slots does
**not** by itself eliminate the harvest-order false 504s (§3.3.4 spells that out);
small bodies do.

³ `lazy-streamed-response-passthrough` captures whether
`OutboundResponse::into_response()` delivers a streamed upstream body to the platform
response **without buffering**. On Cloudflare / Fastly / Spin the platform SDKs accept a
non-`Send` stream natively (WASM single-threaded guest), and the response converter
chains the wrapped `Body::Stream` through — first chunks flow before the upstream stream
ends. On Axum, `axum::body::Body::from_stream` requires `Send + 'static` and core's
`LocalBoxStream` is intentionally non-Send (WASM compat). Rather than spec an
unspecified shim, the Axum response converter buffers `Body::Stream` to `Bytes` within
the adapter-level constant `AXUM_RESPONSE_STREAM_BUFFER_BYTES` (default 16 MiB; the
per-outbound-request `max_response_bytes` is gone by the time the converter runs)
before constructing the axum response — correct, bounded, but first bytes only flow
after full collection. Apps that need true lazy streaming on Axum declare this
capability required and either (a) target a different adapter or (b) wait for a future
mpsc-bridged implementation. Buffered fan-outs are unaffected. See §4.1 and
§7 for the implementation, §8 for the open mpsc-bridge follow-up.

#### 3.5.3 Build / startup enforcement

`ensure_capabilities` runs as a **pre-dispatch gate at each adapter-selecting
entry point**, not as a per-handler call buried inside a specific `Adapter::*`
impl. The reviewer's pointer at `crates/edgezero-cli/src/adapter.rs:75` is the
controlling fact: in PR #269, `execute(..)` checks for a manifest-defined shell
command first (`manifest_command(..)`), runs it via `run_shell`, and only falls
through to `registry::get_adapter(..).execute(AdapterAction, args)` when no shell
command is configured. A capability gate placed *inside* the registry branch would
not fire for shell-overridden adapters, and a gate placed *inside* a single
`Adapter::execute` impl would not cover `Adapter::provision` or the config-validation
hooks. So the gate sits one level up — at the top of every PR-#269 `run_*`
entry point that selects an adapter.

In PR #269 there are **five concrete gate sites**, listed below. Earlier drafts of
this section called the set "one + two siblings" and "four gates"; the
controlling count is **five** (one inside `execute(..)`, four siblings on the
PR-#269 entry points that don't flow through `execute(..)`).

```rust
// 1. crates/edgezero-cli/src/adapter.rs — first statement of execute(..)
pub fn execute(
    adapter_name: &str,
    action: Action,
    manifest_loader: Option<&ManifestLoader>,
    adapter_args: &[String],
) -> Result<(), String> {
    ensure_capabilities(adapter_name, manifest_loader)?;   // ← gate site 1
    // …existing shell-command / registry dispatch follows…
}

// 2–5. Sibling gates on the PR-#269 entry points that don't flow through execute(..):
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String> {
    ensure_capabilities(&args.adapter, args.manifest_loader())?;        // ← site 2
    // …existing provision dispatch follows…
}
pub fn run_config_push(args: &ConfigPushArgs) -> Result<(), String> {
    ensure_capabilities(&args.adapter, args.manifest_loader())?;        // ← site 3
    /* … */
}
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String> {
    ensure_capabilities(&args.adapter, args.manifest_loader())?;        // ← site 4
    /* … */
}
#[cfg(feature = "demo-example")]
pub fn run_demo() -> Result<(), String> {
    ensure_capabilities("axum", manifest_loader())?;                    // ← site 5
    /* …Axum runner… */
}
```

`run_demo` is feature-gated (`demo-example`) and always selects Axum implicitly,
so its gate is a sibling that hardcodes the adapter name rather than reading it
from args. Sites 1–5 are exhaustive: every PR-#269 command that selects an
adapter enters through one of them.

`ensure_capabilities` itself reads from the **registry** (not from `Adapter::execute`)
because capability metadata is the trait fact `capability(Capability) ->
CapabilitySupport`, and the registry is where adapter implementations are looked up
by name. That means **shell-overridden adapters still get checked**: even if the
manifest configures `[adapters.<name>.commands.build]` so dispatch never reaches
`Adapter::execute`, the gate still consults the registered adapter's `capability(..)`
tuple — the shell override only routes the *action*, it does not opt out of the
*manifest contract*.

**Missing-from-registry policy.** If `registry::get_adapter(adapter_name)` returns
`None`, the policy depends on whether the manifest declares any required or optional
capabilities:

| Manifest `[capabilities]` shape | Adapter in registry? | Outcome |
| --- | --- | --- |
| absent or empty (`required = []`, `optional = []`) | no | `log::warn!("adapter '<name>' not in registry; capability check skipped (no capabilities declared)")` — proceed |
| **any** entry in `required` or `optional` | no | **hard failure**: `Err("adapter '<name>' is not in the registry; cannot verify required/optional capabilities. Register an adapter stub that returns capability metadata, or remove the [capabilities] section.")` |
| absent / empty | yes | proceed (loop bodies trivially pass) |
| has entries | yes | check each per the rubric below |

This preserves the "required capabilities fail early" contract while keeping the
brand-new-shell-only-adapter ergonomics for the *no-capabilities* case (e.g. a
contributor wiring a new edge platform via shell-out, before they've written the
adapter stub). An app that declares any capability requires a registered adapter that
can answer the `capability(Capability) -> CapabilitySupport` question; there is no
silent bypass.

Commands covered by the five gate sites above (one inside `execute(..)`, four siblings):

| PR-#269 command | Entry point | Gate site |
| --- | --- | --- |
| `edgezero build` | `run_build` → `execute(Action::Build, ..)` | `execute(..)` |
| `edgezero serve` | `run_serve` → `execute(Action::Serve, ..)` | `execute(..)` |
| `edgezero deploy` | `run_deploy` → `execute(Action::Deploy, ..)` | `execute(..)` |
| `edgezero auth login` / `logout` / `status` | `run_auth` → `execute(Action::AuthLogin/Logout/Status, ..)` | `execute(..)` |
| `edgezero provision` | `run_provision` → `Adapter::provision(..)` | `run_provision(..)` sibling |
| `edgezero config push` | `run_config_push` → adapter push hook (or `--local`) | `run_config_push(..)` sibling |
| `edgezero config validate` | `run_config_validate` → adapter validation hook | `run_config_validate(..)` sibling |
| `edgezero demo` (feature `demo-example`) | `run_demo` → Axum runner | `run_demo(..)` calls `ensure_capabilities("axum", ..)` |

Commands **not** covered (and why):
- `edgezero new` — generates source files; no adapter is selected, so capabilities
  cannot be checked. The scaffold itself is identical across adapters.
- `edgezero auth status` when no manifest is present — `ensure_capabilities`
  short-circuits `Ok(())` if `manifest_loader.is_none()`, which is the same
  policy the registry-lookup path already uses for "no manifest, no capability
  contract." Documented in the rustdoc.

**Today's checkout (pre-#269) collapses to the same shape with fewer rows:**
`Command::{Build, Serve, Deploy, Dev}` all dispatch through the registry's
`Adapter::execute(AdapterAction::{Build, Serve, Deploy}, ..)` plus `Command::Dev`'s
implicit-Axum runner. The gate goes at the top of each of those four handlers (or
the equivalent helper they call) until PR #269 collapses them into the single
`execute(..)` dispatcher. The wording in rounds 1–43 of the appendices is accurate
against that pre-#269 shape.

```rust
fn ensure_capabilities(
    adapter_name: &str,
    manifest: Option<&ManifestLoader>,
) -> Result<(), String> {
    let Some(loader) = manifest else { return Ok(()) };
    let caps = &loader.manifest().capabilities;
    let Some(adapter) = registry::get_adapter(adapter_name) else {
        // Missing-from-registry policy (see §3.5.3 table). If the manifest
        // declares no capabilities, we can't verify anything anyway — log
        // and proceed so brand-new shell-only adapters work before a stub
        // is wired. If it declares any required/optional capabilities, we
        // cannot answer `capability(..)` and must fail closed.
        if caps.required.is_empty() && caps.optional.is_empty() {
            log::warn!(
                "adapter '{adapter_name}' not in registry; capability check skipped (no capabilities declared)",
            );
            return Ok(());
        }
        return Err(format!(
            "adapter '{adapter_name}' is not in the registry; cannot verify required/optional capabilities. \
             Register an adapter stub that returns capability metadata, or remove the [capabilities] section.",
        ));
    };

    let missing: Vec<_> = caps.required.iter().copied()
        .filter(|c| adapter.capability(*c) == CapabilitySupport::Unsupported)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}' does not support required capabilities: {}",
            missing.iter().map(Capability::as_str).collect::<Vec<_>>().join(", "),
        ));
    }
    let degraded: Vec<_> = caps.required.iter().copied()
        .filter(|c| adapter.capability(*c) == CapabilitySupport::BestEffort)
        .collect();
    if !degraded.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}': required capabilities are only best-effort: {}. \
             best-effort means a documented limitation applies — timing (e.g. \
             unbounded cooperative enforcement) or functional (e.g. lazy streaming \
             becomes buffered). See the capability matrix footnotes. Declare them \
             `optional` if the documented limitation is acceptable.",
            degraded.iter().map(Capability::as_str).collect::<Vec<_>>().join(", "),
        ));
    }
    for cap in caps.required.iter().copied()
        .filter(|c| adapter.capability(*c) == CapabilitySupport::BoundedCooperative)
    {
        log::info!(
            "adapter '{adapter_name}': required capability '{}' is bounded-cooperative; see capability docs for the bound",
            cap.as_str(),
        );
    }
    // Adapter-specific service-config reminders. Capability values are static
    // adapter facts (§4.3); some adapters additionally require deployment-time
    // service configuration that EdgeZero cannot validate from the CLI.
    if adapter_name == "fastly"
        && caps.required.contains(&Capability::OutboundHttp)
    {
        log::info!(
            "adapter 'fastly': required capability 'outbound-http' additionally \
             requires dynamic backends to be enabled on the Fastly service. \
             EdgeZero cannot validate this from the CLI; ensure the service \
             configuration is correct before deploying."
        );
    }
    for cap in caps.optional.iter().copied()
        .filter(|c| adapter.capability(*c) == CapabilitySupport::Unsupported)
    {
        log::warn!(
            "adapter '{adapter_name}': optional capability '{}' unavailable",
            cap.as_str(),
        );
    }
    Ok(())
}
```

- **Required + `Unsupported` → hard failure** with an explicit message.
- **Required + `BestEffort` → hard failure.** `BestEffort` means a **documented
  deviation from `Native`** — that can be timing (e.g. Fastly's unbounded source-stall
  in `streamed-upload-deadlines`) or functional (e.g. Axum's buffering of streamed
  responses in `lazy-streamed-response-passthrough`). Either way the deviation is
  real, the matrix footnotes describe it, and "required" should mean the deviation
  is unacceptable. If degradation is acceptable, declare the capability `optional`
  instead — the principle is "required means the matrix footnote's deviation is not
  acceptable for this deployment."
- Required + `BoundedCooperative` → informational log (works, with a documented bound).
- Optional + `Unsupported` → warning. `config-store` and friends stay optional.

#### 3.5.4 Outbound host plumbing — not policy

`[capabilities.outbound].hosts` is **plumbing**, not a security allowlist (non-goal §1.3).
Apps still enforce their own target allowlist in handler code. Adapter use of `hosts`:

- **Spin** requires `allowed_outbound_hosts` in `spin.toml`. The Spin adapter renders
  each entry per the rules below. (`spin.toml.hbs:13` currently hardcodes
  `["https://*:*"]`; that template line is replaced by a render of this list.)

  Every entry is **first canonicalized** by the host-authority subset of
  `OutboundRequest`'s URI rules (§3.1.3): scheme and host are lowercased;
  default ports (`:443` for `https`, `:80` for `http`) are stripped; userinfo
  and fragment are rejected. **Manifest host entries diverge from
  `OutboundRequest` URIs on path/query**: request URIs pass path/query through
  verbatim (the wire-level request target), but manifest host entries are
  host-authority-only declarations, so path/query are also rejected by the
  manifest-host validator (§3.5.1). This divergence is intentional — host
  entries declare "which hosts the app may talk to," not "which paths."
  Sharing the lowercase-scheme / lowercase-host / strip-default-port /
  reject-userinfo / reject-fragment rules with §3.1.3 keeps the canonical
  spelling identical across the two surfaces; the path/query divergence is
  the only difference and is enforced by the validator, not by quietly
  dropping path/query at render time. The render table then takes a
  *canonicalized* input — there is no second normalisation step to drift
  from §3.1.3's spelling.

  | Input form (after canonicalization) | Example | Spin output |
  | --- | --- | --- |
  | wildcard | `"*"` | `["https://*:*", "http://*:*"]` (renders **both** schemes so the "any host" claim and the `http` loopback contract tests (§3.1.3) match the rendered manifest) |
  | scheme-prefixed | `"http://localhost:3000"`, `"https://api.example.com:8443"` | rendered as-is (canonical: scheme/host lowercased, default port stripped) |
  | `host:port` (no scheme) | `"api.example.com:8443"`, `"localhost:3000"` | `"https://<host>:<port>"` — default scheme is https; for http, write the scheme explicitly |
  | bare host (no scheme, no port) | `"api.example.com"` | `"https://<host>"` — **https + Spin default port only**; explicit non-default ports or `http` require writing the full form |
  | wildcard subdomain | `"*.example.com"` | `"https://*.example.com"` |

  The §3.5.1 validator is authoritative — there is no "fallback" branch that
  accepts other `scheme://authority` strings Spin happens to like. Mixing `"*"`
  with specific hosts is allowed (Spin treats `"*"` as fully permissive). Bare
  hosts deliberately mean "https + default port only" — defaulting tight rather
  than promiscuous. Hosts that the canonicalization would change (e.g. uppercase
  `EXAMPLE.com`, default-port `https://x:443`) are accepted and silently
  canonicalized; the rendered `spin.toml` reflects the canonical form, so what
  apps see matches what `OutboundRequest::uri()` reports.
- **Fastly** uses runtime **dynamic backends** that work for any host, so it does not
  need the list at build time; `hosts` is informational for Fastly.
- **Axum / Cloudflare** ignore the list (no host pre-declaration needed).

## 4. Adapter-by-adapter implementation notes

Each adapter renames `src/proxy.rs` → `src/outbound.rs`, replaces its `ProxyClient`
impl with an `OutboundHttpClient` impl, adds `capability()`, and gains a
`tests/contract.rs`.

### 4.1 Axum — `crates/edgezero-adapter-axum`

- `AxumProxyClient` → `AxumOutboundClient`; keeps the pooled `reqwest::Client`.
- `send_all` first runs a **preflight** per slot: any request whose `body` is
  `Body::Stream` OR whose `response_mode` is `Streamed` is converted in place to
  `Err(EdgeError::bad_request(..))` (§3.1.1) so the trait contract holds identically
  on every adapter. The Buffered-mode buffered-body survivors are fanned out via
  `futures::future::join_all` over a private `send_one(req, batch_now)`; index
  alignment is preserved by tracking the original positions while building the
  future set. **`send_all` snapshots `let batch_now = web_time::Instant::now()` once**
  before fanning out and passes the same value to every per-slot
  `dispatch_budget(req, batch_now)` — see §3.3.2 / §4.3 for why a per-slot
  `Instant::now()` would drift the shared-deadline `duration` and (on Fastly) the
  backend identity.
- `send_one(req, now)` flow, in this order:
  1. **Compute the budget.** `let budget = dispatch_budget(req, now)?` (§3.3.2 —
     never an adapter-local formula, so `DEFAULT_NO_DEADLINE_BUDGET = 30 s` is
     applied uniformly when no deadline is set). On expiry-before-dispatch this
     returns `Err(gateway_timeout)` for the slot immediately. For a single `send`,
     `now = web_time::Instant::now()` is taken inline.
  2. **If the request body is `Body::Stream`, drain it to `Bytes` first.** Core
     `Body::Stream` is `LocalBoxStream` (not the `Send + 'static` stream
     `reqwest::Body::wrap_stream` requires), so Axum drains a streamed request body
     into `Bytes` up to `req.max_request_body_bytes` (default 8 MiB) **before**
     constructing the reqwest request. Pre-append checked accounting per §3.4.1;
     over-cap → `bad_request`. The drain itself is raced against `budget.deadline`
     using `tokio::time::timeout`-per-chunk-pull — a stalled upload yields
     `gateway_timeout` rather than consuming the budget silently. Adding reqwest's
     `stream` feature is **not** required.
  3. **Construct the reqwest request.** Build the `reqwest::Request` /
     `RequestBuilder` from the buffered (or now-buffered) body, URI, method,
     and normalized headers. Do not arm the timeout yet — it gets re-read
     at the very last moment in step 4.
  4. **Arm the reqwest timeout and send.** Immediately before
     `.send().await`, re-read `budget.deadline.remaining()`. If `None` (drain
     + construction consumed the budget) → `gateway_timeout` without
     sending. Otherwise `.timeout(remaining)` is set from this
     just-re-read value, **not** from the cached value at end-of-drain and
     **not** from the original `budget.duration`. Re-reading at arming time
     (matching Spin's "at the moment the race starts" — round 21) closes
     the construction-time gap that would otherwise let a 100 ms build
     phase silently extend the SDK timeout past the absolute deadline.
     reqwest's timeout covers the response-body read, so a `Buffered`
     drain inherits the deadline. `Buffered` mode drains the response
     body with a running decompressed-byte counter against `max_bytes`
     (pre-append check per §3.4.1). `Streamed` mode wraps `reqwest`'s
     byte stream with a `tokio::time::timeout`-per-chunk wrapper bounded
     by `budget.deadline`; the wrapper yields a `gateway_timeout` error
     chunk past the deadline so the streamed body honours the deadline
     end-to-end per §3.3.3.
- Errors: `reqwest` timeout → `gateway_timeout`; connect/DNS/TLS → `bad_gateway`;
  over-cap → `bad_gateway`. Any completed exchange (incl. non-2xx) → `Ok`.
- `capability()` per §3.5.2: `outbound-http` = `Native`, `outbound-deadlines` = `Native`,
  `outbound-flexible-phase-budget` = `Native` (Axum's reqwest exposes a single total
  timeout, not a phase split), `send-all-slot-isolation` = `Native`,
  `streamed-upload-deadlines` = `Native`, `lazy-streamed-response-passthrough` =
  `BestEffort` (footnote 3 — Axum buffers, see `response.rs` task in §7),
  `config-store` / `kv-store` / `secret-store` = `Native`. **Nine** capabilities total.
- Reference adapter for the contract (§5): real loopback HTTP.

### 4.2 Cloudflare — `crates/edgezero-adapter-cloudflare`

- `CloudflareProxyClient` → `CloudflareOutboundClient` (stays stateless).
- `send_all` first runs a **preflight** per slot: any request with `Body::Stream`
  OR `response_mode = Streamed` is converted to `Err(EdgeError::bad_request(..))`
  per §3.1.1 *before* `send_one` is invoked. **`send_all` snapshots `let batch_now =
  web_time::Instant::now()` once** before fanning out and passes it to every
  `send_one(req, batch_now)`. Buffered-mode buffered-body survivors are fanned out
  via `join_all`; the Workers JS event loop provides the concurrency. Index
  alignment is preserved.
- `send_one(req, now)` flow, in this order:
  1. **Compute the budget.** `let budget = dispatch_budget(req, now)?` (§3.3.2).
     Expiry before dispatch returns `Err(gateway_timeout)` for the slot.
  2. **If the request body is `Body::Stream`, drain it to `Bytes` first.** Up to
     `req.max_request_body_bytes` (default 8 MiB), pre-append checked accounting;
     over-cap → `bad_request`. The drain is raced against `budget.deadline` using
     a per-chunk-pull `worker::Delay` race — a stalled upload yields
     `gateway_timeout` rather than consuming the budget silently.
  3. **Construct the `worker::Request`.** Build the request from the
     buffered (or now-buffered) body, URI, method, and normalized headers.
     Do not start the `worker::Delay` race yet.
  4. **Arm the race and send.** Immediately before issuing fetch and starting
     the `worker::Delay`, re-read `budget.deadline.remaining()`. `None` →
     `gateway_timeout` without sending. Otherwise race the fetch **and**, in
     `Buffered` mode, the body drain against `worker::Delay(remaining)` using
     this just-re-read value (matching Spin and the round-38 Axum step). On
     expiry drop the future (`gateway_timeout`). The existing gzip/br
     decompression path is kept; the decompressed-byte cap is enforced
     incrementally while decompressing (§3.4.1), with pre-append checked
     accounting.
- **Streamed responses honour the effective-budget deadline.** Wrap the response body
  as `Body::Stream`, with a per-chunk race against a `worker::Delay` bounded by
  `budget.deadline` (the synthetic-if-absent absolute deadline from
  `dispatch_budget`). The wrapper yields a `gateway_timeout` error chunk past the
  deadline so the streamed body honours the deadline end-to-end per §3.3.3.
- `capability()` per §3.5.2: `Native` for **all nine** capabilities
  (`outbound-http`, `outbound-deadlines`, `outbound-flexible-phase-budget` (single
  `worker::Delay` for the total race, no per-phase split), `send-all-slot-isolation`,
  `streamed-upload-deadlines`, `lazy-streamed-response-passthrough`, `config-store`,
  `kv-store`, `secret-store`). Cloudflare's WASM single-threaded guest carries no
  `Send` constraint, so `worker::Body::from_stream` consumes the core `Body::Stream`
  directly **in the response-out direction**
  (`lazy-streamed-response-passthrough` — see §7 `src/response.rs`). The
  **outbound-request upload direction** still drains `Body::Stream` to `Bytes`
  first (bounded by `max_request_body_bytes`, raced against `budget.deadline`),
  because `send_async`-style streamed uploads aren't part of this migration and
  the worker SDK's request-body shape differs from `Body::from_stream`. Don't
  conflate the two — `send_one`'s flow above is the request side; this bullet is
  the response side.

### 4.3 Fastly — `crates/edgezero-adapter-fastly`

The critical adapter. The current code (`proxy.rs:30-35`) does
`send_async_streaming()` then `pending_request.wait()` inside one `send()`, so a
`join_all` of `send()` is fully serial. The fix is **dispatch-all-then-harvest**.

Confirmed `fastly` 0.12.1 API:

```rust
// fastly::http::request
pub fn select<I: IntoIterator<Item = PendingRequest>>(pending_reqs: I)
    -> (Result<Response, SendError>, Vec<PendingRequest>);   // no index returned
pub enum PollResult { Pending(PendingRequest), Done(Result<Response, SendError>) }
// PendingRequest::poll(self) -> PollResult        (non-blocking)
// PendingRequest::wait(self) -> Result<Response, SendError>   (blocks on one)
// Request::send_async(self, backend) -> Result<PendingRequest, SendError>
```

`select` does not report which request completed, so it cannot preserve request↔slot
identity — and the application must know which target answered. The adapter harvests by **indexed
slot** with `wait()` / `poll()`:

```rust
// Each Pending slot carries the metadata `harvest` needs — without these, the
// post-`wait()` body buffering / cap / deadline contract would have nothing to
// work from. (`send_all` rejects streamed REQUEST bodies AND streamed responses
// per §3.1.1 in preflight, so the slot only ever has to handle Buffered
// responses with a max_bytes cap.)
struct PendingSlot {
    pending:    PendingRequest,
    budget:     DispatchBudget,    // duration + absolute deadline (§3.3.2)
    max_bytes:  usize,             // from ResponseMode::Buffered { max_bytes }
}

enum Slot {
    Pending(PendingSlot),
    Done(Result<OutboundResponse, EdgeError>),
    Taken,
}

async fn send_all(
    &self,
    reqs: Vec<OutboundRequest>,
) -> Vec<Result<OutboundResponse, EdgeError>> {
    let n = reqs.len();

    // Single batch-level `now` snapshot — same value passed to every per-slot
    // dispatch_budget so a shared caller Deadline produces the same `duration`
    // and ceiled `budget_ms`, and therefore one dynamic-backend identity per host
    // in a homogeneous-budget batch (§3.3.2 / §4.3).
    let batch_now = web_time::Instant::now();

    // Phase 0 — preflight. send_all rejects streamed REQUEST bodies and streamed
    // RESPONSES per §3.1.1 BEFORE dispatch. Other slots fall through to Phase 1.
    let reqs: Vec<Result<OutboundRequest, EdgeError>> = reqs.into_iter()
        .map(|req| {
            if req.is_stream_body() {
                return Err(EdgeError::bad_request(
                    "send_all requires buffered request bodies"));
            }
            if req.is_stream_response() {
                return Err(EdgeError::bad_request(
                    "send_all requires buffered responses"));
            }
            Ok(req)
        })
        .collect();

    // Phase 1 — dispatch. Every request is in-flight at the host concurrently.
    // dispatch() returns Err for an expired/zero deadline (§3.3.2) so those slots
    // never enter Phase 2. The host connect/first-byte/between-bytes timeouts are
    // set from budget.duration; budget.deadline governs the body-phase cooperative
    // check below.
    let mut slots: Vec<Slot> = reqs.into_iter()
        .map(|maybe_req| match maybe_req {
            Err(e)  => Slot::Done(Err(e)),
            Ok(req) => match dispatch(req, batch_now) {
                // dispatch(req, now) -> Result<(PendingRequest, DispatchBudget, usize), EdgeError>
                // where the third field is max_bytes from ResponseMode::Buffered.
                Ok((pending, budget, max_bytes)) => Slot::Pending(PendingSlot {
                    pending, budget, max_bytes,
                }),
                Err(e) => Slot::Done(Err(e)),
            },
        })
        .collect();

    // Phase 2 — harvest. wait() blocks on one slot; siblings keep progressing at
    // the host. For the headers phase, wall-clock is ~max(header_arrivals), not
    // the sum. Buffered body drain runs *serially* in harvest order, so total
    // wall-clock is ~max(header_arrivals) + Σ body_drain_times — see §3.3.4
    // "Buffered body drain runs in harvest order". poll() opportunistically
    // collects siblings that already finished headers. Only Buffered responses
    // reach this point — Streamed responses were rejected in Phase 0 preflight.
    let mut out: Vec<Option<Result<OutboundResponse, EdgeError>>> =
        (0..n).map(|_| None).collect();
    for i in 0..n {
        match std::mem::replace(&mut slots[i], Slot::Taken) {
            Slot::Done(r)     => out[i] = Some(r),
            Slot::Taken       => { /* already harvested by an earlier poll() */ }
            Slot::Pending(s)  => {
                out[i] = Some(harvest(s.pending.wait(), &s.budget, s.max_bytes));
                for j in (i + 1)..n {
                    // Carefully preserve every variant; the bug we are
                    // avoiding here is "take a Slot::Done(Err(..)) from
                    // preflight or dispatch and replace it with Slot::Taken,
                    // which then drops the Err on the floor and the outer
                    // loop reports a generic 'slot unresolved' internal
                    // error."
                    match std::mem::replace(&mut slots[j], Slot::Taken) {
                        Slot::Done(r)     => out[j] = Some(r),        // preserve preflight / dispatch error
                        Slot::Taken       => { /* already harvested */ }
                        Slot::Pending(s2) => match s2.pending.poll() {
                            PollResult::Done(r)      => out[j] = Some(harvest(r, &s2.budget, s2.max_bytes)),
                            PollResult::Pending(pr2) => slots[j] = Slot::Pending(PendingSlot {
                                pending: pr2,
                                budget: s2.budget,
                                max_bytes: s2.max_bytes,
                            }),
                        },
                    }
                }
            }
        }
    }
    // Invariant: every slot resolved above. Map any unfilled slot to an
    // internal error rather than panic — adapter boundaries must never
    // crash the host on a contract bug.
    out.into_iter()
        .enumerate()
        .map(|(i, r)| r.unwrap_or_else(|| Err(EdgeError::internal(anyhow::anyhow!(
            "fastly outbound: slot {i} unresolved by harvest loop (adapter bug)"
        )))))
        .collect()
}
```

- **`.wait()` is not the problem** — calling it before all requests are dispatched was.
  After Phase 1 every request runs at the host; Phase 2 only collects results.
- **Deadline:** each request's host timeouts are set to the effective budget at dispatch,
  so connect+headers cannot block past it. The body phase checks `budget.deadline`
  **after every blocking body read returns, including the EOF read** (per §3.3.4 —
  the read that discovers EOF can itself cross the deadline and would otherwise
  slip through with `Ok(resp)`). Streamed bodies are wrapped to check before and
  after each underlying read. Bounded overshoot per §3.3.4.
- **Dynamic backends.** Arbitrary HTTPS hosts use Fastly dynamic backends
  (`Backend::builder`). Per Fastly's
  [`BackendBuilder` docs](https://docs.rs/fastly/latest/fastly/backend/struct.BackendBuilder.html),
  a dynamic backend registration with an **identical name + identical properties**
  re-registers / re-uses the existing backend (returns `Ok`); a registration with
  the same name but **conflicting properties** fails with `NameInUse`. Backend
  "sameness" therefore includes every configured property — including the timeouts
  EdgeZero sets from `dispatch_budget(req, now).duration` (§3.3.4). The identity
  therefore covers transport **and** timeout config, not just the authority, so a
  50 ms slot and a 3 s slot to the same host get distinct dynamic-backend names
  (avoiding silent wrong-timeout reuse on the `NameInUse` path; see the
  collision-detection protocol later in §4.3 for the precise reuse rules).

  Identity tuple:
  `scheme + ":" + host + ":" + resolved_port + ":" + tls_mode + ":" + budget_ms`,
  where:
  - `resolved_port` is the URI port or scheme default (`80`/`443`).
  - `tls_mode` is `"tls"` for `https` or `"plain"` for `http`.
  - `budget_ms` is the **true ceil-to-ms** of `dispatch_budget(req).duration` —
    `((duration.as_nanos() + 999_999) / 1_000_000).max(1) as u64`. `as_millis()`
    *floors*, which would turn a 1.9 ms budget into a 1 ms host timeout and
    produce premature Fastly timeouts; ceiling guarantees the host timeout is
    never tighter than the caller's intended budget. The same ceiled value is
    fed into `connect-timeout` / `first-byte-timeout` / `between-bytes-timeout`,
    so the identity tuple and the actual host configuration always match. (Apps
    really wanting a sub-ms wall-clock should not target Fastly — host
    timeouts themselves are millisecond-granular.) §3.3.4's "host timeouts =
    `budget.duration`" is therefore an abbreviation for "host timeouts =
    ceil-to-ms of `budget.duration`"; the body-phase cooperative
    `budget.deadline.is_expired()` check still uses the exact original
    `Deadline`, so the wall-clock contract is unchanged.

  Name = `format!("ez_{:032x}", sha256_128(identity))` — the first 128 bits of a
  SHA-256 digest, collision-resistant in any realistic deployment (the previous
  64-bit FNV-1a draft was not). The name fits inside Fastly's backend-name length
  limit (`ez_` + 32 hex chars = 35 chars) and is valid for any host. In a
  homogeneous-budget batch all slots targeting the same host
  share one backend — **but only because `send_all` takes a single `now` snapshot
  and passes it to every per-slot `dispatch_budget` call** (§3.3.2). Without that,
  sequential `Instant::now()` per slot would derive slightly different `duration`s
  for the same shared caller `Deadline`, which would produce slightly different
  ceiled `budget_ms` values and therefore different identities for the same host
  under one batch deadline. The shared-`now` snapshot is a normative requirement
  of the `send_all` flow, not an implementation hint. In heterogeneous-budget
  fan-out each distinct budget gets its own backend, by design. Per-handler
  backend count is bounded by `unique(host, port, tls, budget_ms)` tuples; apps
  that mix wildly varying budgets should be aware of the dynamic-backend limit on
  their Fastly service.

  **Dispatch-overhead slack, hard-bounded.** Because `batch_now` is captured
  *before* preflight, dynamic-backend creation, and `send_async`, the `budget_ms`
  baked into the backend identity is a *bucketed* timeout — not the exact remaining
  wall-clock at the moment the SDK timer is armed. The Fastly host enforces
  `budget_ms` from the moment it sees the request, so a request can in principle
  complete up to `(now_at_send_async − batch_now) ms` after the absolute fan-out batch
  deadline before the host fires its timeout. To keep this slack
  **deterministically bounded** (so `outbound-deadlines = BoundedCooperative` on
  Fastly is actually true, not just usually-tight):

  - The adapter caps `(now_at_send_async − batch_now)` at
    `pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);`
    (defined alongside `DEADLINE_FAR_FUTURE` in `src/time.rs`, §7).
  - Before each slot's `send_async`, the adapter checks
    `Instant::now() - batch_now <= BATCH_DISPATCH_SLACK_MAX`. If exceeded, the
    remaining slots fail closed with
    `Err(EdgeError::internal("Fastly send_all adapter overhead between batch_now \
     and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) \
     exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale \
     duration"))`. This is an internal diagnostic about **adapter-side** work,
    not a handler-side complaint — handler code runs before `send_all` is even
    invoked, so it runs before `batch_now` is captured and cannot exhaust this
    budget. The interval measured here is adapter overhead: per-slot preflight
    validation, dynamic-backend lookup/creation host calls, and SDK setup
    before `send_async`. If this fires in production, the operator looks at
    backend-creation hostcall latency or a noisy neighbour, not at handler
    code.
  - The cooperative `budget.deadline.is_expired()` check during body drain still
    catches body-phase overshoot per §3.3.4 (one between-bytes-timeout bound).

  Net guarantee, with the explicit **sub-4 ms branch** broken out separately:

  - **`total_ms ≥ 4` (the common case)**: a Fastly slot can complete at most
    **`BATCH_DISPATCH_SLACK_MAX + ms_rounding`** past the absolute fan-out batch
    deadline on the dispatch+headers phase. Because connect and first-byte are
    *separate* host timers (Fastly docs), the budget is split — `connect_ms =
    total_ms / 4`, `first_byte_ms = total_ms - connect_ms` — so their sum equals
    `total_ms` exactly and the dispatch+headers host enforcement is bounded by
    `budget.duration`. If dispatch happens at `batch_now + Δ` with
    `Δ ≤ BATCH_DISPATCH_SLACK_MAX`, the host fires at
    `(batch_now + Δ) + (connect_ms + first_byte_ms) = (batch_now + Δ) + total_ms`,
    which is `Δ + ms_rounding` past the absolute deadline. Setting *both* timers
    to the full budget would have made the worst case ~2× — explicitly *not* what
    this design does (see §3.3.4 / §4.3 code block).
  - **`total_ms < 4` (the sub-4 ms degenerate case)**: §4.3 sets both
    `connect_ms = first_byte_ms = total_ms`, so the dispatch+headers host
    enforcement is bounded by `2 × total_ms` (≤ 6 ms total at the edge). The
    post-deadline slack is therefore up to `BATCH_DISPATCH_SLACK_MAX + total_ms +
    ms_rounding` (strict upper bound `25 + (≤ 3) + (≤ 1) < 29 ms` wall-clock).
    At this scale ms-rounding already
    dominates a meaningful deadline; sub-4 ms outbound budgets are degenerate
    inputs, not a normal operating point. The test row asserts the 2× bound
    explicitly rather than the `=` invariant.

  The body-phase cooperative check still adds up to one between-bytes-timeout
  overshoot during drain (§3.3.4) in either case, but that's the only other
  source. All terms are hard adapter constants, not "scales with preflight."

  Single `send` snapshots `now` inline at `send_one` entry — there is no
  `batch_now` shared across slots — but time still passes between
  `dispatch_budget(req, now)` and `send_async` (backend lookup, possible
  `Backend::builder().finish()` host call, SDK request construction). The
  **same `BATCH_DISPATCH_SLACK_MAX` guard** applies: immediately before
  `send_async`, the adapter checks `Instant::now() - now <=
  BATCH_DISPATCH_SLACK_MAX`; on excess, the single `send` returns
  `EdgeError::internal(..)` with the same "adapter overhead between
  dispatch_budget and SDK arming" diagnostic as `send_all`. The slack window is
  typically narrower for single `send` (no per-slot harvest loop), but the
  bound is the same hard constant; the previous "structurally 0" wording was
  incorrect. The phase-budget split and sub-4 ms branch apply identically.

  §5.4 has a row that locks this. The test cannot use a handler-side sleep before
  `send_all` — that runs *before* the adapter captures `batch_now`, so it never
  exercises the slack guard. The test instead uses an **adapter-internal injection
  hook** (a `#[cfg(test)]` `Fn` slot on `FastlyOutboundClient` invoked between
  `batch_now` capture and per-slot `dispatch()`) to introduce a synthetic delay
  exceeding `BATCH_DISPATCH_SLACK_MAX`. With the hook set, late slots return
  `internal("Fastly send_all adapter overhead between batch_now and SDK arming \
   (preflight + dynamic-backend lookup/creation + SDK setup) exceeded \
   BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration")`;
  without it, no slot ever returns that error. Apps that need exact
  absolute-deadline enforcement on the dispatch+headers phase target a different
  adapter (Axum/CF/Spin all use `budget.deadline.remaining()` at arming time —
  see §4.1 / §4.2 / §4.4 step 3). **Collision detection** is
  belt-and-suspenders. The collision-detection map lives on the
  `FastlyOutboundClient` itself, not per call. Because `OutboundHttpClient` methods
  take `&self` and the trait is `Send + Sync`, the field is
  `Mutex<HashMap<String, (BackendIdentity, Backend)>>` — interior mutability with
  thread-safe access. The simplest race-free protocol:

  1. Acquire the outer lock.
  2. If the name maps to a stored entry `(stored_identity, cached)`:
     - **`stored_identity == identity`**: clone the cached `Backend`, drop the
       lock, dispatch.
     - **`stored_identity != identity`** (an in-adapter SHA-256-128 collision
       between two distinct identities mapping to the same name): fail closed with
       `EdgeError::internal("Fastly dynamic backend name collision in this
       adapter's map — two distinct identities hashed to the same backend name;
       refusing to silently swap settings")`. The previous-round wording reused
       the cached backend by name alone, which would have silently bound a new
       request to whichever identity got cached first — that bug is fixed by the
       explicit identity comparison here. Drop the lock. §5.4 has a row that
       exercises this path via an injectable hash collision under `#[cfg(test)]`.
  3. Otherwise (name is absent), call `Backend::builder(..).finish()` **with the
     lock still held**. The earlier "lock-not-across-host-call" rule from round 20
     is reversed here: Fastly's `finish()` is a short host call that never blocks
     on guest I/O, so holding the lock through it is safe (single-threaded WASM
     has no contention; multi-threaded hosts pay short per-`FastlyOutboundClient`
     serialization, which is one instance per request context).
  4. On `Ok(backend)`: insert `(identity, backend.clone())` into the map and
     return the `Backend`.
  5. On `Err(NameInUse)`: because the outer lock is held continuously through
     steps 2–4, no other thread under this `FastlyOutboundClient` can have
     registered the name without showing up in step 2. Per Fastly's
     [`BackendBuilder` docs](https://docs.rs/fastly/latest/fastly/backend/struct.BackendBuilder.html),
     `NameInUse` means a backend with the same name *and conflicting properties*
     already exists in the session; identical name + identical properties is a
     re-registration that returns `Ok`. So a `NameInUse` here means the name is
     registered by an **external party** (another component, a prior session)
     with properties our builder did not match — and we cannot fully verify the
     stored properties (Fastly's `Backend::from_name` getters don't round-trip
     every builder field — notably SNI / cert hostname). Fail closed with
     `EdgeError::internal("Fastly Backend::builder returned NameInUse for a name
     not in this adapter's collision map; properties conflict with an externally
     registered backend that we cannot safely verify")`. Drop the lock.
  6. On any other `Backend::builder` error, **map to `EdgeError::bad_gateway`** —
     these are service/backend setup failures (dynamic backends disabled on the
     service per §4.3 "Service prerequisite," DNS resolution failure for the
     target, TLS misconfiguration, or any other Fastly-side rejection that
     reaches the guest). Specifically:
     `Err(EdgeError::bad_gateway(format!("Fastly dynamic backend setup failed: {e}")))`.
     `EdgeError::internal` is reserved for **adapter contract bugs** — invariant
     violations the adapter itself should have prevented (the unfilled-slot case
     in the harvest loop, the `BATCH_DISPATCH_SLACK_MAX` overshoot, this
     section's `NameInUse` external-registration case). Drop the lock.

  There is no `BackendSlot::Building` / `Failed` variant and no condvar — holding
  the outer lock through the build means no other thread can observe an
  intermediate state, so the race the round-34 review flagged is structurally
  impossible. A per-name reservation with finer-grained locking is more
  concurrent but only matters on multi-threaded hosts where the Fastly adapter
  isn't used. It applies to:

  - **`send_all`** — each slot looks up its name; if the name already maps to its own
    identity, reuse; if it maps to a *different* identity, fail closed with
    `EdgeError::internal("dynamic backend name collision — refusing to reuse")`.
  - **Single `send`** — same lookup path; same fail-closed behaviour.
  - **Across calls** — the map persists for the lifetime of the
    `FastlyOutboundClient` (one per request context), so a second `send` in the same
    handler reuses the same backend cheaply and a SHA-256-128 collision against an
    earlier call is still caught.
  - **`Backend::builder` returns `NameInUse`** — the adapter cannot fully verify
    the registered identity. Fastly's `Backend::from_name` returns a handle to the
    existing backend but its public getters do not round-trip every builder field
    (SNI hostname / certificate hostname are notably opaque per the
    `BackendBuilder` / `Backend` docs). So the adapter **fails closed** with
    `EdgeError::internal("Fastly Backend::builder returned NameInUse for a name \
     not in this adapter's collision map — refusing to reuse an externally \
     registered backend")`. Names already in the adapter's own map are reused
    cheaply with no `Backend::builder` call (the in-memory `Backend` handle is
    already present); only an *external* registration of a colliding name
    triggers this path, and the safest response is to surface it rather than
    guess. This makes the adapter's collision map authoritative.

  Backends are deduplicated by full identity within and across calls. Requires
  dynamic backends enabled on the service (surfaced via the `outbound-http`
  capability and the service prerequisite below).
- Requests in `send_all` are required to have buffered request bodies AND buffered
  response mode per the trait contract (§3.1.1). A `Body::Stream` request body
  yields `out[i] = Err(EdgeError::bad_request(..))`; a request with
  `response_mode = Streamed` also yields `out[i] = Err(EdgeError::bad_request(..))`.
  This keeps Fastly's dispatch-all-then-harvest model from serializing on slow
  request uploads and removes the cross-slot streamed-response deadline-lifetime
  problem (§3.1.1), identically on every adapter.
- **Streamed request bodies in single `send`.** The single-request path accepts
  `Body::Stream` and uses `Request::send_async_streaming(&backend) -> (StreamingBody,
  PendingRequest)`. The adapter then feeds chunks from the core stream to the
  `StreamingBody`, with these rules:
  - **Byte count cap.** Pre-append checked accounting against
    `req.max_request_body_bytes` (default 8 MiB). Over-cap → `bad_request` (400) —
    the `StreamingBody` is dropped without `finish()`, the `PendingRequest` is
    dropped, and the slot returns the error.
  - **Deadline enforcement has two phases with different bounds:**
    - *Source-stream yield* (`stream.next().await`): **unbounded on Fastly** — no
      guest async primitive can preempt a stalled `stream.next()` waiting for the
      app's source stream to yield. This is the `BestEffort` aspect of
      `streamed-upload-deadlines` on Fastly. Apps that need real-time enforcement
      against an untrusted upload source must pass a buffered request body
      (`Body::Once`) where the bytes are already in hand and no `stream.next().await`
      is involved.
    - *Host write* (`StreamingBody::write_all` / `flush()` on a yielded chunk): these
      are synchronous host calls. Fastly applies the `between-bytes-timeout`
      (= `budget.duration` per §3.3.4) to both reading from origin **and** writing to
      origin — see Fastly's request-timeout docs. So a chunk write that the host
      cannot drain in time errors at the host bound rather than blocking
      indefinitely. **BoundedCooperative** for the write phase, with overshoot ≤ one
      between-bytes-timeout interval. (Treat this as the documented contract; if a
      future host change relaxes that bound, the spec and capability text would
      downgrade.)
    - *Around each chunk*: the adapter checks `budget.deadline.is_expired()` at
      **two** points per iteration — (i) immediately after `stream.next().await`
      returns and **before** `write_all`, so a `stream.next()` that stalled past
      the deadline and *then* finally yielded cannot still write the chunk it just
      produced; and (ii) after the successful `write_all` / `flush()`, so a write
      that pushed the budget over is caught before the next pull. On expiry at
      either point the `StreamingBody` is dropped without `finish()` and the slot
      returns `gateway_timeout`.

    Net: the capability matrix entry `streamed-upload-deadlines = BestEffort` for
    Fastly reflects the worst phase (source-stream yield). The risk section (§8)
    spells out the two-phase decomposition so apps don't assume the BoundedCooperative
    write-side bound covers source stalls.
  - **Response phase: host timeouts are *not* adjustable mid-flight.** The Fastly
    SDK sets connect / first-byte / between-bytes timeouts once before `send_async`
    (§3.3.4) and does not expose post-dispatch mutation. For
    `send_async_streaming`, dispatch happens **before** chunks are fed, so the
    response-phase host timeouts are locked to the phase-split values computed at
    dispatch (`first_byte_ms` for the headers wait, `between_ms` for inter-chunk
    gaps once the response body flows). After the upload `finish()`es the adapter
    checks `budget.deadline.remaining()` cooperatively before calling `wait()` —
    if `None`, drop the `PendingRequest` and return `gateway_timeout` without
    waiting. But if the remaining budget is, say, 10 ms while the host's
    `first_byte_timeout` was set to 150 ms at dispatch (3/4 of a 200 ms budget),
    the host will wait up to its own 150 ms for headers even though only 10 ms
    of batch budget is left; once headers arrive, the response-body phase is
    likewise bounded by `between_bytes_timeout` per inter-chunk gap (200 ms in
    this example). **Total wall-clock for a single streamed upload + response
    on Fastly can therefore exceed `budget.duration` by up to one
    first-byte-timeout (for the headers wait) plus one between-bytes-timeout
    per body-chunk gap** — the same `BoundedCooperative` overshoot bounds as
    elsewhere (§3.3.4). This is a deliberate, documented Fastly-specific
    behaviour of streamed uploads: apps that need tight end-to-end wall-clock
    should pass a buffered request body (`Body::Once`) so the timeouts are set
    with the full budget known and no upload-time eating happens.
- `capability()` per §3.5.2: `outbound-http` = `Native`, `outbound-deadlines` =
  `BoundedCooperative` (footnote 1 — covers single `send`, plus `send_all` headers
  phase AND active body-drain phase per slot; cross-slot harvest-order delay is
  the separate `send-all-slot-isolation` story),
  `outbound-flexible-phase-budget` = `BestEffort` (footnote 5 — rigid 1/4 connect +
  3/4 first-byte split per §4.3 can fail a request that would have fit within the
  total budget), `send-all-slot-isolation` = `BestEffort` (footnote 4 —
  buffered-body harvest order can produce false 504s),
  `streamed-upload-deadlines` = `BestEffort` (footnote 2 — no preemption of a
  stalled `stream.next().await`), `lazy-streamed-response-passthrough` = `Native`
  (Fastly's WASM guest accepts non-Send streams via `Response::with_streaming_body`),
  `config-store` / `kv-store` / `secret-store` = `Native`. **Nine** capabilities
  total. This is the exact tuple `Adapter::capability()` returns on Fastly.

**Streamed-response wrapping.** Even without a guest async timer, the Fastly adapter
wraps streamed response bodies with a **cooperative deadline-aware stream**. Each
`Stream::next` checks `budget.deadline.is_expired()` **both before issuing the
underlying body read and again after it returns** (including the read that
discovers EOF and would otherwise complete the stream cleanly). On expiry at
either check it yields `Err(EdgeError::gateway_timeout(..))` instead of `Ok(chunk)`
or stream-end. This applies to *every* consumer of the wrapped body —
`into_bytes_bounded`, `into_bytes_bounded_until`, `into_response()` proxy
passthrough — so the deadline cannot be bypassed by choosing a non-helper
consumption path or by riding the final blocking read to EOF. Bounded-cooperative
semantics apply: a chunk gap (including the gap before EOF) is bounded by the
host's `between-bytes-timeout` (set to `budget.duration` at dispatch), so per-gap
overshoot ≤ one between-bytes-timeout interval.

**Limitation, stated explicitly.** The harvest loop blocks the single-threaded guest in
`wait()`. This is correct and concurrent (all requests progress at the host in parallel),
but the guest cannot do other work while blocked — the intended behaviour for a fan-out batch.
`wait()` parks efficiently; there is no busy-polling.

**Service prerequisite — dynamic backends.** Fastly outbound HTTP to arbitrary hosts
requires **dynamic backends to be enabled on the Fastly service**. That is a
deployment-time service configuration, not adapter code, and the adapter itself cannot
turn it on. EdgeZero handles the gap as:

1. **Build / deploy:** `ensure_capabilities` emits an informational log when Fastly is
   the target adapter and `outbound-http` is required, reminding the operator to enable
   dynamic backends on the service. EdgeZero deliberately does not pull in the Fastly
   management API to validate this from the CLI.
2. **Runtime:** if dispatch fails because dynamic backends are disabled, the adapter
   surfaces `EdgeError::bad_gateway("Fastly dynamic backends are not enabled on this
   service; enable them in the service configuration")`. Apps see a clear 502 with a
   diagnostic that points at the fix.

So Fastly's static `outbound-http = Native` describes **adapter** support; achieving
runtime success additionally requires the service-side toggle. The capability matrix is
a static contract over adapter behaviour, not a runtime health guarantee for a deployed
service — this distinction is explicit so a green capability check is not misread.



### 4.4 Spin — `crates/edgezero-adapter-spin`

- `SpinProxyClient` → `SpinOutboundClient` (stays stateless).
- `send_all` first runs a **preflight** per slot: any request with `Body::Stream`
  OR `response_mode = Streamed` is converted to `Err(EdgeError::bad_request(..))`
  per §3.1.1 *before* `send_one` is invoked. **`send_all` snapshots `let batch_now =
  web_time::Instant::now()` once** before fanning out and passes it to every
  `send_one(req, batch_now)`. Buffered-mode buffered-body survivors are fanned out
  via `join_all` over `send_one` (`spin_sdk::http::send`); the wasi async reactor
  fans out. Concurrency materialises only under the real Spin/wasi executor — see
  §5.3 for the test consequence.
- `send_one(req, now)`: build a `spin_sdk::http::Request`; compute the budget via the
  core helper `dispatch_budget(req, now)` (§3.3.2); race the **whole** operation
  (send **and**, in `Buffered` mode, body collect) against a wasi monotonic-clock
  timer for **`budget.deadline.remaining()` at the moment the race starts** —
  *not* the snapshot-time `budget.duration`. The two differ by however long
  preflight + builder construction took since `batch_now`; using `remaining()`
  pins the SDK timer to the absolute batch deadline, matching Axum/CF (§4.1 /
  §4.2 step 3). If `remaining()` is `None`, return `gateway_timeout` without
  issuing the request. Single `send` snapshots `now = web_time::Instant::now()`
  inline.
- **Streamed responses honour the effective-budget deadline.** Wrap the response body as
  `Body::Stream`, with a per-chunk race against a wasi monotonic-clock timer bounded
  by `budget.deadline`; the wrapper yields a `gateway_timeout` error chunk past the
  deadline so the streamed body honours the deadline end-to-end per §3.3.3.
- **Streamed request bodies.** Spin/WASI outgoing-body supports streamed writes; the
  adapter feeds chunks from `Body::Stream` to the WASI outgoing-body up to
  `req.max_request_body_bytes` (default 8 MiB), with pre-append checked accounting and
  `bad_request` on overflow. **Two distinct races bound the upload** so
  `streamed-upload-deadlines = Native` is real on Spin (not just claimed):
  1. *Source-pull race*: `futures::select!` between `source_stream.next()` and a
     wasi monotonic-clock timer for `budget.deadline.remaining()`. A
     `stream.next().await` that never yields is preempted at the deadline and the
     slot returns `gateway_timeout` — this is what makes Spin Native for the
     capability (vs. Fastly BestEffort, which cannot preempt this path, §4.3).
  2. *Host-write race*: each `OutgoingBody::write` host call is similarly raced
     against a wasi timer for the remaining deadline, so a slow host accepting
     bytes also surfaces as `gateway_timeout` at the deadline, not unbounded.
  After upload completion the adapter calls `budget.deadline.remaining()`; if
  `None`, the outgoing handle is dropped and the slot returns `gateway_timeout`
  immediately — no response wait. Otherwise the remaining duration governs the
  response race, so upload time is included in the batch budget rather than
  added on top.
- Existing gzip/br decompression is kept; decompressed-byte cap enforced incrementally
  (§3.4.1). `Streamed` mode wraps the response body as `Body::Stream`.
- Spin requires `allowed_outbound_hosts`; the adapter renders it from
  `[capabilities.outbound].hosts` per §3.5.4 when generating `spin.toml`.
- `capability()` per §3.5.2: `Native` for **all nine** capabilities. Spin's wasi
  monotonic-clock timer covers `outbound-deadlines` and `streamed-upload-deadlines`;
  the single wasi-timer race is one total budget (no per-phase split), so
  `outbound-flexible-phase-budget` is `Native` too; the WASI outgoing-body sink
  accepts a non-Send stream so `lazy-streamed-response-passthrough` is `Native`;
  and `join_all` of `spin_sdk::http::send` futures fans out body drains
  concurrently so `send-all-slot-isolation` is `Native`. `config-store` / `kv-store`
  / `secret-store` are `Native` for Spin too.

## 5. Test plan

CLAUDE.md forbids tests needing a network connection or platform credentials. "Network"
means the public internet — a **locally spawned mock origin** is allowed and is how
concurrency and timing are proven. Tests are tiered.

### 5.1 Tier 1 — core contract suite

Location: `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]`, plus a
`MockOutboundClient` exposed behind the existing `test-utils` feature. Runs on native and
wasm targets; async tests use `futures::executor::block_on`.

`MockOutboundClient` is scripted per request: status, headers, body, byte size, simulated
failure, simulated latency, and compressed-payload simulation. It validates the **shared**
logic — `send_all` aggregation, index alignment, `send_all(vec![])` → `vec![]`,
partial-failure isolation, deadline cutoff, decompressed-byte cap, error mapping, non-2xx
passthrough, URI validation, fallible header construction.

### 5.2 Tier 2 — per-adapter translation tests

Location: `tests/contract.rs` in each adapter crate (**created for Axum**; extended for
the other three). No network. Covers request→platform and platform→response conversion,
header preservation, non-2xx mapping, buffered vs. streamed body handling, and
compressed-body decompression, using each adapter's existing harness (`#[tokio::test]`,
`#[wasm_bindgen_test]`, `block_on`).

### 5.3 Tier 3 — per-adapter live behaviour

Proves real fan-out and timing against a locally spawned mock origin.

- **Axum** — implemented now. A `tokio` mock server with configurable per-route delay,
  body size, compression, and chunk pacing.
- **Fastly** — a Viceroy-run test with a backend pointed at the local mock origin.
- **Cloudflare** — a `workerd`/miniflare integration test against the local mock origin.
- **Spin** — a `spin`-runtime test against the local mock origin; the only place Spin's
  `join_all` concurrency runs under the real wasi executor (bare `block_on` will not fan
  out).

Each wasm Tier 3 test is a dedicated CI job. Axum's lands with the implementation; the
three runtime-backed jobs land as the matching runtimes are wired into CI. Until then,
that adapter's behaviour is still covered by Tier 1 (logic) and Tier 2 (translation); the
gap is the live wall-clock/timing proof only, and it is tracked, not silently skipped.

Reference concurrency assertion (Axum):

```rust
#[tokio::test]
async fn send_all_runs_requests_concurrently() {
    let server = MockServer::start_with_delay(Duration::from_millis(200)).await;
    let client = AxumOutboundClient::try_new().unwrap();
    let reqs: Vec<_> = (0..10)
        .map(|_| OutboundRequest::get(server.url("/")).unwrap())
        .collect();

    let start = web_time::Instant::now();
    let results = client.send_all(reqs).await;
    let elapsed = start.elapsed();

    assert!(results.iter().all(Result::is_ok));
    assert!(elapsed < Duration::from_millis(800), "fan-out not concurrent: {elapsed:?}");
}
```

### 5.4 Required test cases → tiers

| Test case | Tier 1 | Tier 2 | Tier 3 |
| --- | --- | --- | --- |
| One outbound request | yes | yes | — |
| Many concurrent outbound requests (wall-clock ≪ sum) | aggregation | — | yes |
| Empty `send_all(vec![])` → empty vec | yes | — | — |
| Response body buffering (`Buffered` mode) | yes | yes | — |
| Streamed response body passthrough (`Streamed` mode) | yes | yes | yes |
| Max response size exceeded → 502 | yes | yes | — |
| Compressed body expands past cap → 502 (decompressed count) | yes | yes | yes |
| Slow streaming body vs. deadline (bounded overshoot) | — | — | yes |
| Headers arrive, deadline expires during body buffering → 504 | — | — | yes |
| Per-request timeout / batch deadline exceeded → 504 | logic | — | yes |
| Partial timeout: one slot 504s, other slots still `Ok` | yes | — | yes |
| Headers preserved (request and response) | yes | yes | — |
| Non-2xx returned as `Ok`, not a transport error | yes | yes | — |
| Invalid outbound URI rejected → 400 | yes | — | — |
| Fallible header construction surfaces `EdgeError` | yes | — | — |
| Streamed request body in `send_all` → per-slot `bad_request` (400) | yes | yes | — |
| Streamed request body in `send` (proxy-forward) succeeds | yes | yes | yes |
| `send(buffered_req)` ≡ `send_all(vec![buffered_req]).pop()` — equivalence over status, headers, body cap, deadline classification, decompression, error mapping | yes | yes | — |
| 3xx upstream response delivered as `Ok` with `Location` (no auto-follow) | yes | yes | yes |
| Non-UTF-8 outbound request header rejected at construction → 400 | yes | — | — |
| Non-UTF-8 upstream response header value dropped with `warn!` diagnostic, **valid sibling values preserved** (multi-value `set-cookie` with one invalid duplicate keeps every valid entry) | yes | yes | — |
| `OutboundRequest::header(name, "café")` (valid non-ASCII UTF-8) succeeds — builder uses `HeaderValue::from_bytes`, not `from_str` | yes | yes | — |
| `OutboundRequest::header(name, "foo\nbar")` and `header(name, "x\0y")` (valid UTF-8 strings with HTTP-forbidden control bytes) → `bad_request("header value contains forbidden bytes: <name>")`. Tests both header-injection vectors (newline / null) explicitly | yes | yes | — |
| `OutboundResponse::into_bytes_bounded_until` (streamed) — **helper-cooperative half (Tier 1):** the helper's `is_expired()` check fires before/after each underlying read against a `MockOutboundClient` stream that simulates a slow source; once `until_deadline` is expired and the next yield boundary is hit, the helper returns 504. Asserts cooperative-only contract per §3.1.4 — no wrapper insertion, no platform timer | yes | — | — |
| `OutboundResponse::into_bytes_bounded_until` (streamed) — **adapter wrapper half (Tier 2 / Tier 3):** the deadline-aware wrapper the adapter installs at response construction time (Axum tokio / CF `worker::Delay` / Spin wasi monotonic-clock / Fastly bounded-cooperative between-bytes-timeout) returns a `gateway_timeout` error chunk past `dispatch_budget(req).deadline` in real time, so a slow source preempts via the wrapper rather than the helper. Asserts wrapper insertion at the response-conversion boundary in each adapter crate | — | yes | yes |
| Streamed body stalls after one chunk; deadline expires → wrapped stream yields error chunk on Axum/CF/Spin; bounded overshoot on Fastly. **Adapter-specific** — the wrapper insertion and platform timer behaviour live in each adapter's response converter; Tier 1's `MockOutboundClient` has no wrapper layer to test. The corresponding cross-adapter contract (helper returns 504 on stall, slot index preserved) is covered by the helper-cooperative row above | — | yes | yes |
| `normalize_for_dispatch` strips `host`, `content-length`, `transfer-encoding`, hop-by-hop on a `headers_mut()`-built request | yes | yes | — |
| Multi-value response headers preserved (e.g. duplicate `set-cookie`) | yes | yes | yes |
| Multi-value outbound request headers preserved on the wire | yes | yes | yes |
| Inbound body: adapter exposes `Body::Stream`; `body_bytes(max)` drains and caches; second call returns clone without re-reading | yes | yes | — |
| Required `BestEffort` capability → **every adapter-selecting CLI command** (`edgezero build`, `edgezero serve`, `edgezero deploy`, `edgezero auth login` / `logout` / `status`, `edgezero provision`, `edgezero config push` / `config validate`, `edgezero demo`) exits non-zero with a clear message — matches the §3.5.3 enforcement set (PR #269: pre-dispatch gate inside `execute(..)` for `build`/`serve`/`deploy`/`auth`, plus sibling gates at the top of `run_provision`, `run_config_push`, `run_config_validate`, and `run_demo`). `edgezero dev` is gone; `demo` is its contributor-only replacement | yes | — | — |
| Axum response converter mapping for a wrapped streamed body: `Err(GatewayTimeout)` chunk during buffered drain → axum response **504**; `Err(BadGateway)` chunk → **502**; over-cap → **502**; `Ok` chunks under cap append normally. The buffering boundary lets Axum preserve the correct status code (no silent coalesce to 502) | — | yes | yes |
| `OutboundRequest::into_parts` / `OutboundResponse::new` / `OutboundResponse::into_parts` round-trip every field (adapter API completeness) | yes | yes | — |
| `body_bytes` cap exceeded → subsequent `body_bytes` / `json_within` / `form_within` calls return the same stored error (poison semantics); `into_request()` returns `Err(stored_err)` (per §3.4.5 round-18 / round-19 — **not** an empty body) | yes | yes | — |
| `into_request()` after middleware buffered body yields `Body::Once(cached)` (proxy-forward still works) | yes | yes | yes |
| Multi-value `set-cookie` round-trips through every adapter's response path (`get_header_all` on Fastly; not `get`) | — | yes | yes |
| Multi-value outbound request header round-trips through every adapter's request path (`append_header` on Fastly; `Headers::append` on CF; WASI `fields` on Spin) | — | yes | yes |
| `DEFAULT_NO_DEADLINE_BUDGET` core constant (Tier 1): `dispatch_budget(no-deadline-no-timeout-request, now)` returns `DispatchBudget { duration: 30 s, deadline: now + 30 s }` per §3.3.2 table. Pure core-logic assertion on the helper, no adapter | yes | — | — |
| Axum no-deadline request budgeted at 30 s end-to-end (Tier 2 / Tier 3): with a real Axum dev server + mock origin, a request without `timeout`/`deadline` actually times out at 30 s via the adapter's wrapper. Adapter-specific wall-clock behaviour | — | yes | yes |
| `OutboundResponse::json_bounded(max)` / `json_bounded_until(max, deadline)` on a streamed body — **helper-cooperative half (Tier 1):** the helpers delegate to `into_bytes_bounded` / `into_bytes_bounded_until` then `serde_json::from_slice`; mock-driven test asserts the helper's cap + cooperative `until_deadline` check + malformed-JSON → 502 mapping. No wrapper insertion | yes | — | — |
| `OutboundResponse::json_bounded_until(max, deadline)` adapter-wrapper half (Tier 2 / Tier 3): the wrapper installed at response construction enforces `dispatch_budget(req).deadline` in real time on Axum / CF / Spin; the caller-supplied `deadline` argument is cooperative only (§3.1.4). Asserts wrapper insertion preserves the JSON outcome | — | yes | yes |
| Streamed body honours `dispatch_budget(req).deadline` end-to-end on Axum/CF/Spin via wrapped stream (including the no-`req.deadline` synthetic-30 s case); bounded-cooperative on Fastly. **Adapter-specific** — the wrapper is installed per-adapter at response-conversion time; Tier 1's mock has no wrapper layer. The cross-adapter contract (`EdgeError::gateway_timeout` chunk past the deadline) is the same row as the cooperative `into_bytes_bounded_until` Tier 1 assertion | — | yes | yes |
| `BodyState::Draining`: drain future dropped mid-flight → cell becomes `Poisoned(cancelled)`; next `body_bytes` returns the stored cancelled error | yes | yes | — |
| Reentrant `body_bytes` while `Draining` returns `Err(EdgeError::internal(..))` without panic | yes | — | — |
| Pre-append cap accounting: a single oversized chunk on a small cap errors **without extending the collected buffer past `max`** (the in-flight chunk briefly co-exists with the buffer during the overflow check, per §3.4.1 / §3.4.4 — the test asserts the *persistent* buffer never grows past `max`, not that the in-flight `current_chunk` is never received). Inbound and outbound bounded drains both covered | yes | yes | — |
| `Form` / `ValidatedForm` migrated to `form_within(DEFAULT_INBOUND_FORM_BYTES = 1 MiB)`; over-cap → 400 | yes | yes | — |
| Adapter `dispatch_budget(req)` everywhere: each adapter calls the core `dispatch_budget(req, now)` helper and threads the resulting `DispatchBudget` to its platform timer. The **core helper** is Tier 1 (covered by the row above); the "every adapter actually calls it" assertion is Tier 2 (contract crate inspects the call site) / Tier 3 (real runtime observes the 30 s cap) | — | yes | yes |
| `.timeout(short).deadline(long)` honours the *shorter* effective — **dispatch_budget classification (Tier 1):** the core helper returns `DispatchBudget { duration: short, deadline: now + short }`. Mock-driven test asserts the classification | yes | — | — |
| `.timeout(short).deadline(long)` honours the *shorter* effective deadline end-to-end (streamed body returns 504 at `now + short`, not `now + long`) — **adapter wrapper (Tier 2 / Tier 3):** wrapper armed with `budget.duration` actually fires at `now + short` against a real platform timer | — | yes | yes |
| Streamed request body over `max_request_body_bytes` → per-slot `bad_request` (400) on every adapter | yes | yes | — |
| Stalled streamed-request-body upload, mechanics differ per adapter — this row is **Tier 2/3 only** because Tier 1's `MockOutboundClient` cannot prove the Axum tokio / Cloudflare `worker::Delay` / Spin wasi / Fastly host-timer behaviour; Tier 1 covers the cross-adapter *contract* (504 on stall, index alignment) via the mock, marked separately. **Axum / Cloudflare** drain `Body::Stream` into `Bytes` **before** constructing the platform request (§4.1 / §4.2), so the relevant stall is the *source-pull* during the drain — tokio / `worker::Delay` races it against `budget.deadline` and returns 504 at the deadline (no separate "host-write" race because by the time the SDK request is constructed the body is already in hand). **Spin** has both phases explicit: source-pull race via wasi monotonic-clock and host-write race over WASI outgoing-body chunk-writes, both bounded by `budget.deadline`, each returning 504 at the deadline (§4.4). **Fastly** has a single phase where source-pull cannot be preempted (BestEffort per `streamed-upload-deadlines`); once chunks are flowing, the host's `between-bytes-timeout` (= `fastly_timeout_ms(budget)`) bounds each gap, so the slot returns 504 **within one between-bytes-timeout past `budget.deadline`** — bounded overshoot per the `BoundedCooperative` rubric, not exact `budget.deadline` (§3.3.4). Test asserts per-adapter mechanics | — | yes | yes |
| Stalled streamed-request-body upload **contract only** (Tier 1, via `MockOutboundClient` with scripted stalls): on the **preemptible-source** adapters (Axum / Cloudflare / Spin) a stalled upload returns `Err(EdgeError::gateway_timeout(..))` to the caller within the configured deadline, slot index alignment is preserved, and other slots are unaffected. **Fastly is excluded from the "within the configured deadline" half of this contract** because `streamed-upload-deadlines` is `BestEffort` on Fastly (§3.5.1 / §3.5.2): a source-pull stall (`stream.next().await` that never yields) is unbounded on Fastly per §4.3, so Tier 1 cannot assert wall-clock containment there. Fastly still observes the index-alignment + partial-failure-isolation half of the contract. The `MockOutboundClient` sets the adapter under test on the mock so this row's Fastly invocation skips the wall-clock assertion and runs only the structural assertions. Mechanics-level wall-clock assertions for all four adapters (including Fastly's `BoundedCooperative` between-chunk bound) live in the Tier 2/3 row above | yes | — | — |
| `body_bytes` / `json_within` / `form_within` after `take_body()` → `internal("body already consumed via take_body")` (no body resurrection) | yes | — | — |
| Valid non-ASCII UTF-8 header (e.g. `x-app-display-name: café`) round-trips through every adapter on request and response | yes | yes | yes |
| Header containing a `\x80` byte is rejected on outbound request (400) and dropped on inbound-of-outbound response with a `warn!` naming the header | yes | yes | — |
| RFC 7230 hop-by-hop strip removes `trailer` (singular) end-to-end; an inbound `trailer: foo` never reaches the outbound wire | yes | yes | — |
| Fastly `send` with `Body::Stream` request body: over `max_request_body_bytes` mid-upload → 400; stalled upload **between** yielded chunks → 504 **within one between-bytes-timeout past `budget.deadline`** (bounded overshoot — `BoundedCooperative`, not exact deadline); stalled `stream.next()` is the documented BestEffort gap on Fastly (no preemption); upload time reduces remaining budget for response. **Adapter-specific mechanics (host between-bytes-timeout, source-pull non-preemption) live in Tier 2 / Tier 3 only** — Tier 1's `MockOutboundClient` cannot reproduce Fastly's host timers | — | yes | yes |
| `dispatch_budget(req)` table: every row of §3.3.2 holds (timeout-only, deadline-only, both, expired, zero-effective, no-deadline-no-timeout) | yes | — | — |
| Fastly `send_all` with mixed budgets, **headers phase**: short-budget slot's *headers* result reflects its own budget (host enforces independently); but its wall-clock-observed *delivery* can be delayed behind an earlier `wait()` (harvest order). **Adapter-specific** — harvest order and per-slot host-timer behaviour belong to Tier 2 (Fastly contract crate) and Tier 3 (Viceroy) | — | yes | yes |
| Fastly `send_all` Buffered mode, **body phase**: a slot whose own `budget.deadline` would have covered its body in isolation can still return `gateway_timeout` because an earlier slot's body drain monopolised harvest. The contract explicitly admits these harvest-order-induced 504s on Fastly Buffered. **Adapter-specific harvest mechanics** — Tier 1's mock has no harvest queue and cannot reproduce the head-of-line block; covered by Tier 2 (deterministic harvest ordering against a host-side fake) and Tier 3 (Viceroy wall-clock) | — | yes | yes |
| `[capabilities] required = ["send-all-slot-isolation"]` on a Fastly target → **every adapter-selecting CLI command** (`build` / `serve` / `deploy` / `auth` / `provision` / `config push` / `config validate` / `demo`) exits non-zero with the BestEffort + required hard-fail message via the §3.5.3 pre-dispatch gates (one inside `execute(..)`, siblings on `run_provision` / `run_config_*` / `run_demo`, PR #269); same manifest on Axum/CF/Spin passes | yes | — | — |
| Fastly mixed-budget `send_all` to the **same host**: slots with `50 ms` and `3 s` budgets create **distinct** dynamic backends (identity tuple includes `budget_ms`); the 50 ms slot's host timeout is not silently inherited by the 3 s slot or vice versa. **Asserts the Fastly identity tuple** — Tier 1's mock has no dynamic-backend abstraction; Tier 2 (Fastly contract crate) inspects the registered-backend map and Tier 3 (Viceroy) observes the wall-clock divergence | — | yes | yes |
| `RequestContext::into_request()` after `body_bytes` poison: returns `Err(stored_err)`, not `Ok(Request<Body::empty()>)` — a permissive proxy-forward cannot mask a stricter middleware's poisoned read | yes | — | — |
| Fastly + `outbound-http = required`: `ensure_capabilities` emits the dynamic-backends informational log | yes | — | — |
| Fastly `Backend::builder().finish()` returns a non-`NameInUse` error (dynamic backends disabled on the service; DNS resolution failure; TLS misconfiguration; any other Fastly-side rejection reaching the guest): adapter maps to **`EdgeError::bad_gateway(..)` (502)**, NOT `internal`. Tests cover each branch via a host-side fake / Viceroy harness | — | yes | yes |
| Fastly `EdgeError::internal` is reserved for **adapter contract bugs only** — not service/backend setup failures. The test inspects the error chain for each Fastly `Err` and asserts that `internal` appears only for: (a) `BATCH_DISPATCH_SLACK_MAX` overshoot, (b) `NameInUse` external-registration collision, (c) the unfilled-slot harvest invariant. Every other Fastly error path is `bad_gateway`, `gateway_timeout`, or `bad_request` | — | yes | yes |
| `Deadline::after(Duration::MAX)` clamps to `DEADLINE_FAR_FUTURE = 7 days` (round 24, down from 365 d to stay under Fastly's u32-ms ceiling); subsequent `dispatch_budget` round-trip still produces a usable budget; no panic | yes | — | — |
| Inbound body `form_within(max)` over-cap → 400; cache + poison behaviour identical to `body_bytes` / `json_within` | yes | yes | — |
| Required `streamed-upload-deadlines` on Fastly → hard build failure (BestEffort + required, per §3.5.3) | yes | — | — |
| Upload consumes the budget — **contract shape (Tier 1, Axum / Cloudflare semantics only):** the cross-adapter contract that `budget.deadline.remaining()` is consulted after the upload drain completes, and that `None` returns `gateway_timeout` *without* dispatching the platform request, is asserted against `MockOutboundClient` configured in **drain-first** mode (the Axum / Cloudflare shape — drain into `Bytes` first, then dispatch). The mock exposes a `did_dispatch()` flag and the assertion is "deadline expired during drain → 504 returned AND `did_dispatch() == false`." **This row covers Axum / Cloudflare only**; Spin and Fastly are explicitly excluded because their adapters dispatch concurrently with (or before) the upload drain and the §3.1.1 contract documents partial upstream sends as possible / expected on those adapters — see the per-adapter Tier 2 / Tier 3 rows below. The mock's drain-first mode is a property of the test harness, not a cross-adapter contract; the Tier 1 row asserts only what the Axum / Cloudflare adapters guarantee | yes | — | — |
| Upload consumes the budget on **Axum** / **Cloudflare** — **adapter mechanics (Tier 2 / Tier 3):** the adapter drains the streamed request body into `Bytes` *before* constructing the platform request, so `budget.deadline.remaining() == None` after the drain → adapter returns `gateway_timeout` **before** constructing/sending the actual `reqwest`/`worker` request. No partial upstream send. Asserted via `crates/edgezero-adapter-{axum,cloudflare}/tests/contract.rs` (Tier 2: inspect the platform-SDK send-call counter on a fake / no-network harness) + Tier 3 against a mock origin (the origin observes zero connections from the timed-out slot) | — | yes | yes |
| Upload consumes the budget on **Spin** — **adapter mechanics (Tier 2 / Tier 3):** the adapter feeds chunks to the WASI outgoing-body; after the upload completes, `budget.deadline.remaining()` is checked. If exhausted, the response future is dropped → `gateway_timeout`. **Partial upstream send is possible** because chunks were flowing — distinct from Axum / Cloudflare. Asserted via the Spin contract crate (Tier 2: WASI outgoing-body chunk-count observation) + Tier 3 against a mock origin under the real Spin runtime (origin observes the partial upload) | — | yes | yes |
| Upload consumes the budget on **Fastly** (`send_async_streaming`): dispatch happens **before** chunks flow, so request bytes have already started reaching the upstream by the time the budget is exhausted. Adapter detects `budget.deadline.remaining() == None`, drops the `StreamingBody` and `PendingRequest` without `wait()`, and returns `gateway_timeout`. **Partial upstream send is expected** — the documented Fastly-specific limitation of streamed uploads. The test asserts this contract honestly. **Adapter-specific** — the `send_async_streaming` + `wait()`-drop sequence is Fastly SDK behaviour Tier 1's mock has no analogue for; covered by Tier 2 (Fastly contract crate) and Tier 3 (Viceroy) | — | yes | yes |
| `batch_deadline = Deadline::after(batch_deadline_ms)` computed once and copied into every target request → all targets share one absolute wall-clock cap (no drift); recomputing `Deadline::after(batch_deadline_ms)` per target would let later targets drift past the batch deadline (counter-example test) | yes | — | yes |
| Outbound request header from `headers_mut()` containing a non-UTF-8 value is **dropped with `warn!`** by `normalize_for_dispatch` (lossy proxy-forward path) — distinct from `header(..)` which **rejects** with 400 (loud construction path) | yes | yes | — |
| Adapter response-out converter (`response.rs`) on CF/Fastly/Spin: `OutboundResponse::into_response()` with a streamed body yields first bytes before the upstream stream ends (no buffer-then-return); driven by a `MockOutboundClient`-fed stream in-process, no platform runtime needed | — | yes | yes |
| Adapter response-out converter on CF/Fastly/Spin: stream errors after headers **abort the downstream response stream** — once headers have been written, HTTP cannot change status to 502/504, so the adapter aborts the chunked body (TCP close on HTTP/1.1, RST_STREAM on HTTP/2) and emits a `log::warn!` naming the originating `EdgeError` variant (`gateway_timeout` or `bad_gateway`). Clients observe an early connection close, not a synthetic 502/504. The originating EdgeError is in the server log | — | yes | yes |
| Adapter response-out converter on Axum: streamed body is buffered to `Bytes` within `AXUM_RESPONSE_STREAM_BUFFER_BYTES` (16 MiB, documented Axum-specific limitation) — first bytes only flow after full collection; over-cap → 502 | — | yes | yes |
| `Deadline::after(d)` and `dispatch_budget`'s `saturating(d)` clamp at `DEADLINE_FAR_FUTURE` (7 d) — `Duration::MAX` does not panic, never produces an `Instant` past the clamp, and `fastly_timeout_ms` of the clamped value fits within Fastly's `u32` ms ceiling without rejection | yes | yes | — |
| `OutboundRequest::is_stream_body()` returns `true` for `Body::Stream` requests and `false` for `Body::Once`; `send_all` preflight uses this to reject without consuming | yes | — | — |
| `OutboundRequest::is_stream_response()` returns `true` for `stream_response()`-marked requests; `send_all` preflight uses this to reject with `bad_request` without consuming, on every adapter | yes | yes | — |
| `send_all` with `stream_response()` returns per-slot `bad_request` (400) on every adapter; single `send` with the same request succeeds (streamed bodies are only valid via `send`) | yes | yes | — |
| `[capabilities.outbound].hosts` validation: rejected — empty string, `ftp://x` (bad scheme), `https://` (missing authority), `https://u:p@x` (userinfo), `https://x/p` (path), `https://x?q` (query), `https://x#f` (fragment), `https://x:0` and `https://x:70000` (out-of-range port), `https://x:abc` (non-numeric port). Accepted — `"*"`, `"*.example.com"`, `"x:8443"`, `"https://[::1]"`, `["*", "api.example.com"]`. Manifest load surfaces every error before the build | yes | — | — |
| `send_all` shared-`now` snapshot: a homogeneous-budget Fastly fan-out batch to one host creates **exactly one** dynamic backend (per the §4.3 identity guarantee); replacing `batch_now` with per-slot `Instant::now()` in a test fork creates distinct backends, catching the drift bug. **Asserts Fastly-specific identity tuple including `budget_ms`** — Tier 1's `MockOutboundClient` has no dynamic-backend abstraction, so this row is Tier 2 (Fastly contract crate) + Tier 3 (Viceroy) only | — | yes | yes |
| Outbound `Host` header includes the explicit port for non-default-port URIs: `http://localhost:3000` → `Host: localhost:3000`; `https://example.com:8443` → `Host: example.com:8443`; `https://example.com` → `Host: example.com` (no port). Adapters never copy `host` from the inbound `req.headers()` | yes | yes | yes |
| **Core URI canonicalization → four-value split (Tier 1 half).** The four accessors `backend_target()` / `host_authority()` / `sni_hostname()` / `cert_host()` are tested in `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]` against a matrix of inputs, with per-scheme expectations (no adapter dependency). **HTTPS DNS-host inputs** (`https://example.com`, `https://example.com:443`, `https://example.com:8443`): `backend_target() == "example.com:443"` / `"example.com:443"` / `"example.com:8443"`; `host_authority() == "example.com"` / `"example.com"` / `"example.com:8443"`; `sni_hostname() == Some("example.com")` on all three; `cert_host() == Some("example.com")` on all three. **HTTPS IP-literal inputs** (`https://127.0.0.1`, `https://[::1]:8443`): `sni_hostname() == None` (RFC 6066 §3); `cert_host() == Some("127.0.0.1")` / `Some("::1")` (bracket-stripped). **HTTP DNS-host inputs** (`http://example.com`, `http://example.com:80`, `http://example.com:8443`): `backend_target() == "example.com:80"` / `"example.com:80"` / `"example.com:8443"`; `host_authority() == "example.com"` / `"example.com"` / `"example.com:8443"`; `sni_hostname() == None` (no TLS, no SNI); `cert_host() == None` (no TLS, no certificate). The HTTPS-only `cert_host()` `Some` is the canonical reason an adapter calls `.disable_ssl()` vs `.enable_ssl()` / `.check_certificate(..)`. This is the core-side guarantee the Fastly row below assumes | yes | — | — |
| **Fastly adapter consumes the four canonical accessors, DNS-name HTTPS path (Tier 2 / Tier 3 half).** For a DNS-name HTTPS host where `req.sni_hostname()` returns `Some(sni)` and `req.cert_host()` returns `Some(cert)`, Fastly dynamic backend construction calls `Backend::builder(name, req.backend_target()).override_host(req.host_authority()).sni_hostname(sni).check_certificate(cert)` (with `sni == cert` because both accessors return the same host string for the DNS-name case). For HTTP (`req.cert_host()` returns `None`), it calls `Backend::builder(name, req.backend_target()).override_host(req.host_authority()).disable_ssl()`. A Tier 2 test (`crates/edgezero-adapter-fastly/tests/contract.rs`, no network — inspects the registered-backend map produced by `FastlyOutboundClient`) and a Tier 3 test (Viceroy round-trip) build `https://example.com:8443` and `http://example.com:8443` and assert: connection target = `example.com:8443` on both; Host = `example.com:8443` on both; SSL enabled with SNI = cert = `example.com` on the first, disabled on the second; identity hashes differ (distinct backends). **DNS-name HTTPS only** — IP-literal HTTPS (where `sni_hostname()` is `None` but `cert_host()` is `Some(ip)`) is the dedicated "Fastly HTTPS to IP literals" row below, which asserts the **distinct** behaviour of skipping `.sni_hostname(..)` while still passing `cert_host()` to `.check_certificate(..)`. **Adapter-specific** — Tier 1's mock has no `Backend::builder` analogue | — | yes | yes |
| URI canonicalization — **core accessor half (Tier 1):** `OutboundRequest::get("https://example.com")` and `OutboundRequest::get("https://example.com:443")` produce identical `backend_target()` / `host_authority()` / `cert_host()` / `sni_hostname()` outputs (`"example.com:443"`, `"example.com"`, `Some("example.com")`, `Some("example.com")` respectively). `http://example.com:80` likewise normalises against `http://example.com`. Explicit non-default ports (`:8443`) are preserved in `backend_target()` and `host_authority()` but stripped from `cert_host()` / `sni_hostname()`. Asserted in `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]` — no adapter | yes | — | — |
| URI canonicalization — **Fastly backend identity half (Tier 2 / Tier 3):** building the canonical inputs above through the Fastly adapter yields **one dynamic backend** per canonical tuple — the identity hash collapses `https://example.com` and `https://example.com:443` into the same `Backend` entry in the registered-backend map. Tier 2 inspects the map; Tier 3 (Viceroy) observes the single backend across both URI spellings | — | yes | yes |
| URI scheme + host case normalisation — **core accessor half (Tier 1):** `OutboundRequest::get("https://EXAMPLE.com")`, `OutboundRequest::get("HTTPS://example.com")`, and `OutboundRequest::get("https://example.com")` produce identical `uri().host()`, `uri().scheme()`, `backend_target()`, `host_authority()`, and `cert_host()` outputs (all lowercase). Path / query are case-preserving (fragments are rejected upstream — round 29). Asserted in core | yes | — | — |
| URI scheme + host case normalisation — **Fastly identity half (Tier 2 / Tier 3):** same canonical inputs produce identical Fastly backend identity across the three case variants — one registered backend, same identity hash | — | yes | yes |
| `OutboundRequest::get("https://example.com/p#anchor")` and `::post(..)` return `bad_request("outbound URI must not contain a fragment")` — fragment detected on the raw input string *before* `http::Uri` truncates at `#`. `OutboundRequest::new(method, uri)` accepts a `Uri` that has already lost the fragment (documented asymmetry per §3.1.3) | yes | — | — |
| Capability enforcement: a manifest requiring `lazy-streamed-response-passthrough` causes the **`edgezero demo` runner** (contributor-only, the PR-#269 replacement for the removed `dev` command) to exit non-zero with the Axum BestEffort hard-fail message — via `run_demo(..)`'s sibling pre-dispatch gate against the Axum adapter, *not* via the `execute(..)` path (`demo` does not flow through it). The same hard-fail also fires via `execute(..)`'s pre-dispatch gate on `build` / `serve` / `deploy` / `auth`, and via the `run_config_*` / `run_provision` siblings for those commands. Test asserts every command exits non-zero | yes | — | — |
| `[capabilities.outbound].hosts` Spin render output is canonicalized: `["HTTPS://EXAMPLE.com:443", "api.example.com"]` → rendered `spin.toml` shows `["https://example.com", "https://api.example.com"]` (lowercase scheme/host, default port stripped, default-scheme https for bare hosts) | yes | — | — |
| Fastly `send_all` dispatch-overhead slack hard-bounded: with the adapter's `#[cfg(test)]` injection hook set to `Duration::from_millis(50)`, a `send_all` of N requests returns an `EdgeError::internal` whose message **contains the stable substring `"BATCH_DISPATCH_SLACK_MAX"`** (the full normative diagnostic per §4.3 is `"Fastly send_all adapter overhead between batch_now and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration"`) for the slots dispatched after the cumulative delay crosses `BATCH_DISPATCH_SLACK_MAX` (25 ms). Without the hook, no slot ever returns that error. A handler-side `thread::sleep` before `send_all` is **not** sufficient — it runs before `batch_now` is captured and cannot exercise the guard. Tests assert against the substring, not the full string, so future wording polish doesn't break them. **The hook lives in the Fastly adapter crate**, so this row is Tier 2 (substring assertion in `crates/edgezero-adapter-fastly/tests/contract.rs`) + Tier 3 (Viceroy with hook) — not Tier 1 (Tier 1's `MockOutboundClient` has no SDK arming step to wrap) | — | yes | yes |
| Fastly dispatch+headers phase-budget split **(common case, `total_ms ≥ 4`)**: a single `send` to a target that never returns headers fires the host timeout at `connect_ms + first_byte_ms = budget.duration`, **not** `2 × budget.duration`. Two separate test fakes — one that hangs the TCP connect, one that hangs after request bytes are sent — each return 504 within `budget.duration + BATCH_DISPATCH_SLACK_MAX + ms_rounding` (< 29 + budget ms), never twice the budget. The sub-4 ms degenerate branch is covered by the row below | — | yes | yes |
| Fastly single-`send` dispatch-overhead slack guard: the same `#[cfg(test)]` injection hook used for `send_all` (round 31) also wraps the single-send path between `dispatch_budget` and `send_async`; with the hook set to 50 ms, a single `send` returns `internal("Fastly send adapter overhead between dispatch_budget and SDK arming exceeded BATCH_DISPATCH_SLACK_MAX; …")`. Single send is **not** "structurally 0 slack" — the same hard constant applies (round 38) | — | yes | yes |
| Fastly body-phase EOF deadline: an upstream that sends headers + N-1 chunks within budget but holds the final read so EOF arrives *after* `budget.deadline` returns `gateway_timeout`, not `Ok(resp)`. Buffered drain checks `is_expired()` after every blocking read including EOF; streamed wrapper checks before and after each underlying read so the consumer sees an `Err` chunk instead of clean stream-end | — | yes | yes |
| `OutboundResponse::into_bytes_bounded_until(max, until)` with `until` **tighter** than `dispatch_budget(req).deadline`: the helper drives a streamed body whose adapter wrapper has 500 ms of effective budget left, but the caller passes `until = now + 100 ms`. The upstream sends data for 90 ms then holds the final read; EOF arrives at 110 ms. The helper returns `gateway_timeout` (not `Ok(bytes)`) because its `until_deadline.is_expired()` check fires before and after the EOF read. (`OutboundResponse` carries no effective-deadline state; the wrapper enforces the request budget separately — whichever fires first wins) | — | yes | yes |
| Fastly phase-split trade-off, documented: a 1 s `send` to a target that takes 300 ms to connect and 10 ms to send first-byte **fails** at the `connect_ms = 250 ms` timer (1/4 of budget) even though the entire exchange would have fit within 1 s. This is the explicit deviation §4.3 documents — preferring the absolute-deadline bound over the "every legal slow-connect request succeeds" property. The `outbound-flexible-phase-budget` capability is `BestEffort` on Fastly (§3.5.1 / §3.5.2 footnote 5); apps that need elastic phase budget declare it required and get the hard build failure on Fastly. §8 risk 9 tracks the configurable-split follow-up | — | yes | yes |
| Required `outbound-flexible-phase-budget` on Fastly → every adapter-selecting CLI command (`build` / `serve` / `deploy` / `auth` / `provision` / `config push` / `config validate` / `demo`) exits non-zero with the BestEffort hard-fail message via the §3.5.3 pre-dispatch gates (one inside `execute(..)`, siblings on `run_provision` / `run_config_*` / `run_demo`, PR #269); same manifest on Axum / Cloudflare / Spin passes | yes | — | — |
| Sub-4 ms Fastly budget: `total_ms = 3` produces `connect_ms = first_byte_ms = 3` (sum 6, not 3) by the explicit `total_ms < 4` degenerate branch in §4.3 code. The absolute-deadline bound shifts to 2× total_ms at this scale; ms rounding already dominates so the test asserts ≤ 2× rather than = | — | yes | yes |
| URI userinfo is rejected at construction: `OutboundRequest::get("https://user:pass@example.com")` → `Err(EdgeError::bad_request("outbound URI must not contain userinfo; pass credentials via the `authorization` header"))`. Credentials never reach `override_host` or any platform SDK | yes | — | — |
| Fastly HTTPS to IP literals: `https://127.0.0.1` and `https://[::1]` build dynamic backends with `.enable_ssl().check_certificate("127.0.0.1")` / `.check_certificate("::1")` (brackets stripped) and **skip** `.sni_hostname()` (SNI is DNS-only per RFC 6066). HTTPS to a DNS host still calls both setters. Identity-tuple round-trip works for both | — | yes | yes |

### 5.5 CI gate impact

The five existing gates in `CLAUDE.md` still apply by **count and shape** —
`cargo fmt --check`, `cargo clippy ... -D warnings`, `cargo test --workspace
--all-targets`, the feature-combination `cargo check`, and the Spin
`cargo check --target <triple>`. `cargo test --workspace --all-targets` now
also runs the Axum `tests/contract.rs` and the Tier 1 suite. The Tier 3
runtime jobs are added to `.github/workflows/test.yml` as separate jobs so a
missing runtime never blocks the core gate.

**Spin gate triple — pre-#269 vs PR-#269.** The fifth gate's literal command
string is checkout-dependent and **not preserved verbatim** across PR #269:

- **Pre-#269 (today's checkout):** `cargo check -p edgezero-adapter-spin --target
  wasm32-wasip1 --features spin` — matches `crates/edgezero-adapter-spin`'s
  current SDK 5 / wasip1 target. This is the form `CLAUDE.md` currently
  quotes.
- **PR-#269 (target baseline):** `cargo check -p edgezero-adapter-spin --target
  wasm32-wasip2 --features spin` — Spin SDK 6 / wasip2 (status-header bullet).
  Implementers landing this spec **after** PR #269 must update the gate quote
  in `CLAUDE.md` and `.github/workflows/*.yml` to `wasm32-wasip2`; preserving
  the stale `wasm32-wasip1` quote would silently break the Spin build. §8
  risk 10 tracks the CLAUDE.md / CI quote refresh.

The other four gates are unaffected by PR #269 and apply identically in
both worlds.

## 6. Migration impact

No back-compat shims. All renames are mechanical.

| Before | After |
| --- | --- |
| `crates/edgezero-core/src/proxy.rs` | `crates/edgezero-core/src/outbound.rs` |
| `ProxyClient` (trait) | `OutboundHttpClient` |
| `ProxyHandle` | `HttpClient` |
| `ProxyRequest` | `OutboundRequest` |
| `ProxyResponse` | `OutboundResponse` |
| `ProxyService<C>` | removed (use `HttpClient`) |
| `RequestContext::proxy_handle()` | `RequestContext::http_client()` |
| `*ProxyClient` in each adapter | `*OutboundClient` |

Other changes:

- **Body stays unified.** `OutboundRequest`/`OutboundResponse` use the core `Body` type;
  buffered is the default, streaming is opt-in via `stream_response()`. Streaming
  proxy-forward (`from_request`) is **preserved** — no public capability is lost.
- **Adapters** set `HttpClient` (not `ProxyHandle`) into request extensions — same
  mechanism, new type.
- **`EdgeError`** gains `BadGateway` / `GatewayTimeout` — additive (`#[non_exhaustive]`).
- **`Manifest`** gains `capabilities` (with nested `outbound`) — additive
  (`#[serde(default)]`); existing manifests parse unchanged.
- **`Adapter` trait** gains `capability()` — all four registered adapters implement it.
- **CLI** dispatch in the PR-#269 world: `ensure_capabilities` is wired in at
  **five pre-dispatch gate sites** (§3.5.3) — one inside
  `edgezero_cli::adapter::execute(..)` (covering `build` / `serve` / `deploy` /
  `auth login` / `auth logout` / `auth status`, *before* the manifest-shell-command
  branch and *before* the registry lookup), and **four siblings** at the top of
  `run_provision`, `run_config_push`, `run_config_validate`, and the
  contributor-only `run_demo`. Every adapter-selecting command runs the
  capability check exactly once at its entry point. `dev` is gone; `demo` is the
  contributor-only replacement that routes through Axum via its own sibling gate.
- **Scaffolding templates** — `handlers.rs.hbs` and any adapter templates that emit
  proxy code are updated to the new types; `spin.toml.hbs:13` renders
  `allowed_outbound_hosts` from `[capabilities.outbound].hosts` instead of the hardcoded
  `["https://*:*"]`. Without this, `edgezero new` would scaffold code against removed
  APIs.
- **Public docs (VitePress under `docs/guide/`)** — rewrite every page referencing
  `ProxyService` / `ProxyRequest` / `ProxyResponse` / `ProxyHandle` / `proxy_handle` /
  the deprecated `ProxyClient`. Known hits at the time of writing:
  `docs/guide/proxying.md`, `docs/guide/handlers.md`, `docs/guide/architecture.md`,
  `docs/guide/what-is-edgezero.md`, the per-adapter pages under `docs/guide/adapters/`,
  and the streaming docs. The new streaming proxy-forward example uses
  `OutboundRequest::from_request` + `HttpClient::send`. As a safety net the migration
  runs **two** repo-wide sweeps and reconciles every hit, including scaffold README
  templates and `examples/app-demo/`:

  1. Proxy-API sweep:
     `rg "Proxy|proxy_handle|ProxyRequest|ProxyResponse|ProxyService|ProxyHandle"`.
  2. `RequestContext` sweep — the round-6 restructure removes `ctx.request()` /
     `ctx.request_mut()` / `ctx.json()` / `ctx.form()` and changes the body API:
     `rg "ctx\.request\(|ctx\.request_mut\(|ctx\.body\(|ctx\.json\(|ctx\.form\(|RequestContext::request\b|RequestContext::request_mut\b|RequestContext::json\b|RequestContext::form\b|fn request\(&self\) -> &Request|fn request_mut\(&mut self\) -> &mut Request|fn json<\|fn form<"`.
     Current callers include `crates/edgezero-core/src/middleware.rs` (the
     `RequestLogger` reads `ctx.request()`), `crates/edgezero-core/src/extractor.rs`
     (the `Json` / `ValidatedJson` / `Form` / `ValidatedForm` extractors call
     `ctx.json()` / `ctx.form()`), `crates/edgezero-core/src/context.rs` itself
     (definitions of `json` / `form` are removed), per-adapter `request.rs` modules
     that materialise `RequestContext`, and doc pages under `docs/guide/`. Each site
     moves to `ctx.parts()` / `ctx.parts_mut()` / `ctx.body_kind()` /
     `ctx.body_bytes(max)` / `ctx.json_within(max)` / `ctx.form_within(max)` /
     `ctx.take_body()` / `ctx.into_request()` per §3.4.5.
- **Consumers** — `examples/app-demo` and downstream consumers migrate call sites: rename types,
  `proxy_handle()` → `http_client()`, adopt `send_all`.

## 7. File-by-file change summary

**`crates/edgezero-core`**
- `src/proxy.rs` → `src/outbound.rs` — `OutboundHttpClient`, `HttpClient`,
  `OutboundRequest`, `OutboundResponse`, `ResponseMode`; drop `ProxyService`. Also
  exposes the public response/request-body cap constants:
  `pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1 * 1024 * 1024;` and
  `pub const DEFAULT_OUTBOUND_REQUEST_BODY_BYTES: usize = 8 * 1024 * 1024;`.
- `src/time.rs` — new module. Contents:
  - `Deadline` (value type, §3.3.1)
  - `DispatchBudget { duration: Duration, deadline: Deadline }` (§3.3.2)
  - `pub fn dispatch_budget(req: &OutboundRequest, now: web_time::Instant) -> Result<DispatchBudget, EdgeError>` (§3.3.2)
  - Constants (§3.3.1, §3.3.4, §4.3):
    - `pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);`
    - `pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_secs(7 * 24 * 60 * 60);` (round 24)
    - `pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);` (round 29)

  The earlier "value type only" wording was stale before round 23 introduced
  `DispatchBudget` and the explicit `now` parameter; this is the complete
  current contents of the file.
- `src/capability.rs` — new: `Capability`, `CapabilitySupport`.
- `src/error.rs` — add `BadGateway` (502), `GatewayTimeout` (504) + constructors;
  extend `status()`.
- `src/extractor.rs` — extractor migration per §3.4.5: `Json<T>` /
  `ValidatedJson<T>` route through `ctx.json_within(DEFAULT_INBOUND_JSON_BYTES)`;
  `Form<T>` / `ValidatedForm<T>` route through `ctx.form_within(DEFAULT_INBOUND_FORM_BYTES)`;
  add `ValidatedJsonWithin<T, MAX>` and `ValidatedFormWithin<T, MAX>` for explicit
  caps. Constants exposed: `pub const DEFAULT_INBOUND_JSON_BYTES: usize = 8 * 1024 * 1024;`
  and `pub const DEFAULT_INBOUND_FORM_BYTES: usize = 1 * 1024 * 1024;`.
- `src/compression.rs` — evolve the existing core async stream decoders (§3.4.1):
  change the chunk error type from `io::Error` to `EdgeError` (wrap each
  `io::Error` with `EdgeError::bad_gateway(..)`). CF/Fastly/Spin response
  converters call into the same module rather than carrying parallel
  decompressor copies.
- `src/context.rs` — `RequestContext` restructured to `{ path_params, parts:
  http::request::Parts, body: BodyCell }` (§3.4.5); `proxy_handle()` →
  `http_client()`; `request()` / `request_mut()` removed, replaced with
  `parts()` / `parts_mut()`; add `body_kind()`, `take_body()`, `body_bytes`,
  `json_within`, `form_within`, and `into_request()`; legacy `json()` and
  `form()` removed.
- `src/body.rs` — **change `Body::Stream`'s error type from `anyhow::Error` to
  `EdgeError`**: `Stream(LocalBoxStream<'static, Result<Bytes, EdgeError>>)`. The
  deadline-aware stream wrappers (§4.1/§4.2/§4.3/§4.4) yield `gateway_timeout`
  chunks, and response converters now downstream-map error chunks without an
  `anyhow::Error → EdgeError` downcast dance — a wrapper that produces a
  `gateway_timeout` chunk can no longer be silently rewritten to `internal` by a
  consumer that maps every stream error to 500. Existing in-tree call sites (proxy
  forwarding, body draining) are updated mechanically; external streams supplied to
  `Body::from_stream` map their source errors into `EdgeError::internal(..)` (the
  honest mapping for an unknown stream-source error). Also implement the pre-append
  checked accounting and bounded-byte rewrite of `into_bytes_bounded` (§3.4.1).
- `src/manifest.rs` — add `ManifestCapabilities` + `ManifestOutboundCapability` +
  `Manifest::capabilities`.
- `src/lib.rs` — re-export new modules; drop proxy re-exports.
- `Cargo.toml` — `MockOutboundClient` under the existing `test-utils` feature.

**`crates/edgezero-adapter`**
- `Cargo.toml` — **add `edgezero-core` as a workspace dependency.** `Capability` /
  `CapabilitySupport` live in `edgezero-core` (so manifest parsing can use them), and
  the `Adapter` trait references them; the crate currently has no dependency on core
  and that must be added. The direction (adapter → core) is the standard one and
  introduces no cycle.
- `src/registry.rs` — add `Adapter::capability()`.

**`crates/edgezero-adapter-{axum,cloudflare,fastly,spin}`**
- `src/proxy.rs` → `src/outbound.rs` — `*OutboundClient` implementing
  `OutboundHttpClient::send` and `send_all`, buffered + streamed modes,
  decompressed-byte cap, header normalization for decompressed responses
  (strip `content-encoding` / `content-length`).
- `src/response.rs` — **per-adapter streaming policy.** Today each adapter's
  response converter (`crates/edgezero-adapter-{axum,fastly,spin}/src/response.rs`)
  buffers `Body::Stream` before producing the platform response. The migration
  preserves lazy streaming **where the platform allows it without violating core's
  `LocalBoxStream` (non-Send) invariant**:

  - **Cloudflare** — WASM, single-threaded JS event loop, no `Send` requirement on
    response bodies. `worker::Body::from_stream` consumes the `Body::Stream`
    directly; chunks flow without buffering.
  - **Fastly** — WASM, single-threaded guest, no `Send` requirement.
    `Response::with_streaming_body` plus a chunk-pump driven by the wrapped
    deadline-aware stream (§4.3) yields chunks without buffering.
  - **Spin** — WASM, WASI async, no `Send` requirement. The WASI outgoing-body
    chunk-write path consumes the `Body::Stream` directly.
  - **Axum** — native, multi-threaded tokio. `axum::body::Body::from_stream` requires
    `Send + 'static`, which conflicts with core `Body::Stream = LocalBoxStream`
    (intentionally non-Send for WASM compat — `body.rs:14`). Designing a real
    `LocalBoxStream → Send` bridge (e.g. `spawn_local` + tokio mpsc) is non-trivial
    and out of scope for this migration. **The Axum response converter therefore
    buffers `Body::Stream` into `Bytes` (bounded, pre-append-checked) before
    constructing the axum response.** The cap is a defined Axum-adapter constant
    `AXUM_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` (a **fixed compile-time constant**;
    no `AxumOutboundConfig` plumbing in this migration). The per-outbound-request
    `max_response_bytes` is unavailable at this stage because the app has already
    consumed `OutboundResponse::into_response()` into a core `Response<Body>` and the
    original cap was attached to the now-discarded `OutboundRequest`. Apps that need
    a different ceiling either edit the constant in their fork, carry the bytes
    through a buffered path explicitly, or wait for the configurable follow-up
    tracked in §8 risk 6.

    **Stream-error handling during buffered drain.** Because the Axum response
    converter buffers `Body::Stream` *before* writing any downstream response
    headers, it can map a stream error to a clean HTTP status (unlike the
    streaming-passthrough adapters, which would have to abort the wire because
    headers had already been sent — §3.1.1 post-header rule). The mapping is:

    | Stream chunk yields | Axum response |
    | --- | --- |
    | `Ok(bytes)`, buffer + bytes.len() ≤ cap | append, continue |
    | `Ok(bytes)`, buffer + bytes.len() > cap | abort drain → axum response status **502** with body `"response body exceeded N bytes"` |
    | `Err(EdgeError::GatewayTimeout(..))` | abort drain → axum response status **504** with the error message |
    | `Err(EdgeError::BadGateway(..))` | abort drain → axum response status **502** with the error message |
    | `Err(other EdgeError)` | abort drain → axum response with the `EdgeError::status()` for that variant (`internal` → 500, etc.) |

    Source: the wrapped streamed body's `EdgeError` chunks already encode the
    intended status; Axum just lifts them to the response. No silent
    coalescing-to-502, no panic. This is the documented Axum-specific
    limitation: lazy streaming proxy-forward works on Cloudflare, Fastly, and
    Spin; Axum buffers, *but the buffering boundary lets it preserve the
    correct status code*. For fan-out handlers and most edge-shaped
    apps this is a non-issue; if true lazy streaming on Axum becomes a
    requirement later, an mpsc bridge is a separate follow-up. Capability text
    and risk section reflect this (see §3.5.2 footnote 3 and §8).

  Buffering is reserved for `Body::Once` on the three WASM adapters; on Axum, the
  buffering path also applies to `Body::Stream`.
- adapter entry — register `HttpClient`; declare `capability()`.
- **Axum `Cargo.toml`** — enable `gzip` and `brotli` features on `reqwest` so
  transparent decompression matches the other three adapters (the workspace
  reqwest dep is `default-features = false` today; the Axum adapter opts these
  features in directly).
- Fastly:
  - Hash-based dynamic-backend naming (`format!("ez_{:032x}", sha256_128(identity))`,
    §4.3). The `edgezero-adapter-fastly/Cargo.toml` adds **`sha2` workspace
    dependency** for the SHA-256 digest; the 128-bit truncation is `&digest[..16]`.
    Alternatively, if a SHA-256 helper already exists in `edgezero-core` (audit step
    in the same sweep), the adapter uses that; either way the dep is declared
    explicitly in this migration, not assumed transitive.
  - Dispatch-time host timeouts and SSL configuration on `BackendBuilder` per
    §3.3.4 / §4.3, using the **four canonical URI accessors** introduced in
    rounds 25 / 46 / 47:
    `Backend::builder(name, req.backend_target())` for the connection target;
    `.override_host(req.host_authority())` for the outgoing `Host` header (the
    accessor encodes the canonicalization — userinfo rejected, default ports
    stripped per §3.1.3, explicit non-default ports preserved); timeouts via
    `connect_timeout` / `first_byte_timeout` / `between_bytes_timeout` with the
    §3.3.4 phase split (1/4 connect, 3/4 first-byte, full budget between-bytes;
    degenerate to `both = total_ms` for sub-4 ms budgets); HTTPS → `.enable_ssl()`
    plus `.check_certificate(req.cert_host().unwrap())` (`cert_host()` is `Some`
    on any HTTPS scheme and pre-strips brackets); `.sni_hostname(sni)` is called
    **only when `req.sni_hostname()` is `Some(sni)`** (DNS-name hosts); IP-literal
    hosts return `sni_hostname() == None` per RFC 6066 §3, so the adapter omits
    `.sni_hostname()` entirely while still passing `cert_host()` to
    `.check_certificate(..)`. HTTP (`cert_host() == None`) → `.disable_ssl()`.
    **The four accessors are the only canonical source** — adapters MUST NOT
    re-derive from `req.uri()` directly, the local `is_ip_literal` parse +
    `trim_start_matches('[')` shape from earlier rounds is gone (round 47).
    The backend is passed to `send_async` / `send_async_streaming` at send time
    via `impl ToBackend`; there is no
    `with_backend(..)` setter on `Request`.
- Spin: render `allowed_outbound_hosts` from the manifest per §3.5.4.
- `tests/contract.rs` — created for Axum; extended for the other three (§5).

**`crates/edgezero-cli`**
- `src/adapter.rs` — wire `ensure_capabilities` as the **first statement** of
  `edgezero_cli::adapter::execute(adapter_name, action, manifest_loader, args)`
  (PR #269), *before* `manifest_command(..)` is consulted and *before* the
  registry lookup. This covers `run_build`, `run_serve`, `run_deploy`, and the
  three `run_auth` sub-actions (which all dispatch through `execute(..)`). The
  three commands that don't flow through `execute(..)` — `run_provision`,
  `run_config_push`, `run_config_validate` — get **sibling pre-dispatch gates**:
  each is the first statement of its `run_*` function and calls the same
  `ensure_capabilities` helper. The contributor-only `run_demo` also calls
  `ensure_capabilities("axum", ..)` at its top before the Axum runner starts.
  **All five gate sites** (one inside `execute(..)`, the four siblings on
  `run_provision` / `run_config_push` / `run_config_validate` / `run_demo`) are
  documented in §3.5.3's gate table. The legacy `handle_build` / `handle_serve`
  / `handle_deploy` / `handle_dev` functions referenced in earlier appendices
  were removed by PR #269.
- scaffolding templates (`handlers.rs.hbs`, `spin.toml.hbs`, adapter templates) — update
  to the new API and manifest-driven outbound hosts.

**`examples/app-demo`**
- migrate to the new types and `send_all` across the per-adapter binaries.
  PR #269 added a separate `examples/app-demo/crates/app-demo-cli/` integration
  crate that drives the typed CLI (`auth`, `provision`, `config push/validate`,
  `demo`) against the demo manifest; update that crate's fixtures alongside the
  adapter binaries so the new outbound types compile end-to-end. The demo
  manifest's `[stores.*]` blocks (PR #269's `ManifestStores { config, kv,
  secrets }` shape) are unchanged — outbound capabilities sit in
  `[capabilities.outbound]` and compose additively with the store sections.

**`docs/`**
- `proxying.md`, `adapters/overview.md`, `handlers.md` (and any other proxy references) —
  rewrite for the outbound API.

**`.github/workflows/test.yml`**
- add Tier 3 runtime jobs (Axum now; Fastly/Cloudflare/Spin as runtimes are wired).

## 8. Open questions / risks

1. **`DEFAULT_MAX_RESPONSE_BYTES` = 1 MiB.** Trivially overridable per request via
   `max_response_bytes`. Confirm the default suits expected target responses.
2. **Tier 3 CI runtimes.** Viceroy / `workerd` / `spin` jobs add CI cost and
   maintenance. The design degrades safely (Tier 1 + Tier 2 always run); the risk is
   schedule, not correctness.
3. **Cloudflare cancellation.** Dropping the raced future to enforce a timeout relies on
   the Workers runtime reclaiming the subrequest. Effective in practice; the Tier 3 CF
   test verifies wall-clock behaviour.
4. **Fastly body-phase overshoot.** The deadline overshoot on Fastly is bounded by one
   between-bytes-timeout interval (§3.3.4). If a stricter guarantee is ever required, the
   adapter would need to cap total body-read attempts — out of scope here.
5. **Naming.** `OutboundHttpClient` (trait) vs. `HttpClient` (handle) are close. They
   never co-occur in app code — handlers see only `HttpClient` — so the overlap is
   low-risk, but a rename of the handle is cheap if preferred.
6. **Axum lazy streaming follow-up.** The Axum response converter buffers `Body::Stream`
   into `Bytes` because core `Body::Stream = LocalBoxStream` is non-Send and Axum's
   `Body::from_stream` requires `Send + 'static` (§3.5.2 footnote 3, §4.1, §7). A real
   bridge — e.g. a `tokio::task::spawn_local` driving a `tokio::sync::mpsc` Send channel
   read by Axum — is implementable but non-trivial and is **deferred**. Apps that need
   lazy streaming on Axum declare the `lazy-streamed-response-passthrough` capability
   required and get a hard build failure today; lifting the limitation is a separate
   future change with its own design + tests.
7. **Fastly streamed-upload write-phase bound.** The spec treats Fastly's
   `between-bytes-timeout` as covering both reading from origin and writing to origin
   (the documented Fastly behaviour). If a future host change relaxes that, the
   write-phase claim (BoundedCooperative, §4.3) would have to downgrade to BestEffort;
   the source-stream-yield BestEffort label is unaffected. Track Fastly host docs.
8. **Fastly buffered-body-drain serialization in `send_all`.** Harvest reads bodies in
   slot order, so wall-clock = `max(header_arrivals) + Σ buffered_body_drain_times`
   on Fastly vs. `max(header_arrivals + body_drain_times)` on Axum/CF/Spin (§3.3.4).
   For small JSON bodies (fan-out batches) the difference is negligible; for ≥ few-MiB
   responses Fastly is suboptimal. **There is no current EdgeZero mitigation** —
   and Streamed mode is not the workaround (it's rejected by `send_all` preflight
   per §3.1.1, and even via single `send` Fastly has no concurrent
   chunk-consumption primitive). Apps that need concurrent large-body fan-out on
   Fastly should (a) target a different adapter for that workload, (b) restructure
   the topology so parallel large-body drains aren't required, or (c) wait for the
   interleaved-drain follow-up. The follow-up — interleaved chunk reads across
   in-flight Fastly `Response` bodies, driven from a single guest harvest loop — is
   non-trivial without an async reactor and is **deferred**. The
   `send-all-slot-isolation` capability (§3.5.1 footnote 4) lets apps declare the
   requirement explicitly and get a hard build failure on Fastly until this lands.
9. **Fastly configurable phase split.** The fixed 1/4 connect + 3/4 first-byte
   split (§4.3) produces premature connect failures for slow-connect upstreams
   even when the total budget would have sufficed. Apps that hit this require
   `outbound-flexible-phase-budget` (§3.5.1 footnote 5) and fall through to the
   hard build failure on Fastly. The follow-up would either expose a per-request
   `fastly_phase_split(connect_ratio: f32)` setter, a per-`OutboundRequest`
   configuration field, or a per-adapter config knob on `FastlyOutboundClient`.
   Each option has a memory-model and capability impact, so it's left **deferred**
   pending a real use case.
10. **CLAUDE.md / CI command-quote refresh for Spin SDK 6 + wasip2.** PR #269
    bumps the Spin adapter to `spin-sdk = "6"` and the target triple to
    `wasm32-wasip2`; the project `CLAUDE.md` and `.github/workflows/*.yml`
    snippets still quote `cargo check -p edgezero-adapter-spin --target
    wasm32-wasip1 --features spin` in several places. The spec itself doesn't
    pin a target triple (it references `spin_sdk::http::send` symbolically,
    which is SDK-6-compatible), so no §3 / §4 / §5 change is needed — but the
    CI gate quotes and the CLAUDE.md table need a follow-up refresh so
    contributors don't paste the old triple. Tracked here so the spec rebase
    appendix (Appendix AR) has a one-line forward pointer.
11. **Per-batch transient-memory cap against adversarial chunking.** §3.4.1's
    `sizeof(current_chunk)` term is source-controlled — an upstream peer that
    yields one large `Bytes` produces a transient resident footprint equal to
    that chunk size plus the persistent buffer cap. EdgeZero currently does not
    rechunk. The follow-up would either: (a) add an opt-in
    `OutboundRequest::max_chunk_bytes(usize)` builder field that wraps the
    upstream stream with a rechunker on the consumer side (lazy, opt-in, no
    perf cost when unset); (b) add a fixed `MAX_TRANSIENT_CHUNK_BYTES` constant
    in `edgezero-core` that every adapter's incoming-body stream must respect
    by rechunking at ingest (eager, breaks lazy passthrough on CF/Fastly/Spin
    when the upstream's natural chunk size exceeds the constant); or (c) leave
    it source-controlled and document the bound at the adapter level
    (`hyper`'s 16 KiB, WASI's 64 KiB, etc.) as the operational floor. Each
    option has a perf and lazy-streaming trade-off; deferred until a
    fan-out batch or downstream consumer reports actual OOM behaviour from
    adversarial chunking. The §3.4.1 / §3.4.4 docs already call out the
    caveat so apps aren't surprised.

## Appendix index — historical, not normative

Appendices A through the last `## Appendix` heading in the document (use that
heading as the canonical upper bound — the index doesn't pin an exact letter
because every round adds another one and the index would otherwise drift)
record the round-by-round evolution of the spec. **The
authoritative normative content is §1–§8**; appendix entries are kept as a paper
trail of what changed and why. Entries in earlier rounds may have been superseded
by later rounds — for example, round-6's "into_request returns Body::empty() after
poison" was changed to a fallible Err in round 18, and round-15's "configurable at
adapter init for `AXUM_RESPONSE_STREAM_BUFFER_BYTES`" was tightened to a fixed
compile-time constant in round 16. When the active sections and an older appendix
disagree, the active sections win. Round 20 (Appendix T) does **not** re-walk every
prior entry; the index note here is the disclaimer for the whole history.

## Appendix A — Review round 1 resolutions

| Review finding | Resolution |
| --- | --- |
| Deadline semantics too strong for Fastly / buffering after exchange | §3.3.3–§3.3.4: deadline scope defined per `ResponseMode`; buffering happens inside the deadline-bounded region; Fastly body phase documented as bounded-cooperative |
| `time::timeout()` cannot live in core | §3.3.5: general combinator removed; core ships only the `Deadline` value type |
| `timers` capability misrepresents Fastly | §3.5.1: renamed `outbound-deadlines`, defined precisely; no general-timer claim |
| Memory bounded per-response, not per-batch | §3.4.4: explicit batch memory model; app bounds N; §1.1 goal reworded |
| Outbound URI validation underspecified | §3.1.3: constructors validate scheme (`http`/`https`) + authority; invalid → 400 |
| Header builder cannot be infallible | §3.1.3: `header(..)` is `Result<Self, EdgeError>`; `headers_mut()` for pre-validated values |
| Compressed cap before/after decompression | §3.4.1: cap is decompressed bytes, enforced incrementally during decompression |
| `[capabilities.outbound]` not modeled | §3.5.1/§3.5.4: `ManifestOutboundCapability` struct, default `["*"]`, Spin render rules |
| Migration misses templates and docs | §6/§7: scaffolding templates and `docs/` pages added to the migration checklist |
| "only outbound type app code touches" inaccurate | §3.1.2: reworded to "only outbound client/handle type" |
| Fastly dynamic backend naming not robust | §4.3: hash-based stable names (`ez_<16hex>`, FNV-1a of authority) |
| Test plan misses riskiest deadline behaviour | §5.4: added slow streaming bodies, compressed expansion, headers-then-deadline, partial timeout, empty input |
| Residual risk: dropping streaming forward | Resolved by decision §1.4 — unified body; streaming proxy-forward preserved |

## Appendix B — Review round 2 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly streamed request bodies would break dispatch-all | §3.1.1 + §4.3: `send_all` rejects `Body::Stream` request bodies on every adapter (per-slot `bad_request`, 400); streamed uploads use `send` |
| "None budget fails immediately" conflicted with optional timeouts | §3.3.2: precise `dispatch_budget` rule — `None` means no deadline; only an expired deadline or `Duration::ZERO` fails immediately |
| Fastly omitted from decompression-cap obligation | §3.4.1: cap obligation explicitly applies to Axum (reqwest), Cloudflare, Fastly, and Spin |
| `Streamed` mode weakened `Ok` semantics | §3.1.1 trait rustdoc differentiates `Ok` semantics — full exchange completion in `Buffered`, headers-only in `Streamed`, with body-phase failures surfacing on consumption |
| Outbound JSON parse error mapping unspecified | §3.1.3 + §3.4.3: malformed upstream JSON / `json::<T>` on a streamed body → `bad_gateway` (502) |
| `Body::into_bytes_bounded` maps to 400 but outbound wants 502 | §3.1.3 + §3.4.1: `OutboundResponse::into_bytes_bounded` does its own bounded drain mapping over-limit to `bad_gateway` (502); it does not delegate to the core helper |
| `Native` overstated for Fastly outbound-deadlines | §3.5.1/2: new `BoundedCooperative` support level added; Fastly `outbound-deadlines` = `BoundedCooperative`; rubric documented so future adapters are judged consistently |
| Test plan missed streamed request bodies in fan-out | §5.4: per-slot 400 rejection test added (Tier 1 + Tier 2); streamed-`send` proxy-forward success test added across tiers |
| Spin host render rules too lossy | §3.5.4: explicit accepted-form table with per-form output and load-time validation rules |

## Appendix C — Review round 3 resolutions

| Review finding | Resolution |
| --- | --- |
| Axum decompression claim didn't hold with current `reqwest` features | §3.4.1 + §7: the Axum adapter's `Cargo.toml` opts in `reqwest`'s `gzip` and `brotli` features so decompression actually happens and the cap obligation applies |
| `header(..)` signature wasn't implementable as written | §3.1.3: signature now has explicit `Display` bounds on the `TryInto::Error` associated types so the impl can format conversion failures into `EdgeError::bad_request` |
| Capability types in core created an unstated crate dependency | §7: `crates/edgezero-adapter/Cargo.toml` adds `edgezero-core` as a workspace dep — direction is adapter → core, no cycle |
| `deploy` skipped capability enforcement | §3.5.3 + §7: `ensure_capabilities` runs in `handle_build`, `handle_serve`, **and** `handle_deploy` |
| `from_request` didn't define header normalization | §3.1.3: explicit rules — strip hop-by-hop headers (RFC 7230 §6.1 list + per-connection-header), replace `host`, drop `content-length`. Defined once in core so adapters don't diverge |
| Streamed-mode response header normalization for decompression unspecified | §3.4.1: when an adapter decompresses, the returned `OutboundResponse.headers` must have `content-encoding` and `content-length` stripped — applies to both `Buffered` and `Streamed` |
| `body_bytes` / `json_within` consumption semantics missing | §3.4.2: first call drains a `Body::Stream` and replaces the context body with `Body::Once(bytes)`; subsequent calls return a cheap clone, re-checking the cap. Network body read at most once |
| Fastly bounded-overshoot calculation depended on implicit timeout state | §3.3.4 + §7: the bound is on `between-bytes-timeout` set *at dispatch* to `effective_at_dispatch`; the Fastly SDK exposes no per-chunk timeout update, so the bound does not shrink while a slot waits behind earlier harvest work. Spec now states this explicitly |

## Appendix D — Review round 4 resolutions

| Review finding | Resolution |
| --- | --- |
| Redirect behaviour could bypass app allowlists | §3.1.4: adapters never auto-follow redirects; 3xx is delivered as `Ok` with `Location` preserved; per-adapter mechanics tabulated; app re-runs its allowlist against `Location` before issuing a new request |
| `Streamed` deadlines lacked a deadline-aware body-drain helper | §3.1.3: `OutboundResponse::into_bytes_bounded_until(max, deadline)` added; §5.4 has a contract test |
| Header preservation conflicted with Spin/WASI UTF-8 limitation | §3.1.4: uniform UTF-8 rule across all adapters — request headers rejected at construction (`bad_request`), upstream response headers dropped with `warn!` diagnostic; ASCII-only headers (auth/tracing/cache/conneg) unaffected |
| Fastly capability conflated adapter support with service config | §4.3: new "Service prerequisite — dynamic backends" subsection; `ensure_capabilities` emits an informational log; runtime failure surfaces as `bad_gateway` with a remediation message; capability matrix is explicitly an adapter-support contract, not a runtime health guarantee |
| `send` / `send_all` equivalence was prose-only | §5.4: explicit equivalence contract test (Tier 1 + Tier 2) — status, headers, body cap, deadline classification, decompression, error mapping all asserted identical |
| Fastly pseudocode contained a production-hostile panic | §4.3: replaced `expect("every slot resolved")` with a graceful per-slot `EdgeError::internal(..)` — adapter boundaries never panic the host on a contract bug |
| `json` helper Content-Type behaviour unspecified | §3.1.3: sets `content-type: application/json` only when absent; caller-set value preserved; `content-length` left to adapter; serialization failure → `internal` |

## Appendix E — Review round 5 resolutions

| Review finding | Resolution |
| --- | --- |
| `into_bytes_bounded_until` promised timer behaviour core cannot implement | §3.1.3: the helper is explicitly cooperative on every adapter. Real-time enforcement comes from adapters with a platform timer (Axum / Cloudflare / Spin) wrapping streamed response bodies with a deadline-aware stream at construction time; Fastly is bounded-cooperative with the same overshoot bound as §3.3.4. §5.4 has a stalled-chunk test |
| Inbound body boundedness wasn't actually covered by the migration | §3.4.2 + new §3.4.5: adapters stop pre-buffering and expose `Body::Stream`; `RequestContext::body_bytes` / `json_within` are `&self`-callable via an internal cache so existing `FromRequest` extractors compile unchanged; `Json` / `ValidatedJson` delegate to `json_within(DEFAULT_INBOUND_BODY_BYTES = 8 MiB)`, with `ValidatedJsonWithin<T, MAX>` for tighter caps |
| Request-header safety rules were bypassable | §3.1.4: new `outbound::normalize_for_dispatch` core helper that adapters MUST call before dispatch — drops non-UTF-8, strips hop-by-hop, removes `host` / `content-length` / `transfer-encoding`. Idempotent. `headers_mut()` and `from_request` are safe to use freely; the final sweep guarantees portability and framing |
| Fastly backend hash key omitted scheme and resolved port | §4.3: identity = `scheme + ":" + host + ":" + resolved_port + ":" + tls_mode`; backends deduplicated by full identity, so `http://x` and `https://x` are not conflated |
| Required + `BestEffort` weakened the capability contract | §3.5.3: required + `BestEffort` is now a **hard failure**; if degradation is acceptable, declare the capability `optional` instead. Required means real enforcement (`Native` or `BoundedCooperative`) |
| Multi-value header preservation not specified or tested | §3.1.4: explicit "preserve every entry" contract — `HeaderMap::append` / `get_all`; §5.4 covers repeated `set-cookie` and repeated outbound request headers |
| Migration doc paths stale | §7: paths corrected to `docs/guide/...`; known hits enumerated (`docs/guide/proxying.md`, `handlers.md`, `architecture.md`, `what-is-edgezero.md`, per-adapter pages, streaming docs); `rg "Proxy\|proxy_handle\|ProxyRequest\|ProxyResponse\|ProxyService\|ProxyHandle"` repo-wide as a safety net |

## Appendix F — Review round 6 resolutions

| Review finding | Resolution |
| --- | --- |
| `OutboundRequest`/`OutboundResponse` API was not implementable by adapters | §3.1.3: added `OutboundRequest::into_parts() -> OutboundRequestParts` (struct exposes every field including `body`, `timeout`, `deadline`, `response_mode`); `OutboundResponse::new`, `headers_mut`, and `into_parts(self) -> (StatusCode, HeaderMap, Body)` for adapter assembly |
| Inbound body cache `request()` / `body()` / `into_request()` semantics undefined | §3.4.5: `RequestContext` is restructured to `{ path_params, parts, body: BodyCell }`; explicit behaviour table for every method post-cache; `into_request()` reassembles with `Body::Once(cached)` so streaming proxy-forward composes with middleware that already buffered |
| Failed inbound body reads had no cache/poison semantics | §3.4.5: new `BodyState::Poisoned(StoredError)` variant — after a failed drain, all subsequent `body_bytes`/`json_within` return the same stored error; `body()` returns `Body::empty()`; the network body is not retried (silent re-read is impossible) |
| Multi-value header preservation lacked per-adapter mechanics | §3.1.4: per-adapter table naming the exact SDK calls — `Fastly::append_header`/`get_header_all`, `worker::Headers::append`, `spin_sdk::Headers::append` (WASI `fields`), reqwest's native append. Spec downgrade path documented if a future SDK breaks round-tripping |
| Axum no-deadline behaviour was ambiguous | §3.3.2: `DEFAULT_NO_DEADLINE_BUDGET = 30 s` is the documented EdgeZero default applied by every adapter when neither `timeout` nor `deadline` is set, preserving the existing Axum 30 s ceiling and making "no deadline" mean the same finite thing everywhere |
| `from_request` and `normalize_for_dispatch` disagreed about `Host` | §3.1.3: `from_request` now **drops** `host`; `normalize_for_dispatch` (§3.1.4) sets it from `req.uri()` at dispatch — single source of truth |
| Streamed JSON ergonomics were misleading | §3.1.3: added `OutboundResponse::json_bounded(self, max)` and `json_bounded_until(self, max, deadline)` consuming convenience methods; the `&self` `json` error text directs callers to those |
| Migration summary had stale bullets | §6 short bullet + §2 summary table updated to include `handle_deploy` and `docs/guide/...` paths; no longer contradict the detailed sections |

## Appendix G — Review round 7 resolutions

| Review finding | Resolution |
| --- | --- |
| Streamed deadline semantics were internally inconsistent | §3.3.3 rewritten: the originating `Deadline` covers the entire exchange end-to-end in both modes. In `Streamed`, adapters wrap the response body with a deadline-aware stream so chunk reads honour the same deadline; `Ok(resp)` returns earliest-possible (headers) but the body still errors past the deadline. `into_bytes_bounded_until` is for tightening below the originating deadline, not for re-applying it |
| Async body cache needed an in-flight state | §3.4.5: `BodyState` adds `Draining`; explicit non-async take/replace protocol; drop-guard turns dropped drain futures into `Poisoned(cancelled)`; reentrant calls during `Draining` return `EdgeError::internal` without panic. §5.4 tests drop-mid-drain and reentrant access |
| Bounded-memory still leaned on a helper that over-allocates by one chunk | §3.4.1: explicit "pre-append checked length accounting" rule for both inbound (`RequestContext::body_bytes`) and outbound (`OutboundResponse::into_bytes_bounded`); `Body::into_bytes_bounded` in `crates/edgezero-core/src/body.rs:84` is rewritten to check before extending. Memory is bounded by `max`, with no per-chunk overshoot |
| `RequestContext::body()` was unimplementable as specified | §3.4.5: `body()` removed. Replaced by `body_kind() -> BodyKind` for non-consuming state inspection and `take_body() -> Body` for consuming extraction. `body_bytes` / `json_within` / `take_body` / `into_request` are the only ways to actually access the body |
| Inbound migration missed `Form` / `ValidatedForm` | §3.4.5: extractor migration table now includes `Form` and `ValidatedForm` — both delegate to a new `ctx.form_within(max)` helper with `DEFAULT_INBOUND_FORM_BYTES = 1 MiB`; `ValidatedFormWithin<T, MAX>` added for explicit caps; legacy `RequestContext::form()` removed |
| Adapter notes bypassed `DEFAULT_NO_DEADLINE_BUDGET` | §4.1 + §4.3 rewritten to compute the budget via `dispatch_budget(req)` (§3.3.2) instead of an adapter-local `min(..)` formula, so no-deadline requests are uniformly bounded to 30 s on every adapter |
| Migration sweep was too proxy-focused | §7 docs migration now documents **two** sweeps: the proxy-API sweep and a new `RequestContext` sweep for `ctx.request()` / `request_mut()` / `ctx.body()` / `fn request(..) -> &Request` patterns, with the known core sites (`middleware.rs`, `extractor.rs`, per-adapter `request.rs`) called out |
| Host normalization wording still disagreed | §3.1.3 + §3.1.4 unified: `from_request` drops `host`; `normalize_for_dispatch` is the sole single-source-of-truth strip; the adapter derives the final `Host` (or SDK equivalent) directly from `req.uri()` at SDK-construction time without re-reading `req.headers()` |

## Appendix H — Review round 8 resolutions

| Review finding | Resolution |
| --- | --- |
| Axum can't stream request bodies through reqwest as previously implied | §3.1.3 adds `OutboundRequest::max_request_body_bytes(n)` with `DEFAULT_OUTBOUND_REQUEST_BODY_BYTES = 8 MiB`; §4.1 specifies that Axum drains streamed request bodies into `Bytes` up to that cap (pre-append checked accounting, `bad_request` on overflow) before issuing the reqwest request. No `reqwest` `stream` feature required. Bounded, predictable, WASM-compatible across the board. CF / Spin notes (§4.2 / §4.4) updated to apply the same cap |
| BodyCell state/API not type-checkable | §3.4.5: `BodyState` adds `Taken`; new public `BodyKind` enum (variants `Initial \| Draining \| Cached { len } \| Poisoned \| Taken`); `take_body() -> Result<Body, EdgeError>` (Err on `Draining` programmer error and on `Poisoned`) — all referenced variants are now real |
| CF/Spin streamed deadline notes lagged the contract | §4.2 + §4.4: both adapters now wrap streamed response bodies with per-chunk platform-timer races bounded by `budget.deadline`, so the streamed body honours the originating deadline end-to-end per §3.3.3. Both also reference `dispatch_budget(req)` rather than an adapter-local formula |
| 30 s no-deadline needed a synthetic absolute deadline | §3.3.2: `dispatch_budget(req) -> DispatchBudget { duration, deadline }` returns **both** the SDK timeout duration AND an absolute `Deadline` — synthetic via `Deadline::after(duration)` if `req.deadline` was `None`. Fastly's between-chunk `is_expired()` check (§3.3.4) and the streamed-body wrappers in §4.1/§4.2/§4.4 all use `budget.deadline`, so cooperative enforcement works uniformly whether or not the caller supplied a deadline |
| `into_bytes_bounded` doc contradicted the streamed-deadline model | §3.1.3 rewritten: the doc now says explicitly that the originating deadline is already honoured by the adapter-wrapped stream, so `into_bytes_bounded` returns 504 on stalled streams without the caller threading the deadline. `_until` is documented as "tighten below the originating deadline," not "re-apply" |
| Hop-by-hop list said `trailers` instead of `trailer` | Replaced everywhere — `from_request` (§3.1.3) and `normalize_for_dispatch` (§3.1.4) now strip `trailer` per RFC 7230 §6.1 |
| UTF-8 header policy needed an implementation guardrail | §3.1.4: validation must use `std::str::from_utf8(value.as_bytes())`, not `HeaderValue::to_str()` (which is stricter than UTF-8 and would drop valid non-ASCII headers like `café`). §5.4 test asserts a valid non-ASCII UTF-8 header survives round-trip plus a `\x80`-byte header is dropped/rejected |
| Stale API references after body rewrite | `http_client()` snippet (§3.1.2) uses `self.parts.extensions.get(..)`; §3.4.5 stale "switch to `body()`" line replaced with the correct `body_kind` / `body_bytes` / `take_body` / `into_request` set; poison semantics use `body_kind() == Poisoned` and `take_body()` semantics; §7 `src/context.rs` file-summary line lists `body_kind`, `take_body`, `form_within`, `into_request`, and the removal of legacy `request()` / `request_mut()` / `json()` / `form()` |

## Appendix I — Review round 9 resolutions

| Review finding | Resolution |
| --- | --- |
| `DispatchBudget.deadline` didn't track the effective budget when both `timeout` and `deadline` were set | §3.3.2 step 5: `deadline` is **always** `Deadline::after(duration)` — i.e. `now + effective_duration` — never the original `req.deadline`. `.timeout(50ms).deadline(5s)` now produces an absolute deadline of `now + 50ms`, and the streamed body / Fastly body-phase use that. New §5.4 test asserts the short-timeout-long-deadline case |
| Streamed request-body drain/write wasn't clearly inside the deadline | §4.1 / §4.2 / §4.4: every adapter races the request-body drain/write against `budget.deadline` (stalled upload → `gateway_timeout`), and **recomputes** the remaining duration from `budget.deadline.remaining()` after the drain — so upload time counts against the budget rather than adding on top. New §5.4 tests for over-cap → 400, stalled upload → 504, drain reduces remaining budget |
| `body_bytes` / `json_within` behaviour after `take_body()` was unspecified | §3.4.5 row: from `Taken`, all buffered helpers return `Err(EdgeError::internal("body already consumed via take_body"))`. New §5.4 test |
| Fastly notes still had stale `min(timeout, deadline.remaining())` and bare `deadline.is_expired()` | §3.3.4 row + Fastly precision paragraph + Fastly pseudocode all updated to `budget.duration` / `budget.deadline.is_expired()`. The synthetic 30 s deadline is honoured uniformly |
| Test plan missed streamed request-body cap and deadline behaviour | §5.4 adds `max_request_body_bytes` over-cap → 400; stalled upload → `budget.deadline` (504); drain time reduces remaining SDK budget |
| Migration sweep missed `ctx.json()` / `ctx.form()` removals | §7 sweep regex updated to include `ctx.json(`, `ctx.form(`, `RequestContext::json`, `RequestContext::form`; known call sites in `context.rs` and `extractor.rs` enumerated |
| Test plan missed valid-non-ASCII-UTF-8 and explicit `trailer` cases | §5.4 adds non-ASCII UTF-8 round-trip row, `\x80` rejection row, and an explicit RFC 7230 `trailer` strip row |
| Stale doc surfaces | §3.1.1 heading changed to "two required methods"; §3.1.3 builder-surface list includes `max_request_body_bytes`; document status header updated to "revised through review rounds 1–8" with the current date |

## Appendix J — Review round 10 resolutions

| Review finding | Resolution |
| --- | --- |
| `dispatch_budget` timeout-only contradiction | §3.3.2 rewritten end-to-end: a single `now` snapshot, candidate **absolute** deadlines (`from_timeout`, `from_caller`, `from_default_only`), effective deadline = min of candidates, duration = `deadline.at - now`. `.timeout(50ms)` with no batch deadline yields `now + 50ms` (not 30 s). Full behaviour table inline |
| Fastly single-`send` streamed request bodies lacked cap/deadline mechanics | §4.3 new bullet — pre-append byte counting against `req.max_request_body_bytes` (over-cap → 400, `StreamingBody` dropped without `finish()`); cooperative between-chunk `budget.deadline.is_expired()` check during upload (stalled → 504, same bounded-cooperative story as the body-read phase); post-upload duration recomputed from `budget.deadline.remaining()` so upload time counts against the budget |
| Fastly `send_all` wall-clock-observed bound overstated for ordered harvest | §3.3.4 new paragraph distinguishing per-slot **result correctness** (host-side, bounded by the slot's own budget) from per-slot **wall-clock-observed delivery** (bounded by `max_over_remaining_slots(effective_at_dispatch)` because harvest is ordered). For uniform-budget fan-outs the bounds coincide; heterogeneous-budget callers are warned |
| `dispatch_budget` could extend an original absolute deadline; `remaining() == None` ambiguity | §3.3.2: single `now` snapshot; expired-deadline check uses `dl.at <= now` directly (no `remaining()` round-trip); duration derived from the chosen absolute deadline and the same `now`, never `Deadline::after(duration)` from a later moment |
| `OutboundRequest` struct snippet missed `max_request_body_bytes` | §3.1.3 struct now lists the field with its default annotation |
| Fastly dynamic-backend warning promised but missing from `ensure_capabilities` | §3.5.3: explicit `if adapter_name == "fastly" && caps.required.contains(&Capability::OutboundHttp)` block in the pseudocode that emits the dynamic-backends `log::info!` reminder |
| Stale "originating deadline" wording | §3.1.3 (`into_bytes_bounded`), §3.3.3 (Streamed body paragraph + practical-implications bullets), and §4.2 / §4.4 / §4.3 adapter notes all rephrased to "**effective-budget deadline**" — wrappers apply for every request regardless of whether `req.deadline` was set |
| Stale "body phase checks `deadline`" line | §3.3.4: replaced with "body phase checks `budget.deadline`" |

## Appendix K — Review round 11 resolutions

| Review finding | Resolution |
| --- | --- |
| `dispatch_budget` pseudocode wouldn't compile against a `Deadline` with a private field | §3.3.1: `Deadline` gains `pub fn instant() -> web_time::Instant` and `pub fn at_instant(instant)`; the pseudocode uses `dl.instant()` / `Deadline::at_instant(now + d)` / `.min_by_key(\|d\| d.instant())` |
| Fastly streamed-upload deadline was overstated | §4.3: deadline enforcement on Fastly streamed uploads is now explicitly **bounded-cooperative *between* yielded chunks only** — a stalled `stream.next().await` cannot be preempted on Fastly (no guest async timer). Apps that need real-time enforcement against an untrusted upload source must use `Body::Once` on Fastly. The capability matrix marks Fastly streamed-upload deadline as `BestEffort` for the stream-source-stall case. §5.4 test row updated to "stalled upload **between** yielded chunks → 504" and explicitly names the BestEffort gap |
| Axum / CF `send_one` had stale operation ordering | §4.1 + §4.2 rewritten as numbered flows: (1) compute budget, (2) drain streamed request body under `budget.deadline`, (3) recompute remaining from `budget.deadline.remaining()`, (4) construct and send platform request. Stale "set timeout then drain later" wording removed |
| Appendix J test rows were outside the §5.4 markdown table | Blank line that broke the table removed at the trailer row → Fastly-upload row boundary |
| Stale "originating deadline" wording in normative areas | `into_bytes_bounded_until` docs, §3.3.2 streamed-mode line, and the §5.4 row all changed to `dispatch_budget(req).deadline` / "effective-budget deadline," explicitly noting the wrapping is unconditional (not gated on `req.deadline.is_some()`) |

## Appendix L — Review round 12 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly `send_all` dropped metadata needed by harvest | §4.3 pseudocode: `Slot::Pending` is now `PendingSlot { pending, budget, response_mode }`; `dispatch(req)` returns `(PendingRequest, DispatchBudget, ResponseMode)`; `harvest(result, &budget, &response_mode)` has everything it needs to enforce body deadline, decompressed-byte cap, and Buffered-vs-Streamed handling per slot |
| Fastly streamed-response deadline was contradictory | §3.1.3 + §4.3: Fastly now wraps streamed response bodies with a **cooperative deadline-aware stream** that checks `budget.deadline.is_expired()` before each yielded chunk and emits `gateway_timeout` past the deadline. Applies to every consumer — `into_bytes_bounded`, `into_bytes_bounded_until`, `into_response()` proxy passthrough — so the deadline cannot be bypassed by choosing a non-helper consumption path |
| Fastly streamed-upload BestEffort gap had no capability hook | §3.5.1 + §3.5.2: new `Capability::StreamedUploadDeadlines` enum variant and `streamed-upload-deadlines` matrix row — `Native` on Axum/CF/Spin, `BestEffort` on Fastly. Apps that need real-time enforcement of stalled `stream.next().await` on uploads declare this required and get a hard build failure on Fastly per the round-5 "required + BestEffort = hard fail" rule |
| `budget.deadline.remaining() == None` after upload was unspecified | §4.1 / §4.2 / §4.3 / §4.4: every adapter explicitly returns `gateway_timeout` *before* constructing/fetching/sending the platform request when the upload consumed the budget |
| the external batch deadline mapping could re-anchor per target | §3.3.2 row rewritten: compute `batch_deadline = Deadline::after(batch_deadline_ms)` **once** at handler entry, then copy that absolute `Deadline` into every target request. The field comment on `OutboundRequest.deadline` (§3.1.3) reinforces the rule. §5.4 has a drift counter-example test |
| RequestContext migration still incomplete around `form_within` and sweep | §3.4.2 API block adds `form_within` (default `1 MiB`, same cache semantics); §7 sweep regex extended to include `fn json<` and `fn form<` for definition sites |
| `Deadline::after` overflow/panic risk | §3.3.1: `Deadline::after(d)` is **saturating** — `Duration::MAX` clamps to the largest representable instant rather than panicking. §5.4 row asserts this |
| Non-UTF-8 request-header policy was split inconsistently | §3.1.4: split is explicit — `OutboundRequest::header(..)` rejects with `bad_request` at construction (loud), `headers_mut()` / `from_request(..)` paths use `normalize_for_dispatch` which **drops + `warn!`** (lossy — doesn't fail an otherwise-good forward over an exotic header). §5.4 covers both paths |

## Appendix M — Review round 13 resolutions

| Review finding | Resolution |
| --- | --- |
| `send_all` contradicted the trait contract for streamed request bodies on Axum/CF/Spin | §4.1 / §4.2 / §4.4: each adapter's `send_all` runs a **preflight** that converts any `Body::Stream` slot to `Err(bad_request)` *before* calling `send_one`. The trait contract (§3.1.1) now holds identically on every adapter — `send_all([stream])` never invokes the single-send drain path; index alignment is preserved |
| Streaming proxy-forward depended on adapter response converters not currently streaming | §7 file-by-file: new `src/response.rs` task per adapter. Replaces today's buffer-then-return paths with platform-native streaming sinks (`axum::body::Body::from_stream`, `worker::Body::from_stream`, Fastly `Response::with_streaming_body`, Spin WASI outgoing-body chunk-writes). Buffering is reserved for `Body::Once` |
| `dispatch_budget` still used raw `now + d` (panic path) | §3.3.2: `saturating(dur)` helper uses `now.checked_add(dur).unwrap_or_else(\|\| now + DEADLINE_FAR_FUTURE)` for every candidate (`from_timeout`, `from_default_only`). `Duration::MAX` no longer panics. §5.4 test on `OutboundRequest::timeout(Duration::MAX)` |
| Adapter capability notes were stale ("Native for all five") | §4.1 / §4.2 / §4.3 / §4.4: each adapter's `capability()` line now enumerates the **six** capabilities (`outbound-http`, `outbound-deadlines`, `streamed-upload-deadlines`, `config-store`, `kv-store`, `secret-store`). Fastly's exact tuple is spelled out: `outbound-deadlines` = `BoundedCooperative`, `streamed-upload-deadlines` = `BestEffort`, the rest `Native` |
| `OutboundDeadlines` enum comment misleadingly excluded streamed responses | §3.5.1: comment now reads "across the *entire exchange*: connect + headers + buffered response body **and** the chunk-yield path of a streamed response body (per §3.3.3)" |
| Host normalization wording split | §3.1.3 `from_request` rewritten — `host` is dropped from headers; the **adapter** derives the final value from `req.uri()` at SDK-construction time (§3.1.4 is the single source of truth); `normalize_for_dispatch` re-strips `host` defensively as a safety net |

## Appendix N — Review round 14 resolutions

| Review finding | Resolution |
| --- | --- |
| Axum lazy response streaming named an unspecified `Send + 'static` shim | §7 + §4.1: Axum's `response.rs` **buffers** `Body::Stream` to `Bytes` within `max_response_bytes` before constructing the axum response — documented Axum-specific limitation, not a fictional shim. Cloudflare / Fastly / Spin keep true lazy streaming (no `Send` requirement in their WASM guests). New `lazy-streamed-response-passthrough` capability (§3.5.1/2) is `Native` on the three WASM adapters and `BestEffort` on Axum; apps that need lazy Axum streaming declare it required → hard build failure today, with the mpsc-bridge follow-up tracked in §8 risk 6 |
| Fastly streamed-upload overstated what is enforced | §4.3 two-phase decomposition: **source-stream yield** (`stream.next().await`) is `BestEffort` (no preemption); **host write** is `BoundedCooperative` (Fastly applies `between-bytes-timeout` to both read-from-origin and write-to-origin per docs); **between writes** the adapter checks `budget.deadline.is_expired()` after each chunk. The capability label `streamed-upload-deadlines = BestEffort` on Fastly reflects the worst phase; the risk section (§8 risk 7) flags the dependency on Fastly's documented host behaviour |
| Saturating deadline semantics inconsistent | §3.3.1 + §3.3.2: one rule everywhere — clamp `dur` to `DEADLINE_FAR_FUTURE = 365 days` *before* adding to `now` (`saturating(dur)` = `now + min(dur, DEADLINE_FAR_FUTURE)`, with `checked_add` belt-and-suspenders). New `pub const DEADLINE_FAR_FUTURE` exposed in the API. Behaviour table now shows the clamp explicitly and adds the `Some(Duration::MAX)` row |
| `send_all` preflight needed adapter-facing introspection | §3.1.3 adds `OutboundRequest::is_stream_body() -> bool` (cheap non-consuming check used by adapter preflights) and `from_parts(OutboundRequestParts) -> Result<Self, EdgeError>` (disciplined round-trip with URI re-validation). Adapter `send_all` bullets call `is_stream_body()` before `send_one` |
| Test plan missed response-converter rewrite | §5.4 adds Tier 3 rows for CF/Fastly/Spin response converters (first bytes flow before upstream stream ends; stream errors after headers surface to client) and an explicit Axum row asserting buffered behaviour with the documented limitation |
| Bounded-memory wording contradicted itself | §3.4.1 reworded: the **persistent collected buffer** is bounded by `max`; worst-case **transient** memory is `max + sizeof(current_chunk)` (the in-flight chunk briefly coexists with the buffer). Not a whole-process ceiling — batch level bound is in §3.4.4 |

## Appendix O — Review round 15 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly `send_all` buffered body drains serialized | §3.3.4 new bullet + §3.2 "Where 'identical' stops being identical" paragraph: explicit, honest documentation that buffered-body drain on Fastly runs in harvest order, so wall-clock = `max(headers) + Σ body_drain_times` vs. `max(headers + body_drain_times)` on Axum/CF/Spin. Small bodies (fan-out batches) are unaffected; large bodies should switch to `Streamed` mode. §8 risk 8 tracks the future interleaved-chunks enhancement |
| Capability metadata inconsistent ("six" / no Fastly tuple) after adding `LazyStreamedResponsePassthrough` | §4.1 / §4.2 / §4.3 / §4.4 `capability()` lines all rewritten to enumerate the **seven** capabilities explicitly. Fastly's tuple is spelled out: `BoundedCooperative` for outbound-deadlines, `BestEffort` for streamed-upload-deadlines, `Native` for the other five |
| Axum buffered fallback had no source for cap | §4.1 + §7 + §3.5.2 footnote 3: introduced `AXUM_RESPONSE_STREAM_BUFFER_BYTES` (defined Axum-adapter constant, default 16 MiB). The per-outbound-request `max_response_bytes` is unavailable by the time the response converter runs; the constant is what the converter uses. Over-cap → 502. Apps that need a different ceiling override the constant at adapter init |
| Streamed error chunks were specified as `EdgeError` but stream is `anyhow::Error` | §7 `src/body.rs` task: **change `Body::Stream`'s error type from `anyhow::Error` to `EdgeError`** so deadline-aware wrappers' `gateway_timeout` chunks survive round-trip without downcasting. In-tree call sites updated mechanically; externally-supplied streams map source errors into `EdgeError::internal(..)` |
| UTF-8 header builder rejected valid non-ASCII | §3.1.4: `OutboundRequest::header(..)` constructs `HeaderValue` via `HeaderValue::from_bytes(value.as_bytes())` (not `from_str`, which is visible-ASCII only), then runs EdgeZero's own `std::str::from_utf8` check. Valid non-ASCII UTF-8 (`café`) round-trips; non-UTF-8 bytes → `bad_request`. Adapter multi-value handling: per-value UTF-8 check, drop only invalid entries, preserve valid siblings (matters for `set-cookie`). §5.4 has the `café` round-trip row |
| Response-converter tests were Tier 3-only | §5.4: response-converter rows for CF/Fastly/Spin (lazy passthrough, stream-error-after-headers) and Axum (buffered cap) are now **Tier 2 as well as Tier 3** — driven by a `MockOutboundClient`-fed stream in-process, so the normal adapter contract suite catches converter regressions without waiting for runtime CI |
| Stale "maximum representable" wording in test row | §5.4: `Duration::MAX` row now asserts the **365-day clamp** to `DEADLINE_FAR_FUTURE`, not an Instant::MAX-style behaviour. Matches §3.3.1/§3.3.2 |

## Appendix P — Review round 16 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly per-slot correctness contradicted the buffered-drain caveat | §3.3.4: per-slot correctness bullet is now explicitly **headers-phase only**; the buffered-body bullet states that a slot can return `gateway_timeout` because earlier slots monopolised harvest, and the `send_all` contract on Fastly **admits harvest-order-induced 504s** in Buffered mode. §5.4 has two rows: headers-phase result correctness, and body-phase harvest-order timeout |
| Streamed-mode "consume chunks concurrently" mitigation had no API | §3.3.4 + §3.2: the Streamed-mode recommendation is **dropped** — Fastly has no concurrent body-drain primitive (no guest reactor), and EdgeZero has no API that recovers parallel large-body fan-out on Fastly. Apps that need that should target a different adapter, restructure their topology, or wait for the interleaved-drain follow-up in §8 risk 8 |
| Header builder signature could not satisfy the UTF-8 rule | §3.1.3: signature changed from `TryInto<HeaderName/Value>` to `AsRef<[u8]>`. The implementation reads bytes, runs the EdgeZero UTF-8 check, then calls `HeaderValue::from_bytes` (not `from_str`). Valid non-ASCII UTF-8 (`café`) round-trips; non-UTF-8 bytes → `bad_request`. `&str`, `String`, `&[u8]`, `Vec<u8>`, `HeaderName`, `HeaderValue` all `AsRef<[u8]>` |
| Post-header stream errors had no defined wire behaviour | §3.1.1 trait rustdoc + §5.4 row: once response headers are sent, HTTP cannot change status, so adapters **abort the downstream body** (TCP close on HTTP/1.1, RST_STREAM on HTTP/2) and `log::warn!` the originating `EdgeError`. Clients observe an early close; the synthetic 502/504 only applies when the error happens before headers go out |
| Public `Deadline::at_instant` bypassed the far-future clamp | §3.3.2 pseudocode: `from_caller` is re-clamped to `now + DEADLINE_FAR_FUTURE` inside `dispatch_budget`. A caller constructing a 100-year `Deadline` via `at_instant` is honoured up to the clamp and no further |
| Fastly backend hash used 64-bit FNV — collision risk for transport identity | §4.3: hash changed to **SHA-256 truncated to 128 bits** (`format!("ez_{:032x}", sha256_128(identity))`). Belt-and-suspenders: in-memory `HashMap<name, identity>` per `send_all` call, fail closed with `EdgeError::internal("dynamic backend name collision — refusing to reuse")` if a name reappears with a different identity |
| `AXUM_RESPONSE_STREAM_BUFFER_BYTES` configurable in prose only | §4.1 + §3.5.2 footnote 3: this is now a **fixed compile-time constant (16 MiB)**, no runtime override. Adding an `AxumOutboundConfig` plumbing layer is tracked in §8 risk 6 alongside the mpsc-bridge follow-up |

## Appendix Q — Review round 17 resolutions

| Review finding | Resolution |
| --- | --- |
| `outbound-deadlines` Fastly claim conflicted with harvest-order false 504s | §3.5.1: new capability `send-all-slot-isolation` separates "each slot's result reflects what it would have produced in isolation" from the single-exchange deadline guarantee. Matrix marks it `Native` on Axum/CF/Spin and `BestEffort` on Fastly. `outbound-deadlines` footnote 1 now explicitly scopes the Fastly `BoundedCooperative` claim to single `send` + headers phase of `send_all`; the cross-slot body caveat is owned by footnote 4 (the new capability). One label, one meaning |
| Risk 8 recommended an impossible Fastly mitigation | §8 risk 8 rewritten: there is **no** EdgeZero mitigation that recovers parallel large-body fan-out on Fastly. Apps target a different adapter, restructure the topology, or wait for the interleaved-drain follow-up. The Streamed-mode-consume-concurrently text is gone. Cross-reference to `send-all-slot-isolation` so the build-time enforcement is discoverable |
| Behaviour table didn't reflect `at_instant` clamp | §3.3.2: table rows for `req.deadline = Some(d)` use `clamped(d) = Deadline::at_instant(d.instant().min(now + DEADLINE_FAR_FUTURE))` instead of raw `d`. New row covers the 100-year `at_instant` case landing on the 365-day clamp |
| Fastly pseudocode comment said "~max(latency), not the sum" | §4.3 pseudocode comment updated: headers phase is `~max(header_arrivals)`; buffered body drain runs serially in harvest order, so total wall-clock is `~max(header_arrivals) + Σ body_drain_times`. Matches §3.3.4 |
| Spin wildcard `*` only rendered HTTPS | §3.5.4: wildcard now renders both schemes — `["https://*:*", "http://*:*"]` — matching the "any host" semantics and the http loopback contract tests. Specific bare hosts still default to https |
| §3.1.4 prose used `.as_bytes()` after signature switched to `AsRef<[u8]>` | §3.1.4: `value.as_bytes()` → `value.as_ref()` so the prose matches the builder's actual `AsRef<[u8]>` bound (which covers `&[u8]`, `Vec<u8>`, `HeaderValue`, in addition to `&str` / `String`) |
| Fastly collision detection was per-`send_all` only | §4.3: the collision-detection `HashMap<name, identity>` lives on the `FastlyOutboundClient` itself (one per request context) and applies to single `send`, `send_all`, and across calls. `Backend::builder` returning `NameInUse` is caught and the registered identity is verified — match → reuse, mismatch → fail closed with `EdgeError::internal` |

## Appendix R — Review round 18 resolutions

| Review finding | Resolution |
| --- | --- |
| `send_all-slot-isolation` would not deserialize (kebab-case mismatch) | Renamed to `send-all-slot-isolation` everywhere — matrix, footnote, prose, test rows, enum doc. `#[serde(rename_all = "kebab-case")]` now produces the same string the spec uses |
| Fastly dynamic backend identity omitted timeout settings | §4.3: identity tuple is now `scheme + ":" + host + ":" + port + ":" + tls_mode + ":" + budget_ms` — distinct budgets to the same host get distinct dynamic backends, so a 50 ms slot and a 3 s slot don't silently share one timeout config. Homogeneous-budget fan-out batches still share one backend per host. Per Fastly's `BackendBuilder` docs, dynamic backend names cannot duplicate in a session and sameness includes settings — the identity must reflect every setting |
| `capability()` tuples missing `send-all-slot-isolation` on every adapter | §4.1 / §4.2 / §4.3 / §4.4 `capability()` lines updated to enumerate **eight** capabilities. Fastly's tuple is `outbound-deadlines = BoundedCooperative`, `send-all-slot-isolation = BestEffort`, `streamed-upload-deadlines = BestEffort`, the rest `Native`. Axum / CF / Spin are `Native` for `send-all-slot-isolation` |
| Trait `send_all` doc still said "behaves identically across adapters" | §3.1.1 trait rustdoc adds an "Identical scope" paragraph: identical is **input/output contract** (preflight, index alignment, per-slot Ok/Err shape); cross-slot timing is governed by `send-all-slot-isolation`. §3.2 paragraph also rewritten to match |
| `RequestContext::into_request()` silently returned `Body::empty()` for Poisoned/Draining | §3.4.5: `into_request() -> Result<Request, EdgeError>` is now **fallible**. `Draining` → `internal`; `Poisoned(err)` → `Err(err.clone_as_edge_error())`; only `Taken` returns `Ok(Body::empty())` (the caller already consumed the body explicitly). A poisoned read can no longer silently become an empty proxy-forward |
| Test plan missed the new capability's critical behaviour | §5.4: added rows for (a) required `send-all-slot-isolation` on Fastly → hard build fail; (b) Fastly same-host mixed-budget `send_all` → distinct backends per `budget_ms` (catches the timeout-identity bug); (c) `into_request()` after poison returns `Err`, not empty |

## Appendix S — Review round 19 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly streamed-upload "remaining-budget host timeout adjustment" overclaimed | §4.3: the post-upload bullet is honest now — Fastly sets host timeouts once at dispatch and the SDK does not expose mutation, so for `send_async_streaming` the response-phase host timeout is locked to `budget.duration`. The adapter checks `budget.deadline.is_expired()` cooperatively before `wait()` (drop + 504 if exhausted), but a non-expired remaining of e.g. 10 ms can still be followed by up to one between-bytes-timeout of host blocking — the same `BoundedCooperative` overshoot bound. Apps that need tight end-to-end wall-clock pass a buffered request body |
| Test plan asserted impossible Fastly "returns before constructing/sending" | §5.4: the upload-budget-exhaustion row is split per-adapter. Axum/Cloudflare buffer the streamed request body before constructing the platform request, so a budget-exhausted drain genuinely returns *before* sending. Fastly's `send_async_streaming` and Spin's WASI outgoing-body both begin sending while chunks flow, so **partial upstream send is expected** on those two — the test asserts that contract honestly rather than the impossible "no partial send anywhere" claim |
| Fastly upload deadline check missed the resumed-after-deadline case | §4.3: the "Around each chunk" bullet now requires **two** `budget.deadline.is_expired()` checks per iteration — once immediately after `stream.next().await` returns and **before** `write_all` (catches a stream that stalled past the deadline and then yielded), and once after the successful `write_all` / `flush()` (catches a write that pushed the budget over). |
| Stale "into_request returns Body::empty()" test row | §5.4: row 1947 rewritten — `into_request()` after poison returns `Err(stored_err)`, matching §3.4.5 and the round-18 fallible-`into_request` change |
| `budget_ms` could collapse sub-millisecond budgets to 0 | §4.3: identity tuple uses `max(1, dispatch_budget(req).duration.as_millis())` — a 100 µs and a 900 µs slot don't share a backend with `0 ms` timeouts. Apps wanting sub-ms wall-clock should not target Fastly (host between-bytes-timeout itself is ms-granular) |
| Appendix Q missing — file jumped P → R with an orphan table | Added the `## Appendix Q — Review round 17 resolutions` heading before the orphan table; round-17 and round-18 appendices are now correctly numbered and ordered |

## Appendix T — Review round 20 resolutions

| Review finding | Resolution |
| --- | --- |
| Streamed response decompression was underspecified | §3.4.1: explicit **streaming-decompressor design** — each WASM adapter wraps the platform raw byte stream with an incremental decoder (`flate2::read::GzDecoder` for gzip, `brotli::Decompressor` for brotli) configured chunk-at-a-time, counts decompressed bytes against the cap, and strips `content-encoding` / `content-length` at construction. Lazy passthrough + decompressed-byte caps + correct header stripping all hold simultaneously. Axum buffers anyway, so a non-streaming decoder is fine there |
| `budget_ms` was floored, not ceiled | §4.3: identity tuple uses **true ceil-to-ms** — `((duration.as_nanos() + 999_999) / 1_000_000).max(1)`. A 1.9 ms budget no longer becomes 1 ms. The same ceiled value is what's fed into the host timeouts, so the identity tuple and the actual host configuration always match. The §3.3.4 "host timeouts = `budget.duration`" wording is documented as shorthand for ceil-to-ms; the body-phase `budget.deadline.is_expired()` check still uses the exact original `Deadline` |
| Fastly backend collision map wasn't implementable | §4.3: the field is `Mutex<HashMap<String, (BackendIdentity, Backend)>>` — interior mutability with `Send + Sync`. The map stores the registered `Backend` handle so subsequent calls skip a fresh host call. **The lock is not held across host calls**: build the backend first, then insert under the lock; on concurrent duplicate-with-same-identity the extra handle is discarded; on duplicate-with-different-identity the adapter fails closed |
| Stalled streamed-upload test row overclaimed uniform behaviour | §5.4 row split into two: **host-write phase** stops at `budget.deadline` on every adapter (Axum/CF/Spin platform timer; Fastly host between-bytes-timeout); **source-pull phase** preempts on Axum/CF/Spin but **cannot preempt on Fastly** (BestEffort per `streamed-upload-deadlines`). No false uniform claim |
| `BestEffort` definition was timing-specific but covers Axum's deterministic-buffer case | §3.5.1: `CapabilitySupport::BestEffort` doc broadened — "available with a documented limitation; can be timing (unbounded cooperative) **or functional** (deterministic behaviour differs from `Native`, e.g. Axum buffers a body that other adapters stream)." CLI error text in §3.5.3 mirrors the broadened meaning |
| Older appendices contained superseded claims | Added the "Appendix index — historical, not normative" note before Appendix A: the round-by-round appendices are a paper trail; the authoritative content is §1–§8, and active sections win when an older appendix entry disagrees. No per-entry retroactive edits — the index disclaimer covers the whole history |

## Appendix U — Review round 21 resolutions

| Review finding | Resolution |
| --- | --- |
| Streamed decompressor had undefined cap ownership | §3.4.1 rewritten: the decoder **only decodes / strips compressed-only headers / surfaces decode errors** — no byte counting in the wrapper. Cap ownership is explicit: Buffered → adapter helper; Streamed + `into_bytes_bounded` → helper's own pre-append check; Streamed + `into_response()` passthrough → **deliberately no EdgeZero cap** (the platform wire is the budget; capping a transparent proxy stream would silently truncate). Removes the `ResponseMode::Streamed has no max_bytes` / "decoder enforces cap" conflict |
| Fastly streamed-upload test rows asserted exact `budget.deadline` for host-write stalls | §5.4: the host-write row now distinguishes Axum/CF/Spin ("at the deadline, real preemption") from Fastly ("within one between-bytes-timeout past `budget.deadline` — bounded overshoot, BoundedCooperative"). The source-pull row keeps its existing per-adapter split |
| Spin's `streamed-upload-deadlines = Native` source-pull guarantee was not specified | §4.4 streamed-request-bodies bullet: **two distinct races** — (1) `futures::select!` around `source_stream.next()` against a wasi monotonic-clock timer (this is what makes the source-pull preemption real on Spin); (2) host-write race around `OutgoingBody::write` against the same timer. The `Native` label now has a spec to point at, not just a claim |
| Fastly ceil-to-ms helper inconsistent across sections | §3.3.4 introduces `fn fastly_timeout_ms(budget) -> u64` (true ceil-to-ms, with `max(1, ..)`) and uses it for `set_connect_timeout_ms` / `first_byte_timeout` / `between_bytes_timeout`. §4.3 dynamic-backend identity uses the same helper, so identity and host configuration always match. The earlier "= `budget.duration`" wording is replaced |
| Streamed decompressor guidance bypassed the repo's existing async helpers | §3.4.1 implementation-hooks paragraph: the migration **evolves** the existing async decoders at `compression.rs:15` / `41` (change their error type from `anyhow::Error` to `EdgeError` per round 15, then lift them into a shared core module reused by CF/Fastly/Spin) rather than writing new `flate2::read::GzDecoder` / `brotli::Decompressor` wrappers from scratch |

## Appendix V — Review round 22 resolutions

| Review finding | Resolution |
| --- | --- |
| `send_all` + `Streamed` responses broke isolation/deadline | §3.1.1 + §4.1 / §4.2 / §4.3 / §4.4 preflight: any request with `response_mode = Streamed` yields `out[i] = Err(EdgeError::bad_request(..))` *before* `send_one` is invoked. `send_all` is now buffered-only on **both** sides — request body **and** response. Removes the cross-slot streamed-body deadline-lifetime hazard by construction; `send-all-slot-isolation = Native` on Axum/CF/Spin stays honest. Streamed responses use single `send` and the app orchestrates concurrency itself on reactor-bearing adapters |
| Fastly timeout setters were on the wrong type (not on `Request`) | §3.3.4 pseudocode now configures timeouts on `BackendBuilder` per Fastly 0.12.1 docs: `Backend::builder(&name, &host).connect_timeout(t).first_byte_timeout(t).between_bytes_timeout(t).finish()?`. Same `t = Duration::from_millis(fastly_timeout_ms(&budget))` is also folded into the dynamic-backend identity (§4.3), so the cached `Backend` and a freshly-built one always carry identical timeouts |
| "Homogeneous-budget shares one backend" was not actually guaranteed | §3.3.2: `dispatch_budget(req, now)` now takes `now` as a parameter (not snapshotted internally). `send_all` takes **one** `now` snapshot at the start of the call and passes it to every per-slot `dispatch_budget`, so a shared caller `Deadline` produces the same `duration` and the same ceiled `budget_ms` for every slot — and therefore one backend identity per host. §4.3 spells out the dependency as a normative requirement, not an optimisation |
| Fastly stalled-upload "between yielded chunks" row claimed exact `budget.deadline` | §5.4: row now says "504 **within one between-bytes-timeout past `budget.deadline`** — bounded overshoot, BoundedCooperative — not exact deadline." Matches §3.3.4 and the §4.3 between-write check semantics |
| Streamed decompressor implementation hook pointed at the wrong file | §3.4.1: implementation-hooks paragraph no longer pins a Spin path; it says the async decoders are at `compression.rs:15` / `41` inside one of the adapters (Spin's `decompress.rs` is a separate buffered slice decoder, not the async helper). §7 migration sweep includes a one-line audit step to confirm the actual source file before the refactor |

## Appendix W — Review round 23 resolutions

| Review finding | Resolution |
| --- | --- |
| Stale `dispatch_budget(req)` call signature in adapter notes | §4.1 / §4.2 / §4.3 pseudocode now use `dispatch_budget(req, batch_now)` / `dispatch_budget(req, now)`. Each `send_all` flow snapshots `let batch_now = web_time::Instant::now()` once before fanning out; per-slot `send_one` calls accept and use that `now`. `send` (single request) snapshots inline. The Fastly backend identity guarantee depends on this — explicit in §4.3 |
| "One concurrency primitive" vs `send_all` rejecting Streamed wasn't reconciled | §3.4.4 batch memory model: dropped the Streamed-mode row entirely — `send_all` is buffered-only on both sides, so there is no `send_all`-with-`Streamed` memory model. The single-`send` Streamed path is the explicit non-portable lane for lazy bodies. Older "switch to Streamed mode" guidance is now confined to historical appendices |
| `send_all` preflight needed `is_stream_response()` accessor | §3.1.3 adds `OutboundRequest::is_stream_response() -> bool` alongside `is_stream_body()`. Adapter preflights call both, reject either to `bad_request`, never consume the request |
| Fastly `send_all` pseudocode still carried `ResponseMode::Streamed` through harvest | §4.3 pseudocode rewritten: `PendingSlot` carries `max_bytes: usize` (not `ResponseMode`), because preflight rejects Streamed before dispatch. The dispatch helper returns `(PendingRequest, DispatchBudget, usize)` and harvest comments confirm only Buffered survives. `batch_now` is explicit at the top of the function |
| Manifest `[capabilities.outbound].hosts` validation was promised but not modelled | §3.5.1: `ManifestOutboundCapability::hosts` gains `#[validate(custom(function = "validate_outbound_hosts"))]`, a custom validator that walks each entry through the §3.5.4 accepted-form table — wildcard, scheme-prefixed (`http`/`https` only), `host:port`, bare host (DNS label or `*.subdomain`). Empty strings / bad schemes / missing authorities all reject at manifest-load time. §5.4 covers the cases |
| Test matrix missed `stream_response()` + `send_all` rejection | §5.4 adds rows for `is_stream_response()` accessor truthiness and for `send_all` rejecting `stream_response()` requests with per-slot `bad_request`. Tier 1 + Tier 2. Also adds the shared-`now` test that catches the backend-identity drift bug |
| Streamed response cap-ownership prose was inconsistent | §3.1.1 trait rustdoc rewritten: over-cap on streamed bodies comes from bounded helpers (`into_bytes_bounded[_until]`, `json_bounded[_until]`) or Axum's response converter — NOT from raw `into_response()` passthrough, and NOT from the streaming decoder (which deliberately does no byte counting per §3.4.1). The trait, §3.4.1, and the streamed-body wrapper now agree |
| Decompressor hook pointed at an adapter when the helpers live in core | §3.4.1: implementation-hooks paragraph now says the decoders **live in `edgezero-core` at `compression.rs:15` / `41`** and the migration **evolves them in place** (no lift, no relocation). CF/Fastly/Spin converters call into the existing core helpers |

## Appendix X — Review round 24 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly HTTPS dynamic backends weren't actually configured for HTTPS | §3.3.4 builder example now configures SSL per `tls_mode`: `Tls` → `.enable_ssl().sni_hostname(host).check_certificate(host)`; `Plain` → `.disable_ssl()`; `override_host(host)` in both. Generalises the existing pattern at `crates/edgezero-adapter-fastly/src/proxy.rs:120`. Identity tuple already includes `tls_mode` (§4.3) so cached and fresh backends match SSL config |
| `DEADLINE_FAR_FUTURE = 365 days` exceeded Fastly's `u32` ms ceiling | §3.3.1: clamp reduced to **7 days**, well under Fastly's ~49.7-day limit (`u32::MAX` ms). `fastly_timeout_ms` adds a `debug_assert!` + `min(u32::MAX - 1)` belt-and-suspenders saturation in case the clamp is bypassed elsewhere. Behaviour table and test rows updated; no legitimate caller is affected |
| Spin and §3.3.4 still used stale `dispatch_budget(req)` signature | §4.4 mirrors Axum/CF: `send_all` snapshots `let batch_now = web_time::Instant::now()` once; private `send_one(req, now)`; single `send` snapshots inline. §3.3.4 Fastly precision sample code now uses `dispatch_budget(req, now)` |
| SHA-256 backend-name hash needed an explicit dependency | §7 Fastly file-summary entry now adds **`sha2` workspace dependency** to `edgezero-adapter-fastly/Cargo.toml`, with the audit step "if `edgezero-core` already exposes a SHA-256 helper, use that instead." Either way the dep is declared in this migration, not assumed transitive |
| "One concurrency primitive" overclaim after Streamed got rejected | §1.4 locked-decision reworded to **"one portable buffered fan-out primitive"** — streamed-response fan-out is explicitly non-portable; single `send` is the path for streamed responses on reactor-bearing adapters (Axum/CF/Spin). §8 risk 8 no longer suggests Streamed mode as a `send_all` workaround |
| `BestEffort` CLI text said "no documented bound" but the broadened def covers functional deviations | §3.5.3 bullet rewritten: required + BestEffort fails because BestEffort means a **documented deviation from Native** (timing OR functional). The matrix footnotes describe the specific deviation per capability |
| Host/authority handling didn't specify non-default ports | §3.1.3 `from_request` doc + §5.4 row: `Host` includes the explicit port when the URI carries one (`http://localhost:3000` → `Host: localhost:3000`; `https://example.com` → `Host: example.com`). Adapters derive from `req.uri()` and never re-read `req.headers()` |

## Appendix Y — Review round 25 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly dynamic backend construction dropped explicit ports | §3.3.4 builder example splits the URI into **three distinct values** — `backend_target = "host:port"` (passed to `Backend::builder` as the connection target, generalising the existing `host_with_port` precedent at `crates/edgezero-adapter-fastly/src/proxy.rs:108`), `host_authority = req.uri().authority()` (passed to `.override_host()` so the outgoing Host header keeps explicit ports per §3.1.3), and `sni_hostname = req.uri().host()` (passed to `.sni_hostname()` / `.check_certificate()` — SNI and certificate verification are not port-qualified). §5.4 Fastly SSL/override row updated to assert all three values on `https://example.com:8443` and `http://example.com:8443` |
| §3.3.4 stale `dispatch_budget(req)?` sample | The Fastly precision sample now explicitly snapshots `let now = web_time::Instant::now();` and calls `dispatch_budget(req, now)?`, with a comment clarifying single `send` snapshots inline while `send_all` passes `batch_now` (round 23) |
| `DEADLINE_FAR_FUTURE = 365 days` references in prose | Active prose updated to 7 days — `Deadline::after` doc comment, `dispatch_budget` saturating-helper comment, "100-year via at_instant" sentence in §3.3.2. Historical appendix entries retain the original 365-day language per the appendix-index note (round 20) |
| `send_all` rustdoc "per `ResponseMode`" was stale | §3.1.1: per-slot `Ok`/`Err` paragraph rewritten to say surviving slots match `send`'s **Buffered-mode** semantics — streamed-mode `Ok`-means-headers-only doesn't apply because preflight rejects streamed responses |

## Appendix Z — Review round 26 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly backend identity didn't actually pin Host override | §3.1.3 constructors now **canonicalize** the URI: userinfo is **rejected** (`bad_request`) so credentials never end up in `override_host`; default ports (`:443` for https, `:80` for http) are normalised away so `https://example.com` and `https://example.com:443` produce identical `OutboundRequest`s. With canonicalization in place the §4.3 identity tuple `(scheme, host, resolved_port, tls_mode, budget_ms)` is sufficient — the Host override is a deterministic function of those fields, not a separate input. §5.4 adds the two parity tests |
| §3.3.4 stale `dispatch_budget(req)?` normative prose | The "Fastly precision" paragraph now says `dispatch_budget(req, now)?` with the explicit note: single `send` snapshots `now` inline, `send_all` passes `batch_now`. Matches the code block immediately below |
| §7 Fastly file summary missing round-25 three-value split | §7 Fastly entry rewritten to spell out the three-value split — `Backend::builder(name, "host:port")` connection target, `.override_host(host_authority)` for the Host header (canonicalized authority, ports preserved when non-default), `.sni_hostname(sni_host).check_certificate(sni_host)` for SNI/cert (host-only). Matches the §3.3.4 sample and §5.4 test row |
| `send-all-slot-isolation` footnote 4 gave the wrong "consumer unaffected" reason | The shared-deadline reason was a non-sequitur — §3.3.4's harvest-order false 504s can happen even with one deadline. The footnote now says **typical small-body fan-outs are unaffected because fan-out response bodies are expected to be small** (the external batch protocol JSON, sub-millisecond drain hostcalls), making the serial-drain wall-clock negligibly different from concurrent |
| `DEFAULT_*` constants used but not declared in active API snippets | §7: `src/time.rs` summary now lists `pub const DEFAULT_NO_DEADLINE_BUDGET = Duration::from_secs(30)` and `pub const DEADLINE_FAR_FUTURE = Duration::from_secs(7 * 24 * 60 * 60)`. `src/outbound.rs` summary now lists `pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1 MiB` and `pub const DEFAULT_OUTBOUND_REQUEST_BODY_BYTES: usize = 8 MiB`. Implementers have a single place to copy from |

## Appendix AA — Review round 27 resolutions

| Review finding | Resolution |
| --- | --- |
| `[capabilities.outbound].hosts` validator was too permissive | §3.5.1 `validate_outbound_hosts` doc rewritten as **host-authority-only plumbing**: rejects userinfo (`https://u:p@x`), path (`/p`), query (`?q`), fragment (`#f`), out-of-range / non-numeric ports, and any scheme other than `http`/`https`. Accepts wildcards, IPv6 (`https://[::1]`), `host:port`, scheme-prefixed forms. §5.4 row enumerates every reject and accept case |
| Cloudflare streamed-request upload path was ambiguous | §4.2 capability bullet clarified: `worker::Body::from_stream` is for the **response-out direction** (`lazy-streamed-response-passthrough`). The **outbound-request upload** still drains `Body::Stream` to `Bytes` first per `send_one`'s flow — `send_async`-style streamed uploads aren't part of this migration, and the worker SDK's request-body shape differs from `Body::from_stream`. The bullet now explicitly says "don't conflate the two" |
| URI canonicalization didn't include scheme/host case | §3.1.3 adds **lowercase scheme + host** to the canonicalization steps (per RFC 3986 §3.1 / §3.2.2 — both are case-insensitive). `https://EXAMPLE.com`, `HTTPS://example.com`, `https://example.com` produce identical requests; path / query / fragment remain case-preserving (they're case-sensitive per spec). §5.4 adds the parity test |
| §1.4 locked decision still said `send_all` "behaves identically" | Reworded: input/output contract is identical (preflight, index alignment, Ok/Err shape); **cross-slot timing is not uniform** — Fastly's body drain runs serially in harvest order. `send-all-slot-isolation` is the capability that lets apps require the stricter guarantee. Matches §3.1.1 / §3.2 / §3.3.4 |
| Compression hook said decoders return `anyhow::Error`; they actually return `io::Error` | §3.4.1: implementation-hooks paragraph corrected. The migration wraps each `io::Error` chunk with `EdgeError::bad_gateway(..)` (decode-side IO failure → 502), distinct from the `gateway_timeout` chunks the deadline wrapper injects |

## Appendix AB — Review round 28 resolutions

| Review finding | Resolution |
| --- | --- |
| `batch_now` froze `budget.duration` before preflight / dispatch work | §4.3 adds an explicit **"Dispatch-overhead slack, documented"** paragraph: backend identity uses the bucketed `budget_ms` (host enforces it from SDK arming time, so dispatch-overhead lets a request live up to `now_at_send_async − batch_now` ms past the absolute deadline on the dispatch+headers phase). Body drain still does cooperative `is_expired()` checks (§3.3.4). §4.4 Spin updated to use **`budget.deadline.remaining()`** at the moment the SDK timer is armed, matching Axum/CF's step 3 (round 23). Apps needing exact dispatch+headers absolute-deadline enforcement target a non-Fastly adapter |
| Capability enforcement omitted `edgezero dev` | §3.5.3 + §7: `ensure_capabilities` now runs in `handle_build`, `handle_serve`, `handle_deploy`, **and `handle_dev`** (the dev command implicitly selects Axum via `dev_server::run_dev` / `try_run_manifest_axum`; manifests requiring `lazy-streamed-response-passthrough` must fail there too) |
| URI canonicalization and Spin host plumbing didn't share canonical spelling | §3.5.4: Spin host rendering **first canonicalizes** each entry by the same rules `OutboundRequest` applies to its URI (§3.1.3) — lowercase scheme/host, strip default ports, userinfo/path/query/fragment already rejected by the §3.5.1 validator. The "fallback `scheme://authority` Spin accepts" prose is removed: the validator is authoritative. Rendered `spin.toml` matches what `OutboundRequest::uri()` reports |
| Case-normalization claimed fragments are passed through; `http::Uri` truncates | §3.1.3: **fragments are rejected** at construction with `bad_request("outbound URI must not contain a fragment")`. Silent truncation surprise is gone. Case-preserving claim now applies only to path and query (which `http::Uri` does preserve, and which RFC 3986 leaves case-sensitive) |
| `get`/`post` `TryInto<Uri, Error = InvalidUri>` excluded already-built `Uri` | §3.1.3: signature loosened to `T: TryInto<Uri>, T::Error: core::fmt::Display`. Now accepts `&str`, `String`, **`Uri`** (whose `try_into::<Uri>` has `Error = Infallible`, which does implement `Display`), and any other sensible TryInto. Error message goes into `EdgeError::bad_request` via the `Display` bound. (Round 29 then changed this further to `impl AsRef<str>` for fragment detection — see Appendix AC) |

## Appendix AC — Review round 29 resolutions

| Review finding | Resolution |
| --- | --- |
| Fragment rejection wasn't enforceable through generic `TryInto<Uri>` | §3.1.3: `get`/`post` signature changed to `impl AsRef<str>` — the raw input string is available for `#` detection *before* `http::Uri` truncates. Fragment rejection is now real for string inputs. `new(Method, Uri)` accepts a `Uri` that has already lost the fragment; the asymmetry is documented loudly: use `get`/`post` when constructing from a raw string and you get fragment rejection for free |
| Fastly dispatch-overhead slack weakened `BoundedCooperative` | §4.3 + §7: introduced `pub const BATCH_DISPATCH_SLACK_MAX = Duration::from_millis(25)`. Before each slot's `send_async`, the adapter asserts `Instant::now() - batch_now <= BATCH_DISPATCH_SLACK_MAX`; over-budget slots fail closed with `EdgeError::internal(..)`. Slack is a **hard-bounded constant**, not "scales with preflight." Net guarantee: dispatch+headers overshoot ≤ 25 ms + `budget_ms`; body-phase overshoot ≤ one between-bytes-timeout. Both terms deterministic and testable, so `outbound-deadlines = BoundedCooperative` on Fastly is honest |
| Test matrix stale relative to recent rounds | §5.4 rows updated: case-preserving claim drops "fragment" (now rejected); fragment-rejection row added; `edgezero dev` capability-enforcement row added; Spin canonical-rendered-output row added; Fastly dispatch-overhead-slack row added |
| Manifest accepting uppercase schemes was ambiguous | §3.5.4 makes the canonicalization order explicit: the §3.5.1 validator accepts uppercase schemes/hosts (RFC 3986 says they're case-insensitive), and the §3.5.4 Spin renderer canonicalizes to lowercase before emitting `spin.toml`. `HTTPS://EXAMPLE.com:443` → accepted → rendered as `https://example.com` |
| Appendix index stale (said A–S, file extends through AB+) | Index note updated to "A–AC (and counting)" with an explicit pointer to the last `## Appendix` heading — keeps the historical-vs-normative boundary trustworthy without requiring per-round edits to the index |

## Appendix AD — Review round 30 resolutions

| Review finding | Resolution |
| --- | --- |
| Validator said "scheme must be lowercase" while the Spin render accepts uppercase | §3.5.1 validator doc rewritten: scheme matching is **case-insensitive** at the validator (RFC 3986 §3.1) — `HTTPS`, `https`, `Https` all accepted. The §3.5.4 Spin renderer then canonicalizes to lowercase before emitting `spin.toml`. One canonical spelling in the rendered manifest |
| Fastly capability footnote understated the new dispatch slack | §3.5.2 footnote 1 rewritten: `BoundedCooperative` on Fastly has **two documented bounds** — single `send` (zero dispatch drift, body ≤ one between-bytes-timeout) and `send_all` (dispatch+headers ≤ `BATCH_DISPATCH_SLACK_MAX + ms_rounding ≈ 26 ms`, body ≤ one between-bytes-timeout). §4.3 corrects the bound to dispatch delay + ms rounding |
| §6 migration checklist omitted `handle_dev` | §6 CLI bullet lists **`handle_build`, `handle_serve`, `handle_deploy`, and `handle_dev`**. Matches §3.5.3 + §7 |
| Header-value wording overclaimed "exactly valid UTF-8" | §3.1.4: spelled out as **valid UTF-8 *and* valid HTTP header-value bytes** — `HeaderValue::from_bytes` rejects control bytes (`\n`, `\0`, etc.) for header-injection prevention. Two distinct error messages: forbidden-bytes vs invalid-UTF-8 |
| `time.rs` doc said "Deadline is the only thing" | §3.3.1 Deadline doc updated to list the full module contents: `Deadline`, `DispatchBudget`, `dispatch_budget`, public timing constants. The §3.3.5 constraint is "no runtime/timer/platform dep in core," not "value type only" |

## Appendix AE — Review round 31 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly dispatch+headers worst case was ~2× the claimed bound | §3.3.4 / §4.3: the budget is now **phase-split** — `connect_timeout = budget * 1/4`, `first_byte_timeout = budget * 3/4`, `between_bytes_timeout = budget`. Their sum equals `budget.duration`, so the dispatch+headers host enforcement is bounded by `budget.duration` plus `BATCH_DISPATCH_SLACK_MAX + ms_rounding`. The earlier "both set to `t`" wording would have been ~2×; spelled out in the §3.3.4 paragraph and the code block. §5.4 row asserts a single `send` to a connect-hang target fires within `budget.duration + ms_rounding`, not twice |
| Dispatch-slack test couldn't exercise the guard from handler code | §4.3 + §5.4: the test uses an **adapter-internal `#[cfg(test)]` injection hook** (a `Fn`-slot on `FastlyOutboundClient`) invoked between `batch_now` capture and per-slot `dispatch()`. A handler-side `thread::sleep` before `send_all` is explicitly insufficient because it runs before `batch_now` is captured; the test row spells this out |
| Header-value builder doc contradicted §3.1.4 | §3.1.3 builder step 3 rewritten: "values that survive are exactly the ones that are **both** valid UTF-8 **and** valid HTTP header bytes" — a valid-UTF-8 string with a forbidden control byte (`\n`, `\0`) still rejects. Two distinct error messages. §5.4 adds the `\n`/`\0` row (header-injection vectors) |
| Axum response converter stream-error behavior was underspecified | §4.1 response.rs paragraph: full mapping table — `GatewayTimeout` chunk → 504, `BadGateway` chunk → 502, over-cap → 502, other `EdgeError` → its own `status()`. The buffering boundary (no headers yet written) is what enables the clean status mapping, unlike the streaming-passthrough adapters which can only abort the wire after headers. §5.4 row covers each branch |
| Generic BestEffort enforcement test row mentioned only build/deploy | §5.4: row extended to "every adapter-selecting CLI command — `build`, `serve`, `deploy`, `dev` — exits non-zero." Matches §3.5.3 |

## Appendix AF — Review round 32 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly `send_all` opportunistic poll lost `Slot::Done(Err(..))` slots | §4.3 pseudocode: the inner `for j in (i+1)..n` loop now matches **all three** variants — `Slot::Done(r)` preserves preflight/dispatch errors into `out[j]`, `Slot::Taken` is a no-op, only `Slot::Pending(s2)` runs the `poll()` path. Index-aligned per-slot errors survive intact; the generic "slot unresolved" internal error is reserved for true contract bugs |
| 1/4 connect + 3/4 first-byte split causes premature connect failures inside the caller's total budget | §3.3.4 / §4.3: documented explicitly — the split preserves the absolute-deadline upper bound at the cost of the "slow-connect-but-fast-everything-else fits in budget" property. A 1 s `send` with a 300 ms connect fails at the `250 ms` connect slice. §5.4 adds the row that asserts this exact deviation (not just "not 2×"). A configurable phase split is a future change; for now apps that hit it target a different adapter |
| Fastly timeout prose inconsistent + edge case at sub-4 ms budgets | §3.3.4 row + §4.3 code: prose now says "phase timers split per §4.3," not "= `budget.duration`." Code handles `total_ms < 4` by setting `connect = first_byte = total_ms` (the absolute bound degenerates to 2× at sub-4 ms scale where ms rounding dominates anyway). `connect_ms + first_byte_ms == total_ms` for `total_ms ≥ 4` |
| IPv6/IP-literal HTTPS behaviour on Fastly was unspecified | §4.3 code: for IP-literal hosts (`https://[::1]`, `https://127.0.0.1`) the adapter **skips** `.sni_hostname()` (SNI is DNS-only per RFC 6066) and passes the bracket-stripped form to `.check_certificate()` (IP-literal cert verification mode). DNS-name hosts call both setters as before. §5.4 adds the dedicated test row |
| §7 omitted core extractor + compression files | §7 `crates/edgezero-core` block now lists `src/extractor.rs` (extractor migration, `DEFAULT_INBOUND_JSON_BYTES = 8 MiB`, `DEFAULT_INBOUND_FORM_BYTES = 1 MiB`, `ValidatedJsonWithin` / `ValidatedFormWithin`) and `src/compression.rs` (evolve in place — error type `io::Error` → `EdgeError::bad_gateway`, shared by CF/Fastly/Spin response converters) |
| Dispatch-slack diagnostic blamed handler CPU | §4.3 paragraph rewritten: diagnostic explicitly names **adapter-side** work (preflight + dynamic-backend lookup/creation + SDK setup), not handler code. Handler code runs before `batch_now` is captured and cannot trip the guard — the wording prevents operator confusion |

## Appendix AG — Review round 33 resolutions

| Review finding | Resolution |
| --- | --- |
| `outbound-deadlines = BoundedCooperative` on Fastly was still too strong given the phase-split deviation | §3.5.1 + §3.5.2: new capability `outbound-flexible-phase-budget` — Native on Axum/CF/Spin (single total timeout), **BestEffort on Fastly** (rigid 1/4:3/4 split per §4.3). Apps that need elastic phase budget declare it required and get a hard build failure on Fastly. `outbound-deadlines` keeps its BoundedCooperative meaning (absolute upper bound); the new capability isolates the "no premature phase failure" property |
| Fastly `NameInUse` recovery overclaimed identity verification | §4.3: the adapter cannot fully verify identity for an externally-registered backend (Fastly's `Backend::from_name` getters don't round-trip every builder field — notably SNI / cert hostname). The adapter now **fails closed** with `EdgeError::internal(..)` on `NameInUse` for names not already in its own collision map. Names in the map are reused from the cached `Backend` handle without a fresh `Backend::builder` call, so the path doesn't fire for normal dedupe |
| Fastly code block used non-existent `fastly_req.with_backend(&backend)` | §4.3 code corrected: `let pending = fastly_req.send_async(&backend)?;`. Fastly's `Request` API attaches the backend at send time via `impl ToBackend` — there is no `with_backend` setter. §7 file summary echoes the correction |
| Sub-4 ms timeout degeneracy contradicted "sum = budget" claim | §3.3.4: prose explicitly notes the sub-4 ms branch sets `connect = first_byte = total_ms`, so the absolute-deadline bound becomes 2 × `total_ms` at that scale. Ms rounding already dominates sub-4 ms scenarios, so the test row asserts ≤ 2× rather than = |
| §7 Fastly file summary was stale for IP literals | §7: TLS rule now says `.sni_hostname(sni_host)` is called **only for DNS-name hosts**; IP-literal hosts skip SNI per RFC 6066 §3. Cert verification still runs with the bracket-stripped form. Matches the §4.3 normative code (round 32) |
| Batch memory model used `N × max_response_bytes` ignoring heterogeneity | §3.4.4: bound rewritten as `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ`. The homogeneous case `N × max_response_bytes` is shown as the simplification; the precise sum is over per-slot caps |
| "Future change (§8 risk slot)" had no corresponding §8 entry | §8: new **risk 9** for configurable Fastly phase split — describes the trade-off, the options (per-request setter / per-`OutboundRequest` field / per-adapter knob), and that it's deferred pending a real use case. Test row in §5.4 now cross-references §8 risk 9 |
| Pre-append rule could overflow `usize` | §3.4.1: rule restated as `collected.len().checked_add(chunk.len()).map_or(true, |n| n > max)` (equivalently `chunk.len() > max.saturating_sub(collected.len())`). Either form is checked; no `+` that could panic on absurd inputs |

## Appendix AH — Review round 34 resolutions

| Review finding | Resolution |
| --- | --- |
| Adapter "eight capabilities" tuples stale after adding `outbound-flexible-phase-budget` | §4.1 / §4.2 / §4.3 / §4.4 `capability()` lines all updated to **nine** capabilities. Axum: `Native` for the new one (single reqwest timeout). Cloudflare: `Native` (single `worker::Delay` race). Spin: `Native` (single wasi-timer race). Fastly: **`BestEffort`** (rigid 1/4:3/4 split per §4.3, footnote 5). Implementers following the per-adapter notes can't miss the hard-fail path on Fastly |
| Sub-4 ms prose contradictory | §3.3.4: prose now matches the §4.3 code — `total_ms < 4` sets both = `total_ms`, so sum = `2*total_ms` (e.g. `total_ms=3` → 6 ms phase total, post-deadline slack up to ~3 ms). At sub-4 ms scale ms-rounding already dominates; the test row asserts ≤ 2× rather than = |
| Phase-split comment claimed `ceil-to-ms(budget * 1/4)` but code does `total_ms / 4` (floor) | §4.3 comment rewritten to match the code exactly: `connect_ms = total_ms / 4` (floor), `first_byte_ms = total_ms - connect_ms` (remainder), so sum = `total_ms` exactly. The earlier "ceil-to-ms of budget * 1/4" framing was a misnomer that would have made the sum exceed `total_ms` for some inputs |
| `req.tls_mode()` / `TlsMode` didn't exist on `OutboundRequest` | §4.3 code: TLS branch now derives from the URI scheme directly — `let tls = req.uri().scheme_str() == Some("https");`. No phantom `tls_mode()` method; the canonicalized scheme in `req.uri()` is the single source of truth (§3.1.3) |
| `parts()` / `parts_mut()` missing from the §3.4.5 behavior table | §3.4.5: behavior table now has the explicit row for `parts() -> &http::request::Parts` and `parts_mut() -> &mut http::request::Parts`. Matches the §6 migration sweep which directs `ctx.request()` / `request_mut()` callers to these |
| Specific `send-all-slot-isolation` test row omitted `edgezero dev` | §5.4 row updated to "**every adapter-selecting CLI command** (`build` / `serve` / `deploy` / `dev`) exits non-zero." Matches the generic BestEffort row and §3.5.3 |
| Appendix index said A–AC, doc extends through AG | Index updated to "A–AG (and counting)". Same self-pointer to the last `## Appendix` heading so the next round-up is automatic |

## Appendix AI — Review round 35 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly backend caching had a same-identity race (loser sees `NameInUse`, looks in map, doesn't find name yet, false external) | §4.3: lookup/build protocol redesigned around a `BackendSlot { Building \| Ready(Backend) }`. The outer lock is held **through** `Backend::builder.finish()` (the lock-across-host-call note from round 20 is reversed — Fastly's host call is short and never blocks on guest I/O, so holding the lock is safe). Concurrent same-identity callers serialize on the slot; `NameInUse` under that protocol is unambiguously external |
| Sub-4 ms exception not carried through normative guarantees | §4.3 "Net guarantee" rewritten with **two explicit branches**: `total_ms ≥ 4` keeps `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (the common case); `total_ms < 4` is `BATCH_DISPATCH_SLACK_MAX + total_ms + ms_rounding` (≤ ~28 ms — sub-4 ms is a degenerate input where ms-rounding already dominates). Test row already asserts the 2× sub-4 ms bound |
| Stale "same `t` value and `tls_mode` are folded into identity" sentence | §3.3.4 prose updated: the identity tuple is `scheme + host + resolved_port + tls_mode + budget_ms`, where `tls_mode` is derived from `req.uri().scheme_str()` and `budget_ms` drives the deterministic phase split. Cached and freshly-built backends match because both are deterministic functions of the same tuple |
| Appendix bookkeeping: index said A–AG but file had AH, and AD/round-30 was skipped | New **Appendix AD — Review round 30 resolutions** inserted between AC and AE (reconstructed from the round-30 review). Index note updated to "A–AH (and counting)" with the same self-pointer to the last `## Appendix` heading |

## Appendix AJ — Review round 36 resolutions

| Review finding | Resolution |
| --- | --- |
| Sub-4 ms exception stale in §3.3.4 prose, capability footnote 1, and the test row | §3.3.4: "shifts to ≤ 2 ms past deadline" replaced with the precise sub-4 ms bound (`total_ms + BATCH_DISPATCH_SLACK_MAX + ms_rounding`, ≤ ~28 ms). §3.5.2 footnote 1 now explicitly scopes its numbers to "common-case `total_ms ≥ 4`" and points at §4.3's two branches. §5.4 phase-split test row also annotated "common case, `total_ms ≥ 4`" with a cross-reference to the existing sub-4 ms row |
| Backend cache protocol had undefined `Building` / `Failed` / condvar state | §4.3 rewritten — the protocol is just `Mutex<HashMap<String, (BackendIdentity, Backend)>>` plus "hold the outer lock through `Backend::builder().finish()`." Removed the `BackendSlot::Building` enum, the unwritten condvar storage, and the unwritten `Failed` notification. Holding the lock through the host call makes the race the round-34 review found structurally impossible without any additional state machine |
| Appendix bookkeeping: index said A–AH but file had AI | Index updated to "A–AI (and counting)". Self-pointer to the last `## Appendix` heading remains the canonical answer |

## Appendix AK — Review round 37 resolutions

| Review finding | Resolution |
| --- | --- |
| Cached Fastly backend reuse skipped identity comparison | §4.3 step 2 now branches on `stored_identity == identity` — match → reuse; mismatch → fail closed with the in-adapter SHA-256-128 collision error. §5.4 row exercises this via an injectable hash collision under `#[cfg(test)]`. The "reuse by name alone" wording is removed |
| `NameInUse` wording was narrower than Fastly's actual same-name rule | §4.3 step 5 rewritten with the precise Fastly contract (per `BackendBuilder` docs): identical name + identical properties returns `Ok` (re-registration); `NameInUse` only fires for identical name + **conflicting** properties. So a `NameInUse` in step 5 means an external party registered with conflicting properties we can't safely match. Error message updated accordingly |
| Sub-4 ms bound "≤ ~28 ms" was loose | §3.3.4 + §4.3 + Appendix AI: replaced "≤ ~28 ms" with the strict upper bound `25 + (≤ 3) + (≤ 1) < 29 ms` (the explicit `BATCH_DISPATCH_SLACK_MAX + total_ms + ms_rounding` arithmetic) so the formula and the number agree |
| Appendix bookkeeping: index said A–AI, file had AJ, and an orphan unheaded round-30 review-table sat after AJ | Removed the orphan round-30 table (the round-30 content is already correctly placed in Appendix AD between AC and AE). Index updated to "A–AJ (and counting)" with the standard self-pointer to the last heading |

## Appendix AL — Review round 38 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly single-`send` dispatch slack claimed "structurally 0" but time still passes between `dispatch_budget` and `send_async` | §4.3: the single-`send` paragraph is rewritten to apply the **same `BATCH_DISPATCH_SLACK_MAX` guard** as `send_all` — re-check `Instant::now() - now <= BATCH_DISPATCH_SLACK_MAX` immediately before `send_async`, fail closed on exceedance with the same diagnostic. §3.5.2 footnote 1 single-`send` bullet now says dispatch+headers overshoot ≤ `BATCH_DISPATCH_SLACK_MAX + ms_rounding` instead of zero. §5.4 adds a row that exercises the single-send hook (matching the existing `send_all` injection-hook test) |
| Axum / Cloudflare arming the timer with a value snapshotted before SDK construction left a construction-time gap | §4.1 step 3/4 split into "construct without arming" and "re-read `budget.deadline.remaining()` immediately before arming reqwest's `.timeout(..)` / `worker::Delay(..)`." Matches Spin's "at the moment the race starts" wording (round 21). The cached after-drain value is no longer reused at arming time; on a 100 ms construction phase the SDK timer now reflects 100 ms less wall-clock, not 100 ms of silent overrun. §4.2 Cloudflare step 3/4 mirrors |
| Early dynamic-backend prose said "name cannot duplicate another in same session," contradicting the precise later `NameInUse` rule | §4.3 Dynamic-backends paragraph rewritten to match the later collision-protocol contract: identical name + identical properties re-registers (`Ok`); identical name + conflicting properties fails (`NameInUse`). Implementers reading top-to-bottom see one consistent rule, and a forward-pointer to the precise reuse-vs-conflict protocol later in the same section |
| Appendix index said A–AJ, file had AK | Index updated to "A–AK (and counting)". Same self-pointer to the last `## Appendix` heading |

## Appendix AM — Review round 39 resolutions

| Review finding | Resolution |
| --- | --- |
| Fastly body deadline underspecified at EOF / final read | §3.3.4 + matrix row + §4.3 "Streamed-response wrapping" all rewritten to require the `budget.deadline.is_expired()` check **after every blocking body read returns, including the EOF read** — not just "between chunk reads." Streamed wrapping checks both before issuing the underlying read and after it returns. A last-chunk-or-EOF-arrives-after-deadline test row is added in §5.4 |
| Fastly `send_all` slack diagnostic was inconsistent between the normative message and the test row | §4.3 narrative now quotes the full normative `internal(..)` message verbatim. §5.4 row asserts against the **stable substring `"BATCH_DISPATCH_SLACK_MAX"`** with the full normative string included for reference — future wording polish doesn't break the tests |
| Appendix index said A–AK, file had AL | Index updated to "A–AL (and counting)". Standard self-pointer to the last `## Appendix` heading |

## Appendix AN — Review round 40 resolutions

| Review finding | Resolution |
| --- | --- |
| `into_bytes_bounded_until` overclaimed tighter-deadline enforcement | §3.1.3 helper doc rewritten: the drain checks **`min(effective_deadline, until_deadline).is_expired()` both before issuing each blocking body read and again after it returns** — including EOF. The `min(..)` is what catches the *tighter* `until` case; without it a final EOF read could complete after `until_deadline` but before the looser effective deadline. The "Enforcement is layered" paragraph clarifies that the adapter wrapper handles the effective budget and the helper's `min(..)` handles tighter `until`. §5.4 adds an "until shorter than budget; EOF arrives after until" test row |
| §4.3 Fastly precision still said "between chunks" before the corrected EOF rule | Wording aligned with §3.3.4: body drain checks `is_expired()` **after every blocking read return, including EOF** — not "between chunks." The earlier paragraph no longer contradicts the later correction |
| Appendix index said A–AL, file had AM | Index updated to "A–AM (and counting)". Standard self-pointer to the last `## Appendix` heading |

## Appendix AO — Review round 41 resolutions

| Review finding | Resolution |
| --- | --- |
| `into_bytes_bounded_until` required `min(effective, until)` state `OutboundResponse` doesn't carry | §3.1.3 helper doc rewritten to drop the `min(..)` framing: the adapter wrapper enforces the **request budget** by yielding error chunks; the helper enforces **`until_deadline`** cooperatively before and after each read (including EOF). The two layers compose because whichever fires first wins — no shared "effective deadline" stored on `OutboundResponse` (which carries only status / headers / body), no `min(..)` computation. Test row reworded to match |
| `send_all` rustdoc overpromised isolation | §3.1.1 + §3.2: "without affecting other slots" scoped to **input handling and per-slot Ok/Err type**. Cross-slot timing is explicitly governed by `send-all-slot-isolation` (BestEffort on Fastly because of harvest-order false 504s, §3.3.4). The trait rustdoc now points at the capability for the stricter guarantee |
| Streamed-upload host-write test row didn't match Axum/CF mechanics | §5.4 row rewritten by adapter: Axum/CF drain `Body::Stream` to `Bytes` *before* constructing the platform request (the relevant stall is source-pull during the drain); Spin has explicit source-pull + host-write races on WASI outgoing-body; Fastly has source-pull (unpreemptable, BestEffort) + bounded-cooperative host-write via between-bytes-timeout. The previous unified "host-write" framing is gone |
| Stale "before yielding each chunk" / "between chunks" wording for Fastly streamed body | §3.1.3 Fastly bullet updated to the EOF-safe rule — "both before issuing the underlying body read and again after it returns (including the EOF read)." No active normative text still says the older form |
| Batch memory warning claimed to be in send_all rustdoc but wasn't | §3.1.1 send_all rustdoc gains a **"Memory model"** paragraph: worst-case `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ`, no global cap on N, app bounds N (especially fan-out batches). Implementers copying the rustdoc into their docs site now see the bound at the API level, not only in §3.4.4 |
| Appendix index said A–AM, file had AN | Index updated to "A–AN (and counting)". Standard self-pointer |

## Appendix AP — Review round 42 resolutions

| Review finding | Resolution |
| --- | --- |
| Stale "between chunk reads" still in active §4.3 Fastly note | §4.3 Deadline bullet rewritten: body phase checks `budget.deadline` **after every blocking body read returns, including the EOF read**; streamed bodies are wrapped to check before and after each underlying read. Aligns with §3.3.4 and the round-39/40 EOF-safe rule |
| Appendix index named an exact upper bound and kept drifting | Index reworded to say "A through the last `## Appendix` heading in the document" with an explicit note that the index deliberately doesn't pin an exact letter — every round adds another and the index would otherwise drift. Round-by-round bookkeeping rows can stop chasing the upper bound after each one |

## Appendix AQ — Review round 43 resolutions

| Review finding | Resolution |
| --- | --- |
| Batch memory model under-counted resident memory | §3.1.1 rustdoc + §3.4.4 split the bound into **persistent collected buffer** (`Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ`) and **transient in-flight chunks** (`Σⱼ sizeof(current_chunkⱼ)` for actively-draining slots, typically 8-64 KiB each). The §3.4.1 pre-append rule is the source. §5.4 row reworded from "without allocating past max" to "**without extending the collected buffer past max**" with the in-flight-chunk note |
| Fastly dynamic-backend error mapping was incomplete | §4.3 step 6 spells out: any other `Backend::builder()` error (dynamic backends disabled, DNS, TLS misconfig, Fastly-side rejection) maps to `EdgeError::bad_gateway(format!("Fastly dynamic backend setup failed: {e}"))`. `EdgeError::internal` is reserved for **adapter contract bugs** — `BATCH_DISPATCH_SLACK_MAX` overshoot, `NameInUse` external collision, unfilled-slot harvest invariant. §5.4 adds two rows: (a) each builder-error branch → 502 via a host fake / Viceroy harness, (b) error-chain inspection asserting `internal` only fires on the three contract-bug cases |
| `into_bytes_bounded_until` didn't define `Body::Once` behaviour | §3.1.3 helper doc adds an explicit branch: `Body::Once` checks `until_deadline.is_expired()` **at entry** before anything else; expired → `gateway_timeout` (precedence over over-cap → `bad_gateway`). `Body::Stream` keeps the existing before/after each read rule. Callers see consistent `gateway_timeout` semantics across body shapes |
| Tier 1 over-claimed for adapter-specific mechanics | §5.4: the stalled-streamed-upload row is **split** into a Tier 2/3 row (adapter mechanics — Axum tokio / CF `worker::Delay` / Spin wasi / Fastly host-timer behaviour, requires runtime CI) and a Tier 1 row (cross-adapter *contract* — 504, index alignment, partial-failure isolation — via `MockOutboundClient` with scripted stalls). Tier 1 no longer claims to prove adapter-specific wall-clock semantics |

## Appendix AR — PR #269 rebase

Rebases the spec onto [`stackpop/edgezero` PR #269](https://github.com/stackpop/edgezero/pull/269) (`feature/extensible-cli`, rev `b4c80e9`). PR #269 reshapes the CLI dispatch, the manifest store sections, the Spin adapter target, and adds an integration-test crate under `examples/app-demo/`. None of the outbound-HTTP design decisions change — this appendix records the wording and reference updates so future readers don't trip on the older symbol names that live on in earlier appendices.

| Area | PR-#269 reality | Spec change |
| --- | --- | --- |
| CLI dispatch | `edgezero-cli` exposes nine commands (`auth login/logout/status`, `build`, `config push/validate`, `deploy`, `demo` [feature-gated, contributor-only], `new`, `provision`, `serve`); every adapter-selecting one routes through a single `edgezero_cli::adapter::execute(adapter_name, action, manifest_loader, args)` helper in `crates/edgezero-cli/src/adapter.rs`. The legacy `handle_build` / `handle_serve` / `handle_deploy` / `handle_dev` free functions are gone. | §3.5.3 paragraph rewritten to use `Adapter::execute` framing; §7 `edgezero-cli` bullet rewritten to point at `src/adapter.rs` and the `run_*` entry points; §5.4 capability rows updated to enumerate the PR-#269 command list. Older appendices (e.g. Appendix M, Appendix AC) still quote `handle_*` — those are historical resolution log, not normative |
| `dev` → `demo` | The `dev` command is removed. `demo` is the feature-gated, contributor-only replacement that runs the bundled demo app under Axum; production users get `--adapter axum serve` instead. | §3.5.3 paragraph + §5.4 `BestEffort` row note that `demo` (not `dev`) is the contributor-only Axum runner that must also fail capability checks. Earlier appendices quoting `edgezero dev` are historical |
| Spin SDK + target | Spin adapter pins `spin-sdk = "6"` and builds for `wasm32-wasip2` (CI gate quoted in CLAUDE.md still reads `wasm32-wasip1`; that's a CLAUDE.md/CI follow-up tracked at the bottom of §8, not a spec change since the spec doesn't pin a target). | No spec change — §3.1.4 / §4.4 / §5.4 reference `spin_sdk::http::send` symbolically and are SDK-6-compatible. §8 risk list updated to note the CLAUDE.md / CI command-quote refresh as a follow-up |
| Spin proxy + store APIs | `SpinRequest` exposes `into_parts`; `IncomingBodyExt::bytes()` replaces the older manual incoming-body drain; `FullBody::new(Bytes)` is the outgoing-body constructor; KV / config / secret stores use async `open` / `get` / `set` / `delete` / `exists` / `get_keys`. | No spec change — the outbound design does not pin Spin's body or store call shapes. §4.4 keeps its `spin_sdk::http::send` shape, which is unchanged |
| Multi-store manifest | The manifest now carries `ManifestStores { config: Option<StoreDeclaration>, kv: Option<StoreDeclaration>, secrets: Option<StoreDeclaration> }` instead of a single store block. | §7 `examples/app-demo` bullet calls out that the demo manifest's `[stores.*]` blocks are unchanged from PR #269 and that `[capabilities.outbound]` composes additively with them. §3.5.1 outbound capability shape is untouched |
| Adapter registry hook | The adapter trait grows `execute(action, args)`, `provision(..)`, `push_config_entries(..)`, plus validation hooks. `ensure_capabilities` plugs into `execute` so every adapter-selecting command runs the check exactly once. | §7 `edgezero-cli` bullet rewritten to put `ensure_capabilities` in `src/adapter.rs::execute` rather than four per-command handlers; the wording explicitly names the new `run_*` entry points the dispatch fans out to |
| `examples/app-demo` integration | PR #269 adds `examples/app-demo/crates/app-demo-cli/` — a typed-CLI integration crate that exercises `auth` / `provision` / `config push|validate` / `demo` against the demo manifest. | §7 `examples/app-demo` bullet now mentions the new integration crate explicitly so the outbound-HTTP migration updates both the per-adapter binaries and the CLI integration crate together |
| Status header | Snapshot through review round 43 (date 2026-06-04). | Bumped to `revised through review rounds 1–43 + PR-#269 rebase · Date: 2026-06-05`, with a one-line "Codebase baseline" pointer to the PR plus an explicit note that earlier appendices retain the legacy `handle_*` / `edgezero dev` wording for historical fidelity |
| Older appendices | Appendices D, M, AA, AB, AC, AD, AH, etc. quote `handle_build` / `handle_dev` / `edgezero dev` verbatim as part of the round-by-round resolution log. | **Left as-is by design.** Rewriting the historical journal would erase the audit trail of which round added which guarantee; the §3.5.3 + §7 + Appendix AR text is authoritative going forward. The status header points readers at this appendix for the resolution |

## Appendix AS — Review round 44 resolutions (PR-#269 reality check + carry-overs)

| Review finding | Resolution |
| --- | --- |
| PR-#269 rebase claims didn't match the local checkout (`Command` has `Build/Deploy/Dev/New/Serve`, `AdapterAction` has only `Build/Deploy/Serve`, `main` still handles `Command::Dev`) | Status header (line 3 onward) reframed: "Target codebase baseline" makes PR #269 the explicit forward target and calls out that it is **not yet merged**; "Current checkout (pre-#269)" enumerates the concrete differences (`args.rs::Command`, `registry.rs::AdapterAction`, `main.rs::Command::Dev`) and says the §3.5.3 / §5.4 / §7 / Appendix AR rows are **contingent** on the PR landing in the documented shape. Outbound HTTP design (§1 / §3.1 / §3.2 / §3.3 / §3.4 / §4) is independent of PR #269 and lands either way |
| Capability enforcement underspecified for non-`execute` paths and manifest shell commands. §3.5.3 said one `execute` hook covers everything, but PR #269 routes `provision` to `Adapter::provision` and `config` to validation hooks, and the dispatcher runs manifest shell commands before the registry lookup. The earlier pseudocode required `registry::get_adapter` for capability metadata, which shell-overridden adapters bypass entirely | §3.5.3 rewritten as **four pre-dispatch gates**: one at the top of `edgezero_cli::adapter::execute(..)` (before `manifest_command` is checked, before the registry lookup), plus three sibling gates at the top of `run_provision`, `run_config_push`, and `run_config_validate`. Each gate consults the **registry** for capability metadata regardless of whether the action ultimately dispatches to a shell command, so shell-overridden adapters still get checked; if the adapter is not in the registry, the gate degrades to a warning so a brand-new shell-only adapter without a registered stub still works. Covered / not-covered table enumerates every PR-#269 command. Pre-#269 fallback wording (gate at each of `Build`/`Serve`/`Deploy`/`Dev` handler tops) is preserved for readers on today's checkout |
| `into_bytes_bounded_until` overpromised tighter deadline enforcement: doc said "if the caller's `until_deadline` is tighter, the helper fires first," then admitted the helper is cooperative and cannot preempt a read in progress | §3.1.4 rustdoc rewritten: helper is explicitly a **cooperative post-read / EOF validator, not a timer-backed race**. New paragraph spells out the concrete failure mode — a read blocked for 500 ms with `until = 100 ms` does **not** return at 100 ms; it returns at 500 ms with `gateway_timeout` (post-read check observed expiry). "Whichever fires first" reworded to "at yield boundaries only." Real-time preemption explicitly delegated to the request builder's `.deadline(min(req_deadline, app_inner_deadline))` (pushed into the wrapper, which is the only layer with timer-backed enforcement on Axum / CF / Spin). §3.1.4 single-quote about the tighter-`until` case (line ~589) likewise updated |
| Tier 1 streamed-upload contract contradicted Fastly's declared `streamed-upload-deadlines = BestEffort` (footnote + §4.3 both say a Fastly source-pull stall is unbounded) | §5.4 Tier 1 streamed-upload-contract row reworded: the "within the configured deadline" half holds **only on the preemptible-source adapters (Axum / Cloudflare / Spin)**; Fastly is explicitly excluded from the wall-clock half and observes only the index-alignment + partial-failure-isolation half. `MockOutboundClient` is parameterised by the adapter under test so the Fastly invocation runs only the structural assertions. Wall-clock mechanics across all four adapters (including Fastly's `BoundedCooperative` between-chunk bound) live in the Tier 2/3 row above |
| Tier 1 still claimed coverage for adapter-only mechanics (Fastly host timers, harvest behaviour, dynamic backend identity, `BATCH_DISPATCH_SLACK_MAX` injection hook) — but Tier 1 is defined as `edgezero-core` + `MockOutboundClient`, which has no analogue for any of those | §5.4 rows demoted from Tier 1 (yes) → Tier 1 (—) with an explicit per-row note pointing at the Tier 2 / Tier 3 home: (a) Fastly `send` `Body::Stream` mechanics (Fastly host between-bytes-timeout, source-pull non-preemption) → Tier 2 (Fastly contract crate) + Tier 3 (Viceroy); (b) Fastly `send_all` mixed-budget headers-phase harvest-order delivery delay → Tier 2 / Tier 3; (c) Fastly `send_all` Buffered body-phase harvest head-of-line block → Tier 2 (deterministic harvest ordering against a host-side fake) + Tier 3 (Viceroy wall-clock); (d) Fastly mixed-budget same-host distinct-backends-by-`budget_ms` identity assertion → Tier 2 (inspect registered-backend map) + Tier 3 (Viceroy); (e) Fastly `send_all` `BATCH_DISPATCH_SLACK_MAX` substring + hook → Tier 2 (`crates/edgezero-adapter-fastly/tests/contract.rs`) + Tier 3 (Viceroy with hook); (f) Fastly upload-consumes-budget `send_async_streaming` + `wait()`-drop sequence → Tier 2 / Tier 3 |
| §3.4.1 memory model still treated `current_chunk` as effectively bounded ("8-64 KiB for typical sources … not unbounded") while only the persistent collected buffer is actually guaranteed under `max` | §3.4.1 rewritten: the `8-64 KiB` figure is now explicitly **descriptive of the adapters' incoming stream chunking, not a contract**. Three concrete consequences spelt out — (a) an upstream yielding one large `Bytes` exceeds the typical figure (4 MiB single-chunk example); (b) EdgeZero does not rechunk, so there is no core-side cap on incoming chunk size; (c) the §3.4.4 batch model inherits the same source-controlled property. New **§8 risk 11** tracks the deferred follow-up: opt-in `max_chunk_bytes` builder field vs. fixed `MAX_TRANSIENT_CHUNK_BYTES` constant vs. leave-and-document, each with its perf / lazy-streaming trade-off |
| §3.4 numbering was out of source order (3.4.5 appeared before 3.4.3 / 3.4.4) | §3.4.5 ("Inbound body migration") **physically moved** to after §3.4.4 ("Batch memory model") — section numbers preserved (so cross-refs in §1, §3.1, §5.4, §6, §7, and 25+ appendix entries still resolve), but physical source order now matches numeric order (3.4.1 → 3.4.2 → 3.4.3 → 3.4.4 → 3.4.5). Verified via `grep -n '^#### 3\.4'`. No content edits inside §3.4.5; pure reorder |

## Appendix AT — Review round 45 resolutions

| Review finding | Resolution |
| --- | --- |
| Capability enforcement had a hard contradiction around unregistered shell adapters: prose said "missing registry metadata degrades to a warning," pseudocode hard-failed on `registry::get_adapter(adapter_name).ok_or_else(..)?` | §3.5.3 now has an explicit **missing-from-registry policy** table: when the manifest declares **no** capabilities (`required = []` AND `optional = []`), missing-from-registry logs a `warn!` and proceeds — the brand-new-shell-only-adapter case still works. When the manifest declares **any** required or optional capability, missing-from-registry is a **hard failure** with a clear "register an adapter stub that returns capability metadata, or remove the `[capabilities]` section" message — the "required capabilities fail early" contract is preserved. Pseudocode rewritten to match (`let Some(adapter) = ..` with the two-branch policy in the `else` arm) |
| Multiple later sections still described capability checks as flowing through "the single `Adapter::execute` dispatch point" / "the shared `Adapter::execute` dispatch" — but §3.5.3 now defines four pre-dispatch gates (one in `execute`, three siblings on `run_provision` / `run_config_*` / `run_demo`) | Four §5.4 test rows reworded to reference the **§3.5.3 pre-dispatch gates** explicitly (one in `execute(..)`, siblings on `run_provision` / `run_config_*` / `run_demo`): (a) generic Required-BestEffort enforcement row, (b) `send-all-slot-isolation` Fastly hard-fail row, (c) `lazy-streamed-response-passthrough` `demo`-runner row (now correctly says `demo` goes through `run_demo`'s sibling gate, *not* through `execute(..)`), (d) `outbound-flexible-phase-budget` Fastly row. §6 migration "CLI dispatch in the PR-#269 world" bullet rewritten to describe the **four-gate** wiring (one inside `execute(..)` before `manifest_command` + registry lookup; siblings on the three commands that don't flow through `execute`). §7 `crates/edgezero-cli` `src/adapter.rs` task rewritten to specify "first statement of `execute(..)`" plus the three sibling-gate placements. Status-header forward pointer (line 6) is left untouched because it lists the surfaces PR #269 *introduces*, not where the gate sits |
| Memory contract overclaimed hard bounds: §3.4.1 / §3.4.4 correctly say resident memory is `max + sizeof(current_chunk)` with the chunk source-controlled, but the §3.4.4 contract bullets just said per-response and per-inbound-body memory are bounded by `max` | §3.4.4 contract bullets rewritten to split **persistent** (post-append, retained, bounded by `max`) vs **transient** (in-flight during the drain, `max + sizeof(current_chunk)` worst case, chunk source-controlled). Per-response, per-inbound-body, and batch entries all carry both terms now. Batch transient `Σⱼ sizeof(current_chunkⱼ)` over actively-draining slots is explicit; the bullet ends with a forward pointer to §8 risk 11 (deferred per-batch transient-chunk cap) |
| `json_bounded_until` rustdoc still implied caller-supplied helper deadlines get real timer enforcement on Axum / CF / Spin via wrapped bodies. The `into_bytes_bounded_until` doc was already fixed in round 44; this one was missed | §3.1.4 `json_bounded_until` rustdoc rewritten to match `into_bytes_bounded_until`: caller-supplied `deadline` is enforced **cooperatively** by the underlying `into_bytes_bounded_until` (at yield boundaries enumerated there); a read already blocked when `deadline` passes is **not** preempted. Real-time enforcement is the **wrapper's** job and applies to the **request budget** only — adapters with platform timers (Axum / CF / Spin) install the deadline-aware stream bounded by `dispatch_budget(req).deadline`; Fastly is `BoundedCooperative` on that bound. To get timer-backed preemption of a tighter deadline, set `.deadline(min(req_deadline, app_inner_deadline))` on the builder so it lands in the wrapper. Malformed-JSON → `bad_gateway` (502) is preserved |
| Fastly dynamic-backend "three distinct values" row was still marked Tier 1, but it asserts Fastly `Backend::builder` / `.override_host` / `.sni_hostname` / `.check_certificate` / `.disable_ssl` mechanics — same shape as the other Fastly-mechanic rows that were demoted in round 44 | §5.4 row split into two: (a) **Tier 1 half** — `OutboundRequest::get(..)` exposes `backend_target()`, `host_authority()`, `sni_hostname()` accessors, tested in `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]` without any adapter dependency; (b) **Tier 2 / Tier 3 half** — Fastly adapter consumes the three values via `Backend::builder(name, backend_target).override_host(..).sni_hostname(..).check_certificate(..)` / `.disable_ssl()`, tested by inspecting the registered-backend map (Tier 2) and a Viceroy round-trip (Tier 3). Each row clearly states what it does and does not assert. Matches the round-44 demotion pattern for the other Fastly-mechanic rows |

## Appendix AU — Review round 46 resolutions

| Review finding | Resolution |
| --- | --- |
| §3.5.2 `Adapter` trait snippet was pre-PR-#269 shaped (only `execute` / `name` / `capability`), but the status header said the target baseline adds `Adapter::provision(..)` and config hooks, and §3.5.3 relies on those paths | §3.5.2 now shows **two trait blocks** — the pre-#269 shape (today's checkout: `execute` + `name` + `capability`) and the PR-#269 target shape (adds `provision`, `push_config_entries`, `validate_config` plus "…other PR-#269 validation hooks elided…"). Explanatory paragraph below the blocks states (a) this spec adds only `capability(..)`; (b) the other PR-#269 methods are owned by PR #269 and shown here only so readers don't misread the trait as exhaustive; (c) the `provision` / config hooks are called from §3.5.3's **sibling** pre-dispatch gates, not from `Adapter::execute`; (d) on today's checkout there is no `provision` / `config` surface, so the sibling-gate wording applies once PR #269 lands |
| Capability-gate counting was inconsistent: §3.5.3 said "single pre-dispatch gate," then "two sibling gates," then "four gates," while the table + later sections include `execute`, `run_provision`, `run_config_push`, `run_config_validate`, and `run_demo` (five) | §3.5.3 normalized to **"pre-dispatch gate at each adapter-selecting entry point"** with **five concrete gate sites** enumerated: (1) inside `execute(..)` first statement, (2) `run_provision`, (3) `run_config_push`, (4) `run_config_validate`, (5) feature-gated `run_demo` hardcoding `"axum"`. Code blocks updated to number all five. Table caption changed from "four gates above (one in execute, three siblings)" to "five gate sites above (one inside execute(..), four siblings)". §6 migration "CLI dispatch" bullet updated to "five pre-dispatch gate sites." §5.4 capability test rows that already listed all four siblings + execute are now consistent with the count. Appendix entries from rounds 44–45 left as historical (they record the count at the time they were written) |
| §5.4 referenced core `OutboundRequest` accessors `backend_target()` / `host_authority()` / `sni_hostname()` that the API surface never defined | §3.1.4 `OutboundRequest` surface now defines all three as **adapter-facing, non-consuming** methods with their precise semantics: `backend_target() -> String` (always `"host:port"`, default ports filled, IPv6 bracketed); `host_authority() -> String` (port only when non-default for scheme, IPv6 bracketed); `sni_hostname() -> Option<&str>` (port-stripped, bracket-stripped, **`None` for IP literals** per RFC 6066 §3 — so IP-literal HTTPS adapters fall back to `uri().host()` for `.check_certificate(..)` and skip `.sni_hostname(..)` entirely). Block intro paragraph names them the "single canonical source" the Fastly identity hash (§4.3) depends on, and pins them as what the §5.4 Tier-1-half three-value row tests |
| Multiple §5.4 rows still claimed Tier 1 coverage for adapter wrappers / platform timers / no-partial-send mechanics — specifically `into_bytes_bounded_until` end-to-end, streamed-body-stalls wrapped-stream, Axum no-deadline 30 s end-to-end, `json_bounded_until` end-to-end, and "Adapter `dispatch_budget` everywhere" | Five §5.4 rows split following the round-44 pattern (Tier-1 contract shape, Tier 2 / 3 wall-clock / wrapper insertion): (a) `into_bytes_bounded_until` row → helper-cooperative half (Tier 1) + adapter-wrapper half (Tier 2/3); (b) "streamed body stalls after one chunk" demoted Tier 1 (yes) → (—) — wrapper insertion / platform timer is adapter-specific; (c) Axum no-deadline 30 s split into `DEFAULT_NO_DEADLINE_BUDGET` core constant (Tier 1) + Axum end-to-end wall-clock (Tier 2/3); (d) `json_bounded_until` row split same way (helper-cooperative Tier 1 + adapter wrapper Tier 2/3); (e) "Streamed body honours `dispatch_budget(req).deadline` end-to-end" demoted Tier 1 (yes) → (—) — wrapper-specific; (f) "Adapter `dispatch_budget` everywhere" demoted to Tier 2/3 with note pointing at the core-helper Tier-1 row; (g) `.timeout(short).deadline(long)` split into dispatch_budget classification (Tier 1) + wrapper-fires-at-`now + short` (Tier 2/3) |
| Fastly three-value Tier 2 row overgeneralised HTTPS: it said HTTPS always calls `.sni_hostname(sni_hostname).check_certificate(sni_hostname)`, but Fastly normative code skips `.sni_hostname(..)` and bracket-strips the cert host for IP literals (per RFC 6066 §3) | §5.4 row scoped to **"DNS-name HTTPS path"**: explicit "where `sni_hostname()` returns `Some(host)`" guard, plus a pointer that "IP-literal HTTPS (where `sni_hostname()` is `None`) is the dedicated 'Fastly HTTPS to IP literals' row below, which asserts the **distinct** behaviour of skipping `.sni_hostname(..)` and passing the bracket-stripped host to `.check_certificate(..)`." DNS-only test assertions preserved; the IP-literal row at row 3067 (later in §5.4) is the canonical IP test |

## Appendix AV — Review round 47 resolutions

| Review finding | Resolution |
| --- | --- |
| IP-literal TLS host handling broke the new accessor contract: §3.1.4 said the three accessors are the "single canonical source" and adapters must not re-derive from `uri()`, but `sni_hostname()` returned `None` for IP literals and told adapters to fall back to `uri().host()` for the cert host. Fastly pseudocode at §4.3 still parsed and trimmed the host locally | §3.1.4 adds a new **fourth accessor `cert_host() -> Option<&str>`**: `Some(host)` for *any* HTTPS scheme (DNS name OR IP literal — port-stripped, bracket-stripped), `None` for HTTP. The full canonical source is now `backend_target()` / `host_authority()` / `sni_hostname()` / `cert_host()`. `sni_hostname()` rustdoc rewritten to be explicit: `None` means "send no SNI" — adapters MUST NOT fall back to `uri().host()` and MUST consult `cert_host()` for certificate verification. Fastly §4.3 pseudocode rewritten: the four-value comment block names each accessor and its semantics; the TLS-setup branch is now `match req.cert_host() { Some(cert) => builder.enable_ssl().check_certificate(cert).maybe_sni(req.sni_hostname()), None => builder.disable_ssl() }`. The previous local `is_ip_literal` parse + `trim_start_matches('[')` is gone — bracket-stripping and IP-literal detection now live in the core accessors |
| §5.4 still marked adapter mechanics as Tier 1: upload-budget rows claimed Tier 1 could prove Axum / Cloudflare "before constructing/sending, no partial upstream send" and Spin WASI outgoing-body behaviour; URI canonicalization rows claimed Tier 1 could prove "one dynamic backend" / "same Fastly backend identity" | Four §5.4 rows split per the round-44 pattern. (a) Upload-budget *contract shape* — `MockOutboundClient` exposes a `did_dispatch()` flag; Tier 1 asserts "deadline expired during drain → 504 AND `did_dispatch() == false`" without any adapter. (b) Upload-budget on Axum / Cloudflare — Tier 2 (platform-SDK send-call counter on a fake harness) + Tier 3 (mock origin observes zero connections). (c) Upload-budget on Spin — Tier 2 (WASI outgoing-body chunk-count observation) + Tier 3 (Spin runtime, mock origin observes the partial upload). (d) URI canonicalization split into a core accessor row (Tier 1) and a Fastly identity row (Tier 2 / Tier 3); URI scheme + host case normalisation split the same way |
| §7 reintroduced gate-count ambiguity: active migration text said "five pre-dispatch gate sites," but the file summary said "All four call sites" after listing `execute` + three siblings + `run_demo` | §7 `crates/edgezero-cli` `src/adapter.rs` bullet updated: "All five gate sites (one inside `execute(..)`, the four siblings on `run_provision` / `run_config_push` / `run_config_validate` / `run_demo`)." Matches the §3.5.3 + §6 wording |
| Appendix AR was stale but still advertised as a rebase-claims surface: the header pointed readers at AR, while AR still said "every adapter-selecting command routes through a single `Adapter::execute` helper" — wording corrected to "four gates" in AS and "five gates" in AU | Status header (line 8) reworded: AR is now explicitly tagged as "round-44 history" and "superseded by Appendices AS / AT / AU / AV." The authoritative surfaces enumerated in the same bullet are §3.5.3 + §3.5.2 + §5.4 + §7. Readers see the current count + shape without having to reconcile AR's older language |
| Minor copy/paste issues: `sni_hostname() == "example.com"` should have been `Some("example.com")`, and the batch-memory formula carried `request_body_iᵢ.len()` (double subscript) | Three-value test row updated to **four-value** and uses `Some("example.com")` for both `sni_hostname()` and `cert_host()`. Batch-memory formula normalised to `Σᵢ request_bodyᵢ.len() + Σᵢ max_response_bytesᵢ` in every active surface (§3.1.1 rustdoc, §3.4.4 contract bullets, §3.4.4 visualisation block, §3.4.4 simplification). Historical appendices left unchanged (they record the round-N wording verbatim) |

## Appendix AW — Review round 48 resolutions

| Review finding | Resolution |
| --- | --- |
| Host/authority wording still bypassed the new canonical-accessor contract: §3.1.4 said adapters MUST consume the four accessors and `host_authority()` owns the outgoing Host, but `from_request` (§3.1.3) and `normalize_for_dispatch` (§3.1.5) still said adapters derive Host directly from `req.uri()` at SDK-construction time | Both proxy-forward sites rewritten to thread `req.host_authority()` end-to-end. `from_request` rustdoc now reads "the adapter sets the final `Host` value from `req.host_authority()` at SDK-construction time — the same canonical accessor every adapter uses (§3.1.4) — and MUST NOT read `req.uri()` for the Host value." Concrete examples (port preservation, IPv6 bracketing, default-port stripping) moved into the accessor doc. `normalize_for_dispatch` step 3 rewritten the same way: "the adapter then sets the final `Host` header from `req.host_authority()` … does NOT re-read `req.headers()` nor reconstruct from `req.uri()` directly." One accessor, one canonical string, every adapter observes the same value. The §7 Fastly file summary already names `req.host_authority()` and was updated in the same edit to remove the leftover "three-value URI split" phrasing |
| Fastly `send_all` body-phase deadline bound overclaimed observed wall-clock behaviour: §3.3.4 admits harvest-order body drain causes false 504s, then said per-slot post-deadline overshoot is one between-bytes-timeout, and §3.5.2 footnote 1 repeated that bound in the capability text without scoping | §3.3.4 "worst-case overshoot" paragraph rewritten: the one-between-bytes-timeout bound now applies **"once that slot is actively draining"**, not to total observed wall-clock. New paragraph spells out that observed completion for slot `k` can be as late as `Σᵢ<ₖ drain_timeᵢ + (effective_at_dispatch for slot k)` — the harvest delay is explicit. The cross-slot weakening is owned by the separate `send-all-slot-isolation` capability (footnote 4), so apps that need cross-slot isolation declare it required and get the Fastly hard build failure. §3.5.2 footnote 1 (`outbound-deadlines` rubric) updated to say "body phase **once a slot is actively draining** is still ≤ one between-bytes-timeout — but the slot's observed completion can additionally be delayed by harvest-order serialization … the bound here is on the active-drain phase only, not on total observed wall-clock across the batch." `outbound-deadlines` and `send-all-slot-isolation` now own non-overlapping slices of the story |
| Tier 1 upload-budget "no platform dispatch" contract contradicted Spin/Fastly's explicitly-documented partial upstream sends. The Tier 1 row required `did_dispatch() == false`, while the Spin and Fastly per-adapter rows said partial upstream send is possible/expected | §5.4 Tier 1 row scoped to **"Axum / Cloudflare semantics only"**: the `did_dispatch() == false` assertion is now the Axum / Cloudflare contract (drain-then-dispatch). The mock's `drain-first` mode is called out as a property of the test harness, not a cross-adapter contract. Row text explicitly excludes Spin and Fastly and points at the per-adapter Tier 2 / Tier 3 rows for those adapters' distinct partial-send semantics |
| Four-value URI row contradicted `cert_host()` for HTTP: `cert_host()` is `None` for non-HTTPS, but the row asserted `http://example.com:8443` produces `cert_host() == Some("example.com")` | §5.4 row split by scheme. **HTTPS DNS-host inputs** (three URL variants): `cert_host() == Some("example.com")` on all; `sni_hostname() == Some("example.com")` on all. **HTTPS IP-literal inputs**: `sni_hostname() == None` (RFC 6066 §3); `cert_host() == Some("127.0.0.1")` / `Some("::1")`. **HTTP DNS-host inputs** (three URL variants): `sni_hostname() == None`; `cert_host() == None`. The HTTPS-only `cert_host() == Some` is now the canonical reason an adapter calls `.disable_ssl()` vs `.enable_ssl()` / `.check_certificate(..)` — a single accessor disambiguates TLS-on-vs-off |
| Stale "three-value" language remained after `cert_host()` was added in round 47 (round 47 added the fourth accessor but didn't sweep). The §3.1.4 accessor-block comment said "tested by the Tier 1 half of the §5.4 three-value row"; the Fastly Tier 2 row title still said "three values"; the §7 Fastly file summary said "three-value URI split" | All three sites updated to "four-value": (a) §3.1.4 accessor-block comment now reads "the §5.4 four-value row"; (b) §5.4 Fastly Tier 2 row title rewritten to "Fastly adapter consumes the four canonical accessors, DNS-name HTTPS path" with the `check_certificate(cert)` argument coming from `req.cert_host()` (not the previously-conflated `sni_host`); (c) §7 Fastly migration entry rewritten to reference "the four canonical URI accessors" and spell out the per-accessor wiring (`backend_target`, `host_authority`, `cert_host`, `sni_hostname`). The earlier "three URI values must be derived from canonicalized `req.uri()`" warning is removed; the new wording says adapters MUST NOT re-derive from `req.uri()` directly and must consume the accessors |
| §5.5 CI gate wording conflicted with the PR-#269 Spin target baseline: status header said PR #269 moves Spin to SDK 6 / wasm32-wasip2, but §5.5 said "the five existing CLAUDE.md gates still apply" — implementers landing the spec post-#269 would have preserved the stale `wasm32-wasip1` quote | §5.5 reworked. **First paragraph** preserved (count + shape of the five gates unchanged). **New "Spin gate triple — pre-#269 vs PR-#269" subsection** explicitly enumerates the two literal command strings: pre-#269 = `cargo check -p edgezero-adapter-spin --target wasm32-wasip1 --features spin`; PR-#269 = `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`. "Implementers landing this spec after PR #269 must update the gate quote … preserving the stale `wasm32-wasip1` quote would silently break the Spin build." §8 risk 10 cross-referenced for the CLAUDE.md / CI command-quote follow-up. The other four gates are stated as unaffected by PR #269 |

## Appendix AX — Review round 49 resolutions

| Review finding | Resolution |
| --- | --- |
| URI canonicalization text contradicted itself across active surfaces: `OutboundRequest` explicitly *preserves* path / query (§3.1.3), but the canonical accessor block (§3.1.4) said the §3.1.3 rules had "rejected path / query," and §3.5.4 said manifest host entries use "the same rules" then rejected path / query. Request-URI rules and manifest-host-entry rules were conflated | §3.1.4 accessor-block comment rewritten: rejects **userinfo and fragments** only; path and query are explicitly preserved per RFC 3986 §3.3 / §3.4 (still accessible via `self.uri()` for the wire-level request line). New paragraph at the end of the block calls out that manifest `[capabilities.outbound].hosts` entries (§3.5.4) are a **separate grammar** — host-authority-only declarations, so the manifest-host validator rejects path / query / fragment / userinfo on the manifest side. §3.5.4 prose updated likewise: "diverge on path/query — request URIs pass them through; manifest host entries reject them. Sharing the lowercase-scheme / lowercase-host / strip-default-port / reject-userinfo / reject-fragment rules with §3.1.3 keeps the canonical spelling identical; the path/query divergence is the only difference and is enforced by the validator, not by quietly dropping at render time." Reader sees one shared subset + one explicit divergence, not two contradictory "same rules" claims |
| `OutboundDeadlines` enum doc-comment and Fastly capability summary both said the `send_all` coverage is "headers phase only," contradicting the round-48 active-body-drain scoping in footnote 1 | `Capability::OutboundDeadlines` doc-comment rewritten to say `send_all` coverage is "both the headers phase and the **active body-drain phase** of each slot — a slot's active drain still honours the single-slot bound (≤ one between-bytes-timeout overshoot per gap on Fastly per §3.3.4). The **cross-slot harvest delay** … is *not* covered here — that is the separate `SendAllSlotIsolation` capability below." Fastly capability summary (`§4.3` end) updated: `outbound-deadlines = BoundedCooperative (footnote 1 — covers single send, plus send_all headers phase AND active body-drain phase per slot; cross-slot harvest-order delay is the separate send-all-slot-isolation story)`. Three surfaces now say the same thing |
| Fastly streamed-upload "response phase" prose used `between_bytes_timeout` as the bound on the post-upload headers wait, but §3.3.4 defines `first_byte_timeout` as the headers wait and `between_bytes_timeout` as the inter-chunk gap (active drain only). Apps reading the streamed-upload prose would have assigned the wrong phase | §4.3 streamed-upload response-phase paragraph rewritten: "the response-phase host timeouts are locked to the phase-split values computed at dispatch (`first_byte_ms` for the headers wait, `between_ms` for inter-chunk gaps once the response body flows)." Concrete worked example switched from "host's between-bytes-timeout was set to 200 ms" to "host's `first_byte_timeout` was set to 150 ms at dispatch (3/4 of a 200 ms budget)." Net-wall-clock claim updated: "exceed `budget.duration` by up to one first-byte-timeout (for the headers wait) plus one between-bytes-timeout per body-chunk gap." Matches the §3.3.4 phase definitions and the §4.3 phase-split formulas |
| Status header bookkeeping was stale: line 8 said Appendix AR is "superseded by Appendices AS / AT / AU / AV" (rounds 44–47), but the file now has Appendix AW (round 48) and AX (this round) | Line 8 pointer extended to "**superseded by Appendices AS / AT / AU / AV / AW / AX** (rounds 44–49)." Readers see a single canonical "what supersedes AR" list that tracks every newer rebase appendix |
