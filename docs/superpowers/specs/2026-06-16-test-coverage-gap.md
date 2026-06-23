# Test Coverage Gap — Assessment & Closure Spec

> **For agentic workers:** the closure work is a plan in §7. REQUIRED
> SUB-SKILL: use `superpowers:subagent-driven-development` or
> `superpowers:executing-plans` to implement task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Date:** 2026-06-16
**Status:** v1 — Draft, revised through reviewer round 7 (deep review)
**Author:** (TBD)
**Branch under assessment:** `feature/extensible-cli`

**Goal:** Assess host-runnable test coverage across the whole workspace
as it stands on the current branch, and lay out a concrete coverage
closure plan for the gaps that can actually be closed with host unit
tests. (The §7 tasks are characterization tests over existing behavior,
not red-green TDD.)

---

## 1. Scope & method

This spec covers all eight crates under `crates/`. It does **not** cover
`examples/app-demo/` (excluded from the workspace) or `docs/`.

**How coverage was measured.** A line-coverage tool (`cargo llvm-cov`)
was not run — the binary is not installed for the project's Rust 1.95.0
toolchain and a from-source install was declined. Instead, coverage was
assessed **structurally and verified by hand**:

1. A per-file scan counting `#[test]` / `#[tokio::test]` markers and
   `#[cfg(test)]` modules across every `src/**/*.rs`.
2. Manual spot-verification of every "zero tests" / "low coverage"
   claim before it was admitted into this spec.

**Why the verification step matters.** The first-pass exploration
over-reported gaps. Three claims were checked and **rejected**:

| Rejected claim                                      | Reality (verified)                                                                                                                                                                                                                                              |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `edgezero-cli` `config push` is untested            | `config.rs` has a full push suite — `typed_push_writes_blob_envelope_to_local_config_file`, `typed_push_dry_run_does_not_write`, `typed_push_envelope_sha_matches_hand_computed_hash`, `typed_push_runs_validator_and_errors_on_bad_config`, `run_config_push_is_stub_pointer_in_bundled_binary`, plus adapter/strict checks (`config.rs:2366` onward). |
| `core/compression.rs` has "only 2 tests, major gap" | Two tests is the full happy-path matrix (gzip + brotli). The real, narrow gap is the **error path**, not presence.                                                                                                                                              |
| `axum/service.rs` (680 LOC) has zero tests          | It has `#[tokio::test]` cases (`service.rs:451,528,558`) the marker scan undercounted because of the attribute form. Well covered.                                                                                                                              |

The structural signal that proved reliable was **`#[cfg(test)]` module
absence** (`cfg(test)=0`), not raw `#[test]` counts.

---

## 2. Headline finding

**Host-testable code is broadly well covered.** `edgezero-core` alone
carries 350+ co-located tests (355 by `#[test]`/`#[tokio::test]` marker
count); the CLI, macros, and adapter CLI
surfaces are all substantially tested. The genuine gaps fall into three
buckets:

1. **Two** host-testable source files with real logic and _zero_
   coverage: `edgezero-core/src/handler.rs` and the pure `syn` helpers in
   `edgezero-cli/src/bin/check_no_nested_app_config.rs` (the
   nested-`AppConfig` CI checker; host-compilable behind the
   `nested-app-config-check` feature).
2. A small set of **error/edge-path depth gaps** in modules that already
   have happy-path tests.
