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

Every adapter entry point has one ingress seam **before middleware, extractor execution,
EdgeZero-owned body buffering, or a `BodyCell::Initial` transition**. Route resolution is
the one deliberate exception: the router resolves once before admission so policy sees the
canonical route identity, but it does not dispatch middleware or a handler. The seam
performs this ordered protocol exactly once per platform request:

1. Stamp `request_start` from the adapter's monotonic clock at the earliest EdgeZero-owned
   entry point. It means "EdgeZero received control", not when the client sent its first
   byte and not when route execution began.
2. Enforce parser-level request-target and header limits, then validate raw request framing,
   where the platform exposes a raw boundary (§1.2). Target overflow returns 414, header
   byte/count overflow returns 431, and framing rejection returns 400. Every rejection
   closes/resets that request without route resolution, app admission, or body polling.
3. Convert only request-head data, retain the unread native body separately, and call
   `RouterService::resolve(method, path)` once. Resolution returns an opaque dispatch token
   plus `RouteResolution::{Matched, MethodNotAllowed, NotFound}`. It performs no middleware,
   handler, body poll, or request-extension injection. The token owns the exact match and
   path parameters so admitted dispatch does not rerun matching.
4. Construct a normalized `IngressHead` containing the method, target, version, visible
   headers, `IngressHeadAccounting`, `IngressFraming`, adapter request metadata, public
   `request_start`, and the stable route resolution. The accounting and framing values say
   whether EdgeZero validated the raw boundary or the host owns those decisions.
5. Invoke the one admission policy stored on `App`. It returns either `Refuse(Response)` or
   `Admit { grant: IngressGrant, read_deadline: Deadline }`. The callback is synchronous
   and body-blind: it can inspect the head but cannot obtain or poll the body. A refusal
   bypasses resolved dispatch, terminates the unread platform body with the strongest
   target-specific primitive, drops any policy-local resources, and converts that response
   normally.
6. On admission, wrap the still-unread platform body with the absolute deadline and native
   cancellation owner, construct the core `Request`, and call
   `RouterService::dispatch_resolved` with the exact token from step 3. For `Matched`, the
   router constructs `RequestContext` with `request_start`, route metadata, and the
   request-owned grant before middleware. For `MethodNotAllowed`/`NotFound`, normal 405/404
   handling runs without middleware or a handler and the grant is dropped exactly once.
   The wrapper is installed for `Body::Once` and `Body::Stream`; a content-type path must
   not pre-buffer before this point.

Route identity is stable and structural, never a registration ordinal or randomized hash:

```rust
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RouteId { /* private canonical method + registered pattern */ }

impl RouteId {
    pub fn method(&self) -> &Method;
    pub fn pattern(&self) -> &str;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteMetadata { /* private RouteId; no duplicate mutable identity fields */ }

impl RouteMetadata {
    pub fn id(&self) -> &RouteId;
    pub fn method(&self) -> &Method;
    pub fn pattern(&self) -> &str;
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RouteResolution {
    Matched(RouteMetadata),
    MethodNotAllowed { allowed: Arc<[RouteMetadata]> },
    NotFound,
}
```

`RouteId` equality/hash is the canonical `(method, registered route pattern)` pair. Dynamic
path parameter values never enter it. `RouteMetadata` owns exactly that `RouteId`; its
method/pattern accessors delegate to the id, so independently mutable duplicate fields
cannot disagree. `MethodNotAllowed.allowed` contains every matched
pattern/method candidate sorted by method then pattern; callers derive an Allow set without
depending on `HashMap` order. `RouteInfo` reuses `RouteMetadata` rather than defining a
second identity. Duplicate method/pattern registration remains the existing build-time
error.

The resolution/dispatch split is an explicit router API, not an adapter-side reimplementation
of matching:

```rust
pub struct ResolvedDispatch { /* private router identity + path parameters */ }

impl ResolvedDispatch {
    pub fn resolution(&self) -> &RouteResolution;
}

impl RouterService {
    pub fn resolve(&self, method: &Method, path: &str) -> ResolvedDispatch;

    pub async fn dispatch_resolved(
        &self,
        resolved: ResolvedDispatch,
        request: Request,
        ingress: AdmittedIngress,
    ) -> Result<Response, EdgeError>;
}
```

`ResolvedDispatch` is opaque, single-use, and bound to the `RouterService` instance that
created it. `dispatch_resolved` rejects a token from another router as `Internal`, consumes
the token, and never matches the method/path again. The `Request` method and path must equal
the values captured by the token; a mismatch is also `Internal`. This prevents middleware or
adapter conversion from admitting one route and dispatching another. `AdmittedIngress`
contains the captured start, finite deadline, and grant; it has no public constructor so only
the ingress protocol can claim admission occurred.

