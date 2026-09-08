# Outbound HTTP Phase 1b: Core Types and Budget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the runtime-independent outbound request, response, client, URL, and dispatch-budget value layer specified in §§3.1-3.3 of the outbound design.

**Architecture:** Build the new `edgezero_core::outbound` module alongside the legacy `proxy` module so the repository compiles while adapters migrate in Phases 4-6. This coexistence is branch-only scaffolding, not a compatibility promise or a releasable state. Phase 7 deletes `proxy` and proves that no public alias remains. Core canonicalizes a URL once, owns all policy values, and exposes only provider-neutral data to adapters.

**Tech Stack:** Rust 1.95, `http`, `bytes`, `serde_json`, `url = "=2.5.8"`, `web-time`, `async-trait`, `futures`.

---

## Preconditions and Merge Rule

- [ ] Confirm Phase 1a tests pass: `cargo test --offline --locked -p edgezero-core --lib`.
- [ ] Read spec §§3.1-3.3, §5.1, §6, and the core entries in §7.
- [ ] Do not publish or merge the series until Phase 7 removes `edgezero_core::proxy`, `ProxyService`, and every legacy public name. Do not add aliases between old and new names.
- [ ] Keep `BudgetInputs` private and limited to `deadline` and `timeout`; response size does not participate in budget selection.

## Task Protocol

For every task below: add only the named tests first; run the exact focused command and require a nonzero failure caused by the missing behavior; implement the smallest listed surface; rerun the same command to zero failures; run `cargo test --offline --locked -p edgezero-core --lib` and `git diff --check`; then stage only the task's files and make the stated commit. A compile error for a not-yet-added public item is an acceptable red result. An already-passing new test is not. The contract declarations below are in the repository's required alphabetical item order; keep every actual module item, struct field, enum variant, and impl method in that order, and add `#[inline]` to every public function as required by the denied workspace lints.

**Required exact test names:** `outbound_request_defaults_and_parts_round_trip`, `outbound_request_canonicalizes_url_table`, `outbound_request_rejects_invalid_target_table`, `outbound_request_rejects_empty_userinfo`, `outbound_request_rejects_backslash_authority_forms`, `outbound_request_preserves_percent_encoded_hash`, `outbound_request_from_request_normalizes_immediately`, `dispatch_validation_precedence_table`, `request_normalization_strips_connection_nominations`, `request_normalization_is_idempotent`, `dispatch_budget_selects_and_attributes_minimum`, `dispatch_budget_rejects_expired_or_zero`, `http_client_delegates_send_and_send_all`, and `outbound_response_parts_preserve_method_headers_and_body`.

**Expected red:** Task 2 initially fails to resolve `edgezero_core::outbound`; Task 3 initially observes nominated/hop-by-hop headers or accepts an invalid request; Task 4 fails to resolve `dispatch_budget`; Task 5 fails to resolve `HttpClient`/`OutboundResponse`. Dependency Task 1 is a lock/dependency precondition and is green when the exact tree assertion passes.

### Task 1: Pin the canonical URL parser

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/edgezero-core/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.lock`

- [ ] Add `url = "=2.5.8"` to workspace dependencies and `url = { workspace = true }` to core.
- [ ] Refresh both independent lock graphs with `cargo check --offline -p edgezero-core` and `cargo check --offline --manifest-path examples/app-demo/Cargo.toml -p app-demo-core`; these commands are intentionally unlocked in this dependency task.
- [ ] Run `cargo tree --offline --locked -p edgezero-core -e normal | rg 'url v2\.5\.8'`; expect exactly the pinned normal dependency.
- [ ] Run `cargo tree --offline --locked --manifest-path examples/app-demo/Cargo.toml -p app-demo-core -e normal | rg 'url v2\.5\.8'`; expect the same exact version in the excluded demo graph.
- [ ] Commit: `build(core): pin outbound URL parser`.

### Task 2: Add canonical outbound requests

**Files:**
- Create: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

**Contract:**

```rust
pub struct OutboundRequest {
    body: Body,
    deadline: Option<Deadline>,
    headers: HeaderMap,
    max_brotli_window_bits: u8,
    max_chunk_bytes: Option<NonZeroU64>,
    max_encoded_response_bytes: Option<u64>,
    max_request_body_bytes: u64,
    max_response_header_bytes: Option<u64>,
    max_response_header_count: Option<u64>,
    method: Method,
    response_mode: ResponseMode,
    timeout: Option<Duration>,
    uri: Uri,
}

pub struct OutboundRequestParts {
    pub body: Body,
    pub deadline: Option<Deadline>,
    pub headers: HeaderMap,
    pub max_brotli_window_bits: u8,
    pub max_chunk_bytes: Option<NonZeroU64>,
    pub max_encoded_response_bytes: Option<u64>,
    pub max_request_body_bytes: u64,
    pub max_response_header_bytes: Option<u64>,
    pub max_response_header_count: Option<u64>,
    pub method: Method,
    pub response_mode: ResponseMode,
    pub timeout: Option<Duration>,
    pub uri: Uri,
}

