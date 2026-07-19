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
- **Verified against the tree (HEAD 970f1f6):** `EdgeError` today has variants `BadRequest, ConfigOutOfDate, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation`. The new arms below must be added to **eight** exhaustive matches — **five in `impl`**: `inner()` (error.rs:87), `kind_str()` (:110), `message()` (:125), `status()` (:180), `IntoResponse`'s `field_path_opt` (:221) — **and three in the test module** (:281, :335, :369), each a `match err { ConfigOutOfDate {..} => .. , <all others> => panic!(..) }` with **no `_` wildcard**. Also **three** per-variant tests must gain rows (only `kind_strings_per_variant` is exhaustive; the other two are subset checks) for both new variants: `kind_strings_per_variant` (:502), `retry_after_only_on_config_out_of_date` (:549 — 502/504 must NOT carry `Retry-After`), and `field_path_only_on_config_out_of_date` (:569). `web-time` presence is confirmed in Task 0.
- **`cargo test` accepts only ONE positional filter** — `cargo test -p X a b` fails with `unexpected argument 'b'` (verified). Use a single common substring or two separate commands.
- **The Clippy gate is STRICT — read this before writing any code.** The root `Cargo.toml` sets `restriction = { level = "deny", priority = -1 }`, and the following are **not** allow-listed, so they are hard errors in **production** code:
  - `missing_inline_in_public_items` → **every public fn needs `#[inline]`** (error.rs already carries 14).
  - `min_ident_chars` → no single-char idents (`d` → `duration`).
  - `arithmetic_side_effects` → **no bare `+` / `-`** on `Instant`/`Duration`; use `checked_add` / `checked_duration_since`.
  - `expect_used`, `unwrap_used`, `as_conversions` → forbidden in production; use `?`/`ok_or`/`From`/`TryFrom`.
  - **`unseparated_literal_suffix`** → integer suffixes need an underscore: `502_u16`, not `502u16` (verified: `502u16` errors; the opposing `separated_literal_suffix` is allow-listed, so the underscore form is the one that passes).
  - **`arbitrary_source_item_ordering`** → **items must be ALPHABETICAL** — consts, enum **variants**, and impl fns alike (verified). This is why `error.rs`'s variants and methods are already alphabetized. Insert new items **in place**; never append.
  - **`duration_suboptimal_units`** (pedantic; CI runs `-D warnings`, so it *fails the build*) → use the largest readable unit: `Duration::from_hours(168)` not `from_secs(7*24*60*60)`; `Duration::from_mins(1)` not `from_secs(60)`.
  - **Verified end-to-end:** this exact `Deadline` code + these constants compile **clean** under the repo's full lint table (`restriction = deny` + `pedantic` + the real allow-list). `std_instead_of_core`/`std_instead_of_alloc` **are** allow-listed, so `use std::time::Duration;` is fine.
  - **In TESTS**, the root `clippy.toml` sets `allow-expect-in-tests = true`, `allow-unwrap-in-tests = true`, `allow-panic-in-tests = true`, `allow-indexing-slicing-in-tests = true` — so `.expect(..)` in tests is fine. **`arithmetic_side_effects` is NOT test-exempt**, which is why the tests below use `checked_add(..).expect("no overflow")` rather than `base + dur`.

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
- Modify: `crates/edgezero-core/src/error.rs` (enum + constructors + 8 match sites + **3** matrix tests)
- Test: `crates/edgezero-core/src/error.rs` (colocated `#[cfg(test)]`)

**Interfaces:**
- Produces: `EdgeError::bad_gateway<S: Into<String>>(msg) -> Self` (502, kind `"bad_gateway"`), `EdgeError::gateway_timeout<S: Into<String>>(msg) -> Self` (504, kind `"gateway_timeout"`). JSON via existing `IntoResponse`: `{ "error": { "status", "kind", "message" } }` (no `field_path` for these two).

- [ ] **Step 1: Write the failing tests (table-driven, BOTH variants)**

The existing `#[cfg(test)] mod tests` already imports `StatusCode`, `CONTENT_TYPE`, `HeaderValue`, `str` and does `use super::*;`, and has a `parse_body(response) -> serde_json::Value` helper (error.rs:498). Add — **no new imports** (re-importing under `-D warnings` fails):

