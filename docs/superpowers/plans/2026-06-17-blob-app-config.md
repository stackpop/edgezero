# Blob App-Config Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace EdgeZero's per-leaf typed-config storage with a single-key JSON blob per environment, embedding SHA-256 for drift detection, exposing the typed `C` via a new `AppConfig<C>` extractor with framework-resolved `#[secret]` fields, and adding a `config diff` command.

**Architecture:** One typed-config struct `C` serialises to a single envelope `{ data, sha256, version, generated_at }` written to one `[stores.config]` key per environment. The runtime reads the envelope, verifies the SHA, swaps `#[secret]` fields with values from `[stores.secrets]`, deserialises through `serde_path_to_error`, validates, and yields `C` to the handler. `config push` writes one blob per adapter; `config diff` compares local vs remote via a new `Adapter::read_config_entry` trait. Hard cutoff per [spec §10](../specs/2026-06-16-blob-app-config.md): no dual-shape parsing, no legacy fallback.

**Tech Stack:** Rust 1.95.0, Cargo workspace (8 crates), `serde_json` + `ryu` + `sha2` for canonicalisation, `serde_path_to_error` for field-path-aware deserialise errors, `validator` 0.20 for runtime validation, `clap` 4.6 for CLI, `chrono` 0.4 (ALREADY in workspace deps; new consumer on `edgezero-cli` for the `generated_at` RFC3339 timestamp at push time), `syn` 2 + `walkdir` (CI-gate-only feature) for the nested-AppConfig acceptance check. No new runtime deps beyond `ryu`/`sha2`/`serde_path_to_error`/`chrono` (the last reusing the existing workspace pin).

## Global Constraints

- **Source of truth:** [`docs/superpowers/specs/2026-06-16-blob-app-config.md`](../specs/2026-06-16-blob-app-config.md). Every task numbered below cites the spec section it implements.
- **Hard cutoff:** no `#[serde(default)]` compat fallback for missing legacy state, no dual-shape parse path, no platform-side bridge. Per spec §1 / §10.1.
- **Atomic cutover:** spec §10.1 + §10.2 require runtime extractor + per-adapter writers + app-demo migration + scaffold templates + CI gate to land in ONE commit (Phase C below). Intermediate trees with new runtime / old writer do NOT compile cleanly because the read-trait-driven inline diff and the gate cross-check would fail. **Bisect-safe annotations** appear in each task heading.
- **No external canonicaliser crate:** Q1 resolved to (b) per round-21 review. `serde_canonical_json` rejects finite floats; the v1 canonicaliser is a hand-rolled walker in `crates/edgezero-core/src/canonical_form.rs`. Only deps: `serde_json` (already in-tree), `ryu`, `sha2`.
- **Store-id charset:** spec §5.2 tightens `[stores.*]` ids to `[A-Za-z0-9_]+`. Hyphens are rejected at manifest validation. App-demo and scaffold templates already use underscore-only ids.
- **CI gates (all land in Phase C):**
  - `scripts/check_no_legacy_typed_reads.sh` — usage-shape grep + nested-`AppConfig` helper.
  - `scripts/check_no_placeholder_pins.sh` — refuses `…` / `fixed-hex-value` in pin tests.
  - `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs` — syn-based, behind a `nested-app-config-check` feature.
