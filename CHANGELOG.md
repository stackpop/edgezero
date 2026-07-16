# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking changes

- **`edgezero-adapter::Adapter::provision` trait method changed shape.** Was
  `fn provision(&self, root, adapter_manifest, component, stores, dry_run) -> Result<Vec<String>, String>`
  with an `Ok(Vec::new())` default. Is now
  `fn provision(&self, root, adapter_manifest, component, stores, deployed: Option<&AdapterDeployedState>, mode: ProvisionMode, dry_run) -> Result<ProvisionOutcome, String>`
  with **no default** — every `impl Adapter` must supply it. Any
  out-of-tree adapter written against the previous shape will fail to
  compile with two errors: the method's arity and its return type. To
  migrate:
  1. Add a `mode: ProvisionMode` match arm and a `deployed: Option<&AdapterDeployedState>` parameter.
  2. Return `ProvisionOutcome::from_status_lines(lines)` (or
     `::with_deployed(lines, deployed_state)` when the cloud arm has
     an id to write back) instead of `Ok(vec![...])`.
  3. Add a fall-through arm `other => Err(...)` on the `match mode` —
     `ProvisionMode` is `#[non_exhaustive]` and may gain variants.

- **`edgezero-adapter::AdapterDeployedState`, `ProvisionOutcome`,
  and `ProvisionMode` are now `#[non_exhaustive]`.** Struct-literal
  construction from a downstream crate (e.g.
  `ProvisionOutcome { status_lines, deployed }`) no longer compiles.
  Use the new constructors:
  - `ProvisionOutcome::from_status_lines(status_lines)` for local mode
    (which returns `deployed: None`).
  - `ProvisionOutcome::with_deployed(status_lines, deployed_state)`
    for cloud mode that populates the writeback.
  - `AdapterDeployedState::default()` + `.fields.insert(...)` /
    `.sub_tables.insert(...)` for the deployed state.

### Added

- **`edgezero provision` / `edgezero config push` cross-process advisory
  lock** (`.edgezero-provision.lock` alongside `edgezero.toml`).
  Serialises concurrent invocations against the same tree so
  read-modify-write on `.env` / `.dev.vars` / `edgezero.toml` no
  longer silently drops a competing writer's edits. Dry-run skips
  the lock. Auto-released on process exit; the sentinel file itself
  is git-ignored per-machine and safe to delete when no invocation
  is running.

### Fixed

- Fastly `service_id` no longer lands under `[local_server]` on
  re-provision; the merged `fastly.toml` correctly carries it at the
  TOML root so `fastly compute deploy` picks it up.
- Fastly cloud `provision` populates `ProvisionOutcome.deployed.service_id`
  from `fastly.toml`; the writeback to `[adapters.fastly.deployed]`
  in `edgezero.toml` no longer silently drops.
- Cloudflare `.dev.vars` commented `__KEY` placeholder now uses
  `<logical>_staging` per spec Task 19 (was
  `<placeholder-<logical>-key>`).
- Cross-adapter `path_mutation_guard` unification (`edgezero-cli`
  test binary): scaffold + push-shim tests share the same mutex, no
  more intermittent CI flakes from PATH-restore races.
- Cloud `config push` now honours the spec's path-containment MUST
  (absolute path + `..` traversal rejection). The strict-local
  "manifest inside adapter crate" check stays `--local`-gated so
  existing cloud fixtures with root-level manifest paths keep
  working.
- Provision `.dev.vars` / `.env` written 0600 on Unix so operator-
  filled secret values are not world-readable.
- Provision line-oriented files reject values containing `\n` or
  `\r` — a malicious env override can no longer split into a second
  `KEY=VALUE` line and inject an unintended env-var.
- Dry-run report emits a **unified diff with 2-line context radius**
  instead of the full pre-image; `.dev.vars` / `.env` operator
  values no longer stream into CI logs.
- Adapter error paths inside `run_local_dry_run` sanitise raw
  `/var/folders/.../edgezero-staging-*` tempdir paths back to the
  project-relative form before surfacing.

### Test coverage

- End-to-end env-overlay: `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`
  now has an integration test that drives it from process env
  through `EnvConfig::store_name()` into the emitted `.edgezero/.env`.
- Case-insensitive adapter arg: `--adapter AXUM` against
  `[adapters.axum]` lowercase now covered (previously only the
  reverse direction was locked).
- Cloudflare `wrangler.toml` schema header preserved at line 1 after
  provision merge into an operator-authored doc.
- `*.toml.hbs` scaffold templates walker asserts no `KEY = ""`
  placeholder leaks past `write_baseline_to_disk`.
- Fastly root-scalar assertions (`service_id`, `[[local_server.kv_stores.sessions]]`
  stub row) now reparse-then-index instead of substring-match, so
  the shipped `service_id`-under-`[local_server]` bug's regression
  class is locked.

### Deprecated / renamed

- Three tests named `provision_local_push_after_provision_preserves_*`
  were renamed to `provision_typed_local_re_run_preserves_*` —
  their bodies never invoked `push_config_entries` and the prior
  name misled readers looking for real push→provision coverage.
  Real push→provision integration coverage is still an open gap.
