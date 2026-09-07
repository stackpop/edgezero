# EdgeZero Inbound Request-Body Design

> **Status:** Draft. The ingress-admission and raw-framing requirements in §1.1-§1.3 are
> P0 implementation gates, not optional follow-ups.
>
> Extracted from the
> [outbound-HTTP design](2026-05-21-outbound-http-design.md) so that specification stays
> focused on outbound HTTP. This document owns the **inbound request-body** contract:
> `RequestContext` body reading, adapter ingress, extractor limits, and the `BodyCell`
> state machine. Outbound streaming proxy-forward consumes the resulting
> `BodyCell`/`into_request()` contract but does not own it.

## 1. Scope

The goal is to stop adapters from pre-buffering unbounded inbound bodies while preserving
the existing `FromRequest::from_request(&RequestContext, ..)` extractor signature. A body
is consumed from the platform at most once, bounded helpers cache successful bytes, and a
partially consumed failed drain becomes sticky poison. This document does not define
outbound response caps, outbound deadlines, or provider send behavior.

The total `StoredError` match below targets the end-state `EdgeError` surface and therefore
depends on the outbound error work landing first: `BudgetSource`, `BadGateway`,
`GatewayTimeout`, and `ResponseTooLarge` are defined by the outbound design, not here. If
the body-state migration is split across earlier commits, each commit must still compile
against the variants present at that point. This design additionally owns
`RequestTimeout` (408) for an admitted inbound body whose absolute read deadline expires;
the final `StoredError` match covers every variant then present without a wildcard.

Rust snippets show interfaces and state shapes; omitted bodies are implementation work.
Implementations must pass the workspace's strict Clippy gate: keep items, enum variants,
and methods alphabetically ordered, add `#[inline]` to public methods, and use checked
arithmetic for drain accounting. State-transition order in prose does not dictate enum
declaration order.

### 1.1 Adapter ingress seam and admission

Every adapter entry point has one ingress seam **before routing, middleware, extractor
execution, EdgeZero-owned body buffering, or a `BodyCell::Initial` transition**. The seam
performs this ordered protocol exactly once per platform request:

1. Stamp `request_start` from the adapter's monotonic clock at the earliest EdgeZero-owned
   entry point. It means "EdgeZero received control", not when the client sent its first
   byte and not when route execution began.
2. Validate raw request framing where the platform exposes a raw boundary (§1.2). A framing
   rejection returns 400 and closes/resets that request without invoking app admission or
   polling/draining the body.
3. Construct a normalized `IngressHead` containing the method, target, version, visible
   headers, `IngressFraming`, adapter request metadata, and `request_start`. The framing
   value itself says whether EdgeZero validated the raw boundary or the host owns framing.
4. Invoke the one admission policy stored on `App`. It returns either `Refuse(Response)` or
   `Admit { read_deadline: Deadline }`. The callback is synchronous and body-blind: it can
   inspect the head but cannot obtain or poll the body. A refusal bypasses the router,
   terminates the unread platform body with the strongest target-specific primitive, and
   converts that response normally.
5. On admission, wrap the still-unread platform body with the absolute deadline and native
   cancellation owner, then construct the core `Request` and enter the router. The wrapper
   is installed for `Body::Once` and `Body::Stream`; a content-type path must not pre-buffer
   before this point.

`Hooks::configure(&mut App)` is the existing app-owned configuration point and uses a new
`App` admission-policy setter. Manifest-generated and hand-written apps that do not install
one use the portable default: admit with
`read_deadline = request_start + DEFAULT_INBOUND_READ_BUDGET`, where
`DEFAULT_INBOUND_READ_BUDGET = 30 s`. Admission cannot return "no deadline"; an app that
intentionally needs a longer upload sets a later finite absolute deadline. The decision is
computed per request and is never cached globally.

The adapter retains the complete `App` admission policy when constructing its request
service. Paths that currently clone only `app.router()` must also carry that policy;
`App::into_router()` remains usable for low-level callers but does not silently install or
claim ingress admission. Hand-built adapter services expose the same explicit policy setter
and otherwise use the portable default.

