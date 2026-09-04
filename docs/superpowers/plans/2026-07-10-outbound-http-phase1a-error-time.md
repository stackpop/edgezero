# Outbound HTTP — Phase 1a: `EdgeError` 502/504 + `time.rs` primitives

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Land the **additive, no-new-dependency on the current baseline** core primitives from the outbound-HTTP spec ([`2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md)): the `EdgeError::BadGateway`/`GatewayTimeout` variants (§7 error.rs) — **`GatewayTimeout` carries a typed `cause: BudgetSource`**, so the `BudgetSource` enum lands **in `error.rs`, Task 1** (NOT `time.rs` — Task 1 builds first and names it) — and the `edgezero-core::time` module's `Deadline` + budget constants. (Constant ownership spans sections, not just §3.3.1: `DEFAULT_NO_DEADLINE_BUDGET` / `DEADLINE_FAR_FUTURE` are §3.3.1, `BATCH_DISPATCH_SLACK_MAX` is §3.3.4/§4.3; `BudgetSource` is §3.3.2/§3.4.3 but lives in `error.rs`. Phase 1a lands the *types + constants*; their producer `dispatch_budget` is Phase 1b.) Neither touches the `proxy → outbound` rename or `Body`, so each task keeps `cargo test --workspace` green. **Scope caveat:** the per-task verification is a deliberate **local subset** (see Task 3 Scope) — it does not run the generated-project build, `app-demo`, or the per-adapter WASM matrices, so a green task is not a claim of full-CI readiness.

**Architecture:** `edgezero-core` only. Additive: new `EdgeError` variants (the enum is `#[non_exhaustive]`, but that does **not** relax exhaustiveness *inside the defining crate* — every exhaustive `match`, including the ones in the test module, must gain the two arms) and a brand-new `time` module. No adapter, CLI, or app-demo change. **`DispatchBudget` and `dispatch_budget` are BOTH deferred to Phase 1b** — the spec (§3.3.2) treats the carrier struct and its authoritative producer as one contract, and shipping a freely-constructible `DispatchBudget` without its producer invites misuse. Phase 1a lands `Deadline` + constants only.

**Round-55 scope alignment:** the master spec's corrected `Body::Stream` constructors,
Fastly buffered-upload caveat, Axum/Cloudflare upload-pull boundary checks, and Spin
exchange state machine are later outbound/adapter work. They do not alter any Phase 1a
task, file, API, or verification command. In particular, this plan must not opportunistically
change `Body`, `proxy`, or an adapter while landing the error/time primitives.

**Tech Stack:** Rust 1.95 (edition 2024), `thiserror`, `serde_json`, `web-time` (for `Instant`), `futures::executor::block_on` for async tests.

## Global Constraints (from the master design and phase index)

- **WASM-first:** no `tokio`/runtime deps; use `web-time::Instant`, not `std::time::Instant`. Core stays `default-features = false`.
- **Colocated tests** (`#[cfg(test)]` same file); async tests use `futures::executor::block_on`.
- **Verbatim constants:** `DEFAULT_NO_DEADLINE_BUDGET = 30 s`, `DEADLINE_FAR_FUTURE = 7 days`, `BATCH_DISPATCH_SLACK_MAX = 25 ms`.
- **CI gates must stay green:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-targets`; `cargo check --workspace --all-targets --features "fastly cloudflare spin"`; `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- **Verified against `crates/edgezero-core/src/error.rs` on `main`** (re-confirm with the compiler-driven Step 6 rather than trusting line numbers, which drift): `EdgeError` today has variants `BadRequest, ConfigOutOfDate, Internal, MethodNotAllowed, NotFound, NotImplemented, ServiceUnavailable, Validation`. The new arms below must be added to **nine** exhaustive matches — **five in `impl`**: `inner()`, `kind_str()`, `message()`, `status()`, `IntoResponse`'s `field_path_opt` — **and four in the test module**: the explicit `ConfigOutOfDate` matches in `config_out_of_date_constructor_round_trips`, `config_out_of_date_from_serde_extracts_path_and_message`, `config_out_of_date_from_serde_redacts_map_key_from_path_and_message`, and `config_out_of_date_from_serde_root_error_passes_through_sentinel`. Each has no `_` wildcard. The compiler-driven Step 6 remains the source of truth. Also **three** per-variant tests must gain rows for both new variants: `kind_strings_per_variant`, `retry_after_only_on_config_out_of_date`, and `field_path_only_on_config_out_of_date`. `web-time` presence is confirmed in Task 0.
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

Run: `rg -n 'web-time|web_time' crates/edgezero-core/Cargo.toml`
Expected: a line like `web-time = { workspace = true }`.

- [ ] **Step 2: If absent, STOP and update/re-review this plan**

Do not add the dependency as part of this task. The plan's locked premise is that
`web-time` already exists in both the workspace and `edgezero-core`; silently adding it
would contradict the no-new-dependency goal and turn Task 0 from verification into
implementation.

- [ ] **Step 3: Verify it compiles** — Run: `cargo check -p edgezero-core` — Expected: `Finished`.

---

### Task 1: `EdgeError::BadGateway` (502) + `GatewayTimeout` (504)

**Files:**
- Modify: `crates/edgezero-core/src/error.rs` (enum + constructors + **9 exhaustive matches: 5 impl + 4 test panic-arms**)
- Test: `crates/edgezero-core/src/error.rs` (colocated `#[cfg(test)]`)

**Interfaces:**
- Produces: `EdgeError::bad_gateway<S: Into<String>>(msg) -> Self` (502, kind `"bad_gateway"`), `EdgeError::gateway_timeout<S: Into<String>>(msg) -> Self` (504, kind `"gateway_timeout"`, `cause: BudgetSource::Unspecified`), and `gateway_timeout_caused(msg, cause)`. The `GatewayTimeout` variant carries a typed `cause: BudgetSource` (consumer reads it by matching the variant; it is a Rust-side field, **not** serialized — the JSON shape is unchanged). JSON via existing `IntoResponse`: `{ "error": { "status", "kind", "message" } }` (no `field_path`, no `cause` in JSON, for these two).

- [ ] **Step 1: Write the failing tests (table-driven, BOTH variants)**

The existing `#[cfg(test)] mod tests` already imports `StatusCode`, `CONTENT_TYPE`, `HeaderValue`, `str` and does `use super::*;`, and has a `parse_body(response) -> serde_json::Value` helper (`tests::parse_body`). Add — **no new imports** (re-importing under `-D warnings` fails):

This code is **pre-wrapped to rustfmt's canonical form at the final nesting depth** (inside `mod tests` → `fn` → `for`). Written as one-liners, the array-of-tuples rows and the message-bearing `assert!` exceed `max_width = 100` once indented into the test module and rustfmt rewraps them — which would surface as a diff at the Task 3 `cargo fmt --all -- --check` gate. (Step 8's `cargo fmt` would rewrap them for you, but the plan shows the landed form.)

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
        // Display must render the new variants (not just the pre-existing ones). Assert
        // the message is present rather than pinning an exact format, so this survives a
        // format tweak while still proving `Display` covers `BadGateway`/`GatewayTimeout`.
        assert!(err.to_string().contains(msg));
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
        // Every OTHER cause too — wire-isolation must hold for ALL FOUR BudgetSource
        // values, so a conditional serializer cannot leak `cause` for any of them
        // (BatchDeadline / Default / PerCallTimeout / Unspecified are all covered).
        (
            EdgeError::gateway_timeout_caused("late", BudgetSource::PerCallTimeout),
            504_u16,
            "gateway_timeout",
            "late",
        ),
        (
            EdgeError::gateway_timeout_caused("late", BudgetSource::BatchDeadline),
            504_u16,
            "gateway_timeout",
            "late",
        ),
        (
            EdgeError::gateway_timeout_caused("late", BudgetSource::Default),
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
        // The typed `cause` is a Rust-side field, NOT serialized — the JSON must NOT leak it.
        assert!(
            body_json["error"].get("cause").is_none(),
            "cause is not part of the wire shape"
        );
    }
}