3. The **runtime-bound portions of adapter platform code** (Spin /
   Fastly / Cloudflare KV stores, secret stores, real proxy `send`,
   Fastly logger init) that **cannot be host unit-tested**. Note the
   pure helper/conversion paths in these crates _are_ already host- and
   contract-tested (see the Tier-3 split in §4); only the SDK/runtime
   paths remain. The gating differs by adapter: **Spin and Cloudflare**
   modules are `#[cfg(all(feature=…, target_arch="wasm32"))]` so they
   never build on the host at all; **Fastly** modules are
   `#[cfg(feature="fastly")]` only (no `target_arch` gate) — they DO
   compile on the host, but their live paths call the Fastly compute SDK
   that isn't meaningfully exercisable off-platform. Either way the live
   paths are out of host-unit-test scope. This is by
   design per CLAUDE.md ("Don't write tests that require a network
   connection or platform credentials") and is the scope of each
   adapter's contract and/or integration tests, not host unit tests.
   (Conversion-layer paths live in `tests/contract.rs`; live-runtime
   paths — real proxy `send`, platform KV reads, logger init, secret
   stores — are integration scope per §8.)

Buckets 1 and 2 are the actionable closure work (§7). Bucket 3 is a
scope/strategy decision (§8), not a unit-test backlog.

---

## 3. Current coverage snapshot (verified)

Counts are inline test functions (`#[test]` + async test attrs) per
crate; "cfg(test)=0 files" lists non-trivial source files with **no test
module at all**.

| Crate                         | Inline tests (approx) | `tests/` dir                                     | cfg(test)=0 source files (non-trivial)                                                                 |
| ----------------------------- | --------------------- | ------------------------------------------------ | ------------------------------------------------------------------------------------------------------ |
| `edgezero-core`               | 355                   | —                                                | `handler.rs` (40) ← **host-testable gap**; `http.rs`/`lib.rs` (re-exports, N/A)                        |
| `edgezero-cli`                | 105+                  | `generated_project_builds.rs`, `lib_consumer.rs` | `main.rs` (entry, N/A); `test_support.rs` (helper, N/A); `demo_server.rs` (demo-example delegate, N/A) |
| `edgezero-macros`             | 11 inline + trybuild  | `app_config_derive.rs`, `ui/`                    | `app_config.rs` (272) — covered via trybuild/UI, **not** a true gap                                    |
| `edgezero-adapter`            | 12                    | —                                                | none (registry/scaffold/cli_support all have modules)                                                  |
| `edgezero-adapter-axum`       | 90+                   | —                                                | `test_utils.rs` (helper, N/A)                                                                          |
| `edgezero-adapter-fastly`     | 55                    | `contract.rs`                                    | `key_value_store.rs`, `secret_store.rs`, `logger.rs` — **all wasm/SDK-gated**                          |
| `edgezero-adapter-cloudflare` | 50                    | `contract.rs`                                    | `key_value_store.rs`, `secret_store.rs`, `context.rs`, `lib.rs` — **all wasm-gated**                   |
| `edgezero-adapter-spin`       | 90+                   | `contract.rs`                                    | `key_value_store.rs`, `secret_store.rs`, `proxy.rs`, `response.rs`, `lib.rs` — **all wasm/SDK-gated**  |

---

## 4. Gap inventory

### Tier 1 — host-testable, zero coverage (must close)

| Gap                                              | File                                       | Why it matters                                                                                                                                                                                                                      |
| ------------------------------------------------ | ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `DynHandler::call` + `IntoHandler::into_handler` | `crates/edgezero-core/src/handler.rs:9-40` | The closure→handler bridge and the `fut.await?.into_response()` error-propagation line (`handler.rs:22`) are the spine of every route. No test exercises them directly today. Pure host logic — trivially testable with `block_on`. |
| `struct_derives_app_config` + `type_contains_app_config_struct` | `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs:103,196` | The recursive `syn` AST walk that the nested-`AppConfig` CI gate depends on (unwraps `Option`/`Vec`/`Box`/`Rc`/`Arc` to any depth). 353 LOC, zero tests. Host-compilable behind the `nested-app-config-check` feature; testable with `syn::parse_str`. Because it's a `bin`, tests must be an inline `#[cfg(test)]` module (free fns aren't importable from `tests/`). |

### Tier 2 — host-testable depth gaps (should close)

