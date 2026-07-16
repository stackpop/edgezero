# Outbound HTTP — Phase 1a: `EdgeError` 502/504 + `time.rs` primitives

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Land the two **purely additive**, dependency-free core primitives from the outbound-HTTP spec ([`2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md)): the `EdgeError::BadGateway`/`GatewayTimeout` variants (§7 error.rs) and the `edgezero-core::time` module's `Deadline` + budget constants (§3.3.1). Neither touches the `proxy → outbound` rename or `Body`, so each task keeps `cargo test --workspace` green.

**Architecture:** `edgezero-core` only. Additive: new `EdgeError` variants (the enum is `#[non_exhaustive]`, and every internal exhaustive `match` is updated in the same task) and a brand-new `time` module. No adapter, CLI, or app-demo change. `dispatch_budget(&OutboundRequest)` is **deferred to Phase 1b** because it needs `OutboundRequest` (the rename slice); this phase lands `Deadline`, `DispatchBudget` (the struct), and the constants, which are standalone.

**Tech Stack:** Rust 1.95 (edition 2021), `thiserror`, `serde_json`, `web-time` (for `Instant`), `futures::executor::block_on` for async tests.

## Global Constraints (inherited from the master plan)

- **WASM-first:** no `tokio`/runtime deps; use `web-time::Instant`, not `std::time::Instant`. Core stays `default-features = false`.
- **Colocated tests** (`#[cfg(test)]` same file); async tests use `futures::executor::block_on`.
- **Verbatim constants:** `DEFAULT_NO_DEADLINE_BUDGET = 30 s`, `DEADLINE_FAR_FUTURE = 7 days`, `BATCH_DISPATCH_SLACK_MAX = 25 ms`.
- **CI gates must stay green:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-targets`.
- **Verified against the tree (HEAD 341d042):** `EdgeError` today has variants `BadRequest, ConfigOutOfDate, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation` — the new arms below must be added to **every** exhaustive match: `kind_str()` (error.rs:110), `message()` (:125), `status()` (:180), `inner()` (:87), and `IntoResponse::into_response`'s `field_path_opt` match (:221). `web-time` presence is confirmed in Task 0.

---

### Task 0: Confirm the `web-time` dependency

**Files:**
- Inspect: `crates/edgezero-core/Cargo.toml`

- [ ] **Step 1: Check whether `web-time` is already a dependency**

Run: `grep -n 'web-time\|web_time' crates/edgezero-core/Cargo.toml`
Expected: a line like `web-time = { workspace = true }` or `web-time = "1"`.

- [ ] **Step 2: If absent, add it**

If Step 1 printed nothing, add under `[dependencies]` in `crates/edgezero-core/Cargo.toml`:

```toml
web-time = { workspace = true }
```

(If the root `Cargo.toml` `[workspace.dependencies]` lacks `web-time`, add `web-time = "1"` there first, then the crate line above.)

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p edgezero-core`
Expected: `Finished`. No test yet — this is a dependency-availability gate.

---

### Task 1: `EdgeError::BadGateway` (502) + `GatewayTimeout` (504)

**Files:**
- Modify: `crates/edgezero-core/src/error.rs` (enum + constructors + 5 match sites)
- Test: `crates/edgezero-core/src/error.rs` (colocated `#[cfg(test)]`)

**Interfaces:**
- Produces (for Phase 1b+ and all adapters): `EdgeError::bad_gateway<S: Into<String>>(msg) -> Self` (502, kind `"bad_gateway"`), `EdgeError::gateway_timeout<S: Into<String>>(msg) -> Self` (504, kind `"gateway_timeout"`). JSON body via existing `IntoResponse`: `{ "error": { "status", "kind", "message" } }` (no `field_path` for these two).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/edgezero-core/src/error.rs`:

The existing test module (`#[cfg(test)] mod tests`) already imports `StatusCode`,
`CONTENT_TYPE`, `HeaderValue`, `str`, and brings the parent into scope with `use
super::*;` — so these tests need **no new imports** (do not re-`use StatusCode`, it would
be an unused/duplicate import under `-D warnings`). The buffered body is read
**synchronously** via `into_body().into_bytes()`, exactly like the existing
`into_response_sets_json_payload` test — not the async `into_bytes_bounded`.

```rust
#[test]
fn bad_gateway_and_gateway_timeout_surface() {
    let bg = EdgeError::bad_gateway("upstream refused");
    assert_eq!(bg.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(bg.message(), "upstream refused");
    assert!(bg.inner().is_none());

    let gt = EdgeError::gateway_timeout("deadline expired");
    assert_eq!(gt.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(gt.message(), "deadline expired");
    assert!(gt.inner().is_none());
}

#[test]
fn bad_gateway_json_shape_has_status_kind_message_no_field_path() {
    let response = EdgeError::bad_gateway("nope").into_response().expect("response");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

    // Body is `{ "error": { "status": 502, "kind": "bad_gateway", "message": "nope" } }`
    // with NO "field_path" key. Read synchronously like the existing test.
    let body = response.into_body().into_bytes().expect("buffered");
    let v: serde_json::Value = serde_json::from_slice(body.as_ref()).unwrap();
    assert_eq!(v["error"]["status"], 502);
    assert_eq!(v["error"]["kind"], "bad_gateway");
    assert_eq!(v["error"]["message"], "nope");
    assert!(v["error"].get("field_path").is_none());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p edgezero-core bad_gateway`
Expected: FAIL to **compile** — `no variant or associated item named bad_gateway`.

- [ ] **Step 3: Add the two variants**

In `crates/edgezero-core/src/error.rs`, inside `pub enum EdgeError { … }` (keep alphabetical-ish grouping; place after `Validation`):

```rust
    /// Upstream/transport failure at the adapter boundary (DNS/TLS/connect/
    /// unreachable, or a non-timeout send failure). HTTP 502.
    #[error("{message}")]
    BadGateway { message: String },
    /// A wall-clock deadline or per-request timeout fired. HTTP 504.
    #[error("{message}")]
    GatewayTimeout { message: String },
```

- [ ] **Step 4: Add the constructors**

In `impl EdgeError { … }`, next to the other `#[inline]` constructors:

```rust
    #[inline]
    pub fn bad_gateway<S: Into<String>>(message: S) -> Self {
        EdgeError::BadGateway { message: message.into() }
    }

    #[inline]
    pub fn gateway_timeout<S: Into<String>>(message: S) -> Self {
        EdgeError::GatewayTimeout { message: message.into() }
    }
```

- [ ] **Step 5: Update every exhaustive match (the crate will not compile until all are done)**

`kind_str()` — add:
```rust
            EdgeError::BadGateway { .. } => "bad_gateway",
            EdgeError::GatewayTimeout { .. } => "gateway_timeout",
```
`message()` — extend the "clone the `message` field" arm to include the two new variants:
```rust
            EdgeError::BadRequest { message }
            | EdgeError::ConfigOutOfDate { message, .. }
            | EdgeError::Validation { message }
            | EdgeError::NotImplemented { message }
            | EdgeError::ServiceUnavailable { message }
            | EdgeError::BadGateway { message }
            | EdgeError::GatewayTimeout { message } => message.clone(),
```
`status()` — add:
```rust
            EdgeError::BadGateway { .. } => StatusCode::BAD_GATEWAY,
            EdgeError::GatewayTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
```
`inner()` — add the two variants to the `=> None` arm list.
`IntoResponse::into_response`'s `field_path_opt` match — add the two variants to the `=> None` arm list.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p edgezero-core bad_gateway gateway_timeout`
Expected: PASS (both new tests) with no other test broken.

- [ ] **Step 7: Lint + full-crate test**

Run: `cargo clippy -p edgezero-core --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 8: Commit**

```bash
git add crates/edgezero-core/src/error.rs
git commit -m "feat(core): add EdgeError::BadGateway (502) + GatewayTimeout (504)"
```

---

### Task 2: `time` module — constants + `Deadline`

**Files:**
- Create: `crates/edgezero-core/src/time.rs`
- Modify: `crates/edgezero-core/src/lib.rs` (add `pub mod time;`)
- Test: `crates/edgezero-core/src/time.rs` (colocated)

**Interfaces:**
- Produces (for Phase 1b `dispatch_budget` + all adapters): `Deadline` (`Copy`), `Deadline::after(Duration) -> Self`, `::at_instant(web_time::Instant) -> Self`, `::instant(&self) -> web_time::Instant`, `::remaining(&self) -> Option<Duration>`, `::is_expired(&self) -> bool`; `pub struct DispatchBudget { pub duration: Duration, pub deadline: Deadline }`; consts `DEFAULT_NO_DEADLINE_BUDGET` (30 s), `DEADLINE_FAR_FUTURE` (7 days), `BATCH_DISPATCH_SLACK_MAX` (25 ms).

- [ ] **Step 1: Write the failing test (new file, tests first)**

Create `crates/edgezero-core/src/time.rs` with **only** the test module and the `use` lines so it fails to compile against absent items:

```rust
use std::time::Duration;

#[cfg(test)]
mod tests {
    use super::*;
    use web_time::Instant;

    #[test]
    fn constants_have_exact_values() {
        assert_eq!(DEFAULT_NO_DEADLINE_BUDGET, Duration::from_secs(30));
        assert_eq!(DEADLINE_FAR_FUTURE, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(BATCH_DISPATCH_SLACK_MAX, Duration::from_millis(25));
    }

    #[test]
    fn after_max_duration_clamps_and_does_not_panic() {
        // Duration::MAX must be clamped to DEADLINE_FAR_FUTURE, never panic.
        let d = Deadline::after(Duration::MAX);
        assert!(!d.is_expired());
        // remaining is positive and no more than the clamp.
        assert!(d.remaining().unwrap() <= DEADLINE_FAR_FUTURE);
    }

    #[test]
    fn expired_deadline_reports_none_and_is_expired() {
        let past = Deadline::at_instant(Instant::now() - Duration::from_secs(1));
        assert!(past.is_expired());
        assert_eq!(past.remaining(), None);
    }

    #[test]
    fn future_deadline_has_remaining() {
        let d = Deadline::after(Duration::from_secs(60));
        assert!(!d.is_expired());
        let r = d.remaining().unwrap();
        assert!(r > Duration::from_secs(50) && r <= Duration::from_secs(60));
    }

    #[test]
    fn dispatch_budget_struct_holds_fields() {
        let d = Deadline::after(Duration::from_secs(5));
        let b = DispatchBudget { duration: Duration::from_secs(5), deadline: d };
        assert_eq!(b.duration, Duration::from_secs(5));
        assert!(!b.deadline.is_expired());
    }
}
```

- [ ] **Step 2: Wire the module in and run the test to verify it fails**

Add to `crates/edgezero-core/src/lib.rs` (alongside the other `pub mod` lines, keep alphabetical: after `pub mod store_registry;` / wherever `t*` sorts — `pub mod time;`):

```rust
pub mod time;
```

Run: `cargo test -p edgezero-core --lib time::`
Expected: FAIL to compile — `cannot find value DEFAULT_NO_DEADLINE_BUDGET`, `cannot find type Deadline`, etc.

- [ ] **Step 3: Implement the constants + `Deadline` + `DispatchBudget`**

Prepend to `crates/edgezero-core/src/time.rs` (above the `#[cfg(test)]` block):

```rust
use web_time::Instant;

/// Budget applied to an outbound request that specifies neither a timeout nor a
/// deadline. §3.3.1.
pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);

/// Hard clamp on any caller-supplied duration, so `Deadline::after` /
/// `dispatch_budget` cannot panic on a pathological `Duration::MAX`. 7 days —
/// below Fastly's u32-ms backend-timeout ceiling, above any realistic budget. §3.3.1.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Max adapter overhead tolerated between the `send_all` `batch_now` snapshot and
/// SDK timer arming before a slot fails closed. §3.3.4 / §4.3.
pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);

/// An absolute, `Copy` wall-clock deadline. Constructed once and shared across a
/// fan-out batch so every slot honours the same cap. §3.3.1.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    /// `now + min(d, DEADLINE_FAR_FUTURE)`. Never panics: the clamped add uses
    /// saturating `checked_add(..).unwrap_or(now)`, so even a defensive overflow
    /// yields an already-expired deadline (fails closed) rather than panicking.
    #[must_use]
    pub fn after(d: Duration) -> Self {
        let now = Instant::now();
        let clamped = d.min(DEADLINE_FAR_FUTURE);
        Deadline(now.checked_add(clamped).unwrap_or(now))
    }

    /// Construct from an absolute instant (used by `dispatch_budget`).
    #[must_use]
    pub fn at_instant(instant: Instant) -> Self {
        Deadline(instant)
    }

    #[must_use]
    pub fn instant(&self) -> Instant {
        self.0
    }

    /// Remaining time, or `None` once the deadline has passed.
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.0.checked_duration_since(Instant::now())
    }

    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.remaining().is_none()
    }
}

/// The resolved budget for one outbound exchange: an effective `duration` (for SDK
/// timers) and the absolute `deadline` (for cooperative checks). §3.3.2.
#[derive(Debug, Clone, Copy)]
pub struct DispatchBudget {
    pub duration: Duration,
    pub deadline: Deadline,
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p edgezero-core --lib time::`
Expected: PASS (all five).

- [ ] **Step 5: Lint + fmt + full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-core/src/time.rs crates/edgezero-core/src/lib.rs
git commit -m "feat(core): add time module (Deadline, DispatchBudget, budget constants)"
```

---

### Task 3: Workspace green gate

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test + lint**

Run: `cargo test --workspace --all-targets && cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: all green — confirms the additive changes broke no adapter, CLI, or macro crate.

- [ ] **Step 2: Feature + wasm checks (the remaining CI gates)**

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin" && cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
Expected: `Finished` for both.

---

## Self-Review

- **Spec coverage:** Task 1 = §7 error.rs (BadGateway/GatewayTimeout full surface + JSON shape); Task 2 = §3.3.1 (Deadline, constants, DispatchBudget). `dispatch_budget()` itself (§3.3.2) is explicitly **deferred to Phase 1b** (needs `OutboundRequest` + the `budget_inputs()` accessor) — not a gap, a sequencing boundary stated up front.
- **Type consistency:** `bad_gateway`/`gateway_timeout` constructor names match the kind strings and the adapter error-mapping in §4.2/§4.4; `Deadline`/`DispatchBudget`/const names match the master plan's Phase-1 interface block and the compile-verified skeleton.
- **No placeholders:** every step has exact code, file paths, commands, and expected output.

## Next (not this plan)

Phase 1b (own plan): `OutboundRequest`/`OutboundResponse`/`OutboundHttpClient`/`ResponseMode`, the `budget_inputs()` accessor + `dispatch_budget()`, `validate_for_dispatch` (pub), canonical URI accessors, `Body::Stream` error-type change, and the `proxy → outbound` rename — the **breaking** slice that lands atomically with the four adapters.
