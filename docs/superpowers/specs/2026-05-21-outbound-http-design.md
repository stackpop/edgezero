# EdgeZero Outbound HTTP — Design Spec

> **Status:** Draft, revised through review rounds 1–51 (round 51 = round-50 carry-over fixes: early Fastly dynamic-backend paragraph reconciled with the corrected `NameInUse` algorithm, `FASTLY_RESPONSE_STREAM_BUFFER_BYTES` added for the buffered passthrough fallback, §5.4 lazy-passthrough rows rebucketed so Fastly is no longer grouped with CF/Spin, residual `between_bytes_timeout` write-side claims scrubbed from §5.4 + §8 risk 7, Spin host-write race rewritten against actual WASI nonblocking + readiness-poll semantics) · **Date:** 2026-06-08
> **Branch:** `docs/outbound-http-spec` · **Audience:** EdgeZero maintainers
> **Driving pattern:** fan-out HTTP workloads — N concurrent outbound requests under a shared wall-clock deadline, results harvested in input order. The spec is written against this pattern as a portable substrate; it deliberately does not name a specific consumer.
> **Target codebase baseline:** [`stackpop/edgezero` PR #269](https://github.com/stackpop/edgezero/pull/269) (`feature/extensible-cli`, rev `b4c80e9`) — **now merged into `main`** (squash-merged as `e483723`). The current tree has *since* gained further work the spec has not fully reconciled: typed config-push (`run_config_push_typed`), pluggable introspection routes, and expanded CI. PR #269 introduces the multi-store manifest (`ManifestStores { config, kv, secrets }`), the `edgezero_cli::adapter::execute(..)` shell-or-registry dispatcher, the expanded `AdapterAction` (`AuthLogin` / `AuthLogout` / `AuthStatus` / `Build` / `Deploy` / `Serve`), separate `Adapter::provision(..)` and config-validation hooks, Spin SDK 6 / wasip2, the contributor-only `demo` command replacing `dev`, and the new `examples/app-demo/crates/app-demo-cli` integration crate.
> **Current checkout (post-#269):** the CLI surface is now the #269 shape — `Command::{Build, Serve, Deploy, Auth, Provision, Config, Demo, New}`, `AdapterAction::{AuthLogin/Logout/Status, Build, Deploy, Serve}`, and the `edgezero_cli::adapter::execute(..)` dispatcher; `dev` is gone. **The CLI gate rows are now reconciled with this surface** (§3.5.3 is authoritative): gating is by command **class** — `build`/`serve`/`deploy`/`provision`/`config push`/`config validate`/`demo` are gated; **`config diff` and `auth *` are exempt** (read-only / credential). The gates target the **typed** entry points (`run_config_push_typed`; the bundled `run_config_push` is a v1 stub that errors), `config validate` is adapter-less (`ConfigValidateArgs` has no `adapter` field) and loops every configured adapter behind a **shared inner op** both its bundled and typed entries call, and `demo` reads the manifest baked in by `app!`. Spec §1 / §3.1 / §3.2 / §3.3 / §3.4 / §4 (the outbound HTTP design itself) is independent of the CLI surface and lands either way.
> **Where rebase claims live (authoritative surfaces):** §3.5.3 build-enforcement, §3.5.2 `Adapter` trait shape (showing both the pre-#269 and PR-#269 forms), §5.4 capability test rows mentioning `demo` / `auth` / `provision` / `config push|validate`, and the §7 `edgezero-cli` migration bullet. The §3.5.3 + §7 active text is authoritative.

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
  `app-demo`, scaffolding templates, and docs are migrated. No deprecated
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
 /// failures surface later, when the caller consumes `resp.body`:
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
 /// ( footnote 4): on Axum/CF/Spin it's `Native` (concurrent body
 /// drains), but on Fastly it's `BestEffort` because buffered-body drains
 /// run in harvest order, so a slot whose own budget would have
 /// covered it can still return `gateway_timeout` because an earlier slot
 /// monopolized harvest. Apps that require the stricter cross-slot timing
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
 /// `sizeof(current_chunk)` term from); the full core-owned bound is therefore
 /// `Σᵢ request_bodyᵢ.len + Σᵢ max_response_bytesᵢ + Σⱼ
 /// sizeof(current_chunkⱼ)` where j ranges over slots currently in a drain
 /// step. Actual process RSS can exceed this by the excluded adapter/host terms. EdgeZero does NOT impose a global cap on N — apps are
 /// responsible for bounding the number of requests passed in. On Fastly all
 /// requests are in-flight at the host simultaneously to make fan-out work,
 /// so a `max_concurrency` knob would defeat the feature; instead, bound N
 /// at the application layer (typically the fan-out batch's target count).
 ///
 /// **Request bodies MUST be buffered (`Body::Once`).** A `Body::Stream`
 /// request body yields `out[i] = Err(EdgeError::bad_request("send_all
 /// requires buffered request bodies; use send for a streamed upload"))`,
 /// identically on every adapter. This rule prevents Fastly's
 /// dispatch-all-then-harvest fan-out from serializing on slow request
 /// uploads.
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
 /// guest reactor,; even on Axum/CF/Spin a consumer iterating
 /// `out[i].body` serially can't outrun the wrapper deadlines that have
 /// been ticking since headers). Apps that want streamed responses use
 /// single `send` and orchestrate concurrency themselves on the three
 /// reactor-bearing adapters — the canonical pattern is `futures::join_all`
 /// of N `send` calls, then consume each `OutboundResponse` via the
 /// **app-facing consuming accessor `into_body -> Body`** and
 /// iterate the `Body::Stream` chunks concurrently across the N slots.
 /// `into_parts(..)` exists too but is labelled adapter-facing because it
 /// returns the (status, headers, body) tuple that response converters
 /// need; pure orchestration paths just want the body. This rule keeps
 /// `send-all-slot-isolation`'s `Native` claim on Axum/CF/Spin honest —
 /// the cross-slot body-lifetime problem is removed by construction rather
 /// than papered over.
 ///
 /// **"Identical" scope.** The trait contract guarantees identical
 /// **input handling**: same preflight, same index alignment, same
 /// per-slot Ok/Err shape. The *cross-slot timing behaviour* is **not**
 /// uniform — see the `send-all-slot-isolation` capability.
 /// On Axum/CF/Spin `join_all` fans out body drains concurrently and a
 /// slot's result reflects what it would have produced in isolation.
 /// On Fastly buffered-body drains run in harvest order, so a
 /// slot can return `gateway_timeout` because an earlier slot
 /// monopolised harvest — even when its own `budget.deadline` would
 /// have covered its body in isolation. Apps that require cross-slot
 /// isolation declare the capability required and get a hard build
 /// failure on Fastly
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
// (Scoped to outbound deliberately — the INBOUND helpers `RequestContext::body_bytes` /
// `json_within` / `form_within` and the `DEFAULT_INBOUND_*` constants stay `usize` in this
// design; migrating the inbound surface to `u64` is the separate §3 inbound follow-up. So
// this invariant does NOT claim "every public cap in the crate is u64" — only the outbound
// ones this spec introduces/touches.) The
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
 /// and downstream consumers (Fastly backend identity in,
 /// app-level allowlist checks, Spin `allowed_outbound_hosts`
 /// matching) compare against one canonical spelling. Userinfo and
 /// fragments are already rejected above; path and query are passed
 /// through verbatim (case-sensitive per RFC 3986 /).
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
 // (per — case-sensitive per RFC 3986 /); they do not
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
 /// Cheap non-consuming check used by `send_all` preflight ( /
 ///–): if `true`, the slot is rejected with `bad_request`
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
    pub max_request_body_bytes: u64,      // applies when `body` is Body::Stream (u64 — see cap note)
}

pub struct OutboundResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Body,                     // Once in Buffered mode, Stream in Streamed mode
}

impl OutboundResponse {
 /// Adapter-facing constructor. Adapters build the response from the
 /// platform SDK's reply: status, normalized headers (decompression
 /// strips `content-encoding`/`content-length`; non-UTF-8
 /// values are dropped — **except `connection`, which is preserved here so
 /// `into_response`'s `normalize_response_for_passthrough` can resolve it fail-closed
 /// (a non-UTF-8 `connection` → `bad_gateway`, never silently dropped; otherwise a
 /// nominated header could smuggle past hop-by-hop removal)**), and the body (`Body::Once` in
 /// `Buffered` mode after the adapter has drained and capped, or a
 /// `Body::Stream` wrapped with the deadline-aware wrapper described
 /// in `into_bytes_bounded_until` for `Streamed` mode).
    pub fn new(status: StatusCode, headers: HeaderMap, body: Body) -> Self;

 /// Adapter-facing destructure. Mirrors `OutboundRequest::into_parts`.
    pub fn into_parts(self) -> (StatusCode, HeaderMap, Body);

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
 /// adapter-facing `into_parts(self) -> (StatusCode, HeaderMap, Body)`
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
 /// Per, adapters with platform timers (Axum/CF/Spin) wrap
 /// `Streamed` response bodies with a deadline-aware stream bounded by
 /// `dispatch_budget(req).deadline` — which is non-`None` even for
 /// timeout-only and no-deadline requests (the synthetic 30 s ceiling) —
 /// so a stalled upstream yields a `gateway_timeout` error chunk and
 /// this drain returns 504. Fastly's bounded-cooperative body check
 /// achieves the same end with a documented overshoot bound.
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
 /// only**: the wrapper's error chunk arrives in real time (timer-backed
 /// on Axum / CF / Spin; bounded-cooperative on Fastly); the
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
 /// enforcement is the wrapper's job** — adapters with platform timers
 /// (Axum / CF / Spin) install a deadline-aware stream bounded by
 /// `dispatch_budget(req).deadline` at response construction time
 /// so the **request budget** is enforced in real time on
 /// those three; Fastly enforces the request budget **cooperatively** on the
 /// body phase (between-bytes), while the `outbound-deadlines` capability is
 /// `BestEffort` overall because cold dispatch is unbounded (footnote 1).
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
 /// `Result` mirrors those methods' signatures for uniformity and reserves a
 /// single `Err(EdgeError::internal(..))` path for an adapter-invariant violation
 /// (reserved to `internal`) — never a network/status condition.
 ///
 /// **RESPONSE-SIDE hop-by-hop normalization is applied here (symmetric with the
 /// request side, §3.1.4).** A proxied UPSTREAM response can carry hop-by-hop headers
 /// that MUST NOT be forwarded downstream: `into_response` strips `connection`,
 /// `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`,
 /// `transfer-encoding`, `upgrade`, **AND every header NOMINATED by the response's own
 /// `connection` value** — so an upstream `Connection: x-private` + `X-Private: secret`
 /// cannot leak `X-Private` to the downstream client. This is centralized in one core
 /// helper `outbound::normalize_response_for_passthrough(&mut Response)` (the response
 /// twin of `normalize_for_dispatch`), so every adapter's passthrough goes through the
 /// same stripping. The `connection` header is resolved **fail-closed** exactly as on the
 /// request side: a non-UTF-8 `connection` value is rejected (`bad_gateway`), never
 /// silently dropped (which would let a nominated header smuggle past removal). §5.4 pins
 /// an end-to-end test: an upstream response with `Connection: x-private` + `X-Private`
 /// yields a downstream response with NEITHER header.
    pub fn into_response(self) -> Result<Response, EdgeError>;
}
```

The complete builder surface — `new`/`get`/`post`/`from_request`/`header`/`headers_mut`/
`body`/`json`/`timeout`/`deadline`/`max_response_bytes`/`max_request_body_bytes`/`stream_response`. Every fallible
step returns `EdgeError`, so handler code uses `?` uniformly.

#### 3.1.4 Adapter behaviour contract — redirects and header encoding

These rules apply identically on every adapter so handler code is portable — **with ONE
documented exception that is deliberately OUTSIDE the portable baseline: raw-byte header
fidelity and repeated non-`set-cookie` field-line preservation.** The portable baseline is
what all four adapters guarantee: valid-UTF-8 header values (§above), multi-value
`set-cookie` preservation, and hop-by-hop stripping. **Cloudflare cannot** preserve
raw (non-UTF-8) header bytes and comma-joins repeated non-`set-cookie` field lines
(workerd limitation — see below); this is NOT part of the portable contract, so the
"identical" claim covers the baseline, not byte-exact fidelity. **A build-time boundary
for apps that genuinely need full fidelity is a candidate `outbound-header-fidelity`
capability** (`Native` on Axum/Fastly/Spin, `BestEffort`/`Unsupported` on Cloudflare) so a
`required` declaration hard-fails on Cloudflare at build — tracked as a §8 follow-up, not
built now; until it exists, apps needing byte-exact headers must avoid Cloudflare manually.
The UTF-8 round-trip being "best-effort in tests" on Cloudflare reflects exactly this
narrowed baseline.

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
UTF-8 string still bearing a forbidden control byte like `\n`/`\0` is rejected — §above.)

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

**Final normalization at dispatch (`outbound::normalize_for_dispatch`).** Two surfaces
bypass the construction-time `header(..)` check — `headers_mut()` exposes raw
`HeaderMap`, and `from_request(..)` carries inbound headers in. Adapters MUST call a
core helper `outbound::normalize_for_dispatch(&mut OutboundRequest)` immediately before
handing the request to the platform SDK. The helper is idempotent and runs the same
rules end-to-end:

1. **First**, handle the `connection` header (see step 2's nomination list) **before**
   any UTF-8 drop, because it governs the removal of *other* headers. A `connection`
   value that is **not valid UTF-8 is rejected** (`EdgeError::bad_request`), NOT silently
   dropped: dropping it would discard the removal intent and let a sender **smuggle a
   nominated header past hop-by-hop stripping** by appending an invalid byte
   (`Connection: x-private,<invalid>` would otherwise forward `X-Private`). This is the
   one header where the lossy drop below is a security hole, so it fails closed instead.
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
   header named in any `connection` header value — parsed from the now-guaranteed-UTF-8
   value per step 1). Idempotent for `from_request`
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
in preflight). It is truly concurrent on all four platforms, and its **input/output
contract** is identical (preflight, index alignment, per-slot Ok/Err shape). Cross-slot
timing **is not uniform** — see the `send-all-slot-isolation` capability and §3.3.4 for
Fastly's buffered-body harvest-order caveat. **For buffered fan-out, app code never calls
`futures::future::join_all`** — `send_all` is it. (Concurrent *streamed*-response requests
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

/// Records which of the two independent timeout dimensions produced the effective
/// deadline, so a timeout OUTCOME can be attributed rather than collapsed. The
/// per-call `OutboundRequest::timeout` and the shared batch `deadline` are separate
/// inputs (§3.3.2 table); the effective deadline is the tighter of the two, and `cause`
/// remembers which one won. A consumer fanning out N calls needs this: a per-call
/// timeout and a batch-deadline expiry map to different downstream semantics
/// (retry-this-call vs abandon-the-batch), so a single undifferentiated "timed out" is
/// insufficient.
// ONE definition, shared by `DispatchBudget` (here) and `EdgeError::GatewayTimeout`
// (§3.4.3). **Defined in `error.rs` (Phase 1a Task 1)** — NOT the `time` module — because
// `error.rs` (Task 1, committed/built first) NAMES it in `GatewayTimeout`, so it must
// exist in Task 1's deliverable or the Task-1 commit fails to build. `time.rs` (Task 2)
// and `dispatch_budget` (Phase 1b) `use crate::error::BudgetSource;`.
// DERIVES + ORDER are COMPILE-VERIFIED (a throwaway crate under `arbitrary_source_item_ordering`):
//   - `Debug` — `EdgeError` derives `Debug` and contains `cause`.
//   - `Clone` — `StoredError` derives `Clone` and contains `cause` (E0277 without it).
//   - `Copy`  — `StoredError::capture` does `cause: *cause` (E0507 without it).
//   - `PartialEq, Eq` — the Phase 1a contract tests assert `cause == Unspecified` etc.
//   - Variants are **alphabetical** — the denied `clippy::arbitrary_source_item_ordering`
//     rejects any other order (verified: the earlier `PerCallTimeout`-first order errored).
/// Which budget INPUT produced the effective deadline (the tightest bound) — the budget
/// SOURCE, NOT the physical phase-timer that fired. On Fastly the per-phase timers
/// (connect/first-byte/between-bytes) are sub-divisions of the budget; when one fires the
/// timeout is still attributed to this source (documented `BestEffort` — §4.3 footnote 5),
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

 // (1) Expired-deadline check using the *single* now snapshot — no remaining
 // round-trip that could lose the distinction between "no deadline" and
 // "deadline expired" (both produce None from remaining).
    if let Some(dl) = inputs.deadline {
        if dl.instant() <= now {
            // The caller's shared batch `deadline` was already in the past → BatchDeadline.
            return Err(EdgeError::gateway_timeout_caused(
                "deadline expired before dispatch", BudgetSource::BatchDeadline));
        }
    }

 // (2) Candidate absolute deadlines. Use checked_add throughout — a caller-
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

 // (3) Effective deadline = min of the candidates (always at least one).
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

 // (4) Duration is derived from the chosen deadline and the same now snapshot
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

**Timeout attribution — the outcome must carry the cause, not collapse to "timed out."**
Because `DispatchBudget` records `cause`, every timeout an adapter (or the pre-dispatch
check) raises is **attributable**: when a slot times out, the synthesized `gateway_timeout`
carries the effective `budget.cause`, so a consumer fanning out N calls can tell a
**per-call-timeout** slot from a **batch-deadline** slot and route them differently
(retry-this-call vs abandon-the-batch). This is carried as a **typed field on the error**:
`EdgeError::GatewayTimeout { message, cause: BudgetSource }` (§3.4.3) — NOT a message string
to be parsed. A slot that times out is raised via `gateway_timeout_caused(msg,
budget.cause)`; a timeout outside a budget context uses `gateway_timeout(msg)`, whose cause
is `Unspecified`. `dispatch_budget`'s own early returns set the
cause where determinable (an already-expired caller `deadline` → `BatchDeadline`; a zero
effective budget → the `cause` just computed).

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
cannot escape the bound (§3.3.2 step 2 / round 16). For brevity the table writes
`clamped(d)` rather than the full expression.

| `req.timeout` | `req.deadline` | `duration` | `deadline` (absolute) |
| --- | --- | --- | --- |
| `None` | `None` | `30 s` | `now + 30 s` |
| `Some(t)` | `None` | `min(t, DEADLINE_FAR_FUTURE)` | `now + min(t, DEADLINE_FAR_FUTURE)` |
| `None` | `Some(d)` | `clamped(d).instant() - now` | `clamped(d)` |
| `Some(t)` | `Some(d)` with `now + min(t, …) ≤ clamped(d).instant()` | `min(t, …)` | `now + min(t, …)` (tighter) — **cause `PerCallTimeout`; EQUALITY goes HERE** |
| `Some(t)` | `Some(d)` with `now + min(t, …) > clamped(d).instant()` | `clamped(d).instant() - now` | `clamped(d)` (strictly tighter) — cause `BatchDeadline` |
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
| Cloudflare | race `Fetch::send_with_signal(&signal)` (+ body drain) against `worker::Delay::from(effective)`; on expiry `controller.abort()` (NOT a dropped future — that leaves the subrequest running) | Real, whole-operation with cancellation |
| Spin | race the entire `send_one` future (send **and** body collect) against a wasi monotonic-clock timer; drop on expiry | Real, whole-operation |
| Fastly | host phase timers split per §4.3 (`connect = budget/4`, `first_byte = 3*budget/4`, `between_bytes = budget`); during body drain, `budget.deadline.is_expired()` is checked **after every blocking body read returns, including the EOF read** (the synthetic 30 s deadline applies when no caller deadline was set); the host between-bytes timeout bounds each gap | Real for connect+headers with a documented phase split (see §4.3 — a connect that itself takes longer than `budget/4` fails even if the rest of the budget would have sufficed); **bounded-cooperative** for the body phase |

**Drop-cancellation guarantee, per adapter (what happens to a LOSING arm).** A fan-out
consumer under deadline pressure needs to know whether a timed-out/deadline-lost send is
actually *aborted* or merely *stopped-being-waited-on* — "harvest returns" ≠ "the pending
request is cancelled." The guarantee:

| Adapter | On deadline/timeout, the in-flight send is… | Origin observes cancel? |
| --- | --- | --- |
| **Axum** | **cancelled** — dropping the `reqwest` future cancels the request (tokio) | Yes (connection dropped) |
| **Cloudflare** | **cancelled** — `controller.abort()` (NOT a bare future-drop, which would leave the subrequest running) | Yes (§5.3 blocking test) |
| **Spin** | **cancelled** — dropping the raced future fires `[subtask-cancel]`, a synchronous host-side teardown | Yes (§5.3 blocking test) |
| **Fastly** | **NOT cancelled** — Fastly exposes no async-cancellation primitive; a dispatched `PendingRequest` is always harvested via blocking `wait()`/`poll()` — the **one** exception is the streamed-upload budget-exhausted path (§3.4 `send_all` cancellation note), which intentionally drops the `StreamingBody`+`PendingRequest`; the host reclaims that subrequest's resources on session teardown. The host phase-timers (connect/first-byte/between-bytes) can *fail* it, but EdgeZero cannot *abort* it, and a **sibling slot's** deadline firing never cancels another slot | **No** — this is a documented BestEffort limitation, not a bug |

So three of four adapters give real drop-cancellation of losers; **Fastly is the honest
exception** — a consumer that strictly requires losing-arm abort must weight this per §5.3
and the `outbound-deadlines` = `BestEffort` classification on Fastly.

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

**No-content responses are handled FIRST, before any decode or Content-Length check.** A
response that is bodyless **by definition** — the response to a **`HEAD`** request, or any
**`1xx`**, **`204`**, **`205`**, or **`304`** status — carries **no payload** even though it
MAY legitimately carry `Content-Encoding` and a *representation* `Content-Length` (e.g. a
`HEAD` echoing what a `GET` would return; a `304` echoing the cached representation's
metadata). For these:
- **Do NOT attempt to decode.** There are no body bytes; feeding EOF to the gzip/br decoder
  would error and produce a **false `bad_gateway` (502)**. Skip the decoder entirely.
- **Do NOT strip `content-encoding` / `content-length`.** On a normal decoded response those
  are stripped because the decoded bytes are the new ground truth — but here there is no
  decode, and those headers are **representation/cache metadata** the client needs (a `304`
  or `HEAD` with them stripped breaks cache validation). Preserve them unchanged.
- The response passes through with headers intact and an empty body.
The bodyless determination is **method- and status-aware** (the converter knows the request
method and the response status). §5.4 pins tests for `HEAD 200` (with `Content-Encoding:
gzip` + representation `Content-Length`, no body → passes through, headers preserved, no
502), `1xx`, `204`, `205`, and `304`. Only AFTER this check do the decode/cap rules below apply.

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

**Pre-append check is mandatory.** Both inbound (`RequestContext::body_bytes`) and
outbound (`OutboundResponse::into_bytes_bounded` / `_until`) bounded drains MUST check
the running total against `max` **before** extending the buffer. **The comparison is done
in the cap's type:** the outbound cap is `u64` (§3.1.3), so the accounting converts the
`usize` lengths up — `let n = u64::try_from(collected.len())? + u64::try_from(chunk.len())?;
if n > max { over-cap }` (lengths are memory-bounded, so `try_from` never actually fails;
no `as`). The inbound helper's cap is still `usize` (the inbound `u64` migration is the §3
follow-up), so its check stays `usize`-native. Either way it is check-then-extend, never extend
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
  does **NOT** enable reqwest's `gzip`/`brotli` auto-decoding, and Axum sends
  `accept-encoding` for identity handling explicitly. reqwest's built-in decoder matches
  **exact lowercase** `content-encoding` values from a single map entry and would not
  honour the portable contract (case-insensitive `GZIP`, `identity`, unknown/**stacked**/
  repeated → passthrough untouched). Instead Axum routes the raw response body through
  the **same shared `content-encoding` inspection + decoder** the other adapters use
  (§3.4.1 policy table), enforcing the decompressed-byte cap incrementally. This is the
  only way all four adapters share one decompression contract.

**Portable `content-encoding` policy (identical on all four adapters).** CF and Fastly
already diverge here today, so the rule is stated normatively rather than left to each
adapter:

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

**Pipeline order — deadline wrapper OUTERMOST, decompressor INSIDE it (defined, not left
independent).** The two layers compose in exactly one order:
`platform raw byte stream → deadline-aware wrapper → decompressor → capped decoded output`.
- The **deadline wrapper is outermost**, wrapping the **RAW (still-compressed) read**, so a
  stall is preempted **before first decoded output, mid-stream, and at EOF** alike — and on
  Fastly it sits at the raw-read boundary the between-bytes bound actually governs (wrapping
  only *decoded* output would miss that boundary).
- Its timeout chunk is a **typed `EdgeError::gateway_timeout` (504)** (the `Body::Stream`
  error type is `EdgeError`, not `io::Error`). The **decompressor must PASS an inner
  `EdgeError` chunk through unchanged** — it maps only its OWN decode `io::Error` (malformed
  compressed data) to `bad_gateway` (502). It must NOT wrap/remap an inner `gateway_timeout`
  into `bad_gateway` (that is the "504 remapped to 502 inside the io::Error boundary" bug).
- **Precedence when both could fire:** the deadline (outer) wins — if the wrapper has yielded
  a `gateway_timeout`, the decoder sees that typed chunk and propagates it; malformed-compression
  → 502 only applies when the decoder actually reaches undecodable bytes *before* the deadline.
- §5.4 tests all four: a compressed **stall before first decoded byte**, **mid-stream stall**,
  **stall at EOF** → each `gateway_timeout` (504); **malformed compression with no stall** → 502;
  and **malformed-compression-vs-timeout precedence** (deadline fires first → 504, not 502).

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

#### 3.4.2 Inbound request bodies — moved

The inbound request-body reading contract (`RequestContext::body_bytes` / `json_within` / `form_within` and their bounded-drain + poison semantics) now lives in the dedicated **[inbound body design](2026-08-22-inbound-body-design.md)** spec. Outbound's `OutboundResponse::into_bytes_bounded` mirrors that pre-append accounting; the inbound spec is the authoritative contract.
#### 3.4.3 New `EdgeError` variants & mapping

`EdgeError` is `#[non_exhaustive]`, so this is additive.

```rust
// crates/edgezero-core/src/error.rs
// Phase 1a lands the first TWO variants (needed for deadline/transport mapping).
EdgeError::BadGateway { message: String }        // -> 502  (Phase 1a)
// GatewayTimeout carries a TYPED `cause`, NOT just a message: a fan-out consumer must
// tell a per-call timeout from a batch-deadline expiry from the harvested result WITHOUT
// parsing strings (§3.3.2). **Phase 1a MUST land this shape** (the `BudgetSource` enum +
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
// Derives + ALPHABETICAL order are COMPILE-VERIFIED (§3.3.2): `Clone`/`Copy` are required
// by `StoredError` (derive + `*cause`); alphabetical order satisfies the denied
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
| Inbound request body over limit / not valid JSON | `bad_request` | 400 |
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

#### 3.4.5 Inbound body migration — moved

The `RequestContext` / `BodyCell` migration (the `BodyState` / `BodyKind` state machine, `into_request()`, `take_body()`, sticky-poison semantics) now lives in the dedicated **[inbound body design](2026-08-22-inbound-body-design.md)** spec. **Outbound depends on it:** `OutboundRequest::from_request(ctx.into_request()?, uri)` (streaming proxy-forward) consumes the `BodyCell` contract defined there — see that spec for the authoritative `into_request()` / `BodyCell` behaviour.
### 3.5 Capability declaration

#### 3.5.1 Manifest section

```toml
# edgezero.toml
[capabilities]
required = ["outbound-http", "outbound-deadlines"]
optional = ["config-store"]

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
// The variants are grouped SEMANTICALLY (all outbound-* together, then the three
// store-* together) and this order is referenced by the matrix rows and
// footnote numbers. `clippy::arbitrary_source_item_ordering` wants alphabetical, which
// would scatter the groups; carry a documented `#[expect]` at the enum instead.
#[expect(clippy::arbitrary_source_item_ordering, reason = "capabilities grouped by kind, tied to the §3.5.2 matrix order")]
pub enum Capability {
    OutboundHttp,                       // can issue outbound HTTP at all
    OutboundDeadlines,                  // wall-clock budget on a *single* outbound
 // exchange: connect + headers + buffered
 // response body AND chunk-yield of a streamed
 // response body. For `send_all`,
 // this covers both the headers phase and the
 // **active body-drain phase** of each slot —
 // a slot's active drain still honours the
 // single-slot bound (≤ one between-bytes-
 // timeout overshoot per gap on Fastly
 //). The **cross-slot harvest delay**
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
 // split — documented deviation). Apps
 // with slow-connect-but-fast-rest workloads
 // require this and get a hard fail on Fastly.
    SendAllSlotIsolation,               // in `send_all`, each slot's result reflects
 // what it would have produced in isolation —
 // sibling-slot timing cannot turn a slot that
 // would have completed within its own
 // `budget.deadline` into a 504. Native on
 // Axum/CF/Spin; BestEffort on Fastly
 // (harvest-order false 504s in Buffered mode,
 //).
    StreamedUploadDeadlines,            // can preempt a stalled `stream.next().await`
 // while feeding a streamed REQUEST body
 // (Fastly = BestEffort)
    LazyStreamedResponsePassthrough,    // `into_response()` on a streamed body
 // delivers chunks without first collecting
 // the whole body. **Cloudflare is the only
 // `Native` adapter.** Axum = BestEffort
 // (non-Send `LocalBoxStream`, footnote 3),
 // Fastly = BestEffort (`stream_to_client`
 // incompatible with `#[fastly::main]`,
 // footnote 6), Spin = BestEffort (buffered
 // `FullBody` public response surface,
 // footnote 7). All three fall back to
 // bounded buffered passthrough.
    ConfigStore,                        // adapter can back a `[stores.config]`
 // binding — read-only key/value config
 // resolved at request time. Gated
 // pre-dispatch like the outbound
 // capabilities. Native on all four
 // adapters (matrix below;).
    KvStore,                            // adapter can back a `[stores.kv]` binding —
 // mutable key/value storage. Gated
 // pre-dispatch; Native on all four adapters.
    SecretStore,                        // adapter can back a `[stores.secret]`
 // binding — secret material surfaced to
 // handlers. Gated pre-dispatch; Native on
 // all four adapters.
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
 /// Fully supported with no documented caveats.
    Native,
 /// Real enforcement with a precisely documented, deterministic bound on any
 /// deviation. Used for timing-related degradations (e.g. Fastly
 /// outbound-deadlines body phase — overshoot ≤ one between-bytes-timeout
 /// interval).
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
// `#[non_exhaustive]` matches the existing manifest-struct precedent (`ManifestStores`
// et al. in manifest.rs are all `#[non_exhaustive]`) — it keeps future field additions
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
// contradiction). One of the failures `from_baked_json` -> Malformed and `config
// validate` surface. §5.4 tests: an unknown field (typo `require`), a duplicate, and a
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
/// `OutboundRequest`'s userinfo rejection ( — credentials must not leak
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
// returns `()` (validator contract), but provisioning (rendering `spin.toml`) and the
// build/serve/deploy DRIFT check both need the *canonical* form to compare — and they
// MUST NOT re-implement parsing (divergence = a manifest that validates but drifts, or
// drifts spuriously). So both delegate to one function that expands each manifest entry
// into a SET of ATOMIC `(scheme, host, port)` triples — because ONE manifest entry can
// render as MULTIPLE `spin.toml` entries (`"*"` = http AND https → two lines), a
// canonicalizer that returned a single multi-scheme value could never set-equal the two
// rendered lines. Atomic-and-flatten fixes that:
// PUBLIC + cross-crate: the consumers (provisioning in edgezero-cli, Spin drift validation
// in the adapter crate) are DIFFERENT crates, so the fn and ALL its types MUST be `pub` and
// exported from `edgezero-core` (a private `fn`/type would not compile at those call sites).
// Concretely:
//   - The error is a DEDICATED `pub enum HostParseError` — NOT the validator's
//     `ValidationError` (that would leak validator internals into the adapter crate, which
//     doesn't depend on `validator`). The MANIFEST validator is a thin wrapper that calls
//     `canonicalize_outbound_host` and maps `HostParseError -> ValidationError` at the
//     validator boundary only; provisioning/drift get the `HostParseError` directly.
//   - `AtomicHost` and ALL its component types are public: `pub struct AtomicHost` with
//     `pub scheme: Scheme`, `pub host: HostPat`, `pub port: Port`, and `pub enum Scheme`,
//     `pub enum HostPat` (`Any` | `Exact(String)` | `WildcardSubdomain(String)`),
//     `pub enum Port` (`Any` | `Exact(u16)`). `HostPat` is part of the surface (an earlier
//     draft omitted it). Derive `Hash, Eq, PartialEq` for the drift `HashSet`.
//   - Provisioning must not hand-inspect internals to build `spin.toml`, so `AtomicHost`
//     exposes a canonical rendering method: `pub fn render_spin_host(&self) -> String`.
//     **It OMITS a scheme-default exact port** (443 for `Https`, 80 for `Http`) so the
//     output is deterministic: `https://x` and `https://x:443` both canonicalize to
//     `{Https, Exact("x"), Exact(443)}` and BOTH render as **`"https://x"`** (NOT
//     `"https://x:443"` — explicitness is already lost at canonicalization, so the renderer
//     must not re-introduce a port that would then mismatch the manifest's `https://x`
//     form). A non-default port renders explicitly: `{Https, Exact("x"), Exact(8443)}` ->
//     `"https://x:8443"`. `Port::Any` -> `":*"` (e.g. `"https://x:*"`); `HostPat::Any` ->
//     `"*"`; `WildcardSubdomain("example.com")` -> `"*.example.com"`. Drift uses `Eq`/`Hash`
//     on the atomics; provisioning uses `render_spin_host`. Neither re-parses.
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
// provision-then-build round-trip for BOTH `"*"` and `:*` (provision renders, build must
// then report NO drift).
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
hard-fails) rather than failing to compile. The registry `Adapter` trait gains one method (`capability`). PR #269 has merged to
main, so the trait already carries the `provision` / config-validation surface; this
spec adds only `capability`:

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

 // Already present on main as of PR #269 — ELIDED here (signatures unchanged by this
 // spec, and NOT consulted for capability metadata; `capability(..)` is the only
 // method `ensure_capabilities` reads). The real trait carries ~12 methods, all with
 // default bodies: `provision`, `push_config_entries`(`_local`),
 // `read_config_entry`(`_local`), `validate_adapter_manifest`,
 // `validate_app_config_keys`, `validate_typed_secrets`, `merged_id_kinds`,
 // `single_store_kinds`. Their real signatures (e.g. `provision` takes
 // `(&self, manifest_root: &Path, adapter_manifest_path: Option<&str>,
 // component_selector: Option<&str>, stores: &ProvisionStores<'_>, dry_run: bool)
 // -> Result<Vec<String>, String>`) live in crates/edgezero-adapter/src/registry.rs;
 // do not re-declare them here.
}
```

This spec only adds `capability(..)`. The `provision` / config-validation methods are
owned by PR #269 and shown above purely so readers don't misread the `Adapter`
reference in §3.5.3 as an exhaustive declaration. The `Adapter::provision(..)` and
config-validation hooks referenced in §3.5.3 / §6 / §7 are called from the **sibling
pre-dispatch gates** on `run_provision` / `run_config_push_typed` /
`run_config_validate`, not from `Adapter::execute` (`run_config_diff_typed` is
**exempt** — read-only diagnostic, §3.5.3 command-class gating). (The pre-#269 checkout had no `provision` / `config`
surface; that fallback is now historical.)

Capability matrix (all four adapters):

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `outbound-http` | Native | Native | Native | Native |
| `outbound-deadlines` | Native | Native | BestEffort¹ | Native |
| `outbound-flexible-phase-budget` | Native | Native | BestEffort⁵ | Native |
| `send-all-slot-isolation` | Native | Native | BestEffort⁴ | Native |
| `streamed-upload-deadlines` | Native | Native | BestEffort² | Native |
| `lazy-streamed-response-passthrough` | BestEffort³ | Native | BestEffort⁶ | BestEffort⁷ |
| `config-store` | Native | Native | Native | Native |
| `kv-store` | Native | Native | Native | Native |
| `secret-store` | Native | Native | Native | Native |

¹ **Fastly `outbound-deadlines` is `BestEffort`, because it cannot be guaranteed on
every request.** The *warm* path (an already-registered/cached dynamic backend) has two
documented, deterministic overshoot bounds — real enforcement with a known finite
ceiling — and those bounds are stated below (common-case `total_ms ≥ 4` phase split; the
sub-4 ms degenerate branch adds `total_ms` to each, see §4.3 "Net guarantee"). **But the
FIRST request to a new host** calls `Backend::builder(..).finish()`, a synchronous host
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
cannot actually honour on a cold start. Apps that accept the warm-path bound declare it
**`optional`** (logged, never gated) and get the documented behaviour below. The
warm-path bounds:

**These bounds hold once the backend is registered (cached).** On the
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
  no per-slot harvest loop. Body phase overshoot ≤ one between-bytes-timeout
  interval (§3.3.4). **Streamed-upload-specific overshoot**: when the request
  body is `Body::Stream` and the upload drain leaves a tiny positive
  `budget.deadline.remaining()`, the post-upload headers wait can additionally
  cost up to one dispatch-time `first_byte_ms` interval before the cooperative
  check at the `wait()` boundary or the response-wrapper preemption fires
  (§4.3 "Response phase"). That overshoot is **one-shot**, not per-chunk —
  the response wrapper preempts at the first post-deadline read.
- **`send_all`** — `batch_now` is shared across slots so dispatch+headers carries
  `BATCH_DISPATCH_SLACK_MAX + ms_rounding` (≈ 26 ms when `total_ms ≥ 4`, §4.3
  "Dispatch-overhead slack"); body phase **once a slot is actively
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
specific path declare it required and get a hard build failure on Fastly per §3.5.3.
Apps that buffer their request bodies before calling `send` are unaffected — buffered
uploads use `Body::Once`, no `stream.next().await`, and fall under `outbound-deadlines`
(`BestEffort` on Fastly — footnote 1; warm-path bound documented, cold dispatch unbounded).

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

⁴ `send-all-slot-isolation` is `BestEffort` on Fastly for **two** reasons, not one.
**(a) Harvest-order body drain** (§3.3.4): a slot whose own `budget.deadline` would have
covered its body in isolation can still return `gateway_timeout` because an earlier slot's
body drain monopolised harvest. **(b) Cold sequential registration** (§4.3): dispatch is
sequential, and a first-time `Backend::builder(..).finish()` can block without a
guest-side bound (§4.3 honesty note), so **one cold slot's registration can delay LATER
slots from being dispatched at all** — degrading their *headers*-phase timing too, not just
the body phase. So the earlier claim that "only the body phase loses isolation and headers
stay correct per-slot" holds **only when every backend is already cached**; a cold-start
batch can lose isolation at dispatch. Both are why the capability is `BestEffort`. Apps
that need cross-slot result isolation declare this capability required and get a hard build
failure on Fastly per the round-5
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
(`OutboundResponse` carries only status / headers / body — §3.1.4), so the
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
This affects only the **response-out** direction; Spin's **outbound streamed-upload**
path is `Native` for `streamed-upload-deadlines` via the hand-built `wasi:http`
request in §4.4 (the SDK's high-level `send` is **not** used for streamed bodies —
it spawns an uncancellable body pump).

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

**Gating is by command CLASS, not "every adapter-selecting command".** Hard-failing on a
capability mismatch is correct only where the mismatch means the *outcome* is broken; it
is wrong where it would block an unrelated operation. The classes:

| Class | Commands | Gated? | Why |
| --- | --- | --- | --- |
| Runtime-producing / mutating | `build`, `serve`, `deploy`, `provision`, `config push`, **`config gc --yes`** | **YES** | These stand up or mutate a runtime that a capability mismatch would make silently broken — fail early. **`config gc --yes` is DESTRUCTIVE** (reclaims/deletes orphaned config-store entries) and adapter-selecting, so it is gated like `config push`. |
| Validation | `config validate` | **YES** | Catching exactly this mismatch is its purpose. |
| Read-only diagnostic | `config diff`, `auth status`, **`config gc` preview (`--dry-run`, the default)** | **NO — exempt** | Read-only. Blocking a diagnostic on a *runtime* capability mismatch prevents the diagnosis you need to fix it. **`config gc` in preview deletes nothing** — it reports what it WOULD delete, so it is exempt, exactly like `config diff`; only the destructive `--yes` form gates. |
| Credential | `auth login`, `auth logout` | **NO — exempt** | Credential lifecycle is orthogonal to runtime capabilities; a mismatch must never block credential cleanup. |

**`config gc` (untyped, adapter-selecting) — added to this matrix and to provenance-preserving
resolution.** It has a defaulted cwd-relative `--manifest` today, so it takes the same
`Option<PathBuf>` provenance change (§3.5.3) and calls `resolve_root_manifest`. Its capability
behaviour splits by mode: **preview (default) is exempt; `--yes` (destructive) is gated** —
matching the read-only-vs-mutating split used everywhere else in this table.

This **reverses an earlier draft that gated `config diff`** — a read-only command must not
hard-fail on an unrelated runtime mismatch.

So there are **five concrete gate sites** — one inside `execute(..)` **but only for the
runtime-producing actions** it dispatches (`build` / `serve` / `deploy`; **not** `auth`),
plus **four siblings** on `run_provision`, `run_config_push_typed`, `run_config_validate`,
and `run_demo`. `config diff` and all `auth` sub-actions are **exempt** (the `execute(..)`
gate must therefore branch on the action, not gate unconditionally).

> **Note — gate the *typed* entry points, and note validation has TWO public entries.**
> In the merged tree the bundled `run_config_push` is a **v1 stub that errors**; real
> config writeback happens in `run_config_push_typed`, and the adapter-selecting diff is
> `run_config_diff_typed` (`crates/edgezero-cli/src/config.rs`). Gating the stubs would
> enforce nothing. **Validation is worse:** `run_config_validate` *and*
> `run_config_validate_typed` are **both public entry points** — generated downstream
> CLIs call `run_config_validate_typed` **directly** (`examples/app-demo/crates/
> app-demo-cli/src/main.rs`), bypassing the bundled `run_config_validate` entirely. So a
> gate placed only on the bundled path is silently skipped by every typed CLI. The gate
> must be a **shared inner operation** both entry points call (e.g. a private
> `gated_validate(..)` invoked by both `run_config_validate` and
> `run_config_validate_typed`), not a check bolted onto one.
>
> **The three config commands are NOT symmetric — do not generalize the shared-op rule
> to all of them.** Per-command, precisely:
>
> | Command | Bundled entry | Typed entry | Gate |
> | --- | --- | --- | --- |
> | `config push` | `run_config_push` — a **v1 stub that returns `Err`**; it performs no writeback, so there is nothing to gate | `run_config_push_typed` — the real writeback | **Gate the TYPED path only.** No shared op is needed: gating an erroring stub enforces nothing. |
> | `config validate` | `run_config_validate` | `run_config_validate_typed` — **called directly by generated CLIs** | **Shared inner op** (`gated_validate`) that BOTH entries call — this is the only command where a shared op is required, because both paths really validate. |
> | `config diff` | — | `run_config_diff_typed` | **Never gated** (read-only, exempt by class). |
>
> So a bare `run_config_push` reference in §5.4 / §7 means **`run_config_push_typed`**;
> a bare `run_config_validate` reference means **the shared `gated_validate` op behind
> both entries**; `config diff` is never a gate site.

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

// 2. crates/edgezero-cli/src/provision.rs
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String> {
 // PARSE ONCE: bind the resolved manifest, gate THAT object, then thread the SAME
 // binding into dispatch — never a throwaway `load_manifest(args)?` re-parse (which would
 // double the I/O and risk gating a different on-disk state than we then act on).
    let manifest = resolve_root_manifest(args)?;   // shared resolver, §3.5.3 discovery invariant
    ensure_capabilities(&args.adapter, ManifestContract::from_opt(manifest.as_ref().map(|rm| &rm.manifest)))?;  // site 2
    run_provision_with(args, &manifest)            // reuse the binding, no re-parse
}

// 3. crates/edgezero-cli/src/config.rs — gate the TYPED path (the bundled
// `run_config_push` is a v1 stub that returns Err; gating it enforces nothing).
pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String>
where C: DeserializeOwned + Serialize + Validate + AppConfigMeta {
    let manifest = resolve_root_manifest(args)?;   // bind once
    ensure_capabilities(&args.adapter, ManifestContract::from_opt(manifest.as_ref().map(|rm| &rm.manifest)))?;  // site 3
    run_config_push_typed_with::<C>(args, &manifest)   // reuse the SAME parsed manifest
}

// 4. config validate is ADAPTER-LESS: `ConfigValidateArgs` has NO `adapter` field.
// It validates against EVERY adapter declared in [adapters]. Both public entries
// (bundled + typed) must route through ONE shared gated op, or the typed path —
// which generated CLIs call directly — silently skips enforcement.
fn gated_validate(ctx: &ValidationContext) -> Result<(), String> {
 // `Manifest::adapters` is a `BTreeMap<String, ManifestAdapter>` (manifest.rs) —
 // iterate its keys directly. (An earlier draft called a
 // `configured_adapter_names` accessor that does not exist anywhere.) BTreeMap
 // keys are ordered, so the failure reported is deterministic across runs.
    for adapter_name in ctx.manifest().adapters.keys() {
        ensure_capabilities(adapter_name, ManifestContract::Present(ctx.manifest()))?;  // site 4
    }
    do_validation_work(ctx)
}
// IMPLEMENTATION CONTRACT — parse ONCE, gate the parsed object, then continue with it.
// The gate must NOT re-parse the manifest as a side effect. `gated_validate` already
// holds a `ValidationContext` (parsed), so it gates `ctx.manifest()` directly and passes
// the SAME `ctx` to `do_validation_work` — no second load. Likewise `provision` already
// owns a `ManifestLoader`; `run_provision` loads the manifest once, gates that value, and
// threads it onward. `load_manifest(args)?` in the `run_provision`/`run_config_push_typed`
// gate calls above is the SINGLE load whose result the command reuses — not an extra
// parse. A gate that reparses would (a) double the I/O and (b) risk gating a different
// on-disk state than the command then acts on (TOCTOU).
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String> {
    gated_validate(&load_validation_context(args)?)
}
pub fn run_config_validate_typed<C>(args: &ConfigValidateArgs) -> Result<(), String>
// Bound matches the current public API — validation only deserializes + validates, so
// NO `Serialize`. Adding `Serialize` here would be a gratuitous breaking bound on the
// generated CLIs that call this directly. (The push path keeps `Serialize` because it
// serialises the config into the blob envelope; validation does not.)
where C: DeserializeOwned + Validate + AppConfigMeta {
    gated_validate(&load_validation_context(args)?)?;   // SAME shared gate
    /* …typed-specific validation… */
    Ok(())
}

// 5. crates/edgezero-cli/src/demo_server.rs — no manifest FILE exists; read the
// manifest baked in by `app!` (Hooks::manifest).
#[cfg(feature = "demo-example")]
pub fn run_demo() -> Result<(), String> {
 // baked ('static): BakedManifest -> ManifestContract via as_contract
    ensure_capabilities("axum", <App as Hooks>::manifest().as_contract())?;         // site 5
    /* …Axum runner… */
}

// NOT A GATE SITE — `config diff` is read-only (diagnostic class) and EXEMPT.
// pub fn run_config_diff_typed<C>(..) { /* no ensure_capabilities */ }
```