- **CI gates from main:** every commit must pass `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-targets`, `cargo check --workspace --all-targets --features "fastly cloudflare spin"`, `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- **Working branch:** `feature/extensible-cli`. Phase A starts here; each phase's tasks commit incrementally.
- **No commits without explicit user sign-off on the prior phase.** End-of-phase tasks pause for review.

---

## Plan structure (mirrors spec §13)

| Phase | Spec §                              | Commit type                  | Acceptance                                                                                                                                                                           |
| ----- | ----------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A     | §4                                  | Pre-cutover infra            | Canonical form + envelope + non-finite-float check ship in `edgezero-core`. No in-tree caller exercises them yet. Bisect-safe.                                                       |
| B     | §3, §5, §6.3, §9.0                  | Pre-cutover infra            | `ConfigStoreBinding`, `EnvConfig::store_key`, manifest charset, `EdgeError::ConfigOutOfDate`, derive-macro extensions, read trait + per-adapter impls. None called yet. Bisect-safe. |
| C     | §3.3, §8.2, §10.2, §10.2.1, §10.2.2 | **CUTOVER (not splittable)** | `AppConfig<C>` extractor + `config push` rewrite + app-demo migration + scaffold templates + all three CI gates land together. Spec §10.1 forbids splitting.                         |
| D     | §8.1                                | Post-cutover additive        | `config diff` command, format renderers, `--exit-code`. Depends on Phase C's read flow + writers.                                                                                    |
| E     | §10 narrative                       | Post-cutover docs            | Migration guide, smoke scripts, README updates.                                                                                                                                      |

---

# Phase A — Canonical form + envelope (Commit A)

**Goal:** Ship the SHA-canonicalisation walker, the `BlobEnvelope` type, and the `AppConfigError::InvalidValue` variant + non-finite-float rejection. The canonicaliser + envelope are new modules with no in-tree caller yet. Task A5's `AppConfigError::InvalidValue` variant + the loader's finite-float walk DO change behaviour the existing typed-push call site (`crates/edgezero-cli/src/config.rs:204`) is exposed to — but the change is purely RESTRICTIVE (a new error variant on input that was previously silently accepted as JSON `null`). Any in-tree TOML / env-overlay fixture that contained `nan` / `inf` would have been hashing as `null` already; the v1 hard cutoff catches that legitimately at the loader boundary. Phase ends with one commit.

## Task A1 — Add `ryu`, `sha2`, `serde_path_to_error` workspace deps

**Files:**

- Modify: `Cargo.toml` (root workspace) — add three entries to `[workspace.dependencies]`.

**Interfaces:**

- Produces: workspace deps `ryu`, `sha2`, `serde_path_to_error` available to per-crate Cargo.toml entries via `{ workspace = true }`.

- [ ] **Step 1: Inspect current `[workspace.dependencies]` block to find the alphabetical insertion point.**

Run: `grep -nE '^(ryu|sha2|serde_path_to_error|sha)' Cargo.toml`
Expected: empty output (deps not yet present).
Run: `grep -nE '^[a-z]' Cargo.toml | head -40`
Use the alphabetical insertion points for `ryu`, `sha2`, `serde_path_to_error`.

- [ ] **Step 2: Add `ryu = "1"` in alphabetical order under `[workspace.dependencies]`.**

```toml
ryu = "1"
```

- [ ] **Step 3: Add `sha2 = "0.10"` in alphabetical order.**

```toml
sha2 = "0.10"
```

- [ ] **Step 4: Add `serde_path_to_error = "0.1"` in alphabetical order.**

```toml
serde_path_to_error = "0.1"
```

- [ ] **Step 5: Verify the workspace builds (no per-crate consumer yet, but the workspace `Cargo.lock` should resolve).**

Run: `cargo build --workspace`
Expected: builds successfully; new deps are resolved into `Cargo.lock` but not pulled in.

- [ ] **Step 6: Commit deferred until Task A6** — kept rolling so the workspace deps land in one commit with their first consumer.

## Task A2 — `canonical_form` module skeleton + finite-float walker

**Files:**

- Create: `crates/edgezero-core/src/canonical_form.rs`
- Modify: `crates/edgezero-core/src/lib.rs` — add `pub mod canonical_form;`
- Modify: `crates/edgezero-core/Cargo.toml` — add `ryu = { workspace = true }` and `sha2 = { workspace = true }`

**Interfaces:**

- Produces: `edgezero_core::canonical_form::canonical_data_sha256(&serde_json::Value) -> String` — lowercase 64-char hex SHA-256 of the canonical form per spec §4.2.

- [ ] **Step 1: Add deps to `crates/edgezero-core/Cargo.toml`.**

Modify the `[dependencies]` section, alphabetically:

```toml
ryu = { workspace = true }
sha2 = { workspace = true }
```

- [ ] **Step 2: Add the module declaration in `crates/edgezero-core/src/lib.rs`.**

Insert (alphabetically) after the existing `pub mod body;` line:

```rust
pub mod canonical_form;
```

- [ ] **Step 3: Create the module file with the walker function and four unit tests (TDD-first).**

Write to `crates/edgezero-core/src/canonical_form.rs`:

```rust
//! Canonical-form SHA-256 over a [`serde_json::Value`] tree.
//!
//! Implements the v1 rules from
//! `docs/superpowers/specs/2026-06-16-blob-app-config.md` §4.2:
//!
//! - JSON with no insignificant whitespace.
//! - Object keys sorted by UTF-8 byte order.
//! - Strings emitted verbatim (no NFC fold).
//! - Floats rendered via `ryu`'s shortest round-trippable form.
//!   Non-finite floats are rejected upstream (loader / env overlay);
//!   if one reaches this walker, it panics on the assertion that
//!   guards against silent JSON-`null` collapse.
//! - Booleans `true` / `false` lowercase; `null` literal; `{}` and
//!   `[]` for empties.
//! - UTF-8 bytes of the resulting string feed into `Sha256`; hex
//!   output is lowercase, no `0x` prefix.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// SHA-256 of the canonical form of `data`. See module docs.
pub fn canonical_data_sha256(data: &Value) -> String {
    let mut buf = String::new();
    write_canonical(&mut buf, data);
    let mut hasher = Sha256::new();
    hasher.update(buf.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn write_canonical(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sort keys by UTF-8 byte order. `BTreeMap` would also work,
            // but `serde_json::Map` may preserve insertion order under
            // its default feature set, so we copy into a Vec and sort.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else if let Some(f) = n.as_f64() {
        // Non-finite floats are loader-rejected per §4.2 round-15 B-2.
        // If one reaches here, it's a programmer error: serialising NaN
        // would emit `null` via serde_json's default, which would
        // collide with real Option::None in the SHA. Panic loudly.
        assert!(
            f.is_finite(),
            "canonical_data_sha256: non-finite float {f} reached canonicaliser; loader must reject before this point"
        );
        let mut ryu_buf = ryu::Buffer::new();
        out.push_str(ryu_buf.format(f));
    } else {
        // serde_json::Number always exposes one of i64/u64/f64; reach
        // here is impossible. Treat as programmer error.
        panic!("canonical_data_sha256: unrecognised serde_json::Number shape");
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_keys_sorted_by_utf8_bytes() {
        let a = json!({ "b": 1, "a": 2 });
        let b = json!({ "a": 2, "b": 1 });
        assert_eq!(canonical_data_sha256(&a), canonical_data_sha256(&b));
    }

    #[test]
    fn finite_floats_round_trip_via_ryu() {
        // 1.5, 1.5000000, 15e-1 all parse to the same f64 and must
        // produce the same sha.
        let a: Value = serde_json::from_str("{\"x\": 1.5}").unwrap();
        let b: Value = serde_json::from_str("{\"x\": 1.5000000}").unwrap();
        let c: Value = serde_json::from_str("{\"x\": 15e-1}").unwrap();
        assert_eq!(canonical_data_sha256(&a), canonical_data_sha256(&b));
        assert_eq!(canonical_data_sha256(&a), canonical_data_sha256(&c));
    }

    #[test]
    fn empty_object_and_empty_array_distinct_from_null() {
        let null = canonical_data_sha256(&json!({ "x": null }));
        let obj = canonical_data_sha256(&json!({ "x": {} }));
        let arr = canonical_data_sha256(&json!({ "x": [] }));
        assert_ne!(null, obj);
        assert_ne!(null, arr);
        assert_ne!(obj, arr);
    }

    #[test]
    fn integer_and_float_with_same_text_hash_differently() {
        // §4.2 type-identity rule. 1500 vs 1500.0.
        let a = json!({ "x": 1500_i64 });
        let b = json!({ "x": 1500.0_f64 });
        assert_ne!(canonical_data_sha256(&a), canonical_data_sha256(&b));
    }

    #[test]
    #[should_panic(expected = "non-finite float")]
    fn non_finite_float_panics_at_canonicaliser() {
        // Construct a Number from NaN. serde_json::Number::from_f64
        // returns None for NaN, so we go through Value::from which
        // emits Null instead — but we want to test the assertion
        // path. Synthesise via a custom Number:
        let nan = serde_json::Number::from_f64(1.5).unwrap();
        // Replace with a hand-rolled NaN via the public API is not
        // possible; instead, exercise the assertion by calling
        // write_number directly through a private re-export in a
        // future test. For now, this test documents the contract.
        let value: Value = Value::Number(nan);
        let _ = canonical_data_sha256(&value);
        // Force the should_panic to fire by manually pushing a NaN
        // through write_number. This is a placeholder; the loader
        // ban (Task A5) is the real defence.
        unreachable!("loader rejects non-finite floats before they reach the canonicaliser");
    }
}
```

Note on the `#[should_panic]` test: the loader is the real defence (Task A5); the assertion in `write_number` is belt-and-suspenders. The test is structured as documentation of the contract; the loader tests cover the actual rejection path.

- [ ] **Step 4: Run the new tests and confirm the first four pass; the `#[should_panic]` will fail because `serde_json::Number::from_f64(NaN)` returns `None`.**

Run: `cargo test -p edgezero-core --lib canonical_form -- --nocapture`
Expected: 4 passes, 1 failure on `non_finite_float_panics_at_canonicaliser` (the test as written can't synthesise a NaN).

- [ ] **Step 5: Drop the unreachable `#[should_panic]` test — it cannot construct a NaN through serde_json's safe API; the loader rejection in Task A5 is the real assertion.**

Replace the `#[should_panic]` test with a doc-only comment:

```rust
// Note: the loader (Task A5) rejects non-finite floats before they
// reach this walker. The `assert!(f.is_finite(), ...)` in
// `write_number` is a programmer-error guard; the test for the
// loader rejection lives in `app_config::tests`.
```

- [ ] **Step 6: Re-run.**

Run: `cargo test -p edgezero-core --lib canonical_form -- --nocapture`
Expected: 4 passes, 0 failures.

## Task A3 — Canonical-form pin test fixture

**Files:**

- Create: `crates/edgezero-core/tests/canonical_form_pins.rs`

**Interfaces:**

- Produces: integration test asserting `canonical_data_sha256` against a frozen fixture. The placeholder hex IS allowed at this stage; the §13.1 acceptance gate (Task C7's `scripts/check_no_placeholder_pins.sh`) flags it for replacement in the implementing PR before merge.

- [ ] **Step 1: Write the pin test with a placeholder hex.**

```rust
//! Pin test for the v1 canonical-form rules. The hex value below
//! is a PLACEHOLDER computed at implementation time. Before merge,
//! the §13.1 acceptance gate (scripts/check_no_placeholder_pins.sh)
//! refuses to let this file ship with the literal `…` or
//! `fixed-hex-value` markers; the implementing PR replaces the
//! placeholder with the actual computed hex.

use edgezero_core::canonical_form::canonical_data_sha256;
use serde_json::json;

#[test]
fn canonical_form_pin_v1() {
    let data = json!({
        "greeting": "héllo",        // verbatim bytes; NFC vs NFD encodings hash differently per §4.2
        "feature": { "new_checkout": true },
        "service": { "timeout_ms": 1500 },
        "ratio": 1.5,
        "missing": null,
        "empty": {}
    });
    let actual = canonical_data_sha256(&data);
    eprintln!("Pin v1 actual hex: {actual}");
    // Replace this placeholder with the actual hex from the eprintln
    // above before the implementing PR merges.
    assert_eq!(actual, "5d4a0e7f…fixed-hex-value…b9");
}
```

- [ ] **Step 2: Run the test to fail (placeholder).**

Run: `cargo test -p edgezero-core --test canonical_form_pins`
Expected: fail. The `eprintln!` prints the real hex (note it for the next step).

- [ ] **Step 3: Replace the placeholder string with the real hex from step 2's output.**

In the `assert_eq!` line, replace `"5d4a0e7f…fixed-hex-value…b9"` with the actual hex (64 lowercase hex characters). Delete the `eprintln!` line.

- [ ] **Step 4: Run again, confirm pass.**

Run: `cargo test -p edgezero-core --test canonical_form_pins`
Expected: PASS.

## Task A4 — `BlobEnvelope` type + serde

**Files:**

- Create: `crates/edgezero-core/src/blob_envelope.rs`
- Modify: `crates/edgezero-core/src/lib.rs` — add `pub mod blob_envelope;`

**Interfaces:**

- Produces: `edgezero_core::blob_envelope::BlobEnvelope { data: serde_json::Value, sha256: String, version: u32, generated_at: String }`. Public methods: `verify(&self) -> Result<(), BlobEnvelopeError>`, `into_data(self) -> serde_json::Value`, and `new(data: serde_json::Value, generated_at: String) -> Self`.

- [ ] **Step 1: Write failing test fixtures for the envelope shape.**

Append to `crates/edgezero-core/src/blob_envelope.rs` (create the file with this content):

````rust
//! Versioned envelope wrapping the canonical-form `data` blob.
//!
//! Shape per spec §4.1:
//! ```json
//! { "data": {...}, "sha256": "<hex>", "version": 1, "generated_at": "<RFC3339 UTC>" }
//! ```

use crate::canonical_form::canonical_data_sha256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current envelope version. Bumped only when the canonical-form
/// rules change in a way that breaks pin compatibility.
pub const ENVELOPE_VERSION_V1: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BlobEnvelope {
    pub data: serde_json::Value,
    pub sha256: String,
    pub version: u32,
    pub generated_at: String,
}

#[derive(Debug, Error)]
pub enum BlobEnvelopeError {
    #[error("unknown envelope version {0}; expected {expected}", expected = ENVELOPE_VERSION_V1)]
    UnknownVersion(u32),
    #[error("sha mismatch: stored {stored}, computed {computed}")]
    ShaMismatch { stored: String, computed: String },
}

impl BlobEnvelope {
    /// Verify the envelope's `version` and recomputed sha against
    /// the embedded `sha256`. Returns `Ok(())` on agreement;
    /// `Err(BlobEnvelopeError)` otherwise.
    pub fn verify(&self) -> Result<(), BlobEnvelopeError> {
        if self.version != ENVELOPE_VERSION_V1 {
            return Err(BlobEnvelopeError::UnknownVersion(self.version));
        }
        let computed = canonical_data_sha256(&self.data);
        if computed != self.sha256 {
            return Err(BlobEnvelopeError::ShaMismatch {
                stored: self.sha256.clone(),
                computed,
            });
        }
        Ok(())
    }

    /// Construct an envelope from `data` by computing the canonical SHA
    /// and stamping `version`. `generated_at` is passed in by the
    /// caller (we don't take a clock dep here).
    pub fn new(data: serde_json::Value, generated_at: String) -> Self {
        let sha256 = canonical_data_sha256(&data);
        Self {
            data,
            sha256,
            version: ENVELOPE_VERSION_V1,
            generated_at,
        }
    }

    pub fn into_data(self) -> serde_json::Value {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_then_verify_round_trips() {
        let envelope = BlobEnvelope::new(
            json!({ "greeting": "hi" }),
            "2026-06-17T00:00:00Z".into(),
        );
        envelope.verify().unwrap();
    }

    #[test]
    fn rejects_unknown_version() {
        let mut envelope = BlobEnvelope::new(json!({ "x": 1 }), "2026-06-17T00:00:00Z".into());
        envelope.version = 99;
        assert!(matches!(envelope.verify(), Err(BlobEnvelopeError::UnknownVersion(99))));
    }

    #[test]
    fn detects_sha_mismatch() {
        let mut envelope = BlobEnvelope::new(json!({ "x": 1 }), "2026-06-17T00:00:00Z".into());
        envelope.sha256 = "ff".repeat(32);
        assert!(matches!(envelope.verify(), Err(BlobEnvelopeError::ShaMismatch { .. })));
    }

    #[test]
    fn json_round_trip_preserves_fields() {
        let envelope = BlobEnvelope::new(json!({ "x": 1 }), "2026-06-17T00:00:00Z".into());
        let s = serde_json::to_string(&envelope).unwrap();
        let parsed: BlobEnvelope = serde_json::from_str(&s).unwrap();
        parsed.verify().unwrap();
    }
}
````

- [ ] **Step 2: Add `pub mod blob_envelope;` to `crates/edgezero-core/src/lib.rs`.**

Insert (alphabetically) after `pub mod body;`:

```rust
pub mod blob_envelope;
```

- [ ] **Step 3: Run the tests, confirm pass.**

Run: `cargo test -p edgezero-core --lib blob_envelope -- --nocapture`
Expected: 4 passes.

## Task A5 — `AppConfigError::InvalidValue` variant + non-finite-float rejection

**Files:**

- Modify: `crates/edgezero-core/src/app_config.rs` (add variant near line 90; add walker + env-overlay check; add tests).

**Interfaces:**

- Produces: new variant `AppConfigError::InvalidValue { path: PathBuf, field_path: String, message: String }`. Both load paths (TOML walk + env overlay) raise this when a non-finite float is detected. Existing call sites of `load_app_config*` are unaffected (no breaking change to the public signature).

- [ ] **Step 1: Add the new variant to `AppConfigError`.**

Edit `crates/edgezero-core/src/app_config.rs` after the `Validation { .. }` variant at line 125. Insert before the closing `}` of the enum:

```rust
    /// A leaf value failed a structural load-time check (e.g. a
    /// non-finite `f64`). Distinct from `Validation` because no
    /// `validator::ValidationErrors` is involved; the loader
    /// flags this directly.
    #[error("invalid value at {field_path} in {}: {message}", path.display())]
    InvalidValue {
        path: PathBuf,
        /// Dotted path of the offending leaf, e.g. `"service.ratio"`.
        field_path: String,
        /// Human-readable reason, e.g. `"non-finite f64 value `NaN`"`.
        message: String,
    },
```

- [ ] **Step 2: Write a failing test that exercises the TOML-walk path.**

Append to the `mod tests` block at the bottom of `app_config.rs`:

```rust
    #[test]
    fn rejects_non_finite_float_in_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.toml");
        std::fs::write(&path, "[service]\nratio = nan\n").unwrap();
        let err = load_app_config_raw_with_options(
            &path,
            "test",
            &AppConfigLoadOptions { env_overlay: false },
        )
        .expect_err("nan should be rejected");
        match err {
            AppConfigError::InvalidValue { field_path, message, .. } => {
                assert_eq!(field_path, "service.ratio");
                assert!(message.contains("NaN") || message.contains("nan"));
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn rejects_positive_infinity_in_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.toml");
        std::fs::write(&path, "[service]\nratio = inf\n").unwrap();
        let err = load_app_config_raw_with_options(
            &path,
            "test",
            &AppConfigLoadOptions { env_overlay: false },
        )
        .expect_err("inf should be rejected");
        assert!(matches!(err, AppConfigError::InvalidValue { .. }));
    }
```

If `tempfile` is not in `[dev-dependencies]` of `edgezero-core`, add it: `tempfile = { workspace = true }` (already in the workspace per existing tests).

- [ ] **Step 3: Run, confirm fail (the loader doesn't reject yet).**

Run: `cargo test -p edgezero-core --lib app_config::tests::rejects_non_finite`
Expected: both tests fail.

- [ ] **Step 4: Add the walker. Find the end of `load_app_config_raw_with_options` (returns `Ok(value)` near line 250) and insert a finite-float check before the return.**

Locate the existing function body in `app_config.rs`:

```rust
pub fn load_app_config_raw_with_options(
    path: &Path,
    app_name: &str,
    opts: &AppConfigLoadOptions,
) -> Result<Value, AppConfigError> {
    // ... existing TOML read + env overlay ...
    Ok(value)
}
```

Replace the `Ok(value)` at the end with:

```rust
    check_no_non_finite_floats(path, &value)?;
    Ok(value)
}

fn check_no_non_finite_floats(path: &Path, value: &Value) -> Result<(), AppConfigError> {
    let mut stack: Vec<(String, &Value)> = vec![(String::new(), value)];
    while let Some((prefix, current)) = stack.pop() {
        match current {
            Value::Float(f) => {
                if !f.is_finite() {
                    return Err(AppConfigError::InvalidValue {
                        path: path.to_path_buf(),
                        field_path: prefix,
                        message: format!("non-finite f64 value `{f}` is not representable in canonical form"),
                    });
                }
            }
            Value::Table(table) => {
                for (k, v) in table.iter() {
                    let next = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    stack.push((next, v));
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let next = if prefix.is_empty() { format!("[{i}]") } else { format!("{prefix}[{i}]") };
                    stack.push((next, item));
                }
            }
            _ => {}
        }
    }
    Ok(())
}
```

(`toml::Value` enum variant for floats is `Float(f64)`; verify by reading `toml` crate docs or by editor IntelliSense.)

- [ ] **Step 5: Run TOML-walk tests, confirm both pass.**

Run: `cargo test -p edgezero-core --lib app_config::tests::rejects_non_finite`
Expected: 2 passes.

- [ ] **Step 6: Find the env-overlay float parser. Grep:**

Run: `grep -n "parse::<f64>\|parse::<f32>" crates/edgezero-core/src/app_config.rs`
Locate the `parse::<f64>()` call. The spec cited line 298. Insert an `is_finite()` check immediately after the `parse` so a non-finite env-supplied value errors before reaching the `Value::Float` construction.

Replace the existing parse-call code (typical shape):

```rust
let parsed: f64 = raw.parse::<f64>().map_err(|e| AppConfigError::EnvOverlay {
    path: path.to_path_buf(),
    message: format!("`{raw}` is not a finite f64: {e}"),
})?;
```

With:

```rust
let parsed: f64 = raw.parse::<f64>().map_err(|e| AppConfigError::EnvOverlay {
    path: path.to_path_buf(),
    message: format!("`{raw}` is not a valid f64: {e}"),
})?;
if !parsed.is_finite() {
    return Err(AppConfigError::InvalidValue {
        path: path.to_path_buf(),
        field_path: dotted_path.clone(),  // adjust to whatever the overlay loop tracks
        message: format!("non-finite f64 value `{parsed}` from env overlay is not representable in canonical form"),
    });
}
```

(The `dotted_path` variable name depends on what the existing overlay code uses to track the segment stack. Inspect the surrounding function to find the right binding.)

- [ ] **Step 7: Add an env-overlay test.**

```rust
    #[test]
    fn rejects_non_finite_float_in_env_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.toml");
        std::fs::write(&path, "[service]\nratio = 1.5\n").unwrap();
        let _guard = EnvGuard::set("TEST__SERVICE__RATIO", "nan");
        let err = load_app_config_raw_with_options(
            &path,
            "test",
            &AppConfigLoadOptions { env_overlay: true },
        )
        .expect_err("nan from env overlay should be rejected");
        assert!(matches!(err, AppConfigError::InvalidValue { .. }));
    }
```

If there's no existing `EnvGuard` helper, locate the existing env-overlay tests for the lock + std::env pattern they use. Reuse the same scaffolding.

- [ ] **Step 8: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib app_config::tests::rejects_non_finite`
Expected: 3 passes.

- [ ] **Step 9: Run the full app_config tests to confirm no regression.**

Run: `cargo test -p edgezero-core --lib app_config`
Expected: all pre-existing tests still pass.

## Task A6 — `deserialize_app_config_with_options` entry points + Phase A commit

**Files:**

- Modify: `crates/edgezero-core/src/app_config.rs` — add two new top-level functions exposing the deserialise-only path.

**Interfaces:**

- Produces:
  - `pub fn deserialize_app_config<C>(path: &Path, app_name: &str) -> Result<C, AppConfigError> where C: DeserializeOwned + AppConfigMeta`
  - `pub fn deserialize_app_config_with_options<C>(path: &Path, app_name: &str, opts: &AppConfigLoadOptions) -> Result<C, AppConfigError> where C: DeserializeOwned + AppConfigMeta`
- These are consumed in Phase C by `config push` / `config diff` (so they can pair with `validate_excluding_secrets` instead of the existing `Validate::validate`).

- [ ] **Step 1: Add the new functions next to `load_app_config_with_options`.**

```rust
/// Like [`load_app_config`] but DOES NOT call `Validate::validate`.
/// Used by `config push` / `config diff` paths that route through
/// `validate_excluding_secrets` instead. See spec §3.3.8.
pub fn deserialize_app_config<C>(path: &Path, app_name: &str) -> Result<C, AppConfigError>
where
    C: DeserializeOwned + AppConfigMeta,
{
    deserialize_app_config_with_options(path, app_name, &AppConfigLoadOptions::default())
}

/// [`deserialize_app_config`] with an explicit [`AppConfigLoadOptions`].
pub fn deserialize_app_config_with_options<C>(
    path: &Path,
    app_name: &str,
    opts: &AppConfigLoadOptions,
) -> Result<C, AppConfigError>
where
    C: DeserializeOwned + AppConfigMeta,
{
    let config_table = load_app_config_raw_with_options(path, app_name, opts)?;
    let typed: C =
        config_table
            .try_into()
            .map_err(|source: TomlDeError| AppConfigError::Deserialize {
                path: path.to_path_buf(),
                target_type: any::type_name::<C>(),
                source: Box::new(source),
            })?;
    Ok(typed)
}
```

- [ ] **Step 2: Add a test.**

```rust
    #[test]
    fn deserialize_does_not_call_validate() {
        // A struct whose Validate impl always fails — but
        // deserialize_app_config_with_options must NOT call it.
        use validator::ValidationError;
        #[derive(Deserialize, AppConfigMeta)]
        struct Fixture {
            value: i32,
        }
        impl validator::Validate for Fixture {
            fn validate(&self) -> Result<(), validator::ValidationErrors> {
                let mut errs = validator::ValidationErrors::new();
                errs.add("value", ValidationError::new("intentional"));
                Err(errs)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.toml");
        std::fs::write(&path, "value = 42\n").unwrap();
        let cfg: Fixture = deserialize_app_config_with_options(
            &path,
            "test",
            &AppConfigLoadOptions { env_overlay: false },
        )
        .unwrap();
        assert_eq!(cfg.value, 42);
    }
```

If `AppConfigMeta` is not yet derivable on a fixture struct (it lives in the macros crate; this is a unit test inside `edgezero-core`), use a hand-rolled impl instead:

```rust
        struct Fixture {
            value: i32,
        }
        impl crate::app_config::AppConfigMeta for Fixture {
            const SECRET_FIELDS: &'static [crate::app_config::SecretField] = &[];
        }
```

- [ ] **Step 3: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib app_config::tests`
Expected: full pass.

- [ ] **Step 4: Run the whole workspace test suite + the four CI gates.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Expected: all four pass clean.

- [ ] **Step 5: Commit Phase A.**

```bash
git add Cargo.toml Cargo.lock \
        crates/edgezero-core/Cargo.toml \
        crates/edgezero-core/src/lib.rs \
        crates/edgezero-core/src/canonical_form.rs \
        crates/edgezero-core/src/blob_envelope.rs \
        crates/edgezero-core/src/app_config.rs \
        crates/edgezero-core/tests/canonical_form_pins.rs
git commit -m "Add canonical form, blob envelope, non-finite-float rejection

Phase A of the blob app-config rewrite per
docs/superpowers/specs/2026-06-16-blob-app-config.md §4.1-§4.2:

- crates/edgezero-core/src/canonical_form.rs: hand-rolled SHA
  canonicaliser over serde_json::Value. Sorts object keys by
  UTF-8 byte order, renders finite floats via ryu's shortest
  round-trippable form, emits verbatim UTF-8 strings (no NFC
  normalisation). Q1 resolution per spec round 21: in-tree only,
  no external canonicaliser crate.
- crates/edgezero-core/src/blob_envelope.rs: BlobEnvelope type
  wrapping data + sha256 + version + generated_at. verify()
  recomputes the sha and rejects mismatch or unknown version.
- crates/edgezero-core/src/app_config.rs: new AppConfigError::
  InvalidValue variant; loader (TOML walk + env overlay)
  rejects non-finite f64 values before serialisation, so
  serde_json never coerces them to JSON null.
- canonical_form_pin_v1 test in crates/edgezero-core/tests/
  pins the v1 byte format against a fixed fixture.
- deserialize_app_config[_with_options] entry points expose the
  deserialise-only path for Phase C's push/diff routing.

No in-tree caller exercises any of this yet; Phase B wires it
into infrastructure (extractor, read trait, etc.) and Phase C
ships the cutover."
```

---

# Phase B — Pre-cutover infrastructure (Commit B)

**Goal:** Add types, traits, macros, and per-adapter scaffolding that the cutover (Phase C) will consume. No in-tree caller exercises the new surfaces yet, so `main` behaviour is unchanged and the commit is bisect-safe.

Each task ends with `cargo test --workspace` passing on the commit boundary. Phase B ends with a single commit covering all tasks.

## Task B1 — Manifest store-id charset tightening + tests

**Files:**

- Modify: `crates/edgezero-core/src/manifest.rs` (the validator at the cited line ~880; tighten character class).

**Interfaces:**

- Produces: tighter `[stores.*]` id validation. Hyphens now rejected with a clear message naming the `EDGEZERO__STORES__<KIND>__<ID>__KEY` export constraint.

- [ ] **Step 1: Write a failing test asserting hyphen rejection.**

In the `mod tests` block of `manifest.rs`, add:

```rust
    #[test]
    fn rejects_hyphenated_store_id() {
        let manifest = r#"
[app]
name = "demo"

[stores.config]
ids = ["feature-flags"]
default = "feature-flags"

[adapters.axum]
"#;
        let err = parse_manifest(manifest).unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("feature-flags"), "{rendered}");
        assert!(rendered.contains("POSIX") || rendered.contains("env") || rendered.contains("hyphen"),
            "error should mention the env-export constraint; got: {rendered}");
    }
```

(Replace `parse_manifest` with the actual validator-entry function used in existing tests.)

- [ ] **Step 2: Run, confirm fail.**

Run: `cargo test -p edgezero-core --lib manifest::tests::rejects_hyphenated_store_id`
Expected: fail (current validator accepts hyphens).

- [ ] **Step 3: Tighten the charset.**

Locate the existing validator block (per spec, around line 880-902). Replace:

```rust
        let chars_bad = id
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'));
```

With:

```rust
        let chars_bad = id
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'));
```

And update the error message in the same block from "ASCII alphanumeric / `_` / `-` are allowed" to:

```rust
        let mut error = ValidationError::new("store_id_format");
        error.message = Some(
            format!(
                "`[stores.<kind>].ids` entry `{bad}` contains a character outside `[A-Za-z0-9_]`. Store ids must be POSIX-shell-exportable so the `EDGEZERO__STORES__<KIND>__<ID>__NAME` / `__KEY` env overrides work; hyphens and other characters are not permitted. Rename it (e.g. `feature-flags` → `feature_flags`)."
            )
            .into(),
        );
```

- [ ] **Step 4: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib manifest`
Expected: new test passes; existing tests still pass.

- [ ] **Step 5: Add positive tests for underscore-only and `__`-still-rejected.**

```rust
    #[test]
    fn accepts_underscore_only_store_id() {
        let manifest = r#"
[app]
name = "demo"

[stores.config]
ids = ["feature_flags"]
default = "feature_flags"

[adapters.axum]
"#;
        parse_manifest(manifest).unwrap();
    }

    #[test]
    fn rejects_double_underscore_in_store_id() {
        let manifest = r#"
[app]
name = "demo"

[stores.config]
ids = ["feature__flags"]
default = "feature__flags"

[adapters.axum]
"#;
        assert!(parse_manifest(manifest).is_err());
    }
```

Run: `cargo test -p edgezero-core --lib manifest`
Expected: 3 new tests pass.

## Task B2 — `EdgeError::ConfigOutOfDate` variant + two constructors

**Files:**

- Modify: `crates/edgezero-core/src/error.rs` (add variant + constructors).
- Modify: `crates/edgezero-core/Cargo.toml` — add `serde_path_to_error = { workspace = true }`.

**Interfaces:**

- Produces:
  - `EdgeError::ConfigOutOfDate { message: String, field_path: String }`
  - `EdgeError::config_out_of_date(message: impl Into<String>, field_path: impl Into<String>) -> Self`
  - `EdgeError::config_out_of_date_from_serde(err: serde_path_to_error::Error<serde_json::Error>) -> Self`

- [ ] **Step 1: Add the variant.**

Edit `crates/edgezero-core/src/error.rs` near the existing enum. Insert:

```rust
    /// The blob's `data` shape disagrees with the deployed `C`
    /// type. Re-running `<app-cli> config push` for the deployed
    /// code revision fixes it. HTTP 503, kind
    /// `"config_out_of_date"`, carries `Retry-After: 60`.
    #[error("config out of date: {message}")]
    ConfigOutOfDate {
        message: String,
        field_path: String,
    },
```

- [ ] **Step 2: Add the constructors on the existing `impl EdgeError` block.**

```rust
    /// Construct from an explicit `(message, field_path)` pair.
    /// Used by the secret walk and validator paths. `field_path`
    /// SHOULD be a dotted path naming the offending field; pass
    /// `String::new()` when no specific field is anchored.
    pub fn config_out_of_date(
        message: impl Into<String>,
        field_path: impl Into<String>,
    ) -> Self {
        Self::ConfigOutOfDate {
            message: message.into(),
            field_path: field_path.into(),
        }
    }

    /// Construct from a `serde_path_to_error` error returned by
    /// the deserialise wrapper around the blob's `data` field.
    pub fn config_out_of_date_from_serde(
        serde_err: serde_path_to_error::Error<serde_json::Error>,
    ) -> Self {
        Self::ConfigOutOfDate {
            message: serde_err.inner().to_string(),
            field_path: serde_err.path().to_string(),
        }
    }
```

- [ ] **Step 3: Add `serde_path_to_error` to `crates/edgezero-core/Cargo.toml`.**

In `[dependencies]`:

```toml
serde_path_to_error = { workspace = true }
```

- [ ] **Step 4: Update the THREE exhaustive matches that `EdgeError` already has to cover `ConfigOutOfDate`.**

The crate ships with three `pub fn` accessors that each `match self` exhaustively. Adding the new variant without extending all three will fail `cargo check`. They are:

1. **`inner()`** at `crates/edgezero-core/src/error.rs:50` — returns `Option<&AnyError>`. Add `ConfigOutOfDate` to the `None` arm:

```rust
            EdgeError::BadRequest { .. }
            | EdgeError::NotFound { .. }
            | EdgeError::NotImplemented { .. }
            | EdgeError::MethodNotAllowed { .. }
            | EdgeError::Validation { .. }
            | EdgeError::ServiceUnavailable { .. }
            | EdgeError::ConfigOutOfDate { .. } => None,
```

2. **`message()`** at `crates/edgezero-core/src/error.rs:74` — returns a rendered `String`. Add an arm:

```rust
            EdgeError::ConfigOutOfDate { message, .. } => message.clone(),
```

3. **`status()`** at `crates/edgezero-core/src/error.rs:128` — returns `StatusCode`. Add:

```rust
            EdgeError::ConfigOutOfDate { .. } => StatusCode::SERVICE_UNAVAILABLE,
```

(There is NO `source()` method on this type and NO custom `Display` impl — the variants are rendered via `thiserror`'s `#[error("...")]` attribute on each variant. Add `#[error("config out of date: {message}")]` to the new variant declaration so the auto-derived `Display` produces a useful string.)

- [ ] **Step 5: Write a constructor test.**

In the existing `mod tests` block:

```rust
    #[test]
    fn config_out_of_date_constructor_round_trips() {
        let err = EdgeError::config_out_of_date("missing field", "feature.new_checkout");
        match err {
            EdgeError::ConfigOutOfDate { message, field_path } => {
                assert_eq!(message, "missing field");
                assert_eq!(field_path, "feature.new_checkout");
            }
            _ => panic!("expected ConfigOutOfDate"),
        }
    }
```

- [ ] **Step 6: Run, confirm compile + pass.**

Run: `cargo test -p edgezero-core --lib error`
Expected: pass; no existing test broken (the three exhaustive matches above — `inner()`, `message()`, `status()` — and the `thiserror` `#[error(...)]` attribute on the new variant cover every dispatch surface).

## Task B3 — `EdgeError` response body adds `kind` field + `Retry-After` on `ConfigOutOfDate`

**Files:**

- Modify: `crates/edgezero-core/src/error.rs` — extend `IntoResponse` impl.
- Modify: `crates/edgezero-core/src/error.rs` — add per-variant `kind` constants.

**Interfaces:**

- Produces: response body shape `{ "error": { "status": <u16>, "kind": "<string>", "message": "<…>", "field_path"?: "<…>" } }`. `field_path` ONLY on `ConfigOutOfDate`. `Retry-After: 60` header ONLY on `ConfigOutOfDate`. Per spec §6.3.1.

- [ ] **Step 1: Add a `kind_str` private method per variant.**

```rust
impl EdgeError {
    fn kind_str(&self) -> &'static str {
        match self {
            EdgeError::BadRequest { .. } => "bad_request",
            EdgeError::Internal { .. } => "internal",
            EdgeError::MethodNotAllowed { .. } => "method_not_allowed",
            EdgeError::NotFound { .. } => "not_found",
            EdgeError::NotImplemented { .. } => "not_implemented",
            EdgeError::ServiceUnavailable { .. } => "service_unavailable",
            EdgeError::Validation { .. } => "validation",
            EdgeError::ConfigOutOfDate { .. } => "config_out_of_date",
        }
    }
}
```

- [ ] **Step 2: Rewrite the `IntoResponse` impl's body-building block.**

Locate the existing `IntoResponse` impl (around `error.rs:159` per spec). The current block writes `{ "error": { "status": ..., "message": ... } }`. Replace with code that:

1. Picks the right status code per variant.
2. Builds a `serde_json::Value::Object` map with `status`, `kind`, `message`, and (for `ConfigOutOfDate`) `field_path`.
3. Sets `Retry-After: 60` ONLY for `ConfigOutOfDate`.

Pseudocode shape (adapt to the exact existing `IntoResponse` form):

```rust
impl IntoResponse for EdgeError {
    fn into_response(self) -> Response {
        let kind = self.kind_str();
        let (status, message, field_path_opt): (StatusCode, String, Option<String>) = match &self {
            EdgeError::BadRequest { message } => (StatusCode::BAD_REQUEST, message.clone(), None),
            EdgeError::Internal { source } => (StatusCode::INTERNAL_SERVER_ERROR, source.to_string(), None),
            EdgeError::MethodNotAllowed { message } => (StatusCode::METHOD_NOT_ALLOWED, message.clone(), None),
            EdgeError::NotFound { message } => (StatusCode::NOT_FOUND, message.clone(), None),
            EdgeError::NotImplemented { message } => (StatusCode::NOT_IMPLEMENTED, message.clone(), None),
            EdgeError::ServiceUnavailable { message } => (StatusCode::SERVICE_UNAVAILABLE, message.clone(), None),
            EdgeError::Validation { message } => (StatusCode::UNPROCESSABLE_ENTITY, message.clone(), None),
            EdgeError::ConfigOutOfDate { message, field_path } => (
                StatusCode::SERVICE_UNAVAILABLE,
                message.clone(),
                Some(field_path.clone()),
            ),
        };
        let mut error_obj = serde_json::Map::new();
        error_obj.insert("status".into(), serde_json::Value::from(status.as_u16()));
        error_obj.insert("kind".into(), serde_json::Value::from(kind));
        error_obj.insert("message".into(), serde_json::Value::from(message));
        if let Some(fp) = field_path_opt {
            error_obj.insert("field_path".into(), serde_json::Value::from(fp));
        }
        let body = serde_json::json!({ "error": serde_json::Value::Object(error_obj) });
        let body_bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
        let mut response = Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .unwrap();
        if matches!(self, EdgeError::ConfigOutOfDate { .. }) {
            response
                .headers_mut()
                .insert(http::header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}
```

Use the actual `Response` / `Body` types as imported elsewhere in `error.rs`.

- [ ] **Step 3: Run existing error tests to confirm nothing regressed (existing tests assert the old shape will fail — that's expected; we update them next).**

Run: `cargo test -p edgezero-core --lib error`
Note which tests fail (they're asserting the old `{ status, message }` shape). Phase B Task B4 below adds the new tests.

## Task B4 — Per-variant `kind` + `Retry-After` tests

**Files:**

- Modify: `crates/edgezero-core/src/error.rs` (update existing IntoResponse tests + add new ones per spec §12.6.1).

**Interfaces:**

- Consumes: `EdgeError::kind_str` + `IntoResponse` impl from Task B3.

- [ ] **Step 1: Update existing tests that assert the old body shape.** Find tests asserting `body.error.status` + `body.error.message` only; extend them to also assert `body.error.kind`. Use the table from spec §12.6.1.

- [ ] **Step 2: Add a new test per variant asserting the `kind` string.**

```rust
    #[test]
    fn kind_strings_per_variant() {
        let cases: &[(EdgeError, &str, u16)] = &[
            (EdgeError::BadRequest { message: "x".into() }, "bad_request", 400),
            (EdgeError::Internal { source: anyhow::anyhow!("x").into() }, "internal", 500),
            (EdgeError::MethodNotAllowed { message: "x".into() }, "method_not_allowed", 405),
            (EdgeError::NotFound { message: "x".into() }, "not_found", 404),
            (EdgeError::NotImplemented { message: "x".into() }, "not_implemented", 501),
            (EdgeError::ServiceUnavailable { message: "x".into() }, "service_unavailable", 503),
            (EdgeError::Validation { message: "x".into() }, "validation", 422),
            (EdgeError::config_out_of_date("x", "f"), "config_out_of_date", 503),
        ];
        for (err, expected_kind, expected_status) in cases {
            // Clone the error if it doesn't impl Clone — work around with match.
            let response = err.clone().into_response();  // adjust if not Clone
            assert_eq!(response.status().as_u16(), *expected_status);
            let body_bytes = ...;  // collect body
            let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
            assert_eq!(body["error"]["kind"], serde_json::Value::from(*expected_kind));
        }
    }
```

(If `EdgeError` is not `Clone`, restructure to build each case in turn rather than collecting in a slice.)

- [ ] **Step 3: Add `Retry-After` presence assertion.**

```rust
    #[test]
    fn retry_after_only_on_config_out_of_date() {
        for (err, expected_retry_after) in [
            (EdgeError::BadRequest { message: "x".into() }, false),
            (EdgeError::Internal { source: anyhow::anyhow!("x").into() }, false),
            (EdgeError::ServiceUnavailable { message: "x".into() }, false),  // round-10 H-3 narrowing
            (EdgeError::config_out_of_date("x", "f"), true),
        ] {
            let response = err.into_response();
            let header = response.headers().get(http::header::RETRY_AFTER);
            if expected_retry_after {
                assert_eq!(header.unwrap().to_str().unwrap(), "60");
            } else {
                assert!(header.is_none(), "unexpected Retry-After header");
            }
        }
    }
```

- [ ] **Step 4: Add `field_path` presence assertion.**

```rust
    #[test]
    fn field_path_only_on_config_out_of_date() {
        let err = EdgeError::BadRequest { message: "x".into() };
        let body: serde_json::Value = parse_body(err.into_response());
        assert!(body["error"].get("field_path").is_none());

        let err = EdgeError::config_out_of_date("x", "feature.new_checkout");
        let body: serde_json::Value = parse_body(err.into_response());
        assert_eq!(body["error"]["field_path"], "feature.new_checkout");
    }

    fn parse_body(response: Response) -> serde_json::Value {
        // Helper: collect body bytes synchronously. If body is async,
        // use futures::executor::block_on; that's the in-tree pattern.
        ...
    }
```

- [ ] **Step 5: Run, confirm all pass.**

Run: `cargo test -p edgezero-core --lib error`
Expected: green.

## Task B5 — `ConfigStoreBinding` + `ConfigRegistry` type change

**Files:**

- Modify: `crates/edgezero-core/src/store_registry.rs` — replace `BoundConfigStore = ConfigStoreHandle` with `ConfigStoreBinding`; change `ConfigRegistry = StoreRegistry<ConfigStoreBinding>`.

**Interfaces:**

- Produces:
  - `pub struct ConfigStoreBinding { pub handle: ConfigStoreHandle, pub default_key: String }` (`#[derive(Clone, Debug)]`).
  - `pub type ConfigRegistry = StoreRegistry<ConfigStoreBinding>`.
- Breaks: any caller that destructures the old `StoreRegistry<ConfigStoreHandle>` value type. An audit shows only the framework's extractor plumbing reads the value; no in-tree caller iterates it directly.

- [ ] **Step 1: Add the binding struct.**

```rust
/// Per-id binding pair for the config store: the handle the
/// extractor calls `get(...)` on, plus the key the extractor
/// looks up by default. The `default_key` is computed by
/// adapters from `EnvConfig::store_key("config", id)`. See spec
/// §5.2.1.
#[derive(Clone, Debug)]
pub struct ConfigStoreBinding {
    pub handle: ConfigStoreHandle,
    pub default_key: String,
}
```

- [ ] **Step 2: Change the type alias.**

Replace `pub type ConfigRegistry = StoreRegistry<BoundConfigStore>;` with:

```rust
pub type ConfigRegistry = StoreRegistry<ConfigStoreBinding>;
```

Keep `pub type BoundConfigStore = ConfigStoreHandle;` — Phase C's `Config` extractor accessors still return `BoundConfigStore` (unwrapping `binding.handle`).

- [ ] **Step 3: Build the workspace to find every caller broken by the value-type change.**

```bash
cargo check --workspace --all-targets > /tmp/check.log 2>&1
echo "Exit: $?"
grep -E '^error' /tmp/check.log | head -30
```

Expected: `cargo check` fails (non-zero exit). The grep enumerates each broken caller — they should be confined to the framework's extractor (where the old `BoundConfigStore` was returned) and adapter request-context builders that construct the registry. Anything else is a clue to a caller the spec missed.

- [ ] **Step 4: Fix the `Config` extractor (`crates/edgezero-core/src/extractor.rs`) so `default()` / `named()` unwrap to the handle.**

Find `Config::default` (around line 622). Change:

```rust
    pub fn default(&self) -> Option<BoundConfigStore> {
        self.0.default()
    }
```

To:

```rust
    pub fn default(&self) -> Option<BoundConfigStore> {
        self.0.default().map(|binding| binding.handle)
    }
```

Same for `named`:

```rust
    pub fn named(&self, id: &str) -> Option<BoundConfigStore> {
        self.0.named(id).map(|binding| binding.handle)
    }
```

- [ ] **Step 5: Fix each adapter's request-context constructor.** Each adapter builds the registry by mapping logical ids to handles; under the new shape, each ID must produce a `ConfigStoreBinding { handle, default_key }`. The `default_key` is `EnvConfig::store_key("config", id)` (added in Task B7). For Task B5 alone, hardcode `default_key: id.to_owned()` temporarily — Task B7 wires the env-var path.

Files to edit (per spec §5.2.1's adapter line references):

- `crates/edgezero-adapter-axum/src/request.rs` (or `context.rs`)
- `crates/edgezero-adapter-cloudflare/src/request.rs`
- `crates/edgezero-adapter-fastly/src/request.rs`
- `crates/edgezero-adapter-spin/src/request.rs`

In each, find the registry-builder loop (typically something like `for id in declared_ids { … handle.clone() … }`) and wrap each handle in `ConfigStoreBinding { handle, default_key: id.clone() }`.

- [ ] **Step 6: Build, confirm clean.**

Run: `cargo check --workspace --all-targets`
Expected: 0 errors.

- [ ] **Step 7: Run all tests.**

Run: `cargo test --workspace --all-targets`
Expected: all pre-existing tests pass.

## Task B6 — `StoreRegistry::default_ref` / `named_ref`

**Files:**

- Modify: `crates/edgezero-core/src/store_registry.rs` — add two ref accessors.

**Interfaces:**

- Produces: `impl<H: Clone> StoreRegistry<H> { fn default_ref(&self) -> Option<&H>; fn named_ref(&self, id: &str) -> Option<&H>; }`.

- [ ] **Step 1: Add the two methods inside the existing `impl<H: Clone> StoreRegistry<H>`.**

```rust
    /// Borrow the default handle without cloning. Mirrors
    /// [`default`](Self::default) but yields a reference.
    #[must_use]
    #[inline]
    pub fn default_ref(&self) -> Option<&H> {
        self.by_id.get(&self.default_id)
    }

    /// Borrow the handle for `id`. Mirrors
    /// [`named`](Self::named) but yields a reference.
    #[must_use]
    #[inline]
    pub fn named_ref(&self, id: &str) -> Option<&H> {
        self.by_id.get(id)
    }
```

- [ ] **Step 2: Write a unit test asserting both return `Some` for declared ids and `None` for unknown.**

```rust
    #[test]
    fn default_ref_and_named_ref_yield_references() {
        // Construct a single-id registry. The fixture helper depends
        // on the existing test scaffolding; reuse whatever's already
        // there for `default()` tests.
        let registry = single_id_registry("only", "value");
        assert_eq!(registry.default_ref(), Some(&"value"));
        assert_eq!(registry.named_ref("only"), Some(&"value"));
        assert_eq!(registry.named_ref("missing"), None);
    }
```

- [ ] **Step 3: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib store_registry`
Expected: pass.

## Task B7 — `EnvConfig::store_key` helper

**Files:**

- Modify: `crates/edgezero-core/src/env_config.rs` — add helper mirroring `store_name`.

**Interfaces:**

- Produces: `EnvConfig::store_key(&self, kind: &str, id: &str) -> String`. Falls back to `id` on unset / blank / control-char values, mirroring `store_name`.

- [ ] **Step 1: Add the helper directly under `store_name` (around line 133).**

```rust
    /// Key for a logical store — `EDGEZERO__STORES__<KIND>__<ID>__KEY` —
    /// falling back to `id` itself when unset, blank, whitespace-only, or
    /// containing control characters. Mirrors [`store_name`]'s filter exactly.
    #[must_use]
    #[inline]
    pub fn store_key(&self, kind: &str, id: &str) -> String {
        self.get(&["stores", kind, id, "key"])
            .filter(|value| !is_blank_or_control(value))
            .map_or_else(|| id.to_owned(), str::to_owned)
    }
```

- [ ] **Step 2: Add tests next to the existing `store_name` tests.**

```rust
    #[test]
    fn store_key_returns_env_var_when_set() {
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "app_config_staging")]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config_staging");
    }

    #[test]
    fn store_key_falls_back_to_id_when_unset() {
        let cfg = EnvConfig::from_vars(std::iter::empty::<(&str, &str)>());
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }

    #[test]
    fn store_key_falls_back_on_blank_value() {
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "   ")]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }

    #[test]
    fn store_key_falls_back_on_control_chars() {
        let cfg = EnvConfig::from_vars([("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY", "bad\x01key")]);
        assert_eq!(cfg.store_key("config", "app_config"), "app_config");
    }
```

- [ ] **Step 3: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib env_config`
Expected: pass.

- [ ] **Step 4: Update each adapter's registry-builder (touched in Task B5) to use `env.store_key("config", id)` instead of the hardcoded `id.to_owned()` placeholder.**

For each of the four adapter `request.rs` files:

```rust
ConfigStoreBinding {
    handle,
    default_key: env.store_key("config", &id),
}
```

(`env` is the `EnvConfig` instance the request-context builder already has in scope.)

Run: `cargo check --workspace --all-targets`
Expected: clean.

## Task B8 — `Config` extractor binding accessors + `RequestContext` helpers

**Files:**

- Modify: `crates/edgezero-core/src/extractor.rs` — add `Config::default_binding()` / `Config::named_binding(id)`.
- Modify: `crates/edgezero-core/src/context.rs` — add `RequestContext::config_store_default_binding()` / `config_store_binding(id)`.

**Interfaces:**

- Produces:
  - `Config::default_binding(&self) -> Option<&ConfigStoreBinding>`
  - `Config::named_binding(&self, id: &str) -> Option<&ConfigStoreBinding>`
  - `RequestContext::config_store_default_binding(&self) -> Option<&ConfigStoreBinding>`
  - `RequestContext::config_store_binding(&self, id: &str) -> Option<&ConfigStoreBinding>`

- [ ] **Step 1: Add the two `Config` accessors.**

In the `impl Config { ... }` block:

```rust
    /// Borrow the default binding (handle + resolved __KEY) without
    /// cloning. Used by the typed `AppConfig<C>` extractor.
    #[must_use]
    #[inline]
    pub fn default_binding(&self) -> Option<&ConfigStoreBinding> {
        self.0.default_ref()
    }

    /// Borrow a binding by id.
    #[must_use]
    #[inline]
    pub fn named_binding(&self, id: &str) -> Option<&ConfigStoreBinding> {
        self.0.named_ref(id)
    }
```

- [ ] **Step 2: Add the two `RequestContext` helpers (in `crates/edgezero-core/src/context.rs`).**

Locate the existing `config_store_default()` / `config_store(id)` methods (around line 172 per spec). Add next to them:

```rust
    /// Borrow the default config-store binding (handle + key). See
    /// spec §5.2.1.
    #[must_use]
    #[inline]
    pub fn config_store_default_binding(&self) -> Option<&ConfigStoreBinding> {
        self.request()
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.default_ref())
    }

    /// Borrow a named binding.
    #[must_use]
    #[inline]
    pub fn config_store_binding(&self, id: &str) -> Option<&ConfigStoreBinding> {
        self.request()
            .extensions()
            .get::<ConfigRegistry>()
            .and_then(|registry| registry.named_ref(id))
    }
```

Add imports as needed (`ConfigStoreBinding`, `ConfigRegistry`).

- [ ] **Step 3: Add tests.**

In `extractor.rs::tests`:

```rust
    #[test]
    fn config_default_binding_returns_resolved_key() {
        // Synthesise a request with a single-id ConfigRegistry where
        // the binding's default_key is "app_config_staging" (mimicking
        // the env-override path).
        let handle = make_dummy_config_store_handle();
        let binding = ConfigStoreBinding { handle, default_key: "app_config_staging".into() };
        let registry = ConfigRegistry::single_id("app_config".into(), binding);
        let request = request_builder()
            .extension(registry)
            .body(Body::empty())
            .unwrap();
        let ctx = RequestContext::from_request(request, PathParams::default());
        let config = block_on(Config::from_request(&ctx)).unwrap();
        let binding = config.default_binding().unwrap();
        assert_eq!(binding.default_key, "app_config_staging");
    }
```

(`make_dummy_config_store_handle` mirrors existing test scaffolding; reuse what's already there.)

- [ ] **Step 4: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib extractor`
Expected: pass.

## Task B9 — `validate_excluding_secrets` wrapper

**Files:**

- Modify: `crates/edgezero-core/src/app_config.rs` — add the wrapper function next to `AppConfigMeta`.

**Interfaces:**

- Produces: `pub fn validate_excluding_secrets<C: validator::Validate + AppConfigMeta>(cfg: &C) -> Result<(), validator::ValidationErrors>`. Skips per-field validators on every `KeyInDefault` / `KeyInNamedStore` field; keeps validators on `StoreRef` fields (those values don't change at runtime).

- [ ] **Step 1: Add the function.**

```rust
/// Validate `cfg` but SKIP per-field validators on `#[secret]` /
/// `#[secret(store_ref = "...")]` fields. Used by `config push` /
/// `config diff` paths where those fields hold operator-typed KEY
/// NAMES, not the resolved secret values. See spec §3.3.8.
///
/// `#[secret(store_ref)]` fields are kept (their value is a store
/// id, identical at push and runtime).
pub fn validate_excluding_secrets<C: validator::Validate + AppConfigMeta>(
    cfg: &C,
) -> Result<(), validator::ValidationErrors> {
    let result = cfg.validate();
    let Err(mut errors) = result else {
        return Ok(());
    };
    // validator 0.20 exposes errors_mut() -> &mut HashMap<&'static str, ValidationErrorsKind>
    let bag = errors.errors_mut();
    for field in C::SECRET_FIELDS {
        if matches!(field.kind, SecretKind::StoreRef) {
            continue; // store_id field; validator stays
        }
        bag.remove(field.name);
    }
    if bag.is_empty() {
        return Ok(());
    }
    Err(errors)
}
```

(Verify `SecretKind` variants exist in scope; if not yet, this task lands AFTER Task B12 below. Reorder if needed.)

- [ ] **Step 2: Add a unit test using a fixture struct with a `#[validate(length(min=32))]` on a `#[secret]` field.**

```rust
    #[test]
    fn validate_excluding_secrets_skips_secret_field_rules() {
        use validator::Validate;
        #[derive(Validate)]
        struct Fixture {
            #[validate(length(min = 32))]
            api_token: String,
            #[validate(length(min = 1))]
            greeting: String,
        }
        impl AppConfigMeta for Fixture {
            const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
                name: "api_token",
                kind: SecretKind::KeyInDefault,
            }];
        }
        let cfg = Fixture { api_token: "short".into(), greeting: "hi".into() };
        // Plain validate fails because api_token < 32 chars.
        assert!(cfg.validate().is_err());
        // validate_excluding_secrets passes (api_token skipped, greeting OK).
        assert!(validate_excluding_secrets(&cfg).is_ok());
    }

    #[test]
    fn validate_excluding_secrets_keeps_non_secret_failures() {
        use validator::Validate;
        #[derive(Validate)]
        struct Fixture {
            #[validate(length(min = 1))]
            api_token: String,
            #[validate(length(min = 32))]
            greeting: String,
        }
        impl AppConfigMeta for Fixture {
            const SECRET_FIELDS: &'static [SecretField] = &[SecretField {
                name: "api_token",
                kind: SecretKind::KeyInDefault,
            }];
        }
        let cfg = Fixture { api_token: "x".into(), greeting: "short".into() };
        assert!(validate_excluding_secrets(&cfg).is_err());
    }
```

- [ ] **Step 3: Run, confirm pass.**

Run: `cargo test -p edgezero-core --lib app_config::tests::validate_excluding_secrets`
Expected: pass.

## Task B10 — `#[derive(AppConfig)]` macro: `SecretKind::KeyInNamedStore` + serde-attribute bans

**Files:**

- Modify: `crates/edgezero-core/src/app_config.rs` — extend `SecretKind` enum with `KeyInNamedStore { store_ref_field: &'static str }`.
- Modify: `crates/edgezero-macros/src/app_config.rs` — parse `#[secret(store_ref = "field")]`; enforce skip/skip_if/flatten bans on EVERY field; emit `AppConfigRoot` impl.
- Modify: `crates/edgezero-core/src/app_config.rs` — add `pub trait AppConfigRoot {}`.

**Interfaces:**

- Produces:
  - `SecretKind::KeyInNamedStore { store_ref_field: &'static str }`.
  - `pub trait AppConfigRoot {}` (open, public).
  - The macro emits `impl AppConfigRoot for C {}` for every `#[derive(AppConfig)]` target.
  - The macro rejects `#[serde(skip_serializing)]`, `#[serde(skip_serializing_if = "...")]`, `#[serde(flatten)]` on ANY field of an `AppConfig`-derived struct with a clear `compile_error!`.
  - The macro accepts `#[secret(store_ref = "field")]` and emits `KeyInNamedStore { store_ref_field: "field" }`.

- [ ] **Step 1: Add `AppConfigRoot` trait + `KeyInNamedStore` variant to `crates/edgezero-core/src/app_config.rs`.**

```rust
/// Marker trait emitted by `#[derive(AppConfig)]`. The §10.2.1
/// Pattern 4 CI gate detects nested AppConfig-rooted structs via
/// this marker. The trait is intentionally open (NOT sealed) so
/// the derive macro can implement it from downstream crates.
pub trait AppConfigRoot {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretKind {
    KeyInDefault,
    StoreRef,
    KeyInNamedStore { store_ref_field: &'static str },
}
```

(If `SecretKind` already exists with two variants, extend it; do NOT introduce a duplicate.)

- [ ] **Step 2: Extend the macro to parse `#[secret(store_ref = "field")]`.**

In `crates/edgezero-macros/src/app_config.rs`, find `parse_secret_kind` (around line 148). Extend the `Meta::List(list)` arm so it accepts either `store_ref` (bare path → `StoreRef`) or `store_ref = "lit"` (name-value → `KeyInNamedStore`).

Sketch:

```rust
fn parse_secret_kind(attr: &Attribute) -> Result<SecretAnnotation, syn::Error> {
    match &attr.meta {
        Meta::Path(_) => Ok(SecretAnnotation::KeyInDefault),
        Meta::List(list) => {
            // Try bare `store_ref` first.
            if let Ok(path) = syn::parse2::<Path>(list.tokens.clone()) {
                if path.is_ident("store_ref") {
                    return Ok(SecretAnnotation::StoreRef);
                }
            }
            // Try `store_ref = "field"`.
            if let Ok(nv) = syn::parse2::<MetaNameValue>(list.tokens.clone()) {
                if nv.path.is_ident("store_ref") {
                    if let Expr::Lit(ExprLit { lit: Lit::Str(s), .. }) = nv.value {
                        return Ok(SecretAnnotation::KeyInNamedStore { store_ref_field: s.value() });
                    }
                }
            }
            Err(syn::Error::new_spanned(
                &list.tokens,
                "`#[secret(...)]` accepts `store_ref` or `store_ref = \"field\"`",
            ))
        }
        _ => Err(syn::Error::new_spanned(attr, "invalid `#[secret]` form")),
    }
}
```

Update `SecretAnnotation` (the macro-internal enum) to carry the new variant.

- [ ] **Step 3: Extend the codegen to emit `KeyInNamedStore` and validate sibling existence.**

In the `expand` function in `app_config.rs` (the macro), build a sibling-name set BEFORE the codegen loop. For each `KeyInNamedStore { store_ref_field }`, error if `store_ref_field` is not present in the sibling set OR if that sibling does not have a `#[secret(store_ref)]` annotation.

Emit:

```rust
SecretAnnotation::KeyInNamedStore { store_ref_field } => {
    let lit = LitStr::new(&store_ref_field, Span::call_site());
    quote!(::edgezero_core::app_config::SecretKind::KeyInNamedStore { store_ref_field: #lit })
}
```

- [ ] **Step 4: Enforce skip/skip_if/flatten bans on EVERY field.**

In `expand`, BEFORE the per-field secret scan, walk every field and reject the three serde attributes:

```rust
fn enforce_no_disallowed_serde_attrs_on_all_fields(
    fields: &Punctuated<Field, syn::Token![,]>,
) -> Result<(), syn::Error> {
    for field in fields {
        for attr in &field.attrs {
            if !attr.path().is_ident("serde") {
                continue;
            }
            // Parse the meta list and look for forbidden idents.
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("skip_serializing")
                    || meta.path.is_ident("skip_serializing_if")
                    || meta.path.is_ident("flatten")
                {
                    return Err(meta.error(format!(
                        "`#[serde({})]` is not allowed on fields of an `AppConfig`-derived struct \
                         (it would desync the canonical-form rules in §4.2 from the serde JSON shape). \
                         If you need a flat layout, define it explicitly.",
                        meta.path.get_ident().unwrap()
                    )));
                }
                Ok(())
            })?;
        }
    }
    Ok(())
}
```

Call this from `expand` before the per-secret loop.

- [ ] **Step 5: Emit `impl AppConfigRoot for C` in the codegen.**

After the existing `impl AppConfigMeta` emission, append:

```rust
        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigRoot
            for #struct_ident #type_generics #where_clause
        {}
