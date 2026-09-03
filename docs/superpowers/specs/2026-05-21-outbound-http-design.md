# EdgeZero Outbound HTTP — Design Spec

> **Status:** Draft, revised through review round 55 (round 55 = Fastly buffered-upload isolation, explicit stream-error constructors, absolute upload-boundary checks, and a concrete Spin exchange state machine) · **Date:** 2026-06-08
> **Branch:** `docs/outbound-http-spec` · **Audience:** EdgeZero maintainers
> **Driving pattern:** fan-out HTTP workloads — N concurrent outbound requests under a shared wall-clock deadline, results harvested in input order. The spec is written against this pattern as a portable substrate; it deliberately does not name a specific consumer.
> **Target codebase baseline:** [`stackpop/edgezero` PR #269](https://github.com/stackpop/edgezero/pull/269) (`feature/extensible-cli`, rev `b4c80e9`) — **now merged into `main`** (squash-merged as `e483723`). Relevant baseline changes are the `edgezero_cli::adapter::execute(..)` shell-or-registry dispatcher, expanded runtime `AdapterAction` variants, Spin SDK 6 / wasip2, the contributor-only `demo` command replacing `dev`, and the app-demo integration crate. Non-outbound store/config lifecycle changes remain outside this design.
> **Current checkout (post-#269):** the CLI surface is now the #269 shape — `Command::{Build, Serve, Deploy, Auth, Provision, Config, Demo, New}`, `AdapterAction::{AuthLogin/Logout/Status, Build, Deploy, Serve}`, and the `edgezero_cli::adapter::execute(..)` dispatcher; `dev` is gone. This outbound spec gates only runtime production: `build` / `serve` / `deploy` through `execute(..)`, plus `demo` before Axum starts. Provisioning and config/store lifecycle policy belongs to its owning specifications (§3.5.3).
> **Where rebase claims live (authoritative surfaces):** §3.5.3 build-enforcement, §3.5.2 `Adapter` trait shape, §5.4 capability tests, and the §7 `edgezero-cli` migration bullet. The §3.5.3 + §7 active text is authoritative.

## 1. Overview

### 1.1 Goal

Make EdgeZero a production-safe substrate for **outbound HTTP fan-out**: an app must be
able to issue many independent target requests concurrently, enforce per-request and
whole-fan-out batch deadlines, keep memory predictable, and run the *same handler source*
unchanged on Axum, Cloudflare Workers, Fastly Compute, and Spin.

"Predictable memory" here means: a documented, bounded cost per buffered outbound request
and response, plus an explicit batch-level memory model the app controls (§3.4.4).
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
  `app-demo`, scaffolding templates, and docs are migrated. No deprecated
  aliases.
- **One portable buffered fan-out primitive.** `send_all` is the only fan-out API
  for buffered request bodies + buffered responses. Its **input/output contract**
  is identical on every adapter (preflight, index alignment, per-slot Ok/Err
  shape — see §3.1.1 / §3.2). **Cross-slot timing is not uniform** — on
  Axum/CF/Spin `join_all` fans out complete exchanges concurrently. On Fastly,
  dispatch is sequential, non-empty request uploads have no finite write bound,
  and buffered response bodies drain serially in harvest order (§3.3.4); the
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
- **Deliverables:** this design plus phased implementation plans. Code changes are
  follow-ups executed from those plans.

## 2. Current state (summary)

| Concern | Today | File |
| --- | --- | --- |
| Outbound trait | `ProxyClient::send(ProxyRequest) -> Result<ProxyResponse, EdgeError>` | `crates/edgezero-core/src/proxy.rs:16` |
| Handle | `ProxyHandle` (`Arc<dyn ProxyClient>`), `RequestContext::proxy_handle()` | `proxy.rs:21`, `context.rs:97` |
| Request type | `ProxyRequest::new(method, uri)`; `ProxyRequest::from_request` (streaming) | `proxy.rs:138`, `proxy.rs:100` |
| Body | `Body { Once(Bytes), Stream(..) }`; `Body::into_bytes_bounded(max)` exists | `body.rs:14`, `body.rs:76` |
| Errors | `EdgeError`: 400/422/404/405/503/500. No 502/504. `#[non_exhaustive]` | `crates/edgezero-core/src/error.rs` |
| Deadlines | No outbound deadline type or dispatch budget; `web_time::Instant` already exists elsewhere in core | `middleware.rs`, `key_value_store.rs` |
| Fastly send | `send_async_streaming()` then `pending_request.wait()` — serializes | `crates/edgezero-adapter-fastly/src/proxy.rs:30` |
| Fastly backend name | host with only `.`/`:` sanitized | `crates/edgezero-adapter-fastly/src/proxy.rs:110` |
| Manifest | no capability declaration or outbound host plumbing | `crates/edgezero-core/src/manifest.rs` |
| Adapter trait | `execute` / `name` plus existing non-outbound lifecycle hooks; no capability metadata | `crates/edgezero-adapter/src/registry.rs` |
| Contract tests | exist for Cloudflare/Fastly/Spin; **Axum has none** | `crates/edgezero-adapter-*/tests/contract.rs` |
| Scaffold templates | emit proxy code | `crates/edgezero-cli/.../handlers.rs.hbs`, `spin.toml.hbs:13` |
| Public docs | document `ProxyService`/`ProxyRequest` | `docs/guide/proxying.md`, `docs/guide/handlers.md`, `docs/guide/architecture.md`, `docs/guide/what-is-edgezero.md`, `docs/guide/adapters/*` |

## 3. Design

> **⚠️ Code blocks in this spec are ILLUSTRATIVE, and many predate the strict lint
> gate.** The workspace denies `clippy::restriction` and warns `clippy::pedantic` under
> `-D warnings` (root `Cargo.toml`). Any snippet copied into an implementation **must**
> be brought to that gate before it will compile in CI. The recurring offenders, with
> their required forms (the Phase 1a plan carries the full list and a verified-green
> example):
> - **`missing_inline_in_public_items`** → every public fn needs `#[inline]`.
> - **`min_ident_chars`** → no single-char idents (`d`→`duration`, `e`→`err`, `m`→`manifest`).
> - **`arithmetic_side_effects`** → no bare `+`/`-`/`*`/`/`; use `checked_*` / `saturating_*` / `try_from`.
> - **`as_conversions`** → no `as` casts; use `From`/`TryFrom`/`u64::from`.
> - **`expect_used` / `unwrap_used`** → forbidden in production (allowed in `#[cfg(test)]`); use `?`/`ok_or`.
> - **`arbitrary_source_item_ordering`** → consts, enum variants, and impl fns alphabetical (or a documented `#[expect(..)]`).
> - **`duration_suboptimal_units`** → `Duration::from_hours(168)`, `from_mins(1)` — not `from_secs(7*24*60*60)` / `from_secs(60)`.
>
> Where a snippet below still shows a bare-`d` closure param, `x as u64`, or
> `a + b`, read it as shorthand for the lint-clean form — do not copy it verbatim.

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
 /// failures surface later, when the caller consumes `resp.into_body()`:
 /// - **Read errors / decompression failures / deadline expiry** during
 /// chunk reads come from the deadline-aware stream wrapper
 /// as `Err(EdgeError::..)` chunks.
 /// - **Over-cap** only fires when the consumer uses a bounded helper
 /// (`OutboundResponse::into_bytes_bounded(max)`, `into_bytes_bounded_until`,
 /// `json_bounded[_until]`) — the streaming decoder itself does **not**
 /// count bytes ( "Cap ownership"). Raw `into_response` passthrough
 /// carries no EdgeZero cap **only on Cloudflare**, the sole adapter that
 /// streams `Body::Stream` lazily to the downstream wire (the wire is the
 /// budget there). **Axum, Fastly, AND Spin all BUFFER `Body::Stream`** in
 /// their response converters within an adapter-level 16 MiB cap
 /// (`AXUM_/FASTLY_/SPIN_RESPONSE_STREAM_BUFFER_BYTES` → 502 on overflow),
 /// so on those three a raw passthrough is still capped. Cloudflare is the
 /// exception, not Axum.
 /// If the caller has *already started writing the downstream response
 /// headers* (e.g. a proxy-forward via `into_response` that the platform
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
 /// (footnote 4): on Axum/CF/Spin it is `Native` (concurrent complete
 /// exchanges), but on Fastly it is `BestEffort` because dispatch is
 /// sequential, non-empty request uploads have no finite host-write bound,
 /// and buffered response-body drains run in harvest order. An earlier
 /// slot can therefore delay later dispatch/observation, and a later slot
 /// whose own budget would have covered it can still return
 /// `gateway_timeout`. Apps that require the stricter cross-slot timing
 /// guarantee declare the capability required and get a hard build failure
 /// on Fastly. `send_all(vec![])` returns `vec![]`.
 ///
 /// **Memory model — CORE-OWNED retained memory only.** This formula bounds the
 /// buffers EdgeZero core holds; it deliberately EXCLUDES (a) adapter-side upload
 /// staging copies (e.g. a `chunk.to_vec()` handed to a platform write path) and
 /// (b) opaque host/runtime buffering (the Fastly/CF/Spin host may retain its own
 /// copy of in-flight bytes). Worst-case core-owned retained buffer for
 /// one `send_all` is `Σᵢ request_bodyᵢ.len + Σᵢ max_response_bytesᵢ`
 /// (per-slot caps). Transient core overhead during a buffered drain adds up to
 /// one in-flight chunk per actively-draining slot (the
 /// `sizeof(current_chunk)` term from §3.4.4); the full core-owned bound is therefore
 /// `Σᵢ request_bodyᵢ.len + Σᵢ max_response_bytesᵢ + Σⱼ
 /// sizeof(current_chunkⱼ)` where j ranges over slots currently in a drain
 /// step. Actual process RSS can exceed this by the excluded adapter/host terms. EdgeZero does NOT impose a global cap on N — apps are
 /// responsible for bounding the number of requests passed in. Fastly attempts
 /// to dispatch every slot before harvest, and every slot whose sequential
 /// dispatch returns is then in flight at the host; an unbounded upload write
 /// can prevent later slots from reaching that state. A `max_concurrency` knob
 /// would not repair that platform gap, so bound N at the application layer
 /// (typically the fan-out batch's target count).
 ///
 /// **Request bodies MUST be buffered (`Body::Once`).** A `Body::Stream`
 /// request body yields `out[i] = Err(EdgeError::bad_request("send_all
 /// requires buffered request bodies; use send for a streamed upload"))`,
 /// identically on every adapter. This rule removes the unbounded
 /// **source-pull** problem from portable fan-out. It does NOT bound Fastly's
 /// guest-to-origin write of a non-empty `Body::Once`; that separate
 /// cross-slot limitation is owned by `send-all-slot-isolation` footnote 4.
 ///
 /// **Response mode MUST be Buffered.** A request whose `response_mode`
 /// is `Streamed` (via `stream_response`) yields `out[i] =
 /// Err(EdgeError::bad_request("send_all requires buffered responses;
 /// use send for a streamed response"))`, identically on every adapter.
 /// Reason: `send_all` returns its `Vec` only after every slot has reached
 /// headers, so a fast slot's deadline-aware streamed body wrapper has
 /// already been running while later siblings were still in headers phase
 /// — by the time the consumer gets the Vec, the fast slot's body may
 /// already be at-or-past its deadline. There is no concurrent
 /// body-consumption primitive in `send_all` to fix this (Fastly has no
 /// guest reactor; even on Axum/CF/Spin a consumer iterating
 /// `out[i].body` serially can't outrun the wrapper deadlines that have
 /// been ticking since headers). Apps that want streamed responses use
 /// single `send` and orchestrate concurrency themselves on the three
 /// reactor-bearing adapters — the canonical pattern is `futures::join_all`
 /// of N `send` calls, then consume each `OutboundResponse` via the
 /// **app-facing consuming accessor `into_body -> Body`** and
 /// iterate the `Body::Stream` chunks concurrently across the N slots.
 /// `into_parts(..)` exists too but is labelled adapter-facing because it
 /// returns the (request method, status, headers, body) tuple that response converters
 /// need; pure orchestration paths just want the body. This rule keeps
 /// `send-all-slot-isolation`'s `Native` claim on Axum/CF/Spin honest —
 /// the cross-slot body-lifetime problem is removed by construction rather
 /// than papered over.
 ///
 /// **"Identical" scope.** The trait contract guarantees identical
 /// **input handling**: same preflight, same index alignment, same
 /// per-slot Ok/Err shape. The *cross-slot timing behaviour* is **not**
 /// uniform — see the `send-all-slot-isolation` capability.
 /// On Axum/CF/Spin `join_all` fans out complete exchanges concurrently and a
 /// slot's result reflects what it would have produced in isolation.
 /// On Fastly sequential dispatch, an unbounded buffered request upload,
 /// cold backend registration, or harvest-order response-body drain can
 /// delay later slots. A slot can therefore return `gateway_timeout` even
 /// when its own `budget.deadline` would have covered it in isolation. Apps that require cross-slot
 /// isolation declare the capability required and get a hard build
 /// failure on Fastly.
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
// crates/edgezero-core/src/context.rs — replaces proxy_handle
// After the round-6 restructure, the context exposes `parts` rather than
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
    max_request_body_bytes: u64,         // cap when `body` is Body::Stream (default 8 MiB)
}

// **All OUTBOUND public byte caps and byte-accounting counters are `u64`, NOT `usize`.**
// This invariant is intentionally scoped to the outbound APIs introduced or changed here.
// The
// crate compiles to `wasm32` on three of the four adapters, where `usize` is **32-bit**:
// a cap or a wire `Content-Length` above 4 GiB is not merely wrap-prone, it is
// **unrepresentable** as `usize`, which would silently break the "portable across all
// four adapters" claim on exactly the targets that matter. `u64` gives one ceiling
// (16 EiB) on every target. The buffered `Bytes` a drain produces is still `usize`-length
// (bounded by available guest memory), but the *cap* and the *running total* it is
// compared against are `u64`, so the comparison and the arithmetic cannot wrap or
// truncate. `Content-Length` is parsed as `u64`; for an **identity** response (no
// `content-encoding`) it equals the decompressed size, so it can be compared against the
// `u64` cap for an early over-cap reject BEFORE buffering. For a **compressed** response
// the wire `Content-Length` is the compressed size and does NOT bound the decoded size,
// so the cap is enforced only incrementally during decompression (§3.4.1) — no early
// reject. Conversions use `u64::from` / `TryFrom`, never `as` (denied lint).

/// How the adapter delivers the response body. Default is `Buffered`.
pub enum ResponseMode {
 /// Adapter reads the full body within the deadline, enforcing a decompressed
 /// byte cap. `OutboundResponse.body` is `Body::Once`.
    Buffered { max_bytes: u64 },     // default max_bytes = DEFAULT_MAX_RESPONSE_BYTES
 /// Adapter returns headers; `OutboundResponse.body` is `Body::Stream`. The
 /// caller buffers later (e.g. `into_bytes_bounded`) or passes the body through.
    Streamed,
}

impl OutboundRequest {
 /// Constructors validate **and canonicalize** the URI:
 ///
 /// - Scheme must be `http` or `https` (plain `http` is permitted —
 /// required for loopback contract tests). Other schemes →
 /// `Err(EdgeError::bad_request("outbound URI scheme must be http or
 /// https"))`.
 /// - An authority must be present. Missing authority →
 /// `Err(EdgeError::bad_request("outbound URI must be absolute with
 /// authority"))`.
 /// - **Userinfo is rejected.** `https://user:pass@example.com` →
 /// `Err(EdgeError::bad_request("outbound URI must not contain
 /// userinfo; pass credentials via the `authorization` header"))`.
 /// This keeps the Fastly backend Host override unambiguous and
 /// stops accidental credential leakage.
 /// - **Fragments are rejected at the string-input boundary.**
 /// `OutboundRequest::get("https://x/p#anchor")` and `::post(..)` parse
 /// the input as a string *first* (they take `impl AsRef<str>` — see
 /// below) and reject a `#` before `http::Uri` ever sees it, with
 /// `Err(EdgeError::bad_request("outbound URI must not contain a
 /// fragment"))`. `http::Uri` truncates at `#`, so a Uri-typed input
 /// has already lost the fragment by the time we receive it.
 /// `OutboundRequest::new(method, uri)` and `OutboundRequest::from_parts`
 /// therefore cannot detect fragments — the caller built a `Uri`, which
 /// means whatever was after `#` is gone. Documented asymmetry, not a
 /// silent surprise: when constructing from a raw string use
 /// `get`/`post` and you get fragment rejection for free; when you
 /// already hold a `Uri`, fragments are not an issue because they were
 /// stripped during `Uri` parsing.
 /// - **Default ports are normalized away.** A `Uri` parsed from
 /// `https://example.com:443` is rewritten so `uri.port` returns
 /// `None`; `http://example.com:80` likewise. This means
 /// `https://example.com` and `https://example.com:443` produce
 /// identical `OutboundRequest`s — same `resolved_port` in the
 /// Fastly identity, same Host override, one dynamic backend. Explicit
 /// non-default ports (`:8443`, `:3000`) are preserved verbatim.
 /// - **Scheme and host are lowercased.** Per RFC 3986 (scheme) and
 /// (host) both are case-insensitive, so `https://EXAMPLE.com`,
 /// `HTTPS://example.com`, and `https://example.com` are the same
 /// origin. The canonicalization rewrites the stored URI to lowercase
 /// so `OutboundRequest::uri` always reports the lowercase form,
 /// and downstream consumers (including Fastly backend identity in §4.3,
 /// app-level allowlist checks, Spin `allowed_outbound_hosts`
 /// matching) compare against one canonical spelling. Userinfo and
 /// fragments are already rejected above; path and query are passed
 /// through verbatim (case-sensitive per RFC 3986 §6.2.2.1).
 ///
 /// These canonicalizations run inside the constructors before the URI
 /// is stored, so every downstream consumer (Fastly backend identity, Host
 /// override, allowlist checks) sees a single canonical form.
    pub fn new(method: Method, uri: Uri) -> Result<Self, EdgeError>;
 /// `get` and `post` take `impl AsRef<str>` (not `TryInto<Uri>`) so the raw
 /// string is available for fragment detection *before* `http::Uri`
 /// truncates at `#`. The impl checks for `#` in the input bytes, then
 /// parses with `Uri::try_from(&str)`, then runs the rest of
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
 /// `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`,
 /// `transfer-encoding`, `upgrade` (RFC 7230), plus every header
 /// named in the inbound `connection` header value;
 /// - `host` is **dropped** from the headers. The adapter sets the final
 /// `Host` value (or platform SDK equivalent) from
 /// `req.host_authority` at SDK-construction time — the same
 /// canonical accessor every adapter uses. The accessor
 /// already encodes the rules: explicit port preserved when the URI
 /// carries a non-default port (`https://example.com:8443` →
 /// `Host: example.com:8443`); port stripped when default
 /// (`https://example.com` → `Host: example.com`); IPv6 hosts
 /// bracketed. **Adapters MUST NOT read `req.uri` for the Host
 /// value** — `host_authority` is the single source of truth, so the
 /// Fastly identity hash, the Cloudflare `set_header("host"..)` arg,
 /// the Axum reqwest Host setter, and the Spin outgoing-request Host
 /// field all observe the same string. No part of the pipeline reads
 /// `host` from `req.headers`. `normalize_for_dispatch` re-strips
 /// `host` defensively as a safety net for callers that reached past
 /// `header(..)` via `headers_mut`;
 /// - `content-length` is dropped — the adapter sets it from the new body
 /// for `Body::Once`, or omits it (relying on chunked transfer) for
 /// `Body::Stream`.
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
 /// 1. `HeaderName::from_bytes(name.as_ref)` — strict name check (HTTP
 /// grammar).
 /// 2. `std::str::from_utf8(value.as_ref).is_err` → reject with
 /// `EdgeError::bad_request("header value is not valid UTF-8: <name>")`
 /// (the EdgeZero rule).
 /// 3. `HeaderValue::from_bytes(value.as_ref)` — applies the **HTTP
 /// header-value byte rule** (visible ASCII + obs-text; rejects
 /// control bytes like `\n`, `\0` that would enable header injection).
 /// Combined with step 2, the values that survive are exactly the ones
 /// that are **both** valid UTF-8 **and** valid HTTP header bytes — a
 /// valid-UTF-8 string containing a forbidden control byte is still
 /// rejected, which is intended security behaviour. Two distinct error
 /// messages distinguish the cause (forbidden-bytes vs invalid-UTF-8).
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
 /// `normalize_for_dispatch` sweep before the request is issued.
    pub fn headers_mut(&mut self) -> &mut HeaderMap;

 /// Set the body from `Body` or a buffered conversion. `Bytes`, `Vec<u8>`,
 /// `&[u8]`, `&str`, and `String` convert to `Body::Once`. A raw `Stream`
 /// does not implement `Into<Body>`; callers wrap typed streams with
 /// `Body::from_stream` and arbitrary external streams with
 /// `Body::from_external_stream`.
    pub fn body(self, body: impl Into<Body>) -> Self;
 /// Serialize `value` as JSON and set the request body to the resulting
 /// bytes. Sets `content-type: application/json` only if the request has
 /// no `content-type` yet — a caller-set value is preserved unchanged.
 /// `content-length` is left to the adapter (it is recomputed from the
 /// serialized body for `Body::Once` and omitted for `Body::Stream`).
 /// Serialization failure yields `Err(EdgeError::internal(..))`.
    pub fn json<T: Serialize>(self, value: &T) -> Result<Self, EdgeError>;

    pub fn timeout(self, d: Duration) -> Self;
    pub fn deadline(self, d: Deadline) -> Self;
    pub fn max_response_bytes(self, n: u64) -> Self;        // sets Buffered { n } (u64 — see cap note)
    pub fn stream_response(self) -> Self;                   // sets Streamed

 /// Cap on the **request** body when it is a `Body::Stream` — see
 /// EdgeZero's core `Body::Stream` is `LocalBoxStream`
 /// (WASM-friendly, not `Send + 'static`), so adapters cannot hand it
 /// directly to a SDK that requires `Send` streams (notably reqwest
 /// without its `stream` feature). The contract is therefore: streamed
 /// request bodies are **bounded** by this cap on every adapter; adapters
 /// MAY pass the stream through to the platform natively (Fastly's
 /// `send_async_streaming`, Spin's WASI outgoing body) or buffer to
 /// `Bytes` within the cap before dispatch (Axum, Cloudflare). Over-cap
 /// during drain → `bad_request` (400) — a client-side misuse.
 /// Default `DEFAULT_OUTBOUND_REQUEST_BODY_BYTES = 8 MiB`.
    pub fn max_request_body_bytes(self, n: u64) -> Self;

    pub fn method(&self) -> &Method;
    pub fn uri(&self) -> &Uri;          // apps inspect this for their own allowlist
    pub fn headers(&self) -> &HeaderMap;

 // ---- Canonicalized URI accessors (adapter-facing, non-consuming) ----
 //
 // These four accessors are the **single canonical source** of the
 // host/port/SNI/cert-host split that every adapter needs. They are
 // derived from `self.uri` after the canonicalization rules
 // have rejected **userinfo and fragments**, validated the port, and
 // lower-cased scheme + host. **Path and query are preserved verbatim**
 // (case-sensitive per RFC 3986 §6.2.2.1); they do not
 // appear in these accessors because none of them are host/port/SNI/cert
 // values, but they remain accessible via `self.uri` for the wire-level
 // request line. **Adapters MUST consume these accessors rather than
 // re-deriving from `uri`** for the host/port/SNI/cert split — both to
 // share the canonicalization logic and so the Fastly identity hash
 // sees a single canonical form. They are also the values
 // tested by the Tier 1 half of the four-value row.
 //
 // **Manifest `[capabilities.outbound].hosts` entries are a separate
 // grammar** — those entries are host-authority-only
 // declarations, so the manifest-host validator **rejects** path / query
 // / fragment / userinfo on the manifest side. That validator and the
 // request-URI canonicalization rules above share the userinfo / fragment
 // reject and the lowercase-scheme/host pass, but diverge on path/query:
 // request URIs pass them through; manifest host entries reject them. The
 // two rule sets must not be conflated.

 /// Connection target — always `"<host>:<port>"`, with the port resolved
 /// (default ports filled in: `http` → 80, `https` → 443). IPv6 hosts
 /// are bracketed (`[::1]:443`). This is what Fastly's
 /// `Backend::builder(name..)` expects and what Spin uses for its
 /// `allowed_outbound_hosts` rendering when the source had no explicit
 /// port. Stable across canonicalization (same value whether the input
 /// was `https://example.com` or `https://example.com:443`).
    pub fn backend_target(&self) -> String;

 /// Authority for the outgoing `Host` header. Carries the explicit port
 /// **only when it is non-default** for the scheme:
 /// `https://example.com:8443` → `"example.com:8443"`;
 /// `https://example.com` → `"example.com"`. IPv6 hosts are bracketed.
 /// This is what Fastly's `.override_host(..)` and Cloudflare's
 /// outbound `Request::set_header("host"..)` consume; Axum / Spin pick
 /// it up the same way.
    pub fn host_authority(&self) -> String;

 /// SNI hostname — what an HTTPS adapter passes to its TLS stack's
 /// SNI setter (Fastly's `.sni_hostname(..)`, Spin/CF's underlying
 /// TLS config, etc.). Port-stripped, bracket-stripped for IPv6.
 /// **Returns `None` for IP-literal hosts** (IPv4 and IPv6)
 /// RFC 6066, which forbids SNI for IP literals. Adapters call
 /// the TLS-stack SNI setter only when this returns `Some`; for `None`
 /// the SNI extension is omitted from the ClientHello. **Adapters
 /// MUST NOT fall back to `uri.host` for SNI** — `None` here
 /// means "send no SNI," not "derive it yourself." The cert verification
 /// host is `cert_host` below, not this accessor.
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
 /// across every adapter**; adapters MUST NOT call `uri.host` and
 /// post-process — they call `cert_host` and pass it through.
 ///
 /// Concrete examples:
 /// - `https://example.com` / `https://example.com:443` → `Some("example.com")`
 /// - `https://example.com:8443` → `Some("example.com")` (port stripped — cert is not port-qualified)
 /// - `https://127.0.0.1` → `Some("127.0.0.1")`
 /// - `https://[::1]` / `https://[::1]:443` → `Some("::1")` (brackets stripped)
 /// - `http://example.com` → `None`
    pub fn cert_host(&self) -> Option<&str>;

 // ---- Adapter-facing inspection (non-consuming) ----
 /// Cheap non-consuming check used by `send_all` preflight: if `true`,
 /// the slot is rejected with `bad_request`
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

/// The public streamed-body error surface is exact: adapters, decoders, and
/// other EdgeZero-owned producers can carry typed 502/504/over-cap failures
/// without converting them through `anyhow::Error`.
pub enum Body {
    Once(Bytes),
    Stream(LocalBoxStream<'static, Result<Bytes, EdgeError>>),
}

impl Body {
 /// Construct a fallible stream whose errors are already classified. This is
 /// the required constructor for every in-tree adapter, decoder, and deadline
 /// wrapper that can emit an `EdgeError`.
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, EdgeError>> + 'static;

 /// Explicit compatibility boundary for arbitrary external stream errors.
 /// Every source error is converted to `anyhow::Error` and then wrapped as
 /// `EdgeError::internal`; this constructor never attempts to recover a typed
 /// `EdgeError` hidden inside the external error.
    pub fn from_external_stream<S, E>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + 'static,
        anyhow::Error: From<E>;

 /// Construct an infallible byte stream.
    pub fn stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Bytes> + 'static;

 /// Returns the exact typed stream for `Body::Stream`, or `None` for
 /// `Body::Once`.
    pub fn into_stream(
        self,
    ) -> Option<LocalBoxStream<'static, Result<Bytes, EdgeError>>>;
}

impl From<Bytes> for Body {
    fn from(value: Bytes) -> Self { Body::Once(value) }
}
// The existing `From<Vec<u8>>`, `From<&[u8]>`, `From<&str>`, and
// `From<String>` implementations remain buffered conversions.

// The separate constructors are intentional. Stable Rust cannot provide one
// generic `from_stream<S, E>` that preserves `E = EdgeError` but maps every
// other `E` to `internal` without overlapping implementations/specialization.
// In-tree code MUST NOT pass an `EdgeError` stream through
// `from_external_stream`, because doing so would erase its status and kind.

/// Disassembled form of an `OutboundRequest`. Adapter-facing only.
pub struct OutboundRequestParts {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: Body,
    pub timeout: Option<Duration>,
    pub deadline: Option<Deadline>,
    pub response_mode: ResponseMode,
    pub max_request_body_bytes: u64,      // applies when `body` is Body::Stream (u64 — see cap note)
}

/// Result of the authoritative raw-response metadata pass. Adapters must act on this
/// before inspecting content encoding/length or constructing `OutboundResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBodyDisposition {
    FramingBodyless, // HEAD, 1xx, 204, or 304: settle/drop the platform handle; never decode
    Payload,         // normal payload path: inspect encoding, decode, and enforce the cap
    ResetContent {
        // Captured before the helper rewrites the downstream `content-length` to zero.
        declared_body: bool,
    }, // 205: apply the bounded settle/abort protocol in §3.4.1
}

pub fn normalize_response_headers(
    request_method: &Method,
    status: StatusCode,
    headers: &mut HeaderMap,
) -> Result<ResponseBodyDisposition, EdgeError>;

pub struct OutboundResponse {
    request_method: Method,          // retained for HEAD/bodyless response semantics
    status: StatusCode,
    headers: HeaderMap,
    body: Body,                     // Once in Buffered mode, Stream in Streamed mode
}

impl OutboundResponse {
 /// Adapter-facing post-conversion constructor. Before calling this, the adapter MUST run
 /// `normalize_response_headers` on the raw response metadata, settle any bodyless response,
 /// and only then inspect `content-encoding` / `content-length`, decode, and cap. The headers
 /// supplied here are therefore already safe for app-facing `headers()` / `into_body()`:
 /// hop-by-hop fields and every `connection` nomination are gone; decompression has removed
 /// `content-encoding` / `content-length`; and lossy UTF-8 handling has run. `Body::Once` is
 /// used in Buffered mode after the adapter has drained and capped; `Body::Stream` is wrapped
 /// with the decoded-output deadline guard in Streamed mode.
    pub fn new(
        request_method: Method,
        status: StatusCode,
        headers: HeaderMap,
        body: Body,
    ) -> Self;

 /// Adapter-facing destructure. Mirrors `OutboundRequest::into_parts`; the retained
 /// method is required by response converters for HEAD/bodyless normalization.
    pub fn into_parts(self) -> (Method, StatusCode, HeaderMap, Body);

 /// Adapter-facing mutation point — used during construction (e.g. to
 /// strip `content-encoding` after decompression). App code uses the
 /// immutable `headers` accessor instead.
    pub fn headers_mut(&mut self) -> &mut HeaderMap;

 // ---- App-facing accessors ----
    pub fn status(&self) -> StatusCode;
    pub fn is_success(&self) -> bool;       // 2xx
    pub fn headers(&self) -> &HeaderMap;
    pub fn body(&self) -> &Body;

 /// **App-facing consuming accessor** for the response body — the orchestration
 /// path for streamed responses recommended by `send_all`'s rustdoc.
 /// Returns the underlying `Body` so app code can iterate `Body::Stream` chunks
 /// directly (the wrapper installed at response construction time still
 /// enforces `dispatch_budget(req).deadline`) or extract the
 /// `Body::Once` `Bytes` if the adapter buffered. This is distinct from the
 /// adapter-facing `into_parts(self) -> (Method, StatusCode, HeaderMap, Body)`
 /// destructure used inside response converters; apps that need just the
 /// body for streaming orchestration call `into_body` and drop the rest.
 /// On `Streamed` mode with single `send`, this is the canonical orchestration
 /// path: drive `send` concurrently across N requests via `futures::join_all`
 /// on Axum/CF/Spin, then iterate each response's `into_body` stream in
 /// parallel — no `send_all` (which is buffered-only by design).
    pub fn into_body(self) -> Body;

 /// Buffer the body with a decompressed-byte cap. Works for both `Once`
 /// and `Stream`. Over-cap yields `Err(EdgeError::response_too_large(..))`
 /// (distinct kind, 502 — NOT `bad_gateway`; §3.4.1).
 ///
 /// This is NOT a thin wrapper over `Body::into_bytes_bounded` — that
 /// helper maps over-limit to `bad_request` (400), correct for inbound
 /// bodies but wrong for an over-large upstream response. This method
 /// performs its own bounded drain (pre-append checked accounting)
 /// and maps over-cap to `response_too_large` (distinct kind, 502 —
 /// §3.4.1; consistent with the top of this doc, NOT `bad_gateway`).
 /// On adapters that decompress
 /// the cap is enforced against decompressed output here too.
 ///
 /// **Effective-budget deadline is already honoured on a streamed body.**
 /// Adapters wrap `Streamed` response bodies with a deadline-aware stream bounded by
 /// `dispatch_budget(req).deadline` — which is non-`None` even for
 /// timeout-only and no-deadline requests (the synthetic 30 s ceiling) —
 /// so a detected expiry yields a `gateway_timeout` error chunk and this
 /// drain returns 504. Axum/Cloudflare provide timer-backed cancellation;
 /// Spin and Fastly retain the BestEffort host gaps documented in §3.5.2.
 /// There is no need to thread the deadline through manually — call
 /// `into_bytes_bounded_until(max, deadline)` only when you want to
 /// **cooperatively narrow** the failure timing on top of the request
 /// budget (see the precise bound and caveat below).
    pub async fn into_bytes_bounded(self, max: u64) -> Result<Bytes, EdgeError>;

 /// As `into_bytes_bounded`, but additionally bounded by a `Deadline`
 /// that the caller passes per drain. **The helper is a *cooperative*
 /// post-read / EOF validator, not a timer-backed race.** The bound it
 /// provides is *exactly* "the first `is_expired` check that observes
 /// expiry returns `gateway_timeout`," where the check sites are
 /// enumerated below. A read that is already blocked when the deadline
 /// passes does **not** get preempted by this helper — it returns when
 /// the underlying source returns (chunk, EOF, or wrapper-emitted error
 /// chunk past the request budget), and the helper's *next* check (or
 /// post-return check for `Body::Once`) is what fires. Real-time
 /// preemption is the *wrapper's* job (the adapter installs a
 /// deadline-aware stream bounded by `dispatch_budget(req).deadline` at
 /// response construction time); the helper only catches the
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
 /// `until_deadline.is_expired()` **at entry**, before doing anything
 /// else, and returns `gateway_timeout` if expired. Otherwise it
 /// checks the buffered length against `max` — under cap → `Ok(bytes)`;
 /// over cap → `response_too_large` (distinct kind, 502; NOT `bad_gateway`
 /// — see "Oversize is a distinct outcome", §3.4.1). **Precedence: expired deadline beats
 /// over-cap** (an over-cap error after the deadline has expired is
 /// masked by the deadline check, since the caller's `until` rolled
 /// the result regardless of cap behaviour). This entry-time check
 /// makes single `send` + `Body::Once` callers see consistent
 /// `gateway_timeout` semantics whether their response arrived
 /// already-buffered or streamed.
 /// - **`Body::Stream`**: the helper checks `until_deadline.is_expired()`
 /// **both before issuing each blocking body read and again after it
 /// returns** — including the EOF read. Returns
 /// `Err(EdgeError::gateway_timeout(..))` (504) on the first expired
 /// check.
 ///
 /// **Enforcement composes layer-wise without sharing state.** The
 /// adapter wrapper installed at response construction time enforces
 /// the request's `dispatch_budget(req).deadline` by yielding
 /// `Err(EdgeError::gateway_timeout(..))` chunks past *that* deadline
 ///; this helper enforces `until_deadline` cooperatively at
 /// the four check sites enumerated above (entry for `Body::Once`;
 /// before and after each underlying read including EOF for
 /// `Body::Stream`). **"Whichever fires first" is at yield boundaries
 /// only**: the wrapper's error chunk is timer-selected on Axum / CF / Spin
 /// (with Spin's host teardown still BestEffort) and cooperatively detected
 /// on Fastly; the
 /// helper's `until_deadline` fires at the next check site. If the
 /// caller's `until_deadline` is tighter and the next underlying read
 /// returns promptly, the helper fires first; if the next underlying
 /// read blocks past `until` but within the wrapper's budget, the helper
 /// still fires (post-read check) and the helper's bound is "read
 /// latency + at most one extra check," not zero. There is no shared
 /// "effective deadline" stored on `OutboundResponse` (which carries
 /// request method / status / headers / body), and no `min(..)` computation in the
 /// helper. Apps that need a single combined check with **timer-backed
 /// preemption** of the tighter deadline pass
 /// `min(req_deadline, app_inner_deadline)` to `.deadline(..)` on the
 /// `OutboundRequest` builder instead of layering here — that pushes
 /// the tighter deadline into the wrapper. Adapter support remains exactly
 /// as classified by `outbound-deadlines` in §3.5.2.
 ///
 /// **Enforcement is layered.** The helper itself is cooperative on every
 /// adapter — its before-and-after-read `is_expired` check cannot
 /// preempt a read in progress. Real-time enforcement of the request
 /// budget comes from the adapter wrapping streamed response bodies at
 /// construction time:
 ///
 /// - **Axum, Cloudflare, Spin** — the adapter wraps the response body
 /// with a deadline-aware stream using its platform timer (tokio /
 /// `worker::Delay` / wasi monotonic-clock), bounded by
 /// `dispatch_budget(req).deadline`. That deadline is non-`None` for
 /// every request (synthetic 30 s ceiling when `req.deadline` was
 /// absent), so the wrapping is unconditional — *not* "only when
 /// `req.deadline.is_some`." Each chunk read is bounded by the
 /// request's effective deadline, so a peer that stalls mid-stream
 /// produces an error chunk at that deadline rather than blocking.
 /// `into_bytes_bounded_until`'s helper-side `is_expired` check on
 /// the caller-supplied `until_deadline` is what catches the
 /// *tighter* `until` case (e.g. the wrapper has 500 ms left but the
 /// caller passed a 100 ms `until`) **at the next yield boundary**,
 /// not in real time. If a read happens to block for the full 500 ms,
 /// the helper returns at 500 ms with `gateway_timeout` (post-read
 /// check observed expiry), not at 100 ms. Use
 /// `min(req_deadline, app_inner_deadline)` on the builder for
 /// timer-backed preemption.
 /// - **Fastly** — no guest async timer, but the adapter still
 /// wraps the streamed response body with a **cooperative
 /// deadline-aware stream** that checks `budget.deadline.is_expired()`
 /// **both before issuing the underlying body read and again after it
 /// returns** (including the read that discovers EOF) and
 /// emits a `gateway_timeout` error chunk past the deadline instead
 /// of `Ok(chunk)` or stream-end. This makes `into_bytes_bounded`,
 /// `into_response` passthrough, and any other consumer of the
 /// wrapped body honour the deadline uniformly — the deadline does
 /// not depend on whether the caller chose this helper specifically.
 /// Bounded-cooperative semantics apply: a stream that yields one
 /// chunk and then stalls returns control on the host's
 /// between-bytes-timeout, so worst-case overshoot per chunk
 /// gap is one between-bytes-timeout interval — never unbounded.
 ///
 /// The real-vs-bounded distinction matches the `outbound-deadlines`
 /// capability matrix in Decompression-cap and 502-mapping
 /// behaviour matches `into_bytes_bounded`.
    pub async fn into_bytes_bounded_until(
        self,
        max: u64,
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
    pub async fn json_bounded<T: DeserializeOwned>(self, max: u64)
        -> Result<T, EdgeError>;

 /// As `json_bounded`, additionally bounded by a caller-supplied
 /// `Deadline`. **The caller-supplied deadline is enforced
 /// cooperatively by `into_bytes_bounded_until`** — that is, at the
 /// yield boundaries enumerated in that helper's rustdoc (entry for
 /// `Body::Once`; before and after each underlying read including EOF
 /// for `Body::Stream`). A read already blocked when `deadline` passes
 /// does **not** get preempted by this helper; it returns when the
 /// underlying source returns, and the next check fires. **Real-time
 /// enforcement is the wrapper's job** — Axum / CF / Spin install a
 /// timer-selected deadline-aware stream bounded by
 /// `dispatch_budget(req).deadline` at response construction time
 /// so expiry becomes a typed 504. Spin's host teardown is still cooperative
 /// and BestEffort (footnote 8). Fastly detects expiry cooperatively on the
 /// body phase; its capability remains BestEffort because cold dispatch and
 /// upload-write gaps are unbounded (footnotes 1–2).
 /// The `deadline` argument here only adds the cooperative
 /// post-read tighten; it does not get its own wrapper. Apps that need
 /// timer-backed preemption of a deadline tighter than the request
 /// budget set `.deadline(min(req_deadline, app_inner_deadline))` on
 /// the `OutboundRequest` builder so the tighter deadline lands in the
 /// wrapper. Malformed JSON maps to `bad_gateway` (502).
    pub async fn json_bounded_until<T: DeserializeOwned>(
        self,
        max: u64,
        deadline: Deadline,
    ) -> Result<T, EdgeError>;
 /// Pass the response through as a core `Response` (keeps a streamed body lazy).
 /// Infallible in safe use: like the other terminal methods it takes `self` by
 /// move, so double-consumption of the body is prevented at compile time. The
 /// `Result` carries exactly two error classes on adapters with
 /// `outbound-header-fidelity = Native`: `Err(EdgeError::internal(..))` for an
 /// adapter-invariant violation, and `Err(EdgeError::bad_gateway(..))` for a malformed
 /// `connection` value introduced after construction through adapter-facing mutation.
 /// Malformed visible nomination syntax is rejected on every adapter; a raw non-UTF-8
 /// value is additionally detectable on Native-fidelity adapters. Cloudflare receives
 /// only post-workerd strings, so it cannot detect the raw-byte case; its documented
 /// `BestEffort` behavior is scoped by `outbound-header-fidelity` (§3.5.2). No other
 /// network/status condition produces an error here.
 ///
 /// **RESPONSE-SIDE hop-by-hop normalization is re-applied here as an idempotent defense
 /// (symmetric with the request side, §3.1.4).** The authoritative pass already ran on
 /// raw response metadata before bodyless/decode/cap decisions. This final pass protects
 /// against a later adapter-side `headers_mut()` mutation. A proxied UPSTREAM response
 /// can carry hop-by-hop headers
 /// that MUST NOT be forwarded downstream: `into_response` strips `connection`,
 /// `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`,
 /// `transfer-encoding`, `upgrade`, **AND every header NOMINATED by the response's own
 /// `connection` value** — so an upstream `Connection: x-private` + `X-Private: secret`
 /// cannot leak `X-Private` to the downstream client. Both passes use one core helper,
 /// `outbound::normalize_response_headers(..)` (the response twin of
 /// `normalize_for_dispatch`), so every adapter and passthrough goes through the same
 /// stripping. On Axum/Fastly/Spin, the `connection` header is resolved
 /// **fail-closed** exactly as on the request side: a non-UTF-8 value is rejected
 /// (`bad_gateway`), never silently dropped. On every adapter, a visible nomination that
 /// is not an RFC field-name token is also `bad_gateway`; implementations do not partially
 /// honor a malformed list. Cloudflare applies the same stripping and syntax validation to
 /// the strings visible after workerd processing but cannot detect malformed raw bytes or
 /// recover original non-`set-cookie` field boundaries. §5.4 pins both the portable visible
 /// header behavior and the stronger `outbound-header-fidelity` contract.
    pub fn into_response(self) -> Result<Response, EdgeError>;
}
```

The complete builder surface — `new`/`get`/`post`/`from_request`/`header`/`headers_mut`/
`body`/`json`/`timeout`/`deadline`/`max_response_bytes`/`max_request_body_bytes`/`stream_response`. Every fallible
step returns `EdgeError`, so handler code uses `?` uniformly.

#### 3.1.4 Adapter behaviour contract — redirects and header encoding

These rules define two explicit levels. The portable baseline on all four adapters strips
visible hop-by-hop headers and visible `connection` nominations and preserves repeated
`set-cookie`. The stronger **`outbound-header-fidelity`** capability additionally guarantees
access to raw response-header octets and original field-line boundaries, including
fail-closed malformed `connection`, malformed/repeated `content-encoding` disposition, and
repeated non-`set-cookie` field-line preservation. Axum/Fastly/Spin are `Native`;
Cloudflare is `BestEffort`, so a `required` declaration hard-fails there. Cloudflare applies
the policy to post-workerd strings and comma-joined non-`set-cookie` values; it cannot claim
malformed raw `connection` → 502 or malformed raw `content-encoding` → forced passthrough
because neither the original bytes nor the original field boundaries reach the guest.

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
| Cloudflare | `worker::RequestInit { redirect: worker::RequestRedirect::Manual, .. }` (the enum, **not** the string `"manual"`) |
| Spin (WASI) | the hand-built `wasi:http` request (§4.4) does not auto-follow — no opt-out needed |
| Fastly | `fastly` does not auto-follow — no opt-out needed |

Apps that want to follow a redirect read `resp.headers().get("location")`, run their
allowlist against the new URI, and issue a new request.

**Header value encoding: UTF-8.** EdgeZero requires every outbound and inbound-of-outbound
header value to be valid UTF-8. The rationale is **portability, not a WASI limitation**:
WASI `http` `fields` values are `list<u8>`, so WASI *can* carry arbitrary bytes — but
Cloudflare Workers models headers as JS strings, and other adapters' header types
(`reqwest`'s `HeaderValue`, etc.) do not uniformly round-trip arbitrary bytes. A single
valid-UTF-8 rule is the portable intersection — uniform behaviour beats per-adapter
lossiness for headers that matter. (The check is additionally an HTTP-validity check: a
  UTF-8 string still bearing a forbidden control byte like `\n`/`\0` is rejected —
  §3.1.3 `header(..)`.)

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
- *Outbound response headers on `outbound-header-fidelity = Native` adapters.* If an upstream response carries non-UTF-8 header values,
  **each individual value** is checked (`std::str::from_utf8` on the raw byte slice from
  the platform SDK) — invalid values are dropped, valid sibling values for the same
  header name are preserved. Multi-value headers like `set-cookie` therefore keep
  every valid entry even if one duplicate is invalid. The adapter emits a `log::warn!`
  naming each dropped header.
  **TWO headers are exempt from this silent drop, because dropping one field line would
  change how the REMAINING lines are interpreted** (the same smuggling shape in both
  cases): (a) **`connection`** — resolved fail-closed by the authoritative raw-response
  normalizer before body disposition or decoding (a non-UTF-8 value → `bad_gateway`), else
  a nominated header could smuggle past hop-by-hop removal;
  and (b) **`content-encoding`** — an invalid value **forces the stacked/passthrough
  branch** (§3.4.1): the value is not decoded and `content-encoding`/`content-length` are
  **preserved**, never stripped. Otherwise `content-encoding: gzip` + a second, invalid
  `content-encoding` line would drop to an apparent *single* `gzip`, and the converter
  would decode a body that is still one layer compressed while stripping the very headers
  that revealed it. §5.4 pins a row **crossing** the two rules (repeated field lines where
  one is non-UTF-8). The rest of the response is delivered normally so a
  malformed exotic header cannot poison an otherwise valid fan-out batch response.
  Cloudflare cannot execute this raw-byte branch. It applies normalization only to the
  post-workerd strings it receives; this is the documented `BestEffort` deviation of
  `outbound-header-fidelity`, not part of the all-adapter baseline.
- ***Cloudflare degradation — precisely scoped (verified against `worker` 0.8.3 +
  workerd source).*** Earlier drafts said CF simply "cannot do multi-value headers."
  That is **too pessimistic and wrong for `set-cookie`**. The actual split:

  - **`set-cookie` IS preserved on Cloudflare.** workerd's `getDisplayedHeaders`
    special-cases it: with the `httpHeadersGetSetCookie` compatibility flag on, each
    `set-cookie` is yielded as its **own** `entries()` tuple. **This repo already
    qualifies** — `wrangler.toml.hbs` pins `compatibility_date = "2023-05-01"`, and the
    flag enables at `2023-03-01`. So the collapse today is **not** a platform limit but
    an **EdgeZero bug**: `cloudflare/src/proxy.rs` calls `HeaderMap::insert`, which
    *removes all previous values*. **Fix: `insert` → `append`.** (`worker`'s own
    `http`-feature conversion does exactly this.) A compat-flag-independent hardening
    is to skip `set-cookie` in the `entries()` loop and re-add it via
    `Headers::get_all("set-cookie")`.
  - **Repeated *non*-`set-cookie` headers are irrecoverably comma-joined.** workerd
    joins same-name values (`kj::strArray(values, ", ")`) in both `entries()` and
    `get()`, and its `getAll()` **throws `TypeError` for any name except
    `set-cookie`**. There is **no API** to recover the original separate field lines —
    two `x-foo` headers arrive as `x-foo: a, b`. Per RFC 9110 §5.3 that is semantically
    equivalent for list-valued fields (exactly why the standard special-cases
    `Set-Cookie`), but it is **not byte-faithful**. This is the real, narrow CF
    limitation.
  - **Raw bytes are unreachable; invalid UTF-8 is lost upstream of EdgeZero.** Header
    values cross `v8::String::NewFromUtf8` before the guest sees them, arriving as Rust
    `String`. So on CF the adapter **cannot detect** whether an upstream value was
    invalid UTF-8, and the "drop the invalid value, keep valid siblings" rule degrades
    to "whatever the runtime already decided". Axum, Fastly, and Spin expose raw bytes
    and honour the rule as written.

  > **⚠️ `get_all` landmine.** `worker-sys`'s `getAll` binding has **no `catch`**, and
  > workerd throws `TypeError` for any name other than `set-cookie`. A
  > `get_all("x-foo")` therefore **unwinds across the wasm boundary** rather than
  > returning `Err`. **Only ever call `get_all("set-cookie")`.**

  Narrowing the portable contract to ASCII-only was **rejected** — it would degrade the
  three adapters that *can* do this correctly. The §5.4 non-ASCII round-trip row is
  asserted on Axum / Fastly / Spin and is **best-effort on Cloudflare**.

**Method and request-body portability.** `OutboundRequest` accepts an arbitrary
`Method`, but the platforms do not. Cloudflare's `fetch` restricts the method set and
**forbids a body on `GET`/`HEAD`**; today the CF adapter silently coerces unsupported
methods to `GET`, which is a correctness bug — a `DELETE` would be issued as a `GET`.
The portable contract:

- **Supported methods** (all four adapters): `GET`, `HEAD`, `POST`, `PUT`, `PATCH`,
  `DELETE`, `OPTIONS`. These are guaranteed to reach the upstream with the method
  intact.
- **Custom / extension methods** are **not portable**. An `OutboundRequest` carrying a
  method outside the list above is rejected at **preflight** with
  `Err(EdgeError::bad_request("method <M> is not portable; supported: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"))`
  — on **every** adapter, so the failure is uniform rather than "works on Fastly,
  silently becomes GET on Cloudflare".
- **`GET`/`HEAD` with a non-empty body** is rejected at preflight with
  `Err(EdgeError::bad_request("GET/HEAD request must not carry a body"))` on every
  adapter (CF's `fetch` forbids it; the others would happily send it, so EdgeZero
  normalises to the strictest platform).
- **No silent coercion, ever.** An adapter MUST NOT rewrite the method to make a
  request sendable. Preflight rejects; it never downgrades.

**Enforcement point: one core validator, run at DISPATCH — not at construction.**
Validating at construction is **not enforceable**: `OutboundRequest::body(..)` is an
infallible chainable setter (`pub fn body(self, body: impl Into<Body>) -> Self`), so a
`GET` that passed a construction-time check can acquire a body immediately afterwards
(`OutboundRequest::get(url)?.body(payload)`) and reach the wire unchecked. The rules
above are therefore enforced by a **single core function**:

```rust
// edgezero-core/src/outbound.rs — the ONLY place these rules live.
// PUBLIC, not pub(crate): the four adapters are SEPARATE crates and must call it.
// (A pub(crate) fn is unreachable from edgezero-adapter-{axum,cloudflare,fastly,spin};
// verified — a pub(crate) validator fails to compile at the adapter call site.)
pub fn validate_for_dispatch(req: &OutboundRequest) -> Result<(), EdgeError>;
```

It is called **exactly once per request, immediately before dispatch**, from **both**
paths: every adapter's `send` (first statement, before any platform request is built)
and `send_all`'s per-slot preflight. Neither path may skip it and no adapter re-implements
it — that is what makes the failure identical on all four adapters, keeps a single
`send` and a one-slot `send_all` equivalent (§5.4), and preserves slot-index alignment
(§3.1.1).

**`GET`/`HEAD` + `Body::Stream` is always rejected.** "Non-empty body" is not decidable
for a stream: `Body::Stream` has **no observable emptiness** without polling it, and
polling consumes it. The validator therefore does **not** attempt to peek-and-rechain.
**Preflight precedence (a `GET`/`HEAD` + `Body::Stream` in `send_all` matches TWO rules —
the method/body rule here AND `send_all`'s generic "no `Body::Stream` in buffered fan-out"
rejection).** The **method/body check runs FIRST**, so the error is the specific
`"GET/HEAD request must not carry a streamed body…"` (below), NOT the generic
streamed-in-`send_all` message — the more informative, method-specific diagnostic wins.
(§5.4 pins this precedence with a `send_all([GET + Body::Stream])` row asserting the
method-specific message.) The rule is:

| Method | Body | Outcome |
| --- | --- | --- |
| `GET` / `HEAD` | `Body::Once(empty)` | `Ok` |
| `GET` / `HEAD` | `Body::Once(non-empty)` | `bad_request("GET/HEAD request must not carry a body")` |
| `GET` / `HEAD` | **`Body::Stream` (any)** | `bad_request("GET/HEAD request must not carry a streamed body; emptiness cannot be determined without consuming the stream")` — rejected **unconditionally**, even if the stream would have yielded nothing |
| other methods | any | `Ok` (subject to the portable-method check above) |

Rejecting an empty-but-streamed `GET` is a deliberate, documented false-positive: the
alternative (peek the first chunk and re-chain it) adds a buffering seam to every
request for a case with no legitimate use.

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

**Response normalization happens before semantic interpretation.** Every adapter calls the
public core helper `outbound::normalize_response_headers(..)` immediately after extracting
the upstream method/status/header fields and **before** bodyless handling, `Content-Encoding`
selection, early `Content-Length` rejection, decompression, or construction of
`OutboundResponse`. The helper resolves every visible `connection` line fail-closed, strips
the standard hop-by-hop fields plus all valid nominations, applies the response UTF-8 policy,
and returns the method/status body disposition used by §3.4.1. This order is security-
relevant: `Connection: content-encoding` and `Connection: content-length` must remove those
fields before either can influence decoding or cap logic. As a result, app-facing
`OutboundResponse::headers()` and `into_body()` never expose raw hop-by-hop metadata.
`into_response()` runs the same helper again only as an idempotent defense against later
adapter-facing mutation.

For 205, `ResponseBodyDisposition::ResetContent { declared_body }` captures whether a
still-visible, valid `Content-Length` declared a positive length **after** nomination
stripping but **before** the helper rewrites the downstream value to `0`. This lets an
adapter abort immediately without consulting a header the normalizer has already changed.

For response `connection`, each comma-delimited nomination across every field line must be
a non-empty RFC field-name token after optional whitespace. A non-UTF-8 value (where raw
bytes are available), empty member, forbidden byte, or invalid token rejects the whole
upstream response as `bad_gateway` (502); valid prefixes are not partially honored.
Cloudflare enforces the same visible-token rule on post-workerd strings but cannot detect a
raw malformed value that workerd did not expose, which remains its header-fidelity caveat.

**Final normalization at dispatch (`outbound::normalize_for_dispatch`).** Two surfaces
bypass the construction-time `header(..)` check — `headers_mut()` exposes raw
`HeaderMap`, and `from_request(..)` carries inbound headers in. Adapters MUST call a
core helper `outbound::normalize_for_dispatch(&mut OutboundRequest)` immediately before
handing the request to the platform SDK. The helper is idempotent and runs the same
rules end-to-end:

1. **First**, handle every `connection` field value (see step 3's nomination list) **before**
   any UTF-8 drop, because it governs the removal of *other* headers. A `connection`
   value that is **not valid UTF-8 is rejected** (`EdgeError::bad_request`), NOT silently
   dropped: dropping it would discard the removal intent and let a sender **smuggle a
   nominated header past hop-by-hop stripping** by appending an invalid byte
   (`Connection: x-private,<invalid>` would otherwise forward `X-Private`). This is the
   one header where the lossy drop below is a security hole, so it fails closed instead.
   Parse each comma-delimited nomination after optional whitespace as an RFC field-name
   token. An empty item, forbidden byte, or otherwise invalid token rejects the request as
   `bad_request`; do not strip only a valid prefix and continue. Header-name comparison and
   removal are case-insensitive, and nominations are accumulated across repeated
   `connection` field lines before any nominated field is removed.
2. Drop any *other* header value that is not valid UTF-8 (drop + `log::warn!` naming the
   header) — same lossy semantics as the response side. This applies **only** to
   values that arrived via `headers_mut()` or `from_request(..)` (which carries
   inbound headers verbatim). `OutboundRequest::header(..)` already rejects invalid
   UTF-8 at construction with `bad_request` (§3.1.3), so a non-UTF-8 value can only
   reach this stage by bypassing the checked builder. The policy split is
   deliberate: construction is loud (caller error → 400); proxy-forward and
   pre-validated-map paths are lossy (don't fail an otherwise-good forward over an
   exotic header). The `warn!` makes the drop observable in either case. **The
   `connection` header is exempt — it was already resolved fail-closed in step 1.**
3. Strip hop-by-hop headers (`connection`, `keep-alive`, `proxy-authenticate`,
   `proxy-authorization`, `te`, `trailer`, `transfer-encoding`, `upgrade`, plus every
   header named in any `connection` header value — parsed and validated from the
   now-guaranteed-UTF-8 values per step 1). Idempotent for `from_request`
   output; mandatory for manually built requests.
4. Remove `host` — `normalize_for_dispatch` is the single source of truth for stripping
   it from the request; the adapter then sets the final `Host` header (or platform
   SDK equivalent) from `req.host_authority()` at SDK-construction time — the canonical
   accessor (§3.1.4) — and does **not** re-read whatever was in `req.headers()` nor
   reconstruct it from `req.uri()` directly. `from_request` (§3.1.3) also drops `host`
   so the two sites agree end-to-end: the request structure carries no `host` from the
   moment it leaves the core builders; the value on the wire comes from
   `host_authority()`, which itself is derived from the canonicalized URI. One
   accessor, one canonical string, every adapter consumes the same value.
5. Remove `content-length` — the adapter sets it from the body (length for
   `Body::Once`; omitted for `Body::Stream`).
6. Remove `transfer-encoding` — the adapter sets it per body type and HTTP version.

Apps can therefore use `headers_mut()` and `from_request` freely; portability and
framing safety are guaranteed by this final sweep, not by individual callers
remembering to sanitize.

**Multi-value headers preserved.** `HeaderMap` permits repeated names — `set-cookie`,
`warning`, custom tracing headers, etc. EdgeZero adapters MUST preserve every entry for
a repeated header **on requests, and for response `set-cookie`**; repeated
*non-`set-cookie`* **response** field-lines are **outside the portable baseline**
(Cloudflare comma-joins them — the documented §3.1.4 exception), so apps needing that
fidelity declare the capability and target a `Native` adapter. Within that scope: use
`HeaderMap::append` (never `insert`) when building, and read with `get_all` (never `get`)
when serializing to the platform SDK or deserializing platform responses. Per-adapter mechanics (the spots
current code uses single-value APIs that collapse):

| Adapter | Request side (build platform request) | Response side (read platform response) |
| --- | --- | --- |
| Axum | `reqwest::RequestBuilder::header` (calls `HeaderMap::append`) | iterate `reqwest::Response::headers()` which is already a `HeaderMap` — preserve as-is |
| Cloudflare | `worker::Headers::append(name, value)` — **not** `set` (collapses) | iterate `worker::Headers` entries; `set-cookie` is enumerated separately by the worker runtime, handled explicitly |
| Fastly | `fastly::Request::append_header(name, value)` — **not** `set_header` | `fastly::Response::get_header_all(name)` per name, **not** `get_header` (returns first only) |
| Spin | append via the WASI HTTP `fields` resource (`wasip3::http::types::Fields::append`, re-exported through `spin_sdk`) — natively multi-value. There is **no** `spin_sdk::http::Headers` type; earlier drafts named one that does not exist | iterate WASI `fields` per name |

Contract tests in §5.4 exercise repeated `set-cookie` response headers and repeated
outbound request headers, so any regression to collapsing duplicates is caught at CI
time. If a future SDK update breaks multi-value round-tripping on one adapter, the
spec downgrades the contract for that adapter and documents the limitation rather than
silently dropping headers.

### 3.2 Concurrent fan-out

`HttpClient::send_all` is the single concurrency API **for buffered fan-out** — the
pattern it serves: N requests, each with a *buffered* response (`send_all` is
buffered-only by design; it rejects `Body::Stream` requests and `Streamed` response mode
in preflight). It is concurrent on Axum/Cloudflare/Spin and uses Fastly's
dispatch-all-then-harvest mechanism. Fastly's sequential dispatch can still be held by an
unbounded non-empty request upload before every later slot is in flight; that limitation is
reported by `send-all-slot-isolation = BestEffort`. The **input/output contract** is
identical (preflight, index alignment, per-slot Ok/Err shape). Cross-slot
timing **is not uniform** — see the `send-all-slot-isolation` capability and §3.3.4 for
Fastly's sequential-dispatch, upload-write, and response-harvest caveats. **For buffered
fan-out, app code never calls `futures::future::join_all`** — `send_all` is it.
(Concurrent *streamed*-response requests
are outside `send_all`'s scope: an app that wants several lazy streamed bodies at once
issues individual `send(..)` calls and orchestrates them itself — that is not "app code
duplicating `send_all`", it is a different, non-buffered use case `send_all` does not cover.
The "single concurrency API" claim is scoped to buffered fan-out.)

| Adapter | `send_all` mechanism | Concurrency source |
| --- | --- | --- |
| Axum | `futures::future::join_all` of per-request `reqwest` sends | tokio reactor |
| Cloudflare | `futures::future::join_all` of `worker::Fetch` sends | Workers JS event loop |
| Spin | `futures::future::join_all` of per-request hand-built `wasi:http` sends (§4.4) | wasi async reactor |
| Fastly | dispatch every request with `send_async`, **then** harvest | Fastly host (parallel) |

**Why a batch API and not `join_all` in app code.** Axum/Cloudflare/Spin have an async
reactor, so `join_all` of independent futures fans out. Fastly Compute has no guest
reactor: a future wrapping Fastly's poll-based `PendingRequest` would return `Pending`
with no waker, and `block_on` would deadlock. Fastly fan-out therefore *must* be
structured as "dispatch all, then harvest" — a shape that cannot be decomposed into N
independent futures. Making `send_all` the one primitive hides this entirely.

**Where "identical" stops being identical: Fastly dispatch, upload, and response harvest.**
Fastly dispatches slots sequentially. A cold backend registration can block before a later
slot is issued, and a non-empty buffered request upload has no finite host-write bound. Once
responses arrive, Fastly's buffered response-body drain also runs in harvest order rather
than concurrently with sibling drains (§3.3.4 "Buffered body drain runs in harvest
order"). Small **response** bodies make only the final term negligible; they do not repair
the cold-registration or request-upload terms. For large responses on Fastly, EdgeZero has
no API that delivers concurrent large-body
fan-out — `Streamed` mode defers drain but does not let the app consume chunks
concurrently across slots either (no guest reactor; §3.2). This is a known
limitation, not a recommendation.

**Partial failure.** `send_all` returns `Vec<Result<OutboundResponse, EdgeError>>`
index-aligned with the input. A single target timing out or returning a 502 yields
`out[i] = Err(..)` or `out[i] = Ok(non-2xx)` without changing the *type* of any
other slot's result. Cross-slot **timing** is governed by `send-all-slot-isolation`
(§3.5.1 footnote 4): `Native` on Axum/CF/Spin, `BestEffort` on Fastly because sequential
dispatch, unbounded non-empty request writes, and serial response harvest can delay a
slot past the result it would have produced in isolation (§3.3.4). Apps that need the stricter
timing guarantee declare the capability required and get a hard build failure on
Fastly. The Fastly deviation covers delayed dispatch/observation from cold registration,
non-empty request upload, and serial response harvest; it is not response-body-only.

### 3.3 Portable deadline

#### 3.3.1 `Deadline` — portable value type, in core

```rust
// crates/edgezero-core/src/time.rs (new module)

/// An absolute monotonic instant after which work should stop. A pure value type
/// — arithmetic over `web_time::Instant`, identical on every target, with no
/// runtime dependency. `time.rs` contains `Deadline`, `DispatchBudget`,
/// `dispatch_budget`, and the public timing constants; the deliberate
/// constraint is that core carries **no runtime / timer / platform
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
 /// not crash the host. The internal `now + min(d, DEADLINE_FAR_FUTURE)` addition
 /// itself uses the same saturating `now.checked_add(clamped).unwrap_or(now)` form
 /// as `dispatch_budget`, so even the defensive case where the clamped add
 /// would overflow the underlying `Instant` yields an already-expired deadline
 /// (fails closed) rather than panicking.
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
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_hours(168); // 7 days; from_hours, not from_secs(7*24*60*60), which trips clippy::duration_suboptimal_units
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
    pub cause: BudgetSource,    // WHICH input set the effective deadline (for attribution)
}

/// Records which configured input selected the effective deadline. The
/// per-call `OutboundRequest::timeout` and the shared batch `deadline` are separate
/// inputs (§3.3.2 table); the effective deadline is the tighter of the two, and `cause`
/// remembers which one won. This is provenance, not the physical timer phase and not
/// proof that the named deadline itself expired.
// ONE definition, shared by `DispatchBudget` (here) and `EdgeError::GatewayTimeout`
// (§3.4.3). **Defined in `error.rs` (Phase 1a Task 1)** — NOT the `time` module — because
// `error.rs` (Task 1, committed/built first) NAMES it in `GatewayTimeout`, so it must
// exist in Task 1's deliverable or the Task-1 commit fails to build. `time.rs` (Task 2)
// and `dispatch_budget` (Phase 1b) `use crate::error::BudgetSource;`.
// DERIVES + ORDER are COMPILE-VERIFIED (a throwaway crate under `arbitrary_source_item_ordering`):
//   - `Debug` — `EdgeError` derives `Debug` and contains `cause`.
//   - `Clone`, `Copy` — budget/error carriers pass provenance by value.
//   - `PartialEq, Eq` — the Phase 1a contract tests assert `cause == Unspecified` etc.
//   - Variants are **alphabetical** — the denied `clippy::arbitrary_source_item_ordering`
//     rejects any other order (verified: the earlier `PerCallTimeout`-first order errored).
/// Which budget INPUT produced the effective deadline (the tightest bound) — the budget
/// SOURCE, NOT the physical phase-timer that fired. On Fastly the per-phase timers
/// (connect/first-byte/between-bytes) are sub-divisions of the budget; when one fires the
/// timeout is still attributed to this source (documented `BestEffort` — §3.5.2 footnote 5),
/// so `BatchDeadline` may be reported for a connect-phase-slice expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// `#[non_exhaustive]`: a future phase/reason variant must stay non-breaking (public enum).
#[non_exhaustive]
pub enum BudgetSource {
    /// The shared batch `deadline` was the tighter bound.
    BatchDeadline,
    /// Neither timeout nor deadline was set — `DEFAULT_NO_DEADLINE_BUDGET` (30 s) applies.
    Default,
    /// The per-call `OutboundRequest::timeout` was the tighter bound.
    PerCallTimeout,
    /// A timeout raised OUTSIDE a dispatch budget (e.g. a caller-supplied inner deadline
    /// in `json_bounded_until`); the default a bare `gateway_timeout(msg)` carries.
    Unspecified,
}

/// `now` is passed in (not snapshotted internally) so a single `send_all` can use
/// **one** `now` snapshot across every slot. Without that, sequential per-slot
/// `Instant::now` calls produce slightly different `duration` values for the same
/// shared `Deadline`, which on Fastly would produce different `budget_ms` values
/// and therefore different dynamic-backend identities for the same host under one
/// batch deadline. `send` (single request) just passes
/// `web_time::Instant::now`.
// PRIVACY + NAME-COLLISION CONTRACT (both verified by compiling a skeleton).
// `time.rs` is a sibling module of `outbound.rs` and cannot read `OutboundRequest`'s
// private fields directly — earlier pseudocode did, which does not compile.
// **Crucially, the accessors CANNOT be named `timeout` / `deadline` /
// `max_response_bytes`**: those names are already taken by the PUBLIC BUILDER SETTERS
// ( — `pub fn timeout(self, d: Duration) -> Self`, etc.). Rust does not overload
// inherent methods, so a same-named getter on the same type is a hard `E0592`
// duplicate-definition error. The inputs are therefore exposed through **one**
// crate-visible accessor returning a struct — no name clash with any setter:
//
// pub(crate) struct BudgetInputs {
// pub timeout: Option<Duration>,
// pub deadline: Option<Deadline>,
// pub max_response_bytes: u64,
// }
// impl OutboundRequest {
// pub(crate) fn budget_inputs(&self) -> BudgetInputs;
// }
//
// (`pub(crate)` — an internal contract between `outbound.rs` and `time.rs`, not app
// surface. Colocating `dispatch_budget` inside `outbound.rs` is rejected: `time.rs`
// must stay independently unit-testable and free of platform-shaped code.) The
// pseudocode below reads `let inputs = req.budget_inputs;` then `inputs.timeout` /
// `inputs.deadline` — **never** `req.timeout` (field) or `req.timeout` (setter).
// Lint contract (workspace denies `clippy::restriction`): public items need
// `#[inline]` (`missing_inline_in_public_items`); no single-char idents
// (`min_ident_chars`) — hence `duration`/`candidate`, never `d`; no bare arithmetic
// (`arithmetic_side_effects`) — hence `checked_add`; no `expect`/`unwrap`
// (`expect_used`/`unwrap_used`); items alphabetical (`arbitrary_source_item_ordering`).
#[inline]
pub fn dispatch_budget(
    req: &OutboundRequest,
    now: web_time::Instant,
) -> Result<DispatchBudget, EdgeError> {
    let inputs = req.budget_inputs();   // single crate-visible accessor (see contract above)

 // (1) Candidate absolute deadlines. Build candidates directly from the single `now`
 // snapshot; do NOT round-trip through `Deadline::remaining()`. An expired caller
 // deadline remains a real (past) candidate, while absence remains no candidate, so
 // "no deadline" and "expired deadline" cannot collapse into the same `None`.
 // Use checked_add throughout — a caller-
 // supplied Duration::MAX must not panic the adapter. The same clamp as
 // Deadline::after: cap the duration at DEADLINE_FAR_FUTURE
 // *before* the add, so the addition itself never overflows in practice
 // (now + 7 days is well within Instant range). checked_add on the
 // clamped value is belt-and-suspenders.
    let saturating = |duration: Duration| -> Deadline {
        let clamped = duration.min(DEADLINE_FAR_FUTURE);
        let inst = now.checked_add(clamped).unwrap_or(now);   // last-resort: now (immediate)
        Deadline::at_instant(inst)
    };
    let from_timeout      = inputs.timeout.map(&saturating);
 // `Deadline::at_instant` is public, so a caller could construct a
 // Deadline well past DEADLINE_FAR_FUTURE and bypass Deadline::after's clamp.
 // Re-clamp `from_caller` here: the caller's deadline is never honoured beyond
 // `now + DEADLINE_FAR_FUTURE`. This only tightens; a caller's deadline closer
 // than that is unaffected.
    let from_caller       = inputs.deadline.map(|d| {
        let far = now.checked_add(DEADLINE_FAR_FUTURE).unwrap_or(now);
        Deadline::at_instant(d.instant().min(far))
    });
    let from_default_only =
        (inputs.timeout.is_none() && inputs.deadline.is_none())
            .then(|| saturating(DEFAULT_NO_DEADLINE_BUDGET));

 // (2) Effective deadline = min of the candidates (always at least one).
 // NOTE: no `.expect(..)` — `clippy::expect_used` is DENIED in production code
 // (the workspace denies the whole `restriction` group; the test exemption in
 // clippy.toml does not apply here). The "unreachable by construction" case
 // becomes an explicit invariant error instead of a panic, which is also the
 // rule that adapter/core boundaries never crash the host.
 // Tag each candidate with the BudgetSource it represents, pick the tightest, and
 // CARRY the cause. On an exact-instant tie the iteration order wins — `from_timeout`
 // is first, so a per-call timeout that coincides with the batch deadline attributes
 // to `PerCallTimeout` (the more specific bound). This is the attribution §3.3 needs.
    let (cause, deadline) = [
        from_timeout.map(|d| (BudgetSource::PerCallTimeout, d)),
        from_caller.map(|d| (BudgetSource::BatchDeadline, d)),
        from_default_only.map(|d| (BudgetSource::Default, d)),
    ]
        .into_iter()
        .flatten()
        .min_by_key(|(_, d)| d.instant())
        .ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "dispatch_budget: no deadline candidate — invariant violated (adapter bug)"
            ))
        })?;

 // (3) Duration is derived from the chosen deadline and the same now snapshot
 // — never `Deadline::after(duration)`, which would re-anchor to a *later*
 // now and could extend the absolute deadline past the caller's intent.
    let duration = deadline.instant().saturating_duration_since(now);
    if duration.is_zero() {
        // `cause` was just computed above — attribute the zero-budget timeout to it.
        return Err(EdgeError::gateway_timeout_caused("effective budget is zero", cause));
    }

    Ok(DispatchBudget { duration, deadline, cause })
}
```

**Timeout provenance — the outcome carries the selected budget input, not the timer phase.**
Because `DispatchBudget` records `cause`, every timeout an adapter (or the pre-dispatch
check) raises carries the effective `budget.cause`. `BudgetSource::BatchDeadline` means
only that the shared deadline was the tighter configured bound. It does **not** prove that
the shared deadline has elapsed: Fastly may fire a rigid connect/first-byte phase timer
while that absolute deadline is still live (§4.3). It is not a retry classification,
physical timeout phase, or batch-abandonment signal. A timeout says only that EdgeZero
stopped waiting, while the origin may already have committed the effect. The provenance is
carried as a typed field on the error:
`EdgeError::GatewayTimeout { message, cause: BudgetSource }` (§3.4.3) — NOT a message string
to be parsed. A slot that times out is raised via `gateway_timeout_caused(msg,
budget.cause)`; a timeout outside a budget context uses `gateway_timeout(msg)`, whose cause
is `Unspecified`. `dispatch_budget` selects provenance before its zero-duration return, so
an already-expired caller deadline is `BatchDeadline` unless a per-call zero timeout has
the same absolute instant; the documented equality rule then selects `PerCallTimeout`.
The zero-effective-budget timeout carries that selected cause.

**This is NORMATIVE for EVERY adapter, not just Fastly.** Every request-budget timeout an
adapter raises — the `reqwest` timeout on **Axum**, the `worker::Delay` expiry on
**Cloudflare**, the raced wasi-timer on **Spin**, and the **streamed-response wrapper's**
`gateway_timeout` chunk on all four — MUST use `gateway_timeout_caused(msg, budget.cause)`,
NOT bare `gateway_timeout`. Where the mapper is a pure error classifier with no budget in
scope (Spin's `map_spin_send_err`, Fastly's `classify`), it **takes the `cause` as a
parameter** (the caller has `budget.cause`): `map_spin_send_err(err, budget.cause)` maps its
five timeout `ErrorCode` variants to `gateway_timeout_caused(.., budget.cause)`, mirroring
`classify(SendFailure, cause)`. The deadline-aware stream wrapper is constructed with
`budget.cause` so its past-deadline chunk carries it. **§5.4 asserts the attributed cause
through ACTUAL adapter results** (not just the core helper): for each of Axum/CF/Spin, a
`.timeout(short).deadline(long)` expiry yields a `PerCallTimeout`-attributed harvested error
and the mirror yields `BatchDeadline`; a no-deadline default yields `Default`. §5.4 pins a Tier 1 test:
`.timeout(short).deadline(long)` that expires yields a `PerCallTimeout`-attributed error,
and `.timeout(long).deadline(short)` yields a `BatchDeadline`-attributed one — the two are
distinguishable from the harvested result alone.

Behaviour table (the implementation gives these directly; listed here for clarity):

All `now + t` entries in this table are shorthand for `now + min(t,
DEADLINE_FAR_FUTURE)` (§3.3.1) — the clamp is universal, not a special case for
`Duration::MAX`.

Below, `clamped(d)` denotes `Deadline::at_instant(d.instant().min(now +
DEADLINE_FAR_FUTURE))` — the re-clamp of a caller's `req.deadline` performed by
`dispatch_budget` so a `Deadline::at_instant` constructed past the 7-day clamp
cannot escape the bound (§3.3.2 step 1). For brevity the table writes
`clamped(d)` rather than the full expression.

| `req.timeout` | `req.deadline` | `duration` | `deadline` (absolute) |
| --- | --- | --- | --- |
| `None` | `None` | `30 s` | `now + 30 s` |
| `Some(t)` | `None` | `min(t, DEADLINE_FAR_FUTURE)` | `now + min(t, DEADLINE_FAR_FUTURE)` |
| `None` | `Some(d)` | `clamped(d).instant() - now` | `clamped(d)` |
| `Some(t)` | `Some(d)` with `now + min(t, …) ≤ clamped(d).instant()` | `min(t, …)` | `now + min(t, …)` (tighter) — **cause `PerCallTimeout`; EQUALITY goes HERE** |
| `Some(t)` | `Some(d)` with `now + min(t, …) > clamped(d).instant()` | `clamped(d).instant() - now` | `clamped(d)` (strictly tighter) — cause `BatchDeadline` |
| any nonzero/absent timeout | expired (`d.instant() <= now`) | — | `Err(gateway_timeout)` with cause `BatchDeadline` |
| `Some(Duration::ZERO)` | `Some(d)` with `d.instant() == now` | — | `Err(gateway_timeout)` with cause `PerCallTimeout` (the equality tie rule) |
| any | selected duration ends up zero | — | `Err(gateway_timeout)` with the selected cause |
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
  `req.deadline.is_some()`. Axum and Cloudflare provide timer-backed cancellation;
  Spin races a monotonic timer but has only cooperative Component Model teardown
  (BestEffort, footnote 8); Fastly checks cooperatively between host reads. Every
  adapter surfaces deadline expiry as a typed `gateway_timeout`; the capability
  matrix states where host work can outlive that result.

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
| Cloudflare | race `Fetch::send_with_signal(&signal)` (+ body drain) against `worker::Delay::from(effective)`; on expiry `controller.abort()` (NOT a dropped future — that leaves the subrequest running) | Real, whole-operation with cancellation |
| Spin | race the entire `send_one` future (send **and** body collect) against a WASI monotonic-clock timer; drop the guest future on expiry | Guest-visible 504 at the timer; host teardown is cooperative and therefore `BestEffort` (footnote 8) |
| Fastly | host phase timers split per §4.3 (`connect = budget/4`, `first_byte = 3*budget/4`, `between_bytes = budget`); during body drain, `budget.deadline.is_expired()` is checked **after every blocking body read returns, including the EOF read** (the synthetic 30 s deadline applies when no caller deadline was set); the host between-bytes timeout bounds each gap | Warm-path connect/headers have a documented phase split and returned body reads have a cooperative bound; the end-to-end capability remains `BestEffort` because cold registration and request-write gaps are unbounded |

**Drop-cancellation guarantee, per adapter (what happens to a LOSING arm).** A fan-out
consumer under deadline pressure needs to know whether a timed-out/deadline-lost send is
actually *aborted* or merely *stopped-being-waited-on* — "harvest returns" ≠ "the pending
request is cancelled." The guarantee:

| Adapter | On deadline/timeout, the in-flight send is… | Origin observes cancel? |
| --- | --- | --- |
| **Axum** | **cancelled** — dropping the `reqwest` future cancels the request (tokio) | Yes (connection dropped) |
| **Cloudflare** | **cancelled** — `controller.abort()` (NOT a bare future-drop, which would leave the subrequest running) | Yes (§5.3 blocking test) |
| **Spin** | guest future is dropped and Component Model cancellation is requested; completion writers default to `Err` | Not guaranteed within a finite bound; host-observed tests are upgrade evidence (footnote 8) |
| **Fastly** | **NOT cancelled** — Fastly exposes no async-cancellation primitive; a dispatched `PendingRequest` is always harvested via blocking `wait()`/`poll()` — the **one** exception is the single-`send` streamed-upload budget-exhausted path (§4.3 "Streamed request bodies in single `send`"), which intentionally drops the `StreamingBody`+`PendingRequest`; the host reclaims that subrequest's resources on session teardown. The host phase-timers (connect/first-byte/between-bytes) can *fail* it, but EdgeZero cannot *abort* it, and a **sibling slot's** deadline firing never cancels another slot | **No** — this is a documented BestEffort limitation, not a bug |

Axum and Cloudflare give bounded cancellation of losing arms. Spin and Fastly expose
different BestEffort gaps: Spin requests cooperative teardown without a documented host
bound, while Fastly exposes no guest cancellation primitive for a dispatched request.

**Fastly precision, stated honestly.** Fastly has no guest wall-clock primitive to
preempt a chunk read in progress. At dispatch the adapter computes `let budget =
dispatch_budget(req, now)?` (§3.3.2, `now` snapshotted inline for single `send`,
passed in as `batch_now` for `send_all` — round 23. `DEFAULT_NO_DEADLINE_BUDGET = 30 s`
and the synthetic absolute deadline both apply when no deadline is set, identical to
every other adapter) and derives the host timeouts via the named helper:

```rust
// Lint contract: the workspace DENIES `clippy::restriction`, so `as_conversions`
// and `arithmetic_side_effects` are hard errors — no `as` casts, no bare `+`/`-`/`/`.
// Use `Duration::as_millis` (already integer-ms), saturating/checked arithmetic, and
// `u64::try_from` instead of `as u64`.
// Returns the EXACT host-timeout ms (true ceil-to-ms) — the single source used for BOTH
// the phase split (connect/first-byte/between-bytes) AND the backend identity key, so a
// cached backend's timers always match the identity it was registered under. No
// bucketing: the cache is per-session and bounded by the fan-out size, so the raw value
// is fine and keeps the deadline bound exact.
fn fastly_timeout_ms(budget: &DispatchBudget) -> u64 {
    // True ceil-to-ms — never floor a sub-ms remainder away.
    // `as_millis` floors, so add 1 when there is a sub-ms remainder.
    let nanos = budget.duration.subsec_nanos();
    let has_remainder = !nanos.is_multiple_of(1_000_000);
    let ceil_ms = budget
        .duration
        .as_millis()
        .saturating_add(u128::from(has_remainder))
        .max(1);

    // The DEADLINE_FAR_FUTURE clamp keeps this below Fastly's 2^32 ms ceiling. Clamp
    // defensively, then convert fallibly — a bug elsewhere must not crash the host,
    // and a bare `as` cast is forbidden by the lint.
    let ceiling = u128::from(u32::MAX).saturating_sub(1);
    let clamped = ceil_ms.min(ceiling);
    u64::try_from(clamped).unwrap_or(u64::from(u32::MAX).saturating_sub(1))
}