| Gap                          | File                                               | What's missing                                                                                                                     |
| ---------------------------- | -------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| `AdapterPushContext` builder | `crates/edgezero-adapter/src/registry.rs:137-172`  | `new()` defaults and the three `with_*` setters have no test. Pure struct logic.                                                   |
| `run_native_cli` error paths | `crates/edgezero-adapter/src/cli_support.rs:82-99` | Happy path is covered; the `ErrorKind::NotFound` → install-hint branch (`:84-92`) and the non-zero-exit branch (`:93-98`) are not. |
| Decompression error path     | `crates/edgezero-core/src/compression.rs:15-60`    | `decode_gzip_stream` / `decode_brotli_stream` test only valid input; malformed input → `Err` is unexercised.                       |

### Tier 2 backlog — verify-then-test (lower confidence)

These are plausible depth gaps surfaced during assessment but **not yet
verified** to the point of writing assertions. Each task is: confirm the
code path exists as described, then add a test mirroring the module's
existing idiom. Do **not** write a test against an assumed API.

| Candidate gap                   | File                                                  | Verify first                                                                                                                                                                                                                                                                                                |
| ------------------------------- | ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Adapter` trait default methods | `crates/edgezero-adapter/src/registry.rs:211,375,401` | `merged_id_kinds` (default `&[]`), `validate_app_config_keys`/`validate_typed_secrets` (default `Ok`) — needs a minimal `Adapter` test-double overriding the required `execute` **and** `name` (both are required; reuse the existing `TestAdapter` at `registry.rs:459` rather than writing a new double). |
| `app!` macro error handling     | `crates/edgezero-macros/src/app.rs`                   | Only 2 codegen tests; missing-file / invalid-TOML diagnostics may be untested — confirm and add a trybuild compile-fail fixture if so.                                                                                                                                                                      |

### Tier 3 — runtime-bound platform code (out of host-unit-test scope)

Cannot be closed with host unit tests; see §8 for strategy. **Gating is
not uniform:** Spin and Cloudflare modules are
`#[cfg(all(feature=…, target_arch="wasm32"))]` (never built on host);
Fastly modules are `#[cfg(feature="fastly")]` only and DO compile on the
host, but their live paths call the Fastly compute SDK — so "out of
host-unit-test scope" is the shared conclusion, via different routes.
**Note the split**: the pure/helper conversion paths in these crates are
_already_ host- or wasm-contract-tested (see the per-row citations) —
only the
SDK/runtime-bound paths remain.

| Area                | Already host/contract-tested                                                                                                                                                                                                                                                          | Genuinely runtime-bound (Tier 3)                                                                                                                                                                                                                                                                                              |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Config stores       | In-memory backend get/miss for all three (`spin/src/config_store.rs:120`, `fastly/src/config_store.rs:81`, `cloudflare/src/config_store.rs:99`)                                                                                                                                       | Real SDK reads (`worker::kv`, Fastly config store, Spin KV)                                                                                                                                                                                                                                                                   |
| Secret stores       | —                                                                                                                                                                                                                                                                                     | All three remain platform-bound: `*/src/secret_store.rs` (no in-memory contract equivalent)                                                                                                                                                                                                                                   |
| KV stores           | —                                                                                                                                                                                                                                                                                     | `*/src/key_value_store.rs` (spin/fastly/cloudflare) — SDK reads                                                                                                                                                                                                                                                               |
| Proxy clients       | Fastly stream transforms (host, `fastly/src/proxy.rs:195`); Spin decode helper (host, `spin/src/decompress.rs:73`)                                                                                                                                                                    | Real outbound `send` (`spin/src/proxy.rs`, fastly/cloudflare request-build → live fetch). **Cloudflare `proxy.rs` is `#[cfg(all(feature="cloudflare", target_arch="wasm32"))]` (`cloudflare/src/lib.rs:15`)** — its `#[cfg(test)]` tests (`proxy.rs:152`) do NOT build under host `cargo test`; wasm/contract follow-up only. |
| Response conversion | Fastly `from_core_response` + `parse_uri` (host, `fastly/src/response.rs:48-61`); Spin `from_core_response` status/body (wasm contract, `spin/tests/contract.rs:34,57`); Cloudflare `from_core_response` status/headers/streaming (wasm contract, `cloudflare/tests/contract.rs:201`) | — (host/contract coverage already adequate)                                                                                                                                                                                                                                                                                   |
| Fastly logger init  | —                                                                                                                                                                                                                                                                                     | `fastly/src/logger.rs` (`log_fastly` + `fern`, fastly-feature-gated)                                                                                                                                                                                                                                                          |