// The typed timeout-attribution contract (§3.3.2 / §3.4.3). WITHOUT these, a mis-wired
// constructor that dropped the cause, or that always stored the wrong variant, would pass
// the suite. These are the exact assertions compile-verified in the errsurface scaffold.
#[test]
fn bare_gateway_timeout_is_unspecified() {
    // `let-else`, NOT `match … { other => panic! }`: a catch-all arm over the
    // multi-variant `EdgeError` trips the denied `clippy::wildcard_enum_match_arm`
    // (compile-verified against the real enum shape under edition 2024). `let-else`
    // has no wildcard arm.
    let EdgeError::GatewayTimeout { cause, .. } = EdgeError::gateway_timeout("x") else {
        panic!("expected GatewayTimeout");
    };
    assert_eq!(cause, BudgetSource::Unspecified);
}
#[test]
fn gateway_timeout_caused_preserves_cause() {
    // Loop var is `expected` (NOT a single char — `min_ident_chars`). **Includes
    // `Unspecified`** so an impl that special-cased the bare constructor's cause is
    // caught. `let-else` again (no `wildcard_enum_match_arm`). rustfmt-canonical
    // (edition 2024) wrapped form, verified.
    for expected in [
        BudgetSource::BatchDeadline,
        BudgetSource::Default,
        BudgetSource::PerCallTimeout,
        BudgetSource::Unspecified,
    ] {
        let EdgeError::GatewayTimeout { cause, .. } =
            EdgeError::gateway_timeout_caused("x", expected)
        else {
            panic!("expected GatewayTimeout");
        };
        assert_eq!(cause, expected);
    }
}
```

In the **same red test edit**, extend all three existing per-variant matrices. These rows
reference the not-yet-created constructors, just like the focused tests above, so they belong
in the same compile-red state rather than being deferred until after implementation. The
following are three labeled insertion fragments for their named existing test functions,
not one contiguous block to paste at a single location:

```rust
        // `kind_strings_per_variant`
        assert_kind!(EdgeError::bad_gateway("x"), "bad_gateway", 502_u16);
        assert_kind!(EdgeError::gateway_timeout("x"), "gateway_timeout", 504_u16);

        // `retry_after_only_on_config_out_of_date`
        assert_retry_after!(EdgeError::bad_gateway("x"), false);
        assert_retry_after!(EdgeError::gateway_timeout("x"), false);

        // `field_path_only_on_config_out_of_date` (there is no helper macro here)
        for err in [
            EdgeError::bad_gateway("x"),
            EdgeError::gateway_timeout("x"),
        ] {
            let body = parse_body(err.into_response().expect("response"));
            assert!(
                body["error"].get("field_path").is_none(),
                "field_path should be absent for gateway errors"
            );
        }