`IngressGrant` is an opaque request-owned lease carrier. It is intentionally not `Clone`:

```rust
pub struct IngressGrant { /* Option<Box<dyn Any + Send + Sync>> */ }

impl IngressGrant {
    pub fn downcast<T: Send + Sync + 'static>(self) -> Result<T, Self>;
    pub fn empty() -> Self;
    pub fn new<T: Send + Sync + 'static>(value: T) -> Self;
}

#[non_exhaustive]
pub enum AdmissionDecision {
    Admit {
        grant: IngressGrant,
        read_deadline: Deadline,
    },
    Refuse(Response),
}
```

The matched `RequestContext` stores the grant in an unsynchronized one-shot cell because
extractors receive `&RequestContext`. `take_ingress_grant(&self)` removes and returns it at
most once; a second call returns `None`. If application code never takes it, context drop
releases it. A refusal never creates a request-owned grant. An admitted 404/405 owns the
grant only until the resolved terminal response, then drops it. The carrier exposes no
type-id string, serializer, or platform handle API; application code knows the concrete
lease type it inserted.

`IngressHead::request_start()` and `RequestContext::request_start()` return the captured
`MonotonicInstant`. Both are the same value and clock domain used by `Deadline`; neither
accessor snapshots a new instant. `IngressHead::route_resolution()` exposes the value above,
and a matched `RequestContext::route_metadata()` returns that exact matched metadata.

`Hooks::configure(&mut App)` is the existing app-owned configuration point and uses a new
`App` admission-policy setter. Manifest-generated and hand-written apps that do not install
one use the portable default: admit with `grant = IngressGrant::empty()` and
`read_deadline = request_start + DEFAULT_INBOUND_READ_BUDGET`, where
`DEFAULT_INBOUND_READ_BUDGET = 30 s`. Admission cannot return "no deadline". The adapter
normalizes every admitted deadline to no later than
`request_start + DEADLINE_FAR_FUTURE` using checked addition; overflow fails closed as an
internal policy error. An app that needs a longer upload than the default sets a later
absolute deadline within that bound. The decision is computed per request and is never
cached globally.

The adapter retains the complete `App` admission policy when constructing its request
service. Paths that currently clone only `app.router()` must also carry that policy;
`App::into_router()` remains usable for low-level callers but does not silently install or
claim ingress admission. Hand-built adapter services expose the same explicit policy setter
and otherwise use the portable default.

Parser limits are startup policy, not admission output: the parser must know them before it
can safely materialize `IngressHead`. `App` therefore also owns immutable
`IngressHeadLimits`, copied into the adapter service during finalization. The default is
finite on every target:

```rust
pub const DEFAULT_MAX_REQUEST_HEADER_BYTES: u64 = 65_536;
pub const DEFAULT_MAX_REQUEST_HEADER_COUNT: u64 = 100;
pub const DEFAULT_MAX_REQUEST_TARGET_BYTES: u64 = 8_192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct IngressHeadLimits {
    max_request_header_bytes: u64,
    max_request_header_count: u64,
    max_request_target_bytes: u64,
}

impl IngressHeadLimits {
    pub fn max_request_header_bytes(&self) -> u64;
    pub fn max_request_header_count(&self) -> u64;
    pub fn max_request_target_bytes(&self) -> u64;
    pub fn with_max_request_header_bytes(self, max: u64) -> Result<Self, EdgeError>;
    pub fn with_max_request_header_count(self, max: u64) -> Result<Self, EdgeError>;
    pub fn with_max_request_target_bytes(self, max: u64) -> Result<Self, EdgeError>;
}
```

All three values must be nonzero. Validation runs when `App` is finalized and before a
listener/guest service starts. Invalid configuration is `Internal`; it never silently means
unlimited. Limits are inclusive and use checked `u64` accounting.

This seam starts when EdgeZero receives control. It cannot reject or time-bound bytes a
provider accepted or buffered before invoking guest code; adapter capability documentation
must state that host-side exposure rather than attributing it to the guest deadline.

### 1.2 Raw request-head limits and framing policy

At a raw parser boundary, request-target bytes are the exact octets between the first and
second spaces of the HTTP/1 request line, before URI normalization. Header bytes are the raw
field-section octets from the first field-name through the terminating empty line, including
colons, optional whitespace, line endings, and the final line ending. Header count is the
number of field lines before duplicate coalescing. The parser rejects on the first byte or
field above the configured value; it does not read an unbounded head and check afterward.
For multiplexed protocols, equivalent accounting is performed on the encoded field section
only when the adapter exposes a bounded pre-materialization hook.