---

## 5. Non-goals

- No line-coverage percentage target. Without `cargo llvm-cov` wired into
  CI, a number would be unverifiable theater. The acceptance gate (§9) is
  the concrete tests landing and passing.
- No refactoring of source to make it testable beyond what's already
  host-runnable. Minimal-change rule applies.
- No new network- or credential-dependent tests (CLAUDE.md prohibition).
- No attempt to host-test wasm-only code by faking platform SDKs.

---

## 6. File structure

All Tier 1 / Tier 2 work adds tests **inside existing files** (or a new
`#[cfg(test)]` module where none exists). No new source modules.

- `crates/edgezero-core/src/handler.rs` — add a new `#[cfg(test)] mod tests`.
- `crates/edgezero-adapter/src/registry.rs` — extend the existing test module (`registry.rs:476`).
- `crates/edgezero-adapter/src/cli_support.rs` — extend the existing test module (`cli_support.rs:134+`).
- `crates/edgezero-core/src/compression.rs` — extend the existing test module.

---

## 7. Closure plan (Tier 1 + Tier 2)

Run scoped tests after each task: `cargo test -p <crate> <test_name>`.
Commit per task.

### Task 1: Cover `handler.rs` (Tier 1)

**Files:**

- Modify/Test: `crates/edgezero-core/src/handler.rs` (append a test module)

- [ ] **Step 1: Add characterization tests** (these pin existing
      behavior, so they should pass once they compile — this is a
      coverage-only spec, not red-green TDD)

Append to `crates/edgezero-core/src/handler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::http::{request_builder, Method, StatusCode};
    use crate::params::PathParams;
    use futures::executor::block_on;

    fn ctx() -> RequestContext {
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        RequestContext::new(request, PathParams::default())
    }

    #[test]
    fn into_handler_wraps_closure_and_call_runs_it() {
        async fn ok(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            Ok("hi")
        }
        let handler = ok.into_handler();
        let response = block_on(handler.call(ctx())).expect("ok response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn call_propagates_handler_error() {
        async fn boom(_ctx: RequestContext) -> Result<&'static str, EdgeError> {
            // `EdgeError::internal` takes `E: Into<anyhow::Error>`; a bare
            // `&str` does NOT satisfy that bound, so wrap with `anyhow!`.
            Err(EdgeError::internal(anyhow::anyhow!("boom")))
        }
        let handler = boom.into_handler();
        // `block_on(...).expect_err(...)` would also compile (`Body` does
        // impl `Debug`), but `match` reads clearer for panic-on-`Ok` and
        // avoids depending on that impl.
        let error = match block_on(handler.call(ctx())) {
            Ok(_) => panic!("expected error"),
            Err(error) => error,
        };
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
```

- [ ] **Step 2: Run to verify they pass**

Run: `cargo test -p edgezero-core handler::tests`
Expected: compiles and both tests PASS (this code is correct as written;
if `RequestContext::new` / `PathParams::default` signatures differ on
your tree, mirror the idiom in `context.rs:379`).

- [ ] **Step 3: Commit**

```bash
git add crates/edgezero-core/src/handler.rs
git commit -m "test(core): cover DynHandler::call and IntoHandler bridge"
```