```

- [ ] **Step 6: Add `trybuild` tests.**

If `trybuild` is not already a dev-dep, add to `crates/edgezero-macros/Cargo.toml`:

```toml
[dev-dependencies]
trybuild = "1"
```

Create `crates/edgezero-macros/tests/derive_ui.rs`:

```rust
#[test]
fn derive_ui() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/secret_with_skip_serializing.rs");
    t.compile_fail("tests/ui/secret_with_flatten.rs");
    t.compile_fail("tests/ui/key_in_named_store_missing_sibling.rs");
    t.pass("tests/ui/secret_with_store_ref_named.rs");
}
```

Create the corresponding `.rs` fixtures under `crates/edgezero-macros/tests/ui/`. Each fixture is a 10-15 line struct that exercises one rule. For `compile_fail` fixtures, also create matching `.stderr` files (run `TRYBUILD=overwrite cargo test` once after authoring to generate them).

- [ ] **Step 7: Run, confirm pass.**

Run: `cargo test -p edgezero-macros`
Expected: pass.

## Task B11 — `TypedSecretEntry` + `validate_typed_secrets` trait extension

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs` — add `TypedSecretEntry` struct + `new` constructor.
- Modify: `crates/edgezero-adapter/src/registry.rs` — change `Adapter::validate_typed_secrets` signature to `&[TypedSecretEntry<'_>]`.
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/cli.rs` — update each impl.
- Modify: `crates/edgezero-cli/src/config.rs` — update the caller (`run_adapter_typed_checks`) to build the new slice including `KeyInNamedStore` entries.

**Interfaces:**

- Produces:
  - `pub struct TypedSecretEntry<'a> { pub store_id: &'a str, pub field_name: &'a str, pub key_value: &'a str }` (`#[non_exhaustive]`).
  - `impl<'a> TypedSecretEntry<'a> { pub fn new(store_id: &'a str, field_name: &'a str, key_value: &'a str) -> Self }`.
  - Trait: `fn validate_typed_secrets(&self, entries: &[TypedSecretEntry<'_>]) -> Result<(), String>;`