This seam starts when EdgeZero receives control. It cannot reject or time-bound bytes a
provider accepted or buffered before invoking guest code; adapter capability documentation
must state that host-side exposure rather than attributing it to the guest deadline.

### 1.2 Raw request-framing policy

`IngressFraming` distinguishes raw-validated framing from a host-managed request whose raw
boundary EdgeZero cannot inspect:

```rust
#[non_exhaustive]
pub enum IngressFraming {
    Chunked,
    ContentLength(u64),
    HostManaged,
    NoBody,
    ProtocolManaged,
}
```

`NoBody`, `ContentLength`, `Chunked`, and `ProtocolManaged` are produced only after raw
validation and record an unambiguous result. `HostManaged` is required when the adapter's
`raw-ingress-framing-validation` support is `Unsupported`: it makes no assertion that
ambiguous framing was rejected and must not be treated by admission policy as trusted
length/framing evidence. No variant is reconstructed from a normalized `HeaderMap`.
For HTTP/1.x, EdgeZero rejects at the parser/connection boundary, before a body reader is
created, when any of these hold:

- both `Content-Length` and `Transfer-Encoding` are present, in either field order;
- `Content-Length` is malformed, signed, overflows `u64`, appears more than once, or uses a
  comma-list form, including repeated identical values;
- `Transfer-Encoding` is malformed, repeats `chunked`, does not end in exactly one
  `chunked`, or contains a transfer coding the adapter does not implement; or
- framing is otherwise invalid for the request's HTTP version. HTTP/2 and HTTP/3 reject a
  `Transfer-Encoding` field rather than treating it as HTTP/1 framing (`TE: trailers` is a
  separate field and rule).

The rejection response is 400; HTTP/1 closes the connection and multiplexed transports
reset only the affected stream. EdgeZero does not drain an ambiguously framed body and does
not invoke the admission callback. This strict duplicate-`Content-Length` policy deliberately
chooses one parser interpretation instead of accepting RFC-permitted identical duplicates.

Axum is the first required raw-boundary implementation. Hyper 1.10.1 can discard a
`Content-Length` encountered after `Transfer-Encoding`, so checking the resulting
`Request<Body>`/`HeaderMap` is insufficient and must not be described as smuggling
protection. The Axum adapter needs a connection/parser integration that observes field-line
framing before Hyper normalization, plus raw-socket tests in §4. Platform SDKs that expose
only normalized requests cannot claim Native framing validation based on assumed host
behavior; they remain Unsupported until a documented/testable raw rejection seam exists.

### 1.3 Absolute body-read deadline and cancellation

The admitted `read_deadline` is carried with the native body and governs the full inbound
read lifetime: waiting for the first byte, every inter-chunk wait, drain completion, and
late EOF/error. It is not reset by routing, middleware, extractors, repeated cached reads,
or transfer into `OutboundRequest::from_request`. Before each platform read and immediately
after any chunk/EOF/error becomes ready, compare the same absolute deadline. At simultaneous
read completion and expiry, expiry wins.

On expiry, the adapter cancels the pending platform read with its strongest available native
primitive, drops/settles every owned reader, and yields
`EdgeError::RequestTimeout { message }` (408). If `BodyCell` is draining, its existing
capture path transitions exactly once to `Poisoned(StoredError::RequestTimeout { .. })`;
the first and every later accessor observe the same status/kind/message and the source is
never polled again. Expiry after a clean cached drain does not retroactively poison cached
bytes. Dropping a caller future without deadline expiry keeps the separate documented
`Internal("inbound body drain cancelled")` poison.

The public deadline contract is portable; the teardown strength is not. Add three
non-outbound capability cells to the shared capability ladder:

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `ingress-admission` | Native | Native | Native | Native |
| `inbound-read-deadlines` | Native | BestEffort until a deployed cancellation probe passes | BestEffort (synchronous host reads are not guest-preemptible) | BestEffort until host-observed cancellation is bounded |
| `raw-ingress-framing-validation` | Native after the parser-boundary work lands | Unsupported | Unsupported | Unsupported |

BestEffort implementations still use pre-read/post-ready checks and terminate ownership on
expiry; they do not claim a finite bound around an uninterruptible host call. A manifest
that requires Native support fails before startup/deploy through the same capability ladder
used elsewhere. Raw framing and body-read deadline are separate cells: a platform host may
reject malformed framing without exposing evidence, and may expose a cancellable body while
hiding raw field lines.