// `dispatch_budget` always takes an explicit `now`. Single `send`
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
// total_ms = ceil-to-ms(budget.duration)
// connect_ms = total_ms / 4 [floor; most connects take <100ms]
// first_byte_ms = total_ms - connect_ms [remainder; sum invariant]
// between_ms = total_ms [body-phase ceiling unchanged]
// Sub-4 ms degenerate case: both = total_ms (sum = 2*total_ms, documented).
// SSL configuration also lives on BackendBuilder: `use_ssl` defaults to false, so
// HTTPS targets MUST opt in explicitly with .enable_ssl() and configure SNI +
// certificate verification (per the existing pattern at
// crates/edgezero-adapter-fastly/src/proxy.rs:120). HTTP targets opt out via
// .disable_ssl().
//
// Four canonicalized values come from the OutboundRequest accessors ( —
// adapters MUST consume these, never re-derive from `req.uri`):
// - `req.backend_target` — connection target `"host:port"` with the
// resolved port; passed as the
// BackendBuilder's `target` arg.
// (current adapter precedent:
// `host_with_port` at
// crates/edgezero-adapter-fastly/src/proxy.rs:108)
// - `req.host_authority` — authority for `.override_host(..)`
// (carries the explicit port only when
// non-default; preserves Host
// semantics).
// - `req.sni_hostname` — `Option<&str>`. `Some(host)` for DNS-name HTTPS
// targets; `None` for IP-literal HTTPS (RFC 6066
// forbids SNI for IP literals). When `None`, the
// adapter omits `.sni_hostname(..)` entirely; it
// does NOT fall back to `req.uri.host`.
// - `req.cert_host` — `Option<&str>`. `Some(host)` for any HTTPS target
// (DNS name OR IP literal — port-stripped,
// bracket-stripped); `None` for non-HTTPS schemes.
// Passed to `.check_certificate(..)` verbatim; the
// adapter does NOT bracket-trim, parse, or
// post-process.
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
let total_ms = fastly_timeout_ms(&budget);                 // exact ceil-to-ms of budget.duration
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
// TLS handling — the accessors carry the canonicalized split. We do NOT
// inspect `req.uri` directly: `cert_host` returns `Some` iff the scheme is
// HTTPS (the adapter-local "is TLS?" question), and `sni_hostname` carries
// the DNS-vs-IP-literal distinction (`None` for IP literals per RFC 6066).
builder = match req.cert_host() {
    Some(cert) => {
 // HTTPS: always set .check_certificate(..). Pass req.cert_host
 // through unmodified — bracket-stripping for IPv6 is already done in
 // the accessor; we never call .trim_start_matches('[').
        let mut b = builder.enable_ssl().check_certificate(cert);
 // SNI: only when the accessor returns Some (DNS-name host).
 // For IP literals (`None`).sni_hostname is omitted entirely.
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
contract — "headers AND body completed within the deadline" — holds **for the response
phase**: either every read (including EOF) observed `!is_expired()`, or the slot returned
`gateway_timeout`. **The REQUEST-transmission phase is not covered by this argument** — no
Fastly timer bounds the guest-to-origin write (footnote 2), which is why Fastly's
`outbound-deadlines` is `BestEffort` rather than `Native`.

**Slot-level vs. wall-clock-observed completion.** The response-side bound above begins
only after a slot's sequential `send_async` dispatch has returned. A cold backend
registration can block before that call, and a non-empty buffered request upload has no
finite host-write bound; either can prevent later slots from being dispatched. For slots
that have reached `PendingRequest`, the Fastly host runs them in parallel and applies each
slot's configured response timeouts independently. What the guest **observes** is then gated
again by harvest order — a dispatched slot with a 50 ms effective budget sitting behind a
3 s `wait()` on slot 0 may have completed at the host at t ≈ 50 ms, but the guest does not
see the result until slot 0's `wait()` returns. So:

- **Per-slot result correctness after dispatch (headers phase):** each dispatched slot's
  connect / first-byte / between-bytes timeouts are configured from its own
  `budget.duration`, and the host enforces them independently. A 50 ms slot that fails to
  receive headers in time errors at 50 ms host-side, not 3 s. This statement does not cover
  a later slot that an earlier cold registration or upload prevented from dispatching.
  For dispatched slots it holds only for the headers phase. Buffered response-body drain is
  bounded by the same host timeouts on a per-chunk-gap basis but is **scheduled
  sequentially in harvest order** — see the next bullet for the wall-clock consequence.
- **Per-slot wall-clock-observed delivery after Phase 1:** once every surviving slot has
  reached `PendingRequest`, Phase 2 is bounded by the response-harvest terms below. There is
  deliberately no finite whole-call bound on Fastly when Phase 1 includes cold registration
  or a non-empty request write. The opportunistic `poll()` of later slots after each
  `wait()` reduces response-harvest delay in practice but does not eliminate it.
- **Buffered body drain runs in harvest order, not concurrently.** `harvest()` does
  `pending.wait()` *and then* drains the response body (Buffered mode) *and then*
  moves to the next slot. On Axum/CF/Spin `join_all` polls all `send_one` futures
  concurrently, so two slow body drains complete in parallel; on Fastly they are
  sequential. Wall-clock for **Phase 2 after every slot has dispatched** is therefore
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
    Typical small **response** bodies make this response-harvest term negligible. They do
    not make batches with cold registration or non-empty request bodies isolated.

The worst-case post-deadline overshoot per slot **once that slot is actively draining**
is therefore **one between-bytes-timeout interval, which is ≤ `effective_at_dispatch`**.
That bound is on the host timeout set at dispatch and does *not* shrink while a slot waits
behind earlier harvest work. **Phase-2 wall-clock observed by the caller** is not bounded
by one between-bytes-timeout — it also includes the sum of preceding slots' response-drain
times. Concretely, after all slots have dispatched, slot `k`'s observed response harvest can
be as late as `Σᵢ<ₖ drain_timeᵢ + (effective_at_dispatch for slot k)`. Whole-call wall-clock
also includes Phase 1 and has no finite Fastly bound when cold registration or a non-empty
upload stalls. Once slot `k`'s response drain begins, the inter-chunk
`is_expired()` check fires within one between-bytes-timeout of `budget.deadline` for that slot.

Apps reasoning about precise wall-clock should treat `effective_at_dispatch` as the
maximum per-slot *active response-drain* overshoot only. It is not a bound on request upload,
sequential dispatch, or observed completion across the whole `send_all`. The
`send-all-slot-isolation` capability
(§3.5.1 footnote 4) is what scopes the cross-slot half: declaring it required gives
the hard build failure on Fastly, signalling that an app needs isolation guarantees
the Fastly dispatch/upload/harvest sequence does not provide. The warm single-slot body-read mechanism has a
documented cooperative bound, but the static `outbound-deadlines` cell remains
`BestEffort` because cold registration and request-write paths have unbounded gaps.
The three cross-slot weaknesses are the separate `BestEffort`
`send-all-slot-isolation` story. A peer dribbling **response** bytes cannot blow past its
active-drain bound indefinitely, but a peer that stops reading a non-empty request can block
without a finite write bound and therefore delay the whole Fastly batch.

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

**Header normalization and no-content handling happen FIRST, before decode or cap logic.**
After `normalize_response_headers` has stripped hop-by-hop fields and `connection`
nominations (§3.1.4), a response that is bodyless by HTTP framing — the response to a
**`HEAD`** request, or any **`1xx`**, **`204`**, or **`304`** status — carries no payload even
though HEAD/304 MAY legitimately carry `Content-Encoding` and a *representation*
`Content-Length` (e.g. a `HEAD` echoing what a `GET` would return; a `304` echoing the cached
representation's metadata). For these:
- **Do NOT attempt to decode.** There are no body bytes; feeding EOF to the gzip/br decoder
  would error and produce a **false `bad_gateway` (502)**. Skip the decoder entirely.
- **Framing headers are status-dependent (RFC 9110 §8.6 / RFC 9112 §6.2).** For a
  **`HEAD` response and `304`**, `content-encoding` and a *representation* `content-length`
  are legitimate metadata the client needs (a `304` or `HEAD` with them stripped breaks
  cache validation) — **preserve them unchanged**. For **`1xx` and `204`**,
  `content-length` is prohibited and is removed. This framing normalization is centralized
  in the same response helper as hop-by-hop stripping so every adapter applies it
  identically.
- The adapter settles or drops the platform body handle according to that runtime's
  protocol and constructs an empty core body. It never feeds a framing-bodyless response
  to the content decoder.

**`205 Reset Content` is a separate semantic-suppression case.** RFC 9110 forbids a server
from generating content in a 205 response, but HTTP/1.1 message framing does not grant 205
the same automatic no-body precedence as HEAD/1xx/204/304. An illegal framed body therefore
cannot simply be replaced with `Body::Once(empty)` while leaving the native body unread:
that would risk connection reuse on unread bytes and, on Spin, falsely complete the
`consume_body` caller-result protocol. For 205, the adapter skips content decoding and polls
the body once under the effective deadline. Clean EOF writes/observes clean completion; any
body data frame, including an empty data frame, causes the adapter to abort/drop the remaining
native body and mark the platform completion protocol as failed. The downstream/core result
is still a 205 with an empty body, and `content-length` is normalized to `0`. A read/protocol
failure before that disposition is `bad_gateway`; deadline expiry is the attributed
`gateway_timeout`. `ResetContent { declared_body: true }` permits immediate abort without a
poll. This at-most-one-poll rule prevents an unbounded discard while ensuring an illegal 205
body is not left live; an empty data frame is not treated as proof of EOF.

The bodyless determination is **method- and status-aware**. Every adapter passes the
originating request method into `OutboundResponse::new`; `OutboundResponse` retains it until
`into_parts`/`into_response`, so a downstream conversion cannot lose HEAD semantics. The
adapter performs the authoritative disposition before constructing the response; final
conversion only rechecks the already-normalized metadata defensively. §5.4 pins tests for
`HEAD 200` (with `Content-Encoding:
gzip` + representation `Content-Length`, no body → passes through, headers preserved, no
502), `1xx`, `204`, clean 205, illegal-body 205, and `304`. Only AFTER this handling do the
decode/cap rules below apply to payload-bearing responses.

In `Buffered` mode, `max_response_bytes` (default `DEFAULT_MAX_RESPONSE_BYTES = 1 MiB`)
caps the body. The cap is measured in **decompressed, app-visible bytes**, not
compressed wire bytes. Every adapter that transparently decompresses gzip/br
**must enforce the cap incrementally during decompression** and abort as soon as the
decompressed output exceeds the cap — this closes the decompression-bomb gap so a
small compressed body cannot expand past the limit. Over-cap →
`Err(EdgeError::response_too_large("response body exceeded N bytes"))` (the distinct
kind, 502 — §3.4.1; NOT `bad_gateway`, so a consumer classifies it apart from transport).

**Early `Content-Length` rejection is sound ONLY for identity (no `content-encoding`)
responses.** When there is no decode, the wire `Content-Length` *is* the decompressed
size, so a `u64` `Content-Length` above the `u64` cap can be rejected **before buffering**
(cheap, correct). When the response **is** compressed, the wire `Content-Length` is the
*compressed* size, which bounds the decompressed size in NEITHER direction — gzip
typically expands, but incompressible input can make the compressed representation
*larger* than its decoded output — so an early reject on the wire `Content-Length` could
wrongly reject a body whose decompressed size is under the cap. For compressed responses
the cap is therefore enforced **only** incrementally during decompression (above); the
early-reject shortcut is skipped. (This corrects an earlier note that implied every wire
`Content-Length` over the cap is rejected up front.)

**Pre-append check is mandatory.** Outbound bounded drains
(`OutboundResponse::into_bytes_bounded` / `_until` and adapter buffered-response drains)
MUST check the running total against `max` **before** extending the buffer. The comparison
is done in the outbound cap's `u64` type (§3.1.3): convert `usize` lengths with
`u64::try_from`, use checked addition, compare, then extend. A single oversized chunk on a
small cap would otherwise allocate past the limit before erroring. The persistent collected
buffer therefore never exceeds `max`; inbound bounded-drain semantics are owned by the
[inbound body design](2026-08-22-inbound-body-design.md).

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
  adds copying/buffering pressure and changes observed chunk boundaries). **Deferred** —
  tracked in §8 risk 11.
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
  does **NOT** enable reqwest's `gzip`/`brotli` auto-decoding, and Axum sends
  `accept-encoding` for identity handling explicitly. reqwest's built-in decoder matches
  **exact lowercase** `content-encoding` values from a single map entry and would not
  honour the portable contract (case-insensitive `GZIP`, `identity`, unknown/**stacked**/
  repeated → passthrough untouched). Instead Axum routes the raw response body through
  the **same shared `content-encoding` inspection + decoder** the other adapters use
  (§3.4.1 policy table), enforcing the decompressed-byte cap incrementally. This is the
  only way all four adapters share one decompression contract.

**Portable `content-encoding` policy for visible values.** The action table is identical on
all four adapters for the values and field structure visible to the guest. Exact treatment
of malformed raw bytes or field lines that workerd has already joined is guaranteed only by
`outbound-header-fidelity`; Cloudflare applies the table to its post-workerd representation.

| `content-encoding` value | Action |
| --- | --- |
| absent, or `identity` | **Passthrough** — no decode; body delivered as-is. `identity` is treated exactly as absent. |
| a single `gzip` | **Decode** one gzip layer; strip `content-encoding` + `content-length`. |
| a single `br` | **Decode** one brotli layer; strip `content-encoding` + `content-length`. |
| anything else — an **unknown** token (`zstd`, `deflate`, `compress`, …) **or a stacked list** (`gzip, br`, `br, gzip`, …) | **Passthrough, untouched** — do **not** attempt to decode; deliver the raw bytes **and leave `content-encoding` / `content-length` intact** so the app can decode itself. Never a hard failure. |

- **Matching is case-insensitive** on the token (`GZIP` == `gzip`) and tolerant of
  optional surrounding whitespace. **`Content-Encoding` is a bare content-coding token
  (RFC 9110 §8.4.1) and carries NO `q=` weight** — quality values belong to
  `Accept-Encoding`, not `Content-Encoding`. A value bearing any parameter (`gzip;q=0.5`,
  `gzip;x=1`) is therefore **not** the bare `gzip`/`br` form: it falls through to
  **passthrough**, exactly like an unknown token — never decoded. Only the two known
  bare single-layer forms decode; everything else passes through.
- **A repeated `content-encoding` field** (two header lines) is treated as the stacked
  case → passthrough untouched.
- Passthrough here means the byte cap (`max_response_bytes` / decompressed-count) is
  applied to the **raw** bytes, since no decode happens.
- Rationale: decoding stacked/unknown encodings is unbounded surface for little value on
  edge fan-out; failing them hard (`502`) would break apps that can decode a `zstd` body
  themselves. Passthrough is deterministic and never worse than "the app got the bytes."

Whenever an adapter **does** decompress (the two known single-layer cases above), the
`OutboundResponse.headers` it returns MUST have
both `content-encoding` and `content-length` removed — the original values describe
compressed wire bytes and no longer match the app-visible body. This applies in both
`Buffered` and `Streamed` modes: callers must never see decoded bytes alongside stale
compressed metadata. Existing Cloudflare and Fastly proxy code already does this and
the contract codifies it.

**Streaming-decompressor design (Streamed mode).** Lazy
`lazy-streamed-response-passthrough` on **Cloudflare** (the only `Native` adapter)
coexists with the cap obligation because the adapter wraps the raw compressed byte
stream with a **streaming decoder** that emits decompressed chunks as they arrive,
never buffering the full body. (**Axum, Fastly, and Spin are all `BestEffort`** for
lazy passthrough, for three different reasons — non-Send `LocalBoxStream`
[footnote 3], `stream_to_client()` vs `#[fastly::main]` [footnote 6], and Spin's
buffered `FullBody` public response surface [footnote 7]. On all three the
streaming-decompressor wrapper still runs, but the response converter buffers
downstream of it within its adapter-level constant —
`AXUM_RESPONSE_STREAM_BUFFER_BYTES` / `FASTLY_RESPONSE_STREAM_BUFFER_BYTES` /
`SPIN_RESPONSE_STREAM_BUFFER_BYTES`, all 16 MiB.) The decoder's *only*
responsibilities are decoding bytes, stripping the two compressed-only headers, and
surfacing decoder errors — it deliberately does **not** enforce a byte cap, because
`ResponseMode::Streamed` carries no `max_bytes` (§3.1.3) and the cap lives with the
consumer:

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
- **Streamed mode + `into_response()` passthrough (proxy-forward):** uncapped **on
  Cloudflare ONLY** — the sole adapter that streams `Body::Stream` lazily to the
  downstream wire (there the wire is the budget, and inserting an EdgeZero cap on a
  transparent proxy stream would silently truncate a valid streamed response). **Axum,
  Fastly, and Spin do NOT stream lazily** — their response converters buffer `Body::Stream`
  into `Bytes` within an adapter-level 16 MiB limit (`AXUM_/FASTLY_/SPIN_RESPONSE_STREAM_BUFFER_BYTES`),
  so on those three a raw `into_response()` passthrough IS capped (over that limit →
  `response_too_large`, §3.4.1) — matching the trait rustdoc and the capability matrix
  (Cloudflare is the only `lazy-streamed-response-passthrough = Native`). Apps that want a
  smaller cap on any adapter do `into_bytes_bounded` first, then re-emit.

**Oversize is a DISTINCT outcome, not a transport error.** When a cap fires — the buffered
drain, `into_bytes_bounded[_until]`, or an adapter's response-converter fallback buffer —
the result is **`EdgeError::response_too_large(..)`**, a distinct variant/kind, **NOT**
`bad_gateway`. It maps to HTTP **502** on the wire (a downstream client has no better
status for "upstream body exceeded the configured cap"), but its **`kind_str()` is
`response_too_large`**, so a fan-out consumer can classify a cap-exceeded slot apart from a
genuine transport `bad_gateway` (DNS/TLS/connection) — the two demand different handling
(the cap is a policy decision the consumer set; the transport failure is upstream being
unreachable). This is required because collapsing both to `bad_gateway` (the earlier
draft) is exactly the conflation a fan-out consumer cannot tolerate. Enforcement stays
**incremental** — the pre-append check fires *during* the drain (and, **for an
identity/undecoded response only**, a wire `Content-Length` above the `u64` cap is
rejected before any buffering — a *compressed* wire length is not comparable to the
decoded cap, per the identity-only rule above), never a
buffer-everything-then-compare. Adding `ResponseTooLarge` to `EdgeError` follows the same
exhaustive-match discipline as the Phase 1a variants (every `match` arm updated; it carries
no `Retry-After` and no `field_path`). §5.4 pins a test: a response exceeding
`max_response_bytes` yields `response_too_large`, and a transport failure yields
`bad_gateway`, distinguishable from the harvested result alone.
- Request-body over-cap keeps its distinct existing outcome — `bad_request` (400),
  a client-side misuse — unchanged.

**Pipeline order — the decoder is inside an absolute-deadline output wrapper.** The layers
compose in exactly one order:
`platform raw stream → EdgeError/io::Error carrier bridge → gzip/br decoder → exact carrier
restoration → absolute-deadline/cancellation wrapper → decoded-byte cap or consumer`.
- Each adapter keeps whatever raw-read timer its transport provides. On Axum, Cloudflare,
  and Spin, the outer decoded-output wrapper races every `decoded.next().await` against the
  absolute deadline and re-checks the deadline after readiness, including terminal EOF and
  error. Fastly performs the same pre/post absolute-deadline checks around each blocking
  decoded read but cannot preempt that read guest-side. This covers work performed by the
  decoder after the final raw read; a converter-only check cannot cover lazy `Streamed`
  consumption.
- Its timeout chunk is a typed `EdgeError::gateway_timeout` (504). On timeout it drops the
  decoder/raw stream and invokes the adapter's available transport cancellation. Fastly has
  no guest timer, so it retains the host raw-read timeout plus cooperative pre/post checks at
  this same outer boundary.
- **Precedence at the decoded-stream boundary:** at or after the deadline, timeout wins over
  simultaneous success, EOF, raw-read failure, or decoder failure. A malformed-compression
  502 applies only when the decoder produces that error before the deadline. A cap outside
  this wrapper may legitimately win after the wrapper yielded a within-deadline chunk: the
  generic streamed helper does not retain the request deadline, so it cannot reclassify a cap
  decision merely because the clock crossed after that yield. Buffered adapter drains remain
  inside the adapter's whole-exchange race, and `into_bytes_bounded_until` rechecks its own
  caller-supplied deadline before returning over-cap; those two surfaces preserve
  timeout-over-cap precedence for the deadline they actually own.
- §5.4 tests all four: a compressed **stall before first decoded byte**, **mid-stream stall**,
  **stall at EOF** → each `gateway_timeout` (504); **malformed compression with no stall** → 502;
  and **malformed-compression-vs-timeout precedence** (deadline fires first → 504, not 502).
  Separate cap-precedence tests assert the narrower ownership rule above rather than claiming
  a universal race after a within-deadline chunk has already been yielded.

**Implementation hooks (don't rewrite what already exists).** The async stream
decoders for gzip and brotli **already live in `edgezero-core` at
`compression.rs:15` and `compression.rs:41`** — they are core helpers, not
adapter-local code. (Spin's `decompress.rs` is a separate **buffered slice**
decoder — not the async helper.) The existing helpers' chunk error type is
**`io::Error`**, and that is **not a free choice**: `TryStreamExt::into_async_read`
(which both helpers use to feed the decoder) is hard-bound to
`Self: TryStreamExt<Error = std::io::Error>` (futures-util `try_stream/mod.rs`). A
decoder input stream therefore **cannot** simply be re-typed to `EdgeError` — that does
not compile — and naively mapping `EdgeError -> io::Error` on the way in would collapse a
`gateway_timeout` (504) into the decoder's generic 502 outcome, the exact bug §3.4.1
forbids.

**The bridge (compile-verified).** Carry the typed error *through* the `io::Error`
boundary instead of converting it. On input, wrap each inner `EdgeError` in a private
carrier: `stream.map_err(|error| io::Error::other(Carried(error)))`. On output, first
capture the `io::Error` diagnostic, then inspect `into_inner()` and restore the original
`EdgeError` only when the boxed source downcasts exactly to `Carried`. A missing or failed
downcast is the decoder's own failure and maps to `EdgeError::bad_gateway(..)` while
preserving the captured diagnostic. The restored stream is then wrapped by the
decoded-output deadline guard above, so a lazy stream checks every yield and terminal EOF.

CF/Fastly/Spin response converters call
into these existing core helpers; **Axum calls into the same shared streaming
decoder — the wrapper runs incrementally there too (§3.4.1), never a non-streaming
whole-body decode.** Axum is `BestEffort` for lazy passthrough only because its
response converter re-collects the already-decompressed chunks into `Bytes` at the
`axum::body::Body::from_stream` (`Send + 'static`) boundary within
`AXUM_RESPONSE_STREAM_BUFFER_BYTES`; the decoder itself never buffers the whole body,
and `Streamed` mode never collects except at that final conversion step (§4.1).

In `Streamed` mode no cap is pre-enforced; the caller applies one via
`OutboundResponse::into_bytes_bounded(max)`. That method does **not** delegate to
`Body::into_bytes_bounded` directly — `Body::into_bytes_bounded` maps over-limit to
`bad_request` (400), correct for the inbound body case but wrong for an over-large
upstream response. `OutboundResponse::into_bytes_bounded` performs its own bounded
drain and maps over-cap to **`response_too_large`** (distinct kind, 502 — §3.4.1; NOT
`bad_gateway`, so a consumer classifies it apart from transport). On adapters that
decompress, the cap is enforced against decompressed output here too.

#### 3.4.2 Inbound dependency boundary

Inbound `RequestContext::into_request` semantics are owned by the dedicated
[inbound body design](2026-08-22-inbound-body-design.md). This outbound spec requires only
that `OutboundRequest::from_request` preserve the source method, normalized headers, and
whatever buffered or streamed `Body` the core request supplies; it does not deliver or test
the inbound body state machine, extractor limits, or adapter ingress buffering.
#### 3.4.3 New `EdgeError` variants & mapping

`EdgeError` is `#[non_exhaustive]`, so this is additive.

```rust
// crates/edgezero-core/src/error.rs
// Phase 1a lands the first TWO variants (needed for deadline/transport mapping).
EdgeError::BadGateway { message: String }        // -> 502  (Phase 1a)
// GatewayTimeout carries a TYPED `cause`, NOT just a message: consumers can observe
// which configured budget input selected the effective deadline without parsing strings
// (§3.3.2). This does not say which physical timer fired or prove that input elapsed.
// **Phase 1a MUST land this shape** (the `BudgetSource` enum +
// the field), even though its producer `dispatch_budget` is Phase 1b — freezing
// `GatewayTimeout { message }` now would bake in a variant the master contract can't use,
// forcing a breaking change later. `inner()` is still `None` (a `cause` is not a source error).
EdgeError::GatewayTimeout { message: String, cause: BudgetSource }  // -> 504  (Phase 1a)
// The OUTBOUND response-handling phase adds a THIRD, so response-cap over-run is a
// distinct machine-classifiable outcome, NOT collapsed into `bad_gateway` (§3.4.1):
EdgeError::ResponseTooLarge { message: String }  // -> 502, kind "response_too_large"

// Defined ONCE in `error.rs` (this file's crate, Phase 1a Task 1); §3.3.2's `DispatchBudget`
// uses it from here. `dispatch_budget` sets `PerCallTimeout`/`BatchDeadline`/`Default`;
// `Unspecified` is what `gateway_timeout(msg)` carries OUTSIDE a budget context.
// Derives + ALPHABETICAL order are COMPILE-VERIFIED (§3.3.2): `Clone`/`Copy` support
// passing provenance by value; alphabetical order satisfies the denied
// `arbitrary_source_item_ordering`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BudgetSource { BatchDeadline, Default, PerCallTimeout, Unspecified }

pub fn bad_gateway(message: impl Into<String>) -> Self;
pub fn gateway_timeout(message: impl Into<String>) -> Self;              // cause = Unspecified
pub fn gateway_timeout_caused(message: impl Into<String>, cause: BudgetSource) -> Self;
pub fn response_too_large(message: impl Into<String>) -> Self;   // outbound phase
```

`EdgeError::status()` gains `BadGateway => 502`, `GatewayTimeout => 504`, and (outbound
phase) `ResponseTooLarge => 502` with `kind_str() == "response_too_large"`. Like the other
two it carries no `Retry-After` and no `field_path`. Each addition follows the same
exhaustive-match discipline (every `match` arm + test matrix updated, alphabetical
insertion); Phase 1a does the first two, the outbound work does the third in the same
mechanical style.

| Condition | `EdgeError` | HTTP status |
| --- | --- | --- |
| Invalid outbound URI (relative / no authority / bad scheme) | `bad_request` | 400 |
| Outbound transport failure (DNS / TLS / connect) | `bad_gateway` | 502 |
| Outbound response over `max_response_bytes` (decompressed) | `response_too_large` (distinct kind — §3.4.1) | 502 |
| Outbound response body not valid JSON / `json::<T>` called on a streamed body | `bad_gateway` | 502 |
| Outbound per-request timeout or batch deadline exceeded | `gateway_timeout` (carries `budget.cause`: per-call vs batch — §3.3.2) | 504 |
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

worst-case CORE-OWNED PAYLOAD bytes   =  persistent + transient
// This is a LOGICAL payload bound, NOT true process RSS. It counts the bytes EdgeZero
// core deliberately holds; it EXCLUDES: `Vec`/`BytesMut` spare capacity (amortised growth
// over-allocates), shared `Bytes` backing allocations not yet freed, gzip/brotli decoder
// working state, and allocator overhead/fragmentation. Actual RSS is this plus those
// core-owned-but-unmodelled terms plus any adapter/host buffering (§ send_all rustdoc).

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
  by `max_response_bytes`. *Transient* worst-case core-owned payload during a drain is
  `max_response_bytes + sizeof(current_chunk)`, where `sizeof(current_chunk)` is
  source-controlled (§3.4.1). The post-check buffer never exceeds `max_response_bytes`.
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

### 3.5 Capability declaration

#### 3.5.1 Manifest section

```toml
# edgezero.toml
[capabilities]
required = ["outbound-http", "outbound-deadlines"]

[capabilities.outbound]
# Optional plumbing. OMITTING this field is NOT the same as `["*"]`:
#   - field absent      → https-only default `["https://*:*"]` (no cleartext)
#   - hosts = ["*"]     → explicit opt-in to BOTH http and https
# So an existing manifest that never declared hosts keeps its https-only posture.
hosts = ["*"]
```

```rust
// crates/edgezero-core/src/manifest.rs — defined INLINE here, NOT in a separate
// capability.rs. `manifest.rs` is textually `include!`d by edgezero-macros
// (manifest_definitions.rs), and edgezero-core depends on edgezero-macros, so the
// macro crate can neither see `edgezero_core::` paths nor add core as a dep (cycle).
// A separate `capability.rs` that `manifest.rs` imports would fail to compile in the
// macro crate. Core re-exports these: `pub use manifest::{Capability, CapabilitySupport};`.
//
// MUST derive Serialize as well as Deserialize: `Manifest` derives `Serialize` and
// `app!` calls `serde_json::to_string(&manifest)` — a Deserialize-only capability type
// breaks the `Manifest` derive. (Both facts verified against the current tree.)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive] // a future capability must not break out-of-tree adapters
// Declaration order follows the repository's alphabetical-item lint. The matrix remains
// in product order; serde names and as_str() make declaration order unobservable.
pub enum Capability {
    LazyStreamedResponsePassthrough, // downstream response chunks flow without
                                     // collecting the whole body. Cloudflare is
                                     // Native; Axum/Fastly/Spin are BestEffort.
    OutboundDeadlines,               // one exchange budget: connect, headers,
                                     // buffered body, and streamed body yields.
                                     // Cross-slot harvest delay is owned by
                                     // SendAllSlotIsolation.
    OutboundFlexiblePhaseBudget,     // the total budget is one elastic pool rather
                                     // than a rigid provider-specific phase split.
    OutboundHeaderFidelity,          // raw response-header octets and original
                                     // field-line boundaries are available for
                                     // security-sensitive normalization.
    OutboundHttp,                    // can issue outbound HTTP at all
    SendAllSlotIsolation,            // sibling timing cannot change the result a slot
                                     // would have produced in isolation.
    StreamedUploadDeadlines,         // can preempt a stalled request-body source/write;
                                     // Fastly and Spin are BestEffort.
}

impl Capability {
    pub fn as_str(&self) -> &'static str;   // kebab-case, for messages
}

// Also inline in manifest.rs (see Capability note above). `Serialize` is needed only if
// it ever appears in a serialized manifest field; it does not today (support is computed
// per-adapter via `Adapter::capability`, not stored), so `Deserialize`/`Serialize` are
// omitted here. If a future manifest field carries a `CapabilitySupport`, add both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapabilitySupport {
 /// Available but with a documented limitation that the matrix footnotes
 /// describe. The limitation can be timing-related (unbounded cooperative
 /// enforcement, e.g. Fastly source-stream-stall in
 /// `streamed-upload-deadlines`) **or functional** (deterministic behaviour
 /// differs from `Native`, e.g. Axum `lazy-streamed-response-passthrough`
 /// buffers rather than streaming). `BestEffort` therefore means
 /// "supported, with a real-world deviation you need to read the footnote
 /// to understand" — not specifically "unbounded cooperative timing."
    BestEffort,
 /// Real enforcement with a precisely documented, deterministic bound on any
 /// deviation. No current outbound matrix cell uses this level; it remains in
 /// the support ladder for future capabilities with a true end-to-end bound.
    BoundedCooperative,
 /// Fully supported with no documented caveats.
    Native,
 /// Not available.
    Unsupported,
}
```

The capability is named **`outbound-deadlines`**, not `timers`, and describes the
wall-clock budget contract for one outbound HTTP exchange. The matrix support level says
whether that contract is Native or has a documented BestEffort gap. It makes no claim
about timing arbitrary guest computation (which EdgeZero does not offer — §3.3.5).

```rust
// crates/edgezero-core/src/manifest.rs — new field on Manifest.
//
// MUST derive Serialize as well as Deserialize: `Manifest` derives `Serialize`
// (manifest.rs) and `app!` calls `serde_json::to_string(&manifest)`. A
// Deserialize-only member breaks `Manifest`'s derive and therefore edgezero-core
// AND the macro crate (which textually `include!`s this file). Same for every
// nested type and for `Capability` itself.
// `deny_unknown_fields` is REQUIRED — without it a typo like `require = [..]` or
// `host = [..]` is silently ignored, disabling enforcement or invoking the broad default
// (fail-open). The `#[validate(custom = ..)]` attaches the disjoint/duplicate check.
// `#[non_exhaustive]` matches the existing manifest-struct precedent and keeps future field additions
// non-breaking for out-of-tree code and composes fine with `Default` + `Deserialize`
// (these are built by deserialization / `..Default::default()`, never external literals).
#[derive(Debug, Default, Serialize, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
#[validate(schema(function = "validate_capabilities_disjoint"))]
pub struct ManifestCapabilities {
    #[serde(default)]
    pub required: Vec<Capability>,
    #[serde(default)]
    pub optional: Vec<Capability>,
    #[serde(default)]
    #[validate(nested)]
    pub outbound: ManifestOutboundCapability,
}
// `validate_capabilities_disjoint(&ManifestCapabilities) -> Result<(), ValidationError>`
// rejects a capability DUPLICATED within `required` or `optional`, or listed in BOTH
// (required ∩ optional must be empty — asking for a capability as both is a
// contradiction). Every manifest parse path rejects these failures; baked input becomes
// `Malformed`. §5.4 tests: an unknown field (typo `require`), a duplicate, and a
// required∩optional overlap each fail.

#[derive(Debug, Default, Serialize, Deserialize, Validate)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ManifestOutboundCapability {
 /// Outbound host plumbing.
 ///
 /// **`Option<Vec<String>>`, NOT `Vec<String>` — this is security-relevant.**
 /// The renderer defines an explicit `"*"` as **http + https**, while an
 /// **absent** field must preserve today's **https-only** default
 /// (`["https://*:*"]`). A bare `Vec` with `#[serde(default)] = ["*"]` collapses
 /// those two cases, so **every existing manifest that never declared hosts would
 /// silently gain cleartext (http) outbound permission** on its next build. The
 /// `Option` keeps them distinct:
 /// - field absent → `None` → render `["https://*:*"]` (unchanged)
 /// - explicit `hosts = ["*"]`→ `Some(["*"])` → render http + https (opt-in)
 /// - explicit `hosts = []` → `Some([])` → rejected by `length(min = 1)`
 ///
 /// Validation applies only when present: `length(min = 1)` enforces at least one
 /// entry, and `validate_outbound_hosts` (below) checks each entry against
 ///'s accepted forms (wildcard, scheme-prefixed, host:port, bare host,
 /// wildcard subdomain). `#[validate(nested)]`-style option handling: the custom
 /// validator is a no-op for `None`.
    #[serde(default)]
    #[validate(length(min = 1), custom(function = "validate_outbound_hosts"))]
    pub hosts: Option<Vec<String>>,
}

/// Per-entry validation for `[capabilities.outbound].hosts`. This is
/// **host-authority-only plumbing**, not a URI field — the same rationale as
/// `OutboundRequest`'s userinfo rejection: credentials must not leak
/// through the manifest into `allowed_outbound_hosts`).
///
/// Each entry MUST be one of:
/// - `"*"` (the wildcard).
/// - `scheme://host[:port]` where:
/// - `scheme ∈ {http, https}`, case-**insensitive** at the validator
/// (RFC 3986) — `HTTPS`, `https`, `Https` all accepted. The
/// Spin renderer then canonicalizes to lowercase before emitting
/// `spin.toml`, so the rendered manifest carries one canonical
/// spelling. Other schemes → rejected at the validator.
/// - `host` is a DNS label, IPv4 literal, IPv6 literal in brackets, or
/// `*` / `*.domain.tld` wildcard form.
/// - `port`, if present, is a decimal integer in `1..=65535` **or the literal
/// `*` (wildcard port = any port)**. The wildcard port is what lets a manifest
/// express "**HTTPS to any host on any port**" as `https://*:*` while still
/// scoping HTTP narrowly (`http://api.internal:8080`) — a distinction the bare
/// `"*"` (which grants http+https on any host/port) cannot make. `:*` is
/// therefore VALID; only a bare numeric out of `1..=65535` rejects.
/// - **NO userinfo, NO path, NO query, NO fragment.** `https://user:pass@x`,
/// `https://x/p`, `https://x?q`, `https://x#f` all reject.
/// - `host[:port]` (no scheme) — same host/port rules as above.
///
/// Empty entries, schemes other than `http`/`https`, ports outside
/// `1..=65535` or non-numeric, any userinfo / path / query / fragment, and
/// malformed authorities the hand-rolled splitter rejects (NOT `http::Uri` —
/// the macro-crate constraint below forbids that dep) all yield a `ValidationError`. `"*"`
/// mixed with specific hosts is allowed; the wildcard renders both schemes
/// and specific hosts render alongside.
///
/// **Grammar (this is a security-relevant splitter — spell it out, don't hand-wave):**
/// `entry := "*" | [scheme "://"] authority`; `scheme := "http" | "https"` (ASCII,
/// case-insensitive); `authority := hostpat [":" port]`; `hostpat := "*" |
/// "*." label ("." label)* | label ("." label)* | "[" ipv6 "]"`. A **`label`** is a
/// non-empty LDH DNS label: 1..=63 chars of ASCII letter/digit/hyphen, **no leading or
/// trailing hyphen, no underscore, no empty label** (so `-x.com`, `x-.com`, `x..com`
/// (empty middle label), `x_y.com` all reject; full name ≤ 253 chars). **`ipv6`** is a
/// standard RFC 4291 address parsed by `std::net::Ipv6Addr::from_str` on the
/// bracket-stripped inner text — anything it rejects (`[::g]`, `[:::1]`, `[12345::]`,
/// `[1:2:3:4:5:6:7:8:9]`) rejects here. `port := "*" | 1..=65535`. **ASCII/IDNA policy:** hostnames are **ASCII-only** — a non-ASCII byte
/// or a raw Unicode label is REJECTED (callers must pre-encode to punycode `xn--`);
/// this splitter does not perform IDNA itself.
///
/// has a Tier 1 test row exercising every accept AND reject case. Rejects:
/// empty string, bad scheme (`ftp://x`), missing authority (`https://`),
/// userinfo (`https://u:p@x`), path (`https://x/p`), query (`https://x?q`),
/// fragment (`https://x#f`), out-of-range port (`https://x:0`, `https://x:70000`),
/// non-numeric port (`https://x:abc`), **empty port (`https://x:`)**,
/// **malformed brackets (`https://[::1` unclosed, `https://::1]` no open bracket)**,
/// **unbracketed IPv6 (`https://::1` — colons ambiguous with the port sep)**,
/// **invalid wildcard placement (`ex*ample.com`, `*.*.com`, `a.*.com`, `**.com`)**,
/// **internal/leading/trailing whitespace (`https:// x`, `x .com`, ` x.com`, `x.com `)**,
/// **trailing dot (`x.com.`)**, **non-ASCII / raw-Unicode host (`ex€ample.com`, `café.com`)**.
/// Accepts: wildcard (`*`), wildcard subdomain (`*.example.com`), bare host with port
/// (`x:8443`), bracketed IPv6 (`https://[::1]`), IPv4 (`https://127.0.0.1`),
/// punycode (`xn--caf-dma.com`), and mixed `"*"` + host.
// Takes the INNER Vec — `validator` applies a custom function on `Option<T>` to the
// contained value only, so `None` (field absent → https-only default) is a
// no-op and never fails validation. Signature matches the `Option<Vec<String>>` field.
fn validate_outbound_hosts(hosts: &[String]) -> Result<(), ValidationError>;

// SHARED CANONICALIZER — one parser, three consumers, ATOMIC entries. `validate_outbound_hosts`
// returns `()` (validator contract), but platform-manifest rendering and the
// build/serve/deploy DRIFT check both need the *canonical* form to compare — and they
// MUST NOT re-implement parsing (divergence = a manifest that validates but drifts, or
// drifts spuriously). So both delegate to one function that expands each manifest entry
// into a SET of ATOMIC `(scheme, host, port)` triples — because ONE manifest entry can
// render as MULTIPLE `spin.toml` entries (`"*"` = http AND https → two lines), a
// canonicalizer that returned a single multi-scheme value could never set-equal the two
// rendered lines. Atomic-and-flatten fixes that:
// PUBLIC + cross-crate: the consumers (platform-manifest generation and Spin drift validation
// in the adapter crate) are DIFFERENT crates, so the fn and ALL its types MUST be `pub` and
// exported from `edgezero-core` (a private `fn`/type would not compile at those call sites).
// Concretely:
//   - The error is a DEDICATED `pub enum HostParseError` — NOT the validator's
//     `ValidationError` (that would leak validator internals into the adapter crate, which
//     doesn't depend on `validator`). The MANIFEST validator is a thin wrapper that calls
//     `canonicalize_outbound_host` and maps `HostParseError -> ValidationError` at the
//     validator boundary only; rendering/drift get the `HostParseError` directly.
//   - `AtomicHost` and ALL its component types are public: `pub struct AtomicHost` with
//     `pub scheme: Scheme`, `pub host: HostPat`, `pub port: Port`, and `pub enum Scheme`,
//     `pub enum HostPat` (`Any` | `Exact(String)` | `WildcardSubdomain(String)`),
//     `pub enum Port` (`Any` | `Exact(u16)`). `HostPat` is part of the surface (an earlier
//     draft omitted it). Derive `Hash, Eq, PartialEq` for the drift `HashSet`.
//   - Platform-manifest generation must not hand-inspect internals to build `spin.toml`, so `AtomicHost`
//     exposes a canonical rendering method: `pub fn render_spin_host(&self) -> String`.
//     **It OMITS a scheme-default exact port** (443 for `Https`, 80 for `Http`) so the
//     output is deterministic: `https://x` and `https://x:443` both canonicalize to
//     `{Https, Exact("x"), Exact(443)}` and BOTH render as **`"https://x"`** (NOT
//     `"https://x:443"` — explicitness is already lost at canonicalization, so the renderer
//     must not re-introduce a port that would then mismatch the manifest's `https://x`
//     form). A non-default port renders explicitly: `{Https, Exact("x"), Exact(8443)}` ->
//     `"https://x:8443"`. `Port::Any` renders `":*"`; `HostPat::Any` renders `"*"` as
//     the HOST COMPONENT while retaining the atomic's concrete scheme and port. Therefore
//     `{Https, Any, Any}` renders exactly `"https://*:*"`, NEVER bare `"*"` (which Spin
//     interprets as both http and https and would widen the https-only default).
//     `WildcardSubdomain("example.com")` renders the host component `"*.example.com"`.
//     Drift uses `Eq`/`Hash` on the atomics; rendering uses `render_spin_host`. Neither
//     re-parses. The input shorthand `"*"` expands to two atomics and consequently renders
//     as two explicit lines: `"http://*:*"` and `"https://*:*"`.
// Keep it dependency-free (no `http::Uri`, per the macro-crate constraint below).
//   pub fn canonicalize_outbound_host(entry: &str) -> Result<Vec<AtomicHost>, HostParseError>;
//   pub struct AtomicHost { pub scheme: Scheme /* Http | Https, CONCRETE — never "both" */,
//                       pub host: HostPat, pub port: Port /* Any | Exact(u16) */ }
// `"*"` -> [ {Http, *, *}, {Https, *, *} ] (two); `https://x` -> [ {Https, x, Exact(443)} ]
// (one); `https://x:*` -> [ {Https, x, Wildcard} ]. Both the manifest hosts AND the parsed
// `spin.toml` `allowed_outbound_hosts` are flattened through this into a `HashSet<AtomicHost>`,
// and DRIFT compares the two SETS. So `https://x:443` == `https://x`, `HTTPS`==`https`,
// `:*` == any-port, list order is irrelevant, AND `"*"`/`:*` round-trip
// (manifest one-entry -> two atomics == spin.toml two-lines -> two atomics). §5.4 pins a
// render-then-validate round-trip for BOTH `"*"` and `:*` (the generated values must
// report NO drift).
//
// DEPENDENCY CONSTRAINT: `canonicalize_outbound_host` lives in `manifest.rs`, which is
// **textually `include!`d into `edgezero-macros`** — a crate with **no `http` dependency**.
// So the canonicalizer/`validate_outbound_hosts` MUST be **dependency-free**: parse the
// authority with a small hand-rolled splitter (scheme `://` split, rsplit host:port,
// bracket-strip IPv6), **NOT** `http::Uri`. Using `http::Uri` here would force adding
// `http` to `edgezero-macros`'s `Cargo.toml` (and the include! then drags it into every
// build of the macro crate). Either keep it dependency-free (preferred) or the §7
// inventory must add `http` to `crates/edgezero-macros/Cargo.toml`. Preferred: no dep.

// Manifest gains: #[serde(default)] #[validate(nested)]
// pub capabilities: ManifestCapabilities,
//
// AND the TOP-LEVEL `Manifest` struct itself gains `#[serde(deny_unknown_fields)]`.
// Without it the strictness above is defeated one level up: `deny_unknown_fields` on
// `ManifestCapabilities` only catches a bad key INSIDE a correctly-spelled
// `[capabilities]` table. A misspelled SECTION — `[capabilites]`, `[capability]`,
// `[Capabilities]` — is an unknown top-level field, silently dropped, leaving
// `capabilities` at its `Default` (empty required+optional). That is fail-OPEN: the
// app declares a contract, the contract vanishes, and `ensure_capabilities` waves it
// through because `caps.required` is empty.
// #[serde(deny_unknown_fields)]
// pub struct Manifest { .. }
```

Every capability field is `#[serde(default)]`, so **schema-conforming** manifests parse
unchanged. This is deliberately narrower than "all existing manifests": the top-level
`#[serde(deny_unknown_fields)]` (above) is an intentional behaviour change — a manifest
carrying an **unknown top-level section** (a custom/misspelled `[...]` table that older
builds silently ignored) now **fails to parse**. That break is the fail-closed direction
and is inventoried here; any repo relying on unknown top-level sections for custom
metadata must move them under a modelled section.

**Top-level strictness is safe here** — `Manifest` already models every documented
section (`adapters`, `app`, `environment`, `logging`, `stores`, `triggers`), so
`deny_unknown_fields` rejects only genuinely-unknown sections, not valid ones. It is a
deliberate behaviour change: a stray/misspelled top-level section becomes a **hard parse
error** instead of a silent drop, which is the fail-closed direction for a
security-relevant contract. (`#[serde(skip)]` internals are unaffected — they are never
read from input.) §5.4 pins this with regression rows: a manifest whose only capability
declaration is under `[capabilites]` (transposed) must **fail**, not parse to an empty
contract; likewise `[capability]` and a stray unknown section.

**Top-level `deny_unknown_fields` is necessary but NOT sufficient — a `[capabilities]` table
misplaced at ANY depth would be silently dropped.** Per-struct `deny_unknown_fields` only
catches ONE level (`[app.capabilities]`), and chasing every level is whack-a-mole:
`[triggers.http.capabilities]`, `[environment.variables.capabilities]`,
`[adapters.axum.build.capabilities]` are two levels down, and `ManifestAdapter` can't even
take `deny_unknown_fields` (its `#[serde(flatten)]` legacy map is mutually exclusive with
it). **So the PRIMARY defense is a depth-independent reserved-key scan, not per-struct
strictness.** Before deserializing into `Manifest`, parse the input into a `toml::Value`
(or `serde_json::Value`) and **recursively reject any table key named `capabilities` that is
not the single top-level `[capabilities]`** — a `reject_misplaced_capabilities(&Value)`
walk that errors on `capabilities` found under any other table at any nesting. This catches
`[app.capabilities]`, `[triggers.http.capabilities]`, `[environment.variables.capabilities]`,
`[adapters.axum.build.capabilities]`, and anything future, in one place, without touching
every struct and without the `flatten` conflict. (Top-level `deny_unknown_fields` still
guards misspelled top-level *sections* like `[capabilites]`; the two are complementary — the
scan owns the depth problem.) §5.4 adds rows: a `capabilities` table nested under `app`,
`triggers.http`, `environment.variables`, and `adapters.axum.build` must EACH **fail to
parse**, not silently drop the block and run with an empty contract.

#### 3.5.2 Adapter capability metadata

The `Capability` / `CapabilitySupport` enums are `#[non_exhaustive]` (a future capability
must not force every out-of-tree adapter to recompile-or-break), and `Adapter::capability`
carries a **default returning `CapabilitySupport::Unsupported`** for any capability an
adapter doesn't recognize — so an out-of-tree adapter compiled against an older core still
builds, and an unknown capability fails closed (Unsupported → a `required` mismatch
hard-fails) rather than failing to compile. The registry `Adapter` trait gains one method
(`capability`). This outbound spec does not change or depend on the trait's store/config
lifecycle methods:

```rust
// crates/edgezero-adapter/src/registry.rs — current (post-#269) shape
pub trait Adapter: Sync + Send {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String>;
    fn name(&self) -> &'static str;
    // Added by this spec. MUST carry a default body: without one, every existing
    // (and every out-of-tree) adapter compiled against an older core would fail to
    // build, and the intended "unknown capability ⇒ unsupported" contract would not
    // hold. The default returns `Unsupported`; in-tree adapters override it.
    fn capability(&self, _capability: Capability) -> CapabilitySupport {
        CapabilitySupport::Unsupported
    }
    // NOTE for in-tree overrides: an adapter that overrides `capability` REPLACES this
    // default — the default does not run for capabilities the override's `match` doesn't
    // name. `Capability` is `#[non_exhaustive]`, so every in-tree `match capability { .. }`
    // MUST end with `_ => CapabilitySupport::Unsupported`, or a capability added later
    // reads as some accidental value instead of the intended fail-closed `Unsupported`.

 // Existing non-outbound methods are elided. `ensure_capabilities` consults only
 // `capability(..)`.
}
```

This reference is intentionally partial. Existing non-outbound trait methods retain
their current ownership and behavior.

Capability matrix (all four adapters):

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `outbound-http` | Native | Native | Native | Native |
| `outbound-header-fidelity` | Native | BestEffort⁸ | Native | Native |
| `outbound-deadlines` | Native | Native | BestEffort¹ | BestEffort⁸ |
| `outbound-flexible-phase-budget` | Native | Native | BestEffort⁵ | Native |
| `send-all-slot-isolation` | Native | Native | BestEffort⁴ | Native |
| `streamed-upload-deadlines` | Native | Native | BestEffort² | BestEffort⁸ |
| `lazy-streamed-response-passthrough` | BestEffort³ | Native | BestEffort⁶ | BestEffort⁷ |

¹ **Fastly `outbound-deadlines` is `BestEffort`, because it cannot be guaranteed on
every request.** Even with an already-registered/cached backend, any non-empty request body
has an unbounded guest-to-origin host-write interval: Fastly exposes no upload-write timer.
For a zero-length request body on the warm path, the dispatch/headers and response-read
portions have the documented deterministic overshoot bounds below (common-case
`total_ms ≥ 4` phase split; the sub-4 ms branch adds `total_ms`, see §4.3 "Net guarantee").
Separately, the **FIRST request to a new host** calls `Backend::builder(..).finish()`, a synchronous host
call that can block waiting for a service-wide dynamic-backend slot. Nothing guest-side
can preempt it, so it may overshoot the deadline *before* the `BATCH_DISPATCH_SLACK_MAX`
check (which runs immediately before `send_async`, i.e. after `finish()` has returned)
ever executes; on that path the guard checks the **absolute deadline FIRST** and returns an
**attributed `gateway_timeout` (504)** — a `finish()` that returns past the deadline is a
genuine expiry, not an EdgeZero bug (`internal` is reserved for slack exceeded *while time
remains*, §4.3). Either way the wall-clock overshoot happened, so the capability is still
`BestEffort`. Because `capability()` is a **static** value
that cannot distinguish cached from cold, the honest declaration is **`BestEffort`** — a
`required outbound-deadlines` therefore hard-fails on Fastly rather than passing a gate it
cannot actually honour for cold registration or body-bearing writes. Apps that accept
those gaps declare it **`optional`** (logged, never gated) and get the documented partial
bounds below.

**These dispatch/headers and response-read bounds hold once the backend is registered
(cached); they do not bound request-body transmission.** On the
**first** request to a new host, `Backend::builder(..).finish()` — a
synchronous host call that can block waiting for a service-wide dynamic-backend slot —
may overshoot *before* the `BATCH_DISPATCH_SLACK_MAX` check (which runs immediately before
`send_async`, i.e. after `finish()` has returned) executes. On that path the guard checks
the deadline first and returns an **attributed `gateway_timeout` (504)** for the actual
expiry (not `internal`); the wall-clock was still overshot, which is exactly why the
capability is `BestEffort` and not `BoundedCooperative` — see §4.3
*Honesty note — what the guard can and
cannot do*.
- **Single `send`** — `now` is snapshotted inline so there is no batch drift,
  but the **same `BATCH_DISPATCH_SLACK_MAX` guard** applies to the gap between
  `dispatch_budget(req, now)` and `send_async` (backend lookup, possible
  `Backend::builder().finish()`, SDK request construction; see §4.3). Worst-case
  dispatch+headers overshoot is `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (the
  same bound as `send_all`); the window is typically narrower because there's
  no per-slot harvest loop. Response-body phase overshoot ≤ one between-bytes-timeout
  interval (§3.3.4). For a non-empty buffered or streamed request body, the upload write
  remains unbounded and sits outside these finite terms. **Streamed-upload-specific
  post-upload overshoot**: when the request
  body is `Body::Stream` and the upload drain leaves a tiny positive
  `budget.deadline.remaining()`, the post-upload headers wait can additionally
  cost up to one dispatch-time `first_byte_ms` interval before the cooperative
  check at the `wait()` boundary or the response-wrapper preemption fires
  (§4.3 "Response phase"). That overshoot is **one-shot**, not per-chunk —
  the response wrapper preempts at the first post-deadline read.
- **`send_all`** — `batch_now` is shared across slots so the measured setup and
  host-timer portions of dispatch+headers carry
  `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (≈ 26 ms when `total_ms ≥ 4`, §4.3
  "Dispatch-overhead slack"); a non-empty buffered body still has the unbounded host-write
  gap before those response timers can establish completion. Response-body phase **once a slot is actively
  draining** is still ≤ one between-bytes-timeout — but the slot's **observed
  completion** can additionally be delayed by the harvest-order serialization
  (preceding slots' drain times). The harvest delay is what the separate
  `send-all-slot-isolation` capability owns (footnote 4); the
  `outbound-deadlines` bound here is on the active-drain phase only, not on
  total observed wall-clock across the batch.

The finite terms above are hard adapter constants, not "scales with preflight"; the
request-write and cold-registration gaps are explicitly unbounded. `Native` is reserved
for adapters with no such caveat — this rubric lets future adapters be judged consistently
without quiet downgrading. A new adapter unable to honour a capability declares
`Unsupported` and is caught at build time. The `send_all` *buffered-body* cross-slot
caveat (harvest-order false 504s) is **not** within this capability — that one is
`send-all-slot-isolation` (footnote 4), so each label means exactly one thing.

² Fastly has no guest primitive to preempt a stalled `stream.next().await` while feeding
a streamed REQUEST body via `send_async_streaming` (§4.3). **Both phases are unbounded
on Fastly:** (a) the *source pull* — a source stream that never yields the next chunk
cannot be preempted; and (b) the *host write* — `between_bytes_timeout` is documented as
**receive-side only** (it bounds the gap between bytes *received from origin*) and does
**not** bound guest-to-origin writes, so it gives no inter-chunk guarantee on the upload
path. (An earlier draft of this footnote claimed `between-bytes-timeout` still bounds
upload inter-chunk gaps — that is wrong; §4.3 and §8 risk 7 are correct.) The only
adapter-side bound is the cooperative `budget.deadline.is_expired()` check **between**
chunks. This is `BestEffort` — no documented preemption bound — and is exposed as the
separate
`streamed-upload-deadlines` capability so apps that need real-time enforcement on this
specific path declare it required and get a hard build failure on every BestEffort target,
including Fastly and Spin, per §3.5.3.
Apps that buffer their request bodies before calling `send` are unaffected **on the
source-pull axis only** — buffered uploads use `Body::Once` with no `stream.next().await`.
They are **NOT** exempt on the **host-write** axis: Fastly's `connect_timeout` ends at TLS
setup and `first_byte_timeout` starts at *"request sent"*, so the interval in which the
host pushes the buffered body to the origin is bounded by **no** Fastly timer, and
`between_bytes_timeout` is receive-side only. An origin that completes the handshake then
stops reading stalls that write unbounded, and the guest cannot abort a dispatched
`PendingRequest` (§3.3.3). Buffered uploads therefore fall under `outbound-deadlines`,
which is **`BestEffort` on Fastly** (footnote 1) — a label that already covers this gap;
buffering narrows the exposure to the write phase, it does not remove it.

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

⁴ `send-all-slot-isolation` is `BestEffort` on Fastly for **three** reasons.
**(a) Harvest-order response-body drain** (§3.3.4): a slot whose own
`budget.deadline` would have covered its body in isolation can still return
`gateway_timeout` because an earlier slot's body drain monopolised harvest. **(b) Cold
sequential registration** (§4.3): a first-time `Backend::builder(..).finish()` can block
without a guest-side bound, preventing later slots from being dispatched. **(c) Non-empty
buffered request upload:** `send_all` removes streamed source pulls, but Fastly has no timer
that bounds the guest-to-origin write of the surviving `Body::Once`. A slow-reading origin
can therefore hold an earlier sequential `send_async` call or its first harvested `wait()`,
delaying dispatch or observation of later slots without a finite guest-visible bound.

Only (a) becomes negligible for small response bodies. Small responses do not repair (b),
and a small request body still has no documented write-time bound for (c). Consequently the
spec makes no general "typical small-body fan-outs are unaffected" claim. Apps that need
cross-slot result isolation declare this capability required and get a hard build failure on
Fastly per the "required + BestEffort = hard fail" rule (§3.5.3). On Axum/CF/Spin,
`join_all` drives complete per-slot exchanges concurrently, so isolation is `Native`.

³ `lazy-streamed-response-passthrough` captures whether
`OutboundResponse::into_response()` delivers a streamed upstream body to the platform
response **without buffering**. **Cloudflare is the only `Native` adapter** (Axum,
Fastly, and Spin are all `BestEffort` — footnotes 3 / 6 / 7 — and each falls back to
bounded buffered passthrough). On Cloudflare the platform SDK accepts a
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

⁶ `lazy-streamed-response-passthrough` is `BestEffort` on Fastly for an
**entry-point-structural** reason, not a WASM-`Send` one. The Fastly Rust SDK does
not expose a `Response::with_streaming_body` method (that exists on `Request`, for
outbound bodies). Early/lazy response streaming to the downstream client goes
through `Response::stream_to_client(self) -> StreamingBody`, which the SDK
explicitly documents as **incompatible with `#[fastly::main]`** — the attribute
implicitly calls `Response::send_to_client()` on the returned response, and
`stream_to_client()` "cannot be used to send final responses with `#[fastly::main]`."
Apps that want true lazy passthrough on Fastly must:
1. drop the `#[fastly::main]` attribute on the entry function,
2. use an undecorated `main()` plus `Request::from_client()` to receive the
   incoming request,
3. construct the `Response`, then call `stream_to_client()` to obtain a
   `StreamingBody` they `finish()` manually.

That is a structural constraint on the Fastly scaffold — `edgezero new` (which takes only
`<name>` and `--dir`; there is **no** `--adapter` flag, it scaffolds all adapters) today
emits a `#[fastly::main]` entry for the Fastly component, and
`OutboundResponse::into_response()`
on Fastly therefore falls back to **buffered passthrough**: drain the wrapped
`Body::Stream` to `Bytes` within the adapter-level constant
**`FASTLY_RESPONSE_STREAM_BUFFER_BYTES`** (default 16 MiB, mirroring Axum's
`AXUM_RESPONSE_STREAM_BUFFER_BYTES`). The per-outbound-request
`max_response_bytes` is unavailable by the time the response converter runs
(`OutboundResponse` carries request method / status / headers / body, but no cap — §3.1.4), so the
adapter-level constant is what the converter uses. Over-cap during the buffered
drain → `response_too_large` (distinct kind, 502 — §3.4.1) — same shape as Axum. After
draining, the buffered `Bytes` is returned through the normal `#[fastly::main]` flow. Apps that need
lazy passthrough on Fastly declare this capability required and get a hard
build failure; the migration path is either (a) target **Cloudflare** (the only
`Native` adapter for this capability) or (b) wait for the §8 risk 12
follow-up that adds a non-`#[fastly::main]` entry-point template + the
`stream_to_client()` plumbing. Buffered passthrough still works on Fastly
unconditionally — only the *lazy* variant is gated.

⁷ `lazy-streamed-response-passthrough` is `BestEffort` on **Spin** for an
**EdgeZero-side public-API** reason — **not** a platform limitation, and not a
WASM-`Send` one. **Spin SDK 6 fully supports lazy response streaming**: its
`Response` body is `IncomingBody<types::Response>`, which implements
`http_body::Body` and reads in 16 KiB frames via `poll_frame`, and
`IncomingBodyExt::stream()` yields a lazy `BodyDataStream`. The SDK is not the
blocker — **EdgeZero's own alias is**. The adapter currently *chooses* the buffered
path (`crates/edgezero-adapter-spin/src/proxy.rs` calls `.bytes()`), and
`crates/edgezero-adapter-spin/src/lib.rs` pins `SpinFullResponse =
Response<FullBody<Bytes>>` across `AppExt::dispatch`, `request::dispatch*`,
`from_core_response`, and `run_app`. Delivering lazy passthrough therefore requires
**migrating those public aliases and signatures** to a streamable response shape — a
breaking public-API change that ripples into `examples/app-demo`, the Spin scaffold
templates, and every downstream consumer of `SpinFullResponse`. It carries its own
design and test surface, so it is **deliberately out of scope for this change**:
Spin's response converter performs **buffered passthrough** (drain the wrapped
`Body::Stream` to `Bytes` within a `SPIN_RESPONSE_STREAM_BUFFER_BYTES` constant,
default 16 MiB, mirroring Axum and Fastly; over-cap → `response_too_large` (502, §3.4.1)), exactly
the Axum/Fastly fallback shape. Apps that need lazy passthrough today declare the
capability required and target **Cloudflare**. Because the platform *does* support it,
lifting Spin to `Native` is a pure EdgeZero refactor and is tracked as a follow-up
(§8 risk 13) — unlike Fastly's footnote 6, which is a genuine platform constraint.
This affects only the **response-out** direction; Spin's outbound request path still
uses the hand-built `wasi:http` request in §4.4 for **both buffered and streamed bodies**.
The SDK's high-level `send` is not used for either body kind because its detached body
pump is not owned by the deadline race; a finite `Body::Once` can still block on host
backpressure. The hand-built path's cancellation guarantee remains `BestEffort` per
footnote 8.