- [ ] **Step 1: Add the struct.**

In `crates/edgezero-adapter/src/registry.rs`:

```rust
/// Per-secret-key entry passed to
/// [`Adapter::validate_typed_secrets`]. `#[non_exhaustive]` for
/// v2 source-compat; construction goes through `new`.
#[non_exhaustive]
pub struct TypedSecretEntry<'a> {
    /// Logical secret-store id this key targets.
    pub store_id: &'a str,
    /// Rust struct field name (e.g. `"api_token"`).
    pub field_name: &'a str,
    /// Blob value — i.e. the secret-store KEY NAME.
    pub key_value: &'a str,
}

impl<'a> TypedSecretEntry<'a> {
    #[must_use]
    #[inline]
    pub fn new(store_id: &'a str, field_name: &'a str, key_value: &'a str) -> Self {
        Self { store_id, field_name, key_value }
    }
}
```

- [ ] **Step 2: Update the trait signature.**

```rust
    fn validate_typed_secrets(
        &self,
        entries: &[TypedSecretEntry<'_>],
    ) -> Result<(), String>;
```

- [ ] **Step 3: Update each adapter's impl.** The bulk of adapters (Axum, Cloudflare, Fastly) ignore the slice; Spin reads `entry.field_name` + `entry.key_value`:

For non-Spin adapters:

```rust
    fn validate_typed_secrets(&self, _entries: &[TypedSecretEntry<'_>]) -> Result<(), String> {
        Ok(())
    }
```

For Spin (`crates/edgezero-adapter-spin/src/cli.rs:363`), translate the existing loop from `(field_name, value)` to `entry.field_name` / `entry.key_value`. The store-id-filter is `entry.store_id == "spin"` if we want strict matching; in v1 keep the current behaviour (check all entries) for parity.

- [ ] **Step 4: Update the caller in `crates/edgezero-cli/src/config.rs::run_adapter_typed_checks`.**

Replace the `Vec<(&str, &str)>` build-loop with a `Vec<TypedSecretEntry<'_>>` that also includes `KeyInNamedStore` resolutions. Pseudocode:

```rust
let mut entries: Vec<TypedSecretEntry<'_>> = Vec::new();
let default_id = ctx.manifest().stores.secrets.as_ref().map(|s| s.default_id());
for field in C::SECRET_FIELDS {
    match field.kind {
        SecretKind::KeyInDefault => {
            let value = raw_table.get(field.name).and_then(Value::as_str);
            if let (Some(value), Some(store_id)) = (value, default_id.as_deref()) {
                entries.push(TypedSecretEntry::new(store_id, field.name, value));
            }
        }
        SecretKind::KeyInNamedStore { store_ref_field } => {
            let store_id = raw_table.get(store_ref_field).and_then(Value::as_str);
            let value = raw_table.get(field.name).and_then(Value::as_str);
            if let (Some(store_id), Some(value)) = (store_id, value) {
                entries.push(TypedSecretEntry::new(store_id, field.name, value));
            }
        }
        SecretKind::StoreRef => {}
    }
}
for name in ctx.manifest().adapters.keys() {
    if let Some(adapter) = adapter_registry::get_adapter(name) {
        adapter.validate_typed_secrets(&entries)?;
    }
}
```

- [ ] **Step 5: Run the workspace test suite to flush out any caller I missed.**

Run: `cargo test --workspace --all-targets`
Expected: pass.

## Task B12 — `Adapter::read_config_entry` trait + `ReadConfigEntry` enum (writer-signature mirror)

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs` — add the enum + two trait methods that mirror `push_config_entries` / `push_config_entries_local` argument-for-argument.

**Interfaces:**

- Produces:
  - `pub enum ReadConfigEntry { Present(String), MissingKey, MissingStore, Unsupported(&'static str) }`.
  - Trait methods `read_config_entry` and `read_config_entry_local` whose parameter lists are byte-for-byte identical to the existing `push_config_entries` / `push_config_entries_local` at `crates/edgezero-adapter/src/registry.rs:277` and `:315`, EXCEPT:
    - `entries: &[(String, String)]` → `key: &str` (read returns one entry, not a list)
    - `dry_run: bool` → dropped (read has no side effects)
    - Return is `Result<ReadConfigEntry, String>` instead of `Result<Vec<String>, String>`.

- [ ] **Step 1: Re-read the existing writer signature so the new read methods stay parameter-for-parameter aligned.**

```bash
sed -n '270,330p' crates/edgezero-adapter/src/registry.rs
```

Note `AdapterPushContext<'ctx>`'s three fields (`local`, `manifest_adapter_deploy_cmd`, `runtime_config_path`) — the read methods consume the same context so Spin can branch on `manifest_adapter_deploy_cmd` (cloud-vs-local) and Axum can resolve the file path under `manifest_root`.

- [ ] **Step 2: Add `ReadConfigEntry` enum.**

```rust
/// Outcome of a single-key read. See spec §9.0.
pub enum ReadConfigEntry {
    /// The remote held the key; the body is the serialised envelope JSON.
    Present(String),
    /// The store exists but the key is absent (operator hasn't pushed yet,
    /// or pushed under a different key).
    MissingKey,
    /// The store itself is absent — wrangler.toml has no matching binding,
    /// fastly.toml has no setup table, axum's local-config-<id>.json file
    /// doesn't exist yet.
    MissingStore,
    /// The adapter cannot query the backend for this entry — e.g. Spin
    /// Cloud's CLI exposes no `get`. `&'static str` carries the human-
    /// readable reason. See spec §8.3 four-branch UX.
    Unsupported(&'static str),
}
```

- [ ] **Step 3: Add the two trait methods. Each parameter list mirrors the corresponding writer EXACTLY** so adapter impls can reuse their existing manifest-root + adapter-manifest-path resolution helpers without rewriting:

```rust
    /// Single-key read against the LIVE platform. Mirrors
    /// [`Self::push_config_entries`]'s argument list per spec §9.0 so
    /// adapters can share helpers (`find_namespace_id` for Cloudflare,
    /// `resolve_label_for_store` for Spin, etc.).
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors push_config_entries (see its clippy::allow rationale)."
    )]
    fn read_config_entry(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        Ok(ReadConfigEntry::Unsupported("adapter does not implement remote read-back"))
    }

    /// Single-key read against the LOCAL emulator state. Mirrors
    /// [`Self::push_config_entries_local`].
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors push_config_entries_local."
    )]
    fn read_config_entry_local(
        &self,
        _manifest_root: &Path,
        _adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        _store: &ResolvedStoreId,
        _key: &str,
        _push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String> {
        Ok(ReadConfigEntry::Unsupported("adapter does not implement local read-back"))
    }
```

- [ ] **Step 4: Run, confirm clean.**

Run: `cargo check --workspace --all-targets`
Expected: clean.

## Task B13 — Per-adapter `read_config_entry` + `_local` impls

**Files:**

- Modify: `crates/edgezero-adapter-axum/src/cli.rs` — implement both methods (file-map reads).
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs` — `wrangler kv key get --binding <B> <K> --remote` for `read_config_entry`; `--local` for `_local`.
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs` — `fastly config-store-entry describe --store-id=<id> --key=<key> --json`.
- Modify: `crates/edgezero-adapter-spin/src/cli.rs` — local: SQLite read via vendored schema; cloud: returns `Unsupported`.

**Interfaces:**

- Consumes: Task B12's trait.

Each adapter has a similar shape. Outline per adapter; flesh out per spec §9.0/9.1-9.4.

- [ ] **Step 1: Axum.** Parse `manifest_root/.edgezero/local-config-<id>.json` as `BTreeMap<String, String>`, look up the key, return `Present` / `MissingKey` / `MissingStore`. The signature now takes the writer-matching parameter list (`manifest_root: &Path`, `store: &ResolvedStoreId`, `key: &str`, …).

```rust
fn read_config_entry_local(
    &self,
    manifest_root: &Path,
    _adapter_manifest_path: Option<&str>,
    _component_selector: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    _push_ctx: &AdapterPushContext<'_>,
) -> Result<ReadConfigEntry, String> {
    let path = manifest_root
        .join(".edgezero")
        .join(format!("local-config-{}.json", store.logical));
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReadConfigEntry::MissingStore),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
        Ok(s) => {
            let map: std::collections::BTreeMap<String, String> = serde_json::from_str(&s)
                .map_err(|e| format!("failed to parse {}: {e}", path.display()))?;
            match map.get(key) {
                Some(v) => Ok(ReadConfigEntry::Present(v.clone())),
                None => Ok(ReadConfigEntry::MissingKey),
            }
        }
    }
}