This code is **pre-wrapped to rustfmt's canonical form at the final nesting depth** (inside `mod tests` → `fn` → `for`). Written as one-liners, the array-of-tuples rows and the message-bearing `assert!` exceed `max_width = 100` once indented into the test module and rustfmt rewraps them — which would surface as a diff at the Task 3 `cargo fmt --all -- --check` gate. (Step 9's `cargo fmt` would rewrap them for you, but the plan shows the landed form.)

```rust
#[test]
fn bad_gateway_and_gateway_timeout_surface() {
    for (err, code, msg) in [
        (
            EdgeError::bad_gateway("upstream refused"),
            StatusCode::BAD_GATEWAY,
            "upstream refused",
        ),
        (
            EdgeError::gateway_timeout("deadline expired"),
            StatusCode::GATEWAY_TIMEOUT,
            "deadline expired",
        ),
    ] {
        assert_eq!(err.status(), code);
        assert_eq!(err.message(), msg);
        assert!(err.inner().is_none());
    }
}

#[test]
fn bad_gateway_and_gateway_timeout_json_shape() {
    for (err, code, kind, msg) in [
        (
            EdgeError::bad_gateway("nope"),
            502_u16,
            "bad_gateway",
            "nope",
        ),
        (
            EdgeError::gateway_timeout("late"),
            504_u16,
            "gateway_timeout",
            "late",
        ),
    ] {
        let response = err.into_response().expect("response");
        assert_eq!(response.status().as_u16(), code);
        let body_json = parse_body(response); // existing helper -> serde_json::Value
        assert_eq!(body_json["error"]["status"], code);
        assert_eq!(body_json["error"]["kind"], serde_json::Value::from(kind));
        assert_eq!(body_json["error"]["message"], serde_json::Value::from(msg));
        assert!(
            body_json["error"].get("field_path").is_none(),
            "502/504 carry no field_path"
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p edgezero-core bad_gateway` (single filter — matches both `bad_gateway_*` test fns) — Expected: FAIL to compile (`no variant or associated item named bad_gateway`).

- [ ] **Step 3: Add the two variants** in `pub enum EdgeError` — **ALPHABETICALLY, not appended.**

`clippy::arbitrary_source_item_ordering` is a denied restriction lint and it **does police enum-variant order** (verified: appending `BadGateway` after `Validation` errors with *"incorrect ordering of items (must be alphabetically ordered)"*). The existing variants are already alphabetical (`BadRequest, ConfigOutOfDate, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation`), so insert **in place**:
- `BadGateway` goes **before** `BadRequest` (BadG < BadR),
- `GatewayTimeout` goes **between** `ConfigOutOfDate` and `Internal` (C < G < I).

Resulting order: `BadGateway, BadRequest, ConfigOutOfDate, GatewayTimeout, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation`.

```rust
    /// Upstream or transport failure (DNS, TLS, connect, unreachable, or a
    /// non-timeout send failure). HTTP 502.
    #[error("{message}")]
    BadGateway { message: String },
    /// A wall-clock deadline or per-request timeout fired. HTTP 504.
    #[error("{message}")]
    GatewayTimeout { message: String },
```

