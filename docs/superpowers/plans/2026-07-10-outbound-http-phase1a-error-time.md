# Outbound HTTP — Phase 1a: `EdgeError` 502/504 + `time.rs` primitives

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Land the two **purely additive**, dependency-free core primitives from the outbound-HTTP spec ([`2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md)): the `EdgeError::BadGateway`/`GatewayTimeout` variants (§7 error.rs) and the `edgezero-core::time` module's `Deadline` + budget constants (§3.3.1). Neither touches the `proxy → outbound` rename or `Body`, so each task keeps `cargo test --workspace` green.

**Architecture:** `edgezero-core` only. Additive: new `EdgeError` variants (the enum is `#[non_exhaustive]`, but that does **not** relax exhaustiveness *inside the defining crate* — every exhaustive `match`, including the ones in the test module, must gain the two arms) and a brand-new `time` module. No adapter, CLI, or app-demo change. **`DispatchBudget` and `dispatch_budget` are BOTH deferred to Phase 1b** — the spec (§3.3.2) treats the carrier struct and its authoritative producer as one contract, and shipping a freely-constructible `DispatchBudget` without its producer invites misuse. Phase 1a lands `Deadline` + constants only.

**Tech Stack:** Rust 1.95 (edition 2021), `thiserror`, `serde_json`, `web-time` (for `Instant`), `futures::executor::block_on` for async tests.

## Global Constraints (inherited from the master plan)

- **WASM-first:** no `tokio`/runtime deps; use `web-time::Instant`, not `std::time::Instant`. Core stays `default-features = false`.
- **Colocated tests** (`#[cfg(test)]` same file); async tests use `futures::executor::block_on`.
- **Verbatim constants:** `DEFAULT_NO_DEADLINE_BUDGET = 30 s`, `DEADLINE_FAR_FUTURE = 7 days`, `BATCH_DISPATCH_SLACK_MAX = 25 ms`.
- **CI gates must stay green:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-targets`; `cargo check --workspace --all-targets --features "fastly cloudflare spin"`; `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- **Verified against the tree (HEAD 970f1f6):** `EdgeError` today has variants `BadRequest, ConfigOutOfDate, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation`. The new arms below must be added to **eight** exhaustive matches — **five in `impl`**: `inner()` (error.rs:87), `kind_str()` (:110), `message()` (:125), `status()` (:180), `IntoResponse`'s `field_path_opt` (:221) — **and three in the test module** (:281, :335, :369), each a `match err { ConfigOutOfDate {..} => .. , <all others> => panic!(..) }` with **no `_` wildcard**. Also the matrix test `kind_strings_per_variant` (:502) must gain rows for both new variants. `web-time` presence is confirmed in Task 0.
- **`cargo test` accepts only ONE positional filter** — `cargo test -p X a b` fails with `unexpected argument 'b'` (verified). Use a single common substring or two separate commands.

---

### Task 0: Confirm the `web-time` dependency

**Files:** Inspect `crates/edgezero-core/Cargo.toml`

- [ ] **Step 1: Check whether `web-time` is already a dependency**

Run: `grep -n 'web-time\|web_time' crates/edgezero-core/Cargo.toml`
Expected: a line like `web-time = { workspace = true }`.

- [ ] **Step 2: If absent, add it** under `[dependencies]`:

```toml
web-time = { workspace = true }
```

(If the root `[workspace.dependencies]` lacks it, add `web-time = "1"` there first.)

- [ ] **Step 3: Verify it compiles** — Run: `cargo check -p edgezero-core` — Expected: `Finished`.

---

### Task 1: `EdgeError::BadGateway` (502) + `GatewayTimeout` (504)

**Files:**
- Modify: `crates/edgezero-core/src/error.rs` (enum + constructors + 8 match sites + matrix test)
- Test: `crates/edgezero-core/src/error.rs` (colocated `#[cfg(test)]`)

**Interfaces:**
- Produces: `EdgeError::bad_gateway<S: Into<String>>(msg) -> Self` (502, kind `"bad_gateway"`), `EdgeError::gateway_timeout<S: Into<String>>(msg) -> Self` (504, kind `"gateway_timeout"`). JSON via existing `IntoResponse`: `{ "error": { "status", "kind", "message" } }` (no `field_path` for these two).

- [ ] **Step 1: Write the failing tests (table-driven, BOTH variants)**

The existing `#[cfg(test)] mod tests` already imports `StatusCode`, `CONTENT_TYPE`, `HeaderValue`, `str` and does `use super::*;`, and has a `parse_body(response) -> serde_json::Value` helper (error.rs:498). Add — **no new imports** (re-importing under `-D warnings` fails):

```rust
#[test]
fn bad_gateway_and_gateway_timeout_surface() {
    for (err, code, msg) in [
        (EdgeError::bad_gateway("upstream refused"), StatusCode::BAD_GATEWAY, "upstream refused"),
        (EdgeError::gateway_timeout("deadline expired"), StatusCode::GATEWAY_TIMEOUT, "deadline expired"),
    ] {
        assert_eq!(err.status(), code);
        assert_eq!(err.message(), msg);
        assert!(err.inner().is_none());
    }
}

#[test]
fn bad_gateway_and_gateway_timeout_json_shape() {
    for (err, code, kind, msg) in [
        (EdgeError::bad_gateway("nope"), 502u16, "bad_gateway", "nope"),
        (EdgeError::gateway_timeout("late"), 504u16, "gateway_timeout", "late"),
    ] {
        let response = err.into_response().expect("response");
        assert_eq!(response.status().as_u16(), code);
        let v = parse_body(response); // existing helper: reads body -> serde_json::Value
        assert_eq!(v["error"]["status"], code);
        assert_eq!(v["error"]["kind"], serde_json::Value::from(kind));
        assert_eq!(v["error"]["message"], serde_json::Value::from(msg));
        assert!(v["error"].get("field_path").is_none(), "502/504 carry no field_path");
    }
}
```

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p edgezero-core bad_gateway` (single filter — matches both `bad_gateway_*` test fns) — Expected: FAIL to compile (`no variant or associated item named bad_gateway`).

- [ ] **Step 3: Add the two variants** in `pub enum EdgeError` (after `Validation`):

```rust
    /// Upstream/transport failure at the adapter boundary (DNS/TLS/connect/
    /// unreachable, or a non-timeout send failure). HTTP 502.
    #[error("{message}")]
    BadGateway { message: String },
    /// A wall-clock deadline or per-request timeout fired. HTTP 504.
    #[error("{message}")]
    GatewayTimeout { message: String },
```

- [ ] **Step 4: Add the constructors** in `impl EdgeError`:

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

- [ ] **Step 5: Update ALL eight exhaustive matches (crate won't compile until every one is done)**

`impl` sites:
- `kind_str()` — add `EdgeError::BadGateway { .. } => "bad_gateway",` and `EdgeError::GatewayTimeout { .. } => "gateway_timeout",`
- `status()` — add `EdgeError::BadGateway { .. } => StatusCode::BAD_GATEWAY,` and `EdgeError::GatewayTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,`
- `message()` — add `| EdgeError::BadGateway { message } | EdgeError::GatewayTimeout { message }` to the "clone the `message`" arm.
- `inner()` — add both variants to the `=> None` arm list.
- `IntoResponse::into_response`'s `field_path_opt` match — add both variants to the `=> None` arm list.

**Test-module sites (these have explicit panic-arms listing every non-`ConfigOutOfDate` variant, NO `_`):** in each of the three `match err { … }` blocks at error.rs ~281, ~335, ~369, add `| EdgeError::BadGateway { .. } | EdgeError::GatewayTimeout { .. }` to the `=> panic!("expected ConfigOutOfDate")` arm.

- [ ] **Step 6: Compiler-driven catch — build and fix any remaining non-exhaustive match**

Run: `cargo build -p edgezero-core --tests`
If it reports `E0004 non-exhaustive patterns` anywhere, add the two arms at that exact site (the compiler prints the file:line). Repeat until it builds. Expected end state: builds clean.

- [ ] **Step 7: Extend the matrix test `kind_strings_per_variant` (error.rs:502)**

That test uses an `assert_kind!($err, $expected_kind:literal, $expected_status:literal)` macro per variant, and the existing rows pass **suffixed** status literals (e.g. `assert_kind!(EdgeError::bad_request("x"), "bad_request", 400_u16);`). Match that form exactly — add two invocations inside it:

```rust
        assert_kind!(EdgeError::bad_gateway("x"), "bad_gateway", 502_u16);
        assert_kind!(EdgeError::gateway_timeout("x"), "gateway_timeout", 504_u16);
```

- [ ] **Step 8: Run the new + matrix tests to verify they pass**

Run: `cargo test -p edgezero-core bad_gateway` then `cargo test -p edgezero-core kind_strings_per_variant`
Expected: PASS.

- [ ] **Step 9: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 10: Commit**

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
- Produces (for Phase 1b `dispatch_budget` + all adapters): `Deadline` (`Copy`), `Deadline::after(Duration) -> Self`, `::at_instant(web_time::Instant) -> Self`, `::instant(&self) -> web_time::Instant`, `::remaining(&self) -> Option<Duration>`, `::is_expired(&self) -> bool`; consts `DEFAULT_NO_DEADLINE_BUDGET` (30 s), `DEADLINE_FAR_FUTURE` (7 days), `BATCH_DISPATCH_SLACK_MAX` (25 ms). **`DispatchBudget` ships in Phase 1b with `dispatch_budget`.**

**Deadline semantics (matches spec §3.3.2 `deadline <= now => expired`):** a deadline whose instant is **exactly now** is **expired** — `is_expired()` is `true` and `remaining()` is `None` at equality, not `Some(0)`. A naive `checked_duration_since(now).is_none()` gets this wrong (it returns `Some(ZERO)` at equality), so the impl below compares instants directly.

- [ ] **Step 1: Write the failing tests (deterministic — bounded by explicit instants, no wall-clock tolerance windows)**

Create `crates/edgezero-core/src/time.rs` with only the test module + `use`:

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

    // Deterministic: build from an explicit past/now/future instant, not a
    // tolerance window over Instant::now().
    #[test]
    fn deadline_before_now_is_expired() {
        let base = Instant::now();
        let past = Deadline::at_instant(base.checked_sub(Duration::from_secs(1)).unwrap_or(base));
        assert!(past.is_expired());
        assert_eq!(past.remaining(), None);
    }

    #[test]
    fn deadline_exactly_now_is_expired() {
        // Equality must count as expired (spec: deadline <= now). Use an instant
        // strictly in the past-or-equal boundary: `base` captured, then asserted
        // against a later `now` inside is_expired(), so `base <= now` holds.
        let base = Instant::now();
        let at_now = Deadline::at_instant(base);
        // By the time is_expired() reads Instant::now(), it is >= base, so expired.
        assert!(at_now.is_expired(), "a deadline at-or-before now is expired");
    }

    #[test]
    fn deadline_in_future_has_positive_remaining() {
        let base = Instant::now();
        let future = Deadline::at_instant(base + Duration::from_secs(3600));
        assert!(!future.is_expired());
        let r = future.remaining().expect("future deadline has remaining");
        // Bounded by explicit instants: remaining is in (elapsed-since-base .. 3600s].
        assert!(r <= Duration::from_secs(3600));
        assert!(r > Duration::from_secs(3599)); // at most ~1s can have elapsed in-test
    }

    #[test]
    fn after_clamps_duration_max_to_far_future() {
        // Prove the 7-DAY CLAMP, not merely "some positive deadline".
        let before = Instant::now();
        let d = Deadline::after(Duration::MAX);
        let after = Instant::now();
        assert!(!d.is_expired());
        let r = d.remaining().expect("clamped deadline is in the future");
        // now + 7d was computed between `before` and `after`; so remaining must be
        // within [FAR_FUTURE - (after-before) - slack, FAR_FUTURE].
        assert!(r <= DEADLINE_FAR_FUTURE, "never exceeds the clamp");
        assert!(r > DEADLINE_FAR_FUTURE - (after - before) - Duration::from_millis(50));
    }

    #[test]
    fn instant_round_trips() {
        let base = Instant::now() + Duration::from_secs(10);
        assert_eq!(Deadline::at_instant(base).instant(), base);
    }
}
```

- [ ] **Step 2: Wire the module in and run to verify failure**

Add `pub mod time;` to `crates/edgezero-core/src/lib.rs` (alphabetical position among the `pub mod` lines).
Run: `cargo test -p edgezero-core --lib time::`
Expected: FAIL to compile (`cannot find value DEFAULT_NO_DEADLINE_BUDGET`, `cannot find type Deadline`).

- [ ] **Step 3: Implement constants + `Deadline`**

Prepend to `crates/edgezero-core/src/time.rs` (above `#[cfg(test)]`):

```rust
use web_time::Instant;

/// Budget for an outbound request that specifies neither timeout nor deadline. §3.3.1.
pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);

/// Hard clamp on any caller-supplied duration so `Deadline::after` cannot panic on a
/// pathological `Duration::MAX`. 7 days — below Fastly's u32-ms ceiling, above any
/// realistic budget. §3.3.1.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Max adapter overhead tolerated between the `send_all` `batch_now` snapshot and SDK
/// timer arming before a slot fails closed. §3.3.4 / §4.3.
pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);

/// An absolute, `Copy` wall-clock deadline, shared across a fan-out batch so every slot
/// honours the same cap. §3.3.1. A deadline at-or-before `now` is **expired**.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    /// `now + min(d, DEADLINE_FAR_FUTURE)`. Never panics: the clamped add uses
    /// saturating `checked_add(..).unwrap_or(now)`, so a defensive overflow yields an
    /// already-expired deadline rather than panicking.
    #[must_use]
    pub fn after(d: Duration) -> Self {
        let now = Instant::now();
        let clamped = d.min(DEADLINE_FAR_FUTURE);
        Deadline(now.checked_add(clamped).unwrap_or(now))
    }

    #[must_use]
    pub fn at_instant(instant: Instant) -> Self {
        Deadline(instant)
    }

    #[must_use]
    pub fn instant(&self) -> Instant {
        self.0
    }

    /// Remaining time, or `None` once the deadline is reached **or passed** (equality
    /// counts as passed — spec §3.3.2 `deadline <= now`).
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        let now = Instant::now();
        if self.0 <= now {
            None
        } else {
            Some(self.0 - now)
        }
    }

    /// `true` once the deadline instant is at-or-before now.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.0 <= Instant::now()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass** — Run: `cargo test -p edgezero-core --lib time::` — Expected: PASS (all six).

- [ ] **Step 5: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-core/src/time.rs crates/edgezero-core/src/lib.rs
git commit -m "feat(core): add time module (Deadline + budget constants)"
```

---

### Task 3: Full CI-gate verification (all five gates)

**Files:** none (verification only). Run from the repo root.

- [ ] **Step 1: Format check + workspace test**

Run: `cargo fmt --all -- --check && cargo test --workspace --all-targets`
Expected: no diff; all green. (Confirms the additive changes broke no crate.)

- [ ] **Step 2: Clippy (all targets, all features)**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 3: Feature-combo check**

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
Expected: `Finished`.

- [ ] **Step 4: Spin wasm target check**

Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
Expected: `Finished`.

(These four commands are exactly the repo's CI gates 1–5 from CLAUDE.md. Do not skip the wasm target — it is the one most likely to catch an accidental `std::time` / non-WASM import.)

---

## Self-Review

- **Spec coverage:** Task 1 = §7 error.rs (both variants, full surface, JSON shape for **both**, matrix test); Task 2 = §3.3.1 (Deadline, constants). `DispatchBudget` + `dispatch_budget()` (§3.3.2) are deferred **together** to Phase 1b — a stated sequencing boundary, not a gap.
- **Compile-safety (the class of bug a prior review caught):** the eight exhaustive matches (5 impl + 3 test panic-arms) are enumerated *and* backed by a compiler-driven catch step; the `cargo test` single-filter rule is applied; `is_expired` compares instants directly so exact-now is expired.
- **No placeholders / no flaky tests:** every step has exact code, paths, single-filter commands, expected output; timing tests are bounded by explicit `at_instant` instants (no `now() - 1s` underflow, no wide tolerance windows), and the clamp test proves the 7-day bound.

## Next (not this plan; each is its own plan, NOT one atomic step)

Phase 1b splits into independently-landable slices (the master roadmap lists them): (1) `DispatchBudget` + `dispatch_budget` + the `budget_inputs()` accessor; (2) `OutboundRequest`/`OutboundResponse`/`ResponseMode` + canonical URI accessors + `validate_for_dispatch`; (3) the `Body::Stream` error-type change and the `proxy → outbound` rename — the breaking slice that lands atomically with the four adapters. **Do not treat the Phase 1b list as a single step.**