⁸ **Spin deadline and Cloudflare header-fidelity caveats.** Spin has a monotonic timer and can race the
guest-visible exchange, but Component Model cancellation is cooperative. Dropping a
`FutureWriter` that has not written launches a background default write, and dropping a
canonical-ABI subtask does not establish a documented one-tick host teardown bound. The
adapter therefore implements explicit request/response completion protocols (§4.4) and
returns a timeout when its timer wins, while `outbound-deadlines` and
`streamed-upload-deadlines` remain `BestEffort` until host-observed runtime tests prove
bounded teardown for stalled upload and response paths. Separately, Spin exposes raw
header bytes and original field lines, so `outbound-header-fidelity` is `Native`.
Cloudflare's `BestEffort` header-fidelity cell reflects workerd's loss of raw octets and
original non-`set-cookie` field boundaries; its response-out runtime can also recompute or
ignore `Content-Length` for a streamed passthrough. EdgeZero uses `EncodeBody::Manual` to
preserve already encoded bytes and visible `Content-Encoding`, but it does not claim an
exact downstream-wire length where Workers owns framing.

#### 3.5.3 Build / startup enforcement

`ensure_capabilities` is an **outbound runtime gate**, not the owner of every CLI lifecycle.
It runs in two places: before shell or registry dispatch for `build` / `serve` / `deploy`,
and before the Axum `demo` runtime starts. The `execute(..)` gate is above the shell-command
branch so a shell override cannot bypass a required outbound capability; it branches on the
action so `auth *` remains exempt. `provision` and every `config` subcommand are outside
this outbound specification. Their owning store/capability specifications decide whether
they participate in any broader cross-capability gate.