The raw parser returns `EdgeError::UriTooLong { message }` (414) for target overflow and
`EdgeError::RequestHeaderFieldsTooLarge { message }` (431) for header byte/count overflow.
Neither diagnostic includes target or header contents. These failures happen before route
resolution and therefore never enter `StoredError` through a body drain, but those variants
are still included in its total match. Malformed request syntax remains `BadRequest` (400).

Admission can inspect whether the configured parser contract actually ran:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum IngressHeadAccounting {
    HostManaged,
    RawValidated {
        request_header_bytes: u64,
        request_header_count: u64,
        request_target_bytes: u64,
    },
}
```

`IngressHead::head_accounting()` returns this value. `RawValidated` is emitted only when all
three values were measured before normalized request construction and found within the exact
`IngressHeadLimits` installed for that service. A normalized `HeaderMap` recount may reject
visible data as defense in depth but must report `HostManaged`; it cannot satisfy the raw
capability or recover field-line/request-target octets discarded by the platform.

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
protection. The Axum adapter needs an audited pinned Hyper parser patch or an upstream parser
hook that rejects the strict policy while Hyper still has ordered `httparse` field lines.
After that parser has rejected ambiguity, the adapter may derive `IngressFraming` from the
surviving normalized headers plus HTTP version; the derivation is trusted only because the
raw parser decision preceded it. A second independent socket pre-parser is not accepted: it
can disagree with Hyper and cannot safely locate subsequent pipelined request heads without
also owning HTTP body framing. Raw-socket tests in §4 pin the parser behavior. Platform SDKs that expose
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

The public deadline contract is portable; the teardown strength is not. Add four
non-outbound capability cells to the shared capability ladder:

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `ingress-admission` | Native | Native | Native | Native |
| `inbound-read-deadlines` | Native | BestEffort until a deployed cancellation probe passes | BestEffort (synchronous host reads are not guest-preemptible) | BestEffort until host-observed cancellation is bounded |
| `raw-ingress-head-limits` | Native after the parser-boundary work lands | Unsupported | Unsupported | Unsupported |
| `raw-ingress-framing-validation` | Native after the parser-boundary work lands | Unsupported | Unsupported | Unsupported |

BestEffort implementations still use pre-read/post-ready checks and terminate ownership on
expiry; they do not claim a finite bound around an uninterruptible host call. A manifest
that requires Native support fails before startup/deploy through the same capability ladder
used elsewhere. Raw head limits, raw framing, and body-read deadline are separate cells: a
platform host may impose undocumented limits or reject malformed framing without exposing
evidence, and may expose a cancellable body while hiding raw target/field-line bytes.

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

  The [blob-app-config design](2026-06-16-blob-app-config.md) §6.3 classification is
  finalized. `StoredError` mirrors its reason-bearing `StoreExtraction` variant and must
  continue to mirror every future `EdgeError` payload before this state machine lands.

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
      StoreExtraction    {
          field_path: Option<String>,
          message: String,
          reason: StoreExtractionReason,
      },
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
          Self::new_low_level(request, params, MonotonicInstant::now())
      }

      #[inline]
      pub fn request_start(&self) -> MonotonicInstant;

      #[inline]
      pub fn route_metadata(&self) -> Option<&RouteMetadata>;

      #[inline]
      pub fn take_ingress_grant(&self) -> Option<IngressGrant>;
  }
  ```

  So the migration is **source-compatible for every caller of `new(..)`** — adapters
  and the router keep passing a `Request` exactly as they do today, and the
  parts/body split becomes an implementation detail. For this low-level constructor,
  `request_start` is snapped when `new` is called, `route_metadata()` returns `None`, and
  `take_ingress_grant()` returns `None`. That path makes
  no `ingress-admission` claim. Resolved router dispatch uses a crate-private constructor
  that consumes `AdmittedIngress` and installs its original start, matched metadata, and
  grant before middleware. What adapters *do* change is
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
  `max + current_chunk.len()`, with the in-flight chunk source-controlled. Outbound's
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
| Parser-level head limits | Exact-limit request targets, header bytes, and header counts pass. One byte/field over rejects before route resolution, admission, request construction, or body polling with 414/431 and redacted diagnostics. Raw accounting includes duplicate field lines and framing syntax; checked overflow fails closed. Host-normalized recounts never report `RawValidated`. |
| Admission ordering and route identity | Raw validation runs first where available. Route resolution then runs exactly once without middleware or body polling, and the callback sees the resulting stable `RouteResolution`. Admission runs exactly once before resolved dispatch/body polling. A matched dispatch uses the admitted route and path parameters without rematching; method/path mutation or a foreign token fails closed. Refusal polls no body, invokes no middleware/handler, terminates the native reader, and preserves the chosen response. The default policy supplies an empty grant and one finite deadline derived from `request_start`. |
| Request ingress metadata | `IngressHead` and matched `RequestContext` expose the same captured `request_start` and route metadata. `take_ingress_grant()` returns the non-clone grant exactly once, then `None`; untaken, refused, 404, and 405 grants each drop exactly once. The preserved low-level context constructor exposes no route and makes no admission claim. |
| Absolute read deadline | A 1-byte-per-step under-cap stream cannot extend its lifetime: first-byte, inter-chunk, EOF, and source-error races use one absolute deadline; expiry wins simultaneous readiness, cancels native ownership, and poisons a draining cell as `request_timeout` (408). Cached success is not retroactively poisoned. |
| Cancellation | A stream held at `Pending` leaves the cell `Draining`; dropping the first `body_bytes` future transitions it to the documented cancellation poison, and the next access returns that stored internal error. |
| Reentrancy | While the first scripted drain is pending, a second body accessor returns `internal("body read already in progress")` without a `RefCell` panic. Resuming the first future can still complete and cache bytes. |
| Consumption | `take_body` and `into_request` cover Initial, Cached, Draining, Poisoned, and Taken. Initial preserves a stream; Cached reassembles `Body::Once`; Taken deliberately produces an empty body only where specified. |
| Extraction | JSON/form success, malformed input, default caps, explicit `Within` caps, and validator failures preserve their documented 400/validation behavior. Multiple extractors share the one cache. |
| Stored errors | `StoredError::capture` exhaustively covers all end-state `EdgeError` variants. Round-trips preserve variant fields, including `BadGatewayReason`, `BudgetSource`, `ResponseLimitReason`, and `StoreExtractionReason` plus its optional field path; `RequestTimeout` remains 408; `Internal` has the one documented source-chain loss without duplicating the `internal error:` prefix. |
| Raw HTTP/1 framing | Axum raw-socket cases cover CL+TE in both orders, duplicate equal/unequal CL, comma-list CL, overflow/signed/malformed CL, repeated/non-final/unsupported TE, valid single CL, and valid terminal chunked framing. Rejections return 400, close the connection, invoke no admission callback, and poll no body. A `HeaderMap`-only unit test is insufficient. |