fn read_config_entry(
    &self,
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    component_selector: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    push_ctx: &AdapterPushContext<'_>,
) -> Result<ReadConfigEntry, String> {
    // Axum has no "remote" — delegate to the local impl.
    self.read_config_entry_local(
        manifest_root, adapter_manifest_path, component_selector, store, key, push_ctx,
    )
}
```

- [ ] **Step 2: Cloudflare.** Shell out to `wrangler kv key get --binding <BINDING> <KEY> --remote` (and `--local` for `_local`). Map exit codes / stderr to the four variants. The writer's `find_namespace_id(&wrangler_path, binding)` helper at `crates/edgezero-adapter-cloudflare/src/cli.rs:326` is reusable here.

```rust
fn read_config_entry(
    &self,
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    _component_selector: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    _push_ctx: &AdapterPushContext<'_>,
) -> Result<ReadConfigEntry, String> {
    let rel = adapter_manifest_path.ok_or_else(|| {
        "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config diff".to_owned()
    })?;
    let wrangler_path = manifest_root.join(rel);
    let binding = store.platform.as_str();
    let output = std::process::Command::new("wrangler")
        .args(["kv", "key", "get", "--binding", binding, key, "--remote"])
        .current_dir(wrangler_path.parent().unwrap())
        .output()
        .map_err(|e| format!("failed to spawn wrangler: {e}"))?;
    if output.status.success() {
        let body = String::from_utf8(output.stdout).map_err(|e| format!("wrangler stdout not UTF-8: {e}"))?;
        Ok(ReadConfigEntry::Present(body))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not found") || stderr.contains("does not exist") {
            Ok(ReadConfigEntry::MissingKey)
        } else if stderr.contains("binding") || stderr.contains("Binding") {
            Ok(ReadConfigEntry::MissingStore)
        } else {
            Err(format!("wrangler kv key get failed: {stderr}"))
        }
    }
}
```

`_local` is identical but with `--local` instead of `--remote`.

- [ ] **Step 3: Fastly.** Shell out to `fastly config-store-entry describe --store-id=<id> --key=<key> --json`. Parse JSON, extract `item_value`, return `Present`. Map "not found" stderr to `MissingKey`.

- [ ] **Step 4: Spin — mirror the FULL write dispatch.** Spec §9.0 says read-back must use the same variant as the write path. The Spin writer's `dispatch_push` at `crates/edgezero-adapter-spin/src/cli.rs:514` has four branches (per its doc-comment at `cli.rs:498-513`); the read path needs each one:

| Write-side branch                                                                 | Trigger                                                                              | Read-side behaviour                                                                                                |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| 1. **`--local` forces SQLite-direct, regardless of runtime-config backend type** | `push_ctx.local == true`                                                             | `read_config_entry_local`: parse runtime-config to resolve the SQLite path (honour `type = "spin"` `path` override; otherwise default `.spin/sqlite_key_value.db`); read the row at `(store=<platform>, key=<key>)`. `Present(value)` / `MissingKey` per row presence; `MissingStore` if the SQLite file doesn't exist. |
| 2. **Manifest deploy command targets Fermyon Cloud**                             | `push_cloud::deploy_command_targets_fermyon_cloud(push_ctx.manifest_adapter_deploy_cmd)` | `read_config_entry`: returns `Unsupported("Spin Cloud key-value CLI exposes no `get`; remote read-back unsupported in v1")` per §8.3 / §9.4. NO shell-out to `spin cloud key-value list` or similar — that lists stores, not keys (round-20 cleanup recipe). |
| 3. **`runtime-config.toml` declares a non-Spin backend**                         | `parsed.key_value_stores.get(platform) == Some(Redis { .. } | AzureCosmos | Unknown)` | `read_config_entry`: error with the same message the writer uses at `cli.rs:632-640` ("backend type `redis` for label `<X>` — use `redis-cli GET <key>` or the equivalent; edgezero does not read from this backend").                                                                                                  |
| 4. **Default — runtime-config absent or `type = "spin"`**                        | Anything else                                                                        | `read_config_entry`: same as branch 1 (SQLite-direct read).                                                                                                                                                                                                                                                              |

Sketch:

```rust
fn read_config_entry(
    &self,
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    _component_selector: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    push_ctx: &AdapterPushContext<'_>,
) -> Result<ReadConfigEntry, String> {
    let platform = store.platform.as_str();
    let spin_manifest_path = adapter_manifest_path
        .map(|rel| manifest_root.join(rel))
        .ok_or_else(|| "[adapters.spin.adapter].manifest must point at spin.toml for config diff".to_owned())?;
    let spin_manifest_dir = spin_manifest_path.parent().unwrap_or(manifest_root);
    let runtime_config_path = push_ctx.runtime_config_path.map_or_else(
        || spin_manifest_dir.join("runtime-config.toml"),
        Path::to_path_buf,
    );

    // Branch 2: Fermyon Cloud auto-detect via deploy command.
    if push_cloud::deploy_command_targets_fermyon_cloud(push_ctx.manifest_adapter_deploy_cmd) {
        return Ok(ReadConfigEntry::Unsupported(
            "Spin Cloud key-value CLI exposes no `get`; remote read-back unsupported in v1"
        ));
    }

    // Branches 3 + 4: parse runtime-config (if present) and dispatch on backend type.
    let parsed = runtime_config::read(&runtime_config_path).ok();
    let backend = parsed.as_ref().and_then(|p| p.key_value_stores.get(platform));
    match backend {
        Some(runtime_config::KeyValueBackend::Redis { .. }) => Err(format!(
            "label `{platform}` in {} is type `redis`; use `redis-cli GET <key>` to read this store directly. \
             edgezero does not read from redis backends.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::AzureCosmos) => Err(format!(
            "label `{platform}` in {} is type `azure_cosmos`; use the Azure CosmosDB CLI to read this store. \
             edgezero does not read from azure_cosmos backends.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Unknown { type_str }) => Err(format!(
            "label `{platform}` in {} is unknown backend type `{type_str}`; edgezero only reads from `type = \"spin\"` (SQLite) backends.",
            runtime_config_path.display()
        )),
        Some(runtime_config::KeyValueBackend::Spin { path }) | None => {
            // Branch 4 (default) or branch 3 → SQLite-direct read.
            let sqlite_path = path
                .as_deref()
                .map(|p| spin_manifest_dir.join(p))
                .unwrap_or_else(|| spin_manifest_dir.join(".spin").join("sqlite_key_value.db"));
            read_sqlite(&sqlite_path, platform, key)
        }
    }
}

fn read_config_entry_local(
    &self,
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    component_selector: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    push_ctx: &AdapterPushContext<'_>,
) -> Result<ReadConfigEntry, String> {
    // Branch 1: --local always SQLite-direct (write-side parallel at cli.rs:565).
    // Reuse the SQLite reader; ignore Fermyon-Cloud auto-detect.
    // (Implementation matches the write branch 1 path with adapter-side
    // verify_label_declared + write_sqlite, but read-side.)
    ...
}

fn read_sqlite(sqlite_path: &Path, store: &str, key: &str) -> Result<ReadConfigEntry, String> {
    if !sqlite_path.exists() {
        return Ok(ReadConfigEntry::MissingStore);
    }
    // Use the vendored spin_key_value schema (same module the writers
    // use): query `SELECT value FROM spin_key_value WHERE store = ? AND key = ?`.
    // Return Present / MissingKey based on row presence.
    ...
}
```

This branch logic is intentionally large because spec §9.0 says "read-back uses the same variant as the write path" — silently collapsing all four into "SQLite or Unsupported" would leave Redis/Azure operators with a confusing diff error and would not detect the Fermyon Cloud branch correctly.

- [ ] **Step 5: Add unit tests for each adapter.** For shell-out adapters, mock with a test scaffold that uses a fake `wrangler` / `fastly` script on PATH. For Axum, write a temp file and assert directly. For Spin, exercise each of the four branches with a fixture `runtime-config.toml`.

- [ ] **Step 6: Run.**

Run: `cargo test --workspace --all-targets`
Expected: pass.

## Task B14 — Commit Phase B

- [ ] **Step 1: Run the four CI gates.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Expected: all four pass.

- [ ] **Step 2: Stage + commit Phase B.**

```bash
git add crates/edgezero-core/src/manifest.rs \
        crates/edgezero-core/src/error.rs \
        crates/edgezero-core/src/store_registry.rs \
        crates/edgezero-core/src/extractor.rs \
        crates/edgezero-core/src/context.rs \
        crates/edgezero-core/src/env_config.rs \
        crates/edgezero-core/src/app_config.rs \
        crates/edgezero-core/Cargo.toml \
        crates/edgezero-macros/src/app_config.rs \
        crates/edgezero-macros/tests/ \
        crates/edgezero-macros/Cargo.toml \
        crates/edgezero-adapter/src/registry.rs \
        crates/edgezero-adapter-axum/src/ \
        crates/edgezero-adapter-cloudflare/src/ \
        crates/edgezero-adapter-fastly/src/ \
        crates/edgezero-adapter-spin/src/ \
        crates/edgezero-cli/src/config.rs \
        Cargo.lock