## 2. Bounded Context Helpers

Wrap the existing `Body::into_bytes_bounded` with context-level helpers:

```rust
// crates/edgezero-core/src/context.rs
impl RequestContext {
 /// Read the inbound request body into `Bytes`, bounded by `max`.
 /// Over-limit yields `Err(EdgeError::bad_request(..))` (400).
 ///
 /// **Takes `&self`** — `RequestContext` carries an internal body cache
 /// (an `unsync::OnceCell<Bytes>` style cell; single-threaded
 /// request, no `tokio` dep). This is deliberate so that existing
 /// `FromRequest` extractors that take `&RequestContext` (e.g. `Json`,
 /// `ValidatedJson`) can call it without a trait-signature breaking
 /// change. The first call drains the underlying `Body::Stream` into
 /// the cell; later calls return a cheap clone. The cached size is
 /// re-validated against `max` on every call, so a later, stricter cap
 /// is still enforced after buffering. The network body is read at most
 /// once.
    #[inline]
    pub async fn body_bytes(&self, max: usize) -> Result<Bytes, EdgeError>;

 /// Call `body_bytes(max)` then deserialize as `application/x-www-form-urlencoded`.
 /// Default cap from extractors: `DEFAULT_INBOUND_FORM_BYTES = 1 MiB`
 /// (forms are typically small). Malformed form data → `bad_request` (400).
 /// Same `&self` cache semantics as `body_bytes`.
    #[inline]
    pub async fn form_within<T: DeserializeOwned>(&self, max: usize)
        -> Result<T, EdgeError>;

 /// Call `body_bytes(max)` then deserialize as JSON. Malformed inbound
 /// JSON yields `Err(EdgeError::bad_request(..))` (a client bug → 400,
 /// in contrast to outbound `OutboundResponse::json` which maps to 502).
 /// Same `&self` cache semantics as `body_bytes`.
    #[inline]
    pub async fn json_within<T: DeserializeOwned>(&self, max: usize)
        -> Result<T, EdgeError>;
}
```

## 3. Context and Adapter Migration