```rust
// 1. crates/edgezero-cli/src/adapter.rs — inside execute(..), BEFORE manifest_command
// and BEFORE the registry lookup. NOT unconditional: auth is EXEMPT (credential
// class), so the gate branches on the action.
pub fn execute(
    adapter_name: &str,
    action: Action,
    manifest_loader: Option<&ManifestLoader>,
    adapter_args: &[String],
) -> Result<(), String> {
 // Runtime-producing actions only. Auth* is exempt — a runtime capability
 // mismatch must never block credential login/logout/status.
    if matches!(action, Action::Build | Action::Serve | Action::Deploy) {
 // file-backed: local borrow -> ManifestContract::Present / None (no 'static needed)
        ensure_capabilities(adapter_name, ManifestContract::from_opt(manifest_loader.map(|l| l.manifest())))?;  // site 1
    }
 // …existing shell-command / registry dispatch follows…
}

// 2. crates/edgezero-cli/src/demo_server.rs — no manifest FILE exists; read the
// manifest baked in by `app!` (Hooks::manifest).
#[cfg(feature = "demo-example")]
pub fn run_demo() -> Result<(), String> {
 // baked ('static): BakedManifest -> ManifestContract via as_contract
    ensure_capabilities("axum", <App as Hooks>::manifest().as_contract())?;         // site 2
    /* …Axum runner… */
}

```