git commit -m "Add binding + manifest charset + EnvConfig::store_key + read trait

Phase B of the blob app-config rewrite per
docs/superpowers/specs/2026-06-16-blob-app-config.md.

- §5.2 manifest store-id charset tightened from [A-Za-z0-9_-]
  to [A-Za-z0-9_]+ so the EDGEZERO__STORES__<KIND>__<ID>__KEY
  override is POSIX-shell-exportable. Hyphens rejected at
  manifest validation with an actionable error.
- §5.2.1 ConfigStoreBinding { handle, default_key } replaces
  ConfigStoreHandle as the ConfigRegistry value type. Adapters
  build the binding via EnvConfig::store_key('config', id).
  Config::default() still returns ConfigStoreHandle (unwraps
  binding.handle); new default_binding() accessor returns the
  binding for the typed extractor.
- §6.3.1 EdgeError gains the ConfigOutOfDate variant + two
  constructors (config_out_of_date / config_out_of_date_from_serde).
  Response body adds a stable `kind` field on every variant;
  Retry-After: 60 fires only on ConfigOutOfDate.
- §3.3.1 SecretKind::KeyInNamedStore { store_ref_field } added;
  the #[derive(AppConfig)] macro accepts #[secret(store_ref =
  \"field\")], rejects #[serde(skip_serializing / skip_serializing_if
  / flatten)] on every field, and emits impl AppConfigRoot for C.
- §3.3.8 validate_excluding_secrets wrapper added.
- §9.0 Adapter::read_config_entry + ReadConfigEntry enum
  (Present / MissingKey / MissingStore / Unsupported). All four
  adapters implement read + read_local; Spin Cloud returns
  Unsupported per round-7 Fermyon-CLI-surface check.

No in-tree caller exercises the new types yet; Phase C wires
them into the extractor + push rewrite + app-demo migration."
```

**STOP for user review before continuing to Phase C.**

---

# Phase C — THE ATOMIC CUTOVER (Commit C)

**Goal:** Land the runtime extractor, per-adapter writers, app-demo migration, scaffold templates, and all three CI gates in ONE commit. Per spec §10.1 + §10.2, splitting any of these into separate commits leaves an unbisectable intermediate state.

Tasks land incrementally locally, but the final commit lumps them all together. Each task ends with `cargo test --workspace` passing; the final task adds the gates and commits.

**Important:** because Phase C is one commit, tasks within it can be reordered as long as the final commit is coherent. The order below maps cleanly to the spec's flow.

## Task C1 — `AppConfig<C>` extractor (skeleton + envelope + sha + secret walk + deserialise)

**Files:**

- Modify: `crates/edgezero-core/src/extractor.rs` — add `pub struct AppConfig<C>(pub C);` + `impl FromRequest`.

**Interfaces:**

- Produces: `pub struct AppConfig<C>(pub C); impl<C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static> FromRequest for AppConfig<C>`.

- [ ] **Step 1: Write the extractor.**

Append to `crates/edgezero-core/src/extractor.rs`:

````rust
/// Typed app-config extractor. See spec §3.3.3 + §4.3.
///
/// ```ignore
/// #[action]
/// pub async fn handler(AppConfig(cfg): AppConfig<MyConfig>) -> Result<Response, EdgeError> {
///     // cfg.api_token is the RESOLVED secret value, not the key name.
///     Ok(text(cfg.greeting))
/// }
/// ```
pub struct AppConfig<C>(pub C);

#[async_trait(?Send)]
impl<C> FromRequest for AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no default config store registered — check [stores.config] in edgezero.toml"
            ))
        })?;
        let key = &binding.default_key;
        let raw = binding
            .handle
            .get(key)
            .await
            .map_err(|e| EdgeError::internal(anyhow::anyhow!(e)))?
            .ok_or_else(|| EdgeError::config_out_of_date(
                format!("missing typed app-config blob at key `{key}` — run `<app-cli> config push` for this deploy"),
                String::new(),
            ))?;
        let envelope: BlobEnvelope = serde_json::from_str(&raw)
            .map_err(|e| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {e}")))?;
        envelope
            .verify()
            .map_err(|e| EdgeError::internal(anyhow::anyhow!("envelope verification failed: {e}")))?;
        let mut data = envelope.into_data();
        // Secret walk per §3.3.3.
        secret_walk::<C>(ctx, &mut data).await?;
        // Deserialise via serde_path_to_error to preserve field_path
        // for ConfigOutOfDate per §4.3.
        use serde::de::IntoDeserializer as _;
        let cfg: C = serde_path_to_error::deserialize(data.into_deserializer())
            .map_err(EdgeError::config_out_of_date_from_serde)?;
        cfg.validate().map_err(|err| {
            EdgeError::config_out_of_date(
                err.to_string(),
                first_violating_field(&err).unwrap_or_default(),
            )
        })?;
        Ok(AppConfig(cfg))
    }
}

fn first_violating_field(errors: &validator::ValidationErrors) -> Option<String> {
    let mut keys: Vec<&'static str> = errors.errors().keys().copied().collect();
    keys.sort();
    keys.first().map(|k| (*k).to_string())
}

async fn secret_walk<C>(
    ctx: &RequestContext,
    data: &mut serde_json::Value,
) -> Result<(), EdgeError>
where
    C: AppConfigMeta,
{
    let data_obj = data.as_object_mut().ok_or_else(|| {
        EdgeError::internal(anyhow::anyhow!("blob `data` is not a JSON object"))
    })?;
    for field in C::SECRET_FIELDS {
        let key_name = data_obj
            .get(field.name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| EdgeError::config_out_of_date(
                format!("missing or non-string value at `{}`", field.name),
                field.name.to_owned(),
            ))?
            .to_owned();
        let (bound, resolved_store_id) = match field.kind {
            SecretKind::KeyInDefault => {
                let bound = ctx.secret_store_default().ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!(
                            "secret field `{}` has kind KeyInDefault but no default secret store is registered",
                            field.name,
                        ),
                        field.name.to_owned(),
                    )
                })?;
                let id = bound.store_name().to_owned();
                (bound, id)
            }
            SecretKind::StoreRef => continue,
            SecretKind::KeyInNamedStore { store_ref_field } => {
                let store_id_str = data_obj
                    .get(store_ref_field)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| EdgeError::config_out_of_date(
                        format!("missing store_ref `{store_ref_field}` for secret field `{}`", field.name),
                        field.name.to_owned(),
                    ))?
                    .to_owned();
                let bound = ctx.secret_store(&store_id_str).ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!("blob declared store_ref `{store_id_str}` but [stores.secrets] has no such id"),
                        field.name.to_owned(),
                    )
                })?;
                (bound, store_id_str)
            }
        };
        let secret = bound.require_str(&key_name).await.map_err(|err| {
            map_secret_error(err, field.name, &resolved_store_id, &key_name)
        })?;
        data_obj.insert(field.name.to_owned(), serde_json::Value::String(secret));
    }
    Ok(())
}

fn map_secret_error(
    err: crate::secret_store::SecretError,
    field_name: &str,
    store_id: &str,
    key_name: &str,
) -> EdgeError {
    use crate::secret_store::SecretError;
    match err {
        SecretError::NotFound { name } => EdgeError::config_out_of_date(
            format!("secret `{name}` in store `{store_id}` not found"),
            field_name.to_owned(),
        ),
        SecretError::Validation(msg) => EdgeError::config_out_of_date(
            format!("secret `{key_name}` in store `{store_id}` rejected: {msg}"),
            field_name.to_owned(),
        ),
        SecretError::Unavailable => EdgeError::service_unavailable(format!(
            "secret store `{store_id}` unreachable"
        )),
        SecretError::Internal(source) => EdgeError::internal(anyhow::anyhow!(
            "secret `{key_name}` in store `{store_id}` produced unexpected store error: {source}"
        )),
    }
}
````

Adjust imports at the top of `extractor.rs` to bring in `crate::blob_envelope::BlobEnvelope`, `crate::app_config::{AppConfigMeta, SecretKind}`, `crate::secret_store::SecretError` (referenced via `crate::` because `extractor.rs` is inside `edgezero-core`).

- [ ] **Step 2: Add the explicit-key `named` and cross-store `from_store` inherent methods (spec §6.2 + §6.2.1).**

```rust
impl<C> AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    /// Read the typed config from the default store under an
    /// EXPLICIT key (instead of the binding's `default_key`).
    /// Returns the inner `C` directly per spec §6.2 — handlers
    /// usually destructure the FromRequest extractor; the inherent
    /// methods exist for call sites that need a different key or
    /// store and prefer the bare `C` over wrapping/unwrapping.
    pub async fn named(ctx: &RequestContext, key: &str) -> Result<C, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no default config store registered — check [stores.config] in edgezero.toml"
            ))
        })?;
        extract_from_handle::<C>(ctx, &binding.handle, key).await
    }

    /// Read the typed config from a NON-default config store.
    /// `key = None` falls back to that store's `binding.default_key`.
    /// Returns the inner `C` directly per spec §6.2.1.
    pub async fn from_store(
        ctx: &RequestContext,
        store_id: &str,
        key: Option<&str>,
    ) -> Result<C, EdgeError> {
        let binding = ctx.config_store_binding(store_id).ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no config store registered for id `{store_id}`"
            ))
        })?;
        let key = key.unwrap_or(&binding.default_key);
        extract_from_handle::<C>(ctx, &binding.handle, key).await
    }
}

/// Shared body: fetch + envelope + sha + secret walk + deserialise
/// + validate. The `FromRequest` impl above delegates to this so
/// the three entry points share one implementation.
async fn extract_from_handle<C>(
    ctx: &RequestContext,
    handle: &crate::config_store::ConfigStoreHandle,
    key: &str,
) -> Result<C, EdgeError>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    let raw = handle
        .get(key)
        .await
        .map_err(|e| EdgeError::internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| EdgeError::config_out_of_date(
            format!("missing typed app-config blob at key `{key}` — run `<app-cli> config push` for this deploy"),
            String::new(),
        ))?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)
        .map_err(|e| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {e}")))?;
    envelope.verify().map_err(|e| {
        EdgeError::internal(anyhow::anyhow!("envelope verification failed: {e}"))
    })?;
    let mut data = envelope.into_data();
    secret_walk::<C>(ctx, &mut data).await?;
    use serde::de::IntoDeserializer as _;
    let cfg: C = serde_path_to_error::deserialize(data.into_deserializer())
        .map_err(EdgeError::config_out_of_date_from_serde)?;
    cfg.validate().map_err(|err| {
        EdgeError::config_out_of_date(
            err.to_string(),
            first_violating_field(&err).unwrap_or_default(),
        )
    })?;
    Ok(cfg)
}
```

Refactor the `FromRequest` impl above to delegate to `extract_from_handle`:

```rust
#[async_trait(?Send)]
impl<C> FromRequest for AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        let binding = ctx.config_store_default_binding().ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no default config store registered — check [stores.config] in edgezero.toml"
            ))
        })?;
        let key = binding.default_key.clone();
        extract_from_handle::<C>(ctx, &binding.handle, &key).await.map(AppConfig)
    }
}
```

- [ ] **Step 3: Add tests for `named` and `from_store`.**

Per spec §12.1 "AppConfig<C> extractor" bullet list: "`named(key)` reads a different key from the same store" — assert it. Plus add a test for `from_store` reading a non-default `[stores.config]` id.

- [ ] **Step 4: Write a missing-blob test.**

```rust
    #[test]
    fn app_config_extractor_returns_config_out_of_date_on_missing_blob() {
        // Mock a ConfigStore whose get() returns Ok(None).
        struct EmptyStore;
        #[async_trait(?Send)]
        impl ConfigStore for EmptyStore {
            async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> { Ok(None) }
        }
        // ... build a request context with that store via a binding ...
        // Run extract; assert Err(EdgeError::ConfigOutOfDate { .. }).
        // Assert message contains "missing typed app-config blob" + "run `<app-cli> config push`".
    }
```

- [ ] **Step 5: Run.** Expected: pass.

Run: `cargo test -p edgezero-core --lib extractor::tests::app_config`

## Task C2 — CLI envelope build + per-adapter writer call (NOT per-adapter envelope code)

**Files:**

- Modify: `crates/edgezero-cli/src/config.rs` — `run_config_push_typed` builds ONE `BlobEnvelope`, serialises it, resolves the target key, and calls the EXISTING `adapter.push_config_entries(...)` writer with exactly one `(key, envelope_json)` entry.
- Modify: each adapter under `crates/edgezero-adapter-*/src/cli.rs` (and `push_sqlite.rs` / `push_cloud.rs` for Spin) ONLY for: (a) any pre-blob-model "flatten + per-leaf loop" preprocessing the writer did, and (b) per-platform cap checks on the single entry. Adapters do NOT touch `BlobEnvelope` — the `(String, String)` writer surface stays exactly as it is at `crates/edgezero-adapter/src/registry.rs:277`.

**Ownership rationale (round-26 H-2):** the existing `push_config_entries` trait method takes `entries: &[(String, String)]` — a sequence of key/value pairs. Adapters don't see `C` or `cfg`. Earlier draft of this task told each adapter to build the envelope from `cfg`, which would have required widening the trait to take `C` (breaking the adapter abstraction) OR duplicating envelope-construction code across all four adapters. The clean ownership: **CLI builds the envelope once, then hands the writer ONE entry: `(resolved_key, envelope_json)`.** Adapters just write what they're handed, exactly like the per-leaf model did.

- [ ] **Step 1: In `run_config_push_typed`, build the envelope after `validate_excluding_secrets`.**

```rust
// crates/edgezero-cli/src/config.rs

// After validate_excluding_secrets(&cfg)?, build the envelope:
let data: serde_json::Value = serde_json::to_value(&cfg)
    .map_err(|e| format!("failed to serialise typed config: {e}"))?;
let envelope = BlobEnvelope::new(data, generated_at_rfc3339());
let body = serde_json::to_string(&envelope)
    .map_err(|e| format!("failed to serialise envelope: {e}"))?;

// Resolve the target key per §5.4 (--key override) / §5.1 (default).
let key = args.key.clone().unwrap_or_else(|| store.logical.clone());

// Call the EXISTING per-adapter writer with one entry.
let entries: Vec<(String, String)> = vec![(key.clone(), body.clone())];
if args.local {
    adapter.push_config_entries_local(
        &manifest_root,
        adapter_manifest_path,
        component_selector,
        &store,
        &entries,
        &push_ctx,
        args.dry_run,
    )?;
} else {
    adapter.push_config_entries(
        &manifest_root,
        adapter_manifest_path,
        component_selector,
        &store,
        &entries,
        &push_ctx,
        args.dry_run,
    )?;
}
```

`generated_at_rfc3339()` is a small helper in `crates/edgezero-cli/src/config.rs`. `chrono = "0.4"` is ALREADY in `[workspace.dependencies]` (`Cargo.toml:33`); add `chrono = { workspace = true }` to `crates/edgezero-cli/Cargo.toml`'s `[dependencies]` and use it directly — no manual formatter needed:

```rust
fn generated_at_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
```

(`to_rfc3339_opts(SecondsFormat::Secs, true)` emits `2026-06-17T18:42:31Z` — RFC3339 UTC with second precision and the trailing `Z`, matching the spec §4.1 example. Deterministic at second granularity means the envelope's `generated_at` only varies across the second boundary, which is fine for the §4.2 SHA contract: `generated_at` is NOT part of the SHA — see §4.1's "Informational only; not part of the sha" bullet.)

The plan's earlier `format_utc` placeholder is gone — round-27 reviewer flagged it as undefined.

- [ ] **Step 2: Per-adapter platform-cap checks.** Adapters that need a pre-platform-call cap check enforce it inside their existing writer. The trait still takes `entries: &[(String, String)]`, so adapter code reads `entries[0].1.len()`:

For **Fastly** (existing writer at `crates/edgezero-adapter-fastly/src/cli.rs`, the `push_config_entries` impl):

```rust
// Pre-platform-call guard for the blob model. Each entry is a
// single envelope JSON string.
for (key, body) in entries {
    if body.len() > 8000 {
        return Err(format!(
            "blob at key `{key}` is {} characters; Fastly Config Store \
             entry value limit is 8 000 characters. Restructure your \
             typed app-config into multiple types and split across \
             [stores.config] ids.",
            body.len(),
        ));
    }
}
```

For **Spin Cloud** (at `crates/edgezero-adapter-spin/src/cli/push_cloud.rs`):

The existing cap check at line 90 (`if pair.len() >= MAX_ARGV_BYTES_PER_INVOCATION`) already fires on the single envelope pair. No new code; just update the error message to point at the §9.4 restructure remediation rather than the legacy per-leaf workaround.

