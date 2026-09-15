# Runtime SDK Hard-Cut Implementation Plan

> **Status:** Implemented. Outbound batch capability names and APIs in this historical plan are
> superseded by `2026-09-15-outbound-http-consumer-alignment.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to
> implement this plan task by task. Apply `superpowers:test-driven-development` to every
> behavioral change and `superpowers:verification-before-completion` before claiming success.

**Goal:** Upgrade the Fastly, Cloudflare, and Spin adapters, demo applications, and generated
projects to the latest stable provider SDKs with no compatibility branches, while correcting
outbound concurrency wording and downgrading unproved Cloudflare deadline capabilities.

**Architecture:** Keep the existing EdgeZero outbound and response-egress contracts intact and
adapt only provider-boundary code required by `fastly = 0.13.1`, `worker = 0.8.5`, and
`spin-sdk = 7.0.0`. Dependency pins remain exact in the workspace and demo; generated projects
use the matching release lines. Capability declarations describe verified behavior rather than
SDK potential, so Cloudflare deadline capabilities remain `BestEffort` until a deployed,
host-observed cancellation fixture proves `Native` semantics.

**Tech Stack:** Rust 1.95, Cargo resolver 2, Fastly Compute SDK 0.13.1, Cloudflare Workers Rust
SDK 0.8.5, Spin Rust SDK 7.0.0/WASIp3, Wasmtime 48.0.2, WASM target checks, Viceroy, Node
documentation guards.

**Source of truth:**
[`2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md) and
[`2026-09-08-response-egress-design.md`](../specs/2026-09-08-response-egress-design.md).

---

## Scope And Constraints

- This is a hard cut. Do not retain SDK 6/0.12/0.8.3 `cfg` branches, aliases, fallback code,
  generated templates, or dependency pins.
- Preserve the existing outbound public API, positional `send_all` behavior, per-slot elapsed
  metadata, independent encoded/decoded limits, and response-egress lifecycle.
- Do not use Fastly's direct `PendingRequest::send_to_client` path for EdgeZero outbound calls;
  it bypasses EdgeZero response limits, decoding, typed outcomes, and slot timing.
- Keep Fastly's raw status-returning response ABI while the high-level SDK send wrappers can
  panic on hostcall errors.
- Do not promote a capability based only on compilation or SDK documentation. A `Native`
  deadline/cancellation claim requires target-level and deployed host-observed evidence.
- Do not add absolute terminal instants or a public batch-start instant. Under the current
  contract, `Ok` is proof of on-time terminal completion and `elapsed` is metadata, not an
  independent lateness detector.
- Historical phase plans and baseline prose may retain old version numbers when explicitly
  labeled historical. Normative/current documentation must use the upgraded versions.
- Do not commit or push during this execution unless the user separately requests it.
- After every production Rust edit, run the narrowest affected crate test before proceeding.

## File Map

- `Cargo.toml`, `Cargo.lock`: exact workspace provider SDK pins and resolved graph.
- `.tool-versions`: Viceroy release capable of loading Fastly SDK 0.13.1 host imports.
- `examples/app-demo/Cargo.toml`, `examples/app-demo/Cargo.lock`: exact demo pins and graph.
- `crates/edgezero-adapter-{fastly,cloudflare,spin}/src/**`: provider API adaptations only where
  the upgraded SDK fails to compile or changes verified behavior.
- `crates/edgezero-adapter-cloudflare/src/cli.rs`: authoritative Cloudflare capability values
  and table-driven expectations.
- `crates/edgezero-cli/src/generator.rs`: generated application dependency versions and tests.
- `docs/guide/capabilities.md`: public capability matrix and Cloudflare evidence caveat.
- `docs/superpowers/specs/2026-05-21-outbound-http-design.md`: normative SDK pins, fan-out
  wording, Cloudflare support levels, evidence gate, and elapsed contract.
- `scripts/check_outbound_docs_contract.mjs`: executable parity guard for documentation matrix.

### Task 1: Establish Dependency And Capability Red Tests

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify: `scripts/check_outbound_docs_contract.mjs`

- [x] Change the Cloudflare capability-table test expectations for
  `OutboundDeadlines` and `StreamedUploadDeadlines` from `Native` to `BestEffort` without yet
  changing production capability dispatch.
- [x] Run
  `cargo test -p edgezero-adapter-cloudflare capability --all-features` and verify it fails only
  because production still returns `Native` for those two rows.