### Task 1b: Cover the nested-`AppConfig` checker helpers (Tier 1)

**Files:**

- Modify/Test: `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs` (append an inline test module — it's a `bin`, so a `tests/` file can't reach its free fns)

- [ ] **Step 1: Add characterization tests** for the two pure `syn`
  helpers (the recursive-unwrap logic is the whole reason this CI gate
  exists, per the blob-app-config spec §3.3.1.2):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn known(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    fn ty(src: &str) -> syn::Type {
        syn::parse_str(src).expect("type parse")
    }

    #[test]
    fn struct_derives_app_config_detects_path_suffixed_derive() {
        let item: syn::ItemStruct =
            syn::parse_str("#[derive(Debug, edgezero_core::AppConfig)] struct C { x: u8 }")
                .expect("struct parse");
        assert!(struct_derives_app_config(&item));
    }

    #[test]
    fn struct_derives_app_config_false_without_it() {
        let item: syn::ItemStruct =
            syn::parse_str("#[derive(Debug)] struct C { x: u8 }").expect("struct parse");
        assert!(!struct_derives_app_config(&item));
    }

    #[test]
    fn type_contains_app_config_unwraps_nested_wrappers() {
        let set = known(&["ChildConfig"]);
        assert_eq!(
            type_contains_app_config_struct(&ty("ChildConfig"), &set),
            Some("ChildConfig".to_string())
        );
        assert_eq!(
            type_contains_app_config_struct(&ty("Option<Vec<Box<ChildConfig>>>"), &set),
            Some("ChildConfig".to_string())
        );
    }

    #[test]
    fn type_contains_app_config_none_for_unrelated_types() {
        let set = known(&["ChildConfig"]);
        assert_eq!(type_contains_app_config_struct(&ty("String"), &set), None);
        assert_eq!(type_contains_app_config_struct(&ty("Vec<String>"), &set), None);
    }
}
```

- [ ] **Step 2: Run** (the bin only builds with its feature)

Run: `cargo test -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config`
Expected: all four tests PASS. (`use super::*` brings the helpers plus
the `syn` / `Type` / `HashSet` imports already at the top of the bin into
scope.)

- [ ] **Step 3: Commit**

```bash
git add crates/edgezero-cli/src/bin/check_no_nested_app_config.rs
git commit -m "test(cli): cover nested-AppConfig checker syn helpers"
```

### Task 2: Cover `AdapterPushContext` builder (Tier 2)

**Files:**

- Modify/Test: `crates/edgezero-adapter/src/registry.rs` (existing `mod tests`, `:476`)

- [ ] **Step 1: Add the tests** to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn push_context_new_is_prod_with_no_paths() {
        let ctx = AdapterPushContext::new();
        assert!(!ctx.local);
        assert_eq!(ctx.manifest_adapter_deploy_cmd, None);
        assert_eq!(ctx.runtime_config_path, None);
    }

    #[test]
    fn push_context_builders_set_each_field() {
        let path = std::path::Path::new("runtime-config.toml");
        let ctx = AdapterPushContext::new()
            .with_local(true)
            .with_manifest_adapter_deploy_cmd("spin cloud deploy")
            .with_runtime_config_path(path);
        assert!(ctx.local);
        assert_eq!(ctx.manifest_adapter_deploy_cmd, Some("spin cloud deploy"));
        assert_eq!(ctx.runtime_config_path, Some(path));
    }
```

- [ ] **Step 2: Run**

