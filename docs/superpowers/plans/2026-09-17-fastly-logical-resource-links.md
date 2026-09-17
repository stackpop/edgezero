# Fastly Logical Resource Links Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove Fastly runtime descriptors and select deployment-specific stores through version-scoped logical resource links while preserving identical application package bytes.

**Architecture:** Deployment resolves physical stores from canonical `__NAME` variables and attaches them to an unpublished Fastly version under stable manifest IDs. The Fastly runtime opens those aliases and derives only the config entry key from `is_staging()`. Shared checked selector resolution and an adapter-owned config-key policy keep push, diff, and deploy aligned without adapter-name branching.

**Tech Stack:** Rust 2024 workspace, Fastly Rust SDK 0.12, Fastly CLI 15.1, Bash composite actions, VitePress documentation.

---

### Task 1: Checked selector resolution and adapter-owned key policy

**Files:**
- Modify: `crates/edgezero-core/src/env_config.rs`
- Modify: `crates/edgezero-adapter/src/registry.rs`
- Modify: `crates/edgezero-cli/src/config.rs`
- Test: colocated unit tests in those files

- [ ] Add failing tests showing that present empty, whitespace, or control-character `__NAME` and `__KEY` selectors return a redacted error rather than a fallback.
- [ ] Add checked `EnvConfig` methods that preserve the existing fallback only when the canonical variable is absent.
- [ ] Add a default adapter hook that validates/resolves the final checked config key for a target without exposing adapter types to generic CLI code.
- [ ] Call the hook from both config push and config diff after CLI/env/fallback precedence, and before provider reads or writes.
- [ ] Run `cargo test -p edgezero-core -p edgezero-adapter -p edgezero-cli`.

### Task 2: Descriptor-free Fastly runtime

