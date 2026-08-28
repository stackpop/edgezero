# EdgeZero Inbound Request-Body Design

> Extracted from the outbound-HTTP spec (`2026-05-21-outbound-http-design.md`) so that spec stays focused on outbound HTTP. This doc owns the **inbound request-body** contract: `RequestContext` body reading and the `BodyCell` state machine. **It is shared, not purely inbound** — the outbound spec's streaming **proxy-forward** (`OutboundRequest::from_request(ctx.into_request()?, ..)`) depends on the `BodyCell`/`into_request()` contract defined here.

---

#### 3.4.2 Inbound request bodies

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


---

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

  ```rust
  #[derive(Clone)]
  enum StoredError {
      BadRequest         { message: String },
      BadGateway         { message: String },
      GatewayTimeout     { message: String, cause: BudgetCause }, // carry the TYPED cause
      ResponseTooLarge   { message: String }, // outbound over-cap (§3.4.1) — distinct kind
      Validation         { message: String },
      Internal           { rendered: String }, // ALREADY-rendered source; no re-prefixing
      ConfigOutOfDate    { message: String, field_path: String },
      MethodNotAllowed   { method: Method, allowed: String }, // keep the typed `Method`,
 // NOT a String — else
 // reconstruction needs a
 // fallible `Method::from_str`
      NotFound           { path: String },
      NotImplemented     { message: String }, // EdgeError has these too — a
      ServiceUnavailable { message: String }, // capture() claiming to be TOTAL must cover
 // ALL EdgeError variants: the 10 pre-existing PLUS `ResponseTooLarge` = 11 (and
 // `GatewayTimeout`'s `cause` round-trips). Miss one and the exhaustive match won't compile.
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
  diagnostic-only benefit). Adapters needing the full chain log it before it poisons the
  cell. *(A `BodyCell` drain only ever produces `bad_request` / `bad_gateway` /
  `gateway_timeout` / `internal`; the structured variants are still covered so the enum
  is total and `capture` never needs a lossy fallback arm.)*

  **Cancelled drain.** A drain future dropped while `Draining` transitions the cell to
  `Poisoned(StoredError::Internal { rendered: "inbound body drain cancelled".into() })`
  via a drop guard (§5.4), so a cancelled read is indistinguishable in shape from any
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
      pub fn new(request: Request, params: PathParams) -> Self {
          let (parts, body) = request.into_parts();   // split INTERNALLY
          Self { path_params: params, parts, body: BodyCell::initial(body) }
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
already buffered the body.

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
bytes.) §5.4 pins this: permissive read (caches) → stricter `body_bytes` (over-cap error,
cell stays `Cached`) → permissive retry (succeeds) — asserting the stricter failure does
**not** poison an intact cache.