The memory guarantee in §3.1 only holds if the adapter does not pre-buffer the
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
  struct BodyCell(/* unsync */ RefCell<BodyState>);

  /// Non-consuming snapshot of cell state for app inspection.
  pub enum BodyKind {
      Cached { len: usize },
      Draining,
      Initial,
      Poisoned,
      Taken,
  }

  enum BodyState {
      Cached(Bytes),                // body drained successfully
      Draining,                     // body taken out, drain in progress
      Initial(Body),                // never read; the platform body is still owned
      Poisoned(StoredError),         // drain failed (over-cap, stream error, drop)
      Taken,                        // body consumed via take_body / into_request
  }

  pub struct RequestContext {
      body: BodyCell,                // interior-mutable
      parts: http::request::Parts,   // method, uri, version, headers, extensions
      path_params: PathParams,
  }
  ```

  **`StoredError` — why the cell cannot just store an `EdgeError`.** The poison
  contract requires that *every* subsequent access (`body_bytes`, `json_within`,
  `form_within`, `into_request`) returns **the same error**. That needs the error to be
  reproducible — but **`EdgeError` is not `Clone`**: its `Internal` variant wraps
  `anyhow::Error` (`error.rs`: `Internal { #[from] source: AnyError }`), which is
  deliberately not clonable. Storing an `EdgeError` and handing out copies therefore
  does not compile. `StoredError` is the clonable, reconstructable **essence** of the
  error that poisoned the cell:

  It **must be a variant-specific snapshot enum, NOT `{ kind, message }`** — that flat
  shape cannot rebuild `EdgeError` faithfully, on two counts a compiler forces:
  (a) `EdgeError::Internal`'s `message()` already renders as `"internal error: {source}"`,
  so rebuilding via `internal(anyhow!(message))` **doubles the prefix**; (b)
  `ConfigOutOfDate` (`field_path`), `MethodNotAllowed` (`method`, `allowed`), and
  `NotFound` (`path`) carry structured payloads a single `message` string cannot hold. So
  `StoredError` mirrors the variants and captures each payload:

  The list below is schematic until the
  [blob-app-config design](2026-06-16-blob-app-config.md) §6.3 typed-classification gate
  is resolved. If that decision adds a field or variant to
  `EdgeError`, `StoredError` must mirror and round-trip its typed carrier before this
  state machine lands; this document does not freeze a message-only store error shape.

  ```rust
  #[derive(Clone)]
  enum StoredError {
      BadGateway         { message: String, reason: BadGatewayReason },
      BadRequest         { message: String },
      ConfigOutOfDate    { field_path: String, message: String },
      GatewayTimeout     { cause: BudgetSource, message: String }, // preserve typed cause
      Internal           { rendered: String }, // ALREADY-rendered source; no re-prefixing
      MethodNotAllowed   { allowed: String, method: Method }, // no fallible method parse
      NotFound           { path: String },
      NotImplemented     { message: String },
      RequestTimeout     { message: String }, // admitted inbound read deadline expired
      ResponseTooLarge   { message: String, reason: ResponseLimitReason },
      ServiceUnavailable { message: String },
      Validation         { message: String },
  }

  impl StoredError {
      /// Capture an EdgeError's essence at poison time (total match — cannot silently
      /// drop a variant). For `Internal`, store `source.to_string` (already rendered),
      /// NOT `err.message`, so reconstruction does not re-add the "internal error: "
      /// prefix.
      fn capture(err: &EdgeError) -> Self { /* one arm per variant */ }
      /// Rebuild an equivalent `EdgeError` — same variant, same fields, same status.
      /// `Internal { rendered }` → `EdgeError::internal(anyhow!(rendered))`.
      fn to_edge_error(&self) -> EdgeError { /* inverse of capture */ }
  }
  ```

  **Decomposition happens once, at poison time.** The drain's `EdgeError` is captured
  into `StoredError` and the cell returns `stored.to_edge_error()` — so *even the first*
  read gets a reconstructed error, and all later reads are identical. Every accessor's
  signature stays `Result<_, EdgeError>` (no `Rc<EdgeError>` leaking into the public API).

  **Documented loss:** for the `Internal` variant the **`anyhow` source chain and
  backtrace are not preserved** — only the rendered string. A reconstructed `internal`
  error's `inner()` yields a fresh `anyhow::Error` carrying that string, not the original
  chain. Accepted trade: the alternatives are `EdgeError: Clone` (impossible without
  dropping `anyhow`) or `Rc<EdgeError>` on every accessor (an API wart for a
  diagnostic-only benefit). A platform stream that needs the original chain for
  diagnostics must log at the stream-error production boundary, before yielding the typed
  error. *(A `BodyCell` drain only ever produces `bad_request` / `bad_gateway` /
  `gateway_timeout` / `request_timeout` / `internal`; the other structured variants are
  still covered so the enum is total and `capture` never needs a lossy fallback arm.)*

  **Cancelled drain.** A drain future dropped while `Draining` transitions the cell to
  `Poisoned(StoredError::Internal { rendered: "inbound body drain cancelled".into() })`
  via a drop guard (§4), so a cancelled read is indistinguishable in shape from any
  other poison — the next access returns that stored error rather than silently
  re-reading a half-consumed body.

  `RefCell` (unsync) is fine because a `RequestContext` is owned per-request and
  EdgeZero's async traits already use `?Send`. No `tokio` dependency in core.

  **Construction contract — `RequestContext::new(Request, PathParams)` is PRESERVED.**
  `parts` and `body` are **private**, and `BodyCell` / `BodyState` are **not public
  types**. Adapters therefore do **not** — and cannot — construct the context from
  "parts + a body cell"; earlier drafts said they should, which both leaks an internal
  type and misassigns ownership (adapters build a `Request`; the **router** builds the
  `RequestContext`). The existing signature is kept verbatim:

  ```rust
  impl RequestContext {
      #[inline]
      pub fn new(request: Request, params: PathParams) -> Self {
          let (parts, body) = request.into_parts();   // split INTERNALLY
          Self { body: BodyCell::initial(body), parts, path_params: params }
      }
  }
  ```

  So the migration is **source-compatible for every caller of `new(..)`** — adapters
  and the router keep passing a `Request` exactly as they do today, and the
  parts/body split becomes an implementation detail. What adapters *do* change is
  **what they put in that `Request`**: a lazy `Body::Stream` instead of a
  pre-buffered body (first bullet above). `BodyCell` never appears in any public
  signature; the only new public surface is the accessor set (`parts()`,
  `parts_mut()`, `body_kind()`, `body_bytes`, `json_within`, `form_within`,
  `take_body`, `into_request`).

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

  Tested in §4: a scripted stream first returns `Pending`, allowing a second accessor to
  observe `Draining`; the second call returns `internal` without a panic, then the first
  drain is either resumed to success or dropped to exercise cancellation poison.

- **Public methods become coherent with the cache.** Their post-cache behaviour is
  explicit so middleware → handler → proxy-forward chains compose:

  | Method | Behaviour |
  | --- | --- |
  | `method()` / `uri()` / `headers()` / `extensions()` | from `parts` — unaffected by body state |
  | `headers_mut()` / `extensions_mut()` | mutates `parts` — unaffected by body state |
  | `parts() -> &http::request::Parts` / `parts_mut() -> &mut http::request::Parts` | direct access to the underlying `Parts` for middleware that needs the full snapshot; same body-state-irrelevance as the granular accessors above. These are the migration target for call sites currently doing `ctx.request()` / `ctx.request_mut()` (§5 sweep). |
  | `body_kind() -> BodyKind` | a non-consuming snapshot of the cell state — variants enumerated above (`Initial \| Draining \| Cached { len } \| Poisoned \| Taken`). There is **no** `body() -> &Body` / `body() -> Body` accessor — a `&Body` reference cannot span the cell's interior mutability, and a value-returning getter would either consume the stream (single-shot) or require a tee. Callers either buffer via `body_bytes`/`json_within` or consume via `take_body`/`into_request`. |
  | `take_body() -> Result<Body, EdgeError>` | consume the body out of the context: `Initial` → `Ok(Body::Stream(..))`, set state to `Taken`; `Cached(bytes)` → `Ok(Body::Once(bytes))`, set state to `Taken`; `Draining` → `Err(EdgeError::internal("body read in progress"))` (programmer error); `Poisoned(err)` → `Err(err.to_edge_error())`; `Taken` → `Ok(Body::empty())`. After a successful `take_body`, the body cannot be re-read or buffered. |
  | `body_bytes(max)` / `json_within(max)` / `form_within(max)` | from `Initial`: drains → `Cached`, returns clone (or → `Poisoned(err)` on drain failure, then returns that error). From `Cached`: re-validates `max` and returns a clone. From `Poisoned`: returns a fresh `EdgeError` reproduced from the stored error. From `Draining`: `Err(EdgeError::internal("body read in progress"))` — programmer error. From `Taken`: `Err(EdgeError::internal("body already consumed via take_body"))` — buffered helpers cannot resurrect a body that was handed out. |
  | `into_request() -> Result<Request, EdgeError>` | reassembles a `Request` from `parts` + the cell's body via the same rules as `take_body`: `Cached` → `Ok(Body::Once(bytes))`, `Initial` → `Ok(Body::Stream(..))`, `Draining` → `Err(EdgeError::internal("body read in progress"))` (programmer error), `Poisoned(err)` → `Err(err.to_edge_error())` — **not** `Body::empty()`, because a poisoned read silently turning into an empty proxy-forward would violate the "poison is sticky" rule below, `Taken` → `Ok(Body::empty())` (the caller consumed via `take_body`, the empty is intentional). This is what `OutboundRequest::from_request(ctx.into_request()?, uri)?` uses, so streaming proxy-forward still works **even after middleware has buffered the body** (the cached `Bytes` flow through), and a permissive proxy-forward cannot mask a stricter middleware's poisoned read. |

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
already buffered the body. The memory cap is independent of §1.3's time bound: both apply
to the same first drain, so an under-cap slow trickle still expires.

**Sticky poison is scoped to READ/DRAIN failures, NOT cap-rechecks on an already-`Cached`
body — stated to remove an apparent contradiction.** Once the body is `Cached { bytes }`,
a `body_bytes(cap)` call is a **stateless length check** (`bytes.len() <= cap`) that does
**not** mutate the cell: an over-cap result returns an error but leaves the state `Cached`,
so a later `body_bytes(larger_cap)` where the cached length fits **legitimately succeeds**.
This is intended and is **not** a violation of stickiness: stickiness governs a body that
was **consumed/failed while draining** (`Initial → Draining → Poisoned`) — there the cell
is poisoned and every subsequent access (any cap) returns the stored error. The rule
"a permissive caller cannot bypass a stricter one" is about a **poisoning drain**, not
about re-reading an intact cache at different caps. (The security property still holds:
the *first* reader that actually drains sets the cache/poison; a cap check against an
existing cache reveals nothing a caller couldn't compute from the already-materialized
bytes.) §4 pins this: permissive read (caches) → stricter `body_bytes` (over-cap error,
cell stays `Cached`) → permissive retry (succeeds) — asserting the stricter failure does
**not** poison an intact cache.

### 3.1 Memory Bound

*(Moved here from the outbound spec's §3.4.4 batch-memory model — this is the inbound-body
half; the outbound spec keeps only the per-response and batch terms.)*

- **Per-inbound-body.** *Persistent* memory — the cached `Bytes` after a successful drain —
  is bounded by the `max` passed to `body_bytes(max)` / `json_within(max)` /
  `form_within(max)`. *Transient* worst-case during the drain is the same shape:
  `max + sizeof(current_chunk)`, with the in-flight chunk source-controlled. Outbound's
  `OutboundResponse::into_bytes_bounded` mirrors this same accounting.

### 3.2 Extractor Migration

*(Moved here from the outbound spec's §7 file-by-file migration — this is inbound-only.)*

- `src/extractor.rs` — extractor migration: `Json<T>` / `ValidatedJson<T>` route through
  `ctx.json_within(DEFAULT_INBOUND_JSON_BYTES)`; `Form<T>` / `ValidatedForm<T>` route
  through `ctx.form_within(DEFAULT_INBOUND_FORM_BYTES)`; add `ValidatedJsonWithin<T, MAX>`
  and `ValidatedFormWithin<T, MAX>` for explicit caps. Constants exposed:
  `pub const DEFAULT_INBOUND_JSON_BYTES: usize = 8 * 1024 * 1024;` and
  `pub const DEFAULT_INBOUND_FORM_BYTES: usize = 1024 * 1024;`.

## 4. Test Plan

Core tests use scripted local streams and `futures::executor::block_on`; they require no
platform runtime or network.

| Surface | Required assertions |
| --- | --- |
| Bounded drain | `Body::Once` and multi-chunk streams succeed at and below the cap; the first byte above the cap returns `bad_request` without unchecked accounting overflow. |
| Successful cache | The platform stream is polled only once; repeated reads clone the same bytes. A permissive read followed by a stricter cached read returns an over-cap error while leaving the cell `Cached`; a later permissive read succeeds. |
| Failed drain | Source error and initial-drain cap overflow transition to `Poisoned`; every later buffered accessor, `take_body`, and `into_request` reconstructs the same variant, status, message, and structured fields. The source is never polled again. |
| Admission ordering | The callback runs exactly once after raw validation where available, or with explicit `IngressFraming::HostManaged` otherwise, and before routing/body polling. Refusal polls no body, invokes no route/middleware, terminates the native reader, and preserves the chosen response. The default policy supplies one finite deadline derived from `request_start`. |
| Absolute read deadline | A 1-byte-per-step under-cap stream cannot extend its lifetime: first-byte, inter-chunk, EOF, and source-error races use one absolute deadline; expiry wins simultaneous readiness, cancels native ownership, and poisons a draining cell as `request_timeout` (408). Cached success is not retroactively poisoned. |
| Cancellation | A stream held at `Pending` leaves the cell `Draining`; dropping the first `body_bytes` future transitions it to the documented cancellation poison, and the next access returns that stored internal error. |
| Reentrancy | While the first scripted drain is pending, a second body accessor returns `internal("body read already in progress")` without a `RefCell` panic. Resuming the first future can still complete and cache bytes. |
| Consumption | `take_body` and `into_request` cover Initial, Cached, Draining, Poisoned, and Taken. Initial preserves a stream; Cached reassembles `Body::Once`; Taken deliberately produces an empty body only where specified. |
| Extraction | JSON/form success, malformed input, default caps, explicit `Within` caps, and validator failures preserve their documented 400/validation behavior. Multiple extractors share the one cache. |
| Stored errors | `StoredError::capture` exhaustively covers all end-state `EdgeError` variants. Round-trips preserve variant fields, including `BadGatewayReason`, `BudgetSource`, and `ResponseLimitReason`; `RequestTimeout` remains 408; `Internal` has the one documented source-chain loss without duplicating the `internal error:` prefix. |
| Raw HTTP/1 framing | Axum raw-socket cases cover CL+TE in both orders, duplicate equal/unequal CL, comma-list CL, overflow/signed/malformed CL, repeated/non-final/unsupported TE, valid single CL, and valid terminal chunked framing. Rejections return 400, close the connection, invoke no admission callback, and poll no body. A `HeaderMap`-only unit test is insufficient. |

Each adapter contract test supplies a body stream whose first poll is observable and
asserts that raw validation, when supported, and admission complete before that poll.
Axum passes only validated framing variants; Cloudflare, Fastly, and Spin pass
`IngressFraming::HostManaged`, and their tests prove admission cannot accidentally upgrade
that value to a validated claim from normalized headers. The same adapter test
then passes the converted core request through `RequestContext`, buffers it as middleware
would, and reassembles it through `into_request`; the outbound-facing request receives the
cached bytes unchanged and retains the original absolute read deadline. Tests must not
substitute an already-buffered body for this lazy ingress assertion. Cloudflare and Spin
need deployed/host-observed cancellation probes before upgrading their BestEffort cells;
Fastly tests cooperative checks without asserting preemption of a synchronous host read.

## 5. File-by-File Change Summary

- `crates/edgezero-core/src/context.rs`: split requests internally into private parts plus
  `BodyCell`; add the state machine, bounded helpers, body-state accessors, `take_body`, and
  fallible `into_request`; remove whole-request borrow accessors.
- `crates/edgezero-core/src/app.rs`: add the synchronous, body-blind admission policy to
  `App`, its default finite inbound read budget, and the normalized ingress-head/decision
  types. The macro continues to use `Hooks::configure(&mut App)` as the app-owned setter
  point.
- `crates/edgezero-core/src/error.rs`: add `RequestTimeout { message }` with status 408 and
  kind `request_timeout`; update every exhaustive match and wire-shape matrix.
- `crates/edgezero-core/src/manifest.rs` and `crates/edgezero-adapter/src/registry.rs`: add
  the three inbound capability cells and fail-closed required-capability handling. Keep
  these counts separate from the outbound design's seven-cell tuple.
- `crates/edgezero-core/src/extractor.rs`: route JSON/form extractors through bounded
  helpers, add explicit-cap variants, and add the two public default constants.
- `crates/edgezero-core/src/body.rs`: retain the core bounded-drain primitive used by the
  context and use checked pre-append accounting if it has not already landed through the
  outbound body work.
- `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}` request/dispatch services: stamp
  `request_start`, run framing validation and app admission before routing/body polling,
  stop eager collection, and wrap each platform request body with its absolute deadline and
  native cancellation owner. Keep `Body::Once` only when the platform already owns bounded
  bytes and admission has already run.
- `crates/edgezero-adapter-axum` server connection path: validate raw HTTP/1 field lines
  before Hyper can discard framing evidence; reject/close on ambiguity. The normalized
  `Request<Body>` conversion is not the enforcement point.
- `crates/edgezero-core` call sites and tests: migrate `request()` / `request_mut()` users
  to parts or body-specific accessors and update the now-fallible `into_request()` calls.
- Adapter contract and host tests: prove admission ordering, one absolute deadline,
  target-specific cancellation strength, and the §1.2 raw-framing behavior/capability.
  No outbound send or provider error-classification behavior is owned by this
  specification.