Run: `cargo test -p edgezero-adapter push_context`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/edgezero-adapter/src/registry.rs
git commit -m "test(adapter): cover AdapterPushContext builder"
```

### Task 3: Cover `run_native_cli` error paths (Tier 2)

**Files:**

- Modify/Test: `crates/edgezero-adapter/src/cli_support.rs` (existing `mod tests`)

- [ ] **Step 1: Add the tests** to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn run_native_cli_missing_program_surfaces_install_hint() {
        let err = run_native_cli(
            "edgezero-no-such-program-xyz",
            &[],
            "install the thing",
        )
        .expect_err("missing program must error");
        assert!(err.contains("install the thing"), "got: {err}");
    }

    #[test]
    fn run_native_cli_nonzero_exit_is_error() {
        // `false` exits non-zero on every supported CI host (unix/macos).
        let err = run_native_cli("false", &[], "hint")
            .expect_err("non-zero exit must error");
        assert!(!err.is_empty());
    }
```

- [ ] **Step 2: Run**

Run: `cargo test -p edgezero-adapter run_native_cli`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/edgezero-adapter/src/cli_support.rs
git commit -m "test(adapter): cover run_native_cli not-found and non-zero-exit paths"
```

### Task 4: Cover decompression error path (Tier 2)

**Files:**

- Modify/Test: `crates/edgezero-core/src/compression.rs` (existing `mod tests`)

- [ ] **Step 1: Add the test** to the existing `#[cfg(test)] mod tests`
      (imports `stream`, `block_on`, `Bytes`, `io` are already in scope there):

```rust
    #[test]
    fn decode_gzip_stream_surfaces_error_on_invalid_input() {
        let garbage = b"this is definitely not a gzip member".to_vec();
        let stream = stream::iter(vec![Ok::<Vec<u8>, io::Error>(garbage)]);
        let result = block_on(async {
            decode_gzip_stream(stream).try_collect::<Vec<Bytes>>().await
        });
        assert!(result.is_err(), "invalid gzip must decode to an error");
    }

    #[test]
    fn decode_brotli_stream_surfaces_error_on_invalid_input() {
        // A high-bit-set lead byte is not a valid brotli stream prefix.
        let garbage = vec![0xFFu8; 64];
        let stream = stream::iter(vec![Ok::<Vec<u8>, io::Error>(garbage)]);
        let result = block_on(async {
            decode_brotli_stream(stream).try_collect::<Vec<Bytes>>().await
        });
        assert!(result.is_err(), "invalid brotli must decode to an error");
    }
```

- [ ] **Step 2: Run**

Run: `cargo test -p edgezero-core compression::tests::decode_`
Expected: both new tests PASS. (If `try_collect` is unresolved, add
`use futures::TryStreamExt as _;` — the existing happy-path tests already
use it, so the import is present in the module.)

- [ ] **Step 3: Commit**

```bash
git add crates/edgezero-core/src/compression.rs
git commit -m "test(core): cover gzip and brotli decode error paths"
```

### Task 5: Tier-2 backlog (verify-then-test)

For each row in §4 "Tier 2 backlog": first read the cited code to
confirm the path exists as described; if it does, add one test mirroring
the module's existing idiom and commit; if it does not, note it in the
v2 changelog and drop it. **Do not** write a test against an assumed API.

---

## 8. Tier 3 strategy — wasm-gated platform code

The _remaining runtime-bound_ files here only compile for `wasm32-*`
targets and/or behind platform SDK features; host `cargo test` never
builds them. (The pure helper code in these crates — e.g.
`spin/src/decompress.rs:73` — is host-tested already and is the subject
of the conversion/decompression error-path follow-up in option 1, not
this runtime-bound bucket.) Options, in
preference order:

**What's already done (do not re-propose):** Spin response status/body
contract tests (`spin/tests/contract.rs:34,57`), in-memory config-store
get/miss for all three adapters (`config_store.rs` per crate), and Fastly
proxy/response host tests all exist. The remaining Tier-3 surface is
narrower than a first read suggests.

1. **Targeted contract-test additions** for what the harness _can_ drive
   but doesn't yet: error propagation (decode failure, oversized body →
   `Err`) and size-limit boundaries in the conversion/decompression
   paths.