`run_demo` is feature-gated (`demo-example`) and always selects Axum implicitly, so its
gate hardcodes the adapter name and reads the **baked** manifest rather than a file.
Sites 1–2 are exhaustive for the outbound runtime gate; unrelated CLI lifecycles are
deliberately absent.

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
| absent or empty (`required = []`, `optional = []`) | no | `log::warn!` "capability check skipped (no capabilities declared)" — proceed |
| **any `required`** entry | no | **hard failure**: `Err("adapter '<name>' is not in the registry; cannot verify REQUIRED capabilities. …")` |
| **only `optional`** entries (no `required`) | no | `log::warn!` "cannot verify its OPTIONAL capabilities — proceeding, since optional capabilities never hard-fail" — proceed |
| absent / empty | yes | proceed (loop bodies trivially pass) |
| has entries | yes | check each per the rubric below |

This preserves the "required capabilities fail early" contract while honouring
"optional never hard-fails" (§3.5.3) — an unverifiable *optional* capability warns and
proceeds, exactly as a known-degraded optional one does; only an unverifiable **required**
capability is fatal. It also keeps the brand-new-shell-only-adapter ergonomics for the
*no-capabilities* case (e.g. a contributor wiring a new edge platform via shell-out,
before they've written the adapter stub). An app that declares a **required** capability
needs a registered adapter that can answer the `capability(Capability) ->
CapabilitySupport` question; there is no silent bypass of a required contract.

Commands covered by the two outbound gate sites above:

| PR-#269 command | Entry point | Gate site |
| --- | --- | --- |
| `edgezero build` | `run_build` → `execute(Action::Build, ..)` | `execute(..)` — **gated** |
| `edgezero serve` | `run_serve` → `execute(Action::Serve, ..)` | `execute(..)` — **gated** |
| `edgezero deploy` | `run_deploy` → `execute(Action::Deploy, ..)` | `execute(..)` — **gated** |
| `edgezero auth login` / `logout` / `status` | `run_auth` → `execute(Action::AuthLogin/Logout/Status, ..)` | **EXEMPT** (credential + read-only class). The `execute(..)` gate must **branch on the action** and skip the `Auth*` actions. |
| `edgezero demo` (feature `demo-example`) | `run_demo` → Axum runner. `run_demo()` takes **no path or loader** and reads no manifest file, so a file-based gate is impossible. **Locked resolution — gate on baked manifest metadata via a new `Hooks` accessor** (see below) | `run_demo()` calls `ensure_capabilities("axum", <App as Hooks>::manifest().as_contract())` before the Axum runner starts |

**The `demo` gate needs a baked-manifest accessor — `app!` must emit one.**
`run_demo()` (`crates/edgezero-cli/src/demo_server.rs`) hardcodes `run_app::<App>()` for the
concrete `app_demo_core::App` (a **struct**, so it cannot be a trait bound), so a test cannot
inject a crafted `Hooks`. **Add a PURE gate helper**, not a generic `run_demo`:

```rust
// edgezero-cli — no server start, so a test can call it directly and cheaply.
// NOT feature-gated: this helper and its test are compiled UNCONDITIONALLY (they do not
// live behind `#[cfg(feature = "demo-example")]`), so the row runs under the plain
// `cargo test --workspace --all-targets` CI gate. Only `run_demo()` itself — which pulls
// in the `app-demo` example — stays behind `demo-example`; the pure gate does not depend
// on the example, only on the `Hooks` trait, so keeping it always-compiled is free.
pub(crate) fn demo_capability_gate<A: Hooks>() -> Result<(), String> {
    ensure_capabilities("axum", <A as Hooks>::manifest().as_contract())
}
```

`run_demo()` (behind `demo-example`) calls `demo_capability_gate::<app_demo_core::App>()?`
**before** `run_app`. The failure test calls `demo_capability_gate::<TestApp>()` directly
with an in-crate `TestApp` — no `demo-example` feature, no blocking server on the success
path — so it is exercised by the default test command. **`TestApp` must override `manifest()`, not just `manifest_json()`:** the default
`manifest()` returns `Absent` (a static in the trait default would be shared across impls), so
overriding only `manifest_json()` would leave the gate seeing `Absent` and proceeding. `TestApp`
overrides `manifest()` to return `Manifest::from_baked_json(<crafted-json>)`.
`run_demo` has no path, no `ManifestLoader`, and no way to find `edgezero.toml` at runtime. But
the `app!` macro **already parses, validates, and serializes the manifest at compile
time** (`crates/edgezero-macros/src/app.rs`: `manifest.finalize()` →
`serde_json::to_string(&manifest)` → `manifest_json_lit`) — it just embeds that JSON for
the router and never exposes it. Today `Hooks`
(`crates/edgezero-core/src/app.rs`) has `routes()` / `stores()` / `name()` and **no
manifest accessor**, so the gate has nothing to consult. Add one:

```rust
// edgezero-core/src/app.rs — extend the existing Hooks trait.
pub trait Hooks {
 // …existing: build_app / configure / name / routes / stores…

 /// Raw manifest JSON baked in at compile time by `app!`.
 /// Default `None` for hand-written `Hooks` impls that never ran the macro.
    fn manifest_json() -> Option<&'static str> { None }

 /// Parsed + finalized, cached view of the above.
 ///
 /// **The default MUST be `Absent` — it must NOT hold a cache.** A `static` declared
 /// inside a trait default method body is **one item shared by every implementor**,
 /// not one per `Self` (items in generic fns are not monomorphized — Rust Reference).
 /// A caching default would therefore let the FIRST app to call `manifest`
 /// populate the value every OTHER app reads — capability checks against the wrong
 /// manifest. Proven: two impls relying on such a default both returned the first
 /// impl's value. The cache lives in each **macro-generated impl** instead (below),
 /// where the `static` is a distinct item per impl.
    fn manifest() -> BakedManifest { BakedManifest::Absent }
}

// edgezero-core/src/manifest.rs
//
// THREE states, not `Option`. `Option<&Manifest>` conflates "this app has no baked
// manifest" (legitimate: a hand-written `Hooks`, no macro → no capability contract →
// proceed) with "the baked manifest is CORRUPT" (an adapter/macro contract bug). If
// both collapse to `None` and `ensure_capabilities` treats `None` as permission to
// proceed, a malformed contract **silently disables required-capability enforcement**
// — it fails OPEN. This enum makes that unrepresentable.
// `#[non_exhaustive]`: a future state (e.g. a lazily-parsed variant) must not silently
// pass a match written today. Matches in OTHER crates (the CLI gate) therefore need a
// `_` arm, and that arm MUST fail closed (treat an unknown state as "cannot verify →
// refuse"), never proceed — see `ensure_capabilities`.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum BakedManifest {
 /// No `app!`-baked manifest (hand-written `Hooks`). No capability contract.
    Absent,
 /// `manifest_json` returned Some, but it did not parse/finalize. An
 /// adapter/macro contract bug: the JSON came from `serde_json::to_string` on an
 /// already-validated `Manifest`, so this is unreachable unless the macro is broken.
    Malformed(&'static str),   // static reason, for the diagnostic
 /// Successfully parsed + finalized.
    Present(&'static Manifest),
}

impl Manifest {
 /// Parse baked JSON and rebuild derived state (`finalize`). Returns
 /// `BakedManifest::Malformed` — never `Absent` — on failure, so a corrupt
 /// contract can never be mistaken for "no contract".
    pub fn from_baked_json(json: &'static str) -> BakedManifest { /* … */ }
}

// What `app!` GENERATES per app — each impl gets its OWN `manifest` fn, hence its
// OWN `static`, hence a genuinely per-app cache.
//
// TWO requirements on the generated code:
// 1. FULLY-QUALIFIED PATHS. This expands in the DOWNSTREAM crate, which may not have
// `Manifest` or `OnceLock` in scope (or may shadow them). Macro output must name
// `::edgezero_core::manifest::Manifest`, `::std::sync::OnceLock`, etc. — never a
// bare `Manifest`/`OnceLock`. (The snippet below is written qualified for exactly
// this reason; do not "tidy" it into short paths.)
// 2. FAIL CLOSED on a malformed baked contract — see `BakedManifest` below.
impl ::edgezero_core::app::Hooks for MyApp {
    fn manifest_json() -> Option<&'static str> { Some(r#"{…baked…}"#) }

    fn manifest() -> ::edgezero_core::manifest::BakedManifest {
 // per-IMPL: a distinct static item, because this fn is generated per impl.
        static CACHE: ::std::sync::OnceLock<::edgezero_core::manifest::BakedManifest> =
            ::std::sync::OnceLock::new();
        *CACHE.get_or_init(|| {
            match <Self as ::edgezero_core::app::Hooks>::manifest_json() {
                None => ::edgezero_core::manifest::BakedManifest::Absent,
                Some(json) => ::edgezero_core::manifest::Manifest::from_baked_json(json),
            }
        })
    }
 // …routes/stores/etc…
}

// edgezero-core/src/manifest.rs — the accessor CANNOT parse+finalize itself, because
// `Manifest::finalize` is `pub(crate)` and the `app!`-generated `manifest` lives in
// the DOWNSTREAM crate, which can't reach it. Core owns the parse+finalize:
impl Manifest {
 /// Parse baked JSON and rebuild derived state (`finalize`).
 /// Returns `Malformed` — NEVER `Absent` — on failure, so a corrupt contract can
 /// never be mistaken for "no contract" (fail closed, see `ensure_capabilities`).
    pub fn from_baked_json(json: &'static str) -> BakedManifest {
        // SAME pipeline as `try_load_from_str`: parse -> VALIDATE -> finalize.
        // Skipping validate() would fail OPEN: `{}` is valid JSON and parses to a
        // default Manifest with EMPTY capabilities, so an invalid-but-parseable
        // contract would become `Present` and the gate would proceed against no
        // required capabilities. finalize() only rebuilds derived logging state — it
        // is NOT validation. A parse OR validation failure is a contract bug -> Malformed.
        let mut manifest: Manifest = match serde_json::from_str(json) {
            Ok(manifest) => manifest,
            Err(_) => return BakedManifest::Malformed("baked manifest did not parse"),
        };
        if manifest.validate().is_err() {
            return BakedManifest::Malformed("baked manifest failed validation");
        }
        manifest.finalize(); // pub(crate) — reachable here, inside core
        // Leaked into a 'static: parsed once per process behind the generated
        // per-impl OnceLock, and the result must outlive the call.
        BakedManifest::Present(Box::leak(Box::new(manifest)))
    }
}
```

> **`from_baked_json` is `#[doc(hidden)]` macro-support API — call it AT MOST ONCE per
> process.** Each successful call `Box::leak`s a `Manifest` into a `'static`; the generated
> `app!` code invokes it exactly once behind a per-impl `OnceLock`, so the leak is bounded
> to one allocation per app. It is `pub` only because the macro expands in the app's crate,
> **not** an invitation for arbitrary callers — a hand-written caller that invokes it per
> request would leak unboundedly. The doc-comment states the once-per-process contract and
> the type is `#[doc(hidden)]`; direct use is out of contract.

- **`app!` MUST emit BOTH methods explicitly.** The macro's generated `impl Hooks`
  cannot rely on the trait defaults: `crates/edgezero-macros/src/app.rs` sets
  **`clippy::missing_trait_methods = deny`** (verified — a generated impl that omits a
  defaulted method is a hard clippy error: *"missing trait method provided by default"*).
  So the macro emits `manifest_json()` (returning `Some(#manifest_json_lit)`) **and**
  `manifest()` (the `OnceLock` + `from_baked_json` body above), exactly as it already
  emits `configure`/`build_app` for the same reason. **Any hand-written `Hooks` impl in
  the codebase must also emit both** (or locally `#[allow]` the lint) — "additive and
  defaulted" is **not** free here.
- **Reparsing JSON alone is WRONG — it skips `finalize()`.** `Manifest::finalize()`
  rebuilds derived state (e.g. resolved routes) that is **not** in the serialized JSON,
  and several fields are `#[serde(skip)]`. A bare `serde_json::from_str` yields a
  half-built `Manifest`. Hence `from_baked_json` calls `finalize()` — and hence it must
  live in **core** (only core can call the `pub(crate)` `finalize`), not in the generated
  downstream impl.
- **Ownership / lifetime — the cache MUST live in the generated impl, never in the trait
  default.** The JSON is a `&'static str` in the binary. The `OnceLock` is a
  function-local `static` inside **each generated `manifest()`**; because every impl
  emits its own `manifest()` fn, each gets a **distinct** `static` — a genuine per-app
  cache, parse paid once per process. **A `static` in the trait DEFAULT body is shared
  by all implementors** (items inside generic fns are not monomorphized), so a caching
  default would serve app A's manifest to app B. This was **empirically proven** with two
  impls relying on such a default: both returned the first one's value. Hence the default
  is `BakedManifest::Absent`, and a single core-global `OnceLock` is likewise rejected. *(This is a second,
  independent reason the macro must emit `manifest()` explicitly — beyond the denied
  `clippy::missing_trait_methods`.)*
- **Failure mode — FAIL CLOSED, and the two failures are distinct.** `manifest()`
  returns a three-state `BakedManifest`, never an `Option`, because `Option` conflates
  two opposite situations: `Absent` (hand-written `Hooks`, no macro → genuinely *no
  capability contract* → `ensure_capabilities` short-circuits `Ok(())`, the same "no
  manifest" policy as §3.5.3) versus `Malformed` (the JSON was baked but doesn't
  parse → an `app!`/core contract bug). If both were `None` and `None` meant "proceed",
  **a corrupt baked contract would silently disable required-capability enforcement** —
  the gate would report success precisely when it can no longer verify anything.
  `Malformed` therefore **hard-fails** with an actionable message. A gate that fails
  open on unreadable input is worse than no gate.
- **Test seam:** capability-gate failure tests drive `demo` with a **test-only `Hooks`
  impl** overriding `manifest_json()` to return crafted JSON (which then flows through
  the real `from_baked_json` + `finalize`) — no file, no macro re-expansion. This is why
  `manifest_json()` is a trait method with a default, not a free function or bare `const`.
  (The test impl must also emit `manifest()` — or `#[allow(clippy::missing_trait_methods)]`
  — per the lint above.)

Commands **not** covered (and why):
- `edgezero new` — generates source files; no adapter is selected, so capabilities
  cannot be checked. The scaffold itself is identical across adapters.
- `edgezero auth *` — **exempt by command class** (credential; §3.5.3 table), so the
  gate never runs for it regardless of manifest presence. (Independently, a genuinely
  absent manifest is `BakedManifest::Absent` ⇒ no capability contract ⇒ `Ok(())`; a
  *malformed* one hard-fails. Documented in the rustdoc.)

**Support-level enforcement ladder (what `required` means).** `capability()` returns one of `Native` > `BoundedCooperative` > `BestEffort` > `Unsupported`. A capability in `required` is satisfied by **`Native` or `BoundedCooperative`** (both are *real* enforcement — `BoundedCooperative` has a precisely documented, deterministic bound); it **hard-fails** on `BestEffort` (real-world deviation the app must opt into) or `Unsupported`. `optional` never hard-fails — a `BestEffort`/`Unsupported` optional capability is logged, not gated. **Apps that require the documented outbound-deadline guarantee declare `outbound-deadlines` `required`.** No adapter reports `BoundedCooperative` for that capability, so the declaration is accepted only on Axum and Cloudflare; it hard-fails on Fastly and Spin. A separate `outbound-deadlines-exact` capability is unnecessary.

**Historical (pre-#269) shape — now superseded (PR #269 has merged to main):**
Before #269 landed, `Command::{Build, Serve, Deploy, Dev}` all dispatched through
the registry's `Adapter::execute(AdapterAction::{Build, Serve, Deploy}, ..)` plus
`Command::Dev`'s implicit-Axum runner, and the gate went at the top of each of
those four handlers (or the equivalent helper they called). #269 collapsed the
runtime-producing actions into the single `execute(..)` dispatcher; `demo` remains
the second outbound runtime gate.

```rust
// NOTE: takes an already-parsed manifest, NOT a `ManifestLoader`. The `demo` gate has
// no manifest *file* to load — it reads the manifest baked in by `app!` via
// `<App as Hooks>::manifest` (see the demo row above). File-backed callers pass
// `Present`/`Absent` from their loader; `demo` passes the baked `BakedManifest`.
//
// FAIL CLOSED. `Absent` (no contract) proceeds; `Malformed` MUST NOT — an earlier
// draft used `Option` and treated `None` as permission to proceed, so a corrupt baked
// contract silently disabled required-capability enforcement. A capability gate that
// fails open on malformed input is worse than no gate: it reports success.
// INPUT TYPE: a LIFETIME-BEARING contract, NOT `BakedManifest`. File-backed runtime
// sites hold a **local** `&Manifest` borrowed from a loader — those are not
// `'static`, so they cannot be wrapped in `BakedManifest::Present(&'static Manifest)`.
// Only `run_demo` has a `'static` (baked) manifest. So the gate accepts a borrow of any
// lifetime, and `BakedManifest` (which is `'static`) converts INTO it:
//
// #[non_exhaustive]  // future states must fail closed in the cross-crate CLI match
// pub enum ManifestContract<'a> {
// Malformed(&'static str), // corrupt baked contract → fail closed
// None, // no contract → proceed (legit: no manifest)
// Present(&'a Manifest), // any lifetime — file-backed OR baked
// }
// impl BakedManifest {
// pub fn as_contract(&self) -> ManifestContract<'_> { /* Absent→None, Malformed→Malformed, Present→Present */ }
// }
// impl<'a> ManifestContract<'a> {
// pub fn from_opt(m: Option<&'a Manifest>) -> Self { m.map_or(Self::None, Self::Present) }
// }
// `ManifestContract` + `as_contract` are public because the generated demo app lives
// outside edgezero-core. File-backed callers build `Present(local_ref)` / `None`;
// `run_demo` calls `<App as Hooks>::manifest.as_contract`.
// `ensure_capabilities` is `pub(crate)` because `run_demo` imports it from another
// edgezero-cli module.
pub(crate) fn ensure_capabilities(
    adapter_name: &str,
    manifest: ManifestContract<'_>,
) -> Result<(), String> {
    let manifest = match manifest {
 // No manifest ⇒ no capability contract to enforce. Legitimate.
        ManifestContract::None => return Ok(()),
 // Corrupt baked contract ⇒ we cannot know what was required. Refuse.
        ManifestContract::Malformed(reason) => {
            return Err(format!(
                "capability check aborted: {reason}. This is an EdgeZero/app! contract \
                 bug — the baked manifest is unreadable, so required capabilities \
                 cannot be verified. Refusing to proceed rather than silently skipping \
                 enforcement."
            ));
        }
        ManifestContract::Present(manifest) => manifest,
 // `ManifestContract` is `#[non_exhaustive]`; a future state we don't recognize is
 // treated like `Malformed` — we cannot verify the contract, so we FAIL CLOSED.
        _ => {
            return Err(
                "capability check aborted: unrecognized manifest-contract state. \
                 Refusing to proceed rather than skipping enforcement."
                    .to_string(),
            );
        }
    };
    let caps = &manifest.capabilities;
    let Some(adapter) = registry::get_adapter(adapter_name) else {
 // Missing-from-registry policy (see table). If the manifest
 // declares no capabilities, we can't verify anything anyway — log
 // and proceed so brand-new shell-only adapters work before a stub
 // is wired. Optional declarations still warn and proceed because optional never
 // hard-fails; only a REQUIRED declaration is unverifiable and must fail closed.
        if caps.required.is_empty() {
            // No REQUIRED capabilities to verify. Optional ones can't be verified either,
            // but "optional never hard-fails" (§3.5.3) — so warn and proceed rather than
            // erroring. A hard fail here would break an optional-only manifest.
            if caps.optional.is_empty() {
                log::warn!(
                    "adapter '{adapter_name}' not in registry; capability check skipped (no capabilities declared)",
                );
            } else {
                log::warn!(
                    "adapter '{adapter_name}' not in registry; cannot verify its OPTIONAL \
                     capabilities — proceeding, since optional capabilities never hard-fail",
                );
            }
            return Ok(());
        }
        // At least one REQUIRED capability and no metadata to check it against → fail closed.
        return Err(format!(
            "adapter '{adapter_name}' is not in the registry; cannot verify REQUIRED \
             capabilities. Register an adapter stub that returns capability metadata, or \
             move those entries to `optional`.",
        ));
    };

    // POSITIVELY accept only `Native | BoundedCooperative` for a REQUIRED capability.
    // `CapabilitySupport` is `#[non_exhaustive]`; matching the two acceptable values and
    // routing EVERYTHING else — `BestEffort`, `Unsupported`, and any FUTURE variant via
    // `_` — to a rejection fails CLOSED. A `.filter(== Unsupported)` / `.filter(==
    // BestEffort)` pair would let an unknown future support level pass a required gate.
    let mut unsupported: Vec<&str> = Vec::new();
    let mut best_effort: Vec<&str> = Vec::new();
    for cap in caps.required.iter().copied() {
        match adapter.capability(cap) {
            CapabilitySupport::Native => {}
            CapabilitySupport::BoundedCooperative => log::info!(
                "adapter '{adapter_name}': required capability '{}' is bounded-cooperative; \
                 see capability docs for the bound",
                cap.as_str(),
            ),
            CapabilitySupport::BestEffort => best_effort.push(cap.as_str()),
            // `Unsupported` AND any unknown/future variant → treated as unsupported.
            _ => unsupported.push(cap.as_str()),
        }
    }
    if !unsupported.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}' does not support required capabilities: {}",
            unsupported.join(", "),
        ));
    }
    if !best_effort.is_empty() {
        return Err(format!(
            "adapter '{adapter_name}': required capabilities are only best-effort: {}. \
             best-effort means a documented limitation applies — timing (e.g. \
             unbounded cooperative enforcement) or functional (e.g. lazy streaming \
             becomes buffered). See the capability reference at \
             https://edgezero.dev/guide/capabilities. Declare them \
             `optional` if the documented limitation is acceptable.",
            best_effort.join(", "),
        ));
    }
 // Adapter-specific service-config reminders. Capability values are static
 // **Scope decision: capabilities are STATIC ADAPTER-CODE facts ("this adapter CAN
 // issue outbound HTTP"), NOT deployment-state validation.** `required outbound-http`
 // passing means the Fastly adapter has the outbound code path — it deliberately does
 // NOT assert the Fastly *service* has dynamic backends enabled, because that is
 // account/service state EdgeZero cannot read from the CLI. So a `required outbound-http`
 // can pass while a mis-provisioned service would fail at deploy/runtime. That gap is
 // intentional and handled at the RIGHT layer: (a) this info-log reminder at gate time;
 // (b) the deploy path surfaces `BackendCreationError::Disallowed` as an actionable
 // `bad_gateway` at runtime (§4.3). It is NOT represented as a capability, because
 // capabilities model code, not deployment prerequisites. (If a future design wants
 // prerequisites gated, they need a SEPARATE representation — a deploy-time preflight,
 // not the capability contract — this is called out so the scope is explicit, not
 // silently fail-open.)
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
    // Optional: warn on BestEffort, Unsupported, AND any unknown/future variant (the
    // ladder logs all degradations, per §3.5.3); only `Native | BoundedCooperative` is
    // "available" and silent. An unknown future support level is a degradation we can't
    // characterize, so it warns rather than passing silently.
    for cap in caps.optional.iter().copied() {
        match adapter.capability(cap) {
            CapabilitySupport::Native | CapabilitySupport::BoundedCooperative => {}
            CapabilitySupport::Unsupported => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' unavailable",
                cap.as_str(),
            ),
            CapabilitySupport::BestEffort => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' is best-effort — a \
                 documented deviation applies; see the capability reference at \
                 https://edgezero.dev/guide/capabilities",
                cap.as_str(),
            ),
            _ => log::warn!(
                "adapter '{adapter_name}': optional capability '{}' reports an unrecognized \
                 support level; treating as degraded",
                cap.as_str(),
            ),
        }
    }
    Ok(())
}
```

**Manifest-discovery invariant — `None` must mean "no manifest exists," NOT "none in the
current directory."** `ManifestContract::None → proceed` is only safe if a file-backed
runtime-producing command (`build` / `serve` / `deploy`) cannot reach the gate with `None`
**while a real `edgezero.toml` exists at the project root**.
Today the CLI resolves the default manifest as `./edgezero.toml` (cwd-relative), but the
Spin adapter independently walks **upward** for `spin.toml` — so `edgezero build --adapter
spin` run from a nested subdirectory finds no `./edgezero.toml` (→ `None` → capability +
host-drift checks skipped) yet Spin still discovers the root `spin.toml` and builds. That
is a **capability-enforcement bypass from a nested cwd.** Fix: these commands must perform
**root-manifest discovery** — walk up from cwd to the first `edgezero.toml` (the same
upward search Spin does for `spin.toml`, anchored at the same root) — before gating.
`None` is then reserved for the genuinely-manifestless cases (`demo` / hand-written
`Hooks`).

**Discovery must be one shared resolver for the three `execute(..)` runtime actions.**
Introduce `pub fn resolve_root_manifest(source: ManifestSource) ->
Result<Option<ResolvedManifest>, String>`. **The `Option` is load-bearing:** discovery can
end in a genuine **`Ok(None)`** — no `edgezero.toml` found walking up to the filesystem root,
which is legitimately "no manifest / no capability contract → proceed" — distinct from
`Err(..)` (a manifest was found but is malformed/invalid). `Ok(Some(rm))` is a found+parsed
manifest. (An earlier draft returned `Result<ResolvedManifest, ..>`, which could not
represent the absent case and mismatched the `.as_ref()` gate call sites; corrected here.)
Its input records whether `EDGEZERO_MANIFEST` supplied an explicit path or the command is
using default upward discovery:
```
pub enum ManifestSource {
    Defaulted,              // no env path → walk up from cwd
    EnvVar(PathBuf),        // EDGEZERO_MANIFEST set → use verbatim
}
/// The resolved manifest, carried through gating AND execution so nothing re-reads it.
pub struct ResolvedManifest {
    pub loader: ManifestLoader,
    pub path: PathBuf,
}
impl ResolvedManifest {
    pub fn manifest(&self) -> &Manifest {
        self.loader.manifest()
    }
}
```
The loader is the feasible owned type: it already owns the non-`Clone` `Manifest` behind a
private `Arc` and exposes `manifest() -> &Manifest`. Each of `run_build`, `run_serve`, and
`run_deploy` binds one resolved value and passes
`resolved.as_ref().map(|item| &item.loader)` into the existing `execute(..)` API; `execute`
derives `ManifestContract` from that same loader and performs the single gate shown above.
The `run_*` entry points do not gate a second time. There is no extraction or clone of
`Manifest` and no second parse. Precedence is
`EDGEZERO_MANIFEST` verbatim, otherwise upward discovery from cwd, otherwise genuine
`Ok(None)`. §5.4 runs each of the three actions from a nested subdirectory under both an
upward-discovered root and an explicit `EDGEZERO_MANIFEST` path.

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
- Optional + **`BestEffort` OR `Unsupported`** → `log::warn!` naming the capability and
  the degradation (the ladder logs *both*, not just Unsupported); never a hard fail.

#### 3.5.4 Outbound host plumbing — not policy

`[capabilities.outbound].hosts` is **plumbing**, not an application security allowlist
(non-goal §1.3). Applications still enforce target policy in handler code.

- **Spin** requires `allowed_outbound_hosts` in `spin.toml`. The platform manifest
  consumes the canonical list below; absent hosts preserve today's
  `["https://*:*"]` default. The outbound runtime path for `build` / `serve` /
  `deploy` compares the canonicalized sets before shell or registry dispatch and
  hard-fails on drift. The existing platform-manifest generation flow is responsible for
  regenerating a user-owned `spin.toml`; this outbound spec does not redesign
  non-runtime lifecycle commands.

  Every entry is canonicalized by the host-authority subset of `OutboundRequest`'s
  URI rules (§3.1.3): lowercase scheme and host; strip `:443` for `https` and
  `:80` for `http`; reject userinfo and fragments. Manifest host entries are
  declarations, not request targets, so paths and queries are also rejected. Drift
  comparison is set-based and order-insensitive.

  The absent default deliberately remains `["https://*:*"]`; it is not widened to
  cleartext HTTP. Apps that need cleartext outbound access declare it explicitly.

  | Input form (after canonicalization) | Example | Spin output |
  | --- | --- | --- |
  | wildcard | `"*"` | `["http://*:*", "https://*:*"]` |
  | scheme-prefixed | `"http://localhost:3000"`, `"https://api.example.com:8443"` | rendered as-is |
  | `host:port` (no scheme) | `"api.example.com:8443"` | `"https://api.example.com:8443"` |
  | bare host | `"api.example.com"` | `"https://api.example.com"` |
  | wildcard subdomain | `"*.example.com"` | `"https://*.example.com"` |

  The §3.5.1 validator is authoritative; there is no fallback for other forms.
  Mixing `"*"` with specific hosts is allowed. Bare hosts mean HTTPS on the default
  port only. Canonicalization occurs once before both rendering and drift comparison.
- **Fastly** uses runtime dynamic backends, so it does not need the list at build time;
  `hosts` is informational there.
- **Axum / Cloudflare** ignore the list because they require no host pre-declaration.

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
     over-cap → `bad_request`. Every pull uses this absolute-deadline protocol:
     (a) call `budget.deadline.remaining()` immediately before the pull and return
     attributed `gateway_timeout` if it is `None`; (b) race `source.next()` against
     `tokio::time::sleep(remaining)`; (c) when the source arm becomes ready, call
     `budget.deadline.is_expired()` **before** inspecting or accepting its chunk,
     EOF, or error, and return the same timeout if expired. The post-ready check
     makes the deadline win simultaneous source/timer readiness and prevents an
     always-ready stream, including one yielding empty chunks, from starving the
     timer past the absolute boundary. Only a source result proven ready before the
     deadline proceeds to error propagation and pre-append cap accounting. Adding
     reqwest's `stream` feature is **not** required.
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
     by `budget.deadline`; the wrapper yields a `gateway_timeout` (attributed via `budget.cause`, §3.3.2) error
     chunk past the deadline so the streamed body honours the deadline
     end-to-end per §3.3.3.
- Errors: `reqwest` timeout → **`gateway_timeout_caused(msg, budget.cause)`** (carries the
  attribution — §3.3.2, NOT bare `gateway_timeout`); connect/DNS/TLS → `bad_gateway`;
  response over-cap → `response_too_large` (distinct kind, 502 — §3.4.1). Any completed exchange (incl. non-2xx) → `Ok`.
- `capability()` per §3.5.2: `outbound-http` = `Native`,
  `outbound-header-fidelity` = `Native`, `outbound-deadlines` = `Native`,
  `outbound-flexible-phase-budget` = `Native` (Axum's reqwest exposes a single total
  timeout, not a phase split), `send-all-slot-isolation` = `Native`,
  `streamed-upload-deadlines` = `Native`, `lazy-streamed-response-passthrough` =
  `BestEffort` (footnote 3 — Axum buffers, see `response.rs` task in §7).
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
     over-cap → `bad_request`. Every pull uses the same absolute-deadline protocol
     as Axum: read `remaining()` immediately before the pull and fail with an
     attributed `gateway_timeout` when absent; race `source.next()` against
     `worker::Delay::from(remaining)`; then, if the source arm becomes ready,
     recheck `is_expired()` **before** accepting its chunk, EOF, or error. Timeout
     wins simultaneous readiness. The post-ready check also prevents continuously
     ready or empty chunks from starving the delay past the absolute deadline.
     Source errors and cap accounting run only after that check.
  3. **Construct the `worker::Request`.** Build the request from the
     buffered (or now-buffered) body, URI, method, and normalized headers.
     Do not start the `worker::Delay` race yet.
  4. **Arm the race and send — WITH an `AbortController`, not just a dropped future.**
     Create a `worker::AbortController`, take its `signal()`, and issue the fetch via
     **`Fetch::send_with_signal(&signal)`** (worker 0.8.3) — **not** the plain `send()`.
     Immediately before issuing, re-read `budget.deadline.remaining()`; `None` →
     `gateway_timeout` without sending. Otherwise race `send_with_signal(..)` **and**,
     in `Buffered` mode, the body drain against `worker::Delay::from(remaining)`
     (worker 0.8.3 — `Delay` has private fields, so `worker::Delay(remaining)` tuple
     construction does NOT compile; use the `From<Duration>` impl). **On expiry,
     call `controller.abort()` — do NOT merely drop the future.** Dropping the Rust
     future does not cancel the in-flight subrequest (the Workers runtime keeps the POST
     going and may complete it after EdgeZero has returned 504); only the abort signal
     actually cancels it. Then return `gateway_timeout`. The existing gzip/br
     decompression path is kept; the decompressed-byte cap is enforced
     incrementally while decompressing (§3.4.1), with pre-append checked
     accounting.
- **Streamed responses honour the deadline AND cancel the subrequest.** Wrap the
  response body as `Body::Stream`, with a per-chunk race against a `worker::Delay`
  bounded by `budget.deadline`. **The `AbortController` (from the send step) MUST move
  INTO the body wrapper — the wrapper owns it for the streamed case**, because the
  subrequest is still live while the body streams. The wrapper `controller.abort()`s in
  **all three** cases, but what it yields differs by cause, so the status is not
  conflated: (a) the deadline `worker::Delay` fires → abort **+ a `gateway_timeout`
  (504) error chunk**; (b) an underlying chunk read fails → abort **+ a `bad_gateway`
  (502) error chunk** (a transport failure is 502, matching the buffered path below —
  **not** 504); (c) the **consumer drops the stream early** (`Drop` on the wrapper) →
  **abort only, no error chunk** (the consumer that dropped is not waiting for one).
  Merely yielding an error chunk without `abort()` (an earlier draft) leaves the CF
  subrequest running after EdgeZero has stopped reading — the same bug the buffered path
  fixes. So the deadline is honoured end-to-end, a mid-stream transport failure maps to
  502 (not 504), **and** the origin observes cancellation in every case.
- Errors: `worker::Delay` expiry → **`gateway_timeout_caused(msg, budget.cause)`** (attributed — §3.3.2); `worker::fetch` transport
  failure (DNS/TLS/connection refused) → `bad_gateway`; **request**-body over-cap →
  `bad_request` (400); **response**-body over-cap (decompressed count) → `response_too_large`
  (distinct kind, 502, per the global response-overflow rule §3.4.1). Any completed exchange
  (incl. non-2xx) → `Ok`. (§3.4.3 is the fallback for variants not enumerated here.)
- **Method / body preflight — no silent coercion.** The current adapter maps
  unsupported methods to `GET`; that is **removed**. Per §3.1.4, a non-portable method
  or a `GET`/`HEAD` carrying a body is rejected in **core preflight** with
  `bad_request`, identically on every adapter. The CF adapter never rewrites the
  method to satisfy `fetch`'s restrictions.
- **Multi-value response headers — two real bugs to fix, not a platform limit.**
  `worker::Headers` has both `append(&self, ..)` and `get_all(&self, ..)`, and this
  repo's pinned `compatibility_date = "2023-05-01"` already enables workerd's
  per-`set-cookie` `entries()` behaviour. `set-cookie` is therefore **fully
  preservable** on CF; today it is dropped by EdgeZero's own code:
  1. **`src/proxy.rs` (upstream → core):** the `entries()` loop calls
     `HeaderMap::insert`, which **removes all previous values** — two upstream
     `set-cookie`s collapse to the last one. **Fix: `insert` → `append`.**
  2. **`src/response.rs` (core → client):** the `&parts.headers` loop calls
     `Headers::set`, which **replaces** — a handler emitting two `Set-Cookie`s ships
     only the last to the browser. **Fix: `set` → `append`.** (`&HeaderMap` iteration
     already yields the name once per value, so `append` is correct and complete.)
- **Panic hazard in the outbound request path — must be fixed.** `Headers::from(&HeaderMap)`
  does `value.to_str().unwrap()`, and `HeaderValue::to_str` **errors on any byte outside
  visible ASCII**. A proxied non-ASCII (but perfectly valid UTF-8) header such as
  `x-app-display-name: café` therefore **panics the worker**. Replace `Headers::from(..)`
  with an explicit loop (`Headers::append` takes `&self`, so no `mut` needed) that
  handles the non-ASCII case per §3.1.4 instead of unwrapping. Duplicate preservation in
  this direction is already correct (`Headers::from` appends per value) — the defect is
  the panic, not the multi-value handling.
- **What stays irrecoverable (§3.1.4):** repeated **non**-`set-cookie` headers are
  comma-joined by workerd (`x-foo: a, b`) with no API to recover the separate field
  lines, and raw upstream bytes / invalid UTF-8 are lost before the guest sees them.
  **Never call `get_all` with any name but `"set-cookie"`** — the binding lacks `catch`
  and workerd throws, unwinding across the wasm boundary.
- **Encoded passthrough must stay byte-preserving.** When the portable content-encoding
  policy chooses passthrough (unknown, parameterized, or stacked/repeated encoding), the
  core response retains the raw bytes and visible `content-encoding`. The Cloudflare
  response-out converter MUST then select `worker::EncodeBody::Manual` (via
  `Response::with_encode_body`) whenever it forwards an already encoded body; leaving the
  default automatic mode lets Workers transform bytes that EdgeZero promised to pass
  through unchanged. Known gzip/br responses decoded by EdgeZero have their encoding
  header removed and use the normal automatic/identity path. Workers can recompute or
  ignore a user `content-length` for a streamed response, so exact downstream-wire
  `Content-Length` retention is part of Cloudflare's documented
  `outbound-header-fidelity = BestEffort` deviation; the app-visible header decision and
  raw encoded payload remain deterministic.
- `capability()` per §3.5.2: `Native` for six outbound capabilities
  (`outbound-http`, `outbound-deadlines`, `outbound-flexible-phase-budget` (single
  `worker::Delay` for the total race, no per-phase split), `send-all-slot-isolation`,
  `streamed-upload-deadlines`, `lazy-streamed-response-passthrough`) and `BestEffort`
  for `outbound-header-fidelity` because workerd removes raw-octet/field-line information
  before the guest. Cloudflare's WASM single-threaded guest carries no
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
// PendingRequest::poll(self) -> PollResult (non-blocking)
// PendingRequest::wait(self) -> Result<Response, SendError> (blocks on one)
// Request::send_async(self, backend) -> Result<PendingRequest, SendError>
```