pub enum ResponseMode { Buffered { max_bytes: u64 }, Streamed }

impl OutboundRequest {
    pub fn body(self, body: impl Into<Body>) -> Self;
    pub fn deadline(self, deadline: Deadline) -> Self;
    pub fn from_parts(parts: OutboundRequestParts) -> Result<Self, EdgeError>;
    pub fn from_request(request: Request, target: Uri) -> Result<Self, EdgeError>;
    pub fn get(uri: impl AsRef<str>) -> Result<Self, EdgeError>;
    pub fn header<N: AsRef<[u8]>, V: AsRef<[u8]>>(
        self,
        name: N,
        value: V,
    ) -> Result<Self, EdgeError>;
    pub fn headers(&self) -> &HeaderMap;
    pub fn headers_mut(&mut self) -> &mut HeaderMap;
    pub fn into_parts(self) -> OutboundRequestParts;
    pub fn json<T: Serialize>(self, value: &T) -> Result<Self, EdgeError>;
    pub fn max_brotli_window_bits(self, bits: u8) -> Self;
    pub fn max_chunk_bytes(self, bytes: NonZeroU64) -> Self;
    pub fn max_encoded_response_bytes(self, bytes: u64) -> Self;
    pub fn max_request_body_bytes(self, bytes: u64) -> Self;
    pub fn max_response_bytes(self, bytes: u64) -> Self;
    pub fn max_response_header_bytes(self, bytes: u64) -> Self;
    pub fn max_response_header_count(self, count: u64) -> Self;
    pub fn method(&self) -> &Method;
    pub fn new(method: Method, uri: Uri) -> Result<Self, EdgeError>;
    pub fn post(uri: impl AsRef<str>) -> Result<Self, EdgeError>;
    pub fn stream_response(self) -> Self;
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn uri(&self) -> &Uri;
}
```

- [ ] Add failing `outbound_request_*` tests for defaults, every builder/accessor, `into_parts`/`from_parts`, JSON content type, and header byte validation. `from_request` must preserve the exact method/body while immediately stripping hop-by-hop, nominated, `Host`, `Content-Length`, and `Transfer-Encoding` headers.
- [ ] Add URL table tests for scheme/host case, default ports, DNS, IPv4, bracketed IPv6, path/query WHATWG normalization, and exact canonical serialization.
- [ ] Add rejection rows for non-HTTP schemes, relative/no-authority targets, nonempty and empty userinfo, raw `#` fragments, every raw-backslash special-URL separator form (`//`, `\\`, `/\\`, `\\/` around an empty-userinfo authority), and malformed UTF-8/header bytes. Add positive rows proving percent-encoded `%23` and `%40` remain path/query data rather than being rejected as fragment/userinfo delimiters.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib outbound::tests::outbound_request_`; expect compile/test failure.
- [ ] Implement the types and builders from spec §3.1. Store the canonical `Uri`; adapters must not parse the caller's source string again.
- [ ] Implement raw-string construction in this order: reject a literal `#`; reject every raw `\\` byte before WHATWG parsing; isolate the raw authority between literal `://` and the first `/`, `?`, or `#` and reject any literal `@` (including `https://@example.com/`); `Url::parse`; reject scheme/authority/remaining userinfo violations; clear default ports; serialize once; parse that serialization into `http::Uri`. Rejecting backslashes is the exact pre-parser rule because WHATWG special URLs normalize them as separators; do not add a second ad hoc authority parser. `new(Uri)` runs the same post-parse validation but cannot recover syntax discarded before it received the typed URI.
- [ ] Implement `from_request` by preserving method/body, replacing only the target, and applying the same request normalizer immediately. Adapters still reapply normalization immediately before SDK construction because `headers_mut` can introduce unsafe fields later.
- [ ] Add and test provider accessors `backend_target`, `cert_host`, `host_authority`, `host_name`, and `sni_hostname`. `host_name` is the host-only value required by Fastly backend identity and must not include IPv6 brackets or a port.
- [ ] Set exact defaults: decoded response 1 MiB, request body 8 MiB, Brotli window bits 24; all byte caps and counters are `u64`.
- [ ] Rerun the focused test and `cargo test --offline --locked -p edgezero-core --lib`; expect success.
- [ ] Commit: `feat(core): add canonical outbound request types`.