- [x] Change generator assertions to require `fastly = "0.13"`, `worker = "0.8.5"`, and
  `spin-sdk = "7"` without yet changing generated dependency strings.
- [x] Run the focused generator tests and verify failures report the old generated versions.
- [x] Change the docs contract guard's expected Cloudflare deadline cells to `BestEffort` and run
  `node scripts/check_outbound_docs_contract.mjs`; verify it fails against the still-`Native`
  public guide/spec matrices.

### Task 2: Upgrade Exact Workspace And Demo Dependencies

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `examples/app-demo/Cargo.toml`
- Modify: `examples/app-demo/Cargo.lock`

- [x] Replace workspace exact pins with `fastly = "=0.13.1"`,
  `fastly-shared = "=0.13.1"`, `fastly-sys = "=0.13.1"`,
  `log-fastly = "=0.13.1"`, `worker = "=0.8.5"`, and `spin-sdk = "=7.0.0"` while preserving
  current feature sets. The logger pin must move with the SDK so it does not retain a transitive
  Fastly 0.12 graph.
- [x] Apply the corresponding exact pins in `examples/app-demo/Cargo.toml`.
- [x] Update the Viceroy tool pin to 0.21.0; older repository/local releases reject host imports
  emitted by Fastly SDK 0.13.1 before tests execute. Update Wasmtime to 48.0.2; the former 44/45
  runner line cannot link Spin SDK 7's stable WASI HTTP 0.3 `request-options` resource.
- [x] Refresh both lockfiles with targeted `cargo update -p <crate> --precise <version>` commands;
  do not accept unrelated dependency churn.
- [x] Use `cargo tree -i` or lockfile inspection to prove each graph resolves exactly the requested
  provider versions and that Spin uses its SDK-provided WASIp3 generation.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"` to expose
  provider API changes before editing adapter code.

### Task 3: Adapt Fastly To SDK 0.13.1

**Files:**
- Modify only if required: `crates/edgezero-adapter-fastly/src/outbound.rs`
- Modify only if required: `crates/edgezero-adapter-fastly/src/fastly_response_abi.rs`
- Modify only if required: `crates/edgezero-adapter-fastly/src/response.rs`
- Modify only if required: `crates/edgezero-adapter-fastly/src/request.rs`
- Test: `crates/edgezero-adapter-fastly/tests/contract.rs`

- [x] Compile the Fastly adapter for `wasm32-wasip1` with its provider feature and record every
  concrete 0.13.1 API or exhaustive-enum failure.
- [x] For each behavioral compiler failure, add or tighten the smallest host-testable mapping
  test before changing production code. Treat new SDK error variants conservatively and preserve
  fixed public diagnostics.
- [x] Adapt the provider boundary without introducing `PendingRequest::send_to_client`, an SDK
  compatibility `cfg`, or a response-buffering fallback.
- [x] Run `cargo test -p edgezero-adapter-fastly --features cli` after each Rust edit.
- [x] Run the Fastly target check and Viceroy-backed tests used by the repository. Verify owned
  response transmission, outbound limits, positional harvest, and `BestEffort` deadline behavior
  remain unchanged.

### Task 4: Adapt Cloudflare To Worker 0.8.5 And Correct Deadline Claims

**Files:**
- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Modify only if required: `crates/edgezero-adapter-cloudflare/src/outbound.rs`
- Modify only if required: `crates/edgezero-adapter-cloudflare/src/response.rs`
- Modify only if required: `crates/edgezero-adapter-cloudflare/src/request.rs`
- Test: `crates/edgezero-adapter-cloudflare/tests/contract.rs`

- [x] Change production capability dispatch so `OutboundDeadlines` and
  `StreamedUploadDeadlines` return `BestEffort`; keep `OutboundFlexiblePhaseBudget`,
  `SendAllSlotIsolation`, `OutboundHttp`, and lazy passthrough at their independently verified
  levels.
- [x] Run the previously failing capability test and verify it passes.
- [x] Compile the adapter for `wasm32-unknown-unknown` with Worker 0.8.5 and capture actual binding
  failures before editing provider code.
- [x] Add a focused failing regression test before any required behavior adaptation, then make
  the minimum provider-boundary change while preserving manual redirect handling,
  `encodeResponseBody = "manual"`, the single owned `AbortController`, typed errors, and fixed
  public diagnostics.
- [x] Run `cargo test -p edgezero-adapter-cloudflare --all-features` and the Cloudflare WASM
  target check. Compilation and local abort tests must not be described as deployed cancellation
  proof.