`select` does not report which request completed, so it cannot preserve request↔slot
identity — and the application must know which target answered. The adapter harvests by **indexed
slot** with `wait()` / `poll()`:

```rust
// Each Pending slot carries the metadata `harvest` needs — without these, the
// post-`wait` body buffering / cap / deadline contract would have nothing to
// work from. (`send_all` rejects streamed REQUEST bodies AND streamed responses
// per in preflight, so the slot only ever has to handle Buffered
// responses with a max_bytes cap.)
struct PendingSlot {
    pending:    PendingRequest,
    budget:     DispatchBudget,    // duration + absolute deadline + cause (§3.3.2)
    max_bytes:  u64,              // from ResponseMode::Buffered { max_bytes } — u64 cap (§3.1.3)
    req_method: Method,           // captured pre-dispatch: `wait()`/`poll()` return only the
                                  // response, but §3.4.1's no-content rule is METHOD-aware — a
                                  // `HEAD` reply carrying `Content-Encoding` + a representation
                                  // `Content-Length` must be passed through, NOT decoded (else a
                                  // false 502). `harvest` combines this with the response status.
}

enum Slot {
    Done(Result<OutboundResponse, EdgeError>),
    Pending(PendingSlot),
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
 // in a homogeneous-budget batch.
    let batch_now = web_time::Instant::now();

 // Phase 0 — preflight. **The shared `validate_for_dispatch` runs FIRST, exactly once
 // per slot** (§3.1.4 — the mandatory shared validator, giving method/body errors
 // precedence: a `GET`/`HEAD` + streamed body yields the method-specific message, not the
 // generic "send_all requires buffered bodies"). ONLY THEN the batch-only checks
 // (send_all rejects streamed REQUEST bodies and streamed RESPONSES). Fastly must not skip
 // the validator and check the batch-only rejection first — that would (a) drop the shared
 // canonicalization/host/URI validation and (b) invert the documented precedence.
    let reqs: Vec<Result<OutboundRequest, EdgeError>> = reqs.into_iter()
        .map(|req| {
            validate_for_dispatch(&req)?;   // FIRST — shared validator, method/body precedence
            if req.is_stream_body() {       // THEN batch-only: send_all is buffered-only
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
 // dispatch returns Err for an expired/zero deadline so those slots
 // never enter Phase 2. The host connect/first-byte/between-bytes timeouts are
 // set from budget.duration; budget.deadline governs the body-phase cooperative
 // check below.
    let mut slots: Vec<Slot> = reqs.into_iter()
        .map(|maybe_req| match maybe_req {
            Err(e)  => Slot::Done(Err(e)),
            Ok(req) => {
 // Capture the method BEFORE `dispatch` consumes `req` — harvest needs it for the
 // §3.4.1 no-content (HEAD) rule; `wait()`/`poll()` surface only the response.
                let req_method = req.method().clone();
                match dispatch(req, batch_now) {
 // dispatch(req, now) -> Result<(PendingRequest, DispatchBudget, u64), EdgeError>
 // (the `u64` is the per-slot response `max_bytes` — u64 cap, not usize; see §3.1.3)
 // where the third field is max_bytes from ResponseMode::Buffered.
                    Ok((pending, budget, max_bytes)) => Slot::Pending(PendingSlot {
                        pending, budget, max_bytes, req_method,
                    }),
                    Err(e) => Slot::Done(Err(e)),
                }
            },
        })
        .collect();

 // Phase 2 — harvest. wait blocks on one slot; siblings keep progressing at
 // the host. For the headers phase, wall-clock is ~max(header_arrivals), not
 // the sum. Buffered body drain runs *serially* in harvest order, so total
 // wall-clock is ~max(header_arrivals) + Σ body_drain_times — see
 // "Buffered body drain runs in harvest order". poll opportunistically
 // collects siblings that already finished headers. Only Buffered responses
 // reach this point — Streamed responses were rejected in Phase 0 preflight.
    let mut out: Vec<Option<Result<OutboundResponse, EdgeError>>> =
        (0..n).map(|_| None).collect();
    for i in 0..n {
        match std::mem::replace(&mut slots[i], Slot::Taken) {
            Slot::Done(r)     => out[i] = Some(r),
            Slot::Taken       => { /* already harvested by an earlier poll() */ }
            Slot::Pending(s)  => {
                out[i] = Some(harvest(s.pending.wait(), &s.budget, s.max_bytes, &s.req_method));
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
                            PollResult::Done(r)      => out[j] = Some(harvest(r, &s2.budget, s2.max_bytes, &s2.req_method)),
                            PollResult::Pending(pr2) => slots[j] = Slot::Pending(PendingSlot {
                                pending: pr2,
                                budget: s2.budget,
                                max_bytes: s2.max_bytes,
                                req_method: s2.req_method,
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
- **Cancellation / drop semantics.** Fastly exposes no async-cancellation primitive
  for an in-flight `PendingRequest`, and Phase 2 harvests with **blocking** `wait()` /
  `poll()` (no `.await` between dispatch and completion), so `send_all` has no interior
  suspension point at which the future could be dropped mid-harvest — once Phase 1
  returns, the loop runs synchronously to completion. Two consequences the contract
  guarantees: (a) **every dispatched `PendingRequest` is always harvested** — the
  Phase 2 invariant (line above) resolves each slot, so no `PendingRequest` is ever
  leaked un-`wait()`-ed. The **only** deliberate drop-without-`wait()` is the
  streamed-upload budget-exhausted path (§5.4 "Upload consumes the budget on Fastly"),
  which drops the `StreamingBody` + `PendingRequest` intentionally. (b) **A sibling
  slot's deadline firing does not abort other slots** — each slot's budget is enforced
  independently by its own dispatch-time host timeouts plus the per-slot cooperative
  `budget.deadline` check, never by cancelling a neighbour. The cross-slot effect is
  strictly a *harvest-order delay* (§3.3.4 / §8 risk 8), not cross-slot cancellation.
- **Dynamic backends.** Arbitrary HTTPS hosts use Fastly dynamic backends
  (`Backend::builder`). Per Fastly's
  [`BackendBuilder` docs](https://docs.rs/fastly/latest/fastly/backend/struct.BackendBuilder.html),
  the **session-uniqueness rule is unconditional** — a dynamic backend name must
  not match the name of any static service backend nor any other dynamic backend
  built during this session. `NameInUse` carries no property-comparison
  semantics: the SDK signals only "this name is taken in this session," and its
  documented recovery (`Backend::from_str(name)`) returns a handle without
  exposing the registered properties. EdgeZero therefore owns the entire
  uniqueness story **at the guest layer**: a **session-scoped** adapter-local cache
  (a per-session `Mutex<HashMap<String, (BackendIdentity, Backend)>>` **field on the
  per-request `FastlyOutboundClient`** — `Mutex` not `RefCell`, because the trait is `Send + Sync` — fresh each request/session; see *Cache
  ownership* below for why a per-request field is correct and a cross-request cache is wrong) holds the identity →
  backend mapping, and a hit
  reuses the cached `Backend` while a miss calls `Backend::builder(..).finish()`
  exactly once. Because EdgeZero hashes every relevant property into the
  backend name (`ez_{sha256_128(identity)}`), distinct identities map to
  distinct names — so a 50 ms slot and a 3 s slot to the same host get distinct
  backends by construction, not by SDK-side property comparison. A
  `NameInUse` on a name **not** in the adapter's collision map can therefore
  only mean an externally-registered backend (a static service backend, or another
  component in **this** session — NOT a prior session, whose names are gone) is squatting the name — fail-closed `EdgeError::internal` because
  the SDK does not let us prove identity match. The precise collision-detection
  protocol is in the §4.3 algorithm later in this section.

  Identity tuple:
  `scheme + ":" + host + ":" + resolved_port + ":" + tls_mode + ":" + budget_ms`,
  where:
  - `resolved_port` is the URI port or scheme default (`80`/`443`).
  - `tls_mode` is `"tls"` for `https` or `"plain"` for `http`.
  - `budget_ms` is the **exact true-ceil-to-ms** of `dispatch_budget(req).duration` —
    `((duration.as_nanos() + 999_999) / 1_000_000).max(1)`, lint-clean form in the
    `fastly_timeout_ms` helper. **No bucketing.** The cache is **per-session** (below),
    so it cannot grow unbounded across requests — earlier drafts bucketed the budget to
    bound a *cross-request* thread-local cache, but that cache model was wrong (Fastly
    backend names are **session-scoped**, verified against `BackendBuilder` docs — a new
    request is a new session with a fresh namespace). Within one session a `send_all`
    shares a single `batch_now`, so same-budget slots to the same host compute the
    **same** `budget_ms` → one backend; distinct budgets → distinct backends. The cache
    is therefore bounded by the number of distinct `(host, budget)` pairs in the
    session's fan-out (≤ batch size), with **no** millisecond drift to bucket away.
  - The **host timers use `budget_ms` exactly** (`connect-timeout` /
    `first-byte-timeout` / `between-bytes-timeout` derive from the exact value), so the
    headers phase is bounded by `budget.duration` (+ ms-rounding + `BATCH_DISPATCH_SLACK_MAX`),
    **not** a looser bucket. The body phase is additionally enforced to the millisecond by
    the cooperative `budget.deadline.is_expired()` check against the original `Deadline`.
    Removing the bucket removes the ~10 % headers-phase overshoot an earlier draft
    introduced; the net guarantee is again exactly what §4.3 "Net guarantee" states.
  - `budget_ms` is the **true ceil-to-ms** — `((duration.as_nanos() + 999_999) /
    1_000_000).max(1)` (lint-clean form in `fastly_timeout_ms`), since `as_millis()`
    floors and would make the host timeout too tight. (Apps wanting sub-ms wall-clock
    should not target Fastly — host timeouts are millisecond-granular.) §3.3.4's "host
    timeouts = `budget.duration`" is an abbreviation for "= ceil-to-ms of `budget.duration`".

  A 50 ms slot and a 3 s slot to the same host get **distinct** backends (distinct
  `budget_ms` → distinct identity → distinct name) — they must, since their host timeouts
  genuinely differ. Within one `send_all`, same-budget slots share `batch_now` → identical
  `budget_ms` → one backend.

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

  **Dispatch-overhead slack — hard-bounded for a CACHED backend, fail-closed-*detected*
  for first-time registration** (see the honesty note after the bullets). Because `batch_now` is captured
  *before* preflight, dynamic-backend creation, and `send_async`, the `budget_ms`
  baked into the backend identity is a *snapshot* timeout (computed from `batch_now`) —
  not the exact remaining wall-clock at the moment the SDK timer is armed. The Fastly host enforces
  `budget_ms` from the moment it sees the request, so a request can in principle
  complete up to `(now_at_send_async − batch_now) ms` after the absolute fan-out batch
  deadline before the host fires its timeout. To keep this slack
  **deterministically bounded** for warm/cached adapter setup and host-timer arming (not
  request-body transmission, whose write gap remains unbounded). The capability itself is
  `BestEffort` because both the cold path below and non-empty request writes have no such
  bound:

  - The adapter caps `(now_at_send_async − batch_now)` at
    `pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);`
    (defined alongside `DEADLINE_FAR_FUTURE` in `src/time.rs`, §7).
  - Before each slot's `send_async`, the adapter checks two things **in this order**:
    **(1) the absolute deadline FIRST** — `if budget.deadline.is_expired() { return
    Err(EdgeError::gateway_timeout_caused("deadline expired during Fastly dispatch",
    budget.cause)); }`. A cold `Backend::builder(..).finish()` can block past the deadline
    (§4.3 honesty note), and when it returns *after* the deadline, that is a genuine
    **504 timeout**, NOT an EdgeZero bug — so it must be an attributed `gateway_timeout`,
    never `internal`. **(2) THEN, only if time still remains** (`!is_expired()`) but the
    adapter overhead exceeded the slack, `Instant::now() - batch_now >
    BATCH_DISPATCH_SLACK_MAX` → the remaining slots fail closed with
    `Err(EdgeError::internal("Fastly send_all adapter overhead between batch_now \
     and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) \
     exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale \
     duration"))`. So `internal` is reserved for **excess adapter overhead while the
     deadline still had time** — a real "our setup is too slow" signal — and never for an
     actual expiry. This is an internal diagnostic about **adapter-side** work,
    not a handler-side complaint — handler code runs before `send_all` is even
    invoked, so it runs before `batch_now` is captured and cannot exhaust this
    budget. The interval measured here is adapter overhead: per-slot preflight
    validation, dynamic-backend lookup/creation host calls, and SDK setup
    before `send_async`. If this fires in production, the operator looks at
    backend-creation hostcall latency or a noisy neighbour, not at handler
    code.
  - The cooperative `budget.deadline.is_expired()` check during body drain still
    catches body-phase overshoot per §3.3.4 (one between-bytes-timeout bound).

  **Honesty note — what the guard can and cannot do.** The check above runs
  *immediately before* `send_async`, i.e. **after** any `Backend::builder(..).finish()`
  in this slot has already returned. It therefore **detects** an overshoot; it cannot
  **preempt** one. That distinction splits the claim in two:

  - **Cache hit (no `finish()` call).** The measured interval is pure adapter compute —
    preflight, a map lookup, SDK setup. It is genuinely bounded, and the setup/arming
    portion holds the stated `BATCH_DISPATCH_SLACK_MAX + ms_rounding` bound. This does not
    turn a body-bearing exchange into a finite end-to-end bound because the subsequent
    host write remains untimed.
  - **First-time dynamic-backend registration.** `finish()` is a **synchronous host
    call that can block** — Fastly may make it wait for a service-wide dynamic-backend
    slot. Nothing guest-side can interrupt it. If it blocks past the deadline, the
    wall-clock overshoot has *already happened* by the time the guard runs — so the guard
    **checks the absolute deadline FIRST and returns an attributed `gateway_timeout` (504)**
    (the honest outcome for a real expiry). It does **not** call this `internal`: `internal`
    is reserved for the *other* case — adapter overhead exceeding the slack **while the
    deadline still has time** (a genuine "our setup is too slow" bug), never for a
    time-that-actually-ran-out. **On this path the dispatch+headers phase is `BestEffort` for
    wall-clock, not bounded** — the guard detects the overshoot and surfaces it as a 504, it
    does not prevent it.

  So the honest one-line statement, which the matrix footnote and §5.4 rows must match:
  *Fastly declares `outbound-deadlines` = `BestEffort`. The warm/cached path enforces a
  documented bound, but a first-time `finish()` registration can overshoot the deadline; the
  guard checks the absolute deadline first and returns an **attributed `gateway_timeout`
  (504)** for that expiry (NOT `internal` — `internal` is reserved for adapter overhead
  exceeding the slack while time still remains), and `capability()` cannot tell warm from
  cold — so the static value is `BestEffort`, and a `required outbound-deadlines` hard-fails
  on Fastly.* Apps
  needing an exact absolute deadline on the dispatch+headers phase — including the first
  request to a new host — target Axum or Cloudflare. Spin also avoids Fastly's blocking
  registration step, but its cooperative host teardown keeps `outbound-deadlines`
  BestEffort (footnote 8).

  Net guarantee, with the explicit **sub-4 ms branch** broken out separately. **Both
  branches below hold on the WARM path only — an already-registered (cached) backend, no
  `finish()` in the measured interval.** On the first request to a new host,
  `Backend::builder(..).finish()` can block unboundedly (per the honesty note above), so
  these equations do NOT apply and the phase is `BestEffort`; that is exactly why the
  capability is `BestEffort` (footnote 1). The bounds:

  - **`total_ms ≥ 4` (the common case), cached backend**: a Fastly slot can complete at most
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
  **same TWO-CHECK guard in the SAME ORDER as `send_all`** applies (§4.3 — this was
  inconsistent before; corrected): immediately before `send_async`, the adapter checks
  **(1) the absolute deadline FIRST** — `if budget.deadline.is_expired() { return
  Err(gateway_timeout_caused("deadline expired during Fastly dispatch", budget.cause)); }`
  (a cold `finish()` returning past the deadline is a genuine **504**, not an EdgeZero bug);
  **(2) THEN, only while time remains**, `Instant::now() - now > BATCH_DISPATCH_SLACK_MAX`
  → `EdgeError::internal(..)` with the same "adapter overhead between dispatch_budget and
  SDK arming" diagnostic as `send_all`. So single `send` returns the attributed **504** for a
  real expiry and `internal` only for excess overhead with time to spare — identical to
  `send_all`. The slack window is typically narrower for single `send` (no per-slot harvest
  loop), but the bound is the same hard constant; the previous "structurally 0" wording was
  incorrect. The phase-budget split and sub-4 ms branch apply identically.

  §5.4 has a row that locks this. The test cannot use a handler-side sleep before
  `send_all` — that runs *before* the adapter captures `batch_now`, so it never
  exercises the slack guard. The test instead uses an **adapter-internal injection
  hook** (a **`#[cfg(feature = "test-utils")]`** `Fn` slot on `FastlyOutboundClient`
  invoked between `batch_now` capture and per-slot `dispatch()`) to introduce a
  synthetic delay exceeding `BATCH_DISPATCH_SLACK_MAX`. **It must be feature-gated,
  not `#[cfg(test)]`** — `tests/contract.rs` is an external integration test and
  compiles the adapter *without* `cfg(test)`, so a `#[cfg(test)]` hook would be
  invisible to it (§5.5 *Executable test seams*). With the hook set, late slots return
  `internal("Fastly send_all adapter overhead between batch_now and SDK arming \
   (preflight + dynamic-backend lookup/creation + SDK setup) exceeded \
   BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration")`;
  without it, no slot ever returns that error. Apps that need exact
  absolute-deadline enforcement on the dispatch+headers phase target Axum or Cloudflare.
  Spin also arms from `budget.deadline.remaining()` (§4.4 step 3), but does not claim a
  finite host-teardown bound. **Collision detection** is
  belt-and-suspenders.

  **Cache ownership — a PER-SESSION map (a `Mutex` field on the per-request client).**
  This is the single authoritative statement; it governs the protocol below and the
  §4.3 *Dynamic backends* discussion. **Fastly dynamic-backend names are
  session-scoped, NOT global across requests** (verified against the `BackendBuilder`
  docs: dynamic-backend *registration* is per-session — each session registers into its
  **own** namespace, so a `NameInUse` only fires within the current session; same-name
  backends do **not** pool or carry over across sessions). Every inbound request is a
  **new session**: `FastlyOutboundClient`
  is constructed per request (`crates/edgezero-adapter-fastly/src/request.rs`), it gets
  a **fresh** cache, and the session's backend namespace is **also fresh** — so request
  #2 re-registering `ez_abc` does **not** collide with request #1's registration (that
  was a different session). The cache is therefore correctly a **field on the per-request
  client**, and its lifetime matches the session by construction:

  ```rust
  struct FastlyOutboundClient {
      // Per-request/per-session dedup map. MUST be Mutex, not RefCell:
      // OutboundHttpClient: Send + Sync (stored as Arc<dyn ..> in http::Extensions),
      // and RefCell is !Sync. The Mutex is uncontended on the single-threaded WASM
      // guest — it satisfies the Sync bound, it does not serialize anything.
      backends: Mutex<HashMap<String, (BackendIdentity, Backend)>>,
      // …
  }
  ```

  **This reverses two earlier drafts wrong for the same root reason** — the
  belief that names persist across requests. One made the cache a cross-request
  `thread_local!` (which would carry **stale** entries into a reused instance's next
  session, where those names are unregistered); another bucketed the budget to bound
  that non-existent cross-request growth. Neither is needed: the map exists only to
  **dedup within a single session's fan-out** (multiple `send_all` slots / multiple
  `send`s in one handler to the same host+budget reuse one registration), so its size
  is bounded by the fan-out, and it is discarded when the request ends. The **test seam**
  (§5.5) exposes this client field.

  The protocol below takes the map's **uncontended `Mutex`**. To state the one model
  plainly, because earlier drafts said three different things: the `Mutex` exists solely
  to satisfy `OutboundHttpClient: Send + Sync` (the handle is stored as
  `Arc<dyn OutboundHttpClient>` in `http::Extensions`) — **not** to serialize anything.
  The Fastly guest is single-threaded, so no two `send_one` bodies interleave during a
  synchronous host call, and **the lock is never held across a host call** (step 3).
  (Read any surviving "no lock", "thread-local", or "lock held through `finish()`"
  phrasing here as superseded by this paragraph.)

  1. Lock the map (`self.backends.lock()` — uncontended; handle any poison by treating a poisoned lock as an internal error, never by unwrapping in production).
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
       explicit identity comparison here. Release the borrow. §5.4 has a row that
       exercises this path via an injectable hash collision under the `test-utils` feature
       (**not** `#[cfg(test)]` — see §5.5 *Executable test seams*).
  3. Otherwise (name is absent), **release the lock**, then call
     `Backend::builder(..).finish()`. The lock is **not** held across this host call:
     `finish()` registers a dynamic backend and can block waiting for a service-wide
     backend slot (see *Dispatch-phase deadline honesty* below), and holding a lock
     across a potentially-blocking host call is a hazard for any future multi-threaded
     host. Releasing it does **not** reintroduce the same-identity race an earlier draft
     worried about: the guest is single-threaded and `finish()` is a synchronous
     (non-`await`) call, so no other `send_one` can run in the gap — a `NameInUse` from a
     genuine miss is therefore still unambiguously external (step 5).
  4. On `Ok(backend)`: **re-acquire the lock**, insert `(identity, backend.clone())`
     into the map, drop the lock, and return the `Backend`.
  5. On `Err(NameInUse)`: per Fastly's
     [`BackendBuilder` docs](https://docs.rs/fastly/latest/fastly/backend/struct.BackendBuilder.html),
     the **session-uniqueness rule is unconditional** — "a dynamic backend name
     must not match the name of any static service backend nor match any other
     dynamic backend built during this session." `NameInUse` does **not** carry
     property-comparison semantics ("same identity → returns Ok" was a false
     premise in earlier drafts); the SDK signals only "this name is taken in
     this session," period. The SDK's documented recovery pattern is to call
     `Backend::from_str(name)` (alias `Backend::from_name`) to obtain a handle
     to the already-registered backend — but `from_str` returns a handle only
     and **does not expose the registered backend's properties** to the guest
     for comparison.

     The lock is released across `finish()` in step 3 and re-acquired in step 4 (not
     held continuously — see *Cache ownership*), but the reasoning is unaffected because
     the Fastly guest is **single-threaded** and `finish()` is a synchronous
     (non-`await`) call: nothing else can run in the gap, so any
     name *we* registered in this session necessarily showed up in step 2's borrow. A
     `NameInUse` here therefore means the name was registered by an **external
     party in this same session**: a static service backend, or another component
     of this instance (**not** a prior session — dynamic-backend names are
     session-scoped and a fresh session starts with a clean namespace). Since the
     SDK does not let us inspect that external
     backend's properties, we cannot prove its identity matches ours. Fail
     closed with `EdgeError::internal("Fastly Backend::builder returned
     NameInUse for a name not in this adapter's collision map; the SDK does
     not expose the externally-registered backend's properties, so we cannot
     prove identity match — refusing to dispatch to a backend with possibly
     mismatched TLS / timeout / SNI configuration")`. Release the borrow.

     The alternative — falling back to `Backend::from_str(name)` and trusting
     the external registration — is exactly the "you should be careful to only
     use this capability in situations in which you are 100% sure that this
     name will always lead to the same place" caveat that Fastly's docs
     attach to `from_str`. Since EdgeZero owns the `ez_{sha256_128(identity)}`
     naming scheme, a `NameInUse` for a name **absent from this session's map**
     can only mean one of: (a) a **static service backend** is configured with
     that name (the SDK's uniqueness rule spans static + dynamic within the
     session), or (b) another component **in this same session** registered it,
     or (c) a SHA-256-128 collision (vanishingly unlikely given the 128-bit
     identity space). None is safe to silently inherit.

     > Note: it canNOT mean "a prior session registered it." Dynamic-backend
     > names are **scoped to the session** and may overlap freely across
     > sessions/instances — a fresh session starts with a clean namespace. An
     > earlier draft attributed the collision to a prior session / another
     > EdgeZero deployment sharing an edge dictionary; that is not how the
     > lifetime works. This matters for the session-scoped cache above: the map
     > and the host's name registry share **exactly** the same lifetime (the
     > session), which is precisely why the map is a reliable mirror of it — and
     > why a *cross-request* (process-persisted) map was not: a longer-lived map would
     > carry stale names into a new session whose host registry has been cleared, so it
     > would stop mirroring the registry. (The map here is per-session/per-request — the
     > correct, shorter lifetime; the rejected alternative is the *cross-request* one.)
  6. On any other `Backend::builder(..).finish()` error — i.e. a
     **`BackendCreationError`** — **map per the exhaustive stage-1 table below, NOT
     with a blanket `bad_gateway`.** This is the one authoritative mapping; earlier
     prose here said "map every other creation error to `bad_gateway`", which is wrong:
     `ConnectTimeoutTooLarge` / `FirstByteTimeoutTooLarge` / `BetweenBytesTimeoutTooLarge`
     / `NameTooLong` / `EncodingError` mean **EdgeZero** violated its own clamp/naming
     invariant → **`internal` (500)**, not a 502. Only genuine host rejections
     (`Disallowed`, `HostError`) are `bad_gateway`. `Disallowed` gets the dedicated
     "enable dynamic backends" diagnostic. (`Backend::builder` is `#[non_exhaustive] =
     false`, so the match is exhaustive with **no** `_` arm — a future SDK variant is a
     compile error to be classified deliberately.)

     > **DNS / TLS / connect failures do NOT occur at this stage.** The SDK separates
     > **`BackendCreationError`** (registering a backend — this step) from
     > **`SendErrorCause`** (actually performing the exchange). Registration does not
     > resolve DNS or complete a TLS handshake; those happen on **send**, and are
     > mapped by the send-stage `SendErrorCause` table below. An earlier draft listed
     > "DNS resolution failure / TLS misconfiguration" here — that is wrong, and it
     > made the corresponding §5.4 test unwritable (a fake *builder* cannot produce a
     > DNS branch). Test the two stages against their own error types.
     `EdgeError::internal` is reserved for **adapter contract bugs** — invariant
     violations the adapter itself should have prevented (the unfilled-slot case
     in the harvest loop, the `BATCH_DISPATCH_SLACK_MAX` overshoot, this
     section's `NameInUse` external-registration case). Release the borrow.

  **Backend *creation* errors are not the transport errors.** The list above covers
  `Backend::builder(..).finish()` — i.e. *registering* a dynamic backend. **DNS, TLS,
  and connection failures do not surface there**; they arrive later, from the **send**
  itself, as a `SendError` whose `SendErrorCause` names the failure. Mapping the two
  stages together (as earlier drafts did) would mislabel a connect failure as a
  "backend setup" error. The normative **send-stage** mapping — applied at
  `pending.wait()` / `poll()` in the harvest loop and on the single-`send` path,
  replacing today's blanket `EdgeError::internal(..)` on `wait()` failure:

  **Per-variant policy.** The variant names below are the real `fastly` 0.12.1 enums
  (`backend::builder::BackendCreationError`, `http::request::SendErrorCause`). **A
  blanket "anything else → 502" is wrong**: several variants mean *EdgeZero* violated
  its own invariant, and reporting those as an upstream gateway failure hides an adapter
  bug behind a plausible 502.

  > **⚠️ The two enums have OPPOSITE properties — verified against the SDK source, and
  > this drives both the code and the tests:**
  >
  > | | `BackendCreationError` | `SendErrorCause` |
  > | --- | --- | --- |
  > | `#[non_exhaustive]`? | **No** | **Yes** |
  > | Exhaustively matchable by us? | **Yes** — omit a `_` arm so a future SDK variant is a **compile error** | **No** — a `_` arm is *mandatory*; new variants silently fall through |
  > | Constructible in a test? | **Yes** (`PartialEq` too) | **No** |
  >
  > So: match `BackendCreationError` exhaustively (no `_`). For `SendErrorCause` an
  > exhaustive match is **impossible** — an earlier draft's instruction to "match them
  > exhaustively so a future variant is a compile error" is unachievable there, and the
  > mandatory `_` arm is exactly why its default must be the *narrow* `Custom`-style
  > 502 rather than a blanket. `SendError` is likewise unconstructible (private fields,
  > no public ctor), though `SendError::root_cause() -> &SendErrorCause` lets the
  > adapter read the cause. See *Send-stage test seam* below for how this is tested.

  **Stage 1 — `BackendCreationError` (registration):**

  | Variant | EdgeError | Status | Why |
  | --- | --- | --- | --- |
  | `Disallowed` | `bad_gateway` | 502 | Dynamic backends off on the service — operator action; carries the dedicated diagnostic. |
  | `NameInUse` | *(not an error here)* | — | Handled by the cache protocol above (identity compare → reuse or fail closed). |
  | `ConnectTimeoutTooLarge`, `FirstByteTimeoutTooLarge`, `BetweenBytesTimeoutTooLarge` | **`internal`** | **500** | **EdgeZero bug.** The `DEADLINE_FAR_FUTURE` clamp (7 d) plus `fastly_timeout_ms`'s u32-ms clamp exist precisely to make these unreachable. Reaching one means our clamp is broken — not an upstream failure. |
  | `NameTooLong` | **`internal`** | **500** | **EdgeZero bug.** We own the naming scheme (`ez_` + 32 hex = fixed length); too-long is impossible unless the scheme changed. |
  | `EncodingError` | **`internal`** | **500** | **EdgeZero bug.** We construct the backend strings; invalid UTF-8 is ours. |
  | `HostError(FastlyStatus)` | `bad_gateway` | 502 | Genuine host-side rejection we don't control. |

  **Stage 2 — `SendErrorCause` (the exchange):**

  | Variant(s) | EdgeError | Status |
  | --- | --- | --- |
  | `DnsTimeout`, `ConnectionTimeout`, `HttpResponseTimeout` | `gateway_timeout` | **504** |
  | `DnsError`, `DestinationNotFound`, `DestinationUnavailable`, `DestinationIpUnroutable`, `ConnectionRefused`, `ConnectionTerminated`, `ConnectionLimitReached` | `bad_gateway` | 502 |
  | `TlsProtocolError`, `TlsCertificateError`, `TlsAlertReceived`, `TlsConfigurationError` | `bad_gateway` | 502 |
  | `HttpIncompleteResponse`, `HttpResponseHeaderSectionTooLarge`, `HttpResponseBodyTooLarge`, `HttpResponseStatusInvalid`, `HttpUpgradeFailed`, `Http2StreamError`, `HttpProtocolError` | `bad_gateway` | 502 — malformed/oversized **upstream** response. |
  | `IoError`, `ImageOptimizerUnsupported` | `bad_gateway` | 502 |
  | `HttpRequestUriInvalid`, `HttpRequestCacheKeyInvalid`, `HttpCacheLimitExceeded`, `HttpCacheApiUnsupported` | **`internal`** | **500** — **EdgeZero bug.** We build the request/URI and don't use the cache API; a *locally-invalid request* is ours, not the upstream's. (§3.1.3/§3.1.4 validate the URI long before dispatch.) |
  | `InternalError(..)` | **`internal`** | **500** — the SDK's own unexpected host-internal fault: not the *origin's* failure (so not 502) and not a malformed request WE built (so not kind-(i)), but a platform-internal error. `internal` (500) covers this kind-(ii) fault per the broadened taxonomy (§5.4). |
  | `Custom(..)` | `bad_gateway` | 502 — unknown/extension cause; the only defensible default, and it is *narrow* rather than a blanket. |

  **Send-stage test seam — classification must NOT take `SendError`/`SendErrorCause`.**
  Because neither type is constructible outside the SDK, a test cannot fabricate one, so
  a §5.4 row demanding "one test per `SendErrorCause`" is **unwritable** against the SDK
  types directly. Split classification in two:

  ```rust
 // 1. A locally-defined, CONSTRUCTIBLE classification of what went wrong.
 // Unit tests build these directly — no SDK types involved.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) enum SendFailure {
      LocalInvariant,    // HttpRequestUriInvalid | HttpRequestCacheKeyInvalid | HttpCacheLimitExceeded | HttpCacheApiUnsupported
      PlatformInternal,  // InternalError: Fastly host/runtime internal fault, not caller input
      Timeout,           // DnsTimeout | ConnectionTimeout | HttpResponseTimeout
      Transport,         // DnsError | Destination* | Connection{Refused,Terminated,LimitReached} | Tls*
      Unknown,           // Custom, and any future #[non_exhaustive] variant
      UpstreamProtocol,  // HttpIncompleteResponse | Http*TooLarge | HttpStatusInvalid | Http2StreamError | ...
  }

 // 2. The POLICY: pure, total, unit-testable, zero SDK dependency. This is what the
 // rows assert — one case per SendFailure, constructed directly. **Takes the budget's
 // `cause`** so its `Timeout` arm can build `gateway_timeout_caused(msg, cause)` — a Fastly
 // host phase-timer firing IS a budget timeout, and the harvest (which holds
 // `PendingSlot.budget.cause`) passes it in. Without this param the timeout branch could
 // only emit `Unspecified`, violating "every budget timeout is caused" (§3.3.2). The other
 // arms ignore `cause`. Tests call `classify(SendFailure::Timeout, BudgetSource::PerCallTimeout)`
 // and assert both the 504 AND the attributed cause.
  pub(crate) fn classify(failure: SendFailure, cause: BudgetSource) -> EdgeError { /* per the table above; Timeout => gateway_timeout_caused(.., cause) */ }

 // 3. The BOUNDARY: the only code that touches the un-constructible SDK enum. A `_`
 // arm is MANDATORY (#[non_exhaustive]); it maps to `Unknown` -> narrow 502.
 // Thin and mechanical by design, because it is the one part unit tests can't reach.
  fn cause_to_failure(cause: &SendErrorCause) -> SendFailure { /* match … , _ => Unknown */ }
  ```

  The §5.4 send-stage rows therefore assert **`classify(SendFailure::X, cause)`** (**Tier 2,
  NOT Tier 1** — `classify` is `pub(crate)` in the Fastly *adapter* crate, so it is an
  in-crate `#[cfg(test)]` unit test there, not a core Tier 1 test; pure, no adapter runtime
  needed, matching §5.4), plus a Tier 3 Viceroy check that a *real* failure
  produces the expected status end-to-end. The `cause_to_failure` map is covered only by
  Tier 3 / review — that is an accepted, documented gap forced by `#[non_exhaustive]`,
  not an oversight.

  **Consequence for the "internal is legal on only three paths" assertion (§5.4):** that
  claim is now **wrong** and must be widened — `internal` is also correct for the
  invariant and platform-internal variants above. The §5.4 row asserts `internal` appears
  **only**
  for: (a) `BATCH_DISPATCH_SLACK_MAX` overshoot, (b) the unfilled-slot harvest
  invariant, (c) the `NameInUse` external-registration case, **(d) the
  clamp/name/encoding `BackendCreationError` variants, (e) `SendFailure::LocalInvariant`,
  and (f) `SendFailure::PlatformInternal`**. Cases (a)–(e) are EdgeZero-invariant
  violations; case (f) is the separately classified Fastly host/runtime internal fault.
  Neither class includes an origin transport/protocol failure.

  **`DnsTimeout` is 504, not 502** — the one genuinely ambiguous cause. It names DNS
  (transport-shaped, which reads 502) but it **is a fired timer**. Classify by
  **"did a timer fire?"**, not by which subsystem reported it. A DNS answer of "no such
  host" is 502; a DNS lookup that ran out of time is 504. Retry policy remains an
  application decision and is not encoded by this mapping.

  Rationale: a fired host timer is a **deadline** outcome (504) and must be
  distinguishable from an upstream that was unreachable (502) — the fan-out caller
  retries those differently. `EdgeError::internal` **is** correct for a **narrow** set
  of send-stage causes: the `SendFailure::LocalInvariant` group
  (`HttpRequestUriInvalid`, `HttpRequestCacheKeyInvalid`, `HttpCache*`) means EdgeZero
  built an invalid request, while `SendFailure::PlatformInternal` represents Fastly's
  explicit host/runtime `InternalError`. It is **never** correct for a
  *transport/upstream* cause
  (those are 502/504). (An earlier draft said `internal` is never correct for any
  send-stage failure — that contradicts the `LocalInvariant` row of the §4.3 table,
  which is authoritative.) A completed exchange, including any non-2xx, is `Ok`.

  **`BackendCreationError::Disallowed`** (dynamic backends not enabled on the
  service) is the one creation error that gets its own diagnostic rather than a bare
  502: map to
  `EdgeError::bad_gateway("Fastly dynamic backends are disabled on this service; enable them or declare static backends — see §4.3 'Service prerequisite'")`,
  so the operator gets an actionable message instead of a generic bad-gateway.

  There is no `BackendSlot::Building` / `Failed` variant and no condvar. There **is** an
  uncontended `Mutex` (the field is `Mutex<HashMap<..>>`, required for `Send + Sync`), but
  it exists only to satisfy that bound — the Fastly guest is **single-threaded**, so there
  is no concurrency for a state machine to guard, and **the lock is never held across the
  `finish()` host call** (the *Cache ownership* protocol releases it, then re-acquires to
  insert). Nothing can observe an intermediate state because nothing else runs while the
  lock is held, and the lock isn't held across the one call that could block. The race the
  round-34 review flagged is structurally impossible for that reason. (Earlier drafts
  variously said "no lock", "hold the `Mutex` across the host call", and used finer-grained
  per-name reservations; all are superseded by the single model in *Cache ownership*.) The protocol applies
  to:

  - **`send_all`** — each slot looks up its name; if the name already maps to its own
    identity, reuse; if it maps to a *different* identity, fail closed with
    `EdgeError::internal("dynamic backend name collision — refusing to reuse")`.
  - **Single `send`** — same lookup path; same fail-closed behaviour.
  - **Across calls within ONE request — a per-session `Mutex<HashMap<..>>` field.**
    Fastly dynamic-backend names are **session-scoped**: a new inbound request is a new
    session with a **fresh** namespace, so request #2 re-registering `ez_abc` does NOT
    collide with request #1 (same-name backends do **not** pool or carry over across
    sessions — registration is per-session). The cache is
    therefore a **field on the per-request `FastlyOutboundClient`**, fresh each request —
    it exists to dedup **within** a single session's fan-out (multiple `send_all` slots /
    multiple `send`s in one handler to the same host+budget reuse one registration), and
    is discarded when the request ends. **It MUST be a `Mutex<HashMap<..>>`, NOT a
    `RefCell`:** `OutboundHttpClient: Send + Sync` (the handle stores `Arc<dyn
    OutboundHttpClient>` in `http::Extensions`, which require `Sync`), and `RefCell` is
    `!Sync` — a `RefCell` field would fail to compile. The `Mutex` is **uncontended** on
    the single-threaded WASM guest (it exists to satisfy `Sync`, not to serialize), and
    the lock is never held across a host call. A SHA-256-128 collision against an earlier
    registration in this session is still caught. *(Two earlier drafts got this wrong for
    the same root cause — the false belief that names persist across requests: one made
    the cache a cross-request `thread_local!`, another used a non-`Sync` `RefCell`. Both
    are superseded by this per-request `Mutex` field.)*
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
  This removes unbounded application **source pulls** from Fastly's
  dispatch-all-then-harvest model and removes the cross-slot streamed-response
  deadline-lifetime problem (§3.1.1), identically on every adapter. It does not
  prevent a non-empty buffered body from blocking in Fastly's untimed host-write
  interval; that separate limitation is footnotes 1/4.
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
      is involved. **This removes only the SOURCE-PULL stall.** It does not bound the
      host write: a `Body::Once` is still pushed to the origin in the untimed window
      between `connect` and `first_byte`, so a slow-reading origin stalls a buffered
      upload too. Buffering is not a remedy for the write path — only a different
      adapter is.
    - *Host write* (`StreamingBody::write_all` / `flush()` on a yielded chunk): these
      are synchronous host calls. **Fastly's `between_bytes_timeout` applies only to
      received bytes (the gap between bytes Fastly receives from the origin), not
      to guest-to-origin writes** — see the [Fastly Backend API
      docs](https://www.fastly.com/documentation/reference/api/services/backend/),
      which describe `between_bytes_timeout` as "maximum duration … that Fastly will
      wait while receiving no data on a download from a backend." No published
      Fastly backend-timeout field bounds the host-side write of guest-supplied
      bytes to origin. **BestEffort** for the write phase: a `StreamingBody::write_all`
      whose host TCP buffer is full because origin stopped acking has no
      adapter-configurable timeout. The adapter's only recourse is the
      cooperative `budget.deadline.is_expired()` check **between** chunks (point
      (i)/(ii) below), which catches the deadline only between writes, not during
      a single blocked write. Apps that need real-time enforcement against a slow
      origin **read path** rely on `between_bytes_timeout` once the response body
      starts flowing. Apps that need real-time enforcement against a slow origin
      **write path** must target a different adapter. `max_request_body_bytes` bounds
      bytes and memory, not elapsed write time: even one small `write_all` can block
      without a documented finite bound, so no cap value creates a wall-clock guarantee.
    - *Around each chunk*: the adapter checks `budget.deadline.is_expired()` at
      **two** points per iteration — (i) immediately after `stream.next().await`
      returns and **before** `write_all`, so a `stream.next()` that stalled past
      the deadline and *then* finally yielded cannot still write the chunk it just
      produced; and (ii) after the successful `write_all` / `flush()`, so a write
      that pushed the budget over is caught before the next pull. On expiry at
      either point the `StreamingBody` is dropped without `finish()` and the slot
      returns `gateway_timeout`.

    Net: the capability matrix entry `streamed-upload-deadlines = BestEffort` for
    Fastly reflects **both** phases — **source-stream yield AND host write are each
    unbounded** on the guest side. There is **no** `BoundedCooperative` write-side
    bound: `between_bytes_timeout` is documented as **receive-side only** (it bounds the
    gap between bytes *received from origin*) and does **not** bound guest-to-origin
    writes. (An earlier draft claimed the write phase was `BoundedCooperative` via
    `between-bytes-timeout` and that only source-yield was the "worst phase" — both are
    wrong; footnote 2 and §8 risk 7 are correct.) The only adapter-side bound on either
    phase is the cooperative `budget.deadline.is_expired()` check **between** chunks, at
    the two points described above.
  - **Response phase: host timeouts are *not* adjustable mid-flight.** The Fastly
    SDK sets connect / first-byte / between-bytes timeouts once before `send_async`
    (§3.3.4) and does not expose post-dispatch mutation. For
    `send_async_streaming`, dispatch happens **before** chunks are fed, so the
    response-phase host timeouts are locked to the phase-split values computed at
    dispatch (`first_byte_ms` for the headers wait, `between_ms` for inter-chunk
    gaps once the response body flows). After the upload `finish()`es the adapter
    checks `budget.deadline.remaining()` cooperatively before calling `wait()` —
    if `None`, drop the `PendingRequest` and return `gateway_timeout` without
    waiting. **If the upload leaves a tiny positive remaining budget**, the
    cooperative check at this boundary passes, and the host then waits up to
    its dispatch-time `first_byte_ms` for headers even though only the tiny
    remainder of batch budget is left. **The headers wait is bounded by at
    most one dispatch-time `first_byte_ms` interval past `budget.deadline`** —
    a single, one-shot overshoot, not a per-chunk accumulator.

    Once headers arrive, the **response body** flows through the cooperative
    deadline-aware wrapper (§4.3 "Streamed-response wrapping"), whose
    `is_expired()` check fires before and after **each** underlying read.
    Because the wrapper checks after the read that delivered the first body
    chunk, and the deadline is already expired by construction in this
    scenario, **the very next deadline-check yields `Err(gateway_timeout)`** —
    the wrapper does **not** wait another `between_bytes_timeout` per chunk
    indefinitely. **Total post-deadline overshoot for a single streamed
    upload + response on Fastly is therefore bounded by `first_byte_ms`
    (the headers wait) plus one `between_bytes_timeout` (the worst-case
    interval during which the host is mid-read of the *first* body chunk
    when the wrapper fires)** — a closed-form bound, not a per-chunk
    accumulator. The previous "plus one between-bytes-timeout per body-chunk
    gap" wording in earlier drafts was wrong; the wrapper preempts after the
    first post-deadline read returns.

    This is a deliberate, documented Fastly-specific behaviour of streamed uploads.
    Passing `Body::Once` removes only the source-pull stall and avoids this post-upload
    timer-staleness shape; it does **not** bound Fastly's host write of those buffered bytes.
    Apps that require a strict end-to-end wall-clock bound for any non-empty upload must
    target an adapter whose upload path is Native.
- `capability()` per §3.5.2: `outbound-http` = `Native`,
  `outbound-header-fidelity` = `Native`, `outbound-deadlines` =
  **`BestEffort`** (footnote 1 — a warm zero-body path has documented partial bounds, but
  every non-empty request retains an unbounded host-write interval and the FIRST request
  to a new host calls `Backend::builder(..).finish()`, which can also overshoot before the
  guard runs; since `capability()` is static and cannot distinguish those paths,
  the honest value is `BestEffort`, so a `required outbound-deadlines` correctly hard-fails
  on Fastly rather than fooling the gate),
  `outbound-flexible-phase-budget` = `BestEffort` (footnote 5 — rigid 1/4 connect +
  3/4 first-byte split per §4.3 can fail a request that would have fit within the
  total budget), `send-all-slot-isolation` = `BestEffort` (footnote 4 — sequential
  cold registration, unbounded non-empty request writes, and buffered response-body
  harvest can each delay sibling slots),
  `streamed-upload-deadlines` = `BestEffort` (footnote 2 — no preemption of a
  stalled `stream.next().await`), `lazy-streamed-response-passthrough` =
  `BestEffort` (footnote 6 — Fastly's `Response::stream_to_client()` is
  incompatible with `#[fastly::main]`, so the default scaffold falls back to
  buffered passthrough; lazy streaming requires a non-`#[fastly::main]` entry).
  This is the exact outbound tuple `Adapter::capability()` returns on Fastly.

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
  via `join_all` over `send_one` (each of which drives the hand-built `wasi:http`
  request + `wasip3::http::client::send` — see below and §4.4); the wasi async reactor
  fans out. Concurrency materialises only under the real Spin/wasi executor — see
  §5.3 for the test consequence.
- `send_one(req, now)`: compute the budget via the core helper
  `dispatch_budget(req, now)` (§3.3.2) before consuming the request into parts, then build
  the hand-built `wasi:http` request (§4.4 — all body kinds, buffered and streamed); race
  the **whole** operation
  (send **and**, in `Buffered` mode, body collect) against a wasi monotonic-clock
  timer for **`budget.deadline.remaining()` at the moment the race starts** —
  *not* the snapshot-time `budget.duration`. The two differ by however long
  preflight + builder construction took since `batch_now`; using `remaining()`
  pins the SDK timer to the absolute batch deadline, matching Axum/CF (§4.1 /
  §4.2 step 3). If `remaining()` is `None`, return `gateway_timeout` without
  issuing the request. Single `send` snapshots `now = web_time::Instant::now()`
  inline.
- **Streamed responses honour the effective-budget deadline — STREAMED MODE ONLY.** This
  is the second phase for `Streamed` mode, where `to_core` returns `Body::Stream` **without
  draining** (the single exchange race covered upload + headers only). Wrap that
  `Body::Stream` with a per-chunk race against a wasi monotonic-clock timer bounded by
  `budget.deadline`; the wrapper yields a `gateway_timeout` (attributed via `budget.cause`, §3.3.2) error chunk past the deadline so
  the streamed body honours the deadline end-to-end per §3.3.3. **BUFFERED mode does NOT get
  this second race:** there, `to_core` `.await`s the full body drain **inside `exchange`**,
  which is already bounded by the single deadline race below — the buffered body is consumed
  once, under one race, never twice. (So the design is: one exchange race covering
  upload+headers+*buffered*-body; for *streamed* bodies the exchange race stops at headers and
  this per-chunk wrapper takes over. No path drains the same body under two races.)
- **ALL uploads — including BUFFERED — use the hand-built `wasi:http` request, not
  `spin_sdk::http::send`.** `spin_sdk::http::send`'s `IntoRequest` conversion spawns a
  **detached, uncancellable** body pump (§4.4) even for a *buffered* `Body::Once`: a
  finite buffer still blocks on **host backpressure** if the origin reads slowly, and
  dropping the `send` future does not cancel that pump — so a buffered upload could block
  past the deadline. Therefore buffered bodies also go through the hand-built request +
  owned in-race pump below (a buffered body is just a one-shot stream of a single
  chunk), which lets the timer select a guest-visible 504 for both body kinds without an
  unowned pump. It does **not** prove bounded host teardown; `streamed-upload-deadlines`
  therefore remains `BestEffort` for Spin (footnote 8).

- **Streamed request bodies — hand-built `wasi:http` request (SDK 6 / WASI 0.3).**

  > **⚠️ Corrected against verified SDK source.** Earlier drafts of this section
  > prescribed a WASI-**0.2** loop — `OutputStream::subscribe()` → `Pollable`,
  > `check_write()` for a permitted byte count, then `write()`. **That API does not
  > exist in Spin SDK 6.** WASI 0.3 deletes `wasi:io` entirely: there is no
  > `pollable`, `output-stream`, `check-write`, or `subscribe` anywhere in `wasip3`'s
  > WIT or in spin-sdk 6's Rust, and `wasi:http@0.3.0`'s `request.new` takes
  > `contents: option<stream<u8>>` (a component-model stream), not an `OutputStream`
  > resource. The old algorithm was not merely deprecated — it was **unimplementable**.
  >
  > **Nor can the upload go through `spin_sdk::http::send`.** Its `IntoRequest` impl
  > (`http_into_wasi_request`) hands a streaming `http_body::Body` to a **detached,
  > uncancellable** pump (`wit_bindgen::spawn`; the runtime's own docs: *"cannot be
  > cancelled or monitored"*). Dropping the `send` future cancels the **subtask** but
  > **not the pump** — so a *stalled source* (precisely what this capability exists to
  > bound) leaves the pump parked in `poll_frame` forever, and the export executor
  > will not exit until spawned tasks drain. That is not a weak timing guarantee; it
  > **pins the component task alive indefinitely**. Routing streamed uploads through
  > the SDK's high-level `send` would make Spin `BestEffort` *and* leak.

  The adapter therefore **builds the `wasi:http` request by hand** and keeps every
  request-body and completion handle inside the raced exchange. This avoids the SDK's
  detached pump, but it does **not** create a documented synchronous host-teardown bound;
  that limitation is why Spin is `BestEffort` for the two deadline capabilities. All of
  this uses public API re-exported by `spin_sdk`: `wasip3`, `wit_stream`, and `wit_future`.

  ```rust
  use spin_sdk::wasip3::{http::{types, client}, wit_stream, wit_future};
  use futures::future::{poll_fn, select, Either};
  use std::task::Poll;

  let (mut writer, contents_rx) = wit_stream::new::<u8>();
  // Dropping an unwritten FutureWriter writes its DEFAULT asynchronously. The default
  // must therefore be failure, never a false clean completion.
  let (trailers_tx, trailers_rx) = wit_future::new(|| {
      Err(types::ErrorCode::InternalError(Some(
          "outbound request body producer dropped before completion".into(),
      )))
  });

  // The wasip3 option/request setters return `Result<(), ()>` (unit error), so bare `?`
  // does NOT compile in an `EdgeError`-returning context; map `()` concretely.
  let bad = |()| EdgeError::internal(anyhow::anyhow!("invalid outbound request component"));
  let opts = types::RequestOptions::new();
  // Set every available host transport/response timer from a nonzero nanosecond
  // snapshot of the remaining effective budget. Ignoring a setter result or leaving
  // first-byte/between-bytes at host defaults could make an independent default timer
  // fire before EdgeZero's deadline while `map_spin_send_err` attributes the result to
  // `budget.cause`. These are fallback bounds; the outer absolute race below remains
  // authoritative and re-reads `remaining()` immediately before `select`.
  let Some(options_remaining) = budget.deadline.remaining() else {
      return Err(EdgeError::gateway_timeout_caused(
          "deadline expired before upload setup", budget.cause));
  };
  // `Deadline` is clamped to seven days, so this conversion cannot overflow u64;
  // keep the checked conversion so violating that invariant fails locally and loudly.
  let transport_ns = u64::try_from(options_remaining.as_nanos())
      .map_err(|_| EdgeError::internal(anyhow::anyhow!(
          "outbound deadline exceeds WASI duration range"
      )))?
      .max(1);
  opts.set_between_bytes_timeout(Some(transport_ns)).map_err(bad)?;
  opts.set_connect_timeout(Some(transport_ns)).map_err(bad)?;
  opts.set_first_byte_timeout(Some(transport_ns)).map_err(bad)?;

  // Bound as `wasi_req`, NOT `req` — the WASI request must not shadow the OUTBOUND
  // request `req`, whose `max_request_body_bytes` / `body` we read below.
  let (wasi_req, request_done) =
      types::Request::new(headers, Some(contents_rx), trailers_rx, Some(opts));
  wasi_req.set_method(&method).map_err(bad)?;   wasi_req.set_scheme(scheme.as_ref()).map_err(bad)?;
  wasi_req.set_authority(auth).map_err(bad)?;   wasi_req.set_path_with_query(pq).map_err(bad)?;

 // The pump lives INSIDE the raced future — no `wit_bindgen::spawn`.
 // `max_req` gates the cap by BODY KIND, matching the portable contract
 // (`max_request_body_bytes` applies to `Body::Stream` ONLY, §3.1.3). **`parts` is the
 // `OutboundRequestParts` from `req.into_parts()`** — the adapter is a SEPARATE crate and
 // cannot read `OutboundRequest`'s PRIVATE fields, so it destructures into the pub-field
 // `OutboundRequestParts` first (this also gives it `method`/`uri`/`headers`/`body` used
 // to build `wasi_req` above). `max_req` is **`u64`** (the cap type, §3.1.3): for a
 // streamed body it is `parts.max_request_body_bytes`; for a BUFFERED `Body::Once` —
 // which also rides this pump for cancellability (§4.4), re-expressed as a one-shot
 // single-chunk stream — it is **`u64::MAX`** (NOT `usize::MAX` — the field is `u64`), so
 // the cap is a no-op. A buffered payload is already a bounded in-memory `Bytes`; capping
 // it here would make >8 MiB buffered uploads fail on Spin alone, which no other adapter does.
  let max_req: u64 = match &parts.body {
      Body::Stream(_) => parts.max_request_body_bytes,
      Body::Once(_) => u64::MAX,
  };
  // Force one scheduler boundary after each accepted chunk, even when both the
  // source and host writer are continuously ready. Without this, an async `while`
  // loop over empty/immediately-ready chunks can monopolize one poll and prevent
  // both `client::send` and the outer deadline timer from being polled.
  async fn cooperative_yield_once() {
      let mut yielded = false;
      poll_fn(move |cx| {
          if yielded {
              Poll::Ready(())
          } else {
              yielded = true;
              cx.waker().wake_by_ref();
              Poll::Pending
          }
      }).await
  }

  let upload_deadline = budget.deadline;
  let upload_cause = budget.cause;
  enum PumpCompletion { Complete, ReaderGone }
  let pump = async move {
      let mut sent: u64 = 0;   // u64 accounting vs the u64 cap (`max_req`) — usize is u32
                               // on wasm32, so a usize counter could wrap below the cap.
 // `source` yields `Option<Result<Bytes, EdgeError>>`
 // (the error-type change). The item MUST be unwrapped — a source error is a real
 // failure (`bad_gateway` from the wrapped stream, or a `gateway_timeout` chunk), not
 // a `Bytes`. Dropping it would silently upload a truncated body.
      loop {
          if upload_deadline.is_expired() {
              return Err(EdgeError::gateway_timeout_caused(
                  "deadline expired during request upload", upload_cause));
          }
          let next = source.next().await;                         // cancellable
          // Absolute post-ready check: timeout outranks a chunk, EOF, or source error
          // that becomes ready at the deadline.
          if upload_deadline.is_expired() {
              return Err(EdgeError::gateway_timeout_caused(
                  "deadline expired during request upload", upload_cause));
          }
          let Some(item) = next else { break };
          let chunk: Bytes = item?;                               // propagate source error
 // pre-append cap check against max_request_body_bytes (u64; no `as`, use try_from)
          let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
          if sent.checked_add(chunk_len).is_none_or(|n| n > max_req) {
              return Err(EdgeError::bad_request("request body exceeded max_request_body_bytes"));
          }
 // checked: bare `+=` trips `clippy::arithmetic_side_effects` (denied).
          sent = sent.saturating_add(chunk_len);
          let unwritten = writer.write_all(chunk.to_vec()).await; // backpressure; cancellable
          if upload_deadline.is_expired() {
              return Err(EdgeError::gateway_timeout_caused(
                  "deadline expired during request upload", upload_cause));
          }
          if !unwritten.is_empty() {
              // READER GONE is NOT an error. The origin stopped reading — almost always
              // because it is about to send (or already sent) an EARLY FINAL response
              // (413 Payload Too Large, 401, a redirect, …). Returning an error here
              // would DISCARD that valid response and report 502 instead, violating
              // "a completed exchange, including non-2xx, is Ok". So end the pump
              // cleanly and let `send` surface the response.
              drop(writer);
              let _ = trailers_tx.write(Ok(None)).await;
              return Ok::<PumpCompletion, EdgeError>(PumpCompletion::ReaderGone);
          }
          cooperative_yield_once().await;
      }
      drop(writer);                                               // EOF
      let _ = trailers_tx.write(Ok(None)).await;                  // completion signal
      Ok::<PumpCompletion, EdgeError>(PumpCompletion::Complete)
  };

  // `run_exchange` is an ORDERED state machine, NOT `join!`. `join!` would delay an
  // already-available response behind a stalled source and could let a moot upload
  // override an early final response. `request_done` is the request-transmission result
  // returned by Request::new; it is load-bearing and must not be discarded.
  // `client::send` resolves to `Result<wasip3::http::types::Response, ErrorCode>` — the
  // LOW-LEVEL WASI response, NOT the core `Response`. Each success arm must (a) map the
  // transport `ErrorCode` via `map_spin_send_err`, THEN (b) convert to core via
  // **`to_core`, which is `async`** — `.and_then(to_core)` does NOT compile because WASI
  // body collection is asynchronous. `to_core` wraps status + `Fields`→multi-value
  // `HeaderMap` synchronously, then: **Streamed mode** wraps the body as `Body::Stream`
  // and returns immediately (no drain); **Buffered mode** `.await`s the FULL body drain +
  // incremental decompress + `max_response_bytes` cap + EOF **inside `exchange`**, so the
  // whole collection is bounded by the outer deadline race below (a slow buffered body
  // cannot outlive the deadline). This is the hand-rolled equivalent of the SDK's
  // `Response::from_response`, done here because we bypass the high-level `send` (§4.4).
  // `to_core: async fn(wasip3::http::types::Response) -> Result<Response, EdgeError>`.
  // Upload completion protocol (the normative transition table follows this snippet):
  // - `Uploading` polls one pump step first, then `send`. A ready source/cap/deadline
  //   failure therefore wins over a send result ready in the same poll.
  // - `Complete` transitions to `AwaitingRequestDone`; keep polling `send` for host
  //   progress, but retain any ready result without accepting it until `request_done`
  //   succeeds.
  // - `ReaderGone` transitions to `ReaderGone`; retain but never poll `request_done`, and
  //   wait for the early response/error from `send`.
  // - if `send` is ready after the pump returned Pending, it is authoritative. Drop the
  //   pump and `request_done` before response conversion.
  // - source/cap/deadline failure does not write success to trailers. Dropping
  //   `trailers_tx` writes its default Err while the original EdgeError is returned.
  // `client::send`, `pump`, and `request_done` are owned by one exchange; no detached task
  // exists. Both WASI result sites pass through `map_spin_send_err(err, budget.cause)`.
  let exchange = run_exchange(
      client::send(wasi_req),
      pump,
      request_done,
      budget.cause,
      to_core,
  );

  // Response completion is a separate two-way WASI protocol, not just a trailers await.
  // Create a caller-result future whose default is Err(InternalError), pass its reader to
  // `Response::consume_body`, and retain its writer together with the returned body stream
  // and trailers future. After body EOF, await trailers; only clean trailers permit writing
  // `Ok(())` to the caller-result writer. Stream/trailers failure maps to bad_gateway (502),
  // unless the absolute deadline has expired, in which case deadline-wins produces the
  // attributed gateway_timeout (504). A local deadline/decode/cap failure or early consumer
  // drop leaves or explicitly writes Err, never a false clean completion. Buffered conversion
  // completes this protocol inside `exchange`; Streamed conversion stores all three handles
  // in the `Body::Stream` wrapper until terminal EOF or drop. §5.4 pins clean EOF,
  // truncated-response, and early-drop cases.

 // `remaining` is Option<Duration>, NOT Result — `?` here would not compile in a
 // Result-returning fn. An already-expired budget must become gateway_timeout
 // explicitly, matching the "expiry before dispatch" contract above.
  let Some(race_remaining) = budget.deadline.remaining() else {
      // Attribute via the budget's cause — every budget timeout is caused (§3.3.2).
      return Err(EdgeError::gateway_timeout_caused(
          "deadline expired before upload dispatch", budget.cause));
  };

  match select(pin!(exchange), pin!(spin_sdk::time::sleep(race_remaining))).await {
      // RECHECK the absolute deadline for BOTH arms (deadline-wins, §3.4.1). `select`
      // polls `exchange` first, so on *simultaneous* readiness (exchange completes exactly
      // as the timer fires) it would return a result produced at/after the deadline —
      // whether that result is `Ok` OR a decode/transport `Err`. An expired deadline
      // outranks both, so a completed-at-deadline exchange becomes a 504 timeout, never a
      // success and never a simultaneously-ready 502. A §5.4 boundary test drives exactly-
      // simultaneous readiness (success AND failure) and asserts 504 for each.
      Either::Left((resp, _)) => {
          if budget.deadline.is_expired() {
              Err(EdgeError::gateway_timeout_caused("deadline expired during upload", budget.cause))
          } else {
              resp // Ok(within deadline) or a real Err (e.g. bad_gateway) passes through
          }
      }
      Either::Right(_) => Err(EdgeError::gateway_timeout_caused("deadline expired during upload", budget.cause)),
  }
  ```

  **`run_exchange` state machine (normative).** The helper owns pinned `send`, `pump`,
  and `request_done` futures. “Poll first” is an explicit biased poll order, not merely
  source-code order passed to a fairness-rotating selector.

  | State | Polling and transition | Request-side ownership boundary |
  | --- | --- | --- |
  | `Uploading` | Poll `pump` first for one cooperative step, then poll `send`. `pump = Err(e)` returns `e`; this includes source, cap, and upload-deadline failures. `pump = Complete` transitions to `AwaitingRequestDone` without accepting a simultaneous send result. `pump = ReaderGone` transitions to `ReaderGone`. A send result is authoritative only when that pump poll returned `Pending`. | All three futures remain owned. On authoritative send success/error, drop `pump` and `request_done` before mapping/converting the send result. On pump error, drop `send` and `request_done`; the request trailers writer defaults to failure. |
  | `AwaitingRequestDone` | Poll `request_done` first, then poll `send` when its result has not already been retained. A ready send response/error is stored, not returned, so polling can continue to drive host progress. A `request_done` error is mapped with `map_spin_send_err(err, budget.cause)` and wins over any stored send result. On `request_done` success, consume/drop it, then use the stored send result or continue polling `send`; map its error or run async `to_core` on success. | `send` and any ready low-level response remain owned while transmission completion is established. No response conversion or caller-visible send error occurs before `request_done` succeeds. |
  | `ReaderGone` | Do not poll `request_done`; poll `send` until its response/error is ready. That result is authoritative because the peer ended request consumption. | Retain `request_done` only until `send` resolves to response headers or error, then drop it **before** `to_core`. It is never stored in the response body wrapper. |

  The cooperative pump boundary after every accepted chunk is part of this state machine:
  an immediately-ready or empty-chunk source cannot monopolize one poll, so `send` and the
  outer timer are polled. A pump failure already ready in the same poll wins; a later source
  failure after an authoritative send result is moot. For `Streamed` responses, the returned
  `Body::Stream` owns only response-side body/trailer/caller-result handles described below;
  no request-side writer, pump, or `request_done` handle crosses the header-return boundary.

  **Why this remains `BestEffort`.** Before response headers, dropping `client::send`
  requests cancellation of its canonical-ABI subtask. After headers, that subtask is
  complete; cancellation instead drops/cancels the response stream and trailers handles
  and resolves the response-result protocol with `Err`. These are the correct cooperative
  Component Model operations, but neither establishes synchronous host teardown or a
  one-monotonic-tick bound. In addition, dropping an unwritten `FutureWriter` schedules its
  default write rather than completing synchronously. The timer can select a 504 result,
  but host work may outlive that selection, so both deadline capabilities stay
  `BestEffort` until runtime evidence proves a finite teardown bound.

  **Completion signalling.** Clean request EOF or reader-gone explicitly writes
  `Ok(None)` to the trailers future; every other drop defaults to `Err`. The separate
  `request_done` result is awaited on full upload and retained on reader-gone as described
  above. The `max_request_body_bytes` cap (default 8 MiB) is enforced with pre-append
  checked accounting inside the pump loop, `bad_request` on overflow.

  **`RequestOptions` do not bound the upload.** WASI 0.3 keeps `set-connect-timeout` /
  `set-first-byte-timeout` / `set-between-bytes-timeout`, but these are transport /
  response-side only — the WIT states they are *"separate from any the user may use to
  bound an asynchronous call."* They are **not** a substitute for the race above. Set all
  three to the ceiled remaining effective budget, check every setter result, and let the
  re-read raced timer own the absolute deadline. No host-default response timer remains
  that can fire independently of the configured dispatch budget.

  **This applies to STREAMED response mode ONLY.** In **Buffered** mode `to_core` already
  drained the full response body **inside** `exchange` (bounded by the outer deadline race),
  so there is **no** second response race — the exchange result IS the finished response.
  In **Streamed** mode, `to_core` returned `Body::Stream` without draining, so after the
  exchange completes the adapter re-reads `budget.deadline.remaining()`; if `None`, the
  streamed response is dropped and the slot returns `gateway_timeout` — no response wait;
  otherwise the remaining duration bounds the per-chunk streamed-body wrapper race (the
  §3.3.3 wrapper), so upload time is included in the batch budget rather than added on top.
  (An earlier draft prescribed this second race unconditionally, contradicting the buffered
  single-race flow — corrected to Streamed-only here.)
- Existing gzip/br decompression is kept; decompressed-byte cap enforced incrementally
  (§3.4.1). `Streamed` mode wraps the response body as `Body::Stream`.
- **Errors — `map_spin_send_err(err, cause)` classifies the WASI `ErrorCode`, mirroring
  Fastly's `classify(SendFailure, cause)` (§4.3).** It **takes `budget.cause`** so its five
  timeout variants map to **`gateway_timeout_caused(.., cause)`** (§3.3.2 — every adapter
  timeout is attributed); the caller passes `budget.cause` (`map_err(|e| map_spin_send_err(e,
  budget.cause))`). The `send` future (`wasip3::http::client::send` for all
  body kinds — buffered and streamed both go through the hand-built request, §4.4)
  fails with a `wasi:http` `ErrorCode`. **The match lists EVERY known variant explicitly and
  has NO `_` arm** — so a future SDK/WIT variant **breaks the build** and *forces* the
  implementer to classify it (fail-loud, the point of a security/behaviour-relevant
  classifier). This resolves the earlier waffle ("keep `_` only if reachable, otherwise
  drop"): with `-D warnings`, an `_` arm over an *exhaustive* enum is an
  `unreachable_patterns` error anyway, so the no-`_` exhaustive match is the only form that
  passes. **The one exception is forced by the attribute:** IF the pinned generated binding
  is genuinely `#[non_exhaustive]` (confirm at implementation — do not assume), the compiler
  *requires* a `_`; then keep all explicit variant arms AND add `_ => bad_gateway` (502) as
  the narrow default, and the exhaustive-classifier test still asserts every KNOWN variant.
  Either way there is exactly one deterministic rule, not "drop it maybe." Mapping:
  - **Timeout variants — ALL FIVE** (verified against wasip3): `DnsTimeout`,
    `ConnectionTimeout`, `ConnectionReadTimeout`, `ConnectionWriteTimeout`,
    `HttpResponseTimeout` → **`gateway_timeout` (504)**. **None may fall through to the
    generic 502** (an earlier draft mapped only three, leaking `ConnectionWriteTimeout`
    and `HttpResponseTimeout` to 502). An **exhaustive classifier unit test** against the
    pinned SDK pins every timeout variant to 504 — a fired timer is a deadline outcome,
    distinguishable from an unreachable upstream without prescribing retry policy.
  - DNS resolution failure, connection refused/terminated, TLS/certificate errors,
    destination-not-found/unavailable → **`bad_gateway` (502)**.
  - Caller-controlled request-policy/size failures → **`bad_request` (400)**:
    `HttpRequestDenied`, `HttpRequestBodySize`, `HttpRequestUriTooLong`,
    `HttpRequestHeaderSectionSize`, and `HttpRequestHeaderSize`. These can be caused by
    a requested target or limits exposed through the outbound API; reporting 500 would
    misclassify caller input as an EdgeZero invariant failure.
  - Adapter/core invariant failures → **`internal` (500)**:
    `HttpRequestLengthRequired` (the adapter owns framing),
    `HttpRequestMethodInvalid` / `HttpRequestUriInvalid` (core preflight must reject
    these), request-trailer size errors (EdgeZero emits no request trailers),
    and `ConfigurationError` (the adapter constructed the request options).
  - Host/runtime catch-all `InternalError` → **`bad_gateway` (502)**. WASI defines this as
    the fallback when no specific code fits; it is not evidence that an EdgeZero invariant
    failed. The locally generated default `InternalError` used by request/response
    completion writers is not reclassified through this arm: source/cap/deadline failures
    preserve and return their original typed `EdgeError` while the default only tells the
    host that completion was not clean.
  - **Future/unknown variant handling — consistent with the no-`_` rule above (this bullet
    previously contradicted it; corrected).** **Every variant in the pinned
    `wasi:http@0.3.0` `ErrorCode` is explicitly named** in one of four outcome buckets (504
    timeout / 502 upstream+transport+protocol+host catch-all / 400 caller-controlled /
    500 locally-invalid), so the match is
    exhaustive over the pinned enum with **NO `_`** — a future WIT variant then *breaks the
    build* and forces classification (the intended fail-loud behaviour). The ONLY case with a
    `_` is if the generated binding is confirmed `#[non_exhaustive]` at implementation, which
    the compiler would then *require*; in that single case the forced `_ => bad_gateway`
    (502) is the narrow default. There is no unconditional `_`. The pinned
    enum has ~30+ variants — `DNS-error`, `destination-{not-found,unavailable,IP-prohibited,
    IP-unroutable}`, `connection-{refused,terminated,timeout,read-timeout,write-timeout,
    limit-reached}`, the `TLS-*` errors, the `HTTP-request-*` / `HTTP-response-*` size/format
    errors, `HTTP-response-incomplete`, `HTTP-{upgrade-failed,protocol-error}`,
    `loop-detected`, `configuration-error`, `internal-error`, … Assignment rule: the five
    timeouts → 504; the explicitly named caller-controlled request variants → 400; the
    explicitly named adapter/core invariants → 500; **everything else
    upstream/transport/protocol** → 502. The **exhaustive classifier
    test enumerates every pinned variant** and asserts its bucket, so an added SDK variant
    that we forgot to map trips the test (via a `#[deny(unreachable_patterns)]`-style
    round-trip or an explicit variant list), not silently defaults.
  - The separate wasi-timer we race the exchange against (§4.4) also yields
    `gateway_timeout` on expiry. **request**-body over-cap → `bad_request` (400);
    **response**-body over-cap (decompressed) → `response_too_large` (distinct kind, 502, §3.4.1). Any
    completed exchange (incl. non-2xx) → `Ok`.
- Spin requires `allowed_outbound_hosts`; the adapter renders it from
  `[capabilities.outbound].hosts` per §3.5.4 when generating `spin.toml`.
- `capability()` per §3.5.2 reports the exact seven-cell tuple:
  `outbound-http` = `Native`, `outbound-header-fidelity` = `Native`,
  `outbound-deadlines` = `BestEffort` (footnote 8),
  `outbound-flexible-phase-budget` = `Native`, `send-all-slot-isolation` = `Native`,
  `streamed-upload-deadlines` = `BestEffort` (footnote 8), and
  `lazy-streamed-response-passthrough` = `BestEffort` (footnote 7).
- **Response-out passthrough is buffered (BestEffort), not lazy.** Spin's public
  response surface is `Response<FullBody<Bytes>>` (`SpinFullResponse`, used by
  `AppExt::dispatch` / `request::dispatch*` / `from_core_response` / `run_app`), so
  lazy passthrough would require a breaking public-API migration plus a WASI-0.3
  rewrite — deferred (footnote 7, §8 risk 13). The converter therefore drains the
  wrapped `Body::Stream` to `Bytes` within `SPIN_RESPONSE_STREAM_BUFFER_BYTES`
  (16 MiB); over-cap → `response_too_large` (502, §3.4.1). The hand-built **outbound
  streamed-upload** path above remains the implementation mechanism, with the
  `BestEffort` cancellation classification in footnote 8.

## 5. Test plan

Tests are split by what they can actually prove. A core mock proves portable semantics; an
adapter seam proves conversion/classification; only a live runtime can prove host behavior.

### 5.1 Tier 1 — core contract, no platform runtime

Colocated `edgezero-core` tests use `MockOutboundClient`, scripted streams, and injected
monotonic instants. They must not claim platform cancellation or wire behavior.

Required coverage:

- Builder defaults and overrides, including buffered vs streamed mode, request and response
  caps, timeout, absolute deadline, all HTTP methods, and mutually exclusive body/json
  setters.
- Body constructors prove the non-overlapping error contract: `from_stream` preserves
  typed `bad_gateway`, `gateway_timeout`, and `response_too_large` chunks exactly;
  `from_external_stream` maps arbitrary source errors to `internal`; `stream` remains
  infallible; `Bytes` and the existing buffered input types convert to `Body::Once`.
- `OutboundRequest::from_request` preserves the supplied body and exact method, including
  DELETE and HEAD. Repeated normalization is idempotent and does not rewrite the method.
- URI preflight rejects unsupported schemes, missing authority, userinfo, fragments, and
  invalid method/URI combinations before any adapter work. Canonical accessors cover DNS
  names, default/non-default ports, bracketed IPv6, Host/SNI/certificate values, and never
  panic.
- Request normalization strips standard hop-by-hop fields plus every field nominated by
  each visible `Connection` value, case-insensitively and across repeated field lines.
  Empty or invalid nomination tokens fail the whole request as 400; valid prefixes are not
  partially honored. Native-fidelity raw non-UTF-8 `Connection` is also a 400.
  Cloudflare's portable baseline is tested over the visible normalized strings.
- Valid non-ASCII UTF-8 custom header values survive on Native-fidelity adapters;
  forbidden controls reject at construction, and invalid UTF-8 introduced through
  `headers_mut` is dropped except for the fail-closed `Connection` case.
- Response normalization performs the same hop-by-hop and nomination stripping before
  `Content-Encoding`/`Content-Length` interpretation. `Connection: content-encoding` and
  `Connection: content-length` cannot influence decode/cap logic. Every adapter classifies
  malformed visible nomination syntax as 502; Native adapters also classify malformed
  raw/non-UTF-8 `Connection` as 502. Cloudflare is tested only to its BestEffort raw-header
  fidelity contract.
- `OutboundResponse` construction and `into_parts` round-trip request method, status,
  normalized headers, and body. Adapters apply bodyless handling for HEAD, every 1xx, 204,
  and 304 before content decoding or streaming. For 205, clean EOF completes normally while
  a positive `Content-Length` or any body data frame aborts the remaining native body;
  Spin's caller-result protocol never reports false clean completion. Output bodies are
  empty and framing metadata is normalized according to §3.4.1. `into_response` re-runs
  normalization idempotently rather than owning the first pass.
- Content decoding covers gzip, br, stacked/repeated encodings, mixed case, unknown
  encodings, and a repeated field set with one malformed raw value. Native-fidelity
  adapters preserve/pass through a malformed encoding set as specified; Cloudflare asserts
  only visible-value behavior.
- The decoder error carrier restores the exact original `EdgeError` variant/status/kind.
  Raw decompressor failures become `bad_gateway`; over-cap becomes
  `response_too_large`; no path stringifies a typed error.
- The deadline wrapper around **decoded output** covers: compressed input that stalls before
  first decoded output, stalls after output, stalls at terminal EOF/trailer validation, and
  yields an error concurrently with deadline expiry. It checks every yield and terminal
  result; deadline/cancellation wins the simultaneous terminal race.
- Pre-append accounting rejects a chunk that would exceed either request or decompressed
  response limits without first allocating/appending it. Exact-limit success and
  `u64`/platform-size conversion boundaries are covered.
- `dispatch_budget` covers timeout-only, deadline-only, both orders, equal bounds,
  already-expired deadline, synthetic default, overflow clamp, and one shared `now` for
  `send_all`. `BudgetSource` asserts only selected-input provenance, never timer phase
  or retry/abandonment semantics.
- Every timeout path carries `budget.cause`; bare inner/caller deadlines use
  `Unspecified`. JSON does not serialize the cause.
- `send_all` preserves input order, allows partial failures, does not cancel siblings
  because one slot fails, and applies the documented empty-input and concurrency semantics.
- One-slot `send_all` matches single-`send` preflight/result semantics. A GET/HEAD streamed
  body hits the method/body diagnostic before the generic `send_all` streamed-body error.
- Buffered and streamed response drains enforce caps and deadlines, recheck after EOF, and
  preserve typed 502/504/response-too-large outcomes.
- Manifest parsing rejects unknown/duplicate capability names, required/optional overlap,
  misplaced nested `capabilities` tables, invalid host grammar, and non-canonicalizable
  ports/authorities. Canonical rendering preserves the scheme of every atomic wildcard:
  the absent https-only default renders exactly `https://*:*`, while explicit bare `*`
  renders separate http and https entries. Baked JSON runs the same validate/finalize
  pipeline and distinguishes Absent from Malformed.

### 5.2 Tier 2 — adapter contracts, no external network

Each adapter crate tests its shipped conversion and classification seams.

| Surface | Required assertions |
| --- | --- |
| Capability metadata | All four adapters return the exact seven cells in §3.5.2, including Cloudflare header fidelity = BestEffort and Spin deadline/upload = BestEffort; unknown future capabilities fail closed as Unsupported. |
| Request conversion | Method/body/headers/canonical authority survive conversion; normalized hop-by-hop fields cannot reappear; buffered and streamed request caps map to 400. Typed `EdgeError` request chunks survive adapter conversion; in-tree paths never route them through `from_external_stream`. |
| Response conversion | Every adapter normalizes raw headers before decode/cap logic, passes the originating method into `OutboundResponse`, and settles native body handles for framing-bodyless and 205 responses; repeated `Set-Cookie` survives. |
| Header fidelity | Axum/Fastly/Spin exercise raw malformed nomination and encoding lines; Cloudflare tests the visible-string baseline and does not assert unavailable octets/line boundaries. Cloudflare encoded passthrough uses `EncodeBody::Manual`; its streamed downstream `Content-Length` is asserted only to the documented BestEffort scope. |
| Decoder integration | Each adapter uses the shared decoder/carrier/deadline pipeline; gzip/br stalls before output, midstream, and at EOF produce attributed 504 rather than hanging or degrading to 502/500. |
| Buffered response-out fallback | Axum/Fastly/Spin enforce their 16 MiB adapter cap and synthesize the standard JSON envelope with the original `EdgeError` status and kind. |
| Lazy response-out | Cloudflare yields bytes before source EOF. The other three report BestEffort and are tested only for bounded buffering. |
| Axum timeout | Fake-time/transport seams prove the remaining budget is armed once and timeout errors preserve provenance. |
| Cloudflare cancellation | The converter calls `AbortController::abort` when the timer wins; a dropped Rust future alone is not accepted as implementation. |
| Streamed request deadline boundary | Axum and Cloudflare check expiry before every pull and after every ready source result. Fake-time streams cover a chunk, EOF, and source error becoming ready exactly at expiry, plus an always-ready sequence of empty chunks; every case returns the attributed 504 and cannot starve the timer. |
| Timeout provenance | Each adapter's actual timed-out result covers timeout-wins, deadline-wins, and synthetic-default input selection in single send, buffered fan-out, and streamed error chunks; no adapter emits a bare un-attributed budget timeout. |
| Fastly stages | Backend identity/canonical host/TLS/SNI inputs, phase-timer rounding, cold registration, serial harvest, and streamed-upload cooperative checks match §4.3. The feature-gated overhead seam proves the slack invariant. `SendFailure` exhaustively maps timeout -> attributed 504, transport/protocol -> 502, local invariants -> 500, and the separately named Fastly platform-internal class -> 500. |
| Spin request protocol | Exercise every `run_exchange` transition and ownership boundary: after full upload, `send` continues to be polled but a ready result is retained until `request_done` succeeds; a `request_done` error wins over that stored result and is mapped; reader-gone retains but never polls `request_done` until `send` resolves, then drops it before response conversion; send-first drops all request handles; clean EOF/reader-gone writes `Ok(None)` trailers; source/cap/deadline failure leaves the default `Err`. Biased simultaneous readiness makes an already-ready pump failure beat `send`, while send ready before a later source failure remains authoritative. An always-ready empty-chunk source yields between chunks so send/timer polling cannot starve, and no request-side handle enters a streamed response wrapper. |
| Spin response protocol | `consume_body` receives the caller-result reader; stream/trailer handles retain the writer; clean EOF/trailers writes `Ok`; body/decode/deadline failure writes or defaults to `Err`; no handle is dropped before its terminal branch. |
| Spin error classifier | Enumerate every pinned `ErrorCode`. The five timeout variants -> attributed 504; caller-controlled request denied/body/URI/header-size variants -> 400; demonstrated length/method/URI/trailer/config invariants -> 500; upstream/transport/protocol and host `InternalError` -> 502. Test both `client::send` and `request_done` mapping sites. |
| Spin timer | Timer selection returns guest-visible 504 and drops owned guest handles. Tests do **not** claim bounded host teardown; that remains Tier 3 characterization and an upgrade criterion. |
| Simultaneous terminal race | For decoder, transport, and Spin exchange seams, a result becoming ready at the absolute deadline yields attributed 504 whether the competing result is success or error. |
| CLI runtime gates | Table-drive the full ladder: required Native/BoundedCooperative succeeds; required BestEffort/Unsupported/future support fails; optional degradation warns and proceeds. Cover missing-registry empty, optional-only, and required manifests; `ManifestContract::None` proceeds while Malformed/future state fails closed. Build/serve/deploy gate before shell dispatch and demo before startup; auth remains exempt. Provision/config commands are outside this spec and are not asserted. |
| Manifest resolver | Build, serve, and deploy from a nested cwd discover the root `edgezero.toml`; `EDGEZERO_MANIFEST` wins verbatim; absence is `Ok(None)`; malformed found input fails; the same owned `ManifestLoader` reaches gate and execution. |
| Spin host drift | Build/serve/deploy compare canonicalized sets before shell dispatch. Equivalent spelling/order passes; actual drift fails with the expected canonical list. Rendering `None`/`https://*:*` must not produce bare `*` or any `http` atomic; explicit input `*` produces the two scheme-specific entries. |

### 5.3 Tier 3 — live host behavior

- **Axum:** a loopback origin verifies real methods/bodies/headers, gzip/br decode stacks,
  bodyless responses, non-2xx pass-through, response stalls, upload stalls, cap errors,
  timeout provenance, and connection cancellation.
- **Cloudflare:** a pinned workerd-compatible harness verifies AbortController cancellation
  from the origin's point of view and lazy streamed-response delivery. This is evidence for
  Cloudflare's Native deadline/cancellation behavior; raw malformed-header fidelity is not
  asserted because workerd does not expose it.
- **Spin:** a pinned `spin up` harness and external origin record whether stalled upload
  and response work is eventually cancelled and whether any component work remains.
  Current capability values stay BestEffort regardless of a single passing run. Promotion
  to Native requires a documented finite bound, repeatable host-observed tests, and the
  corresponding matrix/spec update.
- **Fastly:** live tests run when the supported local/runtime harness can exercise dynamic
  backends and timeout behavior. Until then the no-network adapter tests pin the guest-side
  mechanics and all documented host gaps remain BestEffort.

### 5.4 Required test-case index

The Tier 1 bullets, Tier 2 table, and Tier 3 runtime list above are the required test-case
matrix referenced throughout §§3–4. A reference to “the §5.4 row” means the matching
surface in those lists; implementation plans may split one surface into multiple focused
tests, but may not weaken its tier or substitute a mock for a host-observed claim.

### 5.5 Executable test seams and CI impact

Integration tests that need deterministic adapter states use a narrowly feature-gated
`test-utils` seam rather than `#[cfg(test)]`: external `tests/*.rs` crates do not see
the library's `cfg(test)` items. Seams inject clocks, transport results, or documented
adapter-stage delays; they do not duplicate the behavior being tested or expose platform
SDK types to core Tier 1 tests.

Required gates for implementation changes:

1. Repository `CLAUDE.md` format, test, clippy, feature-combination, and Spin WASM checks.
2. Per-adapter WASM target checks and no-network contract tests.
3. Generated-project build and the excluded `examples/app-demo` build.
4. Published-doc link/navigation verification.
5. Cloudflare host-observed cancellation job for its Native claim.
6. Spin host-observed characterization job when available; failure preserves BestEffort
   and blocks only a proposed promotion to Native.

The spec and Phase 1a plan themselves are documentation changes, so review-time
verification is Markdown structure, internal-link/anchor consistency, scoped terminology,
and `git diff --check`; Rust builds are required when an implementation plan is executed.

## 6. Migration impact

No compatibility shims are introduced for the outbound API rename.

| Before | After |
| --- | --- |
| `crates/edgezero-core/src/proxy.rs` | `crates/edgezero-core/src/outbound.rs` |
| `ProxyClient` | `OutboundHttpClient` |
| `ProxyHandle` | `HttpClient` |
| `ProxyRequest` | `OutboundRequest` |
| `ProxyResponse` | `OutboundResponse` |
| `ProxyService<C>` | removed; use `HttpClient` |
| `RequestContext::proxy_handle()` | `RequestContext::http_client()` |
| adapter `*ProxyClient` | adapter `*OutboundClient` |

Outbound-facing changes:

- `OutboundRequest` and `OutboundResponse` continue to use the unified core `Body`
  type. Buffered responses remain the default; `stream_response()` opts into a streamed
  response. `OutboundRequest::from_request` preserves the source method, normalized
  headers, and supplied body without requiring any inbound extractor or body-state
  redesign.
- `OutboundResponse` carries the originating request method through construction and
  `into_parts` so HEAD and other bodyless-response rules remain enforceable at the final
  conversion boundary.
- `ProxyHandle::client()`, outbound request/response `body_mut()`, and outbound
  `extensions()` / `extensions_mut()` are removed. `HttpClient` exposes only
  `send` / `send_all`; the new request and response values are builder-style and do
  not carry `Extensions`.
- `PROXY_HEADER` and the observable `x-edgezero-proxy: <adapter>` response header are
  preserved. The constant moves to `outbound.rs` without changing its value.
- `Body::Stream` changes its chunk error from `anyhow::Error` to `EdgeError` so
  deadline, gateway, and over-cap outcomes survive decoder and adapter boundaries.
  `Body::from_stream` now accepts only streams already returning `EdgeError`;
  callers with arbitrary external errors use `Body::from_external_stream`, which
  deliberately maps them to `internal`. `Body::into_stream` returns the exact typed
  stream. These signatures are public breaking changes. Every existing
  `Body::from_stream`, `Body::into_stream`, and direct `Body::Stream` call site must be
  audited: EdgeZero-owned outbound/decoder/deadline producers use the typed constructor;
  platform inbound-body sources that previously relied on the generic `anyhow` mapping
  use `from_external_stream` to preserve their existing behavior. That mechanical
  constructor migration is required for compilation but does not redesign inbound body
  extraction, caps, or state.
- `EdgeError` gains `BadGateway`, `GatewayTimeout { cause: BudgetSource }`, and
  `ResponseTooLarge`. Their JSON wire shape remains the existing error envelope and does
  not serialize `BudgetSource`.
- `Manifest` gains the seven outbound capabilities and
  `[capabilities.outbound].hosts`. Existing non-outbound capability and store schemas are
  unchanged. The depth-independent misplaced-`capabilities` rejection is intentionally
  fail-closed.
- `Adapter` gains the defaulted `capability()` method. Each in-tree adapter returns all
  seven matrix values and ends its non-exhaustive match with
  `_ => CapabilitySupport::Unsupported`.
- Capability enforcement has two outbound runtime sites: `execute(..)` gates
  `build` / `serve` / `deploy` before shell or registry dispatch, and `run_demo`
  gates the Axum demo from its baked manifest. `auth *`, provision, and config commands
  are not changed by this outbound specification.
- Build/serve/deploy share `resolve_root_manifest`; `ResolvedManifest` owns a
  `ManifestLoader`, is used for both gating and execution, and is never reparsed.
- Scaffolding, `examples/app-demo`, and public proxying docs migrate to the renamed
  outbound types. Shipped outbound examples declare `required = ["outbound-http"]`.
  Spin's generated platform manifest consumes the canonical outbound host list.
- `docs/guide/capabilities.md` documents all seven outbound capabilities, the support
  matrix, and each BestEffort caveat, and is linked from the VitePress sidebar because
  runtime diagnostics point to that published page.

Repository-wide completion sweeps cover `ProxyClient`, `ProxyHandle`,
`ProxyRequest`, `ProxyResponse`, `ProxyService`, `proxy_handle`, and the four
adapter `*ProxyClient` names across Rust sources, examples, templates, and published
docs. A separate compile-driven sweep covers every `Body::from_stream`,
`Body::into_stream`, and direct `Body::Stream` construction/consumer so no typed
`EdgeError` is accidentally erased at the new explicit external-error boundary. A
generated-project build verifies that scaffold templates compile against the new outbound
API. Inbound body extraction/state and store/config lifecycle behavior remain owned by
their dedicated designs and are not migration prerequisites here.

## 7. File-by-file change summary

Exact filenames should follow the current tree at implementation time; function names are
the durable anchors.

**`crates/edgezero-core`**

- `src/lib.rs` exports `outbound`, `time`, `HttpClient`,
  `OutboundHttpClient`, `OutboundRequest`, `OutboundResponse`,
  `ResponseBodyDisposition`, `ResponseMode`, `Deadline`, and the outbound capability
  types; remove proxy exports.
- `src/proxy.rs` becomes `src/outbound.rs`. It owns request construction and
  validation, canonical URI accessors, the private `BudgetInputs` accessor consumed by
  `time.rs`, `HttpClient::send` / `send_all`, bounded response drains, hop-by-hop
  normalization, response-bodyless rules, and the preserved `PROXY_HEADER`.
  `OutboundResponse::new` accepts the originating request method and
  `into_parts` returns it with status, headers, and body.
- `src/body.rs` changes streamed chunk errors to `EdgeError`, adds
  `From<Bytes>`, and implements pre-append checked accounting.
  `Body::from_stream` is the typed `EdgeError` constructor used by adapters,
  decoders, and deadline wrappers; `Body::from_external_stream` is the explicit
  arbitrary-error boundary and maps every source error to `internal`.
- `src/compression.rs` keeps one shared gzip/br decoder implementation. An exact typed
  carrier crosses the `io::Error`-based decoder boundary and restores the original
  `EdgeError`. The absolute deadline/cancellation wrapper sits **outside decoded
  output** and checks every decoded yield plus terminal EOF/error, so compressed input
  cannot consume unbounded time while emitting no output.
- `src/error.rs` adds `BadGateway`, `GatewayTimeout { message, cause }`,
  `ResponseTooLarge`, and `BudgetSource`; update every exhaustive match and preserve
  the current JSON envelope. Phase 1a is specified by
  `docs/superpowers/plans/2026-07-10-outbound-http-phase1a-error-time.md`.
- `src/time.rs` adds `Deadline` and the three budget constants with `web-time`.
  Phase 1b adds `DispatchBudget` and `dispatch_budget` here as the independently testable
  value/arithmetic layer; they consume `OutboundRequest::budget_inputs()` without owning
  request construction or platform behavior.
- `src/app.rs` adds the three-state baked-manifest accessor to `Hooks`; update both
  handwritten in-core `Hooks` implementations and the macro-generated implementation.
- `src/context.rs` performs only the outbound handle rename
  `proxy_handle()` -> `http_client()`. Inbound request/body state is unchanged by
  this specification; any local test fixture affected by the global `Body::from_stream`
  signature change switches to the appropriate explicit constructor without changing
  behavior.
- `src/manifest.rs` adds the seven outbound capabilities,
  `ManifestCapabilities`, `ManifestOutboundCapability`, host validation, misplaced
  nested-`capabilities` rejection, and the three-state baked-manifest contract.
  Runtime and baked parse paths run the same validation/finalization logic.

**`crates/edgezero-macros`**

- `src/app.rs` bakes the validated manifest JSON and emits the per-app
  `manifest_json()` / `manifest()` accessors. A per-implementation `OnceLock`
  prevents cross-app cache sharing. Its raw TOML parse path runs the shared
  depth-independent `reject_misplaced_capabilities` scan before typed deserialization;
  compile-fail coverage rejects misplaced `capabilities` blocks at multiple depths.

**`crates/edgezero-adapter`**

- `src/registry.rs` adds the defaulted `Adapter::capability()` method. In-tree
  overrides return all seven outbound matrix cells and use a final
  `_ => Unsupported` arm for the non-exhaustive enum. No store/provision API changes
  belong to this spec.

**`crates/edgezero-adapter-{axum,cloudflare,fastly,spin}`**

- Rename each outbound provider module/client from `proxy` / `*ProxyClient` to
  `outbound` / `*OutboundClient`; implement both response modes, request preflight,
  method-aware `OutboundResponse` construction, request and response normalization,
  decompressed-byte caps, typed decoder errors, and the exact seven-cell capability
  tuple.
- Adapter response converters destructure request method, status, headers, and body.
  They normalize raw upstream headers and settle bodyless responses before content
  decoding/capping and `OutboundResponse` construction, then reapply normalization
  idempotently before final platform conversion.
  Cloudflare alone provides Native lazy streamed-response passthrough; Axum, Fastly,
  and Spin use their documented 16 MiB bounded buffered fallback and preserve each
  `EdgeError` status/kind when a drain fails.
- Do not change inbound platform-request buffering or `RequestContext` construction;
  those are owned by the inbound-body design. The one allowed inbound-side edit is the
  compile-required, behavior-preserving constructor migration from generic
  `Body::from_stream` to `Body::from_external_stream` (or an explicit `EdgeError` map
  followed by typed `from_stream`) at existing platform-body conversion sites.

Adapter-specific work:

- **Axum:** use the remaining `DispatchBudget` for reqwest's whole-operation timeout;
  race every streamed request-source pull and apply the absolute post-ready deadline
  check before accepting chunk/EOF/error; disable reqwest auto gzip/br; use the shared
  decoder and normalization. Keep header fidelity Native.
- **Cloudflare:** use `AbortController` plus `worker::Delay` for send/body
  cancellation; use the same pre-pull/post-ready absolute check while draining a
  streamed request source; stream response output lazily; and classify header fidelity
  BestEffort because workerd exposes normalized strings rather than raw malformed field
  lines. Set `EncodeBody::Manual` when forwarding already encoded passthrough bytes so
  Workers does not transform the payload.
- **Fastly:** derive deterministic dynamic-backend names; configure connect,
  first-byte, and between-bytes host timers from the remaining budget; retain the
  documented cold-registration, upload-write, and serial-harvest BestEffort gaps.
  `SendFailure` is constructible and unit-tested independently of SDK-private error
  values. `PlatformInternal` is pinned separately from `LocalInvariant` even though both
  currently map to 500, so a future policy change cannot silently conflate their causes.
- **Spin:** use the hand-built WASI HTTP request for buffered and streamed uploads.
  Implement the biased `run_exchange` state machine and cooperative per-chunk yield;
  retain or consume `request_done` at the exact state boundaries in §4.4; explicitly
  resolve request trailers on clean EOF/reader-gone; use the response `consume_body`
  caller-result protocol; own every future/reader/writer until its terminal branch; and
  classify both `client::send` and upload-completion `ErrorCode` results.
  Caller-controlled request policy/size variants map to 400, core/adapter invariants to
  500, timeouts to attributed 504, and upstream failures to 502. The monotonic race
  returns a guest-visible timeout, while deadline and upload cancellation remain
  BestEffort until host-observed tests establish a finite bound.
  Replace the adapter-local buffered `src/decompress.rs` path with the shared core
  streaming decoder; remove direct production `brotli` / `flate2` dependencies if no
  remaining Spin-only use exists (retain only dependencies still required by tests).

**`crates/edgezero-cli`**

- The `run_build`, `run_serve`, and `run_deploy` command entry points call the shared root
  resolver once and pass the resulting loader to `src/adapter.rs::execute`.
  `execute` gates only `Build` / `Serve` / `Deploy` before its shell-command branch and
  registry lookup, using that same loader; it does not rediscover or reparse. Auth actions
  bypass the runtime gate.
- `src/manifest_source.rs` (or the existing manifest-loading module) adds
  `ManifestSource::{EnvVar, Defaulted}`, `ResolvedManifest { path, loader }`, and
  upward root discovery. A found malformed manifest is an error; genuine absence is
  `Ok(None)`; no path reparses or clones `Manifest`.
- `src/demo_server.rs::run_demo` gates Axum against the baked manifest before startup.
- The Spin build/serve/deploy pre-dispatch path validates canonical
  `allowed_outbound_hosts` drift before shell overrides can bypass it.
- Provision and config command implementations are unchanged by this outbound spec.

**Templates, examples, and docs**

- Rename generated proxy APIs in Rust and Handlebars sources. The root scaffold and
  `examples/app-demo` declare `required = ["outbound-http"]`; Spin's generated
  platform manifest renders the canonical outbound host list.
- Update public proxying, handler, architecture, streaming, and adapter docs. Add
  `docs/guide/capabilities.md` with all seven outbound capabilities and sidebar
  navigation.
- Build one generated project and the excluded `examples/app-demo` workspace so
  template/example drift cannot hide behind the root workspace build.

**Tests and CI**

- Core colocated tests cover builders, URI validation, normalization, bodyless rules,
  typed versus external body-stream constructors, budget selection/provenance, bounded
  drains, typed decoder errors, decoded-output deadlines, and `send_all`
  index/partial-failure semantics.
- Each adapter has no-network contract tests for request conversion, response
  conversion, error classification, capability metadata, and platform-specific
  timeout mechanics.
- Axum loopback tests prove native wire behavior. Cloudflare's runtime cancellation
  test is blocking evidence for its Native cancellation claim. Spin host-observed
  tests characterize cancellation and are the criterion for a later BestEffort ->
  Native upgrade; they are not a prerequisite for the current BestEffort claim.
  Fastly runtime tests remain conditional on a supported local harness.
- Required local gates are the repository `CLAUDE.md` commands, all adapter WASM
  target checks, the generated-project build, and the app-demo build. Documentation-only
  edits use Markdown/link/diff verification rather than rebuilding Rust.

## 8. Open questions / risks

1. **`DEFAULT_MAX_RESPONSE_BYTES` = 1 MiB.** Trivially overridable per request via
   `max_response_bytes`. Confirm the default suits expected target responses.
2. **Tier 3 CI runtimes.** Viceroy / `workerd` / `spin` jobs add CI cost and
   maintenance. The design degrades safely (Tier 1 + Tier 2 always run); the risk is
   schedule, not correctness.
3. **Cloudflare cancellation — RESOLVED.** A timed-out subrequest is cancelled via
   `worker::AbortController::abort()` on the `AbortSignal` passed to
   `Fetch::send_with_signal` (§4.2), not by dropping the Rust future (which would leave
   the POST running after EdgeZero returns 504). The Tier 3 CF test verifies the origin
   observes the abort.
4. **Fastly active response-drain overshoot.** Once an individual warm-path slot is
   actively draining its response, that read-phase overshoot is bounded by one
   between-bytes-timeout interval (§3.3.4). This does not bound cold backend registration,
   request-body writes, or time spent waiting behind earlier `send_all` harvest work; those
   gaps are owned by footnotes 1/2/4 and risks 7/8. If a stricter active-drain guarantee is
   ever required, the adapter would need to cap total body-read attempts — out of scope here.
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
7. **Fastly streamed-upload write-phase has no SDK-configurable bound.**
   Fastly's `between_bytes_timeout` is documented as receive-side only — it
   bounds the gap between bytes received from origin, not the host-side write
   of guest-supplied bytes to origin (Fastly Backend API docs; round 50). No
   published Fastly backend-timeout field bounds the guest-to-origin write
   direction. Streamed-upload write-phase is therefore `BestEffort` on
   Fastly (alongside the source-stream-yield `BestEffort`); the cooperative
   `budget.deadline.is_expired()` check **between** chunks is the only
   adapter-side bound. Apps that need real-time enforcement against a slow
   origin on the write path must **target a different adapter**. (Passing a buffered
   `Body::Once` does *not* fix this: it removes the source-pull stall but the host still
   writes those bytes in the untimed `connect`→`first_byte` window. The write-side gap is
   a property of Fastly's timeout model, not of `StreamingBody`.) If a future
   Fastly platform release adds a documented guest-write timeout, it would close the
   write-side gap only; the capability would remain BestEffort until source-pull
   preemption also has a documented bound. Track Fastly host docs.
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
10. **Spin target documentation drift.** Implementation verification uses
    `wasm32-wasip2` for Spin SDK 6. Any remaining comment that associates Spin with
    `wasm32-wasip1` should be corrected when the implementation touches that file, but
    this documentation-only cleanup is not a prerequisite for the outbound design.
11. **Per-batch transient-memory cap against adversarial chunking.** §3.4.1's
    `sizeof(current_chunk)` term is source-controlled — an upstream peer that
    yields one large `Bytes` produces a transient resident footprint equal to
    that chunk size plus the persistent buffer cap. EdgeZero currently does not
    rechunk. The follow-up would either: (a) add an opt-in
    `OutboundRequest::max_chunk_bytes(u64)` builder field that wraps the
    upstream stream with a rechunker on the consumer side (lazy, opt-in, no
    perf cost when unset); (b) add a fixed `MAX_TRANSIENT_CHUNK_BYTES` constant
    in `edgezero-core` that every adapter's incoming-body stream must respect
    by rechunking at ingest (eager, adds copying/buffering pressure and changes
    observed chunk boundaries); or (c) leave
    it source-controlled and document the bound at the adapter level
    (for example, a provider's natural incoming-frame size) as the operational floor. Each
    option has a perf and lazy-streaming trade-off; deferred until a
    fan-out batch or downstream consumer reports actual OOM behaviour from
    adversarial chunking. The §3.4.1 / §3.4.4 docs already call out the
    caveat so apps aren't surprised.
12. **Fastly lazy-streamed-response-passthrough via non-`#[fastly::main]`
    entry point.** Today's Fastly scaffold uses `#[fastly::main]`, which
    implicitly calls `Response::send_to_client()` on the returned response.
    Fastly's `Response::stream_to_client()` — the only API that flushes
    response bytes to the client lazily — is documented as incompatible
    with `#[fastly::main]`. As a result, the Fastly adapter currently
    falls back to buffered passthrough (drain `Body::Stream` to `Bytes`
    within `FASTLY_RESPONSE_STREAM_BUFFER_BYTES` (16 MiB) before returning —
    the per-request `max_response_bytes` is not available at the response
    converter), and
    `lazy-streamed-response-passthrough` is `BestEffort` on Fastly per
    footnote 6. The follow-up would either: (a) scaffold a non-attribute
    entry (`fn main() { let req = Request::from_client(); … resp.stream_to_client() … }`)
    and route the EdgeZero handler through it, with `stream_to_client()`
    feeding chunks from the wrapped `Body::Stream`; (b) keep
    `#[fastly::main]` for buffered handlers and add a separate
    `#[edgezero::stream_main]` attribute that expands to the
    non-attribute form when the manifest declares
    `lazy-streamed-response-passthrough` required; (c) leave the
    `BestEffort` downgrade and document the migration path. Each option
    affects scaffolding templates, `edgezero new`, and contributor
    docs. **Deferred** until an app explicitly requires lazy Fastly
    passthrough; the §3.5.2 footnote 6 documents the exact constraint
    so adopters aren't surprised.
13. **Spin lazy-streamed-response-passthrough via a streamable public response
    surface.** Spin's response path is buffered by construction today:
    `spin_sdk::http::FullBody` backs the `SpinFullResponse` alias
    (`Response<FullBody<Bytes>>`), which appears in `AppExt::dispatch`,
    `request::dispatch*`, `from_core_response`, and `run_app`. Delivering lazy
    passthrough is therefore **not** an outbound-client change — it is a **breaking
    public-API migration** of those aliases and signatures to a streamable response
    shape. **The platform is not the blocker:** Spin SDK 6 already supports lazy
    response streaming (`IncomingBody: http_body::Body`, 16 KiB `poll_frame`,
    `IncomingBodyExt::stream()`), and the adapter simply *chooses* `.bytes()` today
    (`spin/proxy.rs`). Lifting Spin to `Native` is therefore a **pure EdgeZero
    refactor**, not a platform lift — unlike Fastly's risk 12, which is a real
    platform constraint. (Separately: the WASI-0.2 `check_write()` shape that earlier
    drafts used for the *request-upload* path does not exist in SDK 6 at all; that is
    now corrected in §4.4 via a hand-built `wasi:http` request.) Because the alias
    migration carries its own design, migration, and test surface — and would
    ripple into `examples/app-demo`, the Spin scaffold templates, and every
    downstream consumer of `SpinFullResponse` — Spin is **`BestEffort`** for this
    capability in the current change (footnote 7), with a bounded buffered fallback
    through `SPIN_RESPONSE_STREAM_BUFFER_BYTES` (16 MiB) identical in shape to Axum's
    and Fastly's. **Cloudflare remains the only `Native` adapter** for lazy
    passthrough; apps that require it declare the capability and target CF, getting a
    hard build failure elsewhere. Lifting Spin to `Native` is **deferred** to its own
    change. This affects response-out independently of the hand-built outbound upload
    path; Spin still reports `streamed-upload-deadlines` as `BestEffort` because guest
    cancellation has no documented finite host-teardown bound (footnote 8).
14. **`outbound-deadlines-exact` capability — not needed.** No adapter reports
    `BoundedCooperative` for `outbound-deadlines`; a plain required declaration is
    accepted only on the Native adapters (Axum and Cloudflare) and hard-fails on Fastly
    and Spin. The support ladder already expresses the required distinction.
15. **Spin deadline promotion criterion.** The hand-built WASI protocol prevents an
    unowned detached pump and the monotonic timer can select a guest-visible 504, but
    Component Model cancellation and default `FutureWriter` completion remain
    cooperative. A Tier 2 test that observes dropped guest handles is insufficient for
    Native. Promotion requires repeatable live-runtime evidence of a documented finite
    teardown bound for stalled request upload, response body, and completion-future
    paths, followed by an explicit matrix and footnote update.