2. **Accept as integration-test scope** for anything requiring a live
   runtime: Fastly `logger.rs` `log_fastly` init, real proxy `send`,
   real platform KV reads, and **all secret-store behavior**. The secret
   stores are platform-bound by construction — the source files say so
   directly (`spin/src/secret_store.rs:63` "integration tests require the
   Spin runtime"; `fastly/src/secret_store.rs:75` "require the Fastly
   compute environment"; `cloudflare/src/secret_store.rs:44` calls
   `worker::Env::secret`) — and have no in-memory contract equivalent.
   Track these as integration items, not host unit-test or contract-test
   gaps. CLAUDE.md excludes network/credential tests from the unit suite.
3. **Do not** fake platform SDKs to force host coverage — it tests the
   fake, not the code.

**Decision needed (Q1):** for v2, do we commit to the targeted
contract-test additions in option 1 (error/size-limit paths in the
conversion layer), or explicitly declare Tier 3 out of scope and close
this spec at Tier 1+2? Secret stores and other runtime-bound paths
(option 2) are integration scope regardless of this decision. Recommend:
land Tier 1+2 now, open a follow-up spec scoped to the _remaining_ gaps
above (not the already-covered conversions).

---

## 9. Acceptance gate

- [ ] Tasks 1, 1b, 2–4 land; all five project CI gates (CLAUDE.md) are green:
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  3. `cargo test --workspace --all-targets`
  4. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
  5. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] Neither `handler.rs` nor `check_no_nested_app_config.rs` appears in
      the `cfg(test)=0` host-testable
      file list.
- [ ] Tier-2 backlog (Task 5) rows are each either closed with a test or
      recorded as "not a gap" in the v2 changelog.
- [ ] Tier 3 disposition (Q1) is decided and recorded.

---

## 10. Open questions

- **Q1 (Tier 3 scope):** contract-test expansion now, or follow-up spec?
  (Recommend follow-up.) See §8.
- **Q2 (coverage tooling):** do we want `cargo llvm-cov` wired into CI so
  future coverage claims are measured, not asserted? Out of scope for
  this spec but the natural next step if numeric targets are desired.

---

## v1 changelog

Initial draft. Assessment via structural marker scan plus hand
verification of every admitted gap; three over-reported gaps from the
first exploration pass (`config push`, `compression` "2 tests",
`axum/service.rs`) were checked and rejected before drafting. Concrete
test code in §7 was written against APIs verified on the current branch
(`RequestContext::new`, `EdgeError::internal`/`.status()`,
`request_builder`, `PathParams::default`, `AdapterPushContext`
builders, `run_native_cli` branches, `decode_gzip_stream` idiom).

**Reviewer pass (round 2):** four findings, all verified and applied.
(1) §9 acceptance gate now lists all five CLAUDE.md CI gates, not just
1–3 — the two `cargo check` gates (features + `wasm32-wasip2`) were
missing. (2) Task 4 now adds a brotli invalid-input test alongside gzip,
so it actually closes the both-codecs gap stated in §4. (3) Dropped the
`core/response.rs` `Json<T>` backlog row — verified `Json<T>` is a
`FromRequest` extractor (`extractor.rs:20`, already covered), there is no
`Json` response wrapper, and the status tuple is already covered
(`response.rs:158`). (4) Reworded Task 1 from red-green "failing tests"
to "characterization tests", since coverage tests over existing behavior
pass on first compile.

**Reviewer pass (round 3):** seven findings, all verified and applied.
(1) Removed the `ConfigStoreError::InvalidKey` backlog row — it is
already mapped to `bad_request` (`error.rs:152`) and tested
(`config_store_error_invalid_key_maps_to_bad_request`); leaving it would
have sent Task 5 into dead work. (2) Split the Tier-3 table into
already-host/contract-tested helper paths vs. genuinely runtime-bound
paths — Fastly proxy/response and Spin response conversion are already
covered. (3) Retargeted §8 option 1: Spin response status/body contract
tests and in-memory config-store get/miss already exist; the real
remaining Tier-3 surface is error propagation, size limits, secret
stores, and live runtime. (4) Distinguished config stores (in-memory
contract-covered for all three adapters) from secret stores (no
in-memory equivalent — still platform-bound). (5) Corrected core test
count from "400+" to 355 (marker count). (6) Classified
`edgezero-cli/src/demo_server.rs` as a demo-example delegate (N/A). (7)
Updated status line to reflect the review rounds.