- [ ] **Step 3: Per-adapter writer simplification.** Each adapter's `push_config_entries` impl currently handles the per-leaf model (`for (key, value) in entries { write one }`). Under the blob model the CLI sends ONE entry. The existing loop already handles "one entry" as a degenerate case; minimal code change to most adapters:

| Adapter          | Existing writer call                                                                                                   | Blob-model change                                                                                                                                                                                                                                                                       |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Axum             | inserts each pair into the file map; serialises file at end                                                            | No code change — the one-entry case already works.                                                                                                                                                                                                                                      |
| Cloudflare       | `wrangler kv bulk put <tempfile> --namespace-id=<id> --remote`                                                         | No code change — the one-entry tempfile is still valid bulk-put input.                                                                                                                                                                                                                  |
| Cloudflare local | `wrangler kv bulk put <tempfile> --binding <BINDING> --local`                                                          | No code change.                                                                                                                                                                                                                                                                         |
| Fastly           | per-entry `fastly config-store-entry update --upsert --stdin`                                                          | No code change; the existing loop runs once. Add the cap guard from Step 2.                                                                                                                                                                                                             |
| Spin local       | per-entry SQLite insert                                                                                                | No code change.                                                                                                                                                                                                                                                                         |
| Spin cloud       | chunked `spin cloud key-value set --app <APP> --label <LABEL> <KEY>=<VALUE> [...]` per `push_cloud.rs`'s `write_batch` | **Reuse `write_batch` unchanged with the one-pair vec** (round-26 M-2). The existing diagnostics (auth/link/partial-failure messages at `push_cloud.rs:166`, `:220`, `:235`) stay intact. DO NOT replace `output()` with `status()` — that would regress the actionable error messages. |

- [ ] **Step 4: Per-adapter tests.** For each adapter's existing per-leaf push test, update the input from a multi-entry slice to a single envelope pair. Assert the same writer interaction (shell command shape, file contents, etc.) as the per-leaf test asserted, just with one key.

- [ ] **Step 5: Run.**

```bash
cargo test --workspace --all-targets
```

Expected: pass.

## Task C3 — `ConfigPushArgs` updates + `ConfigCmd` stub variants

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` — add `--key`, `--yes`, `--no-diff` to `ConfigPushArgs`; add `ConfigCmdStubArgs`.
- Modify: `crates/edgezero-cli/src/main.rs` — wire bundled `ConfigCmd` with stub variants + parent `after_help`.

- [ ] **Step 1: Extend `ConfigPushArgs`.**

```rust
    /// Override the default key — §5.4.
    #[arg(long)]
    pub key: Option<String>,
    /// Skip the inline diff prompt and write unconditionally.
    #[arg(long, short)]
    pub yes: bool,
    /// Skip the inline diff render.
    #[arg(long)]
    pub no_diff: bool,
```

- [ ] **Step 2: Add `ConfigCmdStubArgs` + `STUB_POINTER_AFTER_HELP` constant.**

```rust
pub const STUB_POINTER_AFTER_HELP: &str = "\
This command requires a typed app-config struct (`C`) and runs from your generated downstream \
CLI, not the bundled `edgezero` binary. Run `<your-app>-cli config push` (or `... diff`) \
instead. See `<your-app>-cli config push --help`.";

#[derive(clap::Args, Debug)]
pub struct ConfigCmdStubArgs {
    /// Hidden catch-all sink (see spec §3.2.2).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub trailing: Vec<String>,
}
```

- [ ] **Step 3: Update bundled `ConfigCmd` enum to use stub variants.**

In whatever module declares `ConfigCmd` (likely `args.rs`):

```rust
#[derive(clap::Subcommand, Debug)]
pub enum ConfigCmd {
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Push(ConfigCmdStubArgs),
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Diff(ConfigCmdStubArgs),
    Validate(ConfigValidateArgs),
}
```

- [ ] **Step 4: Wire parent `Command::Config` with `subcommand` + `after_help`.**

In the top-level `Command` enum (typically `args.rs`):

```rust
    #[command(subcommand, after_help = crate::args::STUB_POINTER_AFTER_HELP)]
    Config(ConfigCmd),
```

- [ ] **Step 5: Update bundled `main.rs` to print the pointer + exit 2 for `Push` / `Diff`.**

```rust
        Command::Config(ConfigCmd::Push(_)) | Command::Config(ConfigCmd::Diff(_)) => {
            eprintln!("{}", args::STUB_POINTER_AFTER_HELP);
            std::process::exit(2);
        }
        Command::Config(ConfigCmd::Validate(v)) => config::run_config_validate(&v),
```

- [ ] **Step 6: Add tests for parse + behaviour.** Three surfaces per spec §12.8:
  - Bare invocation prints pointer + exits 2.
  - With-flags invocation (catch-all path) also prints pointer + exits 2.
  - `--help` output contains the pointer text byte-for-byte AND does NOT contain `[TRAILING]`.

## Task C4 — `run_config_push_typed` rewrite + inline-diff prompt

**Files:**

- Modify: `crates/edgezero-cli/src/config.rs` — rewrite `run_config_push_typed` to route through `deserialize_app_config_with_options` + `validate_excluding_secrets` + read-back via `Adapter::read_config_entry` + inline diff prompt.

- [ ] **Step 1: Locate `run_config_push_typed` (around line 186 per spec).** Replace the inline `Validate::validate` call with `validate_excluding_secrets`.

- [ ] **Step 2: Add the read-back step.** After validation, call the adapter's `read_config_entry` (or `_local` if `--local`) with the same parameter list the writer takes (per Task B12's writer-mirror signature), and compare shas:

```rust
// Resolve the same parameters the writer uses, then call the
// read-back. Parameter list MATCHES push_config_entries
// argument-for-argument (Task B12), so reuse the same locals.
let remote = if args.local {
    adapter.read_config_entry_local(
        &manifest_root,
        adapter_manifest_path,
        component_selector,
        &store,
        &key,
        &push_ctx,
    )?
} else {
    adapter.read_config_entry(
        &manifest_root,
        adapter_manifest_path,
        component_selector,
        &store,
        &key,
        &push_ctx,
    )?
};
let local_envelope = BlobEnvelope::new(data.clone(), generated_at);
let local_sha = local_envelope.sha256.clone();
match remote {
    ReadConfigEntry::Present(body) => {
        let remote_envelope: BlobEnvelope = serde_json::from_str(&body)?;
        if remote_envelope.sha256 == local_sha {
            println!("# no changes (sha256 matches: {local_sha})");
            return Ok(());
        }
        // Render diff per §8.1.1 (Phase D ships the formatters; for
        // Phase C, render a minimal unified diff inline).
        if !args.no_diff {
            print_diff(&remote_envelope.data, &local_envelope.data, &local_sha, &remote_envelope.sha256);
        }
    }
    ReadConfigEntry::MissingKey | ReadConfigEntry::MissingStore => {
        // No remote sha to skip against; consent gate (Step 3) still runs.
    }
    ReadConfigEntry::Unsupported(_) => {
        // Adapter can't read back (Spin Cloud per §9.4). The §8.3
        // four-branch UX applies here; see Step 3 below.
    }
}
```

- [ ] **Step 3: Add the consent gate (§8.2) + Spin-Cloud-specific four-branch UX (§8.3).**

The §8.2 default consent rules apply to ALL adapters AND to all `ReadConfigEntry` variants — even when there's no remote sha to compare against, the operator's explicit consent is still required before a write.

```rust
fn require_consent(args: &ConfigPushArgs, read: &ReadConfigEntry) -> Result<(), String> {
    if args.yes {
        return Ok(()); // explicit consent
    }
    if args.dry_run {
        return Ok(()); // no write to consent for
    }
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Prompt
        eprint!("Apply changes? [y/N] ");
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).map_err(|e| format!("stdin read failed: {e}"))?;
        if !matches!(buf.trim(), "y" | "Y") {
            return Err("aborted by operator".into());
        }
        Ok(())
    } else {
        Err("non-interactive run requires --yes (no TTY available for prompt)".into())
    }
}
```

For `ReadConfigEntry::Unsupported` (Spin Cloud), the §8.3 four-branch UX overrides the default flow:

| Caller environment      | Behaviour                                                                                                                                                                                                                                                                                                  |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--dry-run`             | Exit non-zero with: `config push --dry-run --adapter spin against Spin Cloud is unsupported (no remote read-back; re-run with --local for the on-disk SQLite write or drop --dry-run to write unconditionally with --yes)`. The flag's contract is "show the diff", which is structurally impossible here. |
| `--yes` (or `-y`) set   | Write unconditionally. No prompt, no diff render. Output: `pushed N entries to Spin Cloud (skip-on-equal unavailable; remote sha not readable)`.                                                                                                                                                           |
| TTY without `--yes`     | Prompt: `cannot read remote on Spin Cloud (no get subcommand); write anyway? [y/N]`. `y` proceeds; `n` exits non-zero.                                                                                                                                                                                     |
| Non-TTY without `--yes` | Exit non-zero: `Spin Cloud read-back unsupported; pass --yes for non-interactive runs (the push writes unconditionally)`.                                                                                                                                                                                  |

Sketch:

```rust
if let ReadConfigEntry::Unsupported(reason) = &remote {
    if args.dry_run {
        return Err(format!(
            "config push --dry-run --adapter spin against Spin Cloud is unsupported \
             ({reason}); re-run with --local for the on-disk SQLite write or drop \
             --dry-run to write unconditionally with --yes"
        ));
    }
    if !args.yes {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err(format!(
                "Spin Cloud read-back unsupported ({reason}); pass --yes for \
                 non-interactive runs (the push writes unconditionally)"
            ));
        }
        eprint!("cannot read remote on Spin Cloud ({reason}); write anyway? [y/N] ");
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).map_err(|e| format!("stdin read failed: {e}"))?;
        if !matches!(buf.trim(), "y" | "Y") {
            return Err("aborted by operator".into());
        }
    }
    // proceed to write
} else {
    require_consent(args, &remote)?;
}
```

- [ ] **Step 4: Call the adapter's writer (Task C2 work).**

- [ ] **Step 5: Tests.** Cover the §8.2 default rules (skip-on-equal, no-write, consent prompt, `--no-diff`/`--yes` interactions) AND the §8.3 four-branch Spin Cloud UX (dry-run rejection, --yes silent write, TTY prompt, non-TTY error).

## Task C5 — App-demo migration

**Files (all under `examples/app-demo/`):**

- Modify: `examples/app-demo/crates/app-demo-core/src/config.rs` — comments + AppConfig derive checks (the macro reruns).
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs` — switch to `AppConfig<AppDemoConfig>` extractor; remove hand-managed `config_store_default()?.get(...)` + `secret_store.require_str(...)` paths.
- Modify: `examples/app-demo/app-demo.toml` — comment updates only (the shape stays the same).
- Modify: each `examples/app-demo/adapters/<adapter>/<adapter>.toml` — comment updates.
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs` if it re-exports anything.

- [ ] **Step 1: Add `AppConfig` to the extractor imports.** In `handlers.rs:8`:

```rust
use edgezero_core::extractor::{AppConfig, Headers, Json, Kv, Path, Query, Secrets, ValidatedPath};
```

- [ ] **Step 2: Replace each typed-config handler.** Find every handler that calls `ctx.config_store_default()?.get(...)?` or `secret_store.require_str(&cfg.<field>)` and rewrite to use `AppConfig<AppDemoConfig>` and `cfg.<field>` directly.

- [ ] **Step 3: Update `app-demo.toml` comment** describing that secret values are key NAMES in the secret store (Model A semantic).

- [ ] **Step 4: Run app-demo's tests + smoke if available.**

## Task C6 — Scaffold template migrations (Push + Validate only; Diff deferred to Phase D)

**Files:**

- Modify: `crates/edgezero-cli/src/templates/core/src/config.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/app/name.toml.hbs`
- Modify: `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs` — wire `Push(...)` → `run_config_push_typed::<C>` and `Validate(...)` → `run_config_validate_typed::<C>` ONLY. The `Diff` arm is NOT added here.
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/README.md.hbs`
- Modify: each `crates/edgezero-adapter-*/src/templates/` adapter-specific manifest as needed.

For each file: per spec §10.2.2, apply the documented edits verbatim — EXCEPT the `cli/src/main.rs.hbs` `Diff` arm. The generated CLI's `TypedConfigCmd` enum (spec §3.2.2) declares `Push` / `Diff` / `Validate`, but `ConfigDiffArgs` + `run_config_diff_typed` don't exist until Phase D. To avoid a generated-project compile failure in Phase C, the template's `TypedConfigCmd` enum in this commit declares only `Push` and `Validate`; Task D2 below adds the `Diff` variant + dispatch in the same commit that ships the diff implementation.

This is purely additive: a generated project from Phase C has `config push` + `config validate`. Phase D's commit adds `config diff` to the same scaffold. No intermediate broken state.

- [ ] **Step 1: Apply each template edit per spec §10.2.2.** For `cli/src/main.rs.hbs`, the enum block looks like:

```rust
// cli/src/main.rs.hbs (Phase C — Push + Validate only):
#[derive(clap::Subcommand, Debug)]
pub enum TypedConfigCmd {
    Push(ConfigPushArgs),
    Validate(ConfigValidateArgs),
}

// ... dispatch:
match cmd {
    TypedConfigCmd::Push(args) => run_config_push_typed::<{{NameUpperCamel}}Config>(&args)?,
    TypedConfigCmd::Validate(args) => run_config_validate_typed::<{{NameUpperCamel}}Config>(&args)?,
}
```

- [ ] **Step 2: Run the existing scaffold-render test to confirm the templates compile cleanly under the new shape.**

Run: `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`
Expected: pass.

## Task C7 — CI gate scripts

**Files:**

- Create: `scripts/check_no_legacy_typed_reads.sh`
- Create: `scripts/check_no_placeholder_pins.sh`
- Create: `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
- Modify: `crates/edgezero-cli/Cargo.toml` — add `[[bin]]` entry + `nested-app-config-check` feature + optional `syn`/`walkdir` deps.
- Modify: `.github/workflows/test.yml` — add the three gate runs.

- [ ] **Step 1: Author `scripts/check_no_legacy_typed_reads.sh`** — copy the spec §10.2.1 script verbatim.

- [ ] **Step 2: Author `scripts/check_no_placeholder_pins.sh`** — copy spec §13.1 verbatim.

- [ ] **Step 3: Author the nested-AppConfig helper** at `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`. Use `syn::parse_file` over each `.rs` file in `argv[1..]`; collect every struct that derives `AppConfig`; walk each struct's field types recursively (`syn::Type::Path` / `Tuple` / `Array`) and report any nested reference. Exit 0 on no violations, 1 on violations with `<file>:<line>:<field>` lines on stdout, 2 on syntax errors.

- [ ] **Step 4: Add the bin + feature to `crates/edgezero-cli/Cargo.toml`.**

```toml
[features]
nested-app-config-check = ["dep:syn", "dep:walkdir"]

[dependencies]
syn = { version = "2", features = ["full", "extra-traits", "visit"], optional = true }
walkdir = { workspace = true, optional = true }

[[bin]]
name = "check_no_nested_app_config"
path = "src/bin/check_no_nested_app_config.rs"
required-features = ["nested-app-config-check"]
```

- [ ] **Step 5: Author the scaffold-render integration test** at `crates/edgezero-cli/tests/scaffold_render.rs` per spec §10.2.1 "rendered-template coverage". Renders every `.rs.hbs` template with deterministic fixture context, writes to a temp dir, invokes the helper, asserts exit 0.

- [ ] **Step 6: Wire the three gates into `.github/workflows/test.yml`.** Add three steps before `cargo test`:

```yaml
- name: No placeholder pins
  run: ./scripts/check_no_placeholder_pins.sh
- name: No legacy typed reads
  run: ./scripts/check_no_legacy_typed_reads.sh
- name: Nested AppConfig audit
  run: cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates
```

- [ ] **Step 7: Run all gates locally.** Expected: pass.

## Task C8 — Commit Phase C

- [ ] **Step 1: Run the four CI gates.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

- [ ] **Step 2: Run the three new gates.**

```bash
./scripts/check_no_legacy_typed_reads.sh
./scripts/check_no_placeholder_pins.sh
cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates
```

- [ ] **Step 3: Commit Phase C.**