### Task 3: Add dispatch validation and request normalization

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`

- [ ] Add failing `dispatch_validation_*` tests for the portable method set, GET/HEAD body rules, streamed-body detection, streamed-response detection, invalid policy combinations, and configured Brotli window values below 10 or above 30. The infallible builder stores any `u8`; only dispatch validation rejects the invalid range.
- [ ] Add failing `request_normalization_*` tests for standard hop-by-hop fields, every valid `Connection` nomination, repeated `Connection` fields, malformed tokens, `Host`, `Content-Length`, `Transfer-Encoding`, and idempotence.
- [ ] Run both filters separately; expect failures:
  - `cargo test --offline --locked -p edgezero-core --lib dispatch_validation_`
  - `cargo test --offline --locked -p edgezero-core --lib request_normalization_`
- [ ] Implement `validate_for_dispatch`, `normalize_for_dispatch`, `is_stream_body`, `is_stream_response`, and private `budget_inputs`. Validation must happen before body polling; normalization must be repeatable at adapter boundaries.
- [ ] Rerun both filters and the core library suite; expect success.
- [ ] Commit: `feat(core): validate outbound dispatch requests`.

### Task 4: Add dispatch budget arithmetic

**Files:**
- Modify: `crates/edgezero-core/src/time.rs`
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

**Contract:**

```rust
pub struct DispatchBudget {
    pub cause: BudgetSource,
    pub deadline: Deadline,
    pub duration: Duration,
}

pub fn dispatch_budget(
    request: &OutboundRequest,
    now: Instant,
) -> Result<DispatchBudget, EdgeError>;
```

- [ ] Add failing `dispatch_budget_*` tests for default, timeout only, deadline only, both orderings, equal values favoring `PerCallTimeout`, zero/expired inputs returning attributed 504, seven-day clamping, and one shared `now` across a batch.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib dispatch_budget_`; expect failure.
- [ ] Implement only checked `Instant`/`Duration` arithmetic. Preserve the winning `BudgetSource` through every clamp.
- [ ] Rerun the focused and core suites; expect success.
- [ ] Commit: `feat(core): compute outbound dispatch budgets`.

### Task 5: Add client and response value surfaces

**Files:**
- Modify: `crates/edgezero-core/src/outbound.rs`
- Modify: `crates/edgezero-core/src/context.rs`
- Modify: `crates/edgezero-core/src/lib.rs`

**Contract:**

```rust
#[async_trait(?Send)]
pub trait OutboundHttpClient: Send + Sync {
    async fn send(&self, request: OutboundRequest) -> Result<OutboundResponse, EdgeError>;
    async fn send_all(
        &self,
        requests: Vec<OutboundRequest>,
    ) -> Vec<Result<OutboundResponse, EdgeError>>;
}

#[derive(Clone)]
pub struct HttpClient {
    inner: Arc<dyn OutboundHttpClient>,
}

pub struct OutboundResponse {
    body: Body,
    headers: HeaderMap,
    request_method: Method,
    status: StatusCode,
}

impl HttpClient {
    pub fn new(client: Arc<dyn OutboundHttpClient>) -> Self;
    pub async fn send(&self, request: OutboundRequest)
        -> Result<OutboundResponse, EdgeError>;
    pub async fn send_all(&self, requests: Vec<OutboundRequest>)
        -> Vec<Result<OutboundResponse, EdgeError>>;
    pub fn with_client<C: OutboundHttpClient + 'static>(client: C) -> Self;
}

impl OutboundResponse {
    pub fn body(&self) -> &Body;
    pub fn headers(&self) -> &HeaderMap;
    pub fn headers_mut(&mut self) -> &mut HeaderMap;
    pub fn into_body(self) -> Body;
    pub fn into_parts(self) -> (Method, StatusCode, HeaderMap, Body);
    pub fn is_success(&self) -> bool;
    pub fn new(
        request_method: Method,
        status: StatusCode,
        headers: HeaderMap,
        body: Body,
    ) -> Self;
    pub fn status(&self) -> StatusCode;
}
```

- [ ] Add failing `http_client_*` tests proving `send`/`send_all` delegation, empty-batch forwarding, index alignment, and mixed slot-result preservation. Complete preflight belongs to each adapter contract in Phases 4-6.
- [ ] Add failing `outbound_response_*` tests for originating method, status/success, immutable and adapter-facing mutable header access, repeated headers, borrowed/consuming body access, and `into_parts`. `into_response` is owned by Phase 2 Task 3 because it depends on response normalization.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib http_client_`; expect a nonzero failure.
- [ ] Run `cargo test --offline --locked -p edgezero-core --lib outbound_response_`; expect a nonzero failure.
- [ ] Implement `HttpClient::send` and `send_all` only. Do not expose the inner client and do not recreate `ProxyService::forward`.
- [ ] Implement `OutboundResponse::new`, `headers_mut`, `status`, `is_success`, `headers`, `body`, `into_body`, and `into_parts() -> (Method, StatusCode, HeaderMap, Body)`. Bounded drains, JSON, and `into_response` remain Phase 2.
- [ ] Add `RequestContext::http_client()` using `self.request.extensions()` directly. Keep `proxy_handle()` only as temporary branch scaffolding and add no alias between the two client types.
- [ ] Rerun focused tests, `cargo test --offline --locked -p edgezero-core --lib`, and `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [ ] Commit: `feat(core): add outbound HTTP client surface`.

## Phase Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-core --target wasm32-unknown-unknown`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] `(cd examples/app-demo && cargo test --locked --workspace --all-targets)`
- [ ] `git diff --check`

Expected result: both core modules compile temporarily, the new outbound API and budget layer are fully tested, and no adapter behavior has changed.