**Files:**
- Modify: `crates/edgezero-core/src/app.rs`
- Modify: `crates/edgezero-macros/src/app.rs`
- Delete: `crates/edgezero-adapter-fastly/src/runtime_descriptor.rs`
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs`
- Modify: `crates/edgezero-adapter-fastly/src/request.rs`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Test: colocated Fastly adapter unit tests

- [ ] Replace descriptor tests with failing tests for logical store aliases and production/staging config keys.
- [ ] Add a pure target-key helper and make the Fastly adapter hook reject a configured key that differs from `<id>` or `<id>_staging`.
- [ ] Remove descriptor modules, constants, errors, Config Store lookup, service ID lookup, and version lookup from request dispatch.
- [ ] Use `fastly::compute_runtime::is_staging()` only to select config keys; keep store aliases logical.
- [ ] Bake `[adapters.fastly.logging]` into `Hooks` metadata, use it from `run_app`, and preserve explicit logging ownership and optional Secret Store behavior without deployment-time logging variables.
- [ ] Run `cargo test -p edgezero-adapter-fastly --features cli -- --test-threads=1` and the no-default-features library check.

### Task 3: Logical-link managed deployment

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Test: colocated managed-deployment tests in `cli.rs`

- [ ] Add failing reconciliation tests for declared alias replacement, undeclared-link preservation, legacy runtime-link removal, shared physical resources, same logical IDs across store kinds, and final exact readback.
- [ ] Build desired links as `(kind, logical alias, selected physical resource ID)`, parse Fastly `resource_type`, and validate selectors with checked `EnvConfig` methods.
- [ ] Remove descriptor plan fields, reads, writes, ownership inference, orphan checks, and final descriptor revalidation.
- [ ] Reconcile declared `(kind, alias)` identities and remove only an undeclared Config link whose alias and resolved physical store are both exactly `edgezero_runtime_env`; preserve every other inherited link, including same-named links of another kind or resource.
- [ ] Retain release verification, source snapshots, package upload/hash verification, recovery output, and stage/activate ordering; make the immediate final barrier re-read exact links, provider-visible package identity, source state, and draft state after every EdgeZero mutation, and document the caller's per-service serialization requirement and remaining provider TOCTOU risk.
- [ ] Remove automatic provisioning and setup of `edgezero_runtime_env`; keep declared physical-store provisioning.
- [ ] Run the Fastly CLI test suite after each implementation slice.

### Task 4: State-aware staging rollback

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `.github/actions/rollback-fastly/scripts/rollback.sh` only if output semantics require clarification
- Modify: `.github/actions/deploy-core/tests/make-fake-fastly-env.sh`
- Modify: `.github/actions/deploy-core/tests/run.sh`
- Test: Fastly adapter and action smoke tests

- [ ] Add failing tests for an unpublished draft, an exactly staged version, and incompatible/missing version state.
- [ ] Read and validate the requested version before staging deactivation.
- [ ] Return success without a provider mutation for an unpublished editable draft.
- [ ] Deactivate only the exact staged version and fail closed for all other states.
- [ ] Run focused Rust rollback tests and Bash action tests.

### Task 5: Action compatibility and selector validation

**Files:**
- Modify: `.github/actions/config-push-fastly/action.yml`
- Modify: `.github/actions/config-push-fastly/scripts/validate.sh`
- Modify: `.github/actions/deploy-core/tests/run.sh`
- Modify: `.github/actions/build-app-cli/action.yml`

- [ ] Add a failing action-surface test requiring the deprecated optional `key` input.
- [ ] Add a failing validation test proving a non-empty deprecated key stops before mutation.
- [ ] Restore `key` as a deprecated input and pass only its presence to validation.
- [ ] Fail with migration guidance before release extraction or provider access.
- [ ] Correct the build-app-cli output description to say release assembly consumes the artifact.
- [ ] Run `.github/actions/deploy-core/tests/run.sh` and ShellCheck.

### Task 6: Rewrite managed-deploy fixtures

**Files:**
- Modify: `.github/actions/deploy-core/tests/make-fake-fastly-env.sh`
- Modify: `.github/actions/deploy-core/tests/assert-production-deploy.sh`
- Modify: `.github/actions/deploy-core/tests/assert-staged-calls.sh`
- Modify: `.github/actions/deploy-core/tests/assert-lost-version.sh`
- Modify: `.github/actions/deploy-core/tests/make-smoke-fixture.sh`
- Modify: `.github/actions/deploy-core/tests/run.sh`
- Modify: `scripts/smoke_test_config_key_override.sh`

- [ ] Remove descriptor store IDs, descriptor files, and descriptor API routes from the fake provider.
- [ ] Seed production links under logical aliases and model staging replacement with staging physical resource IDs.
- [ ] Assert identical package digests and distinct target versions.
- [ ] Assert exact link/package verification occurs before publication.
- [ ] Assert unrelated inherited store links remain present.
- [ ] Rewrite the live Fastly smoke test around logical aliases and fixed production/staging keys, without descriptor seeding or arbitrary Fastly `__KEY` values.
- [ ] Run the complete deploy-core action test suite.

### Task 7: Align local Fastly configuration

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify: `crates/edgezero-cli/src/config.rs`
- Test: colocated local push/diff tests

- [ ] Add failing tests proving local Fastly push, diff, and runtime lookup use the logical alias and production logical key even when remote `__NAME` selects another physical store.
- [ ] Keep remote operations on the environment-selected physical store while routing local Fastly operations to Viceroy's logical alias.
- [ ] Run focused Fastly adapter and CLI config tests.

### Task 8: Documentation and templates

**Files:**
- Modify: `docs/guide/adapters/fastly.md`
- Modify: `docs/guide/blob-app-config-migration.md`
- Modify: `docs/guide/cli-reference.md`
- Modify: `docs/guide/configuration.md`
- Modify: `docs/guide/deploy-github-actions.md`
- Modify: `docs/guide/deploy-action-adoption.md`
- Modify: `docs/guide/manifest-store-migration.md`
- Modify: `docs/guide/kv.md`
- Modify: `docs/superpowers/specs/2026-07-08-edgezero-deploy-github-actions-design.md`
- Modify: `docs/superpowers/specs/2026-06-23-provision-local.md`
- Modify: `docs/superpowers/plans/2026-06-27-provision-local.md`
- Modify: `crates/edgezero-cli/src/templates/root/README.md.hbs`
- Modify: `docs/superpowers/specs/2026-09-17-fastly-logical-resource-links-design.md`
- Modify: `docs/superpowers/plans/2026-09-17-fastly-logical-resource-links.md`
- Delete: `docs/superpowers/plans/2026-09-17-fastly-runtime-review-followup.md`

- [ ] Remove every live reference to runtime descriptors, service/version selector keys, staging selector twins, and `edgezero_runtime_env`.
- [ ] Document deploy-time `__NAME` selection, logical aliases, deterministic target keys, and identical release bytes.
- [ ] Document state-aware rollback and separate real domain, deployment target, and GitHub Environment identifier.
- [ ] Document manifest-baked Fastly logging and the caller's per-service deployment serialization requirement.
- [ ] Update documentation assertions in the action suite.
- [ ] Run docs formatting, lint, and build.

### Task 9: Full verification and publication

**Files:**
- Modify: PR #381 and issue #380 descriptions after the implementation is final

- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo test --workspace --all-targets`.
- [ ] Run `cargo check --workspace --all-targets --features "fastly cloudflare spin"`.
- [ ] Run Fastly and Spin WASM checks used by CI.
- [ ] Run action tests and exact ShellCheck command.
- [ ] Run docs format, lint, and build.
- [ ] Search the tracked tree for forbidden descriptor/runtime-store names and inspect every remaining historical reference.
- [ ] Review the final diff against the accepted design, commit, push, update PR/issue text, and watch CI to completion.
