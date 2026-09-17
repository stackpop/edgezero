# Fastly Runtime Review Follow-up Implementation Plan

> **For agentic workers:** Use the repository's test-driven workflow and complete each checked item before publication.

**Goal:** Close the remaining managed Fastly deployment races and contract gaps while preserving one byte-identical application release across publishers and targets.

**Architecture:** Keep deployment ownership provider-neutral. Fastly computes the local package `files_hash`, verifies the same provider-visible hash immediately before publication, accepts a prior descriptor only when the source version links its exact runtime store, and treats a descriptor-less source with undeclared inherited stores as an audited migration boundary that fails closed.

**Tech Stack:** Rust 1.95, Fastly CLI/API, Bash composite actions, VitePress documentation

---

### Task 1: Preserve safe resource-link ownership

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `docs/guide/deploy-github-actions.md`
- Modify: `docs/guide/adapters/fastly.md`

- [x] Add failing tests for descriptor-less inherited stores and orphan descriptors.
- [x] Fail closed on undeclared inherited Config/KV/Secret links when no prior descriptor proves ownership.
- [x] Require the source version's `edgezero_runtime_env` link to resolve to the descriptor store before trusting a prior descriptor.
- [x] Run `cargo test -p edgezero-adapter-fastly`.

### Task 2: Bind publication to the verified package

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`

- [x] Add failing tests for provider package metadata changing before publication.
- [x] Compute the release package's Fastly `files_hash` locally with `fastly compute hash-files`.
- [x] Read `metadata.files_hash` for the exact target service/version during final revalidation and require an exact match.
- [x] Run `cargo test -p edgezero-adapter-fastly`.

### Task 3: Tighten compatibility and action contracts

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `.github/actions/deploy-fastly/scripts/deploy.sh`
- Modify: `.github/actions/deploy-core/scripts/run-app-cli.sh`
- Modify: `.github/actions/deploy-core/tests/run.sh`

- [x] Add failing tests for unrelated long arguments, manifest-command service/version verification, independently recoverable outputs, and both logging booleans.
- [x] Match attached short flags only on single-dash tokens.
- [x] Confirm a manifest-command `version=N` is active on the requested service before emitting it.
- [x] Publish each independently valid recovery output before any post-invocation contract failure.
- [x] Preserve both supported logging booleans across the action scrub boundary.
- [x] Run the focused Rust and action smoke tests.

### Task 4: Correct the generic deployment documentation

**Files:**

- Modify: `docs/guide/deploy-action-adoption.md`
- Modify: `docs/guide/deploy-github-actions.md`
- Modify: `.github/actions/deploy-core/tests/run.sh`

- [x] Separate the real application `domain`, the `deploy-to` target, and the derived GitHub Environment identifier.
- [x] Document the fail-closed first managed cutover and package identity check.
- [x] Keep examples application-generic and free of downstream project names.
- [x] Run docs and action documentation checks.

### Task 5: Verify and publish

- [x] Run repository formatting, clippy, workspace tests, feature checks, WASM checks, action smoke tests, and docs checks.
- [x] Self-review the diff against all nine findings.
- [ ] Commit, push, update the issue/PR description if needed, and inspect PR checks.