### Task 5: Adapt Spin To SDK 7.0.0 As A Hard Cut

**Files:**
- Modify only if required: `crates/edgezero-adapter-spin/src/outbound.rs`
- Modify only if required: `crates/edgezero-adapter-spin/src/request.rs`
- Modify only if required: `crates/edgezero-adapter-spin/src/response.rs`
- Modify only if required: `crates/edgezero-adapter-spin/src/lib.rs`
- Test: `crates/edgezero-adapter-spin/tests/contract.rs`

- [x] Compile the adapter for `wasm32-wasip2` with Spin SDK 7 and record concrete WASIp3,
  `wit-bindgen`, future, stream, body-writer, and result-writer API differences.
- [x] Add a focused failing test for every behavioral adaptation that can be exercised on the
  host; use target compilation as the red gate for generated binding signature changes.
- [x] Update imports and owned exchange plumbing directly to the SDK 7 API. Remove obsolete SDK 6
  aliases/imports instead of retaining version branches.
- [x] Preserve the timer race for inbound reads, outbound exchange ownership, response-result
  completion, cancellation acknowledgement, and existing `BestEffort` provider limitations.
- [x] Run `cargo test -p edgezero-adapter-spin --all-features` after each Rust edit and finish with
  the Spin `wasm32-wasip2` target check.
- [x] Execute the nonzero Spin contract and SDK-resource suites under Wasmtime 48.0.2 with
  Preview 3 async and HTTP enabled; compilation alone does not establish runtime compatibility.

### Task 6: Align Generator, Demo, And Normative Documentation

**Files:**
- Modify: `crates/edgezero-cli/src/generator.rs`
- Modify if generated output changes: provider templates under
  `crates/edgezero-adapter-*/src/templates/`
- Modify: `docs/guide/capabilities.md`
- Modify: `docs/superpowers/specs/2026-05-21-outbound-http-design.md`
- Modify: `scripts/check_outbound_docs_contract.mjs`
- Verify: `examples/app-demo/crates/app-demo-adapter-*/`

- [x] Update generated dependency strings to Fastly 0.13, Worker 0.8.5, and Spin SDK 7; run the
  focused generator tests until the red assertions pass.
- [x] Generate a fresh all-adapter application in a temporary directory and compare its
  entrypoints and manifests with `examples/app-demo`; remove any obsolete SDK 6/0.12 path rather
  than preserving it.
- [x] Change both public and normative matrices so Cloudflare outbound and streamed-upload
  deadlines are `BestEffort`, with a concise footnote that `AbortController` wiring is present
  but deployed host-observed cancellation evidence is still required for `Native`.
- [x] Tighten the trait summary to “attempt every eligible request before harvest” and preserve
  the explicit distinction: Axum/Cloudflare/Spin drive complete exchanges concurrently, while
  Fastly dispatches sequentially before ordered harvest and does not guarantee overlap.
- [x] Clarify that a successful slot is contractually terminal and on time; consumers must not
  reconstruct lateness from a post-batch clock sample, and no absolute terminal instant is part
  of the API.
- [x] Update current SDK references and evidence commands to 0.13.1/0.8.5/7.0.0. Leave explicitly
  historical baseline and phase-plan references unchanged.
- [x] Run `node scripts/check_outbound_docs_contract.mjs` and confirm matrix parity.

### Task 7: Remove Obsolete Paths And Verify The Complete Hard Cut

**Files:**
- Inspect: all modified files and lockfiles
- Modify only to remove discovered obsolete compatibility code or stale current documentation

- [x] Run `rg` for old SDK pins, SDK-version `cfg` branches, response-buffering constants,
  removed entrypoint signatures, and stale `Native` Cloudflare deadline claims. Classify each
  remaining old version reference as explicitly historical or remove/update it.
- [x] Run `cargo fmt --all -- --check`.
- [x] Run `cargo test --workspace --all-targets`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [x] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [x] Run provider target checks for Fastly `wasm32-wasip1`, Cloudflare
  `wasm32-unknown-unknown`, and Spin `wasm32-wasip2`.
- [x] Run the repository's Viceroy, legacy-API, documentation-contract, and generated-project
  integration checks.
- [x] In `examples/app-demo`, run format, clippy, tests, and all three provider target checks with
  its refreshed lockfile.
- [x] Run documentation format/lint/build checks.
- [x] Review `git diff --check`, `git diff --stat`, and the full scoped diff. Confirm no unrelated
  dirty-worktree changes were overwritten and report any provider-host evidence that could not be
  run locally.