Each adapter contract test supplies a body stream whose first poll is observable and
asserts that raw validation, when supported, route resolution, and admission complete before
that poll, while middleware and handler dispatch do not begin until after admission.
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
  fallible `into_request`; store the captured request start, optional route metadata, and
  one-shot ingress grant; remove whole-request borrow accessors.
- `crates/edgezero-core/src/app.rs`: add the synchronous, body-blind admission policy and
  immutable `IngressHeadLimits` to `App`, its default finite inbound read budget, and the
  normalized ingress-head/decision/accounting types. The macro continues to use
  `Hooks::configure(&mut App)` as the app-owned setter point.
- `crates/edgezero-core/src/error.rs`: add `RequestHeaderFieldsTooLarge { message }` with
  status 431 and kind `request_header_fields_too_large`, `RequestTimeout { message }` with
  status 408 and kind `request_timeout`, and `UriTooLong { message }` with status 414 and
  kind `uri_too_long`; update every exhaustive match and wire-shape matrix.
- `crates/edgezero-core/src/manifest.rs` and `crates/edgezero-adapter/src/registry.rs`: add
  the four inbound capability cells and fail-closed required-capability handling. Keep
  these counts separate from the outbound design's eight-cell tuple.
- `crates/edgezero-core/src/extractor.rs`: route JSON/form extractors through bounded
  helpers, add explicit-cap variants, and add the two public default constants.
- `crates/edgezero-core/src/body.rs`: retain the core bounded-drain primitive used by the
  context and use checked pre-append accounting if it has not already landed through the
  outbound body work.
- `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}` request/dispatch services: stamp
  `request_start`, run framing validation, resolve once, and run app admission before
  middleware/handler dispatch or body polling; stop eager collection; dispatch with the
  opaque resolved token; and wrap each platform request body with its absolute deadline and
  native cancellation owner. Keep `Body::Once` only when the platform already owns bounded
  bytes and admission has already run.
- `crates/edgezero-adapter-axum` server connection path: enforce raw request-target/header
  limits and validate raw HTTP/1 field lines before Hyper can discard framing evidence;
  reject/close on overflow or ambiguity. The normalized
  `Request<Body>` conversion is not the enforcement point.
- `crates/edgezero-core` call sites and tests: migrate `request()` / `request_mut()` users
  to parts or body-specific accessors and update the now-fallible `into_request()` calls.
- Adapter contract and host tests: prove admission ordering, one absolute deadline,
  target-specific cancellation strength, and the §1.2 raw-framing behavior/capability.
  No outbound send or provider error-classification behavior is owned by this
  specification.