```

Only `kind_strings_per_variant` is exhaustive today; the other two are subset checks. Add
the rows to all three anyway so 502/504 are pinned for kind/status, absence of
`Retry-After`, and absence of `field_path` from the first red run onward.

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p edgezero-core gateway_timeout` (single filter — **substring `gateway_timeout` matches ALL FOUR** new fns: `bad_gateway_and_gateway_timeout_surface`, `bad_gateway_and_gateway_timeout_json_shape`, `bare_gateway_timeout_is_unspecified`, `gateway_timeout_caused_preserves_cause`, so the red/green loop actually exercises the attribution tests, not just the surface ones) — Expected: FAIL to compile (`no variant or associated item named bad_gateway`/`gateway_timeout`).

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
    /// Carries typed provenance naming which configured budget input selected the
    /// effective deadline. This is not the physical timer phase, proof that the named
    /// deadline elapsed, or a retry/batch-abandonment decision.
    #[error("{message}")]
    GatewayTimeout { message: String, cause: BudgetSource },
```

`BudgetSource` is defined **in `error.rs` in THIS task (Task 1)**, NOT in `time.rs` (Task 2).
That ordering is load-bearing: Task 1 lands/commits/builds **before** Task 2, and Task 1's
`GatewayTimeout` names `BudgetSource`, so a `time`-module home would make the standalone
Task-1 commit fail to compile. Define it immediately **before `EdgeError`** in `error.rs`
so the denied item-order lint also sees `BudgetSource` (B) before `EdgeError` (E);
`time.rs` (Task 2) and `dispatch_budget` (Phase 1b) later
`use crate::error::BudgetSource;`.

The **derives and variant order are compile-verified** (a throwaway crate under the repo's
`arbitrary_source_item_ordering` deny + `cargo check`):
```rust
// Debug: EdgeError derives Debug and contains `cause`.
// Clone + Copy: budget/error carriers pass provenance by value.
// PartialEq + Eq: the contract tests below assert `cause == BudgetSource::Unspecified` etc.
// Variants ALPHABETICAL: `arbitrary_source_item_ordering` (denied) rejects any other order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// non_exhaustive: public enum that must be able to gain a future budget-input source
// without a breaking change. Verified: intra-crate exhaustive matches
// still compile clean under the denied `wildcard_enum_match_arm`.
#[non_exhaustive]
pub enum BudgetSource {
    BatchDeadline,
    Default,
    PerCallTimeout,
    Unspecified,
}
```

- [ ] **Step 4: Add the constructors** in `impl EdgeError` — **also alphabetically** (the impl's fns are already ordered `bad_request, config_out_of_date, config_out_of_date_from_serde, inner, internal, kind_str, message, status, validation`): put `bad_gateway` **before** `bad_request`, and `gateway_timeout` **between** `config_out_of_date_from_serde` and `inner`.

```rust
    #[inline]
    pub fn bad_gateway<S: Into<String>>(message: S) -> Self {
        EdgeError::BadGateway {
            message: message.into(),
        }
    }
    #[inline]
    pub fn gateway_timeout<S: Into<String>>(message: S) -> Self {
        EdgeError::GatewayTimeout {
            message: message.into(),
            cause: BudgetSource::Unspecified,
        }
    }
    #[inline]
    pub fn gateway_timeout_caused<S: Into<String>>(message: S, cause: BudgetSource) -> Self {
        EdgeError::GatewayTimeout {
            message: message.into(),
            cause,
        }
    }