- [ ] **Step 4: Add the constructors** in `impl EdgeError` — **also alphabetically** (the impl's fns are already ordered `bad_request, config_out_of_date, config_out_of_date_from_serde, inner, internal, kind_str, message, status, validation`): put `bad_gateway` **before** `bad_request`, and `gateway_timeout` **between** `config_out_of_date_from_serde` and `inner`.

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

- [ ] **Step 7: Extend ALL THREE existing per-variant matrix tests**

There are **three** existing per-variant tests in `error.rs`. Only `kind_strings_per_variant` is an *exhaustive* matrix (one row per variant); the other two are **subset** checks (they assert a property for a handful of variants). The new variants belong in all three — the two subset checks need the 502/504 rows so the new variants are actually covered, even though those tests were never exhaustive:

**(a) `kind_strings_per_variant` (:502) — the exhaustive matrix** — uses `assert_kind!($err, $expected_kind:literal, $expected_status:literal)`, and existing rows pass **suffixed** status literals (`assert_kind!(EdgeError::bad_request("x"), "bad_request", 400_u16);`). Match that form:

```rust
        assert_kind!(EdgeError::bad_gateway("x"), "bad_gateway", 502_u16);
        assert_kind!(EdgeError::gateway_timeout("x"), "gateway_timeout", 504_u16);
```

**(b) `retry_after_only_on_config_out_of_date` (:549) — a subset check, not exhaustive** — asserts **only** `ConfigOutOfDate` emits `Retry-After: 60`. 502/504 must **not**, so add:

```rust
        assert_retry_after!(EdgeError::bad_gateway("x"), false);
        assert_retry_after!(EdgeError::gateway_timeout("x"), false);
```

**(c) `field_path_only_on_config_out_of_date` (:569) — a subset check** (today it only exercises `BadRequest` + `ConfigOutOfDate`) — asserts `field_path` is absent for non-`ConfigOutOfDate` variants. Add the same two assertions for `bad_gateway` / `gateway_timeout` in that test's existing style (this overlaps the JSON-shape test in Step 1 — keep both; the matrix is the exhaustive per-variant guard, the Step 1 test is the focused contract).

- [ ] **Step 8: Run the new + matrix tests to verify they pass**

Run: `cargo test -p edgezero-core bad_gateway`, then `cargo test -p edgezero-core kind_strings_per_variant`, then `cargo test -p edgezero-core only_on_config_out_of_date` (one filter matches both the retry_after_* and field_path_* matrices).
Expected: PASS.

- [ ] **Step 9: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings && cargo test -p edgezero-core`
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

**Deadline semantics (matches spec §3.3.2 `deadline <= now => expired`):** a deadline whose instant is **exactly now** is **expired** — `is_expired()` is `true` and `remaining()` is `None` at equality, not `Some(0)`. A naive `checked_duration_since(now).is_none()` gets this wrong (it returns `Some(ZERO)` at equality), so the impl below uses `checked_duration_since(..).filter(|r| !r.is_zero())` — the zero case is filtered explicitly.

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
        assert_eq!(DEADLINE_FAR_FUTURE, Duration::from_hours(168));
        assert_eq!(BATCH_DISPATCH_SLACK_MAX, Duration::from_millis(25));
    }

    // EXACT + deterministic: every assertion pins BOTH the deadline instant and the
    // `now` it is compared against, via the pure `*_at(now)` helpers. No wall-clock
    // tolerance windows, no assumption about how fast the test resumes.

    #[test]
    fn deadline_before_now_is_expired() {
        let base = Instant::now();
        let past = Deadline::at_instant(base);
        let now = base
            .checked_add(Duration::from_secs(1))
            .expect("no overflow");
        assert!(past.is_expired_at(now));
        assert_eq!(past.remaining_at(now), None);
    }

    #[test]
    fn deadline_exactly_now_is_expired() {
        // The equality boundary: deadline instant == now. `deadline <= now` is
        // expired, but `checked_duration_since` returns Some(ZERO) here, so a naive
        // impl would wrongly report NOT expired.
        let base = Instant::now();
        let at_now = Deadline::at_instant(base);
        assert_eq!(
            at_now.remaining_at(base),
            None,
            "zero remaining is expired, not Some(0)"
        );
        assert!(
            at_now.is_expired_at(base),
            "a deadline exactly at now is expired"
        );
    }

    #[test]
    fn deadline_in_future_has_exact_remaining() {
        let base = Instant::now();
        let future = Deadline::at_instant(
            base.checked_add(Duration::from_mins(1))
                .expect("no overflow"),
        );
        assert!(!future.is_expired_at(base));
        // EXACT equality — both instants are explicit, so there is no elapsed-time slop.
        assert_eq!(future.remaining_at(base), Some(Duration::from_mins(1)));
    }

    #[test]
    fn after_clamps_duration_max_to_far_future() {
        // Prove the 7-DAY CLAMP via bounds on the resulting INSTANT (no second
        // now()-snapshot to race against).
        let before = Instant::now();
        let deadline = Deadline::after(Duration::MAX);
        let after = Instant::now();
        // `after()` computed `t0 + FAR_FUTURE` for some t0 in [before, after],
        // so the instant must land within [before+FAR_FUTURE, after+FAR_FUTURE].
        let lower = before
            .checked_add(DEADLINE_FAR_FUTURE)
            .expect("no overflow");
        let upper = after.checked_add(DEADLINE_FAR_FUTURE).expect("no overflow");
        assert!(deadline.instant() >= lower, "clamped below the 7-day bound");
        assert!(
            deadline.instant() <= upper,
            "Duration::MAX was NOT clamped to 7 days"
        );
    }

    #[test]
    fn instant_round_trips() {
        let base = Instant::now()
            .checked_add(Duration::from_secs(10))
            .expect("no overflow");
        assert_eq!(Deadline::at_instant(base).instant(), base);
    }
}
```

> The test snippet above is **pre-wrapped to rustfmt's canonical form** (the `.checked_add(..).expect(..)` chains and the message-bearing `assert!`/`assert_eq!` exceed `max_width = 100` at this nesting). Copy it verbatim and the Task 3 `cargo fmt --all -- --check` gate stays a no-op.

- [ ] **Step 2: Wire the module in and run to verify failure**

Add `pub mod time;` to `crates/edgezero-core/src/lib.rs` (alphabetical position among the `pub mod` lines).
Run: `cargo test -p edgezero-core --lib time::`
Expected: FAIL to compile (`cannot find value DEFAULT_NO_DEADLINE_BUDGET`, `cannot find type Deadline`).

- [ ] **Step 3: Implement constants + `Deadline`**

Prepend to `crates/edgezero-core/src/time.rs` (above `#[cfg(test)]`):

> **This code is written to pass the repo's strict Clippy gate.** The workspace sets
> `restriction = { level = "deny", priority = -1 }` (root `Cargo.toml`), and **none** of
> `missing_inline_in_public_items`, `min_ident_chars`, `arithmetic_side_effects`,
> `expect_used`, or `as_conversions` is allow-listed. Therefore: every public method
> carries **`#[inline]`** (matching the 14 existing `#[inline]`s in `error.rs`); no
> single-char idents (`d` → `duration`); and **no bare `-`/`+`** — all arithmetic is
> `checked_*`. The private `*_at(now)` helpers additionally make the logic **pure and
> deterministically testable** (no hidden `Instant::now()` inside the assertion).

```rust
use web_time::Instant;

/// Max adapter overhead tolerated before a fan-out slot fails closed.
pub const BATCH_DISPATCH_SLACK_MAX: Duration = Duration::from_millis(25);
/// Hard clamp on any caller-supplied duration, so construction cannot panic.
pub const DEADLINE_FAR_FUTURE: Duration = Duration::from_hours(168);
/// Budget applied when a request sets neither a timeout nor a deadline.
pub const DEFAULT_NO_DEADLINE_BUDGET: Duration = Duration::from_secs(30);

/// An absolute, copyable wall-clock deadline. A deadline at or before now is expired.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    /// Returns a deadline `now + min(duration, DEADLINE_FAR_FUTURE)`; never panics.
    #[inline]
    #[must_use]
    pub fn after(duration: Duration) -> Self {
        let now = Instant::now();
        let clamped = duration.min(DEADLINE_FAR_FUTURE);
        Deadline(now.checked_add(clamped).unwrap_or(now))
    }

    /// Constructs a deadline from an absolute instant.
    #[inline]
    #[must_use]
    pub fn at_instant(instant: Instant) -> Self {
        Deadline(instant)
    }

    /// Returns the absolute deadline instant.
    #[inline]
    #[must_use]
    pub fn instant(&self) -> Instant {
        self.0
    }

    /// Returns `true` once the deadline instant is at or before now.
    #[inline]
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Instant::now())
    }

    fn is_expired_at(&self, now: Instant) -> bool {
        self.remaining_at(now).is_none()
    }

    /// Returns the remaining time, or `None` once the deadline is reached or passed.
    #[inline]
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.remaining_at(Instant::now())
    }

    fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.0
            .checked_duration_since(now)
            .filter(|remaining| !remaining.is_zero())
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass** — Run: `cargo test -p edgezero-core --lib time::` — Expected: PASS (all six).

- [ ] **Step 5: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings && cargo test -p edgezero-core`
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