**Reviewer pass (round 4):** three consistency findings, all applied.
(1) §2 bucket 3 reworded from "wasm-gated adapter platform code … cannot
be host unit-tested" (which contradicted the refined Tier-3 split) to
"runtime-bound portions of adapter platform code", explicitly noting the
helper/conversion paths are already host/contract-tested. (2) Moved
secret-store behavior in §8 from option 1 (harness-drivable contract
additions) to option 2 (integration/runtime scope) — verified against
the source TODOs (`spin/src/secret_store.rs:63`,
`fastly/src/secret_store.rs:75`) and Cloudflare's `worker::Env::secret`
dependency; Q1 updated to match. (3) Reframed the §1 goal from
"TDD-style plan" to "coverage closure plan", consistent with Task 1's
characterization-test framing.

**Reviewer pass (round 5):** three findings, all verified and applied.
(1) **Compile blocker** — Task 1's error test used
`Result::expect_err`, which requires `T: Debug`; `Response` is
`http::Response<Body>` and `Body` has no `Debug` impl
(`body.rs:14`), so it would not compile. Replaced with an explicit
`match`. (2) The `Adapter` trait-double backlog row said "only the
required `execute`", but `Adapter` also requires `name()`
(`registry.rs:196,216`); reworded to require both and to reuse the
existing `TestAdapter` (`registry.rs:459`). (3) Reworded §8's "these
files only compile for `wasm32-*`" to "the _remaining runtime-bound_
files", since §8 also discusses host-tested helper code such as
`spin/src/decompress.rs:73`.

**Reviewer pass (round 6):** three Tier-3-table accuracy findings, all
verified and applied. (1) Removed Cloudflare from the "already
host-tested" proxy side — `cloudflare/src/proxy.rs` is
`#[cfg(all(feature="cloudflare", target_arch="wasm32"))]`
(`cloudflare/src/lib.rs:15`), so its `#[cfg(test)]` tests don't build
under host `cargo test`; reclassified as wasm/contract follow-up. The
genuinely host-tested decode helper is Spin's `decompress.rs:73`. (2)
Reworded the §2 headline from "scope of each adapter's
`tests/contract.rs`" to "contract and/or integration tests", since §8
moves live `send` / KV / logger / secret-store paths to integration
scope. (3) Added Cloudflare's response-conversion wasm contract coverage
(`cloudflare/tests/contract.rs:201`) to the Tier-3 table for
completeness, and annotated each conversion cite as host vs. wasm
contract. Also softened the §4 intro to "host- or wasm-contract-tested".

**Deep review (round 7):** multi-agent review vs. committed branch code.
Five verified fixes: (1) Task 1 compile blocker —
`EdgeError::internal("boom")` → `EdgeError::internal(anyhow::anyhow!("boom"))`
(`internal` needs `E: Into<anyhow::Error>`). (2) Missed Tier-1 gap —
`check_no_nested_app_config.rs` (`syn` helpers, 0 tests, host-testable)
added to §2/§4 + Task 1b. (3) Fastly is `#[cfg(feature="fastly")]` only,
not `wasm32`-gated; §2/§4 reworded. (4) §1 config-push cited nonexistent
`raw_push_*` tests → corrected to real `typed_push_*` at `config.rs:2366+`.
(5) `Body` does impl `Debug` (`body.rs:160`); Task 1's `match` kept as
style, false rationale removed. Prettier passes; §9 matches the five
CLAUDE.md gates.