```

(All three literals are shown in rustfmt's canonical **split** form — verified with
`rustfmt --edition 2024`. **Even the single-field `BadGateway`** wraps: rustfmt expands a
struct literal in a function-body tail position across lines regardless of field count, so a
one-liner `BadGateway { message: message.into() }` would fail the `cargo fmt --check` gate.
An earlier note wrongly claimed the single-field form stays inline — it does not.)

- [ ] **Step 5: Update ALL nine exhaustive matches (crate won't compile until every one is done)**

`impl` sites:
- `kind_str()` — add `EdgeError::BadGateway { .. } => "bad_gateway",` and `EdgeError::GatewayTimeout { .. } => "gateway_timeout",`
- `status()` — add `EdgeError::BadGateway { .. } => StatusCode::BAD_GATEWAY,` and `EdgeError::GatewayTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,`
- `message()` — add `EdgeError::BadGateway { message }` and **`EdgeError::GatewayTimeout { message, .. }`** (the `..` ignores the `cause` field — a bare `{ message }` pattern won't compile now that the variant has a second field) to the "clone the `message`" arm.
- `inner()` — add both variants to the `=> None` arm list.
- `IntoResponse::into_response`'s `field_path_opt` match — add both variants to the `=> None` arm list.

**Test-module sites (these have explicit panic-arms listing every non-`ConfigOutOfDate` variant, NO `_`):** in each of the **four** `match err { … }` blocks (the fourth is the root-error sentinel test; Step 6's compiler-driven `E0004` sweep is the source of truth for their exact locations), add `| EdgeError::BadGateway { .. } | EdgeError::GatewayTimeout { .. }` to the `=> panic!("expected ConfigOutOfDate")` arm.

- [ ] **Step 6: Compiler-driven catch — build and fix any remaining non-exhaustive match**

Run: `cargo build -p edgezero-core --tests`
If it reports `E0004 non-exhaustive patterns` anywhere, add the two arms at that exact site (the compiler prints the file:line). Repeat until it builds. Expected end state: builds clean.

- [ ] **Step 7: Run the new + matrix tests to verify they pass**

Run: `cargo test -p edgezero-core gateway_timeout` (the **same filter as the red Step 2** — matches all four surface + attribution fns, so the green step exercises the SAME set that went red, including `bare_gateway_timeout_is_unspecified` and `gateway_timeout_caused_preserves_cause`; a `bad_gateway` filter would silently skip the two cause tests), then `cargo test -p edgezero-core kind_strings_per_variant`, then `cargo test -p edgezero-core only_on_config_out_of_date` (one filter matches both the retry_after_* and field_path_* matrices).
Expected: PASS.

- [ ] **Step 8: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 9: Commit**

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

    // The public API promises `Deadline: Copy` (adapters copy it into per-slot budgets
    // rather than borrowing). Pin it at COMPILE time — a later `#[derive]` edit that
    // drops `Copy` must fail the build, not silently change the API.
    #[test]
    fn deadline_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Deadline>();
    }

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

    // Public smoke tests: the live-clock wrappers (which call Instant::now()) actually
    // delegate to the pure *_at helpers. The *_at tests above cover exact arithmetic;
    // these guard the public surface AND that `after` honours its duration argument.
    #[test]
    fn public_remaining_and_is_expired_smoke() {
        // Tight instant-bracket around `after`: the resulting deadline MUST land in
        // [before + 1h, after + 1h]. Any mutation that perturbs the duration — dropping
        // 30s, or clamping to DEADLINE_FAR_FUTURE = 7 days — falls outside the bracket.
        // (A loose "remaining is (59 min, 1 h]" check would survive a 30s-off mutant.)
        let before = Instant::now();
        let far = Deadline::after(Duration::from_hours(1));
        let after = Instant::now();
        assert!(!far.is_expired());
        // rustfmt-canonical SPLIT form (verified with `rustfmt --edition 2024`): the
        // `.checked_add(..).expect(..)` chain exceeds `chain_width = 60`, so rustfmt
        // breaks it across lines — a one-liner here would fail `cargo fmt --check`.
        let lo = before
            .checked_add(Duration::from_hours(1))
            .expect("no overflow");
        let hi = after
            .checked_add(Duration::from_hours(1))
            .expect("no overflow");
        assert!(
            far.instant() >= lo && far.instant() <= hi,
            "after() must land exactly `now + duration`"
        );
        assert!(far.remaining().is_some());
        // `after(ZERO)` yields `now`; by the time is_expired()/remaining() read a later
        // Instant::now(), it is at-or-past — no `checked_sub` (would underflow a fresh
        // WASM Instant near its epoch).
        let now_deadline = Deadline::after(Duration::ZERO);
        assert!(now_deadline.is_expired());
        assert_eq!(now_deadline.remaining(), None);
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

> The test snippet above is **pre-wrapped to rustfmt's canonical form** — verified by running `rustfmt --edition 2024` on it, not by eyeballing width. The `.checked_add(..).expect(..)` chains split because they exceed **`chain_width = 60`** (NOT `max_width = 100` — an earlier note wrongly cited the 100 limit; the chain heuristic is the binding one, which is why `lo`/`hi` are multi-line here even though they'd fit on one line width-wise). Copy it verbatim and the Task 3 `cargo fmt --all -- --check` gate stays a no-op.