```bash
git add ... # all touched files
git commit -m "Cutover: blob app-config runtime + writers + app-demo + gates

Phase C of the blob app-config rewrite per
docs/superpowers/specs/2026-06-16-blob-app-config.md.

This is the atomic cutover commit per spec §10.1 — splitting any
piece below into a separate commit leaves an unbisectable
intermediate state (new runtime + old writer would fail because
no blob exists in the store, and the §10.2.1 grep gate would
fail in any commit that moves the extractor without migrating
app-demo handlers in the same commit).

- §3.3.3 AppConfig<C> extractor: envelope parse, SHA verify,
  secret walk per Model A (#[secret] / #[secret(store_ref)] /
  #[secret(store_ref = \"field\")]), serde_path_to_error
  deserialise, validator::Validate::validate. Missing blob →
  EdgeError::ConfigOutOfDate per Q3 (d).
- §8.2 config push rewrite: single envelope per [stores.config]
  key per adapter. Per-adapter writers (Axum file map /
  Cloudflare bulk put with namespace-id+remote / Fastly
  --upsert --stdin / Spin direct SQLite / Spin Cloud
  key-value set). Inline diff prompt + --yes / --no-diff /
  --dry-run consent flow per §8.2. --key flag for the per-env
  KEY override paired with __KEY runtime resolution from
  Phase B.
- §10.2 app-demo migration: handlers switch to AppConfig<C>
  extractor; hand-managed config_store_default()?.get(...) and
  secret_store.require_str(&cfg.<field>) paths removed per
  Model A's framework-resolved-secret rule.
- §10.2.2 scaffold templates migrate: core/src/config.rs.hbs,
  core/src/handlers.rs.hbs, app/name.toml.hbs, root/edgezero.
  toml.hbs, root/README.md.hbs, cli/src/main.rs.hbs.
- §10.2.1 CI gates land together:
  - scripts/check_no_legacy_typed_reads.sh: greps for legacy
    typed reads + missing AppConfig usage in app-demo.
  - scripts/check_no_placeholder_pins.sh: refuses placeholder
    markers in canonical_form_pins.rs.
  - crates/edgezero-cli/src/bin/check_no_nested_app_config.rs:
    syn-based AST audit (behind nested-app-config-check
    feature) catching nested AppConfig-rooted types used as
    field types in another AppConfig struct.

The grep gate passes because app-demo + scaffold templates are
migrated in this same commit. The §13.1 placeholder gate
flags the SHA pin in canonical_form_pins.rs; the implementing
PR replaces the placeholder with the real hex before merge."
```

**STOP for user review before Phase D.**

---

# Phase D — `config diff` command (Commit D)

**Goal:** Add the `config diff` subcommand on generated CLIs. Reuses Phase C's read trait + envelope decode.

## Task D1 — `ConfigDiffArgs` clap struct (bundled-CLI exports only)

**Scope (round-27 F-4):** D1 owns the `ConfigDiffArgs` clap struct in `crates/edgezero-cli/src/args.rs`. It does NOT touch the bundled binary's enum dispatch (that already has Push/Diff as stub-pointer variants per C3) or the scaffold template's `TypedConfigCmd` (that is owned by D2 Step 4 below).

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` — add `ConfigDiffArgs` per spec §3.2.2.

- [ ] **Step 1: Add the struct.**

Per spec §3.2.2's args block: `adapter`, `app_config`, `manifest`, `store`, `key`, `no_env`, `local`, `runtime_config`, `format`, `exit_code`. The struct is `#[derive(clap::Args, Debug)]`.

- [ ] **Step 2: Parser-roundtrip tests for the new flags** (per spec §12.11). Cover `--format json`, `--local`, `--store`, `--key`, `--runtime-config`, `--exit-code`. Mirror the existing `ConfigPushArgs` parser tests in `args.rs`.

(The bundled `ConfigCmd::Diff(ConfigCmdStubArgs)` stub-pointer variant declared in C3 already takes the catch-all `ConfigCmdStubArgs`, NOT this new `ConfigDiffArgs`. The struct introduced here exists for the scaffold template's `TypedConfigCmd::Diff(ConfigDiffArgs)` variant that D2 Step 4 wires.)

## Task D2 — `run_config_diff_typed` entry point + format renderers

**Files:**

- Create: `crates/edgezero-cli/src/diff.rs` — three format renderers (`unified`, `structured`, `json`) per spec §8.1.1/8.1.2/8.1.3.
- Modify: `crates/edgezero-cli/src/config.rs` — add `pub fn run_config_diff_typed<C>(args: &ConfigDiffArgs) -> Result<(), String>`.

- [ ] **Step 1: Implement the three renderers.** Each takes two `serde_json::Value` data fields + the local/remote shas + returns a `String` (or writes to stdout).

For `unified`: dotted-path key-sorted lines with `-` / `+` per spec §8.1.1.

For `structured`: per spec §8.1.2 (refer to the spec block — implement verbatim).

For `json`: per spec §8.1.3.

- [ ] **Step 2: Wire `run_config_diff_typed` with EXPLICIT per-variant handling of the read-back outcome.** Round-27 reviewer flagged the earlier compressed "read → render" wording as under-specified for the four `ReadConfigEntry` cases. Each variant gets its own branch + exit-code tag:

```rust
pub fn run_config_diff_typed<C>(args: &ConfigDiffArgs) -> Result<(), String>
where
    C: DeserializeOwned + AppConfigMeta + Validate,
{
    // 1. Deserialise local TOML (deserialise-only) + validate_excluding_secrets.
    let cfg: C = deserialize_app_config_with_options(&app_config_path, &app_name, &load_opts)?;
    validate_excluding_secrets(&cfg).map_err(|e| format!("local validation failed: {e}"))?;

    // 2. Build the local envelope.
    let data: serde_json::Value = serde_json::to_value(&cfg)
        .map_err(|e| format!("failed to serialise local config: {e}"))?;
    let local_envelope = BlobEnvelope::new(data, generated_at_rfc3339());
    let local_sha = local_envelope.sha256.clone();

    // 3. Read back from the platform, mirroring the writer's parameter list.
    let remote = if args.local {
        adapter.read_config_entry_local(
            &manifest_root, adapter_manifest_path, component_selector,
            &store, &key, &push_ctx,
        )?
    } else {
        adapter.read_config_entry(
            &manifest_root, adapter_manifest_path, component_selector,
            &store, &key, &push_ctx,
        )?
    };

    // 4. Branch per spec §8.1's "Missing-remote semantics" + §9.0
    //    Unsupported handling. Each branch sets `outcome` so Step 5
    //    can pick the right exit code.
    enum DiffOutcome { NoChanges, DiffPresent, RemoteAbsent, Unsupported(&'static str) }
    let outcome = match remote {
        ReadConfigEntry::Present(body) => {
            let remote_envelope: BlobEnvelope = serde_json::from_str(&body)
                .map_err(|e| format!("remote envelope parse failed: {e}"))?;
            remote_envelope.verify().map_err(|e| format!("remote envelope verification failed: {e}"))?;
            if remote_envelope.sha256 == local_sha {
                println!("# no changes (sha256 matches: {local_sha})");
                DiffOutcome::NoChanges
            } else {
                render_diff(&remote_envelope.data, &local_envelope.data, &remote_envelope.sha256, &local_sha, args.format.as_str())?;
                DiffOutcome::DiffPresent
            }
        }
        ReadConfigEntry::MissingKey => {
            println!("# no remote at key `{key}`; all <N> leaves added");
            render_diff_against_empty(&local_envelope.data, &local_sha, args.format.as_str())?;
            DiffOutcome::RemoteAbsent
        }
        ReadConfigEntry::MissingStore => {
            eprintln!("# store has no matching backend yet — run `edgezero provision --adapter <name>` first if this is the live remote");
            render_diff_against_empty(&local_envelope.data, &local_sha, args.format.as_str())?;
            DiffOutcome::RemoteAbsent
        }
        ReadConfigEntry::Unsupported(reason) => {
            // §8.3 Spin Cloud surface: actionable error pointing the operator
            // at --local or --yes push, not a half-truth render.
            eprintln!(
                "config diff for {} is unsupported ({reason}). Re-run with --local for the on-disk read, \
                 or push unconditionally with `<app-cli> config push --adapter {} --yes` to update without seeing the diff.",
                args.adapter, args.adapter,
            );
            DiffOutcome::Unsupported(reason)
        }
    };

    // 5. Exit-code per Q10 + Step 3 below.
    apply_exit_code(args.exit_code, outcome)
}
```

Helpers: `render_diff` dispatches to the per-format renderers from Step 1; `render_diff_against_empty` renders the local data as if all leaves were added (`+` lines per §8.1.1). `apply_exit_code` does the §3.2.2 doc-comment translation per the table in Step 3.

- [ ] **Step 3: `--exit-code` semantics per Q10 — explicit per-outcome table.**

| Outcome                                    | Without `--exit-code` | With `--exit-code` |
| ------------------------------------------ | --------------------- | ------------------ |
| No changes (sha matches)                   | exit 0                | exit 0             |
| Diff present                               | exit 0                | exit 1             |
| Remote absent (MissingKey / MissingStore)  | exit 0 (treated as "all leaves added" success) | exit 1 (still a diff present from the operator's POV) |
| Unsupported (Spin Cloud)                   | exit 2 (the diff is structurally impossible)   | exit 2             |
| Parse / network / manifest-load error      | exit 2 (always)       | exit 2             |

Code:

```rust
fn apply_exit_code(exit_code_flag: bool, outcome: DiffOutcome) -> Result<(), String> {
    let code = match (exit_code_flag, outcome) {
        (_, DiffOutcome::Unsupported(_)) => 2,
        (false, _) => 0,
        (true, DiffOutcome::NoChanges) => 0,
        (true, DiffOutcome::DiffPresent | DiffOutcome::RemoteAbsent) => 1,
    };
    if code == 0 {
        Ok(())
    } else {
        std::process::exit(code);
    }
}
```

(Errors raised via `Result::Err` propagate to the bundled / generated `main()` which exits ≥2 — that's how the "always non-zero on error" rule from Q10 + the `dry_run` doc-comment is honored. The `apply_exit_code` helper only fires on success branches.)

- [ ] **Step 4: Add `Diff` to the scaffold template's `TypedConfigCmd` enum + dispatch.** Task C6 deliberately deferred this so the Phase C scaffold compiles without `ConfigDiffArgs` existing. In this commit, update `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`:

```rust
// cli/src/main.rs.hbs (Phase D — adds Diff arm):
#[derive(clap::Subcommand, Debug)]
pub enum TypedConfigCmd {
    Push(ConfigPushArgs),
    Diff(ConfigDiffArgs),
    Validate(ConfigValidateArgs),
}

// ... dispatch:
match cmd {
    TypedConfigCmd::Push(args) => run_config_push_typed::<{{NameUpperCamel}}Config>(&args)?,
    TypedConfigCmd::Diff(args) => run_config_diff_typed::<{{NameUpperCamel}}Config>(&args)?,
    TypedConfigCmd::Validate(args) => run_config_validate_typed::<{{NameUpperCamel}}Config>(&args)?,
}
```

Re-run `cargo test -p edgezero-cli --test generated_project_builds -- --ignored` to confirm the generated project still compiles with the Diff arm added.

- [ ] **Step 5: Per-format tests + per-error-class tests.**

## Task D3 — Commit Phase D

- [ ] **Step 1: Run CI gates locally.**
- [ ] **Step 2: Commit.**

```bash
git commit -m "Add config diff command

Phase D of the blob app-config rewrite per spec §8.1.

- ConfigDiffArgs with --format (unified/structured/json),
  --local, --exit-code, --store, --key, --runtime-config,
  --no-env.
- run_config_diff_typed wires through Phase C's read trait +
  envelope decode. --local flips read_config_entry to
  read_config_entry_local. --exit-code semantics per Q10:
  errors always non-zero regardless of the flag; the flag
  toggles 0/1 only on the diff-present success branch.
- Three format renderers per spec §8.1.1-§8.1.3."
```

**STOP for user review before Phase E.**

---

# Phase E — Docs + smoke scripts (Commit E)

**Goal:** Ship the migration guide, smoke scripts, README updates.

## Task E1 — Migration guide

**Files:**

- Create or expand: `docs/guides/blob-app-config-migration.md` — operator-facing guide per spec §10.

## Task E2 — Smoke scripts

**Files:**

- Modify: `scripts/smoke_test_config.sh` — seed via `app-demo-cli config push --adapter axum` and assert runtime reads expected values.

## Task E3 — Top-level README + scaffold README updates

**Files:**

- Modify: `README.md` if app-config shape is described.
- Modify: scaffold `root/README.md.hbs` if not already done in Phase C.

## Task E4 — Commit Phase E + push the PR

- [ ] **Step 1: Run CI gates.**
- [ ] **Step 2: Commit.**

```bash
git commit -m "Add migration guide, smoke scripts, README updates

Phase E (post-cutover docs) of the blob app-config rewrite.

Operator-facing migration guide covering:
- Why this change (atomic cutover, no compat path).
- Per-adapter mechanics (Axum file shape, Cloudflare bulk put,
  Fastly --upsert --stdin, Spin local SQLite, Spin Cloud
  no-read-back).
- Operator runbook (push, env-var KEY override, secret-store
  pre-provisioning).
- Cleanup recipes for orphan leaf keys per adapter.

Smoke script exercises the seed-then-read path end-to-end."
```

- [ ] **Step 3: Push to origin.**

```bash
git push origin feature/extensible-cli
```

---

## Spec coverage self-review

Mapping spec §s → tasks:

| Spec §                                          | Task(s)                                   |
| ----------------------------------------------- | ----------------------------------------- |
| §3.2.1 (bundled binary loses push/diff)         | C3                                        |
| §3.2.2 (CLI args block)                         | C3, D1                                    |
| §3.3 (secret-field Model A)                     | C1                                        |
| §3.3.1.1 (macro metadata API)                   | B10                                       |
| §3.3.1.2 (top-level only)                       | B10 + C7 (gate)                           |
| §3.3.1.3 (serde rename policy)                  | B10                                       |
| §3.3.1.4 (derive-time validation table)         | B10 (trybuild fixtures)                   |
| §3.3.2 (writer behaviour + structural checks)   | B11                                       |
| §3.3.3 (extractor secret walk)                  | C1                                        |
| §3.3.4 (blob layout)                            | C2 (envelope creation)                    |
| §3.3.6 (SecretError → EdgeError mapping)        | C1 (`map_secret_error`)                   |
| §3.3.7 (sha + secret-key interaction)           | A2 + C1                                   |
| §3.3.8 (push vs runtime validation)             | B9 + C4                                   |
| §4.1 (envelope shape)                           | A4                                        |
| §4.2 (canonical form)                           | A2, A3                                    |
| §4.3 (read-side validation)                     | C1                                        |
| §5.1 (default key)                              | C2                                        |
| §5.2 (runtime override + charset)               | B1, B7                                    |
| §5.2.1 (`ConfigStoreBinding`)                   | B5, B6, B8                                |
| §5.4 (push-side `--key`)                        | C3, C4                                    |
| §6.0 (name `AppConfig<C>`)                      | C1                                        |
| §6.1 (default-key extractor form)               | C1 (`FromRequest` impl)                   |
| §6.2 (explicit-key `named(key)` form)           | C1 (Step 2 inherent impl)                 |
| §6.2.1 (cross-store `from_store(id, key)` form) | C1 (Step 2 inherent impl)                 |
| §6.2.2 (runtime validation)                     | C1 (`cfg.validate()`)                     |
| §6.3 (errors — bullet list)                     | B2, B3, C1                                |
| §6.3.1 (`ConfigOutOfDate` body shape)           | B2, B3, B4                                |
| §6.4 (no caching)                               | C1 (re-reads every call)                  |
| §6.5 (existing `ConfigStore` trait stays)       | (no code work — kept)                     |
| §7.x (SHA discussion)                           | A2 (canonical form)                       |
| §8.1 (config diff)                              | D1, D2                                    |
| §8.2 (config push + consent)                    | C2, C3, C4                                |
| §8.3 (per-adapter read-back / Spin Cloud)       | B13                                       |
| §9.0 (`Adapter::read_config_entry`)             | B12, B13                                  |
| §9.1-9.4 (per-adapter notes)                    | B13, C2                                   |
| §10 (migration)                                 | C5, E1                                    |
| §10.2 (app-demo migration)                      | C5                                        |
| §10.2.1 (CI gates)                              | C7                                        |
| §10.2.2 (scaffold templates)                    | C6                                        |
| §10.3 (manifest charset in scope)               | B1                                        |
| §12.1 (canonical_form pin)                      | A3                                        |
| §12.6.1 (`kind` strings + headers)              | B4                                        |
| §12.2 (push / diff tests)                       | C4 + D2                                   |
| §12.3 (per-adapter end-to-end)                  | C2 (writers) + E2 (smoke)                 |
| §12.4 (migration)                               | E1 (docs)                                 |
| §12.5 (secret-field model)                      | C1 (extractor tests)                      |
| §12.7 (env-var key override)                    | B7 (per-key tests) + E2                   |
| §12.8 (raw-binary stub)                         | C3                                        |
| §12.9 (downstream CLI wiring)                   | C6 (Push + Validate template wiring) + D2 (Diff template wiring per round-26 phasing carve-out) |
| §12.10 (Spin Cloud cap)                         | C2 (Spin Cloud writer)                    |
| §12.11 (parser tests)                           | C3 (ConfigPushArgs) + D1 (ConfigDiffArgs) |
| §12.12 (--store routing)                        | C4 + D2                                   |
| §12.13 (CF local vs remote binding)             | C2 (Cloudflare writer)                    |
| §12.14 (StoreRegistry ref accessors)            | B6                                        |
| §12.15 (raw Config binding accessors)           | B8                                        |
| §12.16 (named-store secret adapter validation)  | B11                                       |
| §12.17 (nested AppConfig fixture)               | C7 (gate fixtures)                        |
| §12.18 (manifest charset)                       | B1                                        |
| §13 (phasing)                                   | Plan structure mirrors it                 |
| §13.1 (placeholder pin gate)                    | C7                                        |

No spec section is unmapped.

---

**Plan complete and saved to `docs/superpowers/plans/2026-06-17-blob-app-config.md`. Two execution options:**

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints.

**Which approach?**