`run_demo` is feature-gated (`demo-example`) and always selects Axum implicitly, so its
gate hardcodes the adapter name and reads the **baked** manifest rather than a file.
Sites 1–5 are exhaustive **for the gated classes**; `config diff` and `auth *` are
deliberately absent (§3.5.3 command-class table).

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

Commands covered by the **five** gate sites above (one inside `execute(..)` — branching to skip `auth` — and four siblings on `run_provision` / `run_config_push_typed` / `run_config_validate` / `run_demo`). `config diff` and `auth *` are **exempt** (read-only / credential classes):

| PR-#269 command | Entry point | Gate site |
| --- | --- | --- |
| `edgezero build` | `run_build` → `execute(Action::Build, ..)` | `execute(..)` — **gated** |
| `edgezero serve` | `run_serve` → `execute(Action::Serve, ..)` | `execute(..)` — **gated** |
| `edgezero deploy` | `run_deploy` → `execute(Action::Deploy, ..)` | `execute(..)` — **gated** |
| `edgezero auth login` / `logout` / `status` | `run_auth` → `execute(Action::AuthLogin/Logout/Status, ..)` | **EXEMPT** (credential + read-only class). The `execute(..)` gate must **branch on the action** and skip the `Auth*` actions. |
| `edgezero provision` | `run_provision` → `Adapter::provision(..)` | `run_provision(..)` sibling — **gated** |
| `edgezero config push` | real writeback is **`run_config_push_typed`** in the downstream typed CLI (the bundled `run_config_push` is a v1 stub that errors) | `run_config_push_typed(..)` — **gated (TYPED entry only)**; the bundled stub is not a gate site (gating an erroring stub enforces nothing). *No shared two-entry op* — unlike `config validate`, only the typed push path performs work |
| `edgezero config diff` | `run_config_diff_typed` → resolves an adapter via `run_shared_checks` | **EXEMPT** (read-only diagnostic class). Reverses the earlier "gate `config diff`" draft — a read-only diff must not hard-fail on a runtime mismatch. |
| `edgezero config validate` | `run_config_validate` / `run_config_validate_typed` — **adapter-less** (`ConfigValidateArgs` has no `adapter` field); validates against **all configured adapters** in `[adapters]` | **gated** via the shared inner op (§ note above) that **both** the bundled and typed entries call, looping every configured adapter |
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
overrides `manifest()` to return `BakedManifest::from_baked_json(<crafted-json>)`.
it has no path, no `ManifestLoader`, and no way to find `edgezero.toml` at runtime. But
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
 /// Successfully parsed + finalized.
    Present(&'static Manifest),
 /// `manifest_json` returned Some, but it did not parse/finalize. An
 /// adapter/macro contract bug: the JSON came from `serde_json::to_string` on an
 /// already-validated `Manifest`, so this is unreachable unless the macro is broken.
    Malformed(&'static str),   // static reason, for the diagnostic
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
  is `None`, and a single core-global `OnceLock` is likewise rejected. *(This is a second,
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

**Support-level enforcement ladder (what `required` means).** `capability()` returns one of `Native` > `BoundedCooperative` > `BestEffort` > `Unsupported`. A capability in `required` is satisfied by **`Native` or `BoundedCooperative`** (both are *real* enforcement — `BoundedCooperative` has a precisely documented, deterministic bound); it **hard-fails** on `BestEffort` (real-world deviation the app must opt into) or `Unsupported`. `optional` never hard-fails — a `BestEffort`/`Unsupported` optional capability is logged, not gated. **Apps that need exact (Native-only) deadline enforcement just declare `outbound-deadlines` `required`:** no adapter reports `BoundedCooperative` for `outbound-deadlines` (Fastly is `BestEffort` — footnote 1), so a required `outbound-deadlines` is satisfied only by the `Native` adapters (Axum/CF/Spin) and hard-fails on Fastly. The separate `outbound-deadlines-exact` capability an earlier draft proposed (§8 risk 14) is therefore **no longer needed** — the downgrade closes that gap directly, with no ad-hoc “declare exactness” mechanism.

**Historical (pre-#269) shape — now superseded (PR #269 has merged to main):**
Before #269 landed, `Command::{Build, Serve, Deploy, Dev}` all dispatched through
the registry's `Adapter::execute(AdapterAction::{Build, Serve, Deploy}, ..)` plus
`Command::Dev`'s implicit-Axum runner, and the gate went at the top of each of
those four handlers (or the equivalent helper they called). #269 collapsed them
into the single `execute(..)` dispatcher plus the sibling gates in the table
above, which is now the active topology.

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
// INPUT TYPE: a LIFETIME-BEARING contract, NOT `BakedManifest`. The file-backed gate
// sites (1–4) hold a **local** `&Manifest` borrowed from a loader — those are not
// `'static`, so they cannot be wrapped in `BakedManifest::Present(&'static Manifest)`.
// Only `run_demo` has a `'static` (baked) manifest. So the gate accepts a borrow of any
// lifetime, and `BakedManifest` (which is `'static`) converts INTO it:
//
// #[non_exhaustive]  // future states must fail closed in the cross-crate CLI match
// pub enum ManifestContract<'a> {
// None, // no contract → proceed (legit: no manifest)
// Malformed(&'static str), // corrupt baked contract → fail closed
// Present(&'a Manifest), // any lifetime — file-backed OR baked
// }
// impl BakedManifest {
// pub fn as_contract(&self) -> ManifestContract<'_> { /* Absent→None, Malformed→Malformed, Present→Present */ }
// }
// impl<'a> ManifestContract<'a> {
// pub fn from_opt(m: Option<&'a Manifest>) -> Self { m.map_or(Self::None, Self::Present) }
// }
// Both are `pub` (and `ManifestContract` + `as_contract` live in edgezero-core): the
// gate sites that call them — `run_provision` / `run_config_push_typed` / `gated_validate`
// in edgezero-CLI — are a DIFFERENT crate, so a private `fn` would not be callable.
// File-backed callers build `ManifestContract::Present(local_ref)` / `None` directly;
// `run_demo` calls `<App as Hooks>::manifest.as_contract`.
// `pub(crate)` — defined in `src/adapter.rs` and imported by the sibling gate sites
// (`run_provision`, `run_config_push_typed`, `gated_validate`, `run_demo`); a bare
// module-private `fn` could not be imported across those modules.
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
 // is wired. If it declares any required/optional capabilities, we
 // cannot answer `capability(..)` and must fail closed.
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
runtime-producing command (`build` / `serve` / `deploy` / `provision` / `config *`) cannot
reach the gate with `None` **while a real `edgezero.toml` exists at the project root**.
Today the CLI resolves the default manifest as `./edgezero.toml` (cwd-relative), but the
Spin adapter independently walks **upward** for `spin.toml` — so `edgezero build --adapter
spin` run from a nested subdirectory finds no `./edgezero.toml` (→ `None` → capability +
host-drift checks skipped) yet Spin still discovers the root `spin.toml` and builds. That
is a **capability-enforcement bypass from a nested cwd.** Fix: these commands must perform
**root-manifest discovery** — walk up from cwd to the first `edgezero.toml` (the same
upward search Spin does for `spin.toml`, anchored at the same root) — before gating.
`None` is then reserved for the genuinely-manifestless cases (`demo` / hand-written
`Hooks`).

**Discovery must be ONE shared resolver, not per-command.** Today `build` uses
`load_manifest_optional` while `provision` and `config` call `ManifestLoader::from_path`
independently — three code paths, so a fix to one leaves the bypass open on the others. So
introduce a **single** `pub fn resolve_root_manifest(source: ManifestSource) ->
Result<Option<ResolvedManifest>, String>`. **The `Option` is load-bearing:** discovery can
end in a genuine **`Ok(None)`** — no `edgezero.toml` found walking up to the filesystem root,
which is legitimately "no manifest / no capability contract → proceed" — distinct from
`Err(..)` (a manifest was found but is malformed/invalid). `Ok(Some(rm))` is a found+parsed
manifest. (An earlier draft returned `Result<ResolvedManifest, ..>`, which could not
represent the absent case and mismatched the `.as_ref()` gate call sites; corrected here.)
**Its input is PATH PROVENANCE, not a bare `PathBuf`** —
today several commands are handed `PathBuf::from("edgezero.toml")` as the *default*, so a
resolver that only sees a `PathBuf` cannot tell "user omitted `--manifest`" (walk up) from
"user explicitly passed `edgezero.toml`" (use verbatim). Both types are **`pub`** (cross-crate
+ typed-caller seam) and concretely defined:
```
pub enum ManifestSource {
    ExplicitFlag(PathBuf),  // --manifest given → use verbatim, no walk
    EnvVar(PathBuf),        // EDGEZERO_MANIFEST set → use verbatim
    Defaulted,              // neither → walk up from cwd
}
/// The resolved manifest, carried through gating AND execution so nothing re-reads it.
pub struct ResolvedManifest {
    pub path: PathBuf,        // the file actually found/used
    pub manifest: Manifest,   // parsed + validated ONCE
}
```
`ManifestSource` is `pub`, so a typed library caller constructs it directly (not "construct
a private enum"). The resolver returns `Option<ResolvedManifest>` **by value**; each gated
command binds it once — `let resolved = resolve_root_manifest(source)?;` — gates on
`ManifestContract::from_opt(resolved.as_ref().map(|rm| &rm.manifest))` (so `None` → proceed,
`Some` → check), and then threads the SAME `resolved` into execution — **no second load**
(the old `load_manifest_optional`/`ManifestLoader::from_path` reload is removed). `config diff` is capability-gate-EXEMPT but **still calls `resolve_root_manifest`**
(it needs the manifest to diff), so it is in the resolver-discovery matrix even though it is
not in the capability-gate matrix.
built at arg-parse time. **The concrete arg change is load-bearing and must be done first:**
today the `--manifest` arg is `#[arg(default_value = "edgezero.toml")] manifest: PathBuf`,
which **materializes the default into the `PathBuf`** so after derive-parsing, "omitted" and
"explicitly `edgezero.toml`" are indistinguishable. So change it to
**`#[arg(long)] manifest: Option<PathBuf>`** (NO `default_value`): `Some(p)` → `ExplicitFlag(p)`,
`None` → consult `EDGEZERO_MANIFEST` (→ `EnvVar`) else `Defaulted`. Do this on the args that
have the flag (`provision`, `config`); `build`/`serve`/`deploy` have **no `--manifest` field
today** and read `EDGEZERO_MANIFEST` — either add the `Option<PathBuf>` flag to them too or
keep them env-only, but the `ManifestSource` they build is `EnvVar`-or-`Defaulted`. **Typed
library callers** (not going through clap) construct `ManifestSource` directly — the enum is
the public seam, so they get the same precedence without a `PathBuf` ambiguity. Precedence:
**(1) `ExplicitFlag` → use verbatim, no walk; (2) else `EnvVar` (`EDGEZERO_MANIFEST`, which
`build`/`serve`/`deploy` — that today have NO `--manifest` field — already honour) → use
verbatim; (3) else `Defaulted` → walk up from cwd to the first `edgezero.toml`; (4) none up
to the filesystem root → the genuine `None`.** Every gated command
(`build`/`serve`/`deploy`/`provision`/`config push`/`config validate`) constructs a
`ManifestSource` and calls the one resolver — no per-command `load_manifest_optional` vs
`ManifestLoader::from_path` divergence. (Note the two `provision`/`config` gate snippets
above call `resolve_root_manifest(args)` as shorthand for "build the `ManifestSource` from
`args`, then resolve".) The resolved manifest is both gated and threaded into execution
(parse-once). §5.4 adds a row **per gated command**: each, run from a nested subdirectory of
a project whose root requires a capability the adapter lacks — under BOTH a `Defaulted`
source AND an `EDGEZERO_MANIFEST` pointing at the root — must **still fail**.

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
  `config-store` and friends stay optional.

#### 3.5.4 Outbound host plumbing — not policy

`[capabilities.outbound].hosts` is **plumbing**, not a security allowlist (non-goal §1.3).
Apps still enforce their own target allowlist in handler code. Adapter use of `hosts`:

- **Spin** requires `allowed_outbound_hosts` in `spin.toml`. The Spin adapter renders
  each entry per the rules below. (`spin.toml.hbs:13` currently hardcodes the literal
  `["https://*:*"]` — not a Handlebars expression; that line becomes a render of this
  list.)

  **Synchronization lifecycle — `provision` writes, `build`/`serve`/`deploy` validate.**
  Rendering at scaffold time alone is **not sufficient**: `spin.toml` is generated only
  by `edgezero new`, which **cannot re-run** over an existing project
  (`ProjectLayout::new` hard-errors with `OutputDirExists`). So a user who later edits
  `[capabilities.outbound].hosts` would keep a **stale** `allowed_outbound_hosts` and
  the Spin app would silently over- or under-permit at runtime. The lifecycle:

  1. **`edgezero new`** renders the initial list from the manifest (as today, but from
     `[capabilities.outbound].hosts` instead of the hardcoded literal).
  2. **`edgezero provision`** (re-)renders `allowed_outbound_hosts` into `spin.toml`.
     This is the **only** command that writes it. `provision` already edits `spin.toml`
     in place via `toml_edit::DocumentMut` (`ensure_kv_label_in_component`) and already
     honours `--dry-run`. **But `Adapter::provision` receives no host data** — its
     signature is `(manifest_root, adapter_manifest_path, component_selector,
     stores: &ProvisionStores, dry_run)`, and `ProvisionStores` carries only store info,
     **not** `[capabilities.outbound].hosts`. So my earlier "no trait-signature change"
     was wrong; the hosts must be made reachable. **A re-read from `manifest_root` does
     NOT work** — `manifest_root` is `args.manifest.parent()` (`provision.rs`), i.e. the
     *directory* with the filename discarded, but `--manifest` accepts an **arbitrary
     filename** (`args.rs`; default `edgezero.toml`). Re-reading `manifest_root/edgezero.toml`
     would read the **wrong file** for a project invoked with `--manifest custom.toml`.
     So the hosts (or the full manifest path) **must be threaded in**, two options:
     (a) **add `hosts: Option<&[String]>` to the provision context** (widen
     `ProvisionStores`, or add a parameter) — the CLI already has the parsed manifest at
     the call site, so it passes the hosts directly; **no re-parse, no wrong-file risk**;
     or (b) thread the **full manifest path** (`args.manifest`, not its parent) so the
     adapter re-reads the correct file. Both are trait-surface changes touching the four
     adapters' `provision` signatures. **LOCKED: option (a), via a new field on
     `ProvisionStores` — NOT a new parameter.** Exact change: add
     `hosts: Option<&'a [String]>` to `struct ProvisionStores<'a>`, keeping
     `Adapter::provision`'s **arity and signature unchanged**
     (`..., stores: &ProvisionStores<'_>, dry_run: bool`) — adapters only ever *receive*
     `&ProvisionStores` and read fields, so a new field is transparent to them, whereas a
     6th positional parameter would break every existing `impl`. **Source-compatibility
     caveat (this is the fix for the earlier over-claim):** `ProvisionStores` is today a
     plain `pub struct` with public fields, so simply adding a field is **not** additive —
     it breaks every *struct-literal* construction (`ProvisionStores { config, kv, secrets
     }`). Two in-tree consequences must be handled as part of this change: (i) add
     **`#[non_exhaustive]`** to `ProvisionStores` and a **`ProvisionStores::new(..)`
     constructor** so **future** field additions are non-breaking; (ii) but adding
     `#[non_exhaustive]` + a constructor does **NOT** save the **current** call sites —
     **THIS field-add is a one-time source break of every existing struct literal**
     (`ProvisionStores { config, kv, secrets }`), ~17 of them in-tree today. All ~17 must
     be migrated to `ProvisionStores::new(config, kv, secrets, hosts)` in the same change.
     (**NOT** an `..Default::default()` spread — that does not work here: `#[non_exhaustive]`
     forbids the struct-update `..` form across crates, and `ProvisionStores` has no
     `Default` impl anyway. The constructor is the only migration path.) **This is a PUBLIC-API break, not
     an in-tree-only migration** (correcting an earlier draft): `ProvisionStores` is a
     `pub struct` passed by the **public** trait method `Adapter::provision`, so any
     **out-of-tree** adapter or its tests that construct a `ProvisionStores { .. }` literal
     — to call `provision` directly, or to build a fixture — also breaks. In-tree we know
     it's ~17 literals; out-of-tree the count is unknown. So: **record this as a breaking
     public-API change** with release implications — it needs a minor/major version bump
     per the crate's policy and a CHANGELOG entry telling downstream adapter authors to
     migrate their literals to `ProvisionStores::new(..)`. `#[non_exhaustive]` (added now)
     makes it the **last** such break — future fields are additive — but it does not
     retroactively spare today's callers, in-tree or out. Do not call this "additive."
     Option (b) is rejected, and "add a parameter" is rejected as arity-breaking.* Sibling fields and comments are preserved (test-pinned, on a fixture
     that already contains `allowed_outbound_hosts`). Writing at provision matches
     EdgeZero's model: *platform manifests are written during provision, not build.*
  3. **`edgezero build` / `serve` / `deploy`** **validate only — they never write.**
     They compare `spin.toml`'s `allowed_outbound_hosts` against the manifest and
     **hard-fail on drift** with an actionable message that renders the expected list
     verbatim for the user to paste (or to fix by re-running `provision`).

  **Why validate-not-rewrite on the build path.** `spin.toml` is **git-tracked and
  user-owned** — the scaffold's `.gitignore` does not exclude it, and real projects
  hand-edit it (the demo's carries hand-written `[variables]`, a custom `source`, and
  comments). Rewriting it on every build would dirty the working tree and **clobber
  intentional edits**. Validation enforces correctness without taking ownership of the
  user's file.

  **Drift comparison is over canonicalized SETS, not raw strings.** Compare after
  applying the canonicalization below (lowercase scheme/host, strip default ports),
  order-insensitively. A raw string compare would false-positive on
  semantically-identical spellings (`https://x:443` vs `https://x`, or a reordered
  list) and make the gate unusable.

  > **⚠️ The hook must live in `edgezero_cli::adapter::execute`, NOT in
  > `SpinCliAdapter::execute`.** The scaffolder **always** writes
  > `[adapters.spin.commands].build/deploy/serve` into `edgezero.toml`, so for any
  > scaffolded project `edgezero build|serve|deploy --adapter spin` takes the
  > `manifest_command` → `run_shell` branch and **`SpinCliAdapter::execute` is never
  > reached**. A validation hook placed in the adapter would be **dead code**. Put it in
  > `edgezero_cli::adapter::execute` *before* the `manifest_command` branch — the same
  > seam `ensure_capabilities` uses (§3.5.3), which is the only funnel that sees the
  > parsed `Manifest` **and** still fires for shell-overridden commands.

  **Default when `[capabilities.outbound].hosts` is absent: `["https://*:*"]`** — i.e.
  **preserve today's https-only wildcard**. The default is deliberately **not** widened
  to include `http://*:*`: doing so would silently grant every existing app permission
  to make **cleartext** outbound calls, and a security posture must not broaden as a
  side effect of a refactor. Apps that need cleartext outbound declare it explicitly.

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
     by `budget.deadline`; the wrapper yields a `gateway_timeout` (attributed via `budget.cause`, §3.3.2) error
     chunk past the deadline so the streamed body honours the deadline
     end-to-end per §3.3.3.
- Errors: `reqwest` timeout → **`gateway_timeout_caused(msg, budget.cause)`** (carries the
  attribution — §3.3.2, NOT bare `gateway_timeout`); connect/DNS/TLS → `bad_gateway`;
  response over-cap → `response_too_large` (distinct kind, 502 — §3.4.1). Any completed exchange (incl. non-2xx) → `Ok`.
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
  **deterministically bounded** on the warm/cached path (so the documented best-effort
  bound Fastly advertises for `outbound-deadlines` is actually true there, not just
  usually-tight — the capability itself is `BestEffort` because the cold path below has no
  such bound):

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
    preflight, a map lookup, SDK setup. It is genuinely bounded, the guard is a
    belt-and-suspenders assertion, and the warm-path deadline holds
    with the stated `BATCH_DISPATCH_SLACK_MAX + ms_rounding` bound (this is the
    documented behaviour an app opts into by declaring `outbound-deadlines` **optional**).
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
  request to a new host — target Axum/CF/Spin, which arm their timers from
  `budget.deadline.remaining()` with no
  blocking registration step.

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
  absolute-deadline enforcement on the dispatch+headers phase target a different
  adapter (Axum/CF/Spin all use `budget.deadline.remaining()` at arming time —
  see §4.1 / §4.2 / §4.4 step 3). **Collision detection** is
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
      Timeout,           // DnsTimeout | ConnectionTimeout | HttpResponseTimeout
      Transport,         // DnsError | Destination* | Connection{Refused,Terminated,LimitReached} | Tls*
      UpstreamProtocol,  // HttpIncompleteResponse | Http*TooLarge | HttpStatusInvalid | Http2StreamError | ...
      LocalInvariant,    // HttpRequestUriInvalid | HttpRequestCacheKeyInvalid | HttpCacheLimitExceeded | HttpCacheApiUnsupported | InternalError
      Unknown,           // Custom, and any future #[non_exhaustive] variant
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
  invariant-violation variants above. The §5.4 row asserts `internal` appears **only**
  for: (a) `BATCH_DISPATCH_SLACK_MAX` overshoot, (b) the unfilled-slot harvest
  invariant, (c) the `NameInUse` external-registration case, **(d) the
  clamp/name/encoding `BackendCreationError` variants, and (e) `SendFailure::LocalInvariant`**
  — all of which are EdgeZero-invariant violations, which is exactly what `internal` is
  reserved for.

  **`DnsTimeout` is 504, not 502** — the one genuinely ambiguous cause. It names DNS
  (transport-shaped, which reads 502) but it **is a fired timer**, and the whole point
  of the 502/504 split is that a fan-out caller retries a *timeout* differently from an
  *unreachable* upstream. Classify by **"did a timer fire?"**, not by which subsystem
  reported it. A DNS answer of "no such host" is 502; a DNS lookup that ran out of time
  is 504.

  Rationale: a fired host timer is a **deadline** outcome (504) and must be
  distinguishable from an upstream that was unreachable (502) — the fan-out caller
  retries those differently. `EdgeError::internal` **is** correct for a **narrow** set
  of send-stage causes — the `SendFailure::LocalInvariant` group (`HttpRequestUriInvalid`,
  `HttpRequestCacheKeyInvalid`, `HttpCache*`, `InternalError`) — because those mean
  *EdgeZero* built a bad request or hit an SDK-internal fault, i.e. an adapter bug, not
  an upstream failure. It is **never** correct for a *transport/upstream* cause
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
      starts flowing; apps that need real-time enforcement against a slow origin
      **write path** for streamed uploads need to size `max_request_body_bytes`
      small enough that a stalled write cannot exceed the auction tolerance,
      *or* target a different adapter.
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

    This is a deliberate, documented Fastly-specific behaviour of streamed
    uploads: apps that need tight end-to-end wall-clock should pass a buffered
    request body (`Body::Once`) so the timeouts are set with the full budget
    known and no upload-time eating happens.
- `capability()` per §3.5.2: `outbound-http` = `Native`, `outbound-deadlines` =
  **`BestEffort`** (footnote 1 — the warm/cached path has a documented deterministic
  bound, but the FIRST request to a new host calls `Backend::builder(..).finish()`, a
  host call that can block on a service-wide backend slot and overshoot the deadline
  before the guard runs; since `capability()` is static and cannot tell cached from cold,
  the honest value is `BestEffort`, so a `required outbound-deadlines` correctly hard-fails
  on Fastly rather than fooling the gate),
  `outbound-flexible-phase-budget` = `BestEffort` (footnote 5 — rigid 1/4 connect +
  3/4 first-byte split per §4.3 can fail a request that would have fit within the
  total budget), `send-all-slot-isolation` = `BestEffort` (footnote 4 —
  buffered-body harvest order can produce false 504s),
  `streamed-upload-deadlines` = `BestEffort` (footnote 2 — no preemption of a
  stalled `stream.next().await`), `lazy-streamed-response-passthrough` =
  `BestEffort` (footnote 6 — Fastly's `Response::stream_to_client()` is
  incompatible with `#[fastly::main]`, so the default scaffold falls back to
  buffered passthrough; lazy streaming requires a non-`#[fastly::main]` entry),
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
  via `join_all` over `send_one` (each of which drives the hand-built `wasi:http`
  request + `wasip3::http::client::send` — see below and §4.4); the wasi async reactor
  fans out. Concurrency materialises only under the real Spin/wasi executor — see
  §5.3 for the test consequence.
- `send_one(req, now)`: build the hand-built `wasi:http` request (§4.4 — all body
  kinds, buffered and streamed); compute the budget via the
  core helper `dispatch_budget(req, now)` (§3.3.2); race the **whole** operation
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
  chunk), which the deadline race can cancel. This keeps `streamed-upload-deadlines`
  genuinely `Native` for both body kinds.

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

  The adapter therefore **builds the `wasi:http` request by hand** for streamed bodies
  and keeps the body pump **inside** the raced future, so a single drop cancels
  everything. All of this is public API — `spin_sdk` re-exports `wasip3`, `wit_stream`,
  and `wit_future`:

  ```rust
  use spin_sdk::wasip3::{http::{types, client}, wit_stream, wit_future};
  use futures::future::{select, Either};

  let (mut writer, contents_rx) = wit_stream::new::<u8>();
  let (trailers_tx, trailers_rx) = wit_future::new(|| Ok(None));

  let opts = types::RequestOptions::new();
  let _ = opts.set_connect_timeout(Some(connect_ns));   // transport-only; see note below

  // Bound as `wasi_req`, NOT `req` — the WASI request must not shadow the OUTBOUND
  // request `req`, whose `max_request_body_bytes` / `body` we read below.
  let (wasi_req, _transmit) = types::Request::new(headers, Some(contents_rx), trailers_rx, Some(opts));
  // The wasip3 setters return `Result<(), ()>` (unit error), so bare `?` does NOT
  // compile in an `EdgeError`-returning context — map the `()` to a concrete error.
  let bad = |()| EdgeError::internal(anyhow::anyhow!("invalid outbound request component"));
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
  let pump = async move {
      let mut sent: u64 = 0;   // u64 accounting vs the u64 cap (`max_req`) — usize is u32
                               // on wasm32, so a usize counter could wrap below the cap.
 // `source` yields `Option<Result<Bytes, EdgeError>>`
 // (the error-type change). The item MUST be unwrapped — a source error is a real
 // failure (`bad_gateway` from the wrapped stream, or a `gateway_timeout` chunk), not
 // a `Bytes`. Dropping it would silently upload a truncated body.
      while let Some(item) = source.next().await {                // cancellable
          let chunk: Bytes = item?;                               // propagate source error
 // pre-append cap check against max_request_body_bytes (u64; no `as`, use try_from)
          let chunk_len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
          if sent.checked_add(chunk_len).is_none_or(|n| n > max_req) {
              return Err(EdgeError::bad_request("request body exceeded max_request_body_bytes"));
          }
 // checked: bare `+=` trips `clippy::arithmetic_side_effects` (denied).
          sent = sent.saturating_add(chunk_len);
          let unwritten = writer.write_all(chunk.to_vec()).await; // backpressure; cancellable
          if !unwritten.is_empty() {
              // READER GONE is NOT an error. The origin stopped reading — almost always
              // because it is about to send (or already sent) an EARLY FINAL response
              // (413 Payload Too Large, 401, a redirect, …). Returning an error here
              // would DISCARD that valid response and report 502 instead, violating
              // "a completed exchange, including non-2xx, is Ok". So end the pump
              // cleanly and let `send` surface the response.
              drop(writer);
              return Ok::<(), EdgeError>(());
          }
      }
      drop(writer);                                               // EOF
      let _ = trailers_tx.write(Ok(None)).await;                  // completion signal
      Ok::<(), EdgeError>(())
  };

  // ORDERED race, NOT `join!`. `join!` waits for BOTH, so it could (a) delay an
  // already-available response until a stalled *source* pull finally ends, or (b) let
  // the pump's outcome override a valid early final response. The rule: a completed
  // `send` (Ok OR Err) IS the exchange result — the origin has spoken, the upload is
  // moot; a pump SOURCE error / cap-overflow only matters if it happens BEFORE any
  // response. (Reader-gone is Ok in the pump above, so it never competes.) No `loop`:
  // every branch returns, so a `loop` here would trip `clippy::never_loop`.
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
  let exchange = async move {
      let mut send_fut = pin!(client::send(wasi_req));
      let pump_fut = pin!(pump);
      match select(send_fut, pump_fut).await {
          // send finished first — the answer. **DROP the losing pump future BEFORE
          // awaiting `to_core`.** `pump_still` is a NAMED binding (not bare `_`), so it
          // would otherwise stay alive across `to_core(..).await`, keeping the `wit_stream`
          // upload `writer` open — and a buffered-response drain in `to_core` could then
          // wait on that still-live writer. The explicit `drop` fires `stream.cancel-write`
          // and cancels the pending `source.next()` NOW, so the response drain never blocks
          // on a moot upload.
          Either::Left((send_res, pump_still)) => {
              drop(pump_still);
              match send_res.map_err(|e| map_spin_send_err(e, budget.cause)) {
                  Ok(wasi_resp) => to_core(wasi_resp).await,
                  Err(e) => Err(e),
              }
          }
          // pump errored before a response — a real client-side upload failure
          // (source yielded Err, or cap overflow); no response can be trusted.
          Either::Right((Err(pump_err), _send)) => Err(pump_err),
          // pump finished OK (full body sent, or reader-gone) — await the STILL-PENDING
          // send future returned by `select` (awaiting `send_fut` directly would be a
          // second mutable borrow).
          Either::Right((Ok(()), send_still)) => match send_still.await.map_err(|e| map_spin_send_err(e, budget.cause)) {
              Ok(wasi_resp) => to_core(wasi_resp).await,
              Err(e) => Err(e),
          },
      }
  };

 // `remaining` is Option<Duration>, NOT Result — `?` here would not compile in a
 // Result-returning fn. An already-expired budget must become gateway_timeout
 // explicitly, matching the "expiry before dispatch" contract above.
  let Some(remaining) = budget.deadline.remaining() else {
      // Attribute via the budget's cause — every budget timeout is caused (§3.3.2).
      return Err(EdgeError::gateway_timeout_caused(
          "deadline expired before upload dispatch", budget.cause));
  };

  match select(pin!(exchange), pin!(spin_sdk::time::sleep(remaining))).await {
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

  **Why this earns `Native`.** Dropping `exchange` drops **both** halves: `client::send`'s
  subtask fires the `[subtask-cancel]` canonical-ABI intrinsic (a *synchronous* host-side
  teardown of the in-flight request), and the pump's in-flight `write_all` fires
  `stream.cancel-write` while its pending `source.next()` future is simply dropped.
  Nothing is left spawned, so the component task exits cleanly. Preemption is bounded by
  **one monotonic-clock tick** past `budget.deadline` — for both a stalled *source pull*
  and a stalled *host write*. This is the same guarantee the old (impossible) two-race
  design claimed, obtained by making the whole exchange one cancellable future instead.

  **Completion signalling.** EOF is `drop(writer)`; the result/trailers channel is
  `trailers_tx.write(..)`. (This is the SDK's own `BodyWriter` reimplemented **without**
  the `spawn`.) The `max_request_body_bytes` cap (default 8 MiB) is enforced with
  pre-append checked accounting **inside the pump loop**, `bad_request` on overflow.

  **`RequestOptions` do not bound the upload.** WASI 0.3 keeps `set-connect-timeout` /
  `set-first-byte-timeout` / `set-between-bytes-timeout`, but these are transport /
  response-side only — the WIT states they are *"separate from any the user may use to
  bound an asynchronous call."* They are **not** a substitute for the race above; set
  `connect_timeout` as a courtesy, and let the raced timer own the deadline.

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
    pinned SDK pins every timeout variant to 504 — a fired timer is a deadline outcome, distinguishable
    from an unreachable upstream (the fan-out caller retries them differently).
  - DNS resolution failure, connection refused/terminated, TLS/certificate errors,
    destination-not-found/unavailable → **`bad_gateway` (502)**.
  - Locally-invalid request (bad URI/settings we constructed) / internal → **`internal`
    (500)** — an EdgeZero bug, not an upstream failure.
  - **Future/unknown variant handling — consistent with the no-`_` rule above (this bullet
    previously contradicted it; corrected).** **Every variant in the pinned
    `wasi:http@0.3.0` `ErrorCode` is explicitly named** in one of the three buckets (504
    timeout / 502 upstream+transport+protocol / 500 locally-invalid), so the match is
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
    timeouts → 504; `configuration-error` / `HTTP-request-*` we constructed / `internal-error`
    → 500; **everything else upstream/transport/protocol** → 502. The **exhaustive classifier
    test enumerates every pinned variant** and asserts its bucket, so an added SDK variant
    that we forgot to map trips the test (via a `#[deny(unreachable_patterns)]`-style
    round-trip or an explicit variant list), not silently defaults.
  - The separate wasi-timer we race the exchange against (§4.4) also yields
    `gateway_timeout` on expiry. **request**-body over-cap → `bad_request` (400);
    **response**-body over-cap (decompressed) → `response_too_large` (distinct kind, 502, §3.4.1). Any
    completed exchange (incl. non-2xx) → `Ok`.
- Spin requires `allowed_outbound_hosts`; the adapter renders it from
  `[capabilities.outbound].hosts` per §3.5.4 when generating `spin.toml`.
- `capability()` per §3.5.2: `Native` for **eight of the nine**;
  `lazy-streamed-response-passthrough` is **`BestEffort`** (footnote 7). Spin's wasi
  monotonic-clock timer covers `outbound-deadlines` and `streamed-upload-deadlines`;
  the single wasi-timer race is one total budget (no per-phase split), so
  `outbound-flexible-phase-budget` is `Native` too; and `join_all` of
  `send_one` futures fans out body drains concurrently so
  `send-all-slot-isolation` is `Native`. `config-store` / `kv-store` /
  `secret-store` are `Native` for Spin too.
- **Response-out passthrough is buffered (BestEffort), not lazy.** Spin's public
  response surface is `Response<FullBody<Bytes>>` (`SpinFullResponse`, used by
  `AppExt::dispatch` / `request::dispatch*` / `from_core_response` / `run_app`), so
  lazy passthrough would require a breaking public-API migration plus a WASI-0.3
  rewrite — deferred (footnote 7, §8 risk 13). The converter therefore drains the
  wrapped `Body::Stream` to `Bytes` within `SPIN_RESPONSE_STREAM_BUFFER_BYTES`
  (16 MiB); over-cap → `response_too_large` (502, §3.4.1). The **outbound streamed-upload** path
  above is unaffected and stays `Native`.

## 5. Test plan

CLAUDE.md forbids tests needing a network connection or platform credentials. That rule
is **unqualified today**, so this spec's blocking Axum Tier 3 test (a loopback mock
origin) would violate it as written. **Resolution (locked): amend CLAUDE.md** to qualify
the rule — "network" means the **public internet**; a **loopback / `127.0.0.1` mock
origin bound to an ephemeral port** is explicitly permitted (no external connectivity, no
credentials). That amendment is a deliverable of this change (§7 *Project meta* / the
CLAUDE.md refresh), not an assumption. A locally-spawned loopback mock origin is how real
fan-out concurrency and wall-clock timing are proven — an in-process transport fake was
considered and rejected because it cannot exercise real socket concurrency, which is the
one thing Tier 3 exists to prove. Tests are tiered.

**Tiers are defined by *owning crate and runtime*, not by abstraction level.** An earlier
draft defined Tier 1 as "core-only" while §5.4 assigned CLI-gate, registry, `demo`, and
Spin-render rows to it — those do not live in `edgezero-core` and could not have run
there. The tiers below are the ones the rows actually map onto.

### 5.1 Tier 1 — host-native, no platform runtime

**Owning crates:** `edgezero-core` (in-crate `#[cfg(test)]`, plus `MockOutboundClient`
behind the existing `test-utils` feature), `edgezero-adapter` (registry + `capability()`),
and `edgezero-cli` (the five capability gates, manifest parsing/validation, the Spin
`allowed_outbound_hosts` render + drift comparison — pure `toml_edit` logic needing no
Spin runtime). Runs under `cargo test --workspace --all-targets`; async tests use
`futures::executor::block_on`.

`MockOutboundClient` is scripted per request: status, headers, body, byte size, simulated
failure, simulated latency, and compressed-payload simulation.

> **⚠️ Tier 1 CANNOT prove `send_all` behaviour. There is no shared `send_all`.**
> Every adapter implements `send_all` **independently** (Fastly's dispatch-all-then-harvest
> engine has nothing in common with the others' `join_all`), so `MockOutboundClient` has no
> shipped orchestration to exercise — it only exercises **its own** implementation. A Tier 1
> `send_all` row therefore proves the **contract shape** (index alignment, `send_all(vec![])`
> → `vec![]`, per-slot `Ok`/`Err`, partial-failure isolation) against a *reference*
> implementation, and proves **nothing** about the four shipped ones. Earlier drafts claiming
> Tier 1 validated "the shared `send_all` logic" were wrong.

What Tier 1 *does* own: the pure-core logic — `dispatch_budget` classification, `Deadline`
clamping, URI canonicalization + the four accessors, `validate_for_dispatch`, pre-append
bounds, `BodyCell` state transitions, `StoredError` reconstruction, 502/504 mapping,
extractor caps, manifest/host-grammar parsing, capability-gate outcomes, and the
`CapabilitySupport` matrix.

### 5.2 Tier 2 — per-adapter contract tests (no network)

**Location:** `tests/contract.rs` in each adapter crate — **native** for Axum, and on the
adapter's **wasm target** for Fastly / Cloudflare / Spin (§5.5). Covers request→platform
and platform→response conversion, header preservation (incl. multi-value), method/body
preflight, non-2xx mapping, buffered vs. streamed handling, decompression, and error
mapping. Requires the **`test-utils` seams and provider fakes** of §5.5 — a Tier 2 row
whose seam is a **first-class deliverable of the same change** (below) is **required** —
the row is not waived for a missing seam; the seam is built as part of landing the adapter.
Only rows needing a real platform runtime are deferred, and those are Tier 3, not Tier 2.

**The `send_all` conformance suite lives here.** Because orchestration is per-adapter, the
only way to hold all four to one standard is a **reusable suite** — a set of assertions
exported from `edgezero-core` under `test-utils` and **invoked by every adapter's
`contract.rs` against its real client**: index alignment, empty batch, partial-failure
isolation, `send ≡ send_all(vec![req]).pop()` equivalence, streamed-body/streamed-response
preflight rejection. This is what makes "portable" a tested claim rather than an assertion,
and it is the direct replacement for the Tier-1 `send_all` rows that could never have
proven it.

### 5.3 Tier 3 — per-adapter live behaviour

Proves real fan-out and timing against a locally spawned mock origin.

- **Axum** — implemented now. A `tokio` mock server with configurable per-route delay,
  body size, compression, and chunk pacing.
- **Fastly** — a Viceroy-run test with a backend pointed at the local mock origin.
- **Cloudflare** — a `workerd`/miniflare integration test against the local mock origin.
- **Spin** — a `spin`-runtime test against the local mock origin; the only place Spin's
  `join_all` concurrency runs under the real wasi executor (bare `block_on` will not fan
  out).

**Which Tier 3 jobs block completion — explicit, because earlier drafts had Tier 3
simultaneously "required" and "deferred".**

| Tier 3 job | Blocks completion? | Rationale |
| --- | --- | --- |
| **Axum** (tokio mock origin) | **YES — blocking** | Native, no external runtime to install. It is the reference adapter and the *only* place real fan-out wall-clock is proven in this change. |
| **Fastly** (Viceroy) | **Partial** | Per the locked decision, Fastly's *core* coverage is **deterministic unit + contract tests** (dispatch/harvest ordering, backend identity, phase-split, slack guard). **Viceroy IS already in CI** (`viceroy 0.17.0` in `.tool-versions`, installed/run by `test.yml`) — so "deferred until Viceroy support exists" is stale and corrected here; what remains deferred is specifically the **live wall-clock concurrency** Tier 3 timing suite (real parallel fan-out timing), not Viceroy availability. |
| **Cloudflare** (`workerd`/miniflare) | **No — deferred** | Lands with the runtime job. |
| **Spin** (`spin` runtime) | **No — deferred** | Lands with the runtime job. **Note:** this is the *only* place Spin's `join_all` concurrency actually fans out — a bare `block_on` will not. So Spin's concurrency claim is **unproven until this job exists**, and that must be stated rather than implied by a green Tier 1/2. |

A deferred job means the adapter's **logic** (Tier 1) and **translation + conformance
suite** (Tier 2) still run and still gate the merge; what is missing is the **live
wall-clock/timing proof** only. That gap is tracked here, not silently skipped — and no
row may claim a wall-clock guarantee whose only proof lives in a deferred job.

**Applying that rule to cancellation — one narrow BLOCKING exception.** The
`Native` claim for Cloudflare and Spin cancellation is precisely a guarantee whose only
proof would otherwise live in a deferred job: Tier 1/2 fakes prove the *Rust-side* drop
and `abort()` call, but **not that the host actually tears the subrequest down**. Per the
rule above, that claim may not rest on a deferred job. So a **focused, host-observed
cancellation test is a blocking deliverable** — narrower than the full runtime suite: for
Cloudflare, an origin that observes the aborted subrequest after `controller.abort()`;
for Spin, that `[subtask-cancel]` tears the request down and leaves **no** spawned pump
running. These two tests land with the adapter work. If either is not implemented, the
corresponding capability (`outbound-deadlines` / `streamed-upload-deadlines`) must be
declared **`BestEffort`, not `Native`**, until it is — the claim follows the proof.

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
| Empty `send_all(vec![])` → empty vec — Tier 1 asserts the core contract shape; **Tier 2 re-runs it per adapter** as part of the mandatory send_all conformance suite (each adapter implements `send_all` independently, so Tier 1 cannot prove the shipped orchestration) | yes | yes | — |
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
| Required `BestEffort` capability → the **gated** command classes (`edgezero build`, `serve`, `deploy`, `provision`, `config push`, `config validate`, `demo`) each exit non-zero with a clear message — matches the §3.5.3 enforcement set: gate inside `execute(..)` for `build`/`serve`/`deploy` (branching to skip `auth`), plus four siblings on `run_provision`, `run_config_push_typed`, `run_config_validate`, and `run_demo` (**five** gate sites). `edgezero dev` is gone; `demo` is its contributor-only replacement | yes | — | — |
| **Exempt command classes do NOT hard-fail** on a required-`BestEffort` mismatch (§3.5.3 command-class gating): the *same* manifest that fails the gated commands above leaves `edgezero config diff` (read-only diagnostic) and `edgezero auth login` / `logout` / `status` (credential) exiting **normally**. Regression guard against re-adding a blanket "every adapter-selecting command" gate — blocking a read-only diff or credential cleanup on an unrelated runtime mismatch is the bug this class split fixes | yes | — | — |
| Axum response converter mapping for a wrapped streamed body: `Err(GatewayTimeout)` chunk during buffered drain → axum response **504**; `Err(BadGateway)` chunk → **502**; over-cap → **502 AND `body_json["error"]["kind"] == "response_too_large"`** (assert the KIND, not just status — status 502 alone would also match `bad_gateway`); `Ok` chunks under cap append normally. The buffering boundary lets Axum preserve the status **and kind** | — | yes | yes |
| **Fastly** response converter `kind` assertion (§3.4.1 / §5.5): an over-cap chunk during the buffered-passthrough drain (`FASTLY_RESPONSE_STREAM_BUFFER_BYTES`) synthesizes via `err.into_response()` and yields **502 AND `kind == "response_too_large"`** — NOT a stringly `FastlyError` and NOT `bad_gateway`. Explicit per-adapter row so a Fastly converter that degraded `response_too_large` into `bad_gateway` (or dropped the JSON envelope) fails. Uses the `test-utils` dispatch seam (§5.5) | — | yes | yes |
| **Spin** response converter `kind` assertion (§3.4.1 / §5.5): an over-cap chunk during the buffered-passthrough drain (`SPIN_RESPONSE_STREAM_BUFFER_BYTES`) synthesizes via `err.into_response()` and yields **502 AND `kind == "response_too_large"`** — NOT threaded through `anyhow` and NOT `bad_gateway`. Explicit per-adapter row so a Spin converter that degraded the kind fails. Uses the Spin transport seam (§5.5) | — | yes | yes |
| **Content-encoding decode matrix — table-driven, Buffered AND Streamed, per adapter** (§3.4.1). The decode contract distinguishes many cases; a single "compressed cap" test does not cover it. One table row per case, asserting decoded body bytes AND header stripping: (a) **absent** `content-encoding` → passthrough, headers unchanged; (b) **`identity`** → passthrough, unchanged; (c) **`gzip`** / **`br`** → decoded, `content-encoding` + `content-length` **stripped**; (d) **case-insensitive** `GZIP`/`Br` → decoded (same as c); (e) **surrounding whitespace** ` gzip ` → decoded; (f) **parameterized** `gzip;q=0.5` / `gzip;x=1` → **passthrough** (not the bare token, §3.4.1), headers **preserved**; (g) **unknown** `zstd`/`deflate`/`compress` → passthrough, preserved; (h) **comma-stacked** `gzip, br` → passthrough, preserved; (i) **repeated `content-encoding` field lines** → treated as stacked → passthrough; (j) **malformed compressed data** under a decoded token → `bad_gateway` (a decode-side IO failure, §3.4.1), NOT `response_too_large`. Each of (a)–(j) runs in **both `Buffered` and `Streamed`** mode and on **each adapter's converter** (the decoder is the shared core helper, but header stripping happens at the adapter boundary). Regression guard: the previous suite would pass with a decoder that only handled bare lowercase `gzip`. | yes | yes | — |
| `OutboundRequest::into_parts` / `OutboundResponse::new` / `OutboundResponse::into_parts` round-trip every field (adapter API completeness) | yes | yes | — |
| `body_bytes` cap exceeded → subsequent `body_bytes` / `json_within` / `form_within` calls return the same stored error (poison semantics); `into_request()` returns `Err(stored_err)` (per §3.4.5 round-18 / round-19 — **not** an empty body) | yes | yes | — |
| `into_request()` after middleware buffered body yields `Body::Once(cached)` (proxy-forward still works) | yes | yes | yes |
| Multi-value `set-cookie` round-trips through every adapter's response path (`get_header_all` on Fastly; not `get`) | — | yes | yes |
| Multi-value outbound request header round-trips through every adapter's request path (`append_header` on Fastly; `Headers::append` on CF; WASI `fields` on Spin) | — | yes | yes |
| `DEFAULT_NO_DEADLINE_BUDGET` core constant (Tier 1): `dispatch_budget(no-deadline-no-timeout-request, now)` returns `DispatchBudget { duration: 30 s, deadline: now + 30 s, cause: Default }` per §3.3.2 table. Pure core-logic assertion on the helper, no adapter | yes | — | — |
| Axum no-deadline request uses the 30 s default budget — **Tier 2 is FAKE-CLOCK, no network** (Tier 2 is a no-network tier). **Requires a seam (specified, not assumed):** the Axum wrapper is armed from a `Deadline`, so the test drives it by constructing the wrapper with a `Deadline::at_instant(now + 30s)` and feeding a **`tokio::time` paused clock** (`#[tokio::test(start_paused = true)]` + `tokio::time::advance(31s)`), asserting the wrapper yields the `gateway_timeout` chunk — no wall-clock wait, no server. This needs **`tokio = { features = ["test-util", "macros"] }` in `crates/edgezero-adapter-axum`'s `[dev-dependencies]`** (add it — the crate does not have `test-util` today) and a `test-utils`-gated constructor that lets the test build the wrapper with an explicit `Deadline`. Gated by `cargo test -p edgezero-adapter-axum`. No literal `sleep(30s)` | — | yes | — |
| Axum no-deadline 30 s **wall-clock** end-to-end — **Tier 3 ONLY (blocking runtime job)**: a real Axum dev server + mock origin. Even here the test uses a **short injected budget** (or a runtime clock override), NOT a literal 30 s wait, to prove the *default is applied* end-to-end; a routine 30 s test is prohibited. Adapter-specific wall-clock behaviour | — | — | yes |
| `OutboundResponse::json_bounded(max)` / `json_bounded_until(max, deadline)` on a streamed body — **helper-cooperative half (Tier 1):** the helpers delegate to `into_bytes_bounded` / `into_bytes_bounded_until` then `serde_json::from_slice`; mock-driven test asserts the helper's cap + cooperative `until_deadline` check + malformed-JSON → 502 mapping. No wrapper insertion | yes | — | — |
| `OutboundResponse::json_bounded_until(max, deadline)` adapter-wrapper half (Tier 2 / Tier 3): the wrapper installed at response construction enforces `dispatch_budget(req).deadline` in real time on Axum / CF / Spin; the caller-supplied `deadline` argument is cooperative only (§3.1.4). Asserts wrapper insertion preserves the JSON outcome | — | yes | yes |
| Streamed body honours `dispatch_budget(req).deadline` end-to-end on Axum/CF/Spin via wrapped stream (including the no-`req.deadline` synthetic-30 s case); bounded-cooperative on Fastly. **Adapter-specific** — the wrapper is installed per-adapter at response-conversion time; Tier 1's mock has no wrapper layer. The cross-adapter contract (`EdgeError::gateway_timeout` chunk past the deadline) is the same row as the cooperative `into_bytes_bounded_until` Tier 1 assertion | — | yes | yes |
| `BodyState::Draining`: drain future dropped mid-flight → cell becomes `Poisoned(cancelled)`; next `body_bytes` returns the stored cancelled error | yes | yes | — |
| Reentrant `body_bytes` while `Draining` returns `Err(EdgeError::internal(..))` without panic | yes | — | — |
| Pre-append cap accounting: a single oversized chunk on a small cap errors **without extending the collected buffer past `max`** (the in-flight chunk briefly co-exists with the buffer during the overflow check, per §3.4.1 / §3.4.4 — the test asserts the *persistent* buffer never grows past `max`, not that the in-flight `current_chunk` is never received). Inbound and outbound bounded drains both covered | yes | yes | — |
| `Form` / `ValidatedForm` migrated to `form_within(DEFAULT_INBOUND_FORM_BYTES = 1 MiB)`; over-cap → 400 | yes | yes | — |
| `Json` / `ValidatedJson` migrated to `json_within(DEFAULT_INBOUND_JSON_BYTES = 8 MiB)`; over-cap → 400; cache + poison behaviour identical to `body_bytes` (§3.4.5 / §7 `src/extractor.rs`) | yes | yes | — |
| Explicit-cap inbound extractors `ValidatedJsonWithin<T, MAX>` / `ValidatedFormWithin<T, MAX>` enforce the const-generic `MAX` (not the default): a body over `MAX` → 400, a body under `MAX` but over the default parses `Ok`. Asserts the `MAX` override path added in §7 `src/extractor.rs` | yes | — | — |
| Per-adapter `capability()` support matrix (§3.5.2): for each of the four registered adapters, `adapter.capability(c)` returns the documented `CapabilitySupport` value (`Native` / `BoundedCooperative` / `BestEffort` / `Unsupported`) for **every** one of the nine capabilities (asserts the §3.5.2 matrix directly, not just gate outcomes — the Axum/Fastly `BestEffort` cells included, e.g. Fastly `outbound-deadlines` = `BestEffort`) | yes | — | — |
| Back-compat manifest parse (§6): a manifest with **no** `[capabilities]` section parses `Ok` with `Manifest::capabilities` defaulted (`#[serde(default)]`), and every adapter-selecting command proceeds (no capability contract to enforce) | yes | — | — |
| Adapter `dispatch_budget(req)` everywhere: each adapter calls the core `dispatch_budget(req, now)` helper and threads the resulting `DispatchBudget` to its platform timer. The **core helper** is Tier 1 (covered by the row above); the "every adapter actually calls it" assertion is Tier 2 (contract crate inspects the call site) / Tier 3 (real runtime observes the 30 s cap) | — | yes | yes |
| `.timeout(short).deadline(long)` honours the *shorter* effective — **dispatch_budget classification (Tier 1):** the core helper returns `DispatchBudget { duration: short, deadline: now + short, cause: PerCallTimeout }`; the mirror `.timeout(long).deadline(short)` returns `cause: BatchDeadline`. Mock-driven test asserts BOTH the effective budget AND the `cause` attribution (§3.3.2 timeout-attribution contract) | yes | — | — |
| **Exact-tie attribution (Tier 1):** `.timeout(t).deadline(d)` constructed (via injected `now`) so `now + t == clamped(d).instant()` **exactly** — the code's `min_by_key` iterates `from_timeout` first, so a tie resolves to **`cause: PerCallTimeout`** (the more specific bound), matching the `≤` row of the §3.3.2 table. Pins the tie so the table and the code cannot drift (an earlier draft's `≥` row disagreed with the code) | yes | — | — |
| **Attribution through ACTUAL adapter results — per adapter, not just `dispatch_budget`** (§3.3.2 is normative for every adapter). For **each of Axum / Cloudflare / Spin** (Fastly covered by its send-stage rows), assert the harvested `EdgeError::GatewayTimeout.cause` on a *real timed-out result*: (a) `.timeout(short).deadline(long)` expiry → `cause == PerCallTimeout`; (b) `.timeout(long).deadline(short)` → `BatchDeadline`; (c) no deadline/timeout, default 30 s fires → `Default`. Covers all THREE paths where the cause flows: **single `send`**, **buffered `send_all`** (per-slot harvested error), and the **streamed-body error chunk** (the wrapper's `gateway_timeout` chunk carries the cause). Uses each adapter's fake-clock/transport seam (§5.5), NOT wall-clock. Regression guard: a `dispatch_budget`-only test passes even if an adapter mapper emitted bare `gateway_timeout` (cause lost); these rows fail in that case. | — | yes | yes |
| `.timeout(short).deadline(long)` honours the *shorter* effective deadline end-to-end (streamed body returns 504 at `now + short`, not `now + long`) — **adapter wrapper (Tier 2 / Tier 3):** wrapper armed with `budget.duration` actually fires at `now + short` against a real platform timer | — | yes | yes |
| Streamed request body over `max_request_body_bytes` → per-slot `bad_request` (400) on every adapter | yes | yes | — |
| Stalled streamed-request-body upload, mechanics differ per adapter — this row is **Tier 2/3 only** because Tier 1's `MockOutboundClient` cannot prove the Axum tokio / Cloudflare `worker::Delay` / Spin WASI-readiness / Fastly host-timer behaviour; Tier 1 covers the cross-adapter *contract* (504 on stall, index alignment) via the mock, marked separately. **Axum / Cloudflare** drain `Body::Stream` into `Bytes` **before** constructing the platform request (§4.1 / §4.2), so the relevant stall is the *source-pull* during the drain — tokio / `worker::Delay` races it against `budget.deadline` and returns 504 at the deadline (no separate "host-write" race because by the time the SDK request is constructed the body is already in hand). **Spin** uses the hand-built `wasi:http` request per §4.4 — the SDK's high-level `spin_sdk::http::send` is **not** used for uploads (buffered or streamed) because its `IntoRequest` impl spawns an **uncancellable** body pump (`wit_bindgen::spawn`), which would leave a stalled source pumping forever and pin the component task alive. Instead the body pump (`wit_stream` writer + `write_all`) lives **inside** a single raced future together with `wasip3::http::client::send`; racing that future against `spin_sdk::time::sleep(remaining)` and dropping it on expiry cancels **both** halves — `[subtask-cancel]` tears the request down host-side and the in-flight `write_all` fires `stream.cancel-write`. A stalled source-pull *and* a stalled host-write are therefore both preempted within one monotonic-clock tick of `budget.deadline` → 504. (The WASI-0.2 `subscribe()` / `check_write()` readiness-poll model earlier drafts described **does not exist** in SDK 6 / WASI 0.3.) The test asserts the drop actually cancels — i.e. a stalled upload returns 504 **and** leaves no spawned pump running. **Fastly** has a single phase where source-pull cannot be preempted (BestEffort per `streamed-upload-deadlines`); the cooperative `budget.deadline.is_expired()` check **between** chunks is the only adapter-side bound, and Fastly's `between_bytes_timeout` is documented as receive-side only — it does **not** bound guest-to-origin writes (BestEffort for the write phase too, no per-chunk-gap claim). The slot returns 504 at the next inter-chunk check after `budget.deadline` expires. Test asserts per-adapter mechanics | — | yes | yes |
| Stalled streamed-request-body upload **contract only** (Tier 1, via `MockOutboundClient` with scripted stalls): on the **preemptible-source** adapters (Axum / Cloudflare / Spin) a stalled upload returns `Err(EdgeError::gateway_timeout(..))` to the caller within the configured deadline, slot index alignment is preserved, and other slots are unaffected. **Fastly is excluded from the "within the configured deadline" half of this contract** because `streamed-upload-deadlines` is `BestEffort` on Fastly (§3.5.1 / §3.5.2): a source-pull stall (`stream.next().await` that never yields) is unbounded on Fastly per §4.3, so Tier 1 cannot assert wall-clock containment there. Fastly still observes the index-alignment + partial-failure-isolation half of the contract. The `MockOutboundClient` sets the adapter under test on the mock so this row's Fastly invocation skips the wall-clock assertion and runs only the structural assertions. Mechanics-level wall-clock assertions for all four adapters (including Fastly's cooperative between-chunk `is_expired()` check — a **best-effort mechanism**, NOT a `BoundedCooperative` capability: Fastly `streamed-upload-deadlines` and `outbound-deadlines` are both `BestEffort`, and cold `finish()` registration is unbounded) live in the Tier 2/3 row above | yes | — | — |
| `body_bytes` / `json_within` / `form_within` after `take_body()` → `internal("body already consumed via take_body")` (no body resurrection) | yes | — | — |
| Valid non-ASCII UTF-8 header (e.g. `x-app-display-name: café`) round-trips through every adapter on request and response. **Asserted on Axum / Fastly / Spin (raw-byte adapters); best-effort on Cloudflare** — `worker::Headers` exposes post-WebIDL strings, so byte-faithful round-trip is not guaranteed there (§3.1.4 *Cloudflare degradation*) | yes | yes | yes |
| **Non-portable method → preflight `bad_request` (400) on every adapter** (§3.1.4): an `OutboundRequest` with a method outside `{GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS}` is rejected in core preflight with `"method <M> is not portable; …"`. Identical for single `send` and for a `send_all` slot (index alignment preserved). **Regression guard:** Cloudflare must **not** silently coerce the method to `GET` — the test asserts a `DELETE`-shaped custom method never reaches the wire as `GET` | yes | yes | — |
| **`GET`/`HEAD` with a non-empty body → preflight `bad_request` (400) on every adapter** (§3.1.4): CF's `fetch` forbids it, so EdgeZero normalises to the strictest platform rather than letting the request succeed on three adapters and fail on one. Asserts uniform rejection, not per-adapter divergence | yes | yes | — |
| **Supported-method matrix:** each of `GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS` reaches the upstream with the method **intact** on every adapter (no coercion, no rewrite) | — | yes | yes |
| **`validate_for_dispatch` runs at DISPATCH, not construction** (§3.1.4): `OutboundRequest::get(url)?.body(payload)` — a `GET` validated at construction that then acquires a body via the infallible `.body()` setter — is still rejected with `bad_request` when it reaches `send` / `send_all`. **This is the regression guard**: a construction-only check passes this test vacuously, so the test must assert the rejection happens with the body attached *after* a successful `get(..)` | yes | yes | — |
| **`GET`/`HEAD` + `Body::Stream` → `bad_request` unconditionally** (§3.1.4), even for a stream that would yield zero bytes — emptiness is not observable without consuming the stream, and the validator does not peek-and-rechain. Asserts the documented false-positive is deliberate | yes | yes | — |
| **`StoredError` reconstruction** (§3.4.5): after a poisoning drain, **every** access (`body_bytes` / `json_within` / `form_within` / `into_request`) returns an `EdgeError` with the **same variant, status, and message**. Asserts poison is reproducible even though `EdgeError` is not `Clone` (its `Internal` variant wraps non-clonable `anyhow::Error`). Also asserts the documented loss: a reconstructed `internal` error's `inner()` carries the message but **not** the original `anyhow` source chain | yes | — | — |
| **`demo` capability gate reads the baked manifest** (§3.5.3): a test-only `Hooks` impl overriding `manifest_json()` to return a crafted manifest that `required`s a capability Axum only `BestEffort`-supports causes `demo_capability_gate::<TestApp>()` to return `Err` (the pure seam — no server started; `run_demo()` calls it before `run_app`). `TestApp` overrides `manifest()` (not just `manifest_json()`, whose value the default `manifest()` ignores). Asserts the gate works with **no manifest file on disk** — the whole point of the baked accessor. A `Hooks` impl with the default `manifest_json() == None` → no capability contract → `demo` runs | yes | — | — |
| **Baked manifest FAILS CLOSED on invalid contract** (§3.5.3): a crafted `manifest_json()` returning JSON that **parses but fails `validate()`** — e.g. an invalid outbound host `{"capabilities":{"outbound":{"hosts":["ftp://x"]}}}` (rejected by `validate_outbound_hosts`), or a capability listed in **both** `required` and `optional` — makes `from_baked_json` return `BakedManifest::Malformed`, so `ensure_capabilities` **hard-fails**. **NOT `{}`:** an empty manifest is explicitly *valid* (every field defaults; `empty_manifest_has_defaults` in `manifest.rs`), so `{}` yields `Present` with empty capabilities, not `Malformed`. Regression guard: parse-only (skipping `validate()`) would make the invalid fixture `Present` and silently disable enforcement | yes | — | — |
| **Manifest strictness — file-backed TOML fail-closed rows** (§3.5.1): fixtures each fail, none silently drops: (a) an **unknown key inside `[capabilities]`** — `require = ["x"]` (typo of `required`) → `deny_unknown_fields` parse error; (b) a **misspelled top-level SECTION** — `[capabilites]` (transposed) → top-level `deny_unknown_fields` parse error, NOT a parse to empty capabilities; (c) `[capability]` (singular) → same top-level rejection; (d) a capability **duplicated within `required`**, and one in **both** `required` and `optional` → `validate_capabilities_disjoint` error | yes | — | — |
| **Misplaced-`capabilities` fail-closed rows (ANY depth)** (§3.5.1) — the `reject_misplaced_capabilities` scan rejects a `capabilities` table nested under any table but the top level. Fixtures at **depth 1**: `[app.capabilities]`, `[triggers.capabilities]`, `[environment.capabilities]`, `[logging.capabilities]`; and at **depth 2**: `[triggers.http.capabilities]`, `[environment.variables.capabilities]`, **`[adapters.axum.build.capabilities]`** (the depth-2 cases that per-struct `deny_unknown_fields` would MISS — and `adapters.*` too, since the scan does not depend on `ManifestAdapter`'s `flatten`). Each MUST **fail to parse**, NOT silently drop the block and run with an empty contract. Regression guard: without the recursive scan, each would parse to an empty capability contract and fail OPEN. Also assert a **valid** top-level `[capabilities]` still parses (the scan doesn't reject the legitimate one). | yes | — | — |
| **`ensure_capabilities` gate branches — direct unit tests** (§3.5.3), one per rung of the ladder against a stub adapter whose `capability()` returns scripted values: (a) **required + `Unsupported`** → `Err` naming the capability; (b) **required + `BestEffort`** → `Err`; (c) **required + `Native`/`BoundedCooperative`** → `Ok`; (d) **optional + `Unsupported`** → `Ok` **and** a `log::warn!` "unavailable" is emitted (asserted via a captured logger); (e) **optional + `BestEffort`** → `Ok` **and** a `warn!` "best-effort" (the round-N regression where only Unsupported warned); (f) **optional + `Native`/`BoundedCooperative`** → `Ok`, **no** warning. Warning assertions use a test log sink so "logs both, not just Unsupported" is actually verified | yes | — | — |
| **Missing-from-registry policy — all three branches** (§3.5.3): with `registry::get_adapter` returning `None`, (a) **no capabilities declared** → `Ok` + `warn!` "check skipped"; (b) **a `required` entry** → `Err` "cannot verify REQUIRED capabilities"; (c) **only `optional` entries** → `Ok` + `warn!` "cannot verify its OPTIONAL capabilities — proceeding" (the optional-only fix — must NOT hard-fail). Each branch asserts the exact outcome + the emitted log line | yes | — | — |
| **Spin `allowed_outbound_hosts` — `provision` writes** (§3.5.4): `edgezero provision --adapter spin` renders `[capabilities.outbound].hosts` into `spin.toml`, **preserving sibling fields and comments** (`toml_edit`), and is a no-op under `--dry-run`. Absent `[capabilities.outbound].hosts` → writes `["https://*:*"]` and **never** widens to include `http://*:*` (security-default regression guard) | yes | — | — |
| **Spin `allowed_outbound_hosts` — `build`/`serve`/`deploy` validate, never write** (§3.5.4): drift between `spin.toml` and the manifest hard-fails with the expected list rendered; `spin.toml` is **byte-identical** afterwards (asserts the build path does not rewrite a git-tracked, user-owned file). Comparison is over **canonicalized sets**: `https://x:443` vs `https://x`, and a reordered list, must **not** report drift | yes | — | — |
| **Spin sync hook fires for shell-overridden commands** (§3.5.4) — **dead-code regression guard.** A scaffolded manifest declares `[adapters.spin.commands].build`, so `edgezero build --adapter spin` takes the `manifest_command` → `run_shell` branch and **never reaches `SpinCliAdapter::execute`**. The test asserts the drift check **still fires** — proving the hook lives in `edgezero_cli::adapter::execute` *before* the `manifest_command` branch, not in the adapter. A hook placed in the adapter passes every other Spin test and silently fails only this one | yes | — | — |
| **Spin early final response is NOT discarded** (§4.4): an origin that returns 413/401/redirect **before** consuming the full streamed request body — so it stops reading and the guest write sees reader-gone — yields that response (non-2xx `Ok`), NOT a 502. Regression guard: the pump's reader-gone must end the pump cleanly (Ok), and the ordered `select` must let a completed `send` win; a `join!` + pump-precedence design fails this by mapping reader-gone to `bad_gateway`. **Tier 2, not Tier 1** — this asserts the shipped Spin `select`/pump orchestration, which Tier 1's `MockOutboundClient` cannot prove; it runs against the Spin transport seam (§5.5) | — | yes | yes |
| **Spin stalled source does not delay an available response** (§4.4): if `send` produces a response while `source.next()` is still stalled, the response returns immediately — it is NOT held until the deadline (which would wrongly produce 504). Asserts the ordered race returns on `send`, not on both futures. **Tier 2, not Tier 1** — shipped Spin orchestration, proven against the Spin transport + timer seam (§5.5), not the Tier 1 mock | — | yes | yes |
| **Spin pump SOURCE error / cap-overflow still surfaces when there is no response** (§4.4): a `source.next()` that yields `Err`, or a body exceeding `max_request_body_bytes`, before any response → `bad_gateway` / `bad_request` (400). Asserts the ordered race propagates a pre-response pump error, so early-response tolerance does not swallow genuine client-side failures. **Tier 2, not Tier 1** — shipped Spin orchestration, proven against the Spin transport seam (§5.5), not the Tier 1 mock | — | yes | yes |
| **Cloudflare `set-cookie` multi-value, upstream → core** (§4.2): two upstream `set-cookie` headers survive as **two** values in the core `HeaderMap`. Regression guard for `HeaderMap::insert` (which removes all previous values) — the test must assert **both** cookies are present, not just that a `set-cookie` exists. Valid on this repo's pinned `compatibility_date = "2023-05-01"`, which enables workerd's per-`set-cookie` `entries()` behaviour | — | yes | yes |
| **Cloudflare `Set-Cookie` multi-value, core → client** (§4.2): a handler emitting **two** `Set-Cookie` headers ships **both** to the client. Regression guard for `Headers::set` (which replaces) on the client-facing response path — distinct from the row above, which is the upstream-response path. Both must use `append` | — | yes | yes |
| **Cloudflare non-ASCII request header does not panic** (§4.2): proxying `x-app-display-name: café` through the CF outbound path completes without unwinding. Regression guard for `Headers::from(&HeaderMap)`'s `value.to_str().unwrap()` — `HeaderValue::to_str` errors on any byte outside visible ASCII, so this **panics the worker** today | — | yes | yes |
| **Cloudflare repeated non-`set-cookie` headers are comma-joined (documented loss)** (§3.1.4): two upstream `x-foo` headers arrive as a single `x-foo: a, b`. The test **asserts the documented degradation** rather than a faithful round-trip — workerd comma-joins them and `getAll()` refuses any name but `set-cookie`, so separate field lines are unrecoverable. Axum / Fastly / Spin preserve them separately | — | yes | yes |
| Header containing a `\x80` byte is rejected on outbound request (400) and dropped on inbound-of-outbound response with a `warn!` naming the header | yes | yes | — |
| RFC 7230 hop-by-hop strip removes `trailer` (singular) end-to-end; an inbound `trailer: foo` never reaches the outbound wire | yes | yes | — |
| Fastly `send` with `Body::Stream` request body: over `max_request_body_bytes` mid-upload → 400; stalled upload **between** yielded chunks (next cooperative `budget.deadline.is_expired()` check fires) → 504 within one chunk-iteration of `budget.deadline`; stalled `stream.next()` AND stalled in-progress `StreamingBody::write_all` are **both BestEffort gaps** on Fastly (no preemption, and `between_bytes_timeout` is documented as *receive-side only* — it does not bound guest-to-origin writes); upload time reduces remaining budget for response. **Adapter-specific mechanics (cooperative inter-chunk check, source-pull and host-write non-preemption) live in Tier 2 / Tier 3 only** — Tier 1's `MockOutboundClient` cannot reproduce Fastly's chunk-iteration timing | — | yes | yes |
| `dispatch_budget(req)` table: every row of §3.3.2 holds (timeout-only, deadline-only, both, expired, zero-effective, no-deadline-no-timeout) | yes | — | — |
| Fastly `send_all` with mixed budgets, **headers phase**: short-budget slot's *headers* result reflects its own budget (host enforces independently); but its wall-clock-observed *delivery* can be delayed behind an earlier `wait()` (harvest order). **Adapter-specific** — harvest order and per-slot host-timer behaviour belong to Tier 2 (Fastly contract crate) and Tier 3 (Viceroy) | — | yes | yes |
| Fastly `send_all` Buffered mode, **body phase**: a slot whose own `budget.deadline` would have covered its body in isolation can still return `gateway_timeout` because an earlier slot's body drain monopolised harvest. The contract explicitly admits these harvest-order-induced 504s on Fastly Buffered. **Adapter-specific harvest mechanics** — Tier 1's mock has no harvest queue and cannot reproduce the head-of-line block; covered by Tier 2 (deterministic harvest ordering against a host-side fake) and Tier 3 (Viceroy wall-clock) | — | yes | yes |
| `[capabilities] required = ["send-all-slot-isolation"]` on a Fastly target → **every adapter-selecting CLI command** (`build` / `serve` / `deploy` / `provision` / `config push` / `config validate` / `demo` — the gated classes) exits non-zero with the BestEffort + required hard-fail message via the §3.5.3 pre-dispatch gates (one inside `execute(..)` branching to skip `auth`, four siblings on `run_provision` / `run_config_push_typed` / `run_config_validate` / `run_demo`); `config diff` and `auth *` are exempt; same manifest on Axum/CF/Spin passes | yes | — | — |
| Fastly mixed-budget `send_all` to the **same host**: slots with `50 ms` and `3 s` budgets create **distinct** dynamic backends (identity tuple includes `budget_ms`); the 50 ms slot's host timeout is not silently inherited by the 3 s slot or vice versa. **Asserts the Fastly identity tuple** — Tier 1's mock has no dynamic-backend abstraction; Tier 2 (Fastly contract crate) inspects the registered-backend map and Tier 3 (Viceroy) observes the wall-clock divergence | — | yes | yes |
| **Fastly per-session cache is bounded by fan-out size** (§4.3): a `send_all` of N slots to the same host with the same shared `batch_now` budget registers **exactly one** backend (identical `budget_ms`); N slots with N distinct budgets register N backends. Across requests the cache is fresh (session-scoped names), so there is no cross-request accumulation to bound. Regression guard against a cross-request thread-local cache | — | yes | yes |
| `RequestContext::into_request()` after `body_bytes` poison: returns `Err(stored_err)`, not `Ok(Request<Body::empty()>)` — a permissive proxy-forward cannot mask a stricter middleware's poisoned read | yes | — | — |
| Fastly + `outbound-http = required`: `ensure_capabilities` emits the dynamic-backends informational log | yes | — | — |
| **Fastly stage 1 — `BackendCreationError` (registration), per the exhaustive §4.3 table.** `Disallowed` and `HostError` → **`bad_gateway` (502)** (genuine host rejection; `Disallowed` carries the "enable dynamic backends" diagnostic). **`ConnectTimeoutTooLarge` / `FirstByteTimeoutTooLarge` / `BetweenBytesTimeoutTooLarge` / `NameTooLong` / `EncodingError` → `internal` (500)** — these mean **EdgeZero** broke its own clamp/naming invariant, so mapping them to 502 would disguise an adapter bug as an upstream failure (regression guard). `BackendCreationError` is constructible + `PartialEq` + not `#[non_exhaustive]`, so each branch is unit-testable directly and the match is exhaustive. A fake builder cannot produce DNS/TLS/connect branches — those are stage-2 | yes | yes | — |
| **Fastly stage 2 — send-failure policy, via the constructible `SendFailure` enum (§4.3).** Table-drive **one case per `SendFailure`**: `Timeout` → `gateway_timeout` (504) **carrying the passed `cause`** (assert `classify(SendFailure::Timeout, BudgetSource::PerCallTimeout)` yields both 504 AND `cause == PerCallTimeout`); `Transport` and `UpstreamProtocol` → `bad_gateway` (502); **`LocalInvariant` → `internal` (500)** (an EdgeZero bug — mapping it to 502 would disguise it as an upstream failure); `Unknown` → `bad_gateway` (502). **Tier 2, NOT Tier 1** — `classify` is `pub(crate)` **in the Fastly ADAPTER crate**, and Tier 1 is core-crate-only (the `MockOutboundClient` layer, no provider crates). It is a pure fn, so its test is an in-crate `#[cfg(test)]` unit test in `crates/edgezero-adapter-fastly` (no runtime, no fake), gated by **`cargo test -p edgezero-adapter-fastly classify`** (which the existing `cargo test --workspace --all-targets` gate already runs). It takes no SDK types, so no runtime is needed — but it is Tier 2 because of where it lives. Deliberately NOT asserted against `SendErrorCause`/`SendError`: both are unconstructible outside the SDK (`#[non_exhaustive]` / private fields). The `cause_to_failure` boundary map is Tier 3 only — a documented gap forced by `#[non_exhaustive]` | — | yes | yes |
| **`DnsTimeout` is a TIMEOUT cause → 504, not 502** (§4.3 send-stage table). Explicit row because it is the one ambiguous cause: it names DNS (transport, 502-ish) but *is* a fired timer, and the fan-out caller retries timeouts differently from unreachable upstreams. Pinned so the mapping cannot drift | — | yes | yes |
| Fastly `EdgeError::internal` (500) covers **faults not attributable to the upstream service** — two kinds, made explicit (broadening the earlier "EdgeZero-invariant violations ONLY" wording, which conflicted with mapping the SDK's `InternalError` here): **(i) EdgeZero-invariant violations** (our bug) and **(ii) SDK/host-internal faults** (the platform itself failing, not the origin). It is **never** used for a genuine upstream/transport failure (DNS/TLS/connect/timeout → 502/504). The test inspects the error chain and asserts `internal` appears **exactly** for: (a) `BATCH_DISPATCH_SLACK_MAX` overshoot, (b) `NameInUse` external-registration collision, (c) the unfilled-slot harvest invariant, (d) the clamp/name/encoding `BackendCreationError` variants (`ConnectTimeoutTooLarge`, `FirstByteTimeoutTooLarge`, `BetweenBytesTimeoutTooLarge`, `NameTooLong`, `EncodingError` — our clamp/naming broken, kind (i)), (e) the locally-invalid-request `SendErrorCause` variants (`HttpRequestUriInvalid`, `HttpRequestCacheKeyInvalid`, `HttpCacheApiUnsupported`, `HttpCacheLimitExceeded` — a request WE constructed is invalid, kind (i)), and (f) **`InternalError`** (the SDK's own unexpected host-internal fault — kind (ii), NOT an upstream failure and NOT our request being malformed). Every other Fastly path is `bad_gateway`, `gateway_timeout`, or `bad_request`. **Regression guard:** an earlier draft mapped (d)/(e) to `bad_gateway`, disguising an EdgeZero bug as an upstream 502 — the test fails if any of them regresses to 502; `InternalError` stays 500 (a host-internal fault is not the *origin's* 502). | — | yes | yes |
| `Deadline::after(Duration::MAX)` clamps to `DEADLINE_FAR_FUTURE = 7 days` (round 24, down from 365 d to stay under Fastly's u32-ms ceiling); subsequent `dispatch_budget` round-trip still produces a usable budget; no panic | yes | — | — |
| Inbound body `form_within(max)` over-cap → 400; cache + poison behaviour identical to `body_bytes` / `json_within` | yes | yes | — |
| Required `streamed-upload-deadlines` on Fastly → hard build failure (BestEffort + required, per §3.5.3) | yes | — | — |
| Upload consumes the budget — **contract shape (Tier 1, Axum / Cloudflare semantics only):** the cross-adapter contract that `budget.deadline.remaining()` is consulted after the upload drain completes, and that `None` returns `gateway_timeout` *without* dispatching the platform request, is asserted against `MockOutboundClient` configured in **drain-first** mode (the Axum / Cloudflare shape — drain into `Bytes` first, then dispatch). The mock exposes a `did_dispatch()` flag and the assertion is "deadline expired during drain → 504 returned AND `did_dispatch() == false`." **This row covers Axum / Cloudflare only**; Spin and Fastly are explicitly excluded because their adapters dispatch concurrently with (or before) the upload drain and the §3.1.1 contract documents partial upstream sends as possible / expected on those adapters — see the per-adapter Tier 2 / Tier 3 rows below. The mock's drain-first mode is a property of the test harness, not a cross-adapter contract; the Tier 1 row asserts only what the Axum / Cloudflare adapters guarantee | yes | — | — |
| Upload consumes the budget on **Axum** / **Cloudflare** — **adapter mechanics (Tier 2 / Tier 3):** the adapter drains the streamed request body into `Bytes` *before* constructing the platform request, so `budget.deadline.remaining() == None` after the drain → adapter returns `gateway_timeout` **before** constructing/sending the actual `reqwest`/`worker` request. No partial upstream send. Asserted via `crates/edgezero-adapter-{axum,cloudflare}/tests/contract.rs` (Tier 2: inspect the platform-SDK send-call counter on a fake / no-network harness) + Tier 3 against a mock origin (the origin observes zero connections from the timed-out slot) | — | yes | yes |
| Upload consumes the budget on **Spin** — **adapter mechanics (Tier 2 / Tier 3):** the adapter feeds chunks to the WASI outgoing-body; after the upload completes, `budget.deadline.remaining()` is checked. If exhausted, the response future is dropped → `gateway_timeout`. **Partial upstream send is possible** because chunks were flowing — distinct from Axum / Cloudflare. Asserted via the Spin contract crate (Tier 2: WASI outgoing-body chunk-count observation) + Tier 3 against a mock origin under the real Spin runtime (origin observes the partial upload) | — | yes | yes |
| Upload consumes the budget on **Fastly** (`send_async_streaming`): dispatch happens **before** chunks flow, so request bytes have already started reaching the upstream by the time the budget is exhausted. Adapter detects `budget.deadline.remaining() == None`, drops the `StreamingBody` and `PendingRequest` without `wait()`, and returns `gateway_timeout`. **Partial upstream send is expected** — the documented Fastly-specific limitation of streamed uploads. The test asserts this contract honestly. **Adapter-specific** — the `send_async_streaming` + `wait()`-drop sequence is Fastly SDK behaviour Tier 1's mock has no analogue for; covered by Tier 2 (Fastly contract crate) and Tier 3 (Viceroy) | — | yes | yes |
| Fastly streamed-upload **tiny-positive-remainder edge case** — the upload drain completes with `budget.deadline.remaining() == Some(small)` (say 10 ms left out of a 200 ms budget). The cooperative check at the `wait()` boundary passes (remaining is positive), and the host then waits up to the dispatch-time `first_byte_ms` (150 ms in this example, 3/4 of `budget.duration`) for the upstream's response headers. The test asserts (a) total wall-clock from dispatch to return is bounded by `budget.duration + first_byte_ms + between_bytes_timeout` (closed-form, **not** per-chunk-accumulating), (b) the response wrapper's `is_expired()` check preempts after the first body chunk read returns rather than waiting another `between_bytes_timeout` per chunk, (c) the slot ultimately returns `gateway_timeout` (with `cause` per §3.3.2). **The earlier "`partial_send = true` diagnostic in the error chain" assertion is removed** — `GatewayTimeout` carries only `{ message, cause }` and `inner()` is `None`, so there is no error chain to inspect and no `partial_send` field; a partial upstream send is instead observed via the **`test-utils` dispatch/did-send counter** (the same seam §5.5 uses), not via the `EdgeError` shape. Fastly-specific (response-phase overshoot is the documented behaviour of `send_async_streaming`); Tier 2 (contract crate, time-injection hook) + Tier 3 (Viceroy wall-clock observation) | — | yes | yes |
| `batch_deadline = Deadline::after(batch_deadline_ms)` computed once and copied into every target request → all targets share one absolute wall-clock cap (no drift); recomputing `Deadline::after(batch_deadline_ms)` per target would let later targets drift past the batch deadline (counter-example test) | yes | — | yes |
| Outbound request header from `headers_mut()` containing a non-UTF-8 value is **dropped with `warn!`** by `normalize_for_dispatch` (lossy proxy-forward path) — distinct from `header(..)` which **rejects** with 400 (loud construction path) | yes | yes | — |
| Adapter response-out converter (`response.rs`) on **Cloudflare** (the only lazy-`Native` adapter): `OutboundResponse::into_response()` with a streamed body yields first bytes before the upstream stream ends (no buffer-then-return); driven by a `MockOutboundClient`-fed stream in-process, no platform runtime needed. **Axum, Fastly, and Spin are excluded from this row** — all three are `BestEffort` and fall back to bounded buffered passthrough, for three distinct reasons: non-Send `LocalBoxStream` (footnote 3), `Response::stream_to_client()` incompatible with `#[fastly::main]` (footnote 6), and Spin's buffered `FullBody` public response surface (footnote 7). See the buffered-fallback row below | — | yes | yes |
| Adapter response-out converter on **Cloudflare**: stream errors after headers **abort the downstream response stream** — once headers have been written, HTTP cannot change status to 502/504, so the adapter aborts the chunked body (TCP close on HTTP/1.1, RST_STREAM on HTTP/2) and emits a `log::warn!` naming the originating `EdgeError` variant (`gateway_timeout` or `bad_gateway`). Clients observe an early connection close, not a synthetic 502/504. The originating EdgeError is in the server log. **Axum, Fastly, and Spin are excluded** because none of them reaches "headers already written" — each buffers the whole body before the response is returned, so a mid-stream error becomes a clean 502/504 in the buffered drain | — | yes | yes |
| Adapter response-out converter buffered fallback on **Axum, Fastly, and Spin**: streamed body is buffered to `Bytes` within the adapter-level constant (`AXUM_RESPONSE_STREAM_BUFFER_BYTES` / `FASTLY_RESPONSE_STREAM_BUFFER_BYTES` / `SPIN_RESPONSE_STREAM_BUFFER_BYTES` — all default 16 MiB, documented adapter-specific limitations). First bytes only flow after full collection. Over-cap → 502. The per-outbound-request `max_response_bytes` is unavailable by the time the converter runs (`OutboundResponse` carries only status / headers / body); the adapter-level constant is what the converter uses. Apps needing lazy passthrough declare `lazy-streamed-response-passthrough` required and target **Cloudflare** (the only `Native` adapter; Axum + Fastly + Spin are `BestEffort`, footnotes 3 / 6 / 7) | — | yes | yes |
| `Deadline::after(d)` and `dispatch_budget`'s `saturating(d)` clamp at `DEADLINE_FAR_FUTURE` (7 d) — `Duration::MAX` does not panic, never produces an `Instant` past the clamp, and `fastly_timeout_ms` of the clamped value fits within Fastly's `u32` ms ceiling without rejection | yes | yes | — |
| `OutboundRequest::is_stream_body()` returns `true` for `Body::Stream` requests and `false` for `Body::Once`; `send_all` preflight uses this to reject without consuming | yes | — | — |
| `OutboundRequest::is_stream_response()` returns `true` for `stream_response()`-marked requests; `send_all` preflight uses this to reject with `bad_request` without consuming, on every adapter | yes | yes | — |
| `send_all` with `stream_response()` returns per-slot `bad_request` (400) on every adapter; single `send` with the same request succeeds (streamed bodies are only valid via `send`) | yes | yes | — |
| `[capabilities.outbound].hosts` validation (§3.5.4 grammar): **rejected** — empty string, `ftp://x` (bad scheme), `https://` (missing authority), `https://u:p@x` (userinfo), `https://x/p` (path), `https://x?q` (query), `https://x#f` (fragment), `https://x:0`/`https://x:70000` (out-of-range port), `https://x:abc` (non-numeric port), `https://x:` (empty port), `https://[::1` and `https://::1]` (malformed brackets), `https://::1` (unbracketed IPv6), `ex*ample.com`/`*.*.com`/`a.*.com`/`**.com` (invalid wildcard placement), `https:// x`/`x .com`/` x.com`/`x.com ` (whitespace), `x.com.` (trailing dot), `café.com`/`ex€ample.com` (non-ASCII — punycode required), **`-x.com`/`x-.com` (leading/trailing hyphen), `x..com` (empty label), `x_y.com` (underscore), and invalid bracketed IPv6 `https://[::g]`/`https://[:::1]`/`https://[12345::]` (rejected by `Ipv6Addr::from_str`)**. **Accepted** — `"*"`, `"*.example.com"`, `"x:8443"`, `"https://[::1]"`, `"https://[2001:db8::1]"`, `"https://127.0.0.1"`, `"xn--caf-dma.com"`, `["*", "api.example.com"]`. Manifest load surfaces every error before the build | yes | — | — |
| `send_all` shared-`now` snapshot: a homogeneous-budget Fastly fan-out batch to one host creates **exactly one** dynamic backend (per the §4.3 identity guarantee); replacing `batch_now` with per-slot `Instant::now()` in a test fork creates distinct backends, catching the drift bug. **Asserts Fastly-specific identity tuple including `budget_ms`** — Tier 1's `MockOutboundClient` has no dynamic-backend abstraction, so this row is Tier 2 (Fastly contract crate) + Tier 3 (Viceroy) only | — | yes | yes |
| Outbound `Host` header includes the explicit port for non-default-port URIs: `http://localhost:3000` → `Host: localhost:3000`; `https://example.com:8443` → `Host: example.com:8443`; `https://example.com` → `Host: example.com` (no port). Adapters never copy `host` from the inbound `req.headers()` | yes | yes | yes |
| **Core URI canonicalization → four-value split (Tier 1 half).** The four accessors `backend_target()` / `host_authority()` / `sni_hostname()` / `cert_host()` are tested in `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]` against a matrix of inputs, with per-scheme expectations (no adapter dependency). **HTTPS DNS-host inputs** (`https://example.com`, `https://example.com:443`, `https://example.com:8443`): `backend_target() == "example.com:443"` / `"example.com:443"` / `"example.com:8443"`; `host_authority() == "example.com"` / `"example.com"` / `"example.com:8443"`; `sni_hostname() == Some("example.com")` on all three; `cert_host() == Some("example.com")` on all three. **HTTPS IP-literal inputs** (`https://127.0.0.1`, `https://[::1]:8443`): `sni_hostname() == None` (RFC 6066 §3); `cert_host() == Some("127.0.0.1")` / `Some("::1")` (bracket-stripped). **HTTP DNS-host inputs** (`http://example.com`, `http://example.com:80`, `http://example.com:8443`): `backend_target() == "example.com:80"` / `"example.com:80"` / `"example.com:8443"`; `host_authority() == "example.com"` / `"example.com"` / `"example.com:8443"`; `sni_hostname() == None` (no TLS, no SNI); `cert_host() == None` (no TLS, no certificate). The HTTPS-only `cert_host()` `Some` is the canonical reason an adapter calls `.disable_ssl()` vs `.enable_ssl()` / `.check_certificate(..)`. This is the core-side guarantee the Fastly row below assumes | yes | — | — |
| **Fastly adapter consumes the four canonical accessors, DNS-name HTTPS path (Tier 2 / Tier 3 half).** For a DNS-name HTTPS host where `req.sni_hostname()` returns `Some(sni)` and `req.cert_host()` returns `Some(cert)`, Fastly dynamic backend construction calls `Backend::builder(name, req.backend_target()).override_host(req.host_authority()).sni_hostname(sni).check_certificate(cert)` (with `sni == cert` because both accessors return the same host string for the DNS-name case). For HTTP (`req.cert_host()` returns `None`), it calls `Backend::builder(name, req.backend_target()).override_host(req.host_authority()).disable_ssl()`. A Tier 2 test (`crates/edgezero-adapter-fastly/tests/contract.rs`, no network — asserts against the **recording `BackendBuilder`** seam, §5.5: `Backend` is opaque and its getters do **not** round-trip `sni_hostname` / `check_certificate` / `override_host`, so inspecting the registered-backend map **cannot** prove these builder calls; map inspection is reserved for identity/reuse assertions) and a Tier 3 test (Viceroy round-trip) build `https://example.com:8443` and `http://example.com:8443` and assert: connection target = `example.com:8443` on both; Host = `example.com:8443` on both; SSL enabled with SNI = cert = `example.com` on the first, disabled on the second; identity hashes differ (distinct backends). **DNS-name HTTPS only** — IP-literal HTTPS (where `sni_hostname()` is `None` but `cert_host()` is `Some(ip)`) is the dedicated "Fastly HTTPS to IP literals" row below, which asserts the **distinct** behaviour of skipping `.sni_hostname(..)` while still passing `cert_host()` to `.check_certificate(..)`. **Adapter-specific** — Tier 1's mock has no `Backend::builder` analogue | — | yes | yes |
| URI canonicalization — **core accessor half (Tier 1):** `OutboundRequest::get("https://example.com")` and `OutboundRequest::get("https://example.com:443")` produce identical `backend_target()` / `host_authority()` / `cert_host()` / `sni_hostname()` outputs (`"example.com:443"`, `"example.com"`, `Some("example.com")`, `Some("example.com")` respectively). `http://example.com:80` likewise normalises against `http://example.com`. Explicit non-default ports (`:8443`) are preserved in `backend_target()` and `host_authority()` but stripped from `cert_host()` / `sni_hostname()`. Asserted in `crates/edgezero-core/src/outbound.rs` `#[cfg(test)]` — no adapter | yes | — | — |
| URI canonicalization — **Fastly backend identity half (Tier 2 / Tier 3):** building the canonical inputs above through the Fastly adapter yields **one dynamic backend** per canonical tuple — the identity hash collapses `https://example.com` and `https://example.com:443` into the same `Backend` entry in the registered-backend map. Tier 2 inspects the map; Tier 3 (Viceroy) observes the single backend across both URI spellings | — | yes | yes |
| URI scheme + host case normalisation — **core accessor half (Tier 1):** `OutboundRequest::get("https://EXAMPLE.com")`, `OutboundRequest::get("HTTPS://example.com")`, and `OutboundRequest::get("https://example.com")` produce identical `uri().host()`, `uri().scheme()`, `backend_target()`, `host_authority()`, and `cert_host()` outputs (all lowercase). Path / query are case-preserving (fragments are rejected upstream — round 29). Asserted in core | yes | — | — |
| URI scheme + host case normalisation — **Fastly identity half (Tier 2 / Tier 3):** same canonical inputs produce identical Fastly backend identity across the three case variants — one registered backend, same identity hash | — | yes | yes |
| `OutboundRequest::get("https://example.com/p#anchor")` and `::post(..)` return `bad_request("outbound URI must not contain a fragment")` — fragment detected on the raw input string *before* `http::Uri` truncates at `#`. `OutboundRequest::new(method, uri)` accepts a `Uri` that has already lost the fragment (documented asymmetry per §3.1.3) | yes | — | — |
| Capability enforcement: a manifest requiring `lazy-streamed-response-passthrough` causes the **`edgezero demo` runner** (contributor-only, the PR-#269 replacement for the removed `dev` command) to exit non-zero with the Axum BestEffort hard-fail message — via `run_demo(..)`'s sibling pre-dispatch gate against the Axum adapter, *not* via the `execute(..)` path (`demo` does not flow through it). The same hard-fail also fires via `execute(..)`'s pre-dispatch gate on `build` / `serve` / `deploy` (**not** `auth` — exempt), and via the `run_config_push_typed` / `run_config_validate` / `run_provision` siblings. Test asserts every **gated** command exits non-zero, and that `config diff` / `auth *` do **not** | yes | — | — |
| `[capabilities.outbound].hosts` Spin render output is canonicalized: `["HTTPS://EXAMPLE.com:443", "api.example.com"]` → rendered `spin.toml` shows `["https://example.com", "https://api.example.com"]` (lowercase scheme/host, default port stripped, default-scheme https for bare hosts) | yes | — | — |
| Fastly `send_all` dispatch-overhead slack hard-bounded: with the adapter's **`test-utils`-gated** injection hook (NOT `#[cfg(test)]` — external integration tests compile the lib without it, §5.5) set to `Duration::from_millis(50)`, a `send_all` of N requests returns an `EdgeError::internal` whose message **contains the stable substring `"BATCH_DISPATCH_SLACK_MAX"`** (the full normative diagnostic per §4.3 is `"Fastly send_all adapter overhead between batch_now and SDK arming (preflight + dynamic-backend lookup/creation + SDK setup) exceeded BATCH_DISPATCH_SLACK_MAX; refusing to arm SDK timers with stale duration"`) for the slots dispatched after the cumulative delay crosses `BATCH_DISPATCH_SLACK_MAX` (25 ms). Without the hook, no slot ever returns that error. A handler-side `thread::sleep` before `send_all` is **not** sufficient — it runs before `batch_now` is captured and cannot exercise the guard. Tests assert against the substring, not the full string, so future wording polish doesn't break them. **The hook lives in the Fastly adapter crate**, so this row is Tier 2 (substring assertion in `crates/edgezero-adapter-fastly/tests/contract.rs`) + Tier 3 (Viceroy with hook) — not Tier 1 (Tier 1's `MockOutboundClient` has no SDK arming step to wrap) | — | yes | yes |
| Fastly dispatch+headers phase-budget split **(common case, `total_ms ≥ 4`)**: a single `send` to a target that never returns headers fires the host timeout at `connect_ms + first_byte_ms = budget.duration`, **not** `2 × budget.duration`. Two separate test fakes — one that hangs the TCP connect, one that hangs after request bytes are sent — each return 504 within `budget.duration + BATCH_DISPATCH_SLACK_MAX + ms_rounding` (< 29 ms + budget), never twice the budget. The sub-4 ms degenerate branch is covered by the row below | — | yes | yes |
| Fastly single-`send` dispatch-overhead slack guard: the same **`test-utils`-gated** injection hook used for `send_all` (round 31) also wraps the single-send path between `dispatch_budget` and `send_async`; with the hook set to 50 ms, a single `send` returns `internal("Fastly send adapter overhead between dispatch_budget and SDK arming exceeded BATCH_DISPATCH_SLACK_MAX; …")`. Single send is **not** "structurally 0 slack" — the same hard constant applies (round 38) | — | yes | yes |
| Fastly body-phase EOF deadline: an upstream that sends headers + N-1 chunks within budget but holds the final read so EOF arrives *after* `budget.deadline` returns `gateway_timeout`, not `Ok(resp)`. Buffered drain checks `is_expired()` after every blocking read including EOF; streamed wrapper checks before and after each underlying read so the consumer sees an `Err` chunk instead of clean stream-end | — | yes | yes |
| `OutboundResponse::into_bytes_bounded_until(max, until)` with `until` **tighter** than `dispatch_budget(req).deadline`: the helper drives a streamed body whose adapter wrapper has 500 ms of effective budget left, but the caller passes `until = now + 100 ms`. The upstream sends data for 90 ms then holds the final read; EOF arrives at 110 ms. The helper returns `gateway_timeout` (not `Ok(bytes)`) because its `until_deadline.is_expired()` check fires before and after the EOF read. (`OutboundResponse` carries no effective-deadline state; the wrapper enforces the request budget separately — whichever fires first wins) | — | yes | yes |
| Fastly phase-split trade-off, documented: a 1 s `send` to a target that takes 300 ms to connect and 10 ms to send first-byte **fails** at the `connect_ms = 250 ms` timer (1/4 of budget) even though the entire exchange would have fit within 1 s. This is the explicit deviation §4.3 documents — preferring the absolute-deadline bound over the "every legal slow-connect request succeeds" property. The `outbound-flexible-phase-budget` capability is `BestEffort` on Fastly (§3.5.1 / §3.5.2 footnote 5); apps that need elastic phase budget declare it required and get the hard build failure on Fastly. §8 risk 9 tracks the configurable-split follow-up | — | yes | yes |
| Required `outbound-flexible-phase-budget` on Fastly → every **gated** CLI command (`build` / `serve` / `deploy` / `provision` / `config push` / `config validate` / `demo`) exits non-zero with the BestEffort hard-fail message via the §3.5.3 pre-dispatch gates (one inside `execute(..)` branching to skip `auth`, four siblings on `run_provision` / `run_config_push_typed` / `run_config_validate` / `run_demo`); `config diff` and `auth *` are exempt (read-only / credential); same manifest on Axum / Cloudflare / Spin passes | yes | — | — |
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

**Where Tier 2 runs per adapter.** Only Axum's `tests/contract.rs` is native, so
it (plus the Tier 1 core suite) runs under the host `cargo test --workspace
--all-targets` gate. The three WASM adapters' contract tests are
`#![cfg(all(feature = "<adapter>", target_arch = "wasm32"))]`-gated, so they do
**not** run on the host gate; they execute in `test.yml`'s existing per-adapter
wasm-target matrix step — `cargo test -p edgezero-adapter-<adapter> --features
"<adapter>,test-utils" --target <wasm-target> --test contract` (Fastly `wasm32-wasip1`,
Spin `wasm32-wasip2`, Cloudflare `wasm32-unknown-unknown`). **`test-utils` MUST be in the
`--features` list** or the seams are absent and the Tier 2 rows silently don't run. The no-network Tier 2
assertions (registered-backend map inspection, SDK call/chunk counters, harvest
ordering against host-side fakes) run there; Tier 3 wall-clock jobs remain the
separate runtime jobs above.

**Two additional CI gates are directly broken by this change and must be updated
(previously omitted from this section).** Both are hard gates, not optional:

1. **Generated-project gate** — `edgezero new` scaffolds against the outbound API.
   The `proxy → outbound` rename means the scaffold templates (`handlers.rs.hbs`,
   `spin.toml.hbs`, adapter templates) emit code against **removed** APIs until they
   are migrated. The existing generated-project CI job must build a freshly scaffolded
   project for each adapter and fail if it references any `Proxy*` symbol.
2. **`examples/app-demo` gate** — app-demo is excluded from the workspace and has its
   own CI job and `Cargo.lock`. It is a first-class consumer of `ProxyHandle` /
   `proxy_handle()` and of `RequestContext`'s removed `request()` / `json()` / `form()`
   accessors, so it breaks on both the rename **and** the §3.4.5 context restructure.
   Its job must be green before merge.

**Executable test seams — must be `test-utils`-gated, NOT `#[cfg(test)]`.** Several
Tier 2 assertions in §5.4 depend on seams that do not exist today. They are
**first-class deliverables**, not incidental scaffolding.

> **⚠️ `#[cfg(test)]` does not work here.** Each adapter's contract tests live in
> `crates/edgezero-adapter-*/tests/contract.rs` — an **external integration test**.
> Cargo compiles the library as a *dependency* of that test target, **without**
> `cfg(test)`, so any `#[cfg(test)]` item in the adapter's `src/` is **invisible** to
> `tests/contract.rs` and the test will not compile. Earlier drafts specified these
> hooks as `#[cfg(test)]`, which is unimplementable. **Every seam below is gated on a
> `test-utils` cargo feature** (the same mechanism `edgezero-core` already uses for
> `MockOutboundClient` — §7). **`tests/contract.rs` sees the seam by being COMPILED with
> that feature enabled** — `cargo test -p edgezero-adapter-<a> --features "<a>,test-utils"
> --test contract` — **NOT** via a `[dev-dependencies]` self-reference: a package cannot
> depend on itself (`edgezero-adapter-<a> = { path = "." }` is a Cargo error). So the
> wiring is at *invocation* time (the feature flag), not a dependency edge — this matches
> the §7 inventory. The alternative — moving these assertions into in-crate `#[cfg(test)]`
> unit tests under `src/` — is acceptable per-assertion, but the seam must then not be
> referenced from `tests/`. Pick one per assertion and say which.

Seams required, **per adapter** (not Fastly-only — CF and Spin need transport/timer
seams for their own Tier 2 rows):

- **Fastly — recording builder, NOT a backend inspector.** A cache/`Backend` inspector
  **cannot** prove the SSL / SNI / certificate / host-override calls: Fastly's `Backend`
  is **opaque** — its public getters do not round-trip `sni_hostname` / `check_certificate`
  / `override_host` / the timeout phase split, so inspecting a cached `Backend` observes
  almost nothing the §5.4 rows assert. The seam must instead be a
  `#[cfg(feature = "test-utils")]` **recording `BackendBuilder`** — a thin trait the
  adapter builds through (`.override_host(..)`, `.sni_hostname(..)`,
  `.check_certificate(..)`, `.connect_timeout(..)`, …) whose test impl **records every
  call** into an observable log. The identity / canonical-accessor / phase-split rows
  assert against that recording. (The per-session cache field — §4.3 — still
  gets a `test-utils` accessor for the *collision/reuse* rows, but that only proves
  name→identity bookkeeping, not the builder calls.)
- **Fastly — injectable clock / dispatch-overhead hook.** A `test-utils` `Duration`
  injection point between `dispatch_budget` and SDK arming, used by both the `send_all`
  and single-`send` `BATCH_DISPATCH_SLACK_MAX` rows. Without it the slack guard is
  **untestable**: a handler-side `sleep` runs *before* `batch_now` is captured and
  cannot exercise the guard.
- **Fastly — send-failure classification needs NO seam (by design).** Neither
  `SendError` (private fields, no public ctor) nor `SendErrorCause` (`#[non_exhaustive]`)
  can be constructed in a test, so no injectable "send-result provider" can hand a test a
  real one. Instead the policy is a **pure function over a locally-defined constructible
  `SendFailure` enum** (§4.3), unit-tested directly; only the thin
  `SendErrorCause -> SendFailure` boundary match is untestable by unit tests, and it is
  covered by Tier 3. Do not spec a fake that fabricates SDK error types — it is
  impossible.
- **All adapters — dispatch counters.** `did_dispatch()` / chunk-write count behind
  `test-utils`, so "deadline expired during drain → 504 **and** no upstream send" and
  the partial-upload rows can assert the *absence* of a dispatch.
- **Fastly — a dispatch/pending abstraction, not just the recording builder + clock.** The recording `BackendBuilder` and the injectable clock prove backend *construction* and the slack guard, but the send_all conformance + phase-timeout + error-mapping rows need to script the **exchange**: `PendingRequest::poll`/`wait` outcomes, header/connect hangs, and responses. Those SDK types are not constructible or scriptable in a test. So the adapter dispatches through a `test-utils` trait — e.g. `trait FastlyDispatch { fn dispatch(..) -> PendingLike; }` with a `PendingLike` the fake can resolve to a scripted response / hang / **local constructible `SendFailure`** (NOT the SDK's `SendErrorCause`, which is `#[non_exhaustive]` with private fields and cannot be constructed in a test — see the `classify(SendFailure)` boundary above) — the real impl wrapping `send_async`/`PendingRequest`. Rows that still cannot be faked this way (real TCP/TLS timing) are **Tier 3 (Viceroy) only**, not Tier 2; the §5.4 tier column reflects that.
- **Cloudflare / Spin — transport + timer fakes.** Their Tier 2 rows (deadline expiry
  per phase, transport-error mapping, upload-stall) assert platform-observable
  behaviour and have **no provider fake today**. Each needs a `test-utils` seam
  that (a) substitutes the outbound transport (`worker::fetch` / `wasip3::http::client::send`)
  with a scriptable fake, and (b) drives the platform timer deterministically.
  **These seams are first-class deliverables of the same change that lands the adapter
  work — they are NOT a waiver.** Per §5.2, a Tier 2 row is *not* dropped because its
  seam does not exist yet; the seam is built and the row ships with it. (The earlier
  "until these exist, the rows are not executable and must not be listed as required"
  wording contradicted §5.2 and is removed.) Rows that genuinely need a real platform
  runtime for **wall-clock/throughput** reasons (real TCP/TLS timing, the full `join_all`
  concurrency fan-out) are deferred, and those are **Tier 3**, not Tier 2. **The one
  carve-out is host-observed cancellation:** because the `Native` deadline/upload claims
  rest on the host actually tearing the subrequest down, the **focused** cancellation
  test (CF: origin observes the abort; Spin: `[subtask-cancel]` leaves no pump running) is
  a **blocking Tier 3 deliverable, exempt from deferral** — see §5.3 *Applying that rule
  to cancellation*. If it is not implemented, the capability is declared `BestEffort`, not
  `Native`. Only the *broader* runtime concurrency suite stays deferred.

**Capability diagnostics must not point into `docs/superpowers/`.** The hard-fail
messages emitted by `ensure_capabilities` currently reference capability footnotes in
this spec, but `docs/superpowers/**` is `srcExclude`d from the published VitePress site
— a user hitting the error has no reachable link. User-facing capability documentation
(the nine capabilities, the support matrix, and the per-adapter caveats) must be
mirrored into a published page under `docs/guide/` and the diagnostics must link there.

**Spin gate triple — now wasip2 (PR #269 merged).** The fifth gate's literal
command string changed with PR #269, which has since merged to main:

- **Active (post-#269, current main):** `cargo check -p edgezero-adapter-spin
  --target wasm32-wasip2 --features spin` — Spin SDK 6 / wasip2. This is the
  gate this spec is written against.
- **Historical (pre-#269):** `cargo check -p edgezero-adapter-spin --target
  wasm32-wasip1 --features spin` — the SDK 5 / wasip1 form. **The `CLAUDE.md` gate quote is
  already the wasip2 form** (verified in-tree). **But two stale Spin→wasip1 spots remain and
  ARE follow-up work** (so this is NOT fully closed, and §7 correctly still lists it): (i)
  `.cargo/config.toml`'s header comment associates Spin with `wasm32-wasip1` (Wasmtime for
  spin) — stale, Spin is wasip2 since #269; fix the comment; (ii) any surviving `wasm32-wasip1`
  Spin references in prose. The remaining *correct* `wasm32-wasip1` mentions are Fastly's.

The other four gates are unaffected and apply identically.

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
  proxy-forward (`from_request`) is **preserved**.
- **Public-surface removals — this is a breaking migration, not "no capability lost".**
  Earlier drafts claimed nothing public was dropped; that is false. The current
  `crates/edgezero-core/src/proxy.rs` exposes several public items the new types
  deliberately do **not** carry. Each is an explicit decision, not an oversight:

  | Removed public item (proxy.rs) | Disposition |
  | --- | --- |
  | `ProxyHandle::client() -> Arc<dyn ProxyClient>` | **Dropped, no analogue.** `HttpClient` intentionally does not expose the underlying client trait object — apps call `send` / `send_all`, not the raw client. Any downstream caller of `.client()` migrates to the request methods. |
  | `ProxyRequest::body_mut()` / `extensions()` / `extensions_mut()`, `ProxyResponse::body_mut()` / `extensions()` / `extensions_mut()` | **Dropped.** The new types are builder-style (immutable after construction) and carry **no `Extensions`** — mutation and extension-stashing were only used internally and are replaced by the body-cell model. No public re-add. |
  | `RequestContext::request_mut()` | **Dropped** — the §3.4.5 restructure removes it (`parts_mut()` covers header/method mutation; the body is a cell). |
  | `PROXY_HEADER` (`x-edgezero-proxy`) + each adapter's `x-edgezero-proxy: <name>` response insert | **PRESERVED (locked).** It is public, observable behavior; every adapter keeps inserting it on proxied responses under the new `*OutboundClient` types. The `PROXY_HEADER` constant moves to `outbound.rs` but keeps its value `"x-edgezero-proxy"`. |

  A `rg 'ProxyHandle::client|\.body_mut\(|extensions_mut\(|request_mut\(|into_request\('`
  sweep is part of the migration completion gate (§7) — every hit is either migrated or
  confirmed in-tree-only. **`into_request(` MUST be in the sweep** because `into_request()`
  becomes **fallible** (`-> Result<Request, EdgeError>`, §3.4.5), so every call site needs a
  `?`/handling change — and the ones most likely to be missed are **outside normal workspace
  compilation**: `examples/app-demo/` (excluded from the workspace) and the
  **scaffold templates** (`crates/edgezero-cli/src/templates/**/*.hbs`, e.g.
  `handlers.rs.hbs` — `.hbs` files don't compile at all). So the sweep runs **repo-wide
  (including `examples/` and `templates/`)**, and the migration adds a **generated-project
  verification** step (`edgezero new` a scaffold, then `cargo build` it) so the template's
  `into_request()` call compiles against the fallible signature.
- **Adapters** set `HttpClient` (not `ProxyHandle`) into request extensions — same
  mechanism, new type.
- **`EdgeError`** gains `BadGateway` / `GatewayTimeout` (Phase 1a) and `ResponseTooLarge`
  (outbound phase, the distinct over-cap outcome §3.4.1) — all additive (`#[non_exhaustive]`).
- **`Manifest`** gains `capabilities` (with nested `outbound`) — the field is additive
  (`#[serde(default)]`), so **schema-conforming** manifests parse unchanged. But the
  top-level `#[serde(deny_unknown_fields)]` added alongside it is an **intentional break**:
  a manifest with an unknown/misspelled top-level section now fails to parse (fail-closed;
  see §3.5.1). Not "all existing manifests parse unchanged."
- **`Adapter` trait** gains `capability()` — all four registered adapters implement it.
- **CLI** dispatch (PR #269, now on main): `ensure_capabilities` is wired in at
  **five pre-dispatch gate sites** (§3.5.3, gated by command **class** — `auth *` and
  `config diff` are **EXEMPT**) — one inside `edgezero_cli::adapter::execute(..)`
  **covering only `build` / `serve` / `deploy`** (the gate **branches on the action and
  skips the `Auth*` actions**), *before* the manifest-shell-command branch and *before*
  the registry lookup; plus **four siblings** at the top of `run_provision`,
  **`run_config_push_typed`**, `run_config_validate`, and the contributor-only
  `run_demo`. **`run_config_diff_typed` is NOT a gate site** — `config diff` is read-only
  and exempt (gating it is the "block a read-only diff on a runtime mismatch" bug the
  class split fixes). (The bundled `run_config_push` is a v1 stub that errors — the
  **typed** function is the real writeback path, so it is the gate site.) `dev` is gone;
  `demo` is the contributor-only replacement that routes through Axum via its own sibling
  gate.
- **Scaffolding templates** — `handlers.rs.hbs` and any adapter templates that emit
  proxy code are updated to the new types; `spin.toml.hbs:13` renders
  `allowed_outbound_hosts` from `[capabilities.outbound].hosts` instead of the hardcoded
  `["https://*:*"]`. Without this, `edgezero new` would scaffold code against removed
  APIs.
- **Shipped examples MUST declare the capability they exercise.** The generated root
  manifest (`templates/root/edgezero.toml.hbs`) and `examples/app-demo/edgezero.toml`
  both include a proxy/outbound route but currently declare **no** `[capabilities]`, so
  the flagship examples never exercise the very gate this spec adds — and worse, model
  the wrong pattern for users. Both gain `[capabilities]\nrequired = ["outbound-http"]`
  (plus `[capabilities.outbound].hosts` for the demo's real targets). **Generated-project
  validation asserts it**: after `edgezero new`, `edgezero config validate` on the scaffold
  must find the declaration and pass. **What the row does NOT assert:** that *stripping*
  `outbound-http` makes the build fail — because enforcement examines only **declared**
  capabilities and does **not infer** requirements from routes or handler code, a stripped
  declaration yields an *empty* contract that **passes**. (Route→capability inference is a
  separate, larger feature, out of scope here; noted so no one writes a "strip → fail" test
  that cannot pass.) The gate's fail path is proven independently by the §5.4
  `ensure_capabilities` unit rows (required + Unsupported/BestEffort → `Err`). So this row
  asserts the positive — the shipped example *declares* what it exercises — which is the
  achievable half of closing the loop.
- **NEW published capability reference — `docs/guide/capabilities.md`.** The nine
  capabilities, the support matrix, and the per-adapter caveats (including Fastly
  `outbound-deadlines` = `BestEffort`) must be mirrored here from §3.5, because
  `ensure_capabilities`' diagnostics link to `https://edgezero.dev/guide/capabilities`
  and `docs/superpowers/**` is `srcExclude`d from the site (see "Capability diagnostics
  must not point into `docs/superpowers/`"). **Also add the page to the VitePress sidebar**
  in `docs/.vitepress/config.mts` — an unreferenced page renders but is unreachable by
  navigation, so the diagnostic link and the sidebar entry ship together.
- **Public docs (VitePress under `docs/guide/`)** — rewrite every page referencing
  `ProxyService` / `ProxyRequest` / `ProxyResponse` / `ProxyHandle` / `proxy_handle` /
  the deprecated `ProxyClient`. Known hits at the time of writing:
  `docs/guide/proxying.md`, `docs/guide/handlers.md`, `docs/guide/architecture.md`,
  `docs/guide/what-is-edgezero.md`, the per-adapter pages under `docs/guide/adapters/`,
  and the streaming docs. **Also fix `docs/guide/adapters/spin.md`** — it documents
  `edgezero new --adapter spin`, but `NewArgs` has **no `--adapter` flag** (only `name` +
  `--dir`); replace with the real invocation. (Listed explicitly because it is a
  *command-correctness* fix, not a Proxy-rename hit, so the sweeps below would miss it.)
  The new streaming proxy-forward example uses
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
  `pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 1 * 1024 * 1024;` and
  `pub const DEFAULT_OUTBOUND_REQUEST_BODY_BYTES: u64 = 8 * 1024 * 1024;` — **`u64`, not
  `usize`**, so the cap type does not ceiling at 4 GiB on `wasm32` (see the byte-cap note
  in §3.1.3). (The inbound `DEFAULT_INBOUND_*` caps carry the same `wasm32` consideration;
  they are part of the separate inbound work — §3 of the consumer feedback — and should
  adopt `u64` when that lands.)
- `src/time.rs` — new module. Contents:
  - `Deadline` (value type, §3.3.1)
  - `DispatchBudget { duration: Duration, deadline: Deadline, cause: BudgetSource }` (§3.3.2).
    The `cause` field's type **`BudgetSource` is NOT defined here** — it is defined in
    `error.rs` (Phase 1a Task 1, because `GatewayTimeout` carries it); `time.rs` does
    `use crate::error::BudgetSource;` and only *references* it. (Single owner: `error.rs`.)
  - `pub fn dispatch_budget(req: &OutboundRequest, now: web_time::Instant) -> Result<DispatchBudget, EdgeError>` (§3.3.2)
  - Constants (§3.3.1, §3.3.4, §4.3):
    - `pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);`
    - `pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_hours(168);` (7 days; `from_hours`, not `from_secs(7*24*60*60)` — `duration_suboptimal_units`)
    - `pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);` (round 29)

  The earlier "value type only" wording was stale before round 23 introduced
  `DispatchBudget` and the explicit `now` parameter; this is the complete
  current contents of the file.
- **No `src/capability.rs`.** `Capability` / `CapabilitySupport` are defined **inline in
  `src/manifest.rs`** (the `include!`d file — see the `edgezero-macros` block below) and
  re-exported: `pub use manifest::{Capability, CapabilitySupport};`. A standalone
  `capability.rs` that `manifest.rs` imported would fail to compile in the macro crate.
- `src/error.rs` — add the outbound error variants and their **complete** surface. Phase 1a
  adds `BadGateway`/`GatewayTimeout`; the outbound response-handling phase adds
  `ResponseTooLarge` (502, kind `response_too_large` — the distinct over-cap outcome, §3.4.1).
  The existing `EdgeError` is `#[non_exhaustive]` with per-variant `kind_str()` / `message()` /
  `status()` / `inner()` arms and an exhaustive `IntoResponse` JSON body, so every
  arm must be updated or the crate won't compile:
  - `BadGateway { message: String }` — `bad_gateway(msg)` constructor; `status()` →
    `502`; `kind_str()` → `"bad_gateway"`; `message()` → the stored `message`;
    `inner()` → `None`.
  - `GatewayTimeout { message: String, cause: BudgetSource }` — `gateway_timeout(msg)`
    (cause `Unspecified`) and `gateway_timeout_caused(msg, cause)` constructors;
    `status()` → `504`; `kind_str()` → `"gateway_timeout"`; `message()` → the stored
    `message`; `inner()` → `None` (the `cause` is a typed field, not a source error).
    Every exhaustive `match` destructures `{ message, cause }` (or `{ message, .. }`).
  - `ResponseTooLarge { message: String }` (outbound phase) — `response_too_large(msg)`
    constructor; `status()` → `502`; `kind_str()` → `"response_too_large"`; `message()` →
    the stored `message`; `inner()` → `None`; no `Retry-After`, no `field_path`.
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[non_exhaustive] pub enum BudgetSource { BatchDeadline,
    Default, PerCallTimeout, Unspecified }` is defined **in `error.rs` (this Task)** so the
    Task-1 commit builds standalone (a `time`-module home would make the Task-1 commit fail
    to compile, since `GatewayTimeout` names it). The derives and alphabetical order are
    compile-verified. `StoredError` mirrors the `cause` field and adds a `ResponseTooLarge`
    arm (§3.4.5) — total match, or it won't compile (and `Clone`/`Copy` on `BudgetSource` is
    exactly what `StoredError`'s derive + `capture`'s `*cause` require).
  - Both serialize through the existing `IntoResponse` JSON shape — which is
    **`{ "error": { "status", "kind", "message", "field_path"? } }`**, not
    `{kind, message}`. The real converter (`error.rs`) inserts `status` (the numeric
    code), `kind`, `message`, and `field_path` **when present**. `BadGateway` /
    `GatewayTimeout` carry no `field_path`, so their bodies are
    `{ "error": { "status": 502|504, "kind": "bad_gateway"|"gateway_timeout", "message": … } }`.
    Add exhaustive-match tests covering
    status code, `kind` string, and serialized body for each.
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
- `src/manifest.rs` — add `Capability` / `CapabilitySupport` (inline; §3.5.1),
  `ManifestCapabilities` + `ManifestOutboundCapability` (with `hosts: Option<Vec<String>>`
  — §3.5.4, so *absent* is distinguishable from explicit `["*"]`), and
  `Manifest::capabilities`. **Also add `reject_misplaced_capabilities(&toml::Value)` (§3.5.1)
  AND wire it into EVERY parse entry point — this is normative, not just a new field.** The
  scan must run BEFORE deserialization on ALL paths, or the depth-independent fail-closed
  guarantee only holds on whichever loader remembered it:
  - the **CLI/library file loaders** (`Manifest::from_toml` / `load_manifest` /
    `ManifestLoader`) — parse to `toml::Value`, scan, then deserialize;
  - **`Manifest::from_baked_json`** (below) — scan the baked JSON `Value` before parse+finalize;
  - the **`app!` macro's compile-time baking** (`crates/edgezero-macros/src/app.rs`) — the
    macro parses the manifest at compile time, so it runs the SAME scan and a misplaced
    `capabilities` becomes a **compile error** (add a `trybuild` **compile-fail** test).
  Factor the scan into a single `pub fn` both the runtime loaders and the macro call, so
  there is one implementation, not three. Also add the **three-state `pub enum BakedManifest
  { Absent, Present(&'static Manifest), Malformed(&'static str) }`** and **`pub fn
  Manifest::from_baked_json(&'static str) -> BakedManifest`** — scan **plus** parse **plus
  `finalize()`** — because the `app!`-generated `manifest()` accessor lives downstream
  and cannot call the `pub(crate)` `finalize()` itself (§3.5.3). **Not `Option`:** it
  must keep "no contract" (`Absent`) distinct from "corrupt contract" (`Malformed`), or
  the gate fails **open** on malformed input.
- `src/app.rs` — extend the `Hooks` trait with the **baked-manifest accessors** the
  `demo` capability gate depends on (§3.5.3): `fn manifest_json() -> Option<&'static str>`
  (defaulted `None`) and `fn manifest() -> BakedManifest` (defaulted **`Absent`**, with
  the per-impl function-local `OnceLock` living in each *generated* impl and calling
  `Manifest::from_baked_json`). **Two independent reasons the macro must emit both
  explicitly:** (1) the workspace denies `clippy::missing_trait_methods`, so relying on
  a trait default is a hard error; (2) a `static` in a trait **default** body is **one
  item shared by all implementors** (proven), so a caching default would serve app A's
  manifest to app B. `ensure_capabilities` takes a lifetime-bearing `ManifestContract<'_>` (**not** `BakedManifest` — the file-backed gate sites hold non-`'static` local borrows); `ManifestContract` and the inherent `BakedManifest::as_contract()` conversion both live in `edgezero-core` beside `BakedManifest`, not in the CLI crate. The gate **fails closed** on
  `Malformed`.
- `src/lib.rs` — re-export new modules; drop proxy re-exports.
- `Cargo.toml` — `MockOutboundClient` under the existing `test-utils` feature.

**Each adapter crate (`edgezero-adapter-{fastly,cloudflare,spin,axum}`)** *(inventory
completeness — the Tier 2 seams §5.5 requires are cargo wiring, not just prose)*
- `Cargo.toml` — add a **`test-utils` feature** gating the injectable seams (recording
  `BackendBuilder` / injectable clock / dispatch hook for Fastly; transport + timer fakes
  for CF/Spin). **NO self dev-dependency** — Cargo rejects a package depending on itself
  (`edgezero-adapter-x = { path = "." }` is an error). An external integration test
  (`tests/contract.rs`) sees a feature by being **compiled with that feature enabled**, so
  the wiring is at *invocation* time, not a dependency edge.
- **Contract-test commands must enable `test-utils`.** The current CI command
  (`.github/workflows/test.yml`) runs `cargo test -p edgezero-adapter-<a> --features <a>
  --target <triple> --test contract` — it **omits `test-utils`**, so the seams are absent
  and the Tier 2 rows silently don't run. Change to `--features "<a>,test-utils"` for
  **every** adapter, **including Axum** (`cargo test -p edgezero-adapter-axum --features
  "axum,test-utils" --test contract`). Without this the §5.5 seams are invisible and the
  Tier 2 rows cannot execute.

**`crates/edgezero-macros`** *(previously omitted from this inventory — it is not
optional: the macro crate **textually includes** core's `manifest.rs`, so every manifest
change lands here whether intended or not)*
- `src/manifest_definitions.rs` — **the constraint, not a task.** This file does
  `include!(concat!(…, "/../edgezero-core/src/manifest.rs"))`, so `manifest.rs` is
  compiled **a second time inside the macro crate**, where `edgezero_core` does not
  exist as a path. And the dependency runs **core → macros** (`edgezero-core/Cargo.toml`
  depends on `edgezero-macros`), so macros **cannot** depend on core to fix it — that
  would be a cycle. Consequences, both mandatory:
  1. `Capability`, `CapabilitySupport`, `ManifestCapabilities`, and
     `ManifestOutboundCapability` must be defined **inside `manifest.rs` itself** and
     must not `use` anything the include context lacks. A separate
     `edgezero-core/src/capability.rs` that `manifest.rs` imports **will not compile in
     the macro crate**. Core re-exports them (`pub use manifest::{Capability, …}`) so
     the rest of core still refers to them by their normal path.
  2. They must derive **`Serialize`** as well as `Deserialize` — `Manifest` derives
     `Serialize` and `app!` calls `serde_json::to_string(&manifest)`. A capability type
     that is `Deserialize`-only breaks the `Manifest` derive.
  **Gate:** `cargo build -p edgezero-macros` must be run after *every* `manifest.rs`
  edit; a workspace `cargo check` alone will surface this only via the macro crate.
- `src/app.rs` — emit **both** baked-manifest accessors in the generated `impl Hooks`
  block (the macro already denies `clippy::missing_trait_methods`, so it cannot rely on
  the trait defaults — same reason it already emits `configure`/`build_app`):
  `fn manifest_json() -> Option<&'static str> { Some(#manifest_json_lit) }` and the
  `manifest()` body (per-impl `OnceLock` → `Manifest::from_baked_json`). The JSON literal
  already exists (`manifest.finalize()` → `serde_json::to_string(&manifest)` →
  `manifest_json_lit`); today it is embedded for the router and never exposed. No new
  compile-time parsing, no new macro dependency.

**`crates/edgezero-adapter`**
- `Cargo.toml` — **add `edgezero-core` as a workspace dependency.** `Capability` /
  `CapabilitySupport` live in `edgezero-core` (so manifest parsing can use them), and
  the `Adapter` trait references them; the crate currently has no dependency on core
  and that must be added. The direction (adapter → core) is the standard one and
  introduces no cycle.
- `src/registry.rs` — add `Adapter::capability()`. **AND the `ProvisionStores` change
  (§3.5.4):** add `pub hosts: Option<&'a [String]>`, add `#[non_exhaustive]`, and add a
  `pub fn ProvisionStores::new(config, kv, secrets, hosts)` constructor. **This is a
  breaking public-API change, inventoried here:** (a) migrate the **~17 in-tree struct
  literals** `ProvisionStores { config, kv, secrets }` → `::new(..)` (CLI + tests; grep
  `ProvisionStores {`); (b) a **minor/major version bump** per the crate's semver policy;
  (c) a **CHANGELOG** entry telling out-of-tree adapter authors to migrate their literals;
  (d) an **external-consumer fixture** in `tests/` that constructs `ProvisionStores` via
  `::new(..)` to guard the constructor as the supported path. No `..Default::default()`
  (non_exhaustive + no `Default`; see §3.5.4).

**`crates/edgezero-adapter-{axum,cloudflare,fastly,spin}`**
- `src/proxy.rs` → `src/outbound.rs` — `*OutboundClient` implementing
  `OutboundHttpClient::send` and `send_all`, buffered + streamed modes,
  decompressed-byte cap, header normalization for decompressed responses
  (strip `content-encoding` / `content-length`).
- `src/request.rs` — **stop pre-buffering the inbound body.** The adapter keeps
  building a core `Request` and keeps calling `RequestContext::new(request, params)`
  unchanged (the parts/`BodyCell` split is internal — §3.4.5 *Construction contract*;
  `BodyCell` is **not** public and adapters never construct it). What changes is the
  **body** placed in that `Request`: wrap the platform body as a lazy `Body::Stream`
  instead of draining it eagerly (Axum buffers JSON with `usize::MAX` today;
  Cloudflare calls `req.bytes()`; Fastly and Spin fully materialize). Callers of the
  removed `ctx.request()` / `ctx.json()` / `ctx.form()` move to `parts()` /
  `body_bytes` / `json_within` / `form_within` (§6 sweep). All four adapters.
- `src/response.rs` — **per-adapter streaming policy.** Today each adapter's
  response converter (`crates/edgezero-adapter-{axum,fastly,spin}/src/response.rs`)
  buffers `Body::Stream` before producing the platform response. The migration
  preserves lazy streaming **where the platform allows it without violating core's
  `LocalBoxStream` (non-Send) invariant**:

  - **Cloudflare** — WASM, single-threaded JS event loop, no `Send` requirement on
    response bodies. `worker::Body::from_stream` consumes the `Body::Stream`
    directly; chunks flow without buffering.
  - **Fastly** — WASM, single-threaded guest, no `Send` requirement, **but**
    Fastly's lazy/early-streaming API (`Response::stream_to_client`) is
    incompatible with `#[fastly::main]` (Fastly SDK docs, capability footnote 6).
    The default scaffold therefore performs **buffered passthrough**: drain the
    wrapped `Body::Stream` to `Bytes` within the adapter-level constant
    `FASTLY_RESPONSE_STREAM_BUFFER_BYTES` (16 MiB) — the per-request
    `max_response_bytes` is unavailable here (`OutboundResponse` carries only
    status / headers / body, no cap metadata) — then return through the normal
    `#[fastly::main]` flow. Apps that need lazy passthrough
    on Fastly declare `lazy-streamed-response-passthrough` required and get a
    hard build failure (Fastly = `BestEffort` for this capability). The
    deadline-aware stream wrapper still runs on the buffered drain path — only
    the *passthrough* is buffered.
  - **Spin** — WASM, WASI async, no `Send` requirement, **but** the public response
    surface is concretely buffered: `spin_sdk::http::FullBody` backs the
    `SpinFullResponse` alias (`Response<FullBody<Bytes>>`), which appears in
    `AppExt::dispatch`, `request::dispatch*`, `from_core_response`, and `run_app`.
    Lazy passthrough would require migrating those **public aliases and signatures**
    plus a WASI-0.3 rewrite — a breaking API change deferred to §8 risk 13. The Spin
    response converter therefore performs **buffered passthrough**: drain the wrapped
    `Body::Stream` to `Bytes` within the adapter-level constant
    `SPIN_RESPONSE_STREAM_BUFFER_BYTES` (default 16 MiB, mirroring Axum and Fastly),
    over-cap → `response_too_large` (502, §3.4.1), then return the buffered `Bytes` through the
    existing `FullBody` flow. Spin is `BestEffort` for
    `lazy-streamed-response-passthrough` (footnote 7). The deadline-aware stream
    wrapper still runs on the buffered drain path — only the *passthrough* is
    buffered. The **outbound streamed-upload** path (§4.4) is unaffected and stays
    `Native`.
  - **Axum** — native, multi-threaded tokio. `axum::body::Body::from_stream` requires
    `Send + 'static`, which conflicts with core `Body::Stream = LocalBoxStream`
    (intentionally non-Send for WASM compat — `body.rs:14`). Designing a real
    `LocalBoxStream → Send` bridge (e.g. `spawn_local` + tokio mpsc) is non-trivial
    and out of scope for this migration. **The Axum response converter therefore
    buffers `Body::Stream` into `Bytes` (bounded, pre-append-checked) before
    constructing the axum response.** **The non-Send `LocalBoxStream` must NOT be held
    across an `.await` in the converter** — Axum's `tower::Service::Future` is `+ Send`
    (`service.rs`), so a plain `async fn` draining the stream would produce a **non-Send
    future** and fail to compile. So the drain runs inside
    **`tokio::task::block_in_place(|| futures::executor::block_on(drain_to_bytes(stream)))`**:
    `block_in_place` hands the worker's other tasks to sibling threads (so the reactor is
    NOT stalled — this is the sanctioned pattern, unlike a *bare* `block_on` which would
    wedge the worker), and the non-Send stream lives **entirely inside the blocking
    closure**, never across the outer Send future's await points. The result is `Bytes`
    (Send), which crosses the boundary cleanly. (Requires the multi-thread runtime, which
    Axum uses.) A timer-backed stream test (a source that yields on a `tokio::time`
    interval) proves the drain makes progress and the `Service` stays `Send`. The cap is a defined Axum-adapter constant
    `AXUM_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB` (a **fixed compile-time constant**;
    no `AxumOutboundConfig` plumbing in this migration). The per-outbound-request
    `max_response_bytes` is unavailable at this stage because the app has already
    consumed `OutboundResponse::into_response()` into a core `Response<Body>` and the
    original cap was attached to the now-discarded `OutboundRequest`. Apps that need
    a different ceiling either edit the constant in their fork, carry the bytes
    through a buffered path explicitly, or wait for the configurable follow-up
    tracked in §8 risk 6.
  - **`src/service.rs` (add to this inventory).** The Axum adapter's `tower::Service` impl
    (`service.rs`) declares `type Future = Pin<Box<dyn Future<..> + Send>>`. This is the
    constraint that forces the `block_in_place` drain above — it belongs in §7 because the
    converter change (`response.rs`) only compiles given this bound, and a reviewer must see
    both files together. No signature change to `service.rs` itself; it is listed so the
    Send requirement is not invisible.

    **Stream-error handling during buffered drain.** Because the Axum response
    converter buffers `Body::Stream` *before* writing any downstream response
    headers, it can map a stream error to a clean HTTP status (unlike the
    streaming-passthrough adapters, which would have to abort the wire because
    headers had already been sent — §3.1.1 post-header rule). The mapping is:

    Every abort row synthesizes the response the **same** way — `err.into_response()`,
    i.e. the standard JSON envelope `{ "error": { "status", "kind", "message" } }` — never
    a plain string body. That is what preserves the distinct `kind` (§3.4.1) and keeps the
    table consistent with the "502/504 response synthesis" paragraph below.

    | Stream chunk yields | Axum response (via `err.into_response()`) |
    | --- | --- |
    | `Ok(bytes)`, buffer + bytes.len() ≤ cap | append, continue |
    | `Ok(bytes)`, buffer + bytes.len() > cap | abort drain → `EdgeError::response_too_large(..)` → **502**, kind `"response_too_large"`, standard JSON envelope |
    | `Err(EdgeError::GatewayTimeout { .. })` | abort drain → **504**, kind `"gateway_timeout"`, JSON envelope |
    | `Err(EdgeError::BadGateway { .. })` | abort drain → **502**, kind `"bad_gateway"`, JSON envelope |
    | `Err(EdgeError::ResponseTooLarge { .. })` | abort drain → **502**, kind `"response_too_large"`, JSON envelope |
    | `Err(other EdgeError)` | abort drain → `err.status()` for that variant (`internal` → 500, etc.), JSON envelope |

    Source: the wrapped streamed body's `EdgeError` chunks already encode the
    intended status **and kind**; Axum just lifts them through `into_response()`. No silent
    coalescing-to-502, no plain-string bodies, no panic. This is the documented buffered-fallback
    behaviour: lazy streaming proxy-forward works **only on Cloudflare**
    (the sole `Native` adapter). Axum, Fastly, and Spin all buffer — for three
    distinct reasons (footnotes 3 / 6 / 7) — *but the buffering boundary lets each
    preserve the correct status code*. For fan-out handlers and most edge-shaped
    apps this is a non-issue; if true lazy streaming on Axum becomes a
    requirement later, an mpsc bridge is a separate follow-up. Capability text
    and risk section reflect this (see §3.5.2 footnote 3 and §8).

    **502/504 response synthesis — all three buffered adapters, not just Axum.** When a buffered-drain adapter (Axum, Fastly, Spin) hits an `EdgeError` while draining the wrapped `Body::Stream` (a `gateway_timeout`/`bad_gateway`/`response_too_large` error chunk, or over-cap), it must synthesize the platform response from **`err.status()`** + `err.into_response()`'s JSON body — the SAME concrete mapping §4.1 gives Axum. Do NOT let it degrade to a stringly platform error (today Fastly's `request.rs` returns a `FastlyError` string; Spin threads it through `anyhow`), which loses the status **and the distinct `kind`** (502-vs-504, and `response_too_large`-vs-`bad_gateway`). §5.4 tests each adapter's converter (**Axum, Fastly, Spin**) for 502, 504, over-cap, and an `internal` (500) error chunk — and **asserts the `kind` string, not just the status**: an over-cap chunk must yield `body_json["error"]["kind"] == "response_too_large"` (NOT merely status 502, which would also match a transport `bad_gateway` and defeat the distinct-outcome contract). One explicit `kind`-assertion row per adapter.

**Only Cloudflare streams `Body::Stream` lazily.** Axum, Fastly, **and Spin** all
  buffer `Body::Stream` to `Bytes` before returning (BestEffort, for three different
  reasons — non-Send `LocalBoxStream`, `stream_to_client()` vs `#[fastly::main]`, and
  Spin's buffered `FullBody` surface — footnotes 3/6/7). So the earlier "buffering is
  reserved for `Body::Once` on the three WASM adapters" is **wrong**: it holds for
  Cloudflare only. On Axum/Fastly/Spin the buffering path applies to **both** `Body::Once`
  and `Body::Stream`; on Cloudflare, `Body::Stream` streams and only `Body::Once` is
  trivially "buffered" (it already is bytes).
- adapter entry — register `HttpClient`; declare `capability()`.
- **Axum `Cargo.toml`** — do **NOT** enable reqwest's `gzip`/`brotli` features (auto-
  decode is exact-lowercase-only and cannot honour the portable case-insensitive/stacked
  policy); Axum uses the shared §3.4.1 decoder like the other adapters (the workspace
  reqwest dep stays `default-features = false`).
- Fastly:
  - Hash-based dynamic-backend naming (`format!("ez_{:032x}", sha256_128(identity))`,
    §4.3). The `edgezero-adapter-fastly/Cargo.toml` adds **`sha2` workspace
    dependency** for the SHA-256 digest; the 128-bit truncation is `&digest[..16]`.
    Alternatively, if a SHA-256 helper already exists in `edgezero-core` (audit step
    in the same sweep), the adapter uses that; either way the dep is declared
    explicitly in this migration, not assumed transitive. **Root `Cargo.toml`
    `[workspace.dependencies]` already declares `sha2` as of PR #269**, so only the
    Fastly crate's `sha2 = { workspace = true }` opt-in is new — no root-manifest
    edit is required.
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
    plus `.check_certificate(cert)` where `cert` came from an explicit
    `if let Some(cert) = req.cert_host()` (**not** `.unwrap()` — `clippy::unwrap_used`
    is denied in production) (`cert_host()` is `Some`
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
- Spin: `allowed_outbound_hosts` synchronization per §3.5.4 — **three touch points, not
  one**:
  - `src/templates/spin.toml.hbs:13` — replace the hardcoded literal `["https://*:*"]`
    with a render of `[capabilities.outbound].hosts` (absent → keep `["https://*:*"]`;
    the default is **not** widened to include `http://*:*`).
  - `src/cli.rs` — a new `toml_edit::DocumentMut` helper alongside
    `ensure_kv_label_in_component` that **sets** (not appends) `allowed_outbound_hosts`
    on the resolved component, preserving sibling fields and comments, honouring
    `--dry-run`. Called from `SpinCliAdapter::provision` — **`provision` is the only
    command that writes `spin.toml`.**
  - `crates/edgezero-cli/src/adapter.rs::execute` — the **validate-on-drift** check for
    `build` / `serve` / `deploy`, placed *before* the `manifest_command` branch.
    **It cannot go in `SpinCliAdapter::execute`:** scaffolded projects always declare
    `[adapters.spin.commands]`, so those commands shell out and never reach the
    adapter trait — a hook there is dead code. Compare **canonicalized sets**,
    order-insensitively; hard-fail with the expected list rendered for the user.
- `tests/contract.rs` — created for Axum; extended for the other three (§5).
- Tier 3 mock origin — new `MockServer` helper (`start_with_delay`, §5.3), a tokio
  loopback HTTP server used only by the native Tier 3 tests. It lives in the Axum
  adapter's `tests/` (or a shared `dev-dependencies` test-support module) so it never
  enters the WASM adapters' build graph; it is **not** the in-process
  `MockOutboundClient` (that is the Tier 1 core mock listed under
  `edgezero-core/Cargo.toml`).

**`crates/edgezero-cli`**
- `src/args.rs` — **change every defaulted `--manifest` arg from `#[arg(default_value =
  "edgezero.toml")] manifest: PathBuf` to `#[arg(long) manifest: Option<PathBuf>`** so
  provenance survives derive-parsing (§3.5.3). **The complete list of arg types (do not omit
  any):** `ProvisionArgs`, `ConfigPushArgs`, `ConfigValidateArgs`, and **`ConfigDiffArgs`**
  (config diff is gate-EXEMPT but still needs the manifest RESOLVED, and it has the same
  `default_value` `PathBuf` today — omitting it leaves a divergent path). `BuildArgs`,
  `ServeArgs`, `DeployArgs` have **no `--manifest` field today** and read `EDGEZERO_MANIFEST`;
  **decision (not either/or): keep them env-driven** — add the `Option<PathBuf>` flag ONLY if
  a `--manifest` flag is separately desired for them, but the resolver treats their source as
  `EnvVar`-or-`Defaulted` regardless.
- `src/lib.rs` (or a new `src/manifest_source.rs`) — add **`pub enum ManifestSource {
  ExplicitFlag(PathBuf), EnvVar(PathBuf), Defaulted }`** and **`pub struct ResolvedManifest {
  pub path: PathBuf, pub manifest: Manifest }`** and the **single** `pub fn
  resolve_root_manifest(source: ManifestSource) -> Result<Option<ResolvedManifest>, String>` (§3.5.3; `Ok(None)` = no manifest found → proceed),
  replacing the divergent `load_manifest_optional` (build) vs `ManifestLoader::from_path`
  (provision/config) paths. The resolved `ResolvedManifest` is threaded into execution — no
  reload. `config diff` calls this too (gate-exempt, but needs the manifest).
  **Public API note:** the `run_*` entry points currently take their arg structs; keep those
  signatures, but each `run_*` builds a `ManifestSource` from its args (via a small
  `impl From<&XArgs> for ManifestSource`) and calls `resolve_root_manifest`. A **typed
  library caller** that bypasses clap calls `resolve_root_manifest(ManifestSource::…)`
  directly — the enum is the seam, so it is unaffected by the arg-struct shapes.
- **Release/versioning (broader than ProvisionStores).** **Correction (verified against the
  tree):** `edgezero-cli` does NOT have its own version — it uses `version = { workspace =
  true }`, and the workspace sets `publish = false` (root `Cargo.toml` `[workspace.package]`).
  So there is **one workspace version** (`0.1.0`) shared by all crates, and nothing publishes
  to crates.io. That changes the accounting: a public break is a **single workspace-version
  bump** (or, if independent per-crate versions are wanted, that split must be done FIRST as
  its own decision) + a CHANGELOG — NOT a per-crate crates.io release. The breaking surface to
  record for that one bump: (a) the `--manifest` arg-type change (`PathBuf` → `Option<PathBuf>`)
  on the arg structs above; (b) any `run_*`/public-fn signature touched by the `ManifestSource`
  seam. Likewise **`edgezero-core`'s removed proxy APIs** (`ProxyService`/`ProxyRequest`/… → the
  (a) the `--manifest` arg-type change (`PathBuf` → `Option<PathBuf>`) on the four arg
  structs above; (b) any `run_*`/public-fn signature touched by the `ManifestSource` seam.
  Likewise **`edgezero-core`'s removed proxy APIs** (`ProxyService`/`ProxyRequest`/… → the
  `*OutboundClient` surface, §6) are a core-crate break. **The full core/adapter breaking
  surface to inventory (each with a version-bump + CHANGELOG + downstream-migration note):**
  (c) **`Body::from_stream` / `Body::into_stream`** — the public `Body::Stream` error type
  changes from `anyhow::Error` to `EdgeError` (§7 `body.rs`), a breaking contract change for
  any caller constructing/consuming a streamed `Body`; (d) the **provider module/client
  renames** (`proxy.rs` → `outbound.rs`, `*ProxyClient` → `*OutboundClient`) across all four
  adapter crates; (e) **Axum's response converter's internal drain change** (`response.rs`) —
  the converter **stays synchronous** (the `LocalBoxStream` is drained inside
  `block_in_place(|| block_on(..))`, NOT an `async fn`, precisely so the `+ Send`
  `Service::Future` still holds — §7 Axum entry). It is a behaviour/impl change, not a
  "synchronous → async signature change" (an earlier note said that; it was wrong and would
  reintroduce the non-Send-future failure). Each affected crate (`edgezero-core`,
  `edgezero-adapter-{axum,cloudflare,fastly,spin}`, `edgezero-cli`) gets its own entry;
  "renames are mechanical" does not exempt them from semver accounting.
- `src/adapter.rs` — wire `ensure_capabilities` as the **first statement** of
  `edgezero_cli::adapter::execute(adapter_name, action, manifest_loader, args)`
  (PR #269), *before* `manifest_command(..)` is consulted and *before* the
  registry lookup. This covers `run_build`, `run_serve`, and `run_deploy` **only** —
  the gate **branches on the action and SKIPS the three `run_auth` sub-actions** (which
  also flow through `execute(..)` but are EXEMPT: credential class). The **three**
  commands that don't flow through `execute(..)` **and are gated** — `run_provision`,
  `run_config_push_typed`, `run_config_validate` — get **sibling pre-dispatch gates**:
  each is the first statement of its function and calls the same `ensure_capabilities`
  helper. Concretely those live at `crates/edgezero-cli/src/provision.rs::run_provision`
  and `crates/edgezero-cli/src/config.rs::{run_config_push_typed, run_config_validate}`
  (the bundled `run_config_push` is a v1 stub that errors — gating it would enforce
  nothing; for validation the gate is a **shared inner op** both `run_config_validate`
  and `run_config_validate_typed` call, since generated CLIs invoke the typed path
  directly). **`run_config_diff_typed` is deliberately NOT gated** — `config diff` is
  read-only and exempt (§3.5.3 command-class gating). The contributor-only `run_demo`
  (`crates/edgezero-cli/src/demo_server.rs`, re-exported via `src/lib.rs`) also calls
  `ensure_capabilities("axum", ..)` at its top before the Axum runner starts. The
  `ensure_capabilities` helper itself is **defined new in `src/adapter.rs`** (alongside
  `execute`) and imported by the sibling gate sites.
  **All five gate sites** (one inside `execute(..)` — branching to gate only
  `build`/`serve`/`deploy` — plus **four siblings** on `run_provision` /
  `run_config_push_typed` / `run_config_validate` / `run_demo`) are documented in
  §3.5.3's gate table. `config diff` and `auth *` are exempt. The legacy `handle_build`
  / `handle_serve` / `handle_deploy` / `handle_dev` functions were removed by PR #269.
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

**`.github/workflows/*.yml`**
- add Tier 3 runtime jobs to `test.yml` (Axum now; Fastly/Cloudflare/Spin as runtimes
  are wired).
- **BLOCKING focused host-observed-cancellation jobs** (§5.3 — these are exempt from the
  general Tier 3 deferral because the `Native` deadline/upload claims rest on them). Each
  must be a concrete, non-vague CI job, not a "future runtime job":
  - **Cloudflare** — job `cf-cancel`. **Exact pins (as EXACT strings, not ranges — the
    implementer confirms the current patch at implementation and writes it verbatim, but the
    job MUST NOT use a caret/`latest`):** `wrangler` and `@cloudflare/vitest-pool-workers`
    pinned in `package-lock.json` (which itself pins the bundled `workerd`), plus `node`
    from `.tool-versions` (24.12.0). **Command:** `npm ci` then `npx vitest run
    cf-cancel.test.ts` using `@cloudflare/vitest-pool-workers` (the supported host-observed
    harness) — **NOT** a bare `wrangler dev` + manual probe. The test starts a **local origin
    fixture** (records whether the inbound subrequest was aborted mid-body), runs the worker so
    a deadline fires mid-stream, and **asserts the origin observed the abort** (connection
    reset / truncated body), not merely that the Rust future was dropped.
  - **Spin** — job `spin-cancel`. **Exact pin:** an EXACT Spin CLI version — `3.7.0` (the
    minimum `wasi:http@0.3.0` baseline; confirm it is the current patch at implementation and
    write it verbatim, **NOT `>= 3.7`**), installed in the job via `curl -sSfL
    https://spinframework.dev/downloads/install.sh | bash -s -- -v v3.7.0`. **NOT
    `.tool-versions`** — that file documents spin-cli is *not* asdf-pinnable (no plugin) and
    is installed manually, so the CI job installs the pinned version itself. **Harness — ONE,
    locked: `spin build` then `spin up` + an EXTERNAL probe** against the origin fixture.
    **Explicitly NOT `spin test`:** `spin test` is a separately-installed **plugin** (not the
    base v3 CLI), and more importantly host-observed cancellation is an **out-of-process**
    property that the in-process test harness cannot assert. The probe drives a component
    whose streamed upload stalls past the deadline and **asserts `[subtask-cancel]` tore the
    request down and left NO spawned pump running** (the component task exits; the origin
    fixture sees the write cancelled).
  - **Shared specifics (all resolved — no remaining TBD):**
    - *Fixture path.* The origin fixture lives at `.github/fixtures/cancel-origin/`, shared
      by both jobs.
    - *Readiness + cleanup.* The fixture prints a ready line the job waits on (bounded
      timeout); both fixture and runtime are torn down in an `always()` step so a hung run
      can't wedge CI.
  - If either job cannot be stood up, the corresponding capability is declared
    `BestEffort`, not `Native` (§5.3) — the job existing (fully specified) is the
    precondition for the `Native` claim, so the workflow entry and the capability value
    ship together.
- **WASM-target config audit — ALL package + root Cargo configs (not just Spin).** The
  `CLAUDE.md` gate quote and the per-adapter wasm-target matrix are correct, but the
  package-local `.cargo/config.toml` files are NOT, and there is more than one stale spot:
  - **`crates/edgezero-adapter-cloudflare/.cargo/config.toml`** hardcodes `target =
    "wasm32-wasip1"` with a **Viceroy** runner — **wrong for Cloudflare**, whose tests build
    for **`wasm32-unknown-unknown`** (CLAUDE.md Compilation-Targets). Fix the target + drop
    the Viceroy runner (Viceroy is Fastly's).
  - The **root `.cargo/config.toml`** header comment and any Spin→`wasm32-wasip1` mention
    (it lists "Wasmtime for spin" under wasip1 — Spin is wasip2 since #269).
  Fastly's `wasm32-wasip1` mentions ARE correct and stay. **§5.5, §7, and §8 must agree:**
  §8 risk 10 is updated (not "closed / all remaining are Fastly's") to point at this audit,
  so no section both schedules and forbids the work.

**`CLAUDE.md`**
- **Spin wasip2 quote refresh: ALREADY DONE — no action.** Gate 5 and the
  Compilation-Targets table already use `wasm32-wasip2` (verified in-tree).
- **Amend the no-network test rule** (§5): today it forbids "tests that require a
  network connection" **unqualified**, which this spec's blocking Axum Tier 3 loopback
  mock origin would violate. Qualify it: "network" means the **public internet**; a
  loopback / `127.0.0.1` mock origin on an ephemeral port is permitted (no external
  connectivity, no credentials). This is a **deliverable of this change**, not an
  assumption.

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
7. **Fastly streamed-upload write-phase has no SDK-configurable bound.**
   Fastly's `between_bytes_timeout` is documented as receive-side only — it
   bounds the gap between bytes received from origin, not the host-side write
   of guest-supplied bytes to origin (Fastly Backend API docs; round 50). No
   published Fastly backend-timeout field bounds the guest-to-origin write
   direction. Streamed-upload write-phase is therefore `BestEffort` on
   Fastly (alongside the source-stream-yield `BestEffort`); the cooperative
   `budget.deadline.is_expired()` check **between** chunks is the only
   adapter-side bound. Apps that need real-time enforcement against a slow
   origin on the write path either pass a buffered request body (`Body::Once`,
   no `StreamingBody` involved) or target a different adapter. If a future
   Fastly platform release adds a documented guest-write timeout, the
   write-phase claim could upgrade to `BoundedCooperative` — track Fastly
   host docs.
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
10. **CLAUDE.md / CI command-quote refresh for Spin SDK 6 + wasip2 — MOSTLY resolved,
    ONE spot remains.** `CLAUDE.md` already quotes
    `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
    (gate 5) and its Compilation-Targets table already lists Spin as
    `wasm32-wasip2` (verified in-tree). **But not ALL remaining `wasm32-wasip1`
    references are Fastly's** — `.cargo/config.toml`'s header comment still associates
    Spin ("Wasmtime for spin") with `wasm32-wasip1` (stale); §5.5/§7 correctly schedule
    that one-line fix. So this risk is **not fully closed** (contradicting an earlier
    "closed / all remaining are Fastly's" note — corrected here); after the config-comment
    fix it is. Retained so this numbered list stays
    stable; **do not schedule this work** — an earlier draft asked for a refresh
    that has since landed.
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
    change. Note this affects only the response-out direction — Spin's outbound
    streamed-*upload* path is unchanged and stays `Native` for
    `streamed-upload-deadlines`.
14. **`outbound-deadlines-exact` (Native-required) capability.** ~~The support-level ladder satisfies a `required outbound-deadlines` with either `Native` or `BoundedCooperative`, so an app cannot demand exact (Native-only) enforcement.~~ **RESOLVED — no longer needed.** Fastly `outbound-deadlines` is now declared `BestEffort` (footnote 1), so **no adapter reports `BoundedCooperative` for `outbound-deadlines`**; a plain `required outbound-deadlines` is already satisfied only by the `Native` adapters (Axum/CF/Spin) and hard-fails on Fastly. The exactness gap the dedicated capability was meant to close is closed by the downgrade itself.