- [ ] **Step 2: Wire the module in and run to verify failure**

Add `pub mod time;` to `crates/edgezero-core/src/lib.rs` (alphabetical position among the `pub mod` lines).
Run: `cargo test -p edgezero-core --lib time::`
Expected: FAIL to compile (`cannot find value DEFAULT_NO_DEADLINE_BUDGET`, `cannot find type Deadline`).

- [ ] **Step 3: Implement constants + `Deadline`**

Insert the implementation between the existing `use std::time::Duration;` and
`#[cfg(test)]`. Add `use web_time::Instant;` after the standard-library import; do not
duplicate or reorder the existing `Duration` import:

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

/// An absolute, copyable monotonic deadline. A deadline at or before now is expired.
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

- [ ] **Step 4: Run the tests to verify they pass** — Run: `cargo test -p edgezero-core --lib time::` — Expected: PASS (all eight).

- [ ] **Step 5: Format, lint, full-crate test**

Run: `cargo fmt -p edgezero-core && cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings && cargo test -p edgezero-core`
Expected: clean, all green.

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-core/src/time.rs crates/edgezero-core/src/lib.rs
git commit -m "feat(core): add time module (Deadline + budget constants)"
```

---

### Task 3: Core CI-gate verification (the five CLAUDE.md gates)

**Files:** none (verification only). Run from the repo root.

**Scope:** Phase 1a is **additive, core-only** (new `EdgeError` variants + a new `time` module; no adapter, CLI, template, or `app-demo` change), so this task runs the **five CLAUDE.md gates** over the workspace **plus one core-only wasm32-unknown-unknown check** (Step 5). It deliberately does **not** run the generated-project build, the `examples/app-demo` build, or the per-adapter WASM test/check/clippy matrices. **This is a risk-reduced local subset, NOT a proof those are unaffected:** every one of them compiles `edgezero-core`, so a core change *could* in principle break them (an unexpected new export collision, a feature-gate interaction). The judgement is that a purely additive `EdgeError`-variant + new-`time`-module change is very unlikely to, and **full CI runs all of them on the PR regardless** — so Task 3 is a fast local gate, and CI is the actual backstop. If you want local certainty, run the full workspace + `examples/app-demo` builds too; otherwise rely on CI. Phases that touch adapters/templates/app-demo DO add those locally. The one WASM target that is *not* redundant here is `wasm32-unknown-unknown`: `web-time::Instant` resolves to its JS `Date`/`performance.now()` path there, whereas the Spin `wasm32-wasip2` gate uses the WASI clock — so the new `time` module's `web-time` dependency must be compiled on that target too.

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

- [ ] **Step 5: Core wasm32-unknown-unknown check (web-time JS path)**

Run: `cargo check -p edgezero-core --target wasm32-unknown-unknown`
Expected: `Finished`. (Compiles the new `time` module against `web-time`'s browser/JS clock backend — the Cloudflare target — which the Spin `wasip2` gate does not exercise. Core-only, so no adapter is pulled in.)

(Steps 1–4 collectively execute all five repo CI commands from `CLAUDE.md`; Step 5 adds the `wasm32-unknown-unknown` core check. Do not skip the wasm targets — they are the ones most likely to catch an accidental `std::time` / non-WASM import.)

---

## Self-Review

- **Spec coverage:** Task 1 = §7 error.rs (both variants, full surface, JSON shape for **both**, matrix test); Task 2 = §§3.3.1/3.3.4 (Deadline and all three constants). `DispatchBudget` + `dispatch_budget()` (§3.3.2) are deferred **together** to Phase 1b — a stated sequencing boundary, not a gap.
- **Compile-safety (the class of bug a prior review caught):** the nine exhaustive matches (5 impl + 4 test panic-arms) are enumerated *and* backed by a compiler-driven catch step; focused tests and all six matrix rows enter the same compile-red edit; the `cargo test` single-filter rule is applied; `is_expired_at` treats a **zero** remaining as expired (`remaining_at` filters out a zero `Duration`), so a deadline exactly at now reads as expired.
- **No placeholders / no flaky tests:** every step has exact code, paths, single-filter commands, expected output; timing tests are bounded by explicit `at_instant` instants (no `now() - 1s` underflow, no wide tolerance windows), and the clamp test proves the 7-day bound.

## Next (not this plan; each is its own plan, NOT one atomic step)

Phase 1b must respect the producer's type dependency: `dispatch_budget(&OutboundRequest, ..)`
cannot land before `OutboundRequest` and its private `budget_inputs()` accessor exist. The
next plan must either (1) land `OutboundRequest`/`ResponseMode`, canonical URI accessors,
`validate_for_dispatch`, `BudgetInputs`, `DispatchBudget`, and `dispatch_budget` in one
buildable slice, or (2) land the request type/accessor in an earlier buildable slice and the
budget carrier/producer immediately after it. `OutboundResponse`, the `Body::Stream` error
change, and the `proxy → outbound` rename can then be sequenced around the four-adapter
atomic migration, but no slice may name a type that does not yet exist.
