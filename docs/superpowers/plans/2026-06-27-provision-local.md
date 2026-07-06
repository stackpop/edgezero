# `provision --local` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `provision --local` and a `provision_typed` trait method so a clean clone can materialise per-adapter local emulator state with zero cloud calls, then `config push --local` only mutates the config-blob table.

**Architecture:** A new `ProvisionMode { Cloud, Local }` parameter threads through the existing `Adapter::provision` trait. Cloud mode keeps today's behaviour. Local mode synthesises minimal manifests via `toml_edit::DocumentMut` when absent, merges per-store bindings, writes adapter-local env files, and never shells out. A typed sibling `provision_typed` runs only from the generated `<app-cli>` and adds `#[secret]`-field placeholders. Dry-run stages a real recursive copy into a `tempfile::TempDir`, dispatches the adapter with `dry_run = false` against the staging tree, then diffs the result back. Cloudflare/Fastly/Spin manifests become gitignored generated state; Axum's `axum.toml` stays tracked.

**Tech Stack:** Rust 1.95.0, Edition 2021, Resolver 2. `toml_edit::DocumentMut` for TOML-preserving merges, `tempfile::TempDir` for dry-run staging, `similar::TextDiff` for diff output, `validator` for manifest schema. No new workspace dependencies.

## Global Constraints

- **Rust 1.95.0**, Edition 2021, Resolver 2, license Apache-2.0. Match `.tool-versions`.
- **No new workspace dependencies.** Promote `toml_edit` and `tempfile` from `[dev-dependencies]` to `[dependencies]` in `crates/edgezero-cli/Cargo.toml` (both already exist as workspace deps).
- **CI gates** must pass at every commit: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-targets`, `cargo check --workspace --all-targets --features "fastly cloudflare spin"`, `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`.
- **Never use `Co-Authored-By` trailers**, "Generated with" footers, or AI bylines in commits or PR bodies.
- **No legacy / compatibility shims.** When changing `ManifestAdapter`'s unknown-subtable handling, frame the change as "remove `deployed` from the unknown-subtable reject list" — not a compatibility path.
- **Mode × dry-run dispatch matrix** (locked by spec §"Mode × dry-run dispatch matrix"):

  | `args.local` | `args.dry_run` | Tempdir staging? | `dry_run` to adapter |
  | --- | --- | --- | --- |
  | No | No | No | `false` |
  | No | Yes | No | `true` |
  | Yes | No | No | `false` |
  | Yes | Yes | **Yes** | **`false`** |

  The bottom row is the ONLY case where `args.dry_run = true` reaches the adapter as `dry_run = false`.
- **`TypedSecretEntry<'_>` lives in `edgezero-adapter`** at `crates/edgezero-adapter/src/registry.rs:178`. Reuse it; never introduce a parallel neutral type.
- **Path containment is mandatory** for every CLI entry point that writes local files (`run_provision`, `run_provision_typed`, `run_config_push_typed` inside its `args.local` arm). Reject absolute paths and any `Component::ParentDir`. Normalise both `project_root` and the joined path before `starts_with`.
- **Cloudflare merge precedence** for managed keys (`id`, `preview_id`): (1) tracked `[adapters.cloudflare.deployed].*` if present; (2) existing local value on the matched binding; (3) placeholder. Sibling operator-authored keys on the same entry are always preserved.
- **Manifest types stay `Deserialize + Validate`**, never `Serialize`. Writeback to `[adapters.<name>.deployed]` lives in the CLI via `toml_edit::DocumentMut`.
- **Spin canonical variable name** is `spin_var = key_value.to_ascii_lowercase()`; env line key is `SPIN_VARIABLE_<spin_var.to_ascii_uppercase()>`.
- **`run_serve` env-file load is adapter-scoped:** `axum` → `<manifest_root>/.edgezero/.env`; `spin` → `<spin_crate_dir>/.env`; other adapters → none.
- **`config push --local` table/key ownership boundary** (spec §"Interaction with `config push --local`"): push only mutates its declared keys; provision-owned tables and sibling keys MUST stay byte-for-byte intact.
- **All four adapter manifests are provision-generated + gitignored.** `axum.toml` joined the Cloudflare/Fastly/Spin set in the 2026-07 amendment: `AxumCliAdapter::synthesise_baseline_manifest` emits it on missing, `.gitignore` and CI enforce it untracked, and `provision --local --dry-run` includes it in the diff allow-list. Adapter merge paths remain no-ops on operator edits.
- **Spec reference:** `docs/superpowers/specs/2026-06-23-provision-local.md` is the single source of truth. Every behaviour rule in this plan traces back to a spec section; consult the spec whenever a task's rationale is unclear.

---

## Section 1 — CLI surface, neutral types, trait method

### Task 1: Add `ProvisionMode` enum + `local` flag on `ProvisionArgs`

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs` (add `ProvisionMode`)
- Modify: `crates/edgezero-cli/src/args.rs:167` (add `local: bool`)
- Test: `crates/edgezero-cli/src/args.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**

- Consumes: nothing
- Produces: `pub enum ProvisionMode { Cloud, Local }` (in `edgezero-adapter`) and `pub local: bool` field on `ProvisionArgs` (clap `#[arg(long)]`, default `false`)

- [ ] **Step 1: Write the failing test** (in `crates/edgezero-cli/src/args.rs`'s test module)

  ```rust
  #[test]
  fn provision_args_local_flag_defaults_false() {
      use clap::Parser;
      #[derive(Parser)]
      struct Cli {
          #[command(flatten)]
          args: ProvisionArgs,
      }
      let cli = Cli::try_parse_from(["bin", "--adapter", "spin"]).unwrap();
      assert!(!cli.args.local);
  }

  #[test]
  fn provision_args_local_flag_parses() {
      use clap::Parser;
      #[derive(Parser)]
      struct Cli {
          #[command(flatten)]
          args: ProvisionArgs,
      }
      let cli = Cli::try_parse_from(["bin", "--adapter", "spin", "--local"]).unwrap();
      assert!(cli.args.local);
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli args::tests::provision_args_local -- --nocapture`
  Expected: FAIL with "no field `local`"

- [ ] **Step 3: Add the `local` field to `ProvisionArgs` AND extend its manual `Default` impl**

  `ProvisionArgs` is `#[non_exhaustive]` AND has a manual `Default` impl at `crates/edgezero-cli/src/args.rs:180`. Both must be updated together — Task 31 (`generate_new` scaffold loop) uses `ProvisionArgs::default()`, and adding a field without updating Default would either fail to compile (if no default value) or silently default to whatever the type's `Default` produces.

  At `args.rs:167` (alongside the existing `dry_run` field):

  ```rust
  /// Switch the flow from cloud-SDK shell-outs to local-file writes.
  /// Adapter-local manifests, env files, and runtime-config TOML are
  /// synthesised or merged in place; no cloud CLIs are invoked. See
  /// spec §"CLI" for the full mode contract.
  #[arg(long)]
  pub local: bool,
  ```

  At `args.rs:180`'s `impl Default for ProvisionArgs`, add the new field initialisation alongside the existing ones:

  ```rust
  impl Default for ProvisionArgs {
      fn default() -> Self {
          Self {
              // ... existing fields untouched ...
              local: false,
          }
      }
  }
  ```

- [ ] **Step 4: Add `ProvisionMode` to the adapter registry**

  In `crates/edgezero-adapter/src/registry.rs` (place alongside the other public types near the top of the file):

  ```rust
  /// Provision dispatch mode. `Cloud` keeps today's cloud-CLI shell-out
  /// behaviour; `Local` writes adapter-local emulator state (no cloud
  /// calls). Threaded through `Adapter::provision` so each adapter
  /// branches once at the top of its impl. See spec §"CLI / trait
  /// surface".
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum ProvisionMode {
      Cloud,
      Local,
  }
  ```

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli args::tests::provision_args_local && cargo test -p edgezero-adapter`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/args.rs crates/edgezero-adapter/src/registry.rs
  git commit -m "Add ProvisionMode + ProvisionArgs.local for provision --local"
  ```

### Task 2: Add neutral `AdapterDeployedState` + `ProvisionOutcome` types

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs`
- Test: `crates/edgezero-adapter/src/registry.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**

- Consumes: nothing
- Produces:
  ```rust
  pub struct AdapterDeployedState {
      pub fields: BTreeMap<String, String>,
      pub sub_tables: BTreeMap<String, BTreeMap<String, String>>,
  }
  pub struct ProvisionOutcome {
      pub status_lines: Vec<String>,
      pub deployed: Option<AdapterDeployedState>,
  }
  ```

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn provision_outcome_default_is_empty() {
      let outcome = ProvisionOutcome::default();
      assert!(outcome.status_lines.is_empty());
      assert!(outcome.deployed.is_none());
  }

  #[test]
  fn adapter_deployed_state_round_trips_via_btreemap() {
      let mut state = AdapterDeployedState::default();
      state.fields.insert("service_id".into(), "SVC1".into());
      let mut kv = BTreeMap::new();
      kv.insert("sessions".to_string(), "abc123".to_string());
      state.sub_tables.insert("kv_namespaces".into(), kv);
      assert_eq!(state.fields["service_id"], "SVC1");
      assert_eq!(state.sub_tables["kv_namespaces"]["sessions"], "abc123");
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter`
  Expected: FAIL with "cannot find type `ProvisionOutcome`"

- [ ] **Step 3: Add the neutral types**

  In `crates/edgezero-adapter/src/registry.rs` near the top of the file (after the existing `use` block, alongside the load-bearing comment at `:218` referenced by the spec):

  ```rust
  use std::collections::BTreeMap;

  /// Adapter-emitted deployed identifiers. Kept neutral (string-keyed
  /// maps only) so `edgezero-adapter` stays dep-free of
  /// `edgezero-core` -- the CLI maps this into the strongly typed
  /// `ManifestAdapterDeployed` shape when writing `edgezero.toml`.
  /// See spec §"Writeback ownership".
  #[derive(Debug, Default, Clone)]
  pub struct AdapterDeployedState {
      pub fields: BTreeMap<String, String>,
      pub sub_tables: BTreeMap<String, BTreeMap<String, String>>,
  }

  /// Return value of `Adapter::provision` (and `provision_typed`).
  /// `status_lines` are operator-facing; `deployed`, when `Some`,
  /// records the cloud-returned identifiers the CLI persists into
  /// `edgezero.toml`'s `[adapters.<name>.deployed]` block. Local
  /// provision returns `deployed: None`.
  #[derive(Debug, Default, Clone)]
  pub struct ProvisionOutcome {
      pub status_lines: Vec<String>,
      pub deployed: Option<AdapterDeployedState>,
  }
  ```

- [ ] **Step 4: Add crate-root re-exports for the new types + the existing surface adapters and CLI consume**

  Today `crates/edgezero-adapter/src/lib.rs` exposes only `pub mod registry;` + `pub mod scaffold;` + the feature-gated `cli_support`. Every plan snippet later that writes `edgezero_adapter::get_adapter(...)`, `edgezero_adapter::ProvisionMode`, `edgezero_adapter::ProvisionOutcome`, or `edgezero_adapter::AdapterDeployedState` would fail to compile without re-exports.

  Add to `crates/edgezero-adapter/src/lib.rs` (alongside the existing `pub mod registry;`):

  ```rust
  // Re-exports so adapters + the CLI can write
  // `edgezero_adapter::TypeName` instead of
  // `edgezero_adapter::registry::TypeName`. Mirrors the surface
  // adapters already touch via `registry::*` imports today.
  pub use registry::{
      get_adapter,
      Adapter,
      AdapterDeployedState,
      ProvisionMode,
      ProvisionOutcome,
      ProvisionStores,
      ResolvedStoreId,
      TypedSecretEntry,
  };
  ```

  Verify the list against the existing public-name surface of `registry.rs` and prune any name that doesn't exist there yet — Tasks 1-4 added `ProvisionMode`, `AdapterDeployedState`, `ProvisionOutcome` to `registry.rs` already; `get_adapter`, `Adapter`, `ProvisionStores`, `ResolvedStoreId`, `TypedSecretEntry` were already there. Task 16c later adds `pub mod env_file;` to the same file.

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter && cargo check -p edgezero-cli`
  Expected: PASS (crate-root names resolve from external callers)

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter/src/registry.rs crates/edgezero-adapter/src/lib.rs
  git commit -m "Add neutral ProvisionOutcome + AdapterDeployedState types + crate-root re-exports"
  ```

### Task 3: Thread `ProvisionMode` + `ProvisionOutcome` through `Adapter::provision`

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs` (trait signature)
- Modify: every `crates/edgezero-adapter-*/src/cli.rs` impl of `Adapter::provision`
- Modify: `crates/edgezero-cli/src/provision.rs` (call site)
- Test: each adapter's existing provision tests

**Interfaces:**

- Consumes: `ProvisionMode`, `ProvisionOutcome`, `AdapterDeployedState` from Task 1-2
- Produces: updated trait `fn provision(manifest_root, adapter_manifest_path, component_selector, stores, deployed: Option<&AdapterDeployedState>, mode: ProvisionMode, dry_run) -> Result<ProvisionOutcome, String>`

- [ ] **Step 1: Run the existing provision suite to capture the green baseline**

  Run: `cargo test --workspace --all-targets provision`
  Expected: PASS (record the test list — Step 5 must keep them all green)

- [ ] **Step 2: Change the trait signature**

  In `crates/edgezero-adapter/src/registry.rs:282`, locate `fn provision(` (currently returns `Result<Vec<String>, String>`). **Preserve the existing parameter order** to minimise churn at call sites — insert `mode` before `dry_run` and `deployed` after `stores`, change the return type:

  ```rust
  fn provision(
      &self,
      manifest_root: &Path,
      adapter_manifest_path: Option<&str>,
      component_selector: Option<&str>,
      stores: &ProvisionStores<'_>,
      deployed: Option<&AdapterDeployedState>,
      mode: ProvisionMode,
      dry_run: bool,
  ) -> Result<ProvisionOutcome, String>;
  ```

  The `deployed` parameter is the same neutral `AdapterDeployedState` Task 8b's `deployed_state_for(&manifest, canonical_adapter_name)` translator already builds for `synthesise_baseline_manifest`. Cloud-arm impls take `deployed = None` (cloud provision creates the deployed state; doesn't consume it). Local-arm impls read it for the precedence rules:
  - **Cloudflare** reads `deployed.and_then(|d| d.sub_tables.get("kv_namespaces"))` to source real namespace ids (spec §"Cloudflare" precedence list at spec:1014).
  - **Fastly** reads `deployed.and_then(|d| d.fields.get("service_id"))` to pin `service_id` in the synthesised `fastly.toml` (spec §"Fastly" at spec:1320).
  - **Axum** ignores it (no deployed-state inputs).
  - **Spin** ignores it (no deployed-state inputs in v1).

  Without this parameter, adapters that need the deployed block would have to re-parse `edgezero.toml` -- breaking the `edgezero-adapter → edgezero-core` dep-free invariant the boundary translator was added to preserve. The CLI MUST pass the same value through both `synthesise_baseline_manifest` AND `provision`; the typed dry-run staging closure (Task 29) builds it once and threads it into both calls.

  Add a doc comment cross-referencing spec §"CLI / trait surface" and §"Writeback ownership". Do NOT add a default impl — every existing impl must update explicitly.

- [ ] **Step 3: Update each adapter's `provision` impl**

  For each of `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/cli.rs`'s existing `fn provision`:

  - Accept the **two** new parameters in this order, matching the trait:
    1. `deployed: Option<&AdapterDeployedState>` between `stores` and `mode` (cloud arm ignores; local arm uses per the precedence above).
    2. `mode: ProvisionMode` between `deployed` and `dry_run`.
  - At the top of the function body, add `match mode { ProvisionMode::Cloud => {} ProvisionMode::Local => return Err("local mode lands in Section 5".to_owned()), }`. This stub keeps the CLI compilable while Section 5 implements each adapter's local arm in turn.
  - Change the return path: where the old impl returned `Ok(status_lines)`, return `Ok(ProvisionOutcome { status_lines, deployed: None })`. Cloud-arm impls always return `deployed: None` at this point; Cloudflare's cloud arm is upgraded in Task 16b to return captured namespace ids.

- [ ] **Step 4: Update the CLI call site**

  In `crates/edgezero-cli/src/provision.rs` find the existing `adapter.provision(...)` call. Update to pass `ProvisionMode::Cloud` (since `--local` is not yet wired through) and to consume the new return type:

  ```rust
  let outcome = adapter.provision(
      manifest_root,
      adapter_cfg.adapter.manifest.as_deref(),
      adapter_cfg.adapter.component.as_deref(),
      &stores,
      None, // cloud arm doesn't consume deployed state; it produces it
      adapter_registry::ProvisionMode::Cloud,
      args.dry_run,
  )?;
  for line in outcome.status_lines {
      println!("{line}");
  }
  // outcome.deployed wiring lands in Task 16.
  ```

- [ ] **Step 5: Run the same provision suite — every prior-green test stays green**

  Run: `cargo test --workspace --all-targets provision`
  Expected: PASS (same tests as Step 1)

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter/src/registry.rs \
          crates/edgezero-adapter-axum/src/cli.rs \
          crates/edgezero-adapter-cloudflare/src/cli.rs \
          crates/edgezero-adapter-fastly/src/cli.rs \
          crates/edgezero-adapter-spin/src/cli.rs \
          crates/edgezero-cli/src/provision.rs
  git commit -m "Thread ProvisionMode + ProvisionOutcome through Adapter::provision"
  ```

### Task 4: Add `Adapter::provision_typed` trait method with default no-op

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs`
- Test: `crates/edgezero-adapter/src/registry.rs` test module

**Interfaces:**

- Consumes: `TypedSecretEntry`, `ProvisionMode`, `ProvisionOutcome`
- Produces: new trait method with default impl returning `ProvisionOutcome::default()`

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn provision_typed_default_impl_returns_empty_outcome() {
      struct StubAdapter;
      // StubAdapter intentionally implements only the required
      // trait methods; `provision_typed` uses the default impl.
      impl Adapter for StubAdapter { /* fill in the minimum required methods using the existing trait surface */ }

      let outcome = StubAdapter
          .provision_typed(
              Path::new("/tmp"),
              None,
              None,
              &[],
              ProvisionMode::Local,
              true,
          )
          .unwrap();
      assert!(outcome.status_lines.is_empty());
      assert!(outcome.deployed.is_none());
  }
  ```

  (Pattern-match `StubAdapter` after the existing `crates/edgezero-adapter/src/registry.rs` test module's stub adapter, which already implements the minimum trait surface.)

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter provision_typed_default`
  Expected: FAIL with "no method `provision_typed`"

- [ ] **Step 3: Add the trait method with default impl**

  In `crates/edgezero-adapter/src/registry.rs`, inside the `pub trait Adapter`, alongside `provision`:

  ```rust
  /// Typed-secret companion to `provision`. Runs ONLY in local mode
  /// (`mode == Local`); cloud mode is a no-op by spec §"CLI / trait
  /// surface". The CLI dispatches this AFTER `provision` on the same
  /// `manifest_root`, so per-store bindings are already in place; this
  /// method only adds adapter-specific per-secret placeholders sourced
  /// from `C::SECRET_FIELDS` (the generic CLI walks them; bundled
  /// `edgezero` cannot).
  ///
  /// The default impl is a no-op so existing adapters compile
  /// untouched while the per-adapter overrides land in Section 5.
  fn provision_typed<'entry>(
      &self,
      _manifest_root: &Path,
      _adapter_manifest_path: Option<&str>,
      _component_selector: Option<&str>,
      _typed_secrets: &[TypedSecretEntry<'entry>],
      _mode: ProvisionMode,
      _dry_run: bool,
  ) -> Result<ProvisionOutcome, String> {
      Ok(ProvisionOutcome::default())
  }
  ```

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter provision_typed_default`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter/src/registry.rs
  git commit -m "Add Adapter::provision_typed trait method with default no-op"
  ```

### Task 5: Extract `run_typed_preflight(&typed, &ctx)`; rewrite validate/push/diff sites

**Files:**

- Modify: `crates/edgezero-cli/src/config.rs` (promote `ValidationContext` to `pub(crate)`, add `load_validation_context_with_options`, add `run_typed_preflight`, add `build_typed_secret_entries`; rewrite validate `:216-217`, push `:284-285`, diff `:415-416`)
- Test: `crates/edgezero-cli/src/config.rs` test module

**Interfaces:**

- Consumes: existing private `typed_secret_checks`, `run_adapter_typed_checks`, `ValidationContext` (type promoted to `pub(crate)`; fields stay private)
- Produces:
  ```rust
  pub(crate) fn load_validation_context_with_options(
      manifest_path: &Path,
      app_config_override: Option<&Path>,
      strict: bool,
      env_overlay: bool,
  ) -> Result<ValidationContext, String>;

  pub(crate) fn run_typed_preflight<C>(
      typed: &C,
      ctx: &ValidationContext,
  ) -> Result<(), String>
  where C: AppConfigMeta;

  pub(crate) fn build_typed_secret_entries<'ctx, C: AppConfigMeta>(
      ctx: &'ctx ValidationContext,
  ) -> Result<Vec<TypedSecretEntry<'ctx>>, String>;
  ```

  An earlier draft of this task proposed a `TypedPreflightInputs<'_, C>` wrapper struct, but that design depends on `Manifest::clone()` (not derived; verified at `crates/edgezero-core/src/manifest.rs:86`) and was rejected by an earlier review. The body below describes the working `&ValidationContext` design.

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn run_typed_preflight_smoke_passes_for_valid_typed_config() {
      // Build a minimal fixture matching the existing validate test
      // pattern. Call load_validation_context to build ctx, then
      // assert run_typed_preflight(&typed, &ctx) returns Ok(()).
      // Follow the fixture pattern of the existing validate_typed_*
      // tests in this module.
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-cli run_typed_preflight_smoke`
  Expected: FAIL with "no function `run_typed_preflight`"

- [ ] **Step 3: Make `ValidationContext` `pub(crate)` + extract the inner loader so push/validate/diff/provision can all call it**

  The naive "construct a fresh `ValidationContext` from a `Manifest`" design does NOT compile: `edgezero_core::manifest::Manifest` does not derive `Clone` (verify: `crates/edgezero-core/src/manifest.rs:86`), and `ManifestLoader` owns its loader state — there is no zero-copy "wrap a borrowed `Manifest`" path. The correct refactor extracts the loader into a primitive-args helper, then has the existing `load_validation_context(args: &ConfigValidateArgs)` delegate to it. Rust has no function overloading — the existing helper at `crates/edgezero-cli/src/config.rs:1128` cannot share its name with a different-arity sibling.

  In `crates/edgezero-cli/src/config.rs`:

  1. Change `struct ValidationContext` at `:77` to `pub(crate) struct ValidationContext` (same crate, push + validate + diff + provision all see it). Fields STAY private. Add `pub(crate)` accessor methods alongside the existing private `manifest()` at `:99` so external callers (Task 29's `run_provision_typed` in `provision.rs`) can read the parts they need without field exposure:

     ```rust
     impl ValidationContext {
         pub(crate) fn manifest(&self) -> &Manifest {
             self.manifest_loader.manifest()
         }
         pub(crate) fn manifest_path(&self) -> &Path {
             &self.manifest_path
         }
         pub(crate) fn app_name(&self) -> &str {
             &self.app_name
         }
         pub(crate) fn app_config_path(&self) -> &Path {
             &self.app_config_path
         }
         pub(crate) fn raw_config(&self) -> &toml::Value {
             &self.raw_config
         }
     }
     ```

     Promote the existing private `fn manifest(&self) -> &Manifest` at `:99` to `pub(crate)` (the body stays one line). The other four are net new. Without these accessors, Task 29 would have to either reach into private fields (won't compile) or duplicate the loader -- both worse than a five-line accessor block.
  2. Add a NEW primitive-args helper (different name) that both the existing wrapper AND provision call:

     ```rust
     /// Build a `ValidationContext` from primitives, honouring
     /// `env_overlay` explicitly. Push (via the existing
     /// `load_validation_context` wrapper below) passes
     /// `env_overlay = !args.no_env`; provision passes
     /// `env_overlay = false` (spec §"Validation"). Mirrors the
     /// existing loader sequence at `config.rs:1128-1161` --
     /// factored out so provision can reuse it without
     /// duplicating loader internals.
     pub(crate) fn load_validation_context_with_options(
         manifest_path: &Path,
         app_config_override: Option<&Path>,
         strict: bool,
         env_overlay: bool,
     ) -> Result<ValidationContext, String> {
         let manifest_loader = ManifestLoader::from_path(manifest_path)
             .map_err(|err| format!("failed to load {}: {err}", manifest_path.display()))?;
         let app_name = manifest_loader
             .manifest()
             .app
             .name
             .clone()
             .ok_or_else(|| format!(
                 "{} has no `[app].name` — required to resolve the typed app-config",
                 manifest_path.display()
             ))?;
         let app_config_path = resolve_app_config_path_primitive(
             app_config_override,
             manifest_path,
             &app_name,
         );
         let mut opts = AppConfigLoadOptions::default();
         opts.env_overlay = env_overlay;
         let raw_config = app_config::load_app_config_raw_with_options(
             &app_config_path, &app_name, &opts,
         )
         .map_err(|err| format_app_config_error(&err))?;
         Ok(ValidationContext {
             app_config_path,
             app_name,
             args_strict: strict,
             manifest_loader,
             manifest_path: manifest_path.to_path_buf(),
             raw_config,
         })
     }
     ```

  3. Refactor the EXISTING `fn load_validation_context(args: &ConfigValidateArgs)` at `:1128` to delegate, preserving its existing signature so validate / diff don't need to change:

     ```rust
     fn load_validation_context(args: &ConfigValidateArgs) -> Result<ValidationContext, String> {
         load_validation_context_with_options(
             &args.manifest,
             args.app_config.as_deref(),
             args.strict,
             !args.no_env,
         )
     }
     ```

  4. Refactor `resolve_app_config_path(args: &ConfigValidateArgs, ...)` at `:1164` into a primitive-args sibling that the new options helper calls. Keep the existing args-shaped one (calls the primitive one) so the old call sites at `:1142` continue to compile:

     ```rust
     fn resolve_app_config_path_primitive(
         explicit: Option<&Path>,
         manifest_path: &Path,
         app_name: &str,
     ) -> PathBuf {
         // body identical to today's resolve_app_config_path body,
         // but reads `explicit` instead of `args.app_config`.
     }

     fn resolve_app_config_path(
         args: &ConfigValidateArgs,
         manifest_path: &Path,
         app_name: &str,
     ) -> PathBuf {
         resolve_app_config_path_primitive(
             args.app_config.as_deref(),
             manifest_path,
             app_name,
         )
     }
     ```

  5. Add the shared typed preflight as a thin wrapper:

     ```rust
     /// The single typed-secret preflight entry point shared by
     /// validate, push, diff, and provision. Runs the same
     /// `typed_secret_checks` + `run_adapter_typed_checks` pair the
     /// push path runs today at `config.rs:284-285`. Caller owns
     /// the `ValidationContext` -- which means caller owns the
     /// env_overlay decision (no implicit reload).
     pub(crate) fn run_typed_preflight<C>(
         typed: &C,
         ctx: &ValidationContext,
     ) -> Result<(), String>
     where
         C: AppConfigMeta,
     {
         typed_secret_checks(typed, ctx)?;
         run_adapter_typed_checks::<C>(ctx)?;
         Ok(())
     }
     ```

  Visibility: `ValidationContext` (type), `load_validation_context_with_options`, `resolve_app_config_path_primitive`, `run_typed_preflight`, and the new `build_typed_secret_entries` helper (below) are `pub(crate)`. `typed_secret_checks`, `run_adapter_typed_checks`, and the wrappers around the primitive helpers stay private.

  6. **Extract the `TypedSecretEntry` slice-builder** from `run_adapter_typed_checks` at `config.rs:1295-1325` so provision can call it too. The existing slice-build loop lives inside that private function; lift it to a sibling helper:

     ```rust
     pub(crate) fn build_typed_secret_entries<'ctx, C: AppConfigMeta>(
         ctx: &'ctx ValidationContext,
     ) -> Result<Vec<TypedSecretEntry<'ctx>>, String> {
         let raw_table = ctx
             .raw_config
             .as_table()
             .ok_or_else(|| "raw app-config was not a TOML table after load".to_owned())?;
         let default_store_id = ctx
             .manifest()
             .stores
             .secrets
             .as_ref()
             .map(StoreDeclaration::default_id);
         let mut entries: Vec<TypedSecretEntry<'_>> = Vec::new();
         for field in C::SECRET_FIELDS {
             // body identical to the existing loop at config.rs:1308-1323
         }
         Ok(entries)
     }
     ```

     Rewrite `run_adapter_typed_checks` to call `build_typed_secret_entries(ctx)?` and then hand the resulting slice to `adapter.validate_typed_secrets(&entries)?` — no behaviour change for push/validate/diff. Provision (Task 29) calls `build_typed_secret_entries(ctx)` directly to get the slice it hands to `provision_typed`.

- [ ] **Step 4: Rewrite the three existing call sites to use the new helper**

  Replace each of these two-line pairs with a single `run_typed_preflight` call against the surrounding `ctx`:

  - validate at `crates/edgezero-cli/src/config.rs:216-217` — already has `ctx` in scope; call `run_typed_preflight(&typed, &ctx)?;`.
  - push at `crates/edgezero-cli/src/config.rs:284-285` — already has `ctx.validation` in scope (a `&ValidationContext`); call `run_typed_preflight(&typed, &ctx.validation)?;`.
  - diff at `crates/edgezero-cli/src/config.rs:415-416` — already has `ctx` in scope; call `run_typed_preflight(&typed, &ctx)?;`.

  The existing `load_validation_context(args: &ConfigValidateArgs)` wrapper now delegates to `load_validation_context_with_options`, so validate / push / diff don't need to change their `ValidationContext` construction. Provision (Task 29) uses `load_validation_context_with_options` directly with `env_overlay = false`.

- [ ] **Step 5: Run the full test suite — every existing typed test stays green**

  Run: `cargo test -p edgezero-cli`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/config.rs
  git commit -m "Extract run_typed_preflight; route validate/push/diff through it"
  ```

---

## Section 2 — Shared path safety

### Task 6: Create `path_safety` module with containment helper + unit tests

**Files:**

- Create: `crates/edgezero-cli/src/path_safety.rs`
- Modify: `crates/edgezero-cli/src/lib.rs` (declare `mod path_safety;`)
- Test: `crates/edgezero-cli/src/path_safety.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**

- Consumes: nothing
- Produces:
  ```rust
  pub(crate) fn assert_provision_paths_contained(
      project_root: &Path,
      adapter_manifest_path: Option<&str>,
      adapter_crate_path: Option<&str>,
  ) -> Result<(), String>;
  pub(crate) fn lexical_normalize(path: &Path) -> PathBuf;
  ```

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use std::path::Path;

      #[test]
      fn rejects_absolute_manifest_path() {
          let err = assert_provision_paths_contained(
              Path::new("."),
              Some("/etc/wrangler.toml"),
              None,
          )
          .unwrap_err();
          assert!(err.contains("must be a project-relative path"), "{err}");
      }

      #[test]
      fn rejects_parent_traversal_in_manifest_path() {
          let err = assert_provision_paths_contained(
              Path::new("."),
              Some("../outside/spin.toml"),
              None,
          )
          .unwrap_err();
          assert!(err.contains("must not contain `..` traversal"), "{err}");
      }

      #[test]
      fn rejects_parent_traversal_in_crate_path() {
          let err = assert_provision_paths_contained(
              Path::new("."),
              None,
              Some("../escape"),
          )
          .unwrap_err();
          assert!(err.contains("must not contain `..` traversal"), "{err}");
      }

      #[test]
      fn accepts_relative_root_default() {
          assert_provision_paths_contained(
              Path::new("."),
              Some("crates/edgezero-adapter-spin/spin.toml"),
              Some("crates/edgezero-adapter-spin"),
          )
          .unwrap();
      }

      #[test]
      fn accepts_nested_relative_root() {
          assert_provision_paths_contained(
              Path::new("examples/app-demo"),
              Some("crates/app-demo-adapter-spin/spin.toml"),
              Some("crates/app-demo-adapter-spin"),
          )
          .unwrap();
      }

      #[test]
      fn accepts_empty_root_string_as_dot() {
          // args.manifest.parent() returns "" for a bare `--manifest edgezero.toml`.
          assert_provision_paths_contained(
              Path::new(""),
              Some("crates/edgezero-adapter-spin/spin.toml"),
              None,
          )
          .unwrap();
      }

      #[test]
      fn rejects_manifest_outside_adapter_crate() {
          // Crate = "crates/cf", but manifest = "tmp/wrangler.toml"
          // (sibling of the crate, NOT inside it). Step 1 passes
          // (both under project root); step 2 must catch the
          // crate-vs-manifest mismatch.
          let err = assert_provision_paths_contained(
              Path::new("."),
              Some("tmp/wrangler.toml"),
              Some("crates/cf"),
          )
          .unwrap_err();
          assert!(err.contains("must resolve inside"), "{err}");
      }

      #[test]
      fn accepts_manifest_under_adapter_crate() {
          assert_provision_paths_contained(
              Path::new("."),
              Some("crates/cf/wrangler.toml"),
              Some("crates/cf"),
          )
          .unwrap();
      }
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli path_safety`
  Expected: FAIL (module does not exist)

- [ ] **Step 3: Create the module**

  Write `crates/edgezero-cli/src/path_safety.rs`:

  ```rust
  //! Path containment for CLI entry points that resolve
  //! manifest-declared paths and let adapters write files through
  //! them. See spec §"Path containment (MUST)".

  use std::path::{Component, Path, PathBuf};

  /// Reject absolute paths and `..` traversal for the
  /// `[adapters.<name>.adapter].manifest` and `.crate` strings, then
  /// assert:
  ///   1. each path resolves under the project root (defence in depth);
  ///   2. when both `.crate` and `.manifest` are set, the manifest
  ///      path resolves under the crate path -- the spec's
  ///      stronger promise that local provision never creates
  ///      files outside the adapter crate or its gitignored
  ///      local-state dirs.
  ///
  /// Callers SHOULD pass the absolute manifest-loader root when
  /// they have it, but the helper defensively normalises so a
  /// relative `args.manifest.parent()` ("" or "examples/app-demo")
  /// compares correctly.
  pub(crate) fn assert_provision_paths_contained(
      project_root: &Path,
      adapter_manifest_path: Option<&str>,
      adapter_crate_path: Option<&str>,
  ) -> Result<(), String> {
      // Treat "" as ".": Path::parent() returns "" for a bare
      // `--manifest edgezero.toml`, and Path::new("").join(...) does
      // NOT prepend anything, so starts_with would fail silently.
      let root_raw = if project_root.as_os_str().is_empty() {
          Path::new(".")
      } else {
          project_root
      };
      let root = lexical_normalize(root_raw);
      // When `root` normalises to "." (caller passed "" or "." --
      // a bare `--manifest edgezero.toml` or an explicit
      // cwd-relative path), the joined-vs-root `starts_with`
      // check is structurally broken: `lexical_normalize` strips
      // the leading `./` from the join, leaving e.g.
      // `crates/cf/wrangler.toml` -- which does NOT start with
      // ".". Skip Step 1's containment check in that case; the
      // absolute + `..` rejection below already guarantees the
      // candidate sits under cwd, and Step 2 (manifest-inside-
      // crate) compares two paths that BOTH go through the same
      // normalisation so the leading-dot strip cancels out
      // there. The relative-root test fixtures
      // (`accepts_relative_root_default`,
      // `accepts_empty_root_string_as_dot`) only pass with this
      // short-circuit in place.
      let do_step1_starts_with = root != Path::new(".");

      // Step 1: each path is project-relative + no `..` + (when
      // root is concretely-rooted) resolves under the project root.
      for (label, raw) in [
          ("[adapters.<name>.adapter].manifest", adapter_manifest_path),
          ("[adapters.<name>.adapter].crate", adapter_crate_path),
      ] {
          let Some(raw) = raw else { continue };
          let candidate = Path::new(raw);
          if candidate.is_absolute() {
              return Err(format!(
                  "{label} must be a project-relative path; got absolute `{raw}`"
              ));
          }
          if candidate
              .components()
              .any(|c| matches!(c, Component::ParentDir))
          {
              return Err(format!(
                  "{label} must not contain `..` traversal; got `{raw}`"
              ));
          }
          if do_step1_starts_with {
              let normalized = lexical_normalize(&root.join(candidate));
              if !normalized.starts_with(&root) {
                  return Err(format!(
                      "{label} resolves outside project root `{}`: `{}`",
                      root.display(),
                      normalized.display()
                  ));
              }
          }
      }

      // Step 2: when both are set, manifest MUST sit inside the
      // adapter crate dir. Closes the spec's stronger promise --
      // without this, crate = "crates/cf" + manifest =
      // "tmp/wrangler.toml" would pass step 1 but write to a path
      // outside the adapter crate.
      if let (Some(crate_raw), Some(manifest_raw)) =
          (adapter_crate_path, adapter_manifest_path)
      {
          let crate_resolved = lexical_normalize(&root.join(Path::new(crate_raw)));
          let manifest_resolved = lexical_normalize(&root.join(Path::new(manifest_raw)));
          if !manifest_resolved.starts_with(&crate_resolved) {
              return Err(format!(
                  "[adapters.<name>.adapter].manifest `{manifest_raw}` must \
                   resolve inside [adapters.<name>.adapter].crate `{crate_raw}`; \
                   resolved manifest path `{}` is not under crate path `{}`",
                  manifest_resolved.display(),
                  crate_resolved.display()
              ));
          }
      }
      Ok(())
  }

  /// Lexically normalise: collapse `.` components and pass `..`
  /// through unchanged (caller already rejected `..`). No
  /// `fs::canonicalize` -- paths may not exist on first-run
  /// bootstrap, and canonicalising would resolve operator-set
  /// symlinks on the project root.
  pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
      let mut out = PathBuf::new();
      for c in path.components() {
          match c {
              Component::CurDir => {}
              other => out.push(other.as_os_str()),
          }
      }
      if out.as_os_str().is_empty() {
          out.push(".");
      }
      out
  }
  ```

- [ ] **Step 4: Declare the module**

  In `crates/edgezero-cli/src/lib.rs` add `mod path_safety;` alongside the other module declarations.

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli path_safety`
  Expected: PASS (all 8 tests green)

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/path_safety.rs crates/edgezero-cli/src/lib.rs
  git commit -m "Add path_safety module with containment helper"
  ```

### Task 7: Wire path safety into `config push --local` + mirror tests beside config fixtures

**Files:**

- Modify: `crates/edgezero-cli/src/config.rs` (insert call at the top of the push `args.local` arm near `:841`)
- Test: `crates/edgezero-cli/src/config.rs` (`#[cfg(test)] mod tests`, beside the existing `~:1543` config fixtures)

**Interfaces:**

- Consumes: `path_safety::assert_provision_paths_contained`
- Produces: push errors out before any adapter call when manifest/crate paths violate containment

- [ ] **Step 1: Write the failing tests** (in `crates/edgezero-cli/src/config.rs` near the existing config-fixture tests)

  ```rust
  #[test]
  fn config_push_local_rejects_parent_traversal_in_adapter_manifest() {
      // Build a minimal manifest fixture where
      // [adapters.spin.adapter].manifest = "../outside/spin.toml",
      // then call run_config_push_typed with args.local = true.
      // Assert the error contains "must not contain `..` traversal".
      // Pattern-match the existing typed-push fixture builders in
      // this module.
  }

  #[test]
  fn config_push_local_rejects_absolute_adapter_manifest() {
      // Same fixture pattern, but manifest = "/tmp/some.toml".
      // Assert error contains "must be a project-relative path".
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli config_push_local_rejects`
  Expected: FAIL (push currently accepts the bad paths)

- [ ] **Step 3: Wire the helper into push**

  In `crates/edgezero-cli/src/config.rs:841` (the push local-write block), at the TOP of the `args.local` arm and BEFORE any adapter dispatch, add:

  ```rust
  if args.local {
      crate::path_safety::assert_provision_paths_contained(
          manifest_root,
          adapter_cfg.adapter.manifest.as_deref(),
          adapter_cfg.adapter.crate_path.as_deref(), // use the existing field name
      )?;
      // ... existing local-write dispatch
  }
  ```

  If the manifest field for the adapter crate path is named differently in `ManifestAdapterConfig`, use the existing field name as-is (see `crates/edgezero-core/src/manifest.rs:379`).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli config_push_local_rejects`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/config.rs
  git commit -m "Wire path_safety into config push --local"
  ```

---

## Section 3 — Local provision orchestration + dry-run staging

### Task 8: Refactor `run_provision` to derive `ProvisionMode` from `args.local` + call path safety

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs`
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `ProvisionMode`, path-safety helper
- Produces: `run_provision` resolves mode + runs containment check + dispatches; tempdir staging lands in Task 10-11

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn provision_local_rejects_parent_traversal_in_adapter_manifest() {
      // Fixture with [adapters.spin.adapter].manifest = "../outside/spin.toml".
      // Call run_provision with args.local = true.
      // Assert error contains "must not contain `..` traversal" AND
      // the project parent dir is unchanged (sentinel mtime check).
  }

  #[test]
  fn provision_local_rejects_absolute_adapter_manifest() {
      // Fixture with manifest = "/tmp/some.toml". Assert error and
      // /tmp/some.toml is absent / unchanged.
  }

  #[test]
  fn provision_local_rejects_parent_traversal_in_adapter_crate() {
      // Mirror against the crate field.
  }

  #[test]
  fn provision_local_accepts_relative_manifest_root_default() {
      // Run with --manifest edgezero.toml (cwd-relative; parent is "").
      // Assert success.
  }

  #[test]
  fn provision_local_accepts_relative_manifest_root_nested() {
      // Run with --manifest examples/app-demo/edgezero.toml.
      // Assert success.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli provision_local_`
  Expected: FAIL

- [ ] **Step 3: Add path-safety call + mode derivation at the top of `run_provision`**

  In `crates/edgezero-cli/src/provision.rs`, at the TOP of `run_provision` (before any other manifest-path use):

  ```rust
  let manifest_root_for_check = args
      .manifest
      .parent()
      .filter(|p| !p.as_os_str().is_empty())
      .unwrap_or_else(|| Path::new("."));
  crate::path_safety::assert_provision_paths_contained(
      manifest_root_for_check,
      adapter_cfg.adapter.manifest.as_deref(),
      adapter_cfg.adapter.crate_path.as_deref(),
  )?;

  let mode = if args.local {
      adapter_registry::ProvisionMode::Local
  } else {
      adapter_registry::ProvisionMode::Cloud
  };
  ```

  Then thread `mode` into the `adapter.provision(...)` call (replacing the hard-coded `ProvisionMode::Cloud` from Task 3).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli provision_local_`
  Expected: PASS (all 5 tests)

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Wire path_safety + ProvisionMode into run_provision"
  ```

### Task 8b: CLI-owned first-run bootstrap synthesis BEFORE `validate_adapter_manifest`

**Files:**

- Modify: `crates/edgezero-adapter/src/registry.rs` (add the `synthesise_baseline_manifest` trait method with default impl alongside `provision`/`provision_typed`)
- Modify: `crates/edgezero-cli/src/provision.rs` (insert bootstrap restructure between `strict_handler_paths` at `:77` and `validate_adapter_manifest` at `:83`)
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `Adapter::synthesise_baseline_manifest(&self, ...)` trait method (new), added to `Adapter` with a default `Ok(Vec::new())` impl. Adapters with a primitive synthesiser (Cloudflare in Task 17, Fastly in Task 21, Spin in Task 24, plus `runtime-config.toml` from Task 24) override and return `Ok(vec![(file_path, contents), ...])` — one tuple per file they synthesise.
- Produces: when `args.local && !manifest_exists`, the CLI calls each registered synthesiser and writes the result BEFORE `validate_adapter_manifest` is invoked. Spec §"First-run bootstrap re-order in `run_provision`" — this is the load-bearing reorder.

This task lands the trait method + CLI call site. The per-adapter synthesiser overrides land in Tasks 17 / 21 / 24 (they replace the default `Ok(Vec::new())` with concrete synthesis). Until those land, the trait method returns an empty vec and bootstrap is a no-op — but the call site is in place so Section 5 just plugs into it.

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn provision_local_synthesises_missing_adapter_manifest_before_validation() {
      // Fixture: clean tempdir with edgezero.toml declaring
      //   [adapters.spin.adapter] crate = "crates/spin",
      //   manifest = "crates/spin/spin.toml"
      // and crates/spin/ exists but spin.toml does NOT.
      // Plug in a fake Spin adapter whose synthesise_baseline_manifest
      // returns the CONFIGURED adapter manifest path verbatim
      // (NOT a static "spin.toml") so the synthesised file lands
      // at <root>/crates/spin/spin.toml -- mirrors what the real
      // Spin override at Task 24 does. The fake reads
      // `adapter_manifest_path` and returns
      // Ok(vec![(PathBuf::from(adapter_manifest_path.unwrap_or("spin.toml")),
      //          "# stub\n".to_string())]).
      // The fake's validate_adapter_manifest asserts the file
      // exists when called.
      // Run run_provision with args.local = true, args.dry_run = false.
      // Assert Ok AND <root>/crates/spin/spin.toml contains "# stub"
      // (NOT <root>/spin.toml -- that would be the regression
      // shape this test guards against).
  }

  #[test]
  fn provision_local_bootstrap_is_a_no_op_when_manifest_already_present() {
      // Same fixture but spin.toml already exists with operator content.
      // Run provision; assert spin.toml content is byte-for-byte unchanged.
  }

  #[test]
  fn provision_cloud_never_runs_bootstrap_synthesis() {
      // Same missing-manifest fixture but args.local = false.
      // Plug in a fake synthesiser that panics if called.
      // Run run_provision. Assert error mentions the missing manifest
      // (validation fails as today) AND the synthesiser was never called.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli provision_local_synthesises provision_local_bootstrap provision_cloud_never_runs_bootstrap`
  Expected: FAIL

- [ ] **Step 3: Add the trait method (neutral types only — no `&Manifest`, no `stores`)**

  `edgezero-adapter` has ZERO dependency on `edgezero-core` (verified: `crates/edgezero-adapter/Cargo.toml` lists no core dep; `crates/edgezero-adapter/src/registry.rs:1-3` imports only `std::*`). The trait method MUST stay on that boundary: it cannot take `&edgezero_core::manifest::Manifest`. Cross-crate state is handed in via the already-neutral `AdapterDeployedState` (added in Task 2 and lives in this same crate). The CLI translates `&Manifest` → `Option<&AdapterDeployedState>` before calling, so adapters never need to name `Manifest`.

  In `crates/edgezero-adapter/src/registry.rs`, alongside `provision` and `provision_typed`:

  ```rust
  /// First-run bootstrap synthesiser, called by the CLI ONLY when
  /// `mode == Local` AND the adapter manifest (or related local
  /// files like `runtime-config.toml`) is absent. Returns
  /// `Ok(Vec::new())` for adapters that own no synthesised local
  /// state (e.g. Axum — `axum.toml` stays tracked).
  ///
  /// Each `(relative_path, contents)` tuple is written by the CLI
  /// under `manifest_root` BEFORE `validate_adapter_manifest`
  /// runs, so a clean clone can pass validation. See spec
  /// §"First-run bootstrap re-order in `run_provision`".
  ///
  /// **Boundary contract (MUST):** signature uses only `std` +
  /// types defined IN this crate. Adapters that need values from
  /// the parent manifest (Fastly reads `service_id`) receive them
  /// through the neutral `Option<&AdapterDeployedState>` argument
  /// -- the CLI translates from `&Manifest` to
  /// `AdapterDeployedState` at the call site. This preserves the
  /// load-bearing "edgezero-adapter is dep-free of edgezero-core"
  /// invariant documented at registry.rs:220.
  ///
  /// **Inputs available before `stores` is built** (the existing
  /// `crates/edgezero-cli/src/provision.rs:106` `ProvisionStores`
  /// construction sits AFTER validation, which Task 8b reorders
  /// around): app_name, adapter manifest path, component
  /// selector, deployed-state for THIS adapter.
  fn synthesise_baseline_manifest(
      &self,
      _manifest_root: &Path,
      _adapter_manifest_path: Option<&str>,
      _component_selector: Option<&str>,
      _app_name: &str,
      _deployed: Option<&AdapterDeployedState>,
  ) -> Result<Vec<(std::path::PathBuf, String)>, String> {
      Ok(Vec::new())
  }
  ```

  The default impl is `Ok(Vec::new())` so every existing adapter compiles untouched. Cloudflare / Fastly / Spin override this in Tasks 17 / 21 / 24. Axum keeps the default.

- [ ] **Step 4: Restructure `run_provision` so synthesis + validation + dispatch all see the same root (real worktree OR tempdir)**

  Today's `crates/edgezero-cli/src/provision.rs` runs `strict_handler_paths` then `validate_adapter_manifest` (line 83) then builds `stores` (line 106) then `adapter.provision`. The reorder this task lands:

  1. Resolve `manifest_root` (existing logic at `:78-82`).
  2. Resolve `app_name` from the already-loaded manifest (no `stores` needed; mirror the `manifest.app.name.clone()` access push uses at `config.rs:1135`).
  3. Translate the parent `&Manifest`'s deployed block for THIS adapter into the neutral `AdapterDeployedState` shape the trait method accepts. Lives at the CLI boundary so adapters never name `Manifest`:

     ```rust
     fn deployed_state_for(
         manifest: &edgezero_core::manifest::Manifest,
         canonical_adapter_name: &str,
     ) -> Option<edgezero_adapter::AdapterDeployedState> {
         let entry = manifest.adapter_entry(canonical_adapter_name)?;
         let typed = entry.1.deployed.as_ref()?;
         let mut state = edgezero_adapter::AdapterDeployedState::default();
         if let Some(svc) = typed.service_id.as_ref() {
             state.fields.insert("service_id".to_string(), svc.clone());
         }
         if !typed.kv_namespaces.is_empty() {
             state.sub_tables.insert(
                 "kv_namespaces".to_string(),
                 typed.kv_namespaces.clone(),
             );
         }
         if !typed.preview_kv_namespaces.is_empty() {
             state.sub_tables.insert(
                 "preview_kv_namespaces".to_string(),
                 typed.preview_kv_namespaces.clone(),
             );
         }
         Some(state)
     }
     ```

  4. Branch on the dispatch matrix BEFORE validation. Synthesis is called ONLY in the local arms — cloud must never invoke `synthesise_baseline_manifest`, per the spec contract ("`args.local && !manifest_exists`") and the regression test from Step 1 (`provision_cloud_never_runs_bootstrap_synthesis`). Computing baseline_pairs above the match would silently fire the synthesiser for cloud mode and fail that test.

     ```rust
     // Translate the manifest's deployed block ONCE; pass to
     // both synthesise_baseline_manifest and provision so the
     // local arms see the same Option<&AdapterDeployedState>.
     let deployed = deployed_state_for(manifest, canonical_adapter_name);
     match (args.local, args.dry_run) {
         (false, dry_run) => {
             // Cloud mode: NO synthesis. Validate against the real
             // worktree as today, then dispatch. Cloud arm passes
             // `deployed = None` -- cloud provision PRODUCES
             // deployed state, doesn't consume it.
             adapter.validate_adapter_manifest(
                 manifest_root,
                 adapter_cfg.adapter.manifest.as_deref(),
                 adapter_cfg.adapter.component.as_deref(),
             )?;
             let owned_stores = build_stores_against(
                 manifest_root, args, adapter, manifest,
             )?;
             let stores = owned_stores.as_refs();
             adapter.provision(
                 manifest_root, /* …existing args… */, &stores,
                 None,
                 adapter_registry::ProvisionMode::Cloud, dry_run,
             )?
         }
         (true, false) => {
             // Local real-write: synthesise baseline + materialise
             // into the worktree, then validate + dispatch against
             // it. Synthesis happens INSIDE this arm so cloud never
             // touches it.
             let baseline_pairs = adapter.synthesise_baseline_manifest(
                 manifest_root,
                 adapter_cfg.adapter.manifest.as_deref(),
                 adapter_cfg.adapter.component.as_deref(),
                 &app_name,
                 deployed.as_ref(),
             )?;
             write_baseline_to_disk(manifest_root, &baseline_pairs)?;
             adapter.validate_adapter_manifest(
                 manifest_root,
                 adapter_cfg.adapter.manifest.as_deref(),
                 adapter_cfg.adapter.component.as_deref(),
             )?;
             let owned_stores = build_stores_against(
                 manifest_root, args, adapter, manifest,
             )?;
             let stores = owned_stores.as_refs();
             adapter.provision(
                 manifest_root, /* …existing args… */, &stores,
                 deployed.as_ref(),
                 adapter_registry::ProvisionMode::Local, false,
             )?
         }
         (true, true) => {
             // Local dry-run: synthesise baseline (still INSIDE
             // this arm; cloud never sees the call), stage to
             // tempdir, write baseline INTO the tempdir, validate
             // + dispatch against the tempdir.
             let baseline_pairs = adapter.synthesise_baseline_manifest(
                 manifest_root,
                 adapter_cfg.adapter.manifest.as_deref(),
                 adapter_cfg.adapter.component.as_deref(),
                 &app_name,
                 deployed.as_ref(),
             )?;
             // Pass the project-relative crate path directly; no
             // need to compute an absolute path and strip back to
             // relative inside run_with_staging.
             let adapter_crate_rel = adapter_cfg
                 .adapter
                 .crate_path
                 .as_deref()
                 .map(Path::new)
                 .unwrap_or_else(|| Path::new("."));
             let (outcome, _tempdir) = run_with_staging(
                 manifest_root,
                 adapter_crate_rel,
                 |staged_root, _staged_crate| {
                     write_baseline_to_disk(staged_root, &baseline_pairs)?;
                     adapter.validate_adapter_manifest(
                         staged_root,
                         adapter_cfg.adapter.manifest.as_deref(),
                         adapter_cfg.adapter.component.as_deref(),
                     )?;
                     let owned_stores = build_stores_against(
                         staged_root, args, adapter, manifest,
                     )?;
                     let stores = owned_stores.as_refs();
                     adapter.provision(
                         staged_root, /* …existing args… */, &stores,
                         deployed.as_ref(),
                         adapter_registry::ProvisionMode::Local, false,
                     )
                 },
             )?;
             outcome
         }
     }
     ```

  Helper:

  ```rust
  /// Write each (rel, contents) baseline pair under `root`, skipping
  /// files that already exist (preserves operator content + earlier
  /// synthesis). Used for both worktree writes (real-write local) and
  /// tempdir writes (dry-run staging) — the only difference is which
  /// root is passed in.
  fn write_baseline_to_disk(
      root: &Path,
      pairs: &[(PathBuf, String)],
  ) -> Result<(), String> {
      for (rel_path, contents) in pairs {
          let abs = root.join(rel_path);
          if abs.exists() {
              continue;
          }
          if let Some(parent) = abs.parent() {
              std::fs::create_dir_all(parent)
                  .map_err(|e| format!("create {}: {e}", parent.display()))?;
          }
          std::fs::write(&abs, contents)
              .map_err(|e| format!("write {}: {e}", abs.display()))?;
      }
      Ok(())
  }
  ```

  Key restructure rules:
  - `stores` construction stays at its current position relative to validation, BUT moves INSIDE each arm so each arm's `manifest_root` (real or tempdir) flows through. The existing `EnvConfig::from_env()` + `reject_merged_id_collisions` checks at `provision.rs:106` go inside the arms too.
  - `strict_handler_paths(manifest)?;` stays where it is (line 77) — manifest-level only, no per-arm change.
  - The synthesiser call lives INSIDE each local arm (`(true, false)` and `(true, true)`), NOT before the match. Cloud mode (`(false, _)`) MUST NEVER call `synthesise_baseline_manifest` -- the `provision_cloud_never_runs_bootstrap_synthesis` regression test from Step 1 asserts this. Moving the call above the match would silently fire the synthesiser for cloud mode and fail that test.
  - For dry-run, the diff + status rewriting Task 12 lands wraps the staged outcome before the tempdir drops.

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli provision_local_synthesises provision_local_bootstrap provision_cloud_never_runs_bootstrap`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter/src/registry.rs crates/edgezero-cli/src/provision.rs
  git commit -m "Add Adapter::synthesise_baseline_manifest + CLI bootstrap before validate"
  ```

**Section 5 follow-up**: Tasks 17 (Cloudflare), 21 (Fastly), 24 (Spin) MUST also override `synthesise_baseline_manifest` to return their primitive synthesiser output as `(PathBuf, String)` pairs — Cloudflare returns `[("wrangler.toml", synthesise_wrangler_toml(...))]`, Fastly returns `[("fastly.toml", synthesise_fastly_toml(...))]`, Spin returns BOTH `[("spin.toml", ...), ("runtime-config.toml", ...)]`. Each task's checklist already adds the synthesise functions; one extra step per task to wire the override.

### Task 9: Add recursive copy helper for dry-run staging

**Files:**

- Create: `crates/edgezero-cli/src/copy_tree.rs`
- Modify: `crates/edgezero-cli/src/lib.rs` (declare `mod copy_tree;`)
- Test: `crates/edgezero-cli/src/copy_tree.rs` test module

**Interfaces:**

- Consumes: nothing
- Produces: `pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()>`

- [ ] **Step 1: Write the failing test**

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use std::fs;
      use tempfile::TempDir;

      #[test]
      fn copies_nested_files_and_dirs() {
          let src = TempDir::new().unwrap();
          fs::create_dir_all(src.path().join("a/b")).unwrap();
          fs::write(src.path().join("a/top.toml"), "x = 1").unwrap();
          fs::write(src.path().join("a/b/nested.toml"), "y = 2").unwrap();

          let dst = TempDir::new().unwrap();
          copy_dir_recursive(src.path(), dst.path()).unwrap();

          assert_eq!(
              fs::read_to_string(dst.path().join("a/top.toml")).unwrap(),
              "x = 1"
          );
          assert_eq!(
              fs::read_to_string(dst.path().join("a/b/nested.toml")).unwrap(),
              "y = 2"
          );
      }

      #[test]
      fn missing_src_returns_error() {
          let dst = TempDir::new().unwrap();
          assert!(copy_dir_recursive(Path::new("/nonexistent"), dst.path()).is_err());
      }
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-cli copy_tree`
  Expected: FAIL (module does not exist)

- [ ] **Step 3: Write the helper**

  `crates/edgezero-cli/src/copy_tree.rs`:

  ```rust
  //! Small internal recursive directory copy used by `provision
  //! --local --dry-run` to stage mutable adapter paths. No new
  //! workspace dep -- built on `std::fs` only. Preserves regular
  //! files and re-creates directories; symlinks and special files
  //! are out of scope per spec §"Dry-run".

  use std::fs;
  use std::io;
  use std::path::Path;

  pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
      fs::create_dir_all(dst)?;
      for entry in fs::read_dir(src)? {
          let entry = entry?;
          let file_type = entry.file_type()?;
          let src_path = entry.path();
          let dst_path = dst.join(entry.file_name());
          if file_type.is_dir() {
              copy_dir_recursive(&src_path, &dst_path)?;
          } else if file_type.is_file() {
              fs::copy(&src_path, &dst_path)?;
          }
          // Symlinks / special files intentionally skipped.
      }
      Ok(())
  }
  ```

- [ ] **Step 4: Promote `tempfile` to a runtime CLI dep**

  In `crates/edgezero-cli/Cargo.toml`, move the existing `tempfile = { workspace = true }` line from `[dev-dependencies]` (line 48) into `[dependencies]`. The line stays valid in dev-deps too — leave it ONLY in `[dependencies]` to avoid duplicate-dep clippy warnings.

- [ ] **Step 5: Add `toml_edit` to runtime CLI deps**

  In `crates/edgezero-cli/Cargo.toml`'s `[dependencies]` block, add `toml_edit = { workspace = true }` (the workspace entry already exists; adapter crates already depend on it).

- [ ] **Step 6: Declare the new module**

  In `crates/edgezero-cli/src/lib.rs`, add `mod copy_tree;`.

- [ ] **Step 7: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli copy_tree && cargo check -p edgezero-cli --all-features`
  Expected: PASS

- [ ] **Step 8: Commit**

  ```bash
  git add crates/edgezero-cli/src/copy_tree.rs crates/edgezero-cli/src/lib.rs crates/edgezero-cli/Cargo.toml
  git commit -m "Add copy_tree helper; promote tempfile + toml_edit to CLI runtime deps"
  ```

### Task 10: Add `run_with_staging` helper that hosts both base + typed provision in one tempdir

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs`
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `copy_tree::copy_dir_recursive`, `tempfile::TempDir`
- Produces:
  ```rust
  pub(crate) fn run_with_staging<F, R>(
      project_root: &Path,
      /// PROJECT-RELATIVE adapter crate path (e.g. "crates/cf"
      /// or "."), NOT an absolute path. The old shape took an
      /// absolute path and computed `crate_rel` via
      /// `strip_prefix(project_root)` -- which silently fails
      /// for the default `--manifest edgezero.toml` shape where
      /// `project_root == "."` and `adapter_crate_dir == "crates/cf"`
      /// (Rust's `Path::strip_prefix` does NOT treat "." as a
      /// prefix of "crates/cf"). Taking the relative path
      /// directly sidesteps the issue entirely; the caller
      /// already has this value from
      /// `adapter_cfg.adapter.crate_path.as_deref()`.
      adapter_crate_rel: &Path,
      body: F,
  ) -> Result<(R, tempfile::TempDir), String>
  where
      F: FnOnce(&Path /* staged project_root */, &Path /* staged adapter_crate_dir */) -> Result<R, String>;
  ```

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn run_with_staging_drops_tempdir_after_body() {
      use std::fs;
      let project = tempfile::TempDir::new().unwrap();
      fs::write(project.path().join("edgezero.toml"), "x").unwrap();
      let adapter_crate_rel = Path::new("crates/sample");
      let adapter_crate_abs = project.path().join(adapter_crate_rel);
      fs::create_dir_all(&adapter_crate_abs).unwrap();
      fs::write(adapter_crate_abs.join("manifest.toml"), "y").unwrap();

      let staged_paths = run_with_staging(
          project.path(),
          adapter_crate_rel,
          |staged_root, staged_crate| {
              Ok((staged_root.to_path_buf(), staged_crate.to_path_buf()))
          },
      )
      .unwrap();
      let (staged_root, staged_crate) = staged_paths.0;
      // After staging the original project tree is byte-identical:
      assert_eq!(fs::read_to_string(project.path().join("edgezero.toml")).unwrap(), "x");
      // Staged copies existed during body execution:
      assert!(staged_root.is_absolute());
      assert!(staged_crate.starts_with(&staged_root));
  }

  #[test]
  fn run_with_staging_copies_edgezero_toml_into_staged_root() {
      // Regression for the relative-source-symlink bug fixed in
      // an earlier review round AND the strip_prefix bug fixed
      // by switching to project-RELATIVE crate paths (Task 10
      // signature). The test reads staged_root/edgezero.toml
      // INSIDE the closure and asserts the bytes match the
      // project file's bytes. Uses an ABSOLUTE project_root to
      // avoid mutating process cwd -- the strip_prefix bug is
      // not about relative project_root resolution itself, it's
      // about the staging helper computing crate_rel
      // incorrectly. The relative-root case (manifest_root = ".")
      // is exercised end-to-end by the path_safety tests
      // (Task 6) and the worktree-clean dry-run test (Task 13).
      use std::fs;
      let project = tempfile::TempDir::new().unwrap();
      fs::write(project.path().join("edgezero.toml"), "real-project-bytes\n").unwrap();
      let adapter_crate_rel = Path::new("crates/sample");
      fs::create_dir_all(project.path().join(adapter_crate_rel)).unwrap();
      fs::write(
          project.path().join(adapter_crate_rel).join("manifest.toml"),
          "x",
      )
      .unwrap();

      let observed = run_with_staging(
          project.path(),
          adapter_crate_rel,
          |staged_root, _staged_crate| {
              fs::read_to_string(staged_root.join("edgezero.toml"))
                  .map_err(|e| format!("read staged edgezero.toml: {e}"))
          },
      )
      .unwrap();
      assert_eq!(observed.0, "real-project-bytes\n");
  }
  ```

  No `scopeguard` (forbidden by the "no new workspace deps" rule); no process-cwd mutation (which would require a global test lock for thread-safety); no test-specific RAII helpers. Test discipline: pass absolute paths in tests; let the production callers thread relative paths through the path-safety + staging stack where they're handled correctly.

- [ ] **Step 2: Run BOTH tests to verify they fail**

  Run: `cargo test -p edgezero-cli run_with_staging`
  Expected: FAIL (both `run_with_staging_drops_tempdir_after_body` AND `run_with_staging_copies_edgezero_toml_into_staged_root` listed in the failing-tests output).

- [ ] **Step 3: Implement `run_with_staging`**

  In `crates/edgezero-cli/src/provision.rs`:

  ```rust
  /// Stage a real recursive copy of the adapter crate dir AND the
  /// `.edgezero/` dir (if present) under a fresh `TempDir`, then
  /// invoke `body` with the staged paths. The original project
  /// worktree is never mutated. Caller is responsible for diffing
  /// the staged tree against the project tree before the returned
  /// TempDir drops. See spec §"Dry-run".
  pub(crate) fn run_with_staging<F, R>(
      project_root: &Path,
      adapter_crate_rel: &Path,
      body: F,
  ) -> Result<(R, tempfile::TempDir), String>
  where
      F: FnOnce(&Path, &Path) -> Result<R, String>,
  {
      let tempdir = tempfile::TempDir::new()
          .map_err(|e| format!("failed to create staging tempdir: {e}"))?;
      let staged_root = tempdir.path();

      // Copy edgezero.toml (read-only input). We previously used
      // a symlink as an optimisation, but for the default
      // `--manifest edgezero.toml` shape `project_root` is "."
      // and `project_root.join("edgezero.toml")` is the relative
      // path `./edgezero.toml`. Unix `symlink(src, dst)`
      // interprets a relative `src` AS RELATIVE TO `dst`'s parent
      // dir -- so `staged_root/edgezero.toml -> ./edgezero.toml`
      // resolves to `staged_root/edgezero.toml` itself, a broken
      // symlink that points at the staged tree instead of the
      // real project file. A path-based reader inside the closure
      // would either fail (file doesn't exist) or read the wrong
      // content. Copying sidesteps this entirely; `edgezero.toml`
      // is small enough that the perf cost is irrelevant.
      let edgezero_toml = project_root.join("edgezero.toml");
      if edgezero_toml.exists() {
          let staged_edgezero = staged_root.join("edgezero.toml");
          if let Some(parent) = staged_edgezero.parent() {
              std::fs::create_dir_all(parent)
                  .map_err(|e| format!("failed to create staged parent dir: {e}"))?;
          }
          std::fs::copy(&edgezero_toml, &staged_edgezero)
              .map_err(|e| format!("failed to stage edgezero.toml: {e}"))?;
      }

      // Real-copy the adapter crate dir (mutable). `adapter_crate_rel`
      // is project-relative (e.g. "crates/cf" or "."), so the
      // source and staged paths derive cleanly from it. No
      // strip_prefix needed -- removing that call sidesteps the
      // "." vs "crates/cf" Path::strip_prefix bug.
      let src_crate = project_root.join(adapter_crate_rel);
      let staged_crate = staged_root.join(adapter_crate_rel);
      crate::copy_tree::copy_dir_recursive(&src_crate, &staged_crate)
          .map_err(|e| format!("failed to stage adapter crate dir: {e}"))?;

      // Real-copy `.edgezero/` if present; otherwise create empty.
      let dot_edgezero = project_root.join(".edgezero");
      let staged_dot = staged_root.join(".edgezero");
      if dot_edgezero.exists() {
          crate::copy_tree::copy_dir_recursive(&dot_edgezero, &staged_dot)
              .map_err(|e| format!("failed to stage .edgezero/: {e}"))?;
      } else {
          std::fs::create_dir_all(&staged_dot)
              .map_err(|e| format!("failed to create staged .edgezero/: {e}"))?;
      }

      let result = body(staged_root, &staged_crate)?;
      Ok((result, tempdir))
  }

  // Previous revs had a `symlink_or_copy_file` helper for the
  // read-only inputs; it's been removed because the only call
  // site (edgezero.toml staging above) now copies unconditionally
  // to avoid the relative-symlink-resolution bug. If a future
  // input ever needs symlinking, the helper can be reintroduced
  // -- but the relative-root case MUST canonicalize the source
  // before symlinking, OR keep using `std::fs::copy`.
  ```

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-cli run_with_staging`
  Expected: PASS (both `run_with_staging_drops_tempdir_after_body` AND `run_with_staging_copies_edgezero_toml_into_staged_root` green)

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Add run_with_staging tempdir helper for dry-run staging"
  ```

### Task 11: Finalise the mode×dry-run dispatch matrix wrapper (consumes Task 8b + 10)

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs`
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `run_with_staging` (Task 10), `write_baseline_to_disk` + the three-arm match Task 8b sketches
- Produces: behavioural tests covering all four cells of the matrix from Global Constraints

Task 8b lays the structural reorder (synthesis + validation + dispatch all see the same root). This task adds the behavioural test coverage for the four cells of the dispatch matrix and resolves the `build_stores` placeholder Task 8b's sketch references. The previous draft also listed `resolve_adapter_crate_dir` here, but that helper is no longer introduced -- Task 10's `run_with_staging` now takes the project-relative crate path directly (Task 10 step 3 has the rationale).

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn provision_local_dry_run_leaves_worktree_clean() {
      // Build a clean app-demo-like fixture in a tempdir.
      // Snapshot every file's (path, mtime) before the call.
      // Run run_provision with args.local = true, args.dry_run = true.
      // Assert exit Ok and the (path, mtime) snapshot is unchanged.
  }

  #[test]
  fn provision_local_no_dry_run_writes_to_worktree() {
      // Same fixture, args.local = true, args.dry_run = false.
      // Assert the expected adapter manifest now exists.
  }

  #[test]
  fn provision_cloud_dry_run_passes_dry_run_true_to_adapter() {
      // Install a fake adapter that records the dry_run value it
      // received. Run args.local = false, args.dry_run = true.
      // Assert the adapter saw dry_run = true.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli provision_local_dry_run provision_cloud_dry_run`
  Expected: FAIL

- [ ] **Step 3: Resolve the helpers Task 8b's sketch referenced**

  Task 8b's restructure references two helpers that need concrete implementations:

  ```rust
  // `resolve_adapter_crate_dir` is NOT introduced here. The
  // previous draft had the dispatch matrix pre-compute an
  // absolute crate dir and pass it to `run_with_staging`, which
  // then strip_prefix'd it back -- a brittle round-trip that
  // failed for the default `--manifest edgezero.toml` shape
  // (`Path::new("crates/cf").strip_prefix(Path::new("."))` is
  // an Err). The current shape passes the project-RELATIVE
  // crate path directly into `run_with_staging` (Task 10), so
  // no separate "resolve" step is needed inside the dispatch
  // matrix. `run_serve` still has its own
  // `resolve_adapter_crate_dir_for_serve` helper (Task 34)
  // because that path joins the spin crate dir against the
  // manifest root for the env-file load -- different concern,
  // different call site.

  /// Owned counterpart to the borrowed `ProvisionStores<'_>` at
  /// `crates/edgezero-adapter/src/registry.rs:91`. We need owned
  /// `Vec<ResolvedStoreId>` storage so the dispatch matrix can
  /// keep the stores alive across the staging closure, then
  /// hand the adapter a fresh `ProvisionStores<'_>` borrowing
  /// the vecs. Returning a `ProvisionStores<'_>` directly from
  /// `build_stores_against` (the obvious shape) does NOT compile
  /// -- the resolved `Vec<ResolvedStoreId>` locals would drop at
  /// function return, leaving the returned slices dangling.
  pub(crate) struct OwnedProvisionStores {
      pub config: Vec<ResolvedStoreId>,
      pub kv: Vec<ResolvedStoreId>,
      pub secrets: Vec<ResolvedStoreId>,
  }

  impl OwnedProvisionStores {
      pub fn as_refs(&self) -> ProvisionStores<'_> {
          ProvisionStores {
              config: &self.config,
              kv: &self.kv,
              secrets: &self.secrets,
          }
      }
  }

  /// Replicate the existing store-construction block at
  /// `crates/edgezero-cli/src/provision.rs:106-110` so each arm
  /// of the dispatch matrix can build stores against its own
  /// root. Returns the OWNED form so the caller can outlive the
  /// builder (call `.as_refs()` immediately before each adapter
  /// dispatch). Mechanical lift -- same `EnvConfig::from_env()`,
  /// `reject_merged_id_collisions`, `resolve_kind` calls, just
  /// owned at the boundary.
  fn build_stores_against(
      _root: &Path,
      args: &ProvisionArgs,
      adapter: &dyn Adapter,
      manifest: &Manifest,
  ) -> Result<OwnedProvisionStores, String> {
      // `_root` is reserved for future per-root state but unused
      // today. Prefix with `_` to silence `unused_variables` under
      // `-D warnings` while keeping the precedent for future
      // additions. Remove the underscore the moment a caller
      // needs the value.
      let env_config = EnvConfig::from_env();
      reject_merged_id_collisions(&args.adapter, adapter, manifest, &env_config)?;
      Ok(OwnedProvisionStores {
          config: resolve_kind(manifest.stores.config.as_ref(), &env_config, "config"),
          kv:     resolve_kind(manifest.stores.kv.as_ref(),     &env_config, "kv"),
          secrets: resolve_kind(manifest.stores.secrets.as_ref(), &env_config, "secrets"),
      })
  }
  ```

  All three helpers go in `crates/edgezero-cli/src/provision.rs` as private items (and `pub(crate)` for `OwnedProvisionStores` if Task 29 imports it from there). Call sites pattern `let owned = build_stores_against(...)?; adapter.provision(..., &owned.as_refs(), ...);` so the `owned` vecs outlive the dispatch.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli provision_local_dry_run provision_cloud_dry_run`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Wire mode x dry-run dispatch matrix into run_provision"
  ```

### Task 12: Add diff allow-list + "would write" status rewriting in dry-run

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs`
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `similar::TextDiff` (already a workspace dep used by `config diff`)
- Produces: `pub(crate) fn render_dry_run_report(...)` that prints "would write" status + per-file diff

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn dry_run_status_lines_use_would_write_verb() {
      // Run provision --local --dry-run, capture stdout.
      // Assert every status line referencing a file starts with
      // "would write " (case-sensitive) and contains a project-
      // relative path (no tempdir path leakage).
  }

  #[test]
  fn dry_run_diff_covers_all_allowlist_paths() {
      // Allow-list per spec: wrangler.toml, fastly.toml, spin.toml,
      // runtime-config.toml, .edgezero/.env, .dev.vars,
      // <spin_crate>/.env. Build a fixture exercising the active
      // adapter, run --dry-run, assert the printed diff section
      // mentions each path the adapter would touch and does NOT
      // include any path outside the allow-list.
  }

  #[test]
  fn dry_run_diff_handles_manifest_in_subdir_of_adapter_crate() {
      // Fixture deliberately puts the manifest in a SUB-directory
      // of the adapter crate so the bug actually surfaces:
      //   [adapters.cloudflare.adapter]
      //   crate = "crates/cf"
      //   manifest = "crates/cf/config/wrangler.toml"
      // The OLD static-name allow-list would compute the diff
      // pair as `crates/cf/wrangler.toml` (adapter_crate_dir_rel
      // joined with literal "wrangler.toml") -- WRONG location,
      // diff would silently report no changes because both sides
      // of `crates/cf/wrangler.toml` would be absent.
      //
      // Run --dry-run with this fixture. Assert the diff section
      // includes BOTH `crates/cf/config/wrangler.toml` AND
      // `crates/cf/config/.dev.vars` (the sibling). Asserting
      // the absence of `crates/cf/wrangler.toml` from the diff
      // output also catches the static-name regression.
      //
      // A test using `crates/cf/wrangler.toml` would NOT catch
      // the bug because adapter_crate_dir_rel and the manifest
      // parent dir would coincidentally match.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli dry_run_status dry_run_diff_covers`
  Expected: FAIL

- [ ] **Step 3: Implement the dry-run report**

  Add to `crates/edgezero-cli/src/provision.rs`:

  ```rust
  /// Resolved per-adapter allow-list inputs the dry-run driver
  /// diffs. Built by the CLI from the resolved adapter manifest
  /// path (NOT a static filename) so nested paths like
  /// `crates/cf/wrangler.toml` resolve correctly. Spec §"Per-
  /// adapter local state" defines membership per adapter:
  /// - Axum: project-root `.edgezero/.env` only.
  /// - Cloudflare: resolved `wrangler.toml` + sibling `.dev.vars`.
  /// - Fastly: resolved `fastly.toml`.
  /// - Spin: resolved `spin.toml` + sibling `runtime-config.toml`
  ///   + sibling `.env`.
  /// `axum.toml` is NOT in this list (it stays tracked).
  pub(crate) struct DryRunAllowList {
      /// (project_path, staged_path) pairs the driver diffs.
      pub pairs: Vec<(PathBuf, PathBuf)>,
  }

  /// Build the allow-list from the resolved adapter manifest path.
  /// Caller-provided `adapter_manifest_abs` is the absolute path
  /// the adapter would write to (`manifest_root.join(
  /// adapter_manifest_path.unwrap_or(<adapter_default>))`); the
  /// helper computes its sibling paths (`.dev.vars`, `.env`,
  /// `runtime-config.toml`) and the corresponding staged-tempdir
  /// twins.
  ///
  /// **Case contract:** `Manifest::adapter_entry` at
  /// `crates/edgezero-core/src/manifest.rs:121` returns the
  /// CANONICAL (operator-spelling) key -- e.g. `Fastly`. The
  /// match arms are lowercase by convention. Callers MUST
  /// lowercase the adapter name before passing it in.
  pub(crate) fn build_dry_run_allow_list(
      project_root: &Path,
      staged_root: &Path,
      adapter_lower: &str,
      adapter_manifest_abs: &Path,
  ) -> DryRunAllowList {
      let project_manifest = adapter_manifest_abs
          .strip_prefix(staged_root)
          .map(|rel| project_root.join(rel))
          .unwrap_or_else(|_| adapter_manifest_abs.to_path_buf());
      let manifest_parent_staged = adapter_manifest_abs
          .parent()
          .unwrap_or(staged_root)
          .to_path_buf();
      let manifest_parent_project = project_manifest
          .parent()
          .unwrap_or(project_root)
          .to_path_buf();
      let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
      let mut add = |proj: PathBuf, staged: PathBuf| pairs.push((proj, staged));
      match adapter_lower {
          "axum" => {
              add(
                  project_root.join(".edgezero/.env"),
                  staged_root.join(".edgezero/.env"),
              );
          }
          "cloudflare" => {
              add(project_manifest.clone(), adapter_manifest_abs.to_path_buf());
              add(
                  manifest_parent_project.join(".dev.vars"),
                  manifest_parent_staged.join(".dev.vars"),
              );
          }
          "fastly" => {
              add(project_manifest.clone(), adapter_manifest_abs.to_path_buf());
          }
          "spin" => {
              add(project_manifest.clone(), adapter_manifest_abs.to_path_buf());
              add(
                  manifest_parent_project.join("runtime-config.toml"),
                  manifest_parent_staged.join("runtime-config.toml"),
              );
              add(
                  manifest_parent_project.join(".env"),
                  manifest_parent_staged.join(".env"),
              );
          }
          _ => {}
      }
      DryRunAllowList { pairs }
  }

  /// Caller passes the result of `build_dry_run_allow_list(...)`;
  /// the diff inputs come from `allow_list.pairs`. Status-line
  /// rewriting (`wrote X` → `would write X`) is adapter-agnostic
  /// and uses only the (project_root, staged_root) prefix swap
  /// plus a verb-prefix table -- no adapter name needed.
  pub(crate) fn render_dry_run_report(
      project_root: &Path,
      staged_root: &Path,
      allow_list: &DryRunAllowList,
      outcome: &adapter_registry::ProvisionOutcome,
  ) -> String {
      use similar::TextDiff;
      let mut out = String::new();

      // Status lines: rewrite tempdir paths back to project-relative
      // AND prefix the verb with "would ".
      for line in &outcome.status_lines {
          let rewritten = line.replace(
              staged_root.to_string_lossy().as_ref(),
              project_root.to_string_lossy().as_ref(),
          );
          let with_verb = rewritten
              .replacen("wrote ", "would write ", 1)
              .replacen("created ", "would create ", 1)
              .replacen("appended ", "would append ", 1);
          out.push_str(&with_verb);
          out.push('\n');
      }

      // Per-file diff section: caller-provided pairs already
      // resolved (project_path, staged_path) per adapter, with
      // resolved adapter manifest path threaded through.
      for (proj_path, staged_path) in &allow_list.pairs {
          if !staged_path.exists() {
              continue;
          }
          let new = std::fs::read_to_string(staged_path).unwrap_or_default();
          let old = std::fs::read_to_string(proj_path).unwrap_or_default();
          if old == new {
              continue;
          }
          let diff = TextDiff::from_lines(&old, &new);
          out.push_str(&format!("\n--- {}\n+++ {}\n", proj_path.display(), proj_path.display()));
          for change in diff.iter_all_changes() {
              let sign = match change.tag() {
                  similar::ChangeTag::Delete => "-",
                  similar::ChangeTag::Insert => "+",
                  similar::ChangeTag::Equal => " ",
              };
              out.push_str(&format!("{sign}{change}"));
          }
      }
      out
  }

  /// Per-adapter default manifest filename, used as the
  /// fallback when `[adapters.<name>.adapter].manifest` is unset.
  /// Mirrors each adapter crate's existing default (Cloudflare
  /// `cli.rs:198`, Fastly `cli.rs:214`, Spin `cli.rs:195`).
  /// Axum is not in the dry-run allow-list (axum.toml stays
  /// tracked).
  pub(crate) fn default_adapter_manifest_for(adapter_lower: &str) -> &'static str {
      match adapter_lower {
          "cloudflare" => "wrangler.toml",
          "fastly" => "fastly.toml",
          "spin" => "spin.toml",
          _ => "", // axum has no per-adapter manifest in the allow-list
      }
  }

  // Path resolution that used to live in `resolve_allow_list_pair`
  // is now in `build_dry_run_allow_list` above -- threading the
  // resolved adapter manifest path through avoids the
  // static-filename pitfall flagged by an earlier review. See
  // `dry_run_diff_handles_manifest_in_subdir_of_adapter_crate`
  // for the regression test: it uses
  // `manifest = "crates/cf/config/wrangler.toml"` (manifest in
  // a SUB-directory of the adapter crate dir) which the
  // static-name version would silently mis-resolve to
  // `crates/cf/wrangler.toml` and report no diff.
  ```

  In the `(true, true)` arm of the dispatch matrix (Task 11), after the adapter returns, the caller computes `adapter_manifest_abs` (the staged-root-joined adapter manifest path used by the adapter dispatch above), calls `build_dry_run_allow_list(...)` with it, then `render_dry_run_report(...)` and `println!` the result.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli dry_run_status dry_run_diff_covers`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Add dry-run allow-list diff + would-write status rewriting"
  ```

### Task 13: Add the dry-run worktree-clean + no-tempdir-leak test against the real fixture

**Files:**

- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: real app-demo fixture, `dry_run_allow_list`
- Produces: regression test asserting (a) the worktree is byte-identical after dry-run AND (b) the printed status leaks no tempdir paths to operators (spec §"Dry-run" → "status lines rewritten back to project paths")

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn provision_local_dry_run_worktree_clean_and_no_tempdir_paths_in_stdout() {
      // For each adapter ("cloudflare", "fastly", "spin", "axum"):
      //   - Run provision --local --dry-run against the in-tree
      //     examples/app-demo fixture (use --manifest
      //     examples/app-demo/edgezero.toml).
      //   - Snapshot every (file_path, mtime) under
      //     examples/app-demo/ before AND after; assert the
      //     snapshot is unchanged (no spurious worktree
      //     mutation -- the tempdir contained every write).
      //   - Capture stdout. The spec contract (§"Dry-run" -
      //     "status lines rewritten back to project paths")
      //     says operators MUST see project-relative paths
      //     only, NEVER raw tempdir paths. Assert:
      //       * `std::env::temp_dir().to_string_lossy()` does
      //         NOT appear anywhere in stdout (regex or
      //         substring check).
      //       * Each `would write` line names an
      //         `examples/app-demo/...` path (project-relative
      //         to manifest root), NOT a `/var/folders/...`
      //         path or any path under the OS temp root.
      //       * The diff blocks in stdout reference the same
      //         project-relative paths.
      //
      // If the rewriting in Task 12 ever regresses, the
      // tempdir-path absence assertion fails -- locks the
      // "no tempdir leak" contract.
  }
  ```

- [ ] **Step 2: Run test to verify it fails initially**

  Run: `cargo test -p edgezero-cli provision_local_dry_run_worktree_clean_and_no_tempdir_paths_in_stdout`
  Expected: FAIL until Section 5 lands per-adapter local writers (the stub adapter impls from Task 3 still return `Err`).

- [ ] **Step 3: Mark the test `#[ignore]` with a TODO comment for now**

  Annotate the test with `#[ignore = "re-enable after Section 5 lands per-adapter local provision"]`.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Add provision_local_dry_run worktree-clean + no-tempdir-leak test (ignored until Section 5)"
  ```

---

## Section 4 — `ManifestAdapterDeployed` schema + writeback

### Task 14: Add `ManifestAdapterDeployed` struct + remove `deployed` from reject list

**Files:**

- Modify: `crates/edgezero-core/src/manifest.rs` (new struct + field; remove `deployed` from unknown-subtable reject list)
- Test: `crates/edgezero-core/src/manifest.rs` test module

**Interfaces:**

- Consumes: nothing new
- Produces: `pub struct ManifestAdapterDeployed { kv_namespaces, preview_kv_namespaces, service_id }` and `pub deployed: Option<ManifestAdapterDeployed>` on `ManifestAdapter`

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn adapter_deployed_block_parses_and_validates_cloudflare() {
      let toml = r#"
      [app]
      name = "demo"

      [adapters.cloudflare]
      [adapters.cloudflare.adapter]
      crate = "crates/x"
      manifest = "crates/x/wrangler.toml"
      [adapters.cloudflare.deployed]
      kv_namespaces.sessions = "abc123"
      preview_kv_namespaces.sessions = "abc123_preview"
      "#;
      let m: Manifest = toml::from_str(toml).unwrap();
      m.validate().unwrap();
      let d = m.adapters["cloudflare"].deployed.as_ref().unwrap();
      assert_eq!(d.kv_namespaces["sessions"], "abc123");
      assert_eq!(d.preview_kv_namespaces["sessions"], "abc123_preview");
      assert!(d.service_id.is_none());
  }

  #[test]
  fn adapter_deployed_block_parses_and_validates_fastly() {
      let toml = r#"
      [app]
      name = "demo"

      [adapters.fastly]
      [adapters.fastly.adapter]
      crate = "crates/x"
      manifest = "crates/x/fastly.toml"
      [adapters.fastly.deployed]
      service_id = "SVC1"
      "#;
      let m: Manifest = toml::from_str(toml).unwrap();
      m.validate().unwrap();
      assert_eq!(
          m.adapters["fastly"].deployed.as_ref().unwrap().service_id.as_deref(),
          Some("SVC1")
      );
  }

  #[test]
  fn adapter_deployed_block_rejects_unknown_field() {
      let toml = r#"
      [app]
      name = "demo"

      [adapters.fastly]
      [adapters.fastly.adapter]
      crate = "x"
      manifest = "x/fastly.toml"
      [adapters.fastly.deployed]
      typo_field = "x"
      "#;
      let err = toml::from_str::<Manifest>(toml).unwrap_err();
      assert!(err.to_string().contains("unknown field"), "{err}");
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-core adapter_deployed_block`
  Expected: FAIL

- [ ] **Step 3: Add the struct + field**

  In `crates/edgezero-core/src/manifest.rs` (near `ManifestAdapter`):

  ```rust
  /// Deploy-time identifiers returned by cloud CLIs and persisted
  /// in `edgezero.toml` so teammates' `provision --local` can
  /// regenerate adapter manifests with real ids. See spec
  /// §"Where durable identifiers live".
  #[derive(Debug, Default, Deserialize, Validate)]
  #[serde(deny_unknown_fields)]
  pub struct ManifestAdapterDeployed {
      /// Primary namespace ids, keyed by logical
      /// `[stores.kv]` / `[stores.config]` id (Cloudflare only).
      #[serde(default)]
      pub kv_namespaces: BTreeMap<String, String>,
      /// Preview-namespace ids, keyed by the SAME logical id.
      /// Separate map so a legal store id like `sessions_preview`
      /// cannot collide with a sibling-suffix convention.
      #[serde(default)]
      pub preview_kv_namespaces: BTreeMap<String, String>,
      /// Fastly compute service id returned by `fastly compute deploy`.
      #[serde(default)]
      pub service_id: Option<String>,
  }
  ```

  Add a `pub deployed: Option<ManifestAdapterDeployed>` field on `ManifestAdapter`. **Remove `deployed` from the unknown-subtable reject list** at `manifest.rs:780` (locate the current "legacy / unknown" rejection bucket and drop `"deployed"` from whatever string set / match arm names rejected sub-tables).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-core adapter_deployed_block`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-core/src/manifest.rs
  git commit -m "Add ManifestAdapterDeployed; remove deployed from reject list"
  ```

### Task 15: Add `validate_manifest_deployed_adapter_match` Manifest-level validator

**Files:**

- Modify: `crates/edgezero-core/src/manifest.rs`
- Test: `crates/edgezero-core/src/manifest.rs` test module

**Interfaces:**

- Consumes: `ManifestAdapterDeployed` from Task 14
- Produces: Manifest-level schema validator rejecting cross-adapter misuse, case-insensitive

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn adapter_deployed_block_rejects_service_id_outside_fastly() {
      let toml = r#"
      [app]
      name = "demo"
      [adapters.cloudflare]
      [adapters.cloudflare.adapter]
      crate = "x"
      manifest = "x/wrangler.toml"
      [adapters.cloudflare.deployed]
      service_id = "SVC1"
      "#;
      let m: Manifest = toml::from_str(toml).unwrap();
      let err = m.validate().unwrap_err().to_string();
      assert!(err.contains("deployed_service_id_only_on_fastly"), "{err}");
  }

  #[test]
  fn adapter_deployed_block_rejects_kv_namespaces_outside_cloudflare() {
      let toml = r#"
      [app]
      name = "demo"
      [adapters.fastly]
      [adapters.fastly.adapter]
      crate = "x"
      manifest = "x/fastly.toml"
      [adapters.fastly.deployed]
      kv_namespaces.sessions = "abc"
      "#;
      let m: Manifest = toml::from_str(toml).unwrap();
      let err = m.validate().unwrap_err().to_string();
      assert!(err.contains("deployed_kv_namespaces_only_on_cloudflare"), "{err}");
  }

  #[test]
  fn adapter_deployed_block_validator_is_case_insensitive() {
      // `[adapters.Fastly.deployed].service_id` accepted.
      // `[adapters.FASTLY.deployed].service_id` accepted.
      // `[adapters.Cloudflare.deployed].service_id` rejected.
      for ok in ["Fastly", "FASTLY", "fastly"] {
          let t = format!(r#"
          [app]
          name = "demo"
          [adapters.{ok}]
          [adapters.{ok}.adapter]
          crate = "x"
          manifest = "x/f.toml"
          [adapters.{ok}.deployed]
          service_id = "SVC1"
          "#);
          let m: Manifest = toml::from_str(&t).unwrap();
          m.validate().unwrap_or_else(|e| panic!("{ok}: {e}"));
      }
      for bad in ["Cloudflare", "CLOUDFLARE"] {
          let t = format!(r#"
          [app]
          name = "demo"
          [adapters.{bad}]
          [adapters.{bad}.adapter]
          crate = "x"
          manifest = "x/c.toml"
          [adapters.{bad}.deployed]
          service_id = "SVC1"
          "#);
          let m: Manifest = toml::from_str(&t).unwrap();
          assert!(m.validate().is_err(), "{bad} should reject service_id");
      }
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-core adapter_deployed_block_rejects adapter_deployed_block_validator_is_case_insensitive`
  Expected: FAIL

- [ ] **Step 3: Add the Manifest-level validator**

  In `crates/edgezero-core/src/manifest.rs:86` (the existing top-level `#[validate(schema(...))]` slot that already hosts `validate_manifest_adapter_keys_case_unique`), add a second schema validator:

  ```rust
  #[derive(Debug, Deserialize, Validate)]
  #[validate(schema(function = "validate_manifest_adapter_keys_case_unique"))]
  #[validate(schema(function = "validate_manifest_deployed_adapter_match"))]
  pub struct Manifest { /* existing fields */ }
  ```

  Add the function at the bottom of the file's validator section:

  ```rust
  fn validate_manifest_deployed_adapter_match(
      manifest: &Manifest,
  ) -> Result<(), validator::ValidationError> {
      for (name, adapter) in &manifest.adapters {
          let Some(deployed) = adapter.deployed.as_ref() else { continue };
          if deployed.service_id.is_some()
              && !name.eq_ignore_ascii_case("fastly")
          {
              return Err(validator::ValidationError::new(
                  "deployed_service_id_only_on_fastly",
              ));
          }
          let cloudflare_only_map_set = !deployed.kv_namespaces.is_empty()
              || !deployed.preview_kv_namespaces.is_empty();
          if cloudflare_only_map_set
              && !name.eq_ignore_ascii_case("cloudflare")
          {
              return Err(validator::ValidationError::new(
                  "deployed_kv_namespaces_only_on_cloudflare",
              ));
          }
      }
      Ok(())
  }
  ```

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-core adapter_deployed_block`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-core/src/manifest.rs
  git commit -m "Add Manifest-level validate_manifest_deployed_adapter_match (case-insensitive)"
  ```

### Task 16: Add CLI writeback for `[adapters.<name>.deployed]` via `toml_edit::DocumentMut`

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs`
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `ProvisionOutcome.deployed`, `toml_edit::DocumentMut`
- Produces: `pub(crate) fn merge_deployed_into_manifest(manifest_path, adapter_name, state, dry_run)` writing through `toml_edit`, preserving sibling keys and adjacent comments

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn merge_deployed_round_trips_cloudflare_namespaces() {
      // Write a manifest fixture with [adapters.cloudflare] but no
      // [.deployed] block. Build an AdapterDeployedState with
      // sub_tables["kv_namespaces"]["sessions"] = "abc".
      // Call merge_deployed_into_manifest with dry_run = false.
      // Re-parse the manifest, assert
      // m.adapters["cloudflare"].deployed.kv_namespaces["sessions"] == "abc".
  }

  #[test]
  fn merge_deployed_preserves_adjacent_operator_comments() {
      // Manifest fixture has a `# operator note` on [adapters.spin].
      // Run merge_deployed_into_manifest for cloudflare.
      // Re-read raw text; assert the spin comment is still present.
  }

  #[test]
  fn merge_deployed_dry_run_does_not_mutate_file() {
      // Snapshot mtime; call with dry_run = true; assert unchanged.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli merge_deployed`
  Expected: FAIL

- [ ] **Step 3: Implement the writeback helper**

  In `crates/edgezero-cli/src/provision.rs`:

  ```rust
  pub(crate) fn merge_deployed_into_manifest(
      manifest_path: &Path,
      adapter_name: &str,
      state: &adapter_registry::AdapterDeployedState,
      dry_run: bool,
  ) -> Result<(), String> {
      use toml_edit::{table, value, DocumentMut};
      let raw = std::fs::read_to_string(manifest_path)
          .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
      let mut doc: DocumentMut = raw.parse()
          .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;

      let adapters = doc["adapters"].or_insert(table());
      let adapter_tbl = adapters[adapter_name].or_insert(table());
      let deployed = adapter_tbl["deployed"].or_insert(table());

      for (key, val) in &state.fields {
          deployed[key] = value(val.clone());
      }
      for (sub_name, sub_map) in &state.sub_tables {
          let sub_tbl = deployed[sub_name].or_insert(table());
          for (key, val) in sub_map {
              sub_tbl[key] = value(val.clone());
          }
      }

      if dry_run {
          return Ok(());
      }
      std::fs::write(manifest_path, doc.to_string())
          .map_err(|e| format!("write {}: {e}", manifest_path.display()))?;
      Ok(())
  }
  ```

- [ ] **Step 4: Call `merge_deployed_into_manifest` from `run_provision` using the CANONICAL adapter key**

  After the dispatch matrix lands the `outcome`, look up the canonical (case-preserving) adapter key via `manifest.adapter_entry(&args.adapter)` so a manifest declaring `[adapters.Fastly]` does not get a parallel `[adapters.fastly]` table created during writeback. `Manifest::adapter_entry` at `crates/edgezero-core/src/manifest.rs:132` returns `Option<(&String, &ManifestAdapter)>` — the first element is the canonical key as the operator typed it.

  ```rust
  if let Some(deployed) = outcome.deployed.as_ref() {
      let (canonical_adapter_key, _) = manifest
          .adapter_entry(&args.adapter)
          .ok_or_else(|| format!("adapter `{}` vanished from manifest", args.adapter))?;
      merge_deployed_into_manifest(
          &args.manifest,
          canonical_adapter_key,    // NOT &args.adapter -- preserves operator spelling
          deployed,
          args.dry_run, // cloud-mode dry_run short-circuits inside the helper
      )?;
  }
  ```

  Adjust the test from Step 1 (`merge_deployed_round_trips_cloudflare_namespaces`) so its fixture declares `[adapters.Cloudflare]` (mixed-case), invokes the merger with the canonical key, and asserts the round-tripped manifest still has `[adapters.Cloudflare.deployed]` (not a parallel `[adapters.cloudflare.deployed]` sibling).

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli merge_deployed`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Add toml_edit-based [adapters.<name>.deployed] writeback"
  ```

### Task 16b: Cloudflare cloud `provision` returns created namespace ids

**Files:**

- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs` (Cloud arm of `provision`; existing namespace-creation site at `:270`)
- Test: same file's test module

**Interfaces:**

- Consumes: `wrangler kv namespace create` stdout parsing (existing)
- Produces: Cloudflare's `ProvisionMode::Cloud` arm populates `ProvisionOutcome.deployed.sub_tables["kv_namespaces"]` with `<logical_id> → <namespace_id>` entries; Task 16's CLI writeback then lands them in `[adapters.cloudflare.deployed].kv_namespaces.*`.

Task 3 changed every adapter's `provision` to return `ProvisionOutcome { deployed: None }`. Without this task the `[adapters.cloudflare.deployed]` block stays empty forever, defeating the entire "teammates' `provision --local` picks up real namespace ids" promise in spec §"Where durable identifiers live".

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn cloudflare_cloud_provision_returns_created_namespace_ids() {
      // Fixture: stores.kv.ids = ["sessions"]. Install a fake
      // `wrangler` shim on PATH that prints the standard wrangler
      // create-namespace stdout containing id = "abc123".
      // Call provision with mode = Cloud, dry_run = false.
      // Assert outcome.deployed is Some, and:
      //   outcome.deployed.unwrap().sub_tables["kv_namespaces"]["sessions"] == "abc123"
  }

  #[test]
  fn cloudflare_cloud_provision_dry_run_returns_none_deployed() {
      // Mode = Cloud, dry_run = true; no wrangler invoked.
      // Assert outcome.deployed.is_none() (no shell-out happened,
      // so no real id to record).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_cloud_provision_returns cloudflare_cloud_provision_dry_run`
  Expected: FAIL

- [ ] **Step 3: Capture the ids in Cloudflare's Cloud arm — create with `store.platform`, persist under `store.logical`**

  The existing loop at `crates/edgezero-adapter-cloudflare/src/cli.rs:207` iterates `stores.kv.iter().chain(stores.config.iter())` and splits each store into `store.logical` (operator-typed id from `[stores.<kind>].ids`) and `store.platform` (env-overlay-resolved name actually written into `wrangler.toml`'s `binding =`). `wrangler kv namespace create` must take the **platform** name (so the binding the runtime resolves matches the namespace), but `[adapters.cloudflare.deployed].kv_namespaces.*` is keyed by the **logical** id (so teammates' env overlays remap correctly).

  Thread the split through `AdapterDeployedState`:

  ```rust
  let mut deployed = edgezero_adapter::AdapterDeployedState::default();
  let mut kv_ns = std::collections::BTreeMap::new();
  for store in stores.kv.iter().chain(stores.config.iter()) {
      // create_kv_namespace takes the PLATFORM name -- that's the
      // binding wrangler dev / Workers runtime will resolve.
      let ns_id = create_kv_namespace(&store.platform)?;
      // Persist under the LOGICAL id so [adapters.cloudflare.deployed]
      // stays env-overlay-independent. Teammates' provision --local
      // re-resolves logical -> platform via their own env overlay,
      // looking up ns_id via the same logical key.
      kv_ns.insert(store.logical.to_string(), ns_id);
  }
  if !kv_ns.is_empty() && !dry_run {
      deployed.sub_tables.insert("kv_namespaces".to_string(), kv_ns);
  }
  let deployed = if deployed.fields.is_empty() && deployed.sub_tables.is_empty() {
      None
  } else {
      Some(deployed)
  };
  Ok(edgezero_adapter::ProvisionOutcome { status_lines, deployed })
  ```

  The `&dry_run` guard skips populating `deployed` in dry-run mode (no real `wrangler` invocation happened; no real ids to record). Existing `create_kv_namespace` at `cli.rs:535` takes `(binding: &str)`; no signature change.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_cloud_provision`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: cloud provision returns created namespace ids via deployed"
  ```

**Fastly note:** Fastly's `service_id` writeback stays manual in v1 per spec §"Writeback ownership" (the `fastly compute deploy` shell command in `[adapters.fastly.commands].deploy` bypasses the adapter dispatch entirely; the spec documents the one-time copy step). No equivalent task for Fastly here.

**Spin note:** Spin's cloud `provision` does not create durable cloud identifiers in v1 (its `[stores.kv]` ids resolve via `runtime-config.toml` blocks, not cloud namespaces). No `deployed` payload to return.

### Task 16c: Add `edgezero_adapter::env_file::append_lines_dedup` (used by Section 5)

**Files:**

- Create: `crates/edgezero-adapter/src/env_file.rs`
- Modify: `crates/edgezero-adapter/src/lib.rs` (`pub mod env_file;`)
- Test: `crates/edgezero-adapter/src/env_file.rs` test module

**Interfaces:**

- Consumes: nothing
- Produces: `pub fn append_lines_dedup(path: &Path, new_lines: &[String], dry_run: bool) -> Result<(), String>` + the key-normalised dedup contract (commented `# KEY=...` lines dedup against uncommented `KEY=...` lines and vice versa, per spec §"Merge mechanics" → "Line-oriented")

This task lives BEFORE Section 5 because Cloudflare's `.dev.vars` writer (Task 19) and `provision_typed` (Task 20) both call the helper. Spin (Task 25, 26) and Axum (Task 27, 28) follow. Placing module creation here keeps the per-task compile contract: every adapter step Section 5 lands can import the helper from day one. The helper MUST live in `edgezero-adapter` (NOT `edgezero-cli`) so adapter crates can reach it without inverting the dep graph (Cloudflare/Fastly/Spin/Axum Cargo.toml have no `edgezero-cli` dep).

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;
      use std::fs;
      use tempfile::TempDir;

      #[test]
      fn appends_new_lines_and_skips_existing_keys() {
          let dir = TempDir::new().unwrap();
          let path = dir.path().join(".env");
          fs::write(&path, "AAA=existing\n").unwrap();
          append_lines_dedup(
              &path,
              &["AAA=NEW".to_string(), "BBB=NEW".to_string()],
              false,
          ).unwrap();
          let after = fs::read_to_string(&path).unwrap();
          // AAA stays at the operator value; BBB appended.
          assert!(after.contains("AAA=existing"));
          assert!(after.contains("BBB=NEW"));
          assert!(!after.contains("AAA=NEW"));
      }

      #[test]
      fn dedup_treats_commented_and_uncommented_form_as_same_key() {
          let dir = TempDir::new().unwrap();
          let path = dir.path().join(".env");
          // Operator already uncommented + edited the override line.
          fs::write(&path, "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=staging\n").unwrap();
          // Re-provision would otherwise re-add the commented form.
          append_lines_dedup(
              &path,
              &["# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config".to_string()],
              false,
          ).unwrap();
          let after = fs::read_to_string(&path).unwrap();
          let occurrences = after
              .lines()
              .filter(|l| normalised_key(l).as_deref()
                  == Some("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY"))
              .count();
          assert_eq!(occurrences, 1, "commented override must NOT reappear: {after}");
      }

      #[test]
      fn dry_run_makes_no_write() {
          let dir = TempDir::new().unwrap();
          let path = dir.path().join(".env");
          fs::write(&path, "KEEP=me\n").unwrap();
          let before = fs::metadata(&path).unwrap().modified().unwrap();
          append_lines_dedup(&path, &["NEW=x".to_string()], true).unwrap();
          let after = fs::metadata(&path).unwrap().modified().unwrap();
          assert_eq!(before, after);
      }
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter env_file`
  Expected: FAIL (module does not exist)

- [ ] **Step 3: Create the module**

  Write `crates/edgezero-adapter/src/env_file.rs`:

  ```rust
  //! Line-oriented env-file dedup shared by all adapters that
  //! write provision-owned `.env` / `.dev.vars` files. Key-
  //! normalised: a line whose key matches an existing commented
  //! OR uncommented entry is skipped. See spec §"Merge mechanics"
  //! → "Line-oriented".

  use std::fs;
  use std::path::Path;

  /// Append each `<key>=<value>` line iff its normalised key does
  /// NOT already appear in the file (commented OR uncommented).
  /// Existing lines are preserved byte-for-byte. Creates the file
  /// (and parent dirs) when absent.
  pub fn append_lines_dedup(
      path: &Path,
      new_lines: &[String],
      dry_run: bool,
  ) -> Result<(), String> {
      let mut existing = String::new();
      if path.exists() {
          existing = fs::read_to_string(path)
              .map_err(|e| format!("read {}: {e}", path.display()))?;
      }
      let existing_keys: std::collections::BTreeSet<String> = existing
          .lines()
          .filter_map(normalised_key)
          .collect();

      let mut to_append = String::new();
      for line in new_lines {
          let Some(k) = normalised_key(line) else { continue };
          if existing_keys.contains(&k) {
              continue;
          }
          to_append.push_str(line);
          if !line.ends_with('\n') {
              to_append.push('\n');
          }
      }
      if to_append.is_empty() || dry_run {
          return Ok(());
      }
      if let Some(parent) = path.parent() {
          fs::create_dir_all(parent)
              .map_err(|e| format!("create {}: {e}", parent.display()))?;
      }
      let mut combined = existing;
      if !combined.is_empty() && !combined.ends_with('\n') {
          combined.push('\n');
      }
      combined.push_str(&to_append);
      fs::write(path, combined)
          .map_err(|e| format!("write {}: {e}", path.display()))?;
      Ok(())
  }

  /// Strip at most ONE leading `#` + adjacent whitespace, then
  /// parse `<key>=<value>` and return the trimmed key. Returns
  /// `None` for blank lines and comment-only lines.
  ///
  /// Single-`#` semantics matter: `## KEY=value` (double hash --
  /// the markdown-style heading shape some operators use as
  /// section separators inside `.env` files) is NOT treated as
  /// a commented `KEY=value` line; it returns `None` and gets
  /// left alone. `trim_start_matches('#')` would strip every
  /// leading `#` and accidentally collapse the section-heading
  /// case into the commented-config case -- false dedup against
  /// an operator's section separators. `strip_prefix('#')` after
  /// `trim_start()` matches the docstring intent exactly.
  pub(crate) fn normalised_key(line: &str) -> Option<String> {
      let trimmed = line.trim_start();
      // Strip exactly ONE leading `#`, then any whitespace that
      // follows it -- e.g. `# KEY=value`, `#KEY=value`, and
      // `KEY=value` all normalise to the same key; `## KEY` does
      // NOT.
      let after_hash = trimmed.strip_prefix('#').unwrap_or(trimmed);
      let stripped = after_hash.trim_start();
      let (k, _) = stripped.split_once('=')?;
      let k = k.trim();
      if k.is_empty() {
          None
      } else {
          Some(k.to_string())
      }
  }
  ```

  Add a test asserting the single-hash semantic:

  ```rust
  #[test]
  fn normalised_key_strips_at_most_one_leading_hash() {
      // Uncommented and single-hash forms dedup against each other:
      assert_eq!(normalised_key("KEY=v"),     Some("KEY".into()));
      assert_eq!(normalised_key("#KEY=v"),    Some("KEY".into()));
      assert_eq!(normalised_key("# KEY=v"),   Some("KEY".into()));
      assert_eq!(normalised_key("  # KEY=v"), Some("KEY".into()));

      // Double-hash leaves the second `#` in the key, producing
      // a DIFFERENT normalised key ("# KEY") so dedup does NOT
      // collapse `## KEY=v` into `KEY=v`. Operator section
      // separators using `## …` stay intact. Crucially, the
      // returned key is `"# KEY"` and NOT `"KEY"` -- the dedup
      // map sees them as distinct entries.
      assert_eq!(normalised_key("## KEY=v"), Some("# KEY".into()));

      // Comment-only lines (no `=`) return None.
      assert_eq!(normalised_key("# comment"),  None);
      assert_eq!(normalised_key("### header"), None);
      assert_eq!(normalised_key(""),           None);
  }
  ```

  In `crates/edgezero-adapter/src/lib.rs` add `pub mod env_file;` alongside the existing module declarations.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter env_file`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter/src/env_file.rs crates/edgezero-adapter/src/lib.rs
  git commit -m "Add edgezero_adapter::env_file::append_lines_dedup (used by Section 5)"
  ```

---

## Section 5 — Per-adapter local writers

### Task 17: Cloudflare primitive synthesiser for `wrangler.toml`

**Files:**

- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Test: same file's test module

**Interfaces:**

- Consumes: `[app].name` from `edgezero.toml`
- Produces: `pub(crate) fn synthesise_wrangler_toml(app_name: &str) -> String`

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn synthesises_minimal_wrangler_toml_with_header() {
      let out = synthesise_wrangler_toml("demo");
      assert!(out.starts_with("# edgezero-provision: v1"));
      assert!(out.contains(r#"name = "demo""#));
      assert!(out.contains(r#"main = "build/worker/shim.mjs""#));
      assert!(out.contains("compatibility_date = "));
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter-cloudflare synthesises_minimal_wrangler_toml`
  Expected: FAIL

- [ ] **Step 3: Implement the synthesiser**

  ```rust
  /// Synthesised baseline `wrangler.toml` for clean clones. See
  /// spec §"Cloudflare (`wrangler.toml`)". Built via
  /// `toml_edit::DocumentMut` (NOT raw `format!`) so any legal
  /// `[app].name` -- including names with TOML-significant
  /// characters like `"`, `\`, or newlines -- is escaped
  /// correctly. Manifest validation today only length-bounds
  /// the name, so raw interpolation could produce invalid TOML
  /// for legal inputs.
  pub(crate) fn synthesise_wrangler_toml(app_name: &str) -> String {
      use toml_edit::{value, DocumentMut};
      let mut doc = DocumentMut::new();
      doc.decor_mut()
          .set_prefix("# edgezero-provision: v1\n");
      doc["name"] = value(app_name);
      doc["main"] = value("build/worker/shim.mjs");
      doc["compatibility_date"] = value("2024-01-01");
      doc.to_string()
  }
  ```

  Add a fuzz-style test covering pathological-but-legal `[app].name` values:

  ```rust
  #[test]
  fn synthesise_wrangler_toml_escapes_pathological_app_names() {
      for name in [
          r#"has"quote"#,
          r#"has\backslash"#,
          "has\nnewline",
          r#"has = equals"#,
      ] {
          let out = synthesise_wrangler_toml(name);
          // Re-parsing must succeed AND round-trip the name.
          let doc: toml_edit::DocumentMut = out.parse().unwrap();
          assert_eq!(doc["name"].as_str(), Some(name), "input: {name:?}");
      }
  }
  ```

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter-cloudflare synthesises_minimal_wrangler_toml`
  Expected: PASS

- [ ] **Step 5: Override `synthesise_baseline_manifest` to plug into Task 8b's CLI bootstrap**

  ```rust
  fn synthesise_baseline_manifest(
      &self,
      _manifest_root: &Path,
      adapter_manifest_path: Option<&str>,
      _component_selector: Option<&str>,
      app_name: &str,
      _deployed: Option<&AdapterDeployedState>,
  ) -> Result<Vec<(std::path::PathBuf, String)>, String> {
      let rel = adapter_manifest_path
          .map(std::path::PathBuf::from)
          .unwrap_or_else(|| std::path::PathBuf::from("wrangler.toml"));
      Ok(vec![(rel, synthesise_wrangler_toml(app_name))])
  }
  ```

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: primitive synthesiser for wrangler.toml + bootstrap override"
  ```

### Task 18: Cloudflare local-mode `provision` — emit `[[kv_namespaces]]` bindings with deployed precedence

**Files:**

- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Test: same file's test module

**Interfaces:**

- Consumes: synthesised baseline (Task 17), `ProvisionStores`, `Option<&AdapterDeployedState>` from the new trait parameter (Task 3 step 2). The adapter does NOT re-parse `edgezero.toml` -- the CLI's `deployed_state_for(...)` translator (Task 8b step 3) builds the neutral state and threads it through both `synthesise_baseline_manifest` and `provision`.
- Produces: Cloudflare's `ProvisionMode::Local` arm in `provision`

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn cloudflare_local_provision_emits_bindings_with_placeholders_when_no_deployed() {
      // Fixture: edgezero.toml declares [stores.kv].ids = ["sessions"].
      // No [adapters.cloudflare.deployed].
      // Call provision with mode = Local, dry_run = false.
      // Read wrangler.toml, assert it contains:
      //   [[kv_namespaces]]
      //   binding = "sessions"
      //   id = "<placeholder-namespace-id-sessions>"
  }

  #[test]
  fn cloudflare_local_provision_uses_deployed_namespace_id_when_set() {
      // Fixture: edgezero.toml has
      //   [adapters.cloudflare.deployed]
      //   kv_namespaces.sessions = "abc123"
      // Run provision. Assert wrangler.toml contains id = "abc123".
  }

  #[test]
  fn cloudflare_local_provision_preserves_sibling_operator_keys() {
      // Fixture wrangler.toml has an existing entry:
      //   [[kv_namespaces]]
      //   binding = "sessions"
      //   id = "operator-set"
      //   usage_model = "bundled"     # operator-added
      // Run provision; deployed.kv_namespaces.sessions = "from-cloud".
      // Re-read wrangler.toml; assert:
      //   id == "from-cloud" (deployed wins)
      //   usage_model == "bundled" (sibling preserved)
  }

  #[test]
  fn cloudflare_local_provision_falls_back_to_existing_local_id_when_no_deployed() {
      // Fixture wrangler.toml has id = "operator-set".
      // No deployed block. Assert id stays "operator-set".
  }

  #[test]
  fn cloudflare_local_provision_resolves_nested_adapter_manifest_path() {
      // Fixture: adapter_manifest_path = "crates/cf/wrangler.toml"
      // (app-demo style). PRE-SEED crates/cf/wrangler.toml with
      // the synthesised baseline (call synthesise_wrangler_toml
      // and write the result to that path) so the test exercises
      // what local provision does AFTER Task 8b's CLI bootstrap.
      // The adapter's local arm assumes the file is present; the
      // test must mirror that contract. Run local provision.
      // Assert binding upsert lands inside crates/cf/wrangler.toml
      // (NOT a sibling at the manifest_root level).
  }

  #[test]
  fn cloudflare_local_provision_errors_if_manifest_absent() {
      // Same fixture but DO NOT pre-seed wrangler.toml. Run local
      // provision. Assert error mentions the missing path and
      // synthesis was NOT attempted (the adapter trait does not
      // receive app_name, so synthesis cannot happen here -- it's
      // Task 8b's job).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_local_provision`
  Expected: FAIL

- [ ] **Step 3: Implement Cloudflare's local arm**

  Replace the `ProvisionMode::Local => return Err(...)` stub in `provision` with a block that uses the logical/platform split (spec §"Logical ID vs platform-resolved name (v1 contract)"; matches the cloud arm at Task 16b):

  1. Resolve `wrangler_path = manifest_root.join(adapter_manifest_path.unwrap_or("wrangler.toml"))` (mirrors today's resolution at `crates/edgezero-adapter-cloudflare/src/cli.rs:198`). The app-demo fixture points Cloudflare at `crates/app-demo-adapter-cloudflare/wrangler.toml`, so the root-level fallback is rarely the operative path. **Assume `wrangler_path` already exists** -- Task 8b's CLI bootstrap calls `synthesise_baseline_manifest` and writes the baseline BEFORE `provision` runs. The adapter trait does not receive `app_name`, so synthesis CANNOT happen here; treating the file as already-present keeps the trait surface narrow. If the file is unexpectedly absent (operator deleted it after bootstrap, or a programmer error somewhere), return an error pointing at the missing path -- do NOT silently re-synthesise.
  2. Parse the file via `toml_edit::DocumentMut`.
  3. For each `store` in `stores.kv.iter().chain(stores.config.iter())` (matches `crates/edgezero-adapter-cloudflare/src/cli.rs:207`):
     - **Lookups use `store.logical`** (env-overlay-independent stable key):
       - Deployed namespace id from `[adapters.cloudflare.deployed].kv_namespaces.<store.logical>`.
       - Deployed preview id from `[adapters.cloudflare.deployed].preview_kv_namespaces.<store.logical>`.
       - Placeholder suffix: `format!("<placeholder-namespace-id-{}>", store.logical)`.
     - **TOML cells use `store.platform`** (env-overlay-resolved name the Workers runtime calls `env.kv(...)` with):
       - Match existing `[[kv_namespaces]]` array-of-tables entries by `binding == store.platform`.
       - Written `binding = "..."` value is `store.platform`.
     - `<resolved_id>` precedence: deployed lookup (by `store.logical`) → existing local `id` on the matched binding (matched by `store.platform`) → placeholder (suffix from `store.logical`).
     - `preview_id`: deployed lookup (by `store.logical`); otherwise OMIT entirely (do NOT synthesise a placeholder; matches existing code at `cli.rs:821`).
     - If a matching entry exists, upsert ONLY `id` and (when present) `preview_id`. If not found, append a new entry with `binding`, `id`, and (when present) `preview_id`.
  4. Write the file back (skip the write when `dry_run`; status line still emitted).
  5. Return `Ok(ProvisionOutcome { status_lines: vec![...], deployed: None })`.

  The deployed-state lookup uses the neutral `Option<&AdapterDeployedState>` parameter the trait now receives (Task 3 step 2). Within Cloudflare's local arm: `deployed.and_then(|d| d.sub_tables.get("kv_namespaces")).and_then(|kv| kv.get(store.logical))` for `id`, and the symmetrical `"preview_kv_namespaces"` lookup for `preview_id`. The adapter does NOT re-parse `edgezero.toml` -- the CLI's Task 8b boundary translator (`deployed_state_for(manifest, canonical_adapter_name)`) reads `[adapters.cloudflare.deployed]` from the already-loaded `&Manifest` and hands the adapter the neutral state. This preserves the `edgezero-adapter → edgezero-core` dep-free invariant.

  **Required env-overlay round-trip test** (in addition to the four tests above): set `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config` in the test's process env, fixture has `[stores.config].ids = ["app_config"]` and `[adapters.cloudflare.deployed].kv_namespaces.app_config = "abc123"`. Run local provision. Assert `wrangler.toml` contains `binding = "prod_config"` (platform-resolved name) AND `id = "abc123"` (deployed value looked up by logical id). A bug that collapses the split would either write `binding = "app_config"` or fail to find the deployed id.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_local_provision`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: local-mode provision emits [[kv_namespaces]] bindings (deployed precedence)"
  ```

### Task 19: Cloudflare `.dev.vars` emission

**Files:**

- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Test: same file

**Interfaces:**

- Consumes: `ProvisionStores`, env-overlay-resolved platform names
- Produces: `.dev.vars` lines for KV/CONFIG/SECRETS `__NAME` + CONFIG `__KEY` (commented out by default)

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn cloudflare_local_provision_writes_dev_vars_name_lines() {
      // Fixture: stores.config.ids = ["app_config"], stores.kv.ids = ["sessions"].
      // Run provision local. Read .dev.vars:
      //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME="app_config"
      //   EDGEZERO__STORES__KV__SESSIONS__NAME="sessions"
      //   # EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="app_config_staging"
  }

  #[test]
  fn cloudflare_local_provision_dev_vars_dedup_respects_commented_overrides() {
      // Pre-populate .dev.vars with the operator's uncommented:
      //   EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="real_staging"
      // Re-run provision. Assert the commented placeholder is NOT
      // re-added (key-normalised dedup).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_local_provision_writes_dev_vars`
  Expected: FAIL

- [ ] **Step 3: Implement `.dev.vars` line-wise merge**

  Add a helper that:
  - Parses existing `.dev.vars` (if present) into a `BTreeMap<String, String>` keyed by the **normalised key** (strip leading `#` + whitespace, parse `<key>=<value>`).
  - For each store id and `__NAME`/`__KEY` line provision would emit, skip if the normalised key already exists in the parsed map.
  - Append missing lines; never rewrite existing lines.
  - Values use platform-resolved names (matches binding in `wrangler.toml`).

  Append the lines to `.dev.vars` next to `wrangler.toml` (same dir).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_local_provision_writes_dev_vars`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: local-mode provision writes .dev.vars __NAME / __KEY lines"
  ```

### Task 20: Cloudflare `provision_typed` — `.dev.vars` secret placeholders

**Files:**

- Modify: `crates/edgezero-adapter-cloudflare/src/cli.rs`
- Test: same file

**Interfaces:**

- Consumes: `&[TypedSecretEntry<'_>]`
- Produces: Cloudflare's `provision_typed` impl: appends `<key_value>=""` lines to `.dev.vars`

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn cloudflare_provision_typed_appends_secret_placeholders_to_dev_vars() {
      let entries = [TypedSecretEntry::new("default", "api_token", "demo_api_token")];
      // Fixture: manifest_root = tempdir, adapter_manifest_path =
      // "crates/cf/wrangler.toml". Run provision_typed with
      // mode = Local, dry_run = false. Assert:
      //   tempdir/crates/cf/.dev.vars exists AND contains
      //   the line `demo_api_token=""`.
      // Asserts NOT just "the file exists somewhere" but the
      // exact path next to wrangler.toml -- a regression test
      // for the manifest_root-vs-wrangler-parent confusion.
  }

  #[test]
  fn cloudflare_provision_typed_dev_vars_lands_next_to_wrangler_toml() {
      // Same shape, but explicitly verify the path:
      // adapter_manifest_path = "crates/cf/wrangler.toml" =>
      // .dev.vars at tempdir/crates/cf/.dev.vars (NOT
      // tempdir/.dev.vars).
      // Locks the wrangler.parent() resolution against drift.
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_provision_typed`
  Expected: FAIL

- [ ] **Step 3: Implement `provision_typed`**

  Override the trait method:

  ```rust
  fn provision_typed<'entry>(
      &self,
      manifest_root: &Path,
      _adapter_manifest_path: Option<&str>,
      _component_selector: Option<&str>,
      typed_secrets: &[TypedSecretEntry<'entry>],
      mode: ProvisionMode,
      dry_run: bool,
  ) -> Result<ProvisionOutcome, String> {
      if !matches!(mode, ProvisionMode::Local) {
          return Ok(ProvisionOutcome::default());
      }
      // `.dev.vars` lives next to `wrangler.toml` (Wrangler reads
      // it via the same resolution it uses for the manifest). The
      // app-demo fixture points Cloudflare at
      // `crates/app-demo-adapter-cloudflare/wrangler.toml` (see
      // examples/app-demo/edgezero.toml:166), so `.dev.vars` MUST
      // land in `crates/app-demo-adapter-cloudflare/.dev.vars`,
      // not at `manifest_root/.dev.vars`. Task 19 already anchors
      // its writes the same way; this is the typed-secret mirror.
      let wrangler_path = manifest_root.join(
          adapter_manifest_path.unwrap_or("wrangler.toml"),
      );
      let dev_vars = wrangler_path
          .parent()
          .unwrap_or(manifest_root)
          .join(".dev.vars");
      let mut status = Vec::new();
      let lines = typed_secrets.iter()
          .map(|e| format!("{}=\"\"", e.key_value))
          .collect::<Vec<_>>();
      edgezero_adapter::env_file::append_lines_dedup(&dev_vars, &lines, dry_run)?;
      status.push(format!(
          "wrote {} secret placeholders ({} entries)",
          dev_vars.display(),
          typed_secrets.len()
      ));
      Ok(ProvisionOutcome { status_lines: status, deployed: None })
  }
  ```

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter-cloudflare cloudflare_provision_typed`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: provision_typed appends secret placeholders to .dev.vars"
  ```

### Task 21: Fastly primitive synthesiser for `fastly.toml`

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn synthesises_minimal_fastly_toml_with_header_and_no_service_id() {
      let out = synthesise_fastly_toml("demo", None);
      assert!(out.starts_with("# edgezero-provision: v1"));
      assert!(out.contains("manifest_version = 3"));
      assert!(out.contains(r#"name = "demo""#));
      assert!(out.contains(r#"language = "rust""#));
      assert!(out.contains("[scripts]"));
      assert!(out.contains("[local_server]"));
      assert!(!out.contains("service_id"));
  }

  #[test]
  fn synthesises_fastly_toml_pins_service_id_when_deployed_present() {
      let out = synthesise_fastly_toml("demo", Some("SVC1"));
      assert!(out.contains(r#"service_id = "SVC1""#));
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-fastly synthesises_minimal_fastly synthesises_fastly_toml_pins_service_id`
  Expected: FAIL

- [ ] **Step 3: Implement**

  ```rust
  pub(crate) fn synthesise_fastly_toml(app_name: &str, service_id: Option<&str>) -> String {
      use toml_edit::{table, value, DocumentMut};
      let mut doc = DocumentMut::new();
      doc.decor_mut()
          .set_prefix("# edgezero-provision: v1\n");
      doc["manifest_version"] = value(3);
      doc["name"] = value(app_name);
      doc["language"] = value("rust");
      if let Some(svc) = service_id {
          doc["service_id"] = value(svc);
      }
      let scripts = doc["scripts"].or_insert(table());
      scripts["build"] = value("cargo build --profile release --target wasm32-wasip1");
      doc["local_server"].or_insert(table());
      doc.to_string()
  }
  ```

  Add the same pathological-name fuzz test as Cloudflare (`synthesise_fastly_toml_escapes_pathological_app_names`) plus a `synthesise_fastly_toml_escapes_pathological_service_id` covering service ids containing quotes / backslashes (`fastly compute deploy` may return arbitrary strings).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-fastly synthesises_minimal_fastly synthesises_fastly_toml_pins_service_id`
  Expected: PASS

- [ ] **Step 5: Override `synthesise_baseline_manifest`**

  ```rust
  fn synthesise_baseline_manifest(
      &self,
      _manifest_root: &Path,
      adapter_manifest_path: Option<&str>,
      _component_selector: Option<&str>,
      app_name: &str,
      deployed: Option<&AdapterDeployedState>,
  ) -> Result<Vec<(std::path::PathBuf, String)>, String> {
      // The CLI translated [adapters.fastly.deployed].service_id
      // into deployed.fields["service_id"] before calling -- no
      // edgezero-core import needed here. See Task 8b step 3 for
      // the boundary translator.
      let deployed_service_id = deployed
          .and_then(|d| d.fields.get("service_id"))
          .map(String::as_str);
      let rel = adapter_manifest_path
          .map(std::path::PathBuf::from)
          .unwrap_or_else(|| std::path::PathBuf::from("fastly.toml"));
      Ok(vec![(rel, synthesise_fastly_toml(app_name, deployed_service_id))])
  }
  ```

  No edgezero-core dependency in the adapter crate — `AdapterDeployedState` is the neutral type from Task 2. The synthesiser falls back to omitting `service_id` per spec when the deployed block is absent (no `"service_id"` key in `deployed.fields`).

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter-fastly/src/cli.rs
  git commit -m "Fastly: primitive synthesiser for fastly.toml + bootstrap override"
  ```

### Task 22: Fastly local-mode `provision` — `[local_server.*]` + `edgezero_runtime_env`

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Test: same file

**Interfaces:**

- Consumes: synthesised baseline, `ProvisionStores`, deployed `service_id`
- Produces: Fastly local arm — synthesise/merge `fastly.toml` with kv_stores + config_stores + edgezero_runtime_env

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn fastly_local_provision_writes_kv_and_config_store_blocks() {
      // Fixture: stores.kv.ids = ["sessions"], stores.config.ids = ["app_config"].
      // Run local provision. Read fastly.toml, assert:
      //   [[local_server.kv_stores.sessions]]
      //   key = "__init__"
      //   data = ""
      //   [local_server.config_stores.app_config]
      //   format = "inline-toml"
      //   [local_server.config_stores.app_config.contents]
      //   # empty table -- NOT `contents = ""`
      //
      // `contents` MUST be a TOML table, not a string. The
      // existing Fastly push writer at
      // crates/edgezero-adapter-fastly/src/cli.rs:986 calls
      // `contents_entry.as_table_mut()` and refuses to edit in
      // place when the value isn't a table; provision writing
      // a string here would brick subsequent `config push --local`.
  }

  #[test]
  fn fastly_local_provision_writes_edgezero_runtime_env() {
      // Same fixture. Assert fastly.toml contains:
      //   [local_server.config_stores.edgezero_runtime_env]
      //   format = "inline-toml"
      //   [local_server.config_stores.edgezero_runtime_env.contents]
      //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME = "app_config"
      //   EDGEZERO__STORES__KV__SESSIONS__NAME = "sessions"
      //   # EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY = "app_config_staging"
  }

  #[test]
  fn fastly_local_provision_errors_if_manifest_absent() {
      // DO NOT pre-seed fastly.toml. Run local provision.
      // Assert error mentions the missing path (synthesis is
      // Task 8b's CLI bootstrap concern; the adapter trait does
      // not receive app_name so it cannot synthesise here).
  }

  #[test]
  fn fastly_local_provision_upserts_deployed_service_id_into_existing_manifest() {
      // Pre-seed fastly.toml WITHOUT a service_id key (operator
      // deleted it, or it was synthesised before a deploy
      // happened). Pass deployed.fields["service_id"] = "SVC1"
      // via the new `deployed` parameter. Run local provision.
      // Re-parse fastly.toml; assert top-level
      // `service_id = "SVC1"` is now present. Locks the spec's
      // "synthesising OR merging" rule -- the prior plan rev
      // only handled service_id during synthesis (Task 8b), so
      // operators who pre-seeded fastly.toml from a stale
      // template would never get the deployed value pinned.
  }

  #[test]
  fn fastly_local_provision_leaves_operator_service_id_alone_when_deployed_absent() {
      // Pre-seed fastly.toml with `service_id = "operator-set"`.
      // Pass `deployed = None` (no [adapters.fastly.deployed]).
      // Run local provision. Assert service_id is still
      // "operator-set" -- when there's no cloud authority, the
      // operator's local value wins.
  }

  #[test]
  fn fastly_local_provision_resolves_nested_adapter_manifest_path() {
      // Fixture: adapter_manifest_path = "crates/fastly/fastly.toml".
      // PRE-SEED crates/fastly/fastly.toml via synthesise_fastly_toml
      // (mirrors what Task 8b's bootstrap would write). Run local
      // provision. Assert merges land inside crates/fastly/fastly.toml
      // (NOT a sibling at the manifest_root level).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-fastly fastly_local_provision`
  Expected: FAIL

- [ ] **Step 3: Implement Fastly's local arm**

  In `crates/edgezero-adapter-fastly/src/cli.rs`'s `provision`:

  1. Resolve `fastly_path = manifest_root.join(adapter_manifest_path.unwrap_or("fastly.toml"))` (mirrors today's resolution at `crates/edgezero-adapter-fastly/src/cli.rs:214`). **Assume `fastly_path` already exists** -- Task 8b's CLI bootstrap writes the baseline (including the pinned `service_id` from `deployed.fields["service_id"]`) BEFORE `provision` runs, and the trait does not receive `app_name`, so the adapter cannot synthesise here. If the file is unexpectedly absent, return an error pointing at the missing path -- do NOT silently re-synthesise.
  2. Parse `fastly_path` via `toml_edit::DocumentMut`.
  3. **Upsert top-level `service_id` from `deployed.fields["service_id"]`** when present. Spec §"Fastly" → "Where durable identifiers live" says local provision reads `[adapters.fastly.deployed].service_id` when "synthesising OR MERGING `fastly.toml`" -- not just on first synthesis. If `deployed.fields.get("service_id")` is `Some(svc)`, set `doc["service_id"] = value(svc.as_str())` (overwrite stale local value with the cloud-authoritative one); if `None`, leave any existing operator-set value alone. Operator workflow: first cloud `deploy` creates the service id; operator commits it under `[adapters.fastly.deployed].service_id` in `edgezero.toml`; teammates' next `provision --local` pins it inside their gitignored `fastly.toml`.
  4. For each `stores.kv.ids` entry: append `[[local_server.kv_stores.<platform>]]` array entry with `key = "__init__"`, `data = ""` IFF absent.
  5. For each `stores.config.ids` entry: append `[local_server.config_stores.<platform>]` normal table with `format = "inline-toml"` AND an empty `[local_server.config_stores.<platform>.contents]` SUB-TABLE (NOT `contents = ""` — must be a TOML table per the existing Fastly push writer at `crates/edgezero-adapter-fastly/src/cli.rs:986`, which calls `contents_entry.as_table_mut()` and refuses to edit in place when the value isn't a table). IFF absent.
  6. Append `[local_server.config_stores.edgezero_runtime_env]` block IFF absent, with `contents` containing one `EDGEZERO__STORES__<KIND>__<LOGICAL_ID>__NAME = "<platform>"` per id (KV/CONFIG/SECRETS) and commented-out `# EDGEZERO__STORES__CONFIG__<LOGICAL_ID>__KEY = "<logical_id>_staging"` examples for CONFIG ids only.
  7. Write back (skip on `dry_run`).
  8. Status lines describe what was added.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-fastly fastly_local_provision`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-fastly/src/cli.rs
  git commit -m "Fastly: local-mode provision writes [local_server.*] + edgezero_runtime_env"
  ```

### Task 23: Fastly `provision_typed` — secret-store array entries

**Files:**

- Modify: `crates/edgezero-adapter-fastly/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn fastly_provision_typed_writes_secret_store_entries_under_resolved_store_id() {
      let entries = [
          TypedSecretEntry::new("default", "api_token", "demo_api_token"),
          TypedSecretEntry::new("vendor_secrets", "vendor_key", "vendor_demo_key"),
      ];
      // Run provision_typed with mode = Local. Read fastly.toml.
      // Assert it contains:
      //   [[local_server.secret_stores.default]]
      //   key = "demo_api_token"
      //   env = "DEMO_API_TOKEN"
      //   [[local_server.secret_stores.vendor_secrets]]
      //   key = "vendor_demo_key"
      //   env = "VENDOR_DEMO_KEY"
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter-fastly fastly_provision_typed`
  Expected: FAIL

- [ ] **Step 3: Implement**

  Override `provision_typed`: parse `fastly.toml` via `toml_edit::DocumentMut`, for each `TypedSecretEntry`, locate the `[[local_server.secret_stores.<store_id>]]` array-of-tables (creating if absent), and append an entry with `key = "<key_value>"` and `env = "<KEY_VALUE_UPPER>"` IFF an entry with the same `key` is not already present. Write back unless `dry_run`.

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter-fastly fastly_provision_typed`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-fastly/src/cli.rs
  git commit -m "Fastly: provision_typed writes [[local_server.secret_stores.*]] entries"
  ```

### Task 24: Spin primitive synthesiser — `spin.toml` (component-id resolution) + `runtime-config.toml`

**Files:**

- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn synthesises_spin_toml_uses_app_name_when_component_unset() {
      let out = synthesise_spin_toml("demo", None);
      assert!(out.starts_with("# edgezero-provision: v1"));
      assert!(out.contains("spin_manifest_version = 2"));
      assert!(out.contains(r#"name = "demo""#));
      assert!(out.contains(r#"component = "demo""#));
      assert!(out.contains("[component.demo]"));
  }

  #[test]
  fn synthesises_spin_toml_honors_component_selector() {
      let out = synthesise_spin_toml("demo", Some("worker"));
      assert!(out.contains(r#"component = "worker""#));
      assert!(out.contains("[component.worker]"));
      // wasm path matches the component id, not the app name:
      assert!(out.contains("/release/worker.wasm"));
  }

  #[test]
  fn synthesises_runtime_config_toml_is_header_only() {
      let out = synthesise_runtime_config_toml();
      assert_eq!(out, "# edgezero-provision: v1\n");
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-spin synthesises_spin_toml synthesises_runtime_config_toml`
  Expected: FAIL

- [ ] **Step 3: Implement**

  ```rust
  pub(crate) fn synthesise_spin_toml(app_name: &str, component: Option<&str>) -> String {
      use toml_edit::{array, table, value, Array, ArrayOfTables, DocumentMut, Table};
      let component_id = component.unwrap_or(app_name);
      let component_under = component_id.replace('-', "_");

      let mut doc = DocumentMut::new();
      doc.decor_mut()
          .set_prefix("# edgezero-provision: v1\n");
      doc["spin_manifest_version"] = value(2);

      let application = doc["application"].or_insert(table());
      application["name"] = value(app_name);
      application["version"] = value("0.1.0");

      // [[trigger.http]] is an array-of-tables; build via
      // ArrayOfTables so toml_edit emits the `[[...]]` syntax
      // and so the component reference is escaped correctly.
      let mut http_trigger = Table::new();
      http_trigger["route"] = value("/...");
      http_trigger["component"] = value(component_id);
      let trigger = doc["trigger"].or_insert(table());
      let trigger_table = trigger.as_table_mut().expect("trigger must be a table");
      let mut http_aot = ArrayOfTables::new();
      http_aot.push(http_trigger);
      trigger_table.insert("http", toml_edit::Item::ArrayOfTables(http_aot));

      // `[component.<id>]` is a sub-table; component_id may
      // contain TOML-significant chars in pathological cases,
      // so insert via the typed Table API rather than splicing
      // the id into a section header.
      let component_section = doc["component"].or_insert(table());
      let component_table = component_section
          .as_table_mut()
          .expect("component must be a table");
      let mut comp = Table::new();
      comp["source"] = value(format!(
          "../../target/wasm32-wasip2/release/{component_under}.wasm"
      ));
      comp["key_value_stores"] = value(Array::new());
      component_table.insert(component_id, toml_edit::Item::Table(comp));

      doc.to_string()
  }

  pub(crate) fn synthesise_runtime_config_toml() -> String {
      // No body keys yet -- the adapter's local arm appends
      // `[key_value_store.<name>]` blocks via toml_edit on top
      // of this header-only baseline.
      String::from("# edgezero-provision: v1\n")
  }
  ```

  Add Spin pathological-name tests: `synthesise_spin_toml_escapes_pathological_app_names` AND `synthesise_spin_toml_escapes_pathological_component_id` (the component id flows into BOTH the `component = "..."` value AND the `[component.<id>]` key — both must round-trip cleanly).

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-spin synthesises_spin_toml synthesises_runtime_config_toml`
  Expected: PASS

- [ ] **Step 5: Override `synthesise_baseline_manifest` for BOTH files**

  Spin owns two synthesised files (`spin.toml` + `runtime-config.toml`), so return two tuples:

  ```rust
  fn synthesise_baseline_manifest(
      &self,
      _manifest_root: &Path,
      adapter_manifest_path: Option<&str>,
      component_selector: Option<&str>,
      app_name: &str,
      _deployed: Option<&AdapterDeployedState>,
  ) -> Result<Vec<(std::path::PathBuf, String)>, String> {
      let spin_rel = adapter_manifest_path
          .map(std::path::PathBuf::from)
          .unwrap_or_else(|| std::path::PathBuf::from("spin.toml"));
      // runtime-config.toml lives next to spin.toml.
      let rc_rel = spin_rel
          .parent()
          .map(|p| p.join("runtime-config.toml"))
          .unwrap_or_else(|| std::path::PathBuf::from("runtime-config.toml"));
      Ok(vec![
          (spin_rel, synthesise_spin_toml(app_name, component_selector)),
          (rc_rel,   synthesise_runtime_config_toml()),
      ])
  }
  ```

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-adapter-spin/src/cli.rs
  git commit -m "Spin: primitive synthesiser for spin.toml + runtime-config.toml + bootstrap override"
  ```

### Task 25: Spin local-mode `provision` — bindings + `runtime-config.toml` blocks + `.env` `__NAME` lines

**Files:**

- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn spin_local_provision_writes_kv_bindings_and_runtime_config_blocks() {
      // Fixture: stores.config.ids = ["app_config"], stores.kv.ids = ["sessions"].
      // Run local provision. Assert:
      //   spin.toml has component.<id>.key_value_stores = ["app_config", "sessions"]
      //   runtime-config.toml has
      //     [key_value_store.app_config]
      //     type = "spin"
      //     path = ".spin/sqlite_key_value.db"
      //     [key_value_store.sessions]
      //     type = "spin"
      //     path = ".spin/sqlite_key_value.db"
  }

  #[test]
  fn spin_local_provision_writes_env_name_lines_for_kv_config_secrets() {
      // Fixture: stores.config.ids = ["app_config"], stores.kv.ids = ["sessions"],
      // stores.secrets.ids = ["default"].
      // Run local provision. Read .env next to spin.toml, assert it contains:
      //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config
      //   EDGEZERO__STORES__KV__SESSIONS__NAME=sessions
      //   EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default
      //   # EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging
  }

  #[test]
  fn spin_local_provision_errors_if_spin_toml_absent() {
      // DO NOT pre-seed spin.toml. Run local provision.
      // Assert error mentions the missing path -- synthesis is
      // Task 8b's CLI bootstrap concern; the adapter trait does
      // not receive app_name or the component-id fallback so it
      // cannot synthesise here.
  }

  #[test]
  fn spin_local_provision_errors_if_runtime_config_toml_absent() {
      // Pre-seed spin.toml but NOT runtime-config.toml.
      // Assert error mentions runtime-config.toml is missing.
      // Both files are part of Spin's synthesise_baseline_manifest
      // output (Task 24); a present spin.toml without
      // runtime-config.toml is a programmer error worth flagging
      // explicitly.
  }

  #[test]
  fn spin_local_provision_resolves_nested_adapter_manifest_path() {
      // Fixture: adapter_manifest_path = "crates/spin/spin.toml".
      // PRE-SEED both crates/spin/spin.toml (via synthesise_spin_toml)
      // and crates/spin/runtime-config.toml (via
      // synthesise_runtime_config_toml) -- mirrors what Task 8b's
      // bootstrap would write. Run local provision. Assert merges
      // land inside crates/spin/spin.toml AND
      // crates/spin/runtime-config.toml (runtime-config sits
      // next to spin.toml, NOT at the manifest_root level).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-spin spin_local_provision`
  Expected: FAIL

- [ ] **Step 3: Implement Spin's local arm**

  1. Resolve `spin_path = manifest_root.join(adapter_manifest_path.unwrap_or("spin.toml"))` (mirrors today's resolution at `crates/edgezero-adapter-spin/src/cli.rs:195`). Compute `runtime_config_path = spin_path.parent().unwrap_or(manifest_root).join("runtime-config.toml")` -- `runtime-config.toml` lives next to `spin.toml`, matching the Spin convention. **Assume both files already exist** -- Task 8b's CLI bootstrap calls Spin's `synthesise_baseline_manifest` (which returns BOTH paths from Task 24) and writes them BEFORE `provision` runs. The trait does not receive `app_name` or the component-id default, so synthesis CANNOT happen here. If either file is unexpectedly absent, return an error pointing at the missing path.
  2. Parse `spin_path` via `toml_edit::DocumentMut`; locate `[component.<component_id>]` and append missing store ids to `key_value_stores`.
  3. Parse `runtime_config_path`; append `[key_value_store.<platform>]` with `type = "spin"`, `path = ".spin/sqlite_key_value.db"` for each id.
  4. Append `__NAME` and commented `__KEY` lines to `<spin_crate>/.env` via `edgezero_adapter::env_file::append_lines_dedup` (introduced in Task 16c).
  5. Write back; status lines.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-spin spin_local_provision`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-spin/src/cli.rs
  git commit -m "Spin: local-mode provision writes bindings + runtime-config + .env __NAME lines"
  ```

### Task 26: Spin `provision_typed` — lowercased variables + `SPIN_VARIABLE_*` placeholders

**Files:**

- Modify: `crates/edgezero-adapter-spin/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn spin_provision_typed_writes_lowercased_variables_and_uppercased_env() {
      let entries = [TypedSecretEntry::new("default", "API_TOKEN", "Demo_API_TOKEN")];
      // Run provision_typed with mode = Local. Assert:
      //   spin.toml contains:
      //     [variables]
      //     demo_api_token = { default = "", secret = true }
      //     [component.<id>.variables]
      //     demo_api_token = "{{ demo_api_token }}"
      //   .env contains:
      //     SPIN_VARIABLE_DEMO_API_TOKEN=
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter-spin spin_provision_typed_writes_lowercased`
  Expected: FAIL

- [ ] **Step 3: Implement**

  Override `provision_typed`:

  - Resolve `component_id` using Spin's existing manifest-based resolution -- `provision_typed` does NOT receive `app_name` (verify by re-reading the trait signature at Task 4), so the `[app].name` fallback the old plan wording mentioned cannot work here. Instead:
    1. If `component_selector` is `Some(id)`, use it verbatim AND verify a `[component.<id>]` block exists in the parsed `spin.toml` (mirrors today's verification at `crates/edgezero-adapter-spin/src/cli.rs:942`); error if absent.
    2. If `component_selector` is `None`, parse `spin.toml` and walk `doc["component"].as_table()`. If exactly ONE entry exists, use its key. If zero or multiple, return an explicit "ambiguous Spin component; set `[adapters.spin.adapter].component` to one of: ..." error.

    Synthesis fallback (where `app_name` IS available) lives in `synthesise_spin_toml` (Task 24); that's the right place for the app-name default. By the time `provision_typed` runs, the baseline is already on disk and the manifest's `[component.*]` is the authoritative source.
  - For each entry, compute `spin_var = entry.key_value.to_ascii_lowercase()`.
  - Upsert `[variables].<spin_var>` and `[component.<component_id>.variables].<spin_var>` IFF absent.
  - Append `SPIN_VARIABLE_<spin_var.to_ascii_uppercase()>=` to `<spin_crate>/.env` via `edgezero_adapter::env_file::append_lines_dedup` (Task 16c).

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter-spin spin_provision_typed`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-spin/src/cli.rs
  git commit -m "Spin: provision_typed writes lowercased [variables] + SPIN_VARIABLE_ placeholders"
  ```

### Task 27: Axum local-mode `provision` — ensure `.edgezero/` + write `.edgezero/.env`

**Files:**

- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Test: same file

**Interfaces:**

- Consumes: `ProvisionStores`
- Produces: Axum's `ProvisionMode::Local` arm. Does NOT touch `axum.toml`.

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn axum_local_provision_creates_dot_edgezero_dir() {
      // Empty fixture. Run local provision. Assert .edgezero/ exists.
  }

  #[test]
  fn axum_local_provision_does_not_touch_axum_toml() {
      // Pre-create axum.toml with sentinel content. Run provision local.
      // Assert axum.toml content is byte-for-byte unchanged.
  }

  #[test]
  fn axum_local_provision_writes_env_name_lines() {
      // Same shape as the Spin test, but writes to .edgezero/.env.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-adapter-axum axum_local_provision`
  Expected: FAIL

- [ ] **Step 3: Implement Axum's local arm**

  1. `std::fs::create_dir_all(<manifest_root>/.edgezero)`.
  2. Write `EDGEZERO__STORES__<KIND>__<LOGICAL_ID>__NAME=<platform>` lines and commented `__KEY` examples to `.edgezero/.env` via `edgezero_adapter::env_file::append_lines_dedup` (Task 16c).
  3. **Do NOT** synthesise, merge, or touch `axum.toml`.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-axum axum_local_provision`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-axum/src/cli.rs
  git commit -m "Axum: local-mode provision creates .edgezero/ and writes .env (no axum.toml changes)"
  ```

### Task 28: Axum `provision_typed` — append secret placeholders to `.edgezero/.env`

**Files:**

- Modify: `crates/edgezero-adapter-axum/src/cli.rs`
- Test: same file

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn axum_provision_typed_appends_secret_placeholders_to_edgezero_env() {
      let entries = [TypedSecretEntry::new("default", "api_token", "demo_api_token")];
      // Run provision_typed local. Read .edgezero/.env, assert line:
      //   demo_api_token=
  }
  ```

- [ ] **Step 2: Run test to verify it fails**

  Run: `cargo test -p edgezero-adapter-axum axum_provision_typed`
  Expected: FAIL

- [ ] **Step 3: Implement**

  Override `provision_typed`: for each entry, append `<key_value>=` (empty value) to `.edgezero/.env` via `edgezero_adapter::env_file::append_lines_dedup`.

- [ ] **Step 4: Run test to verify it passes**

  Run: `cargo test -p edgezero-adapter-axum axum_provision_typed`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-axum/src/cli.rs
  git commit -m "Axum: provision_typed appends secret placeholders to .edgezero/.env"
  ```

### Task 28b: Re-enable the worktree-clean dry-run test

**Files:**

- Modify: `crates/edgezero-cli/src/provision.rs` (remove `#[ignore]`)

- [ ] **Step 1: Drop the `#[ignore]` attribute on `provision_local_dry_run_worktree_clean_and_no_tempdir_paths_in_stdout`** (introduced in Task 13).

- [ ] **Step 2: Run the test**

  Run: `cargo test -p edgezero-cli provision_local_dry_run_worktree_clean_and_no_tempdir_paths_in_stdout`
  Expected: PASS (all four adapters land their writes under the tempdir, worktree byte-identical)

- [ ] **Step 3: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs
  git commit -m "Re-enable provision_local_dry_run worktree-clean test"
  ```

---

## Section 6 — Typed provision (CLI export + scaffold)

### Task 29: Add `pub fn run_provision_typed<C>` export

**Files:**

- Modify: `crates/edgezero-cli/src/lib.rs:55`
- Modify: `crates/edgezero-cli/src/provision.rs` (function body)
- Test: `crates/edgezero-cli/src/provision.rs` test module

**Interfaces:**

- Consumes: `run_provision`, `run_typed_preflight`, `Adapter::provision_typed`
- Produces:
  ```rust
  pub fn run_provision_typed<C: DeserializeOwned + Validate + AppConfigMeta>(
      args: &ProvisionArgs,
  ) -> Result<(), String>;
  ```

- [ ] **Step 1: Write the failing tests**

  ```rust
  #[test]
  fn run_provision_typed_cloud_short_circuits_without_loading_app_config() {
      // Fixture: edgezero.toml + missing/malformed <app>.toml.
      // Call run_provision_typed with args.local = false.
      // Assert OK (delegated to run_provision without touching <app>.toml).
  }

  #[test]
  fn run_provision_typed_local_fails_loud_on_malformed_app_config() {
      // Fixture: <app>.toml has a non-secret validator-rejected value.
      // Call with args.local = true.
      // Assert error contains "app config validation failed".
  }

  #[test]
  fn run_provision_typed_local_builds_typed_secret_entries_from_raw_table() {
      // Fixture: <app>.toml has api_token = "demo_api_token".
      // C has a #[secret] field `api_token` with KeyInDefault.
      // Capture the TypedSecretEntry slice the adapter receives via
      // a fake adapter. Assert:
      //   slice[0].store_id == "default"
      //   slice[0].field_name == "api_token"
      //   slice[0].key_value == "demo_api_token"
  }

  #[test]
  fn run_provision_typed_local_dry_run_runs_capability_preflight() {
      // Fixture: manifest declares an adapter whose `[stores.kv]`
      // declaration violates enforce_single_store_capability
      // (e.g. multiple ids on an adapter that only supports one).
      // Run run_provision_typed with args.local = true, dry_run = true.
      // Assert error matches the same wording
      // enforce_single_store_capability emits today; assert the
      // tempdir was never created (no staging happened, the gate
      // fired first). Locks parity with run_provision's gate at
      // crates/edgezero-cli/src/provision.rs:64.
  }

  #[test]
  fn run_provision_typed_local_dry_run_runs_handler_paths_preflight() {
      // Fixture: manifest declares a handler whose path violates
      // strict_handler_paths. Run typed dry-run; assert the
      // strict_handler_paths error fires before staging.
      // Locks parity with run_provision's gate at
      // crates/edgezero-cli/src/provision.rs:77.
  }

  #[test]
  fn run_provision_typed_local_dry_run_handles_case_preserving_adapter_key() {
      // Fixture: manifest declares `[adapters.Fastly]` (mixed
      // case). Run typed dry-run with args.adapter = "fastly".
      // Manifest::adapter_entry returns canonical "Fastly".
      // Assert the dry-run diff section is NON-EMPTY (covers
      // fastly.toml from the allow-list). Without lowercasing
      // the adapter name in the render call, the allow-list
      // match falls through to `_ => &[]` and the diff is
      // silently empty.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli run_provision_typed`
  Expected: FAIL

- [ ] **Step 3: Implement `run_provision_typed`**

  Existing helpers this body relies on (all verified to exist at the cited paths):

  - `crate::config::load_validation_context_with_options` (Task 5 introduces; primitive-arg loader).
  - `crate::config::run_typed_preflight` (Task 5).
  - `crate::config::build_typed_secret_entries::<C>(ctx)` (Task 5 step 6 extracts).
  - `edgezero_core::app_config::AppConfigMeta` (NOTE the correct path is `app_config`, NOT `app`; trait at `crates/edgezero-core/src/app_config.rs:34`).
  - `edgezero_adapter::get_adapter(name) -> Option<&'static dyn Adapter>` (at `crates/edgezero-adapter/src/registry.rs:513`; returns `Option`, not `Result`).
  - `crate::path_safety::assert_provision_paths_contained` (Task 6).
  - `manifest_root_from(&args.manifest)` — small helper in `provision.rs` that returns `args.manifest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."))`. Extract from the existing inline duplication in `run_provision`.
  - `build_stores_against` (Task 11 step 3).
  - `write_baseline_to_disk` (Task 8b step 4).
  - `render_dry_run_report` (Task 12).

  In `crates/edgezero-cli/src/provision.rs`:

  ```rust
  pub fn run_provision_typed<C>(args: &ProvisionArgs) -> Result<(), String>
  where
      C: serde::de::DeserializeOwned
          + validator::Validate
          + edgezero_core::app_config::AppConfigMeta,
  {
      // 1. Cloud-mode short-circuit (MUST per spec §"Validation").
      if !args.local {
          return run_provision(args);
      }

      // 2. Build the ValidationContext with env_overlay = false
      //    (provision captures operator-typed values). The wrapper
      //    handles --manifest -> Manifest + raw TOML load.
      let ctx = crate::config::load_validation_context_with_options(
          &args.manifest,
          /* app_config override */ None, // see "Out of scope: --app-config on provision" in spec
          /* strict */ false,
          /* env_overlay */ false,
      )?;

      // 3. Base preflight gates -- MUST mirror run_provision's
      //    pre-validation gates so typed dry-run can't bypass
      //    expensive-mistake protection (capability check at
      //    `crates/edgezero-cli/src/provision.rs:64`; handler-path
      //    check at `:77`). The real-write arm below calls
      //    run_provision, which runs these for free; the dry-run
      //    arm inlines base provision inside staging, so the gates
      //    need to fire HERE before any tempdir work begins.
      enforce_single_store_capability(ctx.manifest(), &args.adapter)?;
      strict_handler_paths(ctx.manifest())?;

      // 4. Path containment. Look up the adapter entry via the
      //    canonical (case-insensitive) key.
      let (canonical_adapter_name, adapter_cfg) = ctx
          .manifest()
          .adapter_entry(&args.adapter)
          .ok_or_else(|| format!("adapter `{}` not declared in manifest", args.adapter))?;
      let manifest_root = manifest_root_from(&args.manifest);
      crate::path_safety::assert_provision_paths_contained(
          manifest_root,
          adapter_cfg.adapter.manifest.as_deref(),
          adapter_cfg.adapter.crate_path.as_deref(),
      )?;

      // 5. Typed deserialise + non-secret validate. We already have
      //    the raw TOML in ctx.raw_config; for the typed deserialise,
      //    re-run the typed loader with the same env_overlay = false
      //    setting -- there is no shared raw->typed conversion helper
      //    in app_config today, so two loads is correct.
      let mut opts = edgezero_core::app_config::AppConfigLoadOptions::default();
      opts.env_overlay = false;
      let cfg: C = edgezero_core::app_config::deserialize_app_config_with_options::<C>(
          ctx.app_config_path(),
          ctx.app_name(),
          &opts,
      )
      .map_err(|e| format!("app config load failed: {e}"))?;
      edgezero_core::app_config::validate_excluding_secrets(&cfg)
          .map_err(|e| format!("app config validation failed: {e}"))?;

      // 6. Shared typed preflight (typed_secret_checks +
      //    run_adapter_typed_checks, which covers Spin's variable-
      //    name rules via adapter.validate_typed_secrets).
      crate::config::run_typed_preflight(&cfg, &ctx)?;

      // 7. Build the TypedSecretEntry slice the adapter will see.
      let entries = crate::config::build_typed_secret_entries::<C>(&ctx)?;

      // 8. Dispatch. The base provision (run_provision) handles
      //    bootstrap synthesis + per-store binding emission;
      //    provision_typed only adds per-secret placeholders.
      let adapter = edgezero_adapter::get_adapter(canonical_adapter_name)
          .ok_or_else(|| format!("adapter `{canonical_adapter_name}` not registered"))?;

      match args.dry_run {
          false => {
              // Real-write: base provision mutates the worktree
              // first (synthesis + bindings), then provision_typed.
              run_provision(args)?;
              let outcome = adapter.provision_typed(
                  manifest_root,
                  adapter_cfg.adapter.manifest.as_deref(),
                  adapter_cfg.adapter.component.as_deref(),
                  &entries,
                  edgezero_adapter::ProvisionMode::Local,
                  false,
              )?;
              for line in outcome.status_lines {
                  println!("{line}");
              }
              Ok(())
          }
          true => {
              // Shared dry-run staging: ONE tempdir hosts base
              // provision + provision_typed so the typed merge
              // sees the baseline manifest the base step wrote.
              // run_with_staging takes the project-RELATIVE crate
              // path (Task 10) -- pass adapter_cfg.adapter.crate_path
              // directly, no need for the old resolve-then-strip
              // helper.
              let adapter_crate_rel = adapter_cfg
                  .adapter
                  .crate_path
                  .as_deref()
                  .map(Path::new)
                  .unwrap_or_else(|| Path::new("."));
              let deployed_state = deployed_state_for(
                  ctx.manifest(),
                  canonical_adapter_name,
              );
              let baseline_pairs = adapter.synthesise_baseline_manifest(
                  manifest_root,
                  adapter_cfg.adapter.manifest.as_deref(),
                  adapter_cfg.adapter.component.as_deref(),
                  ctx.app_name(),
                  deployed_state.as_ref(),
              )?;
              let ((_outcome, report), _tempdir) = run_with_staging(
                  manifest_root,
                  adapter_crate_rel,
                  |staged_root, _staged_crate| {
                      write_baseline_to_disk(staged_root, &baseline_pairs)?;
                      adapter.validate_adapter_manifest(
                          staged_root,
                          adapter_cfg.adapter.manifest.as_deref(),
                          adapter_cfg.adapter.component.as_deref(),
                      )?;
                      let owned_stores = build_stores_against(
                          staged_root, args, adapter, ctx.manifest(),
                      )?;
                      let stores = owned_stores.as_refs();
                      let base = adapter.provision(
                          staged_root,
                          adapter_cfg.adapter.manifest.as_deref(),
                          adapter_cfg.adapter.component.as_deref(),
                          &stores,
                          deployed_state.as_ref(),
                          edgezero_adapter::ProvisionMode::Local,
                          false,
                      )?;
                      let typed = adapter.provision_typed(
                          staged_root,
                          adapter_cfg.adapter.manifest.as_deref(),
                          adapter_cfg.adapter.component.as_deref(),
                          &entries,
                          edgezero_adapter::ProvisionMode::Local,
                          false,
                      )?;
                      let combined = edgezero_adapter::ProvisionOutcome {
                          status_lines: base
                              .status_lines
                              .into_iter()
                              .chain(typed.status_lines)
                              .collect(),
                          deployed: base.deployed.or(typed.deployed),
                      };
                      // Render INSIDE the closure: staged_root is
                      // still valid here; the tempdir drops after
                      // this closure returns. Lowercase the adapter
                      // name -- the allow-list builder arms are
                      // lowercase, but adapter_entry returns the
                      // canonical operator spelling ("Fastly"
                      // would silently miss the allow-list).
                      //
                      // Build the allow-list from the RESOLVED
                      // adapter manifest path (NOT a static
                      // filename) so nested paths like
                      // `crates/cf/wrangler.toml` diff correctly.
                      let adapter_lower =
                          canonical_adapter_name.to_ascii_lowercase();
                      let adapter_manifest_abs = staged_root.join(
                          adapter_cfg.adapter.manifest.as_deref()
                              .unwrap_or(default_adapter_manifest_for(
                                  &adapter_lower,
                              )),
                      );
                      let allow_list = build_dry_run_allow_list(
                          manifest_root,
                          staged_root,
                          &adapter_lower,
                          &adapter_manifest_abs,
                      );
                      let report = render_dry_run_report(
                          manifest_root,
                          staged_root,
                          &allow_list,
                          &combined,
                      );
                      Ok((combined, report))
                  },
              )?;
              // Per Task 12, render_dry_run_report MUST run INSIDE
              // the staging closure (the tempdir handle is dropped
              // by the time we get here). The closure should build
              // a (combined_outcome, report_string) tuple and the
              // outer let-binding takes both. The report uses the
              // crate-RELATIVE path (NOT the joined absolute one).
              println!("{report}");
              Ok(())
          }
      }
  }
  ```

  The one `...` in the dry-run report call is intentional: `render_dry_run_report` needs the tempdir root, but `_tempdir` was dropped after the closure returned. The implementing PR has two equivalent fixes: (a) thread the diff/status rewriting INSIDE the closure (preferred — the closure already has `staged_root` in scope); or (b) change `run_with_staging` to return `(R, TempDir, PathBuf /* staged_root */)` so the caller can diff before drop. Pick (a); the snippet's closure already constructs the combined outcome, so move the `render_dry_run_report(...) -> String` call before the `Ok(...)` return and have the closure return `(combined, report_string)`. Update the outer `let (combined, _tempdir) = ...` to `let ((combined, report), _tempdir) = ...` and `println!("{report}")` immediately after.

- [ ] **Step 4: Export from `lib.rs:55`**

  In `crates/edgezero-cli/src/lib.rs`, alongside the existing `pub use provision::run_provision;`:

  ```rust
  pub use provision::run_provision_typed;
  ```

- [ ] **Step 5: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli run_provision_typed`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/provision.rs crates/edgezero-cli/src/lib.rs
  git commit -m "Add run_provision_typed<C> public entry point"
  ```

### Task 30: Update scaffold template `main.rs.hbs` to route Cmd::Provision through `run_provision_typed`

**Files:**

- Modify: `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:98`
- Test: scaffold contract test (likely in `crates/edgezero-cli/src/generator.rs`)

- [ ] **Step 1: Find the existing scaffold contract test**

  Look for tests in `crates/edgezero-cli/src/generator.rs` that render the scaffold and grep the output. If a test already verifies the Push / Validate Cmd dispatch, mirror its pattern.

- [ ] **Step 2: Write the failing test**

  ```rust
  #[test]
  fn scaffold_renders_provision_with_typed_dispatch() {
      let rendered = render_scaffold(/* ... */);
      let main_rs = rendered.get("crates/<proj_cli>/src/main.rs").unwrap();
      assert!(
          main_rs.contains("edgezero_cli::run_provision_typed::<DemoConfig>(&args)"),
          "main.rs missing typed provision dispatch:\n{main_rs}"
      );
  }
  ```

  (Adapt `DemoConfig` and project name to whatever the scaffold fixture uses.)

- [ ] **Step 3: Run test to verify it fails**

  Run: `cargo test -p edgezero-cli scaffold_renders_provision_with_typed`
  Expected: FAIL

- [ ] **Step 4: Update the template**

  In `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:98`, replace:

  ```handlebars
  Cmd::Provision(args) => edgezero_cli::run_provision(&args),
  ```

  with:

  ```handlebars
  Cmd::Provision(args) => edgezero_cli::run_provision_typed::<{{NameUpperCamel}}Config>(&args),
  ```

- [ ] **Step 5: Run test to verify it passes**

  Run: `cargo test -p edgezero-cli scaffold_renders_provision_with_typed`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/templates/cli/src/main.rs.hbs
  git commit -m "Scaffold: route Cmd::Provision through run_provision_typed"
  ```

### Task 30b: Update the in-tree `app-demo-cli` to call `run_provision_typed`

**Files:**

- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs:98`

The scaffold template change in Task 30 only affects FUTURE projects generated by `edgezero new`. The CHECKED-IN `app-demo-cli` predates this spec and currently still calls `run_provision(&args)` (verified at `examples/app-demo/crates/app-demo-cli/src/main.rs:98`). Smoke scripts (Tasks 35-37) warm up via `app-demo-cli provision --adapter <name> --local`, so without this update the typed-secret placeholders never land in the smoke fixtures — Spin's `[variables]` declarations, `SPIN_VARIABLE_*` lines, and Cloudflare's `.dev.vars` placeholders would all be missing, breaking spec §"Per-adapter test contract".

- [ ] **Step 1: Write the failing test**

  Add a smoke assertion that, after `smoke_warmup_provision_local "spin"`, the Spin-side `.env` contains a `SPIN_VARIABLE_DEMO_API_TOKEN=` line (the app-demo `#[secret] api_token` field's canonical Spin variable). The assertion can live in `scripts/smoke_test_secrets.sh` as a post-warm-up grep, or as a Rust-level integration test in `examples/app-demo/crates/app-demo-cli/tests/`. The former is closer to operator reality.

- [ ] **Step 2: Run the smoke to verify it fails**

  Run: `./scripts/smoke_test_secrets.sh spin`
  Expected: FAIL on the `SPIN_VARIABLE_DEMO_API_TOKEN=` grep (line absent — bundled `run_provision` doesn't walk typed secrets).

- [ ] **Step 3: Update the in-tree CLI**

  In `examples/app-demo/crates/app-demo-cli/src/main.rs:98`, change:

  ```rust
  Cmd::Provision(args) => edgezero_cli::run_provision(&args),
  ```

  to:

  ```rust
  Cmd::Provision(args) => edgezero_cli::run_provision_typed::<AppDemoConfig>(&args),
  ```

  Use whatever name `app-demo-cli` already uses for the typed `C` (likely `AppDemoConfig` or `DemoConfig` — match the `use` statements at the top of the same file; the scaffold template's `{{NameUpperCamel}}Config` pattern in Task 30 mirrors this).

- [ ] **Step 4: Re-run the smoke to verify it passes**

  Run: `./scripts/smoke_test_secrets.sh spin`
  Expected: PASS — typed dispatch now lands the `SPIN_VARIABLE_DEMO_API_TOKEN=` placeholder.

- [ ] **Step 5: Commit**

  ```bash
  git add examples/app-demo/crates/app-demo-cli/src/main.rs scripts/smoke_test_secrets.sh
  git commit -m "app-demo-cli: route Cmd::Provision through run_provision_typed"
  ```

### Task 31: Update `edgezero new` generator to loop provision over selected adapters

**Files:**

- Modify: `crates/edgezero-cli/src/generator.rs` (`generate_new`)
- Test: same file

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn generate_new_provisions_every_selected_adapter_cloudflare_spin() {
      // Run generate_new selecting cloudflare + spin (NO axum).
      // Assert in the generated project tree:
      //   crates/<proj>-adapter-cloudflare/wrangler.toml exists
      //   crates/<proj>-adapter-cloudflare/.dev.vars exists
      //   crates/<proj>-adapter-spin/spin.toml exists
      //   crates/<proj>-adapter-spin/runtime-config.toml exists
      //   crates/<proj>-adapter-spin/.env exists
      //   project/.edgezero/ is ABSENT — `.edgezero/` is the
      //     Axum-owned local-state dir per spec §"Axum" in
      //     "Per-adapter local state"; Cloudflare and Spin do
      //     NOT create it (Cloudflare writes `.dev.vars`,
      //     Spin writes `<spin_crate>/.env`).
      //   project/axum.toml ABSENT (scaffold only emits it when
      //     axum is in adapter_artifacts.adapter_ids).
  }

  #[test]
  fn generate_new_provisions_every_selected_adapter_axum() {
      // Run generate_new selecting axum alone (covers the
      // .edgezero/ side that's missing from the Cloudflare+Spin
      // fixture above).
      // Assert:
      //   project/axum.toml exists (scaffold-emitted)
      //   project/.edgezero/ exists (Axum local-state dir)
      //   project/.edgezero/.env exists (Task 27 wrote __NAME lines)
  }

  #[test]
  fn generate_new_fails_when_any_adapter_provision_fails() {
      // Inject a synthetic failure for one adapter.
      // Assert generate_new returns an error mentioning the failed adapter.
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli generate_new_provisions generate_new_fails_when_any`
  Expected: FAIL

- [ ] **Step 3: Add the loop**

  Inside `generate_new`, AFTER the template render step that writes `edgezero.toml` and all per-adapter crates, insert:

  ```rust
  for adapter_id in &adapter_artifacts.adapter_ids {
      let mut prov_args = crate::args::ProvisionArgs::default();
      prov_args.adapter = adapter_id.clone();
      prov_args.local = true;
      prov_args.dry_run = false;
      prov_args.manifest = project_root.join("edgezero.toml");
      crate::run_provision(&prov_args).map_err(|err| {
          format!(
              "scaffold provision failed for adapter `{adapter_id}`: {err}"
          )
      })?;
  }
  ```

  If `ProvisionArgs` is `#[non_exhaustive]` (Task 1 keeps it so), build it via `Default::default()` + per-field assignment exactly as shown — do NOT use struct-update syntax.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli generate_new_provisions generate_new_fails_when_any`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/generator.rs
  git commit -m "Scaffold: loop run_provision over every selected adapter"
  ```

---

## Section 7 — Gitignore, migration, `run_serve`

### Task 32: Extend existing `gitignore.hbs` scaffold template with provision entries

**Files:**

- Modify: `crates/edgezero-cli/src/templates/root/gitignore.hbs` (template already exists)
- Test: scaffold contract test at `crates/edgezero-cli/src/generator.rs`'s test module (existing `.gitignore` assertion at `:1192-1193`)

The template AND the emit-list wiring (`root_gitignore` → `.gitignore`) already exist on this branch — verified `crates/edgezero-cli/src/templates/root/gitignore.hbs` is present and `crates/edgezero-cli/src/generator.rs:694` has `("root_gitignore", ".gitignore")`. This task adds the new manifest + local-state entries; no template creation or emit-list edits needed.

- [ ] **Step 1: Read the current template**

  Run: `cat crates/edgezero-cli/src/templates/root/gitignore.hbs`

  Note which of the four manifest names (`fastly.toml`, `spin.toml`, `wrangler.toml`, `runtime-config.toml`) and which of the local-state paths (`.edgezero/`, `.wrangler/`, `.spin/`, `.dev.vars`, `.env`) are already present.

- [ ] **Step 2: Write the failing assertion**

  Extend the existing scaffold test (the assertion already at `generator.rs:1192-1193` reads `.gitignore`):

  ```rust
  for entry in ["fastly.toml", "spin.toml", "wrangler.toml",
                "runtime-config.toml", ".edgezero/", ".wrangler/",
                ".spin/", ".dev.vars", ".env"] {
      assert!(gitignore.contains(entry), "missing `{entry}` in .gitignore");
  }
  assert!(!gitignore.contains("axum.toml"), "axum.toml must NOT be gitignored");
  ```

- [ ] **Step 3: Run test to verify it fails**

  Run: `cargo test -p edgezero-cli scaffold` (or the specific test name covering the `.gitignore` assertion)
  Expected: FAIL on whichever entries are missing.

- [ ] **Step 4: Add the missing entries to the existing template**

  Append (only the entries Step 1 confirmed missing) — keep existing entries / comments / whitespace untouched. Suggested section to add:

  ```
  # Cloudflare / Fastly / Spin manifests — regenerated by `edgezero provision --local`.
  # axum.toml is INTENTIONALLY NOT in this list (it stays tracked).
  fastly.toml
  spin.toml
  wrangler.toml
  runtime-config.toml

  # Per-adapter local emulator state
  .edgezero/
  .wrangler/
  .spin/
  .dev.vars
  .env
  ```

- [ ] **Step 5: Run test to verify it passes**

  Run: `cargo test -p edgezero-cli scaffold`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git add crates/edgezero-cli/src/templates/root/gitignore.hbs
  git commit -m "Scaffold: extend .gitignore with provision-owned manifests + local state"
  ```

### Task 33: Gitignore + `git rm --cached` the in-tree app-demo manifests

**Files:**

- Modify: root `.gitignore`
- Delete (via `git rm --cached`):
  - `examples/app-demo/crates/app-demo-adapter-fastly/fastly.toml`
  - `examples/app-demo/crates/app-demo-adapter-cloudflare/wrangler.toml`
  - `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`
  - Any in-tree `runtime-config.toml` next to the Spin manifest

- [ ] **Step 1: Add the four manifest patterns + `.dev.vars` to root `.gitignore`**

  Append to the existing root `.gitignore` (the current file at `.gitignore:18` already covers `.env` and the `.wrangler/` / `.spin/` state dirs but does NOT include `.dev.vars` — Cloudflare's smoke warm-up creates that file via Task 19, so without this entry the post-smoke worktree would not be clean):

  ```
  # Cloudflare / Fastly / Spin manifests — regenerated by `edgezero provision --local`.
  # NOTE: axum.toml is intentionally NOT in this list.
  fastly.toml
  spin.toml
  wrangler.toml
  runtime-config.toml

  # Cloudflare per-adapter local secret placeholders — written by
  # `<app-cli> provision --adapter cloudflare --local` (Task 20).
  # Operator-edited values must NEVER be committed.
  .dev.vars
  ```

- [ ] **Step 2: Untrack the existing in-tree manifests using the portable runbook**

  Run from repo root (do NOT use `xargs -r` — it is GNU-only):

  ```bash
  # Regex matches the four generated manifest files AND any tracked
  # `.dev.vars` (Cloudflare's per-secret placeholder file).
  # The plan's in-tree app-demo fixture happens to NOT track
  # `.dev.vars` today, but aligning the regex with the spec's
  # downstream-migration regex (spec §"Migration for downstream
  # projects") keeps the two runbooks symmetrical and protects
  # against future drift where a contributor commits a smoke
  # artifact by mistake.
  tracked=$(git ls-files | rg '(^|/)(fastly|spin|wrangler|runtime-config)\.toml$|(^|/)\.dev\.vars$' || true)
  if [ -n "$tracked" ]; then
      printf '%s\n' "$tracked" | xargs git rm --cached
  fi
  ```

- [ ] **Step 3: Verify the worktree still has the files locally** (so the smoke + dev loop keeps working until step 4's `provision --local` warm-up).

  Run: `ls examples/app-demo/crates/app-demo-adapter-{fastly,cloudflare,spin}/`
  Expected: each adapter's manifest is still present in the worktree.

- [ ] **Step 4: Sanity-check the gitignore covered ALL five gated paths**

  Run the SAME regex the CI gate (Task 37) installs — match the four generated manifests AND `.dev.vars`. The regex MUST stay byte-identical to Task 37's so the two checks can never drift; if you find yourself changing one, change the other in the same commit.

  ```sh
  git ls-files | rg '(^|/)(fastly|spin|wrangler|runtime-config)\.toml$|(^|/)\.dev\.vars$' && exit 1 || true
  ```

  Expected: no output. (`axum.toml` is intentionally NOT in the regex; it stays tracked.) Task 37 lands the same check as a permanent CI step; this step is the local pre-flight verification that the `git rm --cached` in Step 2 caught every tracked instance.

- [ ] **Step 5: Commit**

  ```bash
  git add .gitignore
  git commit -m "Gitignore Cloudflare/Fastly/Spin manifests; regenerate via provision --local"
  ```

### Task 34: Extend `run_serve` with adapter-scoped env-file load

**Files:**

- Modify: `crates/edgezero-cli/src/lib.rs:179` (`run_serve`)
- Test: `crates/edgezero-cli/src/lib.rs` test module (or a dedicated `tests/run_serve_env_load.rs`)

**Interfaces:**

- Consumes: a new `crates/edgezero-cli/src/env_file.rs` module exposing ONLY a process-env loader (`pub(crate) fn load_into_process_env(path: &Path) -> Result<(), String>`). The dedup/append helper from Task 16c lives in `edgezero-adapter` and is not used here — `run_serve` only needs the file → `std::env::set_var` direction.
- Produces: at most one adapter-scoped env file loaded into process env per invocation

- [ ] **Step 1: Write the failing test**

  ```rust
  #[test]
  fn run_serve_adapter_axum_loads_dot_edgezero_dot_env() {
      // Fixture: <manifest_root>/.edgezero/.env contains
      //   AXUM_SAW=axum_value
      // Use a fake serve dispatch that records env::vars() at call time.
      // Run run_serve with --adapter axum. Assert recorded vars contain
      //   ("AXUM_SAW", "axum_value")
  }

  #[test]
  fn run_serve_adapter_spin_loads_spin_crate_dot_env() {
      // Fixture: <spin_crate>/.env contains SPIN_SAW=spin_value.
      // Run with --adapter spin; assert recorded vars contain it.
  }

  #[test]
  fn run_serve_adapter_spin_does_NOT_load_axum_env_file() {
      // Fixture: BOTH .edgezero/.env (EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=axum_only)
      // AND <spin_crate>/.env (EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=spin_only).
      // Run --adapter spin. Assert recorded value == "spin_only".
  }

  #[test]
  fn run_serve_adapter_axum_does_NOT_load_spin_env_file() {
      // Mirror of the above; assert axum sees "axum_only".
  }

  #[test]
  fn run_serve_adapter_match_is_case_insensitive() {
      // Run `edgezero serve --adapter Spin` (mixed case).
      // `Manifest::adapter_entry` (case-insensitive) finds the
      // manifest entry and the adapter registry dispatch
      // succeeds today regardless of case. The env-file load
      // arm MUST also match -- assert the spawned child saw
      // the Spin `.env` value, NOT an empty env. A literal
      // `match args.adapter.as_str() { "spin" => ... }` would
      // silently skip loading; the lowercase-once fix above
      // closes that gap. Repeat for `--adapter Axum` and
      // `--adapter AXUM` (case-insensitive matcher applies to
      // both arms).
  }
  ```

- [ ] **Step 2: Run tests to verify they fail**

  Run: `cargo test -p edgezero-cli run_serve_adapter`
  Expected: FAIL

- [ ] **Step 3: Implement the adapter-scoped env-file load**

  Today's `run_serve` at `crates/edgezero-cli/src/lib.rs:179-188` does only `load_manifest_optional`, `ensure_adapter_defined`, and `adapter::execute`. The env-file load needs to do its own adapter-entry lookup (to find the canonical adapter key + `[adapters.<name>.adapter].manifest`/.crate paths) and resolve the Spin crate dir using the same convention provision uses. Concrete additions:

  In `crates/edgezero-cli/src/lib.rs:179`'s `run_serve`, replace the body with:

  ```rust
  pub fn run_serve(args: &ServeArgs) -> Result<(), String> {
      let manifest = load_manifest_optional()?;
      ensure_adapter_defined(&args.adapter, manifest.as_ref())?;

      // Adapter-scoped env-file load (spec §"Env-file `__NAME` values
      // match adapter bindings"). At most one file is loaded per
      // invocation; cloudflare / fastly read their own files via the
      // emulator and need no CLI-side help.
      //
      // `ServeArgs` carries only `adapter` (no `.manifest` field at
      // `crates/edgezero-cli/src/args.rs:202`). The manifest root
      // comes from the already-loaded `ManifestLoader` -- same
      // pattern existing dispatch uses at
      // `crates/edgezero-cli/src/adapter.rs:99`.
      if let Some(loader) = manifest.as_ref() {
          let manifest_doc = loader.manifest();
          let manifest_root = manifest_doc
              .root()
              .map(Path::to_path_buf)
              .unwrap_or_else(|| PathBuf::from("."));
          let entry = manifest_doc.adapter_entry(&args.adapter);
          // Lowercase ONCE so the match arms can stay literal.
          // `adapter_entry` is case-insensitive (preserves
          // operator spelling at `manifest.rs:121`) and the
          // adapter registry's lookup is case-insensitive too,
          // so `edgezero serve --adapter Spin` dispatches fine
          // -- but without this lowercase, the literal "spin"
          // arm below would silently skip loading Spin's .env.
          let adapter_lower = args.adapter.to_ascii_lowercase();
          match adapter_lower.as_str() {
              "axum" => {
                  let env_file = manifest_root.join(".edgezero").join(".env");
                  if env_file.exists() {
                      env_file::load_into_process_env(&env_file)?;
                  }
              }
              "spin" => {
                  if let Some((_, adapter_cfg)) = entry {
                      let spin_crate = resolve_adapter_crate_dir_for_serve(
                          &manifest_root,
                          adapter_cfg.adapter.crate_path.as_deref(),
                      );
                      let env_file = spin_crate.join(".env");
                      if env_file.exists() {
                          env_file::load_into_process_env(&env_file)?;
                      }
                  }
              }
              _ => {}
          }
      }

      adapter::execute(
          &args.adapter,
          adapter::Action::Serve,
          manifest.as_ref(),
          &[],
      )
  }

  /// Join `manifest_root` with the operator-declared adapter
  /// crate path; fall back to `manifest_root` when unset. This
  /// helper exists ONLY for `run_serve` -- `run_provision`'s
  /// dispatch matrix (Task 11) passes the project-relative
  /// crate path directly into `run_with_staging` and does NOT
  /// need an analogous resolver. If a future caller needs the
  /// same join, extract to a shared module rather than copying
  /// the body.
  fn resolve_adapter_crate_dir_for_serve(
      manifest_root: &Path,
      raw_crate_path: Option<&str>,
  ) -> PathBuf {
      match raw_crate_path {
          Some(p) => manifest_root.join(p),
          None => manifest_root.to_path_buf(),
      }
  }
  ```

  Create `crates/edgezero-cli/src/env_file.rs`:

  ```rust
  //! Process-env loader used by `run_serve` to expose
  //! provision-written .env files to spawned children. Honours
  //! "existing env wins" -- skip lines whose key is already set.
  //! Dedup/merge logic lives in `edgezero_adapter::env_file`
  //! (Task 16c); this module only goes file -> std::env.
  use std::fs;
  use std::path::Path;

  pub(crate) fn load_into_process_env(path: &Path) -> Result<(), String> {
      let raw = fs::read_to_string(path)
          .map_err(|e| format!("read {}: {e}", path.display()))?;
      for line in raw.lines() {
          let trimmed = line.trim_start();
          if trimmed.is_empty() || trimmed.starts_with('#') {
              continue;
          }
          let Some((k, v)) = trimmed.split_once('=') else { continue };
          let k = k.trim();
          if std::env::var(k).is_ok() {
              continue; // existing env wins
          }
          std::env::set_var(k, v.trim().trim_matches('"'));
      }
      Ok(())
  }
  ```

  Add `mod env_file;` to `crates/edgezero-cli/src/lib.rs` alongside the other module declarations.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-cli run_serve_adapter`
  Expected: PASS

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-cli/src/lib.rs crates/edgezero-cli/src/env_file.rs
  git commit -m "run_serve: adapter-scoped env-file load (axum vs spin); existing env wins"
  ```

## Section 8 — Smoke warm-up + docs + CI

### Task 35: Add `scripts/lib/smoke_warmup.sh` + wire into every smoke (drops obsolete `backup_in_tree`)

**Files:**

- Create: `scripts/lib/smoke_warmup.sh`
- Modify: all four `scripts/smoke_test_*.sh`

This task lands the warm-up AND drops the obsolete `backup_in_tree` calls in a single commit, so smokes stay green at the per-task boundary.

- [ ] **Step 1: Audit each script for obsolete backup calls**

  Run: `grep -n 'backup_in_tree' scripts/smoke_test_*.sh`

  Note every line backing up `fastly.toml` / `spin.toml` / `wrangler.toml` / `runtime-config.toml` / `.dev.vars`. Lines for `axum.toml` (or anything still tracked) stay.

- [ ] **Step 2: Create the warm-up helper**

  Write `scripts/lib/smoke_warmup.sh`:

  ```sh
  # Shared smoke warm-up: provisions per-adapter local state via the
  # generated app-demo-cli so smoke scripts can boot emulators on
  # fresh clones where Cloudflare/Fastly/Spin manifests are gitignored.
  #
  # Caller must `ROOT_DIR=...` before sourcing (existing smoke bootstrap
  # pattern; see scripts/smoke_test_config.sh:19).
  #
  # app-demo is excluded from the root workspace (Cargo.toml:1 lists
  # explicit members; examples/app-demo is not among them), so cargo
  # commands run from inside DEMO_DIR. app-demo-cli has NO adapter
  # features — adapter selection happens via the CLI arg.
  : "${ROOT_DIR:?ROOT_DIR must be set by the caller (existing smoke bootstrap)}"
  DEMO_DIR="$ROOT_DIR/examples/app-demo"

  smoke_canonical_adapter() {
      case "$1" in
          cf|cloudflare) echo "cloudflare" ;;
          *)             echo "$1" ;;
      esac
  }

  smoke_warmup_provision_local() {
      local adapter
      adapter="$(smoke_canonical_adapter "$1")"
      (
          cd "$DEMO_DIR"
          cargo run --quiet -p app-demo-cli -- \
              provision --adapter "$adapter" --local
      )
  }
  ```

- [ ] **Step 3: Wire warm-up into every smoke AND drop obsolete backups in the same edit**

  For each of `scripts/smoke_test_config.sh`, `smoke_test_kv.sh`, `smoke_test_secrets.sh`, `smoke_test_config_key_override.sh`:

  1. AFTER the `ROOT_DIR=...` bootstrap line and BEFORE any `config push --local`, emulator boot, or assertion, source the helper and run the warm-up:

     ```sh
     # shellcheck source=lib/smoke_warmup.sh
     . "$ROOT_DIR/scripts/lib/smoke_warmup.sh"
     smoke_warmup_provision_local "$ADAPTER"  # raw operator alias; canonicalised inside
     ```

     Replace `$ADAPTER` with whatever local variable the script already uses for the operator-supplied adapter argument.

  2. Delete each obsolete `backup_in_tree` call referencing the four gitignored manifests + `.dev.vars`.

- [ ] **Step 4: Run every smoke against the worktree**

  ```bash
  ./scripts/smoke_test_config.sh cloudflare
  ./scripts/smoke_test_kv.sh fastly
  ./scripts/smoke_test_secrets.sh spin
  ./scripts/smoke_test_config_key_override.sh axum
  ```

  Expected: each smoke warms up first, then runs to completion. All PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add scripts/lib/smoke_warmup.sh scripts/smoke_test_config.sh scripts/smoke_test_kv.sh \
          scripts/smoke_test_secrets.sh scripts/smoke_test_config_key_override.sh
  git commit -m "Smoke: warm-up via provision --local; drop obsolete backup_in_tree calls"
  ```

### Task 36: Update documentation (10 docs/ files)

**Files:**

- Modify: per the spec's "Documentation impact" table:
  - `docs/guide/getting-started.md`
  - `docs/guide/cli-walkthrough.md`
  - `docs/guide/cli-reference.md`
  - `docs/guide/configuration.md`
  - `docs/guide/manifest-store-migration.md`
  - `docs/guide/blob-app-config-migration.md`
  - `docs/guide/adapters/cloudflare.md`
  - `docs/guide/adapters/fastly.md`
  - `docs/guide/adapters/spin.md`
  - `docs/guide/kv.md`
- Possibly: root `README.md`
- Build: `cd docs && npm run build` to regenerate `docs/.vitepress/dist/`

- [ ] **Step 1: Apply per-file updates** as the spec's documentation impact table prescribes. Key wording rules:
  - "Cloudflare / Fastly / Spin manifests are gitignored" — NOT "per-adapter manifests."
  - "Axum's `axum.toml` stays tracked" wherever the gitignore split is mentioned.
  - For each per-adapter page, document the `provision --adapter <name> --local` invocation + the manual `cd <adapter_crate>; spin up ...` sourcing pattern for the Spin direct-run case (path-explicit, NOT bare `source .env`).

- [ ] **Step 2: Run docs lint**

  Run: `cd docs && npm ci && npx prettier --check '**/*.md' && npx eslint .`
  Expected: PASS.

- [ ] **Step 3: Rebuild docs site**

  Run: `cd docs && npm run build`
  Expected: clean build.

- [ ] **Step 4: Commit**

  ```bash
  git add docs/
  git commit -m "Docs: update for provision --local + gitignored adapter manifests"
  ```

### Task 37: Add CI grep gate for all four gitignored adapter manifests

**Files:**

- Modify: `.github/workflows/test.yml` (or wherever the workspace-level CI lives)

The spec gitignores FOUR manifest filenames: `fastly.toml`, `spin.toml`, `wrangler.toml`, `runtime-config.toml`. The CI gate must cover all four, NOT just `runtime-config.toml`. `axum.toml` is intentionally NOT in the set (Axum exception).

- [ ] **Step 1: Add the gate as a CI step**

  In the existing test workflow, after dependency install + before `cargo test`, add:

  ```yaml
  - name: Enforce Cloudflare/Fastly/Spin manifests and .dev.vars are not tracked
    run: |
      if git ls-files | rg '(^|/)(fastly|spin|wrangler|runtime-config)\.toml$|(^|/)\.dev\.vars$'; then
        echo "::error::These adapter manifests AND Cloudflare's .dev.vars must be gitignored (spec §'Cloudflare / Fastly / Spin manifests are gitignored' + §'Migration for downstream projects'). axum.toml is the only adapter manifest that stays tracked. .dev.vars carries operator secret values and must NEVER be committed."
        exit 1
      fi
  ```

  The regex matches the four generated manifests AND `.dev.vars` -- aligning with the gitignore entries from Task 33 and the spec's downstream migration runbook. Without `.dev.vars` in the CI gate, an operator who accidentally `git add`'d their `.dev.vars` after editing real secret values would push the secrets to the remote unflagged.

- [ ] **Step 2: Verify the step is present**

  Run: `yq '.jobs.test.steps[] | select(.name == "Enforce Cloudflare/Fastly/Spin manifests and .dev.vars are not tracked")' .github/workflows/test.yml`
  Expected: the step is present.

- [ ] **Step 3: Sanity-check the regex matches the five targets but NOT `axum.toml`**

  Run locally:

  ```sh
  printf '%s\n' \
    'crates/x/fastly.toml' \
    'crates/x/spin.toml' \
    'crates/x/wrangler.toml' \
    'crates/x/runtime-config.toml' \
    'crates/x/.dev.vars' \
    'crates/x/axum.toml' \
    | rg '(^|/)(fastly|spin|wrangler|runtime-config)\.toml$|(^|/)\.dev\.vars$'
  ```
  Expected: prints the first FIVE (four manifests + `.dev.vars`), NOT `axum.toml`.

- [ ] **Step 4: Commit**

  ```bash
  git add .github/workflows/test.yml
  git commit -m "CI: gate fastly/spin/wrangler/runtime-config.toml + .dev.vars (axum.toml exempt)"
  ```

---

## Section 9 — Per-adapter contract tests + Spin env-label alignment

### Task 38: Cloudflare provision_local_* suite

**Files:** `crates/edgezero-adapter-cloudflare/src/cli.rs` test module

- [ ] **Step 1: Write four tests** — `first_run_writes_expected_files`, `re_provision_is_byte_identical`, `push_after_provision_preserves_dev_vars_secret_value`, `zero_cloud_calls`. The fourth test uses a panicking `fake_wrangler` shim on `PATH` (extend the existing `fake_wrangler_returning` infrastructure with a `fake_wrangler_panicking()` variant per spec §"Per-adapter test contract" item 5).

- [ ] **Step 2: Run all four to verify they fail**

  Run: `cargo test -p edgezero-adapter-cloudflare provision_local_`
  Expected: FAIL on whichever assertions are missing.

- [ ] **Step 3: Fill in the test bodies** following the existing test fixture pattern in this crate's test module.

- [ ] **Step 4: Run tests to verify they pass**

  Run: `cargo test -p edgezero-adapter-cloudflare provision_local_`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-cloudflare/src/cli.rs
  git commit -m "Cloudflare: per-adapter provision_local_ contract tests"
  ```

### Task 39: Fastly provision_local_* suite

**Files:** `crates/edgezero-adapter-fastly/src/cli.rs` test module

- [ ] **Step 1: Write the same four tests** as Task 38 (Cloudflare), plus the Fastly-specific secret-store assertion: `push_after_provision_preserves_local_server_secret_stores_entry` (hand-edit a `[[local_server.secret_stores.<store_id>]]` entry with a real `env` mapping, run `config push --local`, re-parse, assert unchanged).

- [ ] **Step 2: Run to verify they fail**

  Run: `cargo test -p edgezero-adapter-fastly provision_local_`
  Expected: FAIL.

- [ ] **Step 3: Fill in the bodies; the `zero_cloud_calls` test uses `fake_fastly_panicking()`.**

- [ ] **Step 4: Run to verify pass**

  Run: `cargo test -p edgezero-adapter-fastly provision_local_`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-fastly/src/cli.rs
  git commit -m "Fastly: per-adapter provision_local_ contract tests"
  ```

### Task 40: Spin provision_local_* suite + env-label alignment

**Files:** `crates/edgezero-adapter-spin/src/cli.rs` test module

This is the largest test task — Spin's env-label alignment (spec §"Per-adapter test contract" item 4) is load-bearing.

- [ ] **Step 1: Write the four common tests + the Spin-specific env-label alignment quartet:**

  - `provision_local_writes_expected_env_lines` — after `provision --local`, Spin's `.env` contains `EDGEZERO__STORES__{CONFIG,KV,SECRETS}__<logical_id>__NAME=<platform_name>` for every declared id.
  - `provision_local_labels_line_up` — parse `.env`, `runtime-config.toml`, `spin.toml` via `toml_edit::DocumentMut`; build three sets (env-line `__NAME` values; `[key_value_store.<name>]` block names in runtime-config; `[component.<id>.key_value_stores]` array entries in spin.toml). Assert set equality.
  - `provision_local_env_overlay_round_trips` — set `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config` in the test's process env, re-run provision, assert the alignment assertion against `prod_config`.
  - `re_provision_preserves_operator_uncommented_override` — uncomment + edit `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging`, re-run provision, assert the operator line intact byte-for-byte (validates the key-normalised dedup contract).

- [ ] **Step 2: Run to verify they fail**

  Run: `cargo test -p edgezero-adapter-spin provision_local_ re_provision_preserves`
  Expected: FAIL.

- [ ] **Step 3: Fill in bodies; `zero_cloud_calls` uses `fake_spin_panicking()`.**

- [ ] **Step 4: Run to verify pass**

  Run: `cargo test -p edgezero-adapter-spin provision_local_ re_provision_preserves`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-spin/src/cli.rs
  git commit -m "Spin: per-adapter provision_local_ contract tests + env-label alignment"
  ```

### Task 41: Axum provision_local_* suite

**Files:** `crates/edgezero-adapter-axum/src/cli.rs` test module

- [ ] **Step 1: Write the four common tests, scoped to the no-manifest-mutation contract:**

  - `provision_local_creates_dot_edgezero_dir`
  - `provision_local_does_not_touch_axum_toml` (regression test for the Axum exception)
  - `provision_local_writes_env_name_lines`
  - `re_provision_preserves_operator_env_edits`

- [ ] **Step 2: Run to verify they fail**

  Run: `cargo test -p edgezero-adapter-axum provision_local_`
  Expected: FAIL.

- [ ] **Step 3: Fill bodies.**

- [ ] **Step 4: Run to verify pass**

  Run: `cargo test -p edgezero-adapter-axum provision_local_`
  Expected: PASS.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/edgezero-adapter-axum/src/cli.rs
  git commit -m "Axum: per-adapter provision_local_ contract tests (axum.toml untouched)"
  ```

### Task 42: Adapter contract test sweep (existing `tests/contract.rs`)

**Files:** existing adapter contract tests at `crates/edgezero-adapter-*/tests/contract.rs`

- [ ] **Step 1: Audit existing contract tests** — verify they still pass under the new trait surface (Tasks 3 + 4 changed `provision` return type). If any contract test asserts the OLD `Vec<String>` return shape, update to the new `ProvisionOutcome` shape; if anything asserts `Adapter::deploy` behavior, leave untouched (v1 keeps deploy unchanged).

- [ ] **Step 2: Run the full contract suite**

  Run: `cargo test --workspace --all-targets contract`
  Expected: PASS.

- [ ] **Step 3: Commit any updates**

  ```bash
  git add crates/edgezero-adapter-*/tests/contract.rs
  git commit -m "Update adapter contract tests for new ProvisionOutcome return type"
  ```

### Task 43: Final CI gate run + branch verification

**Files:** none (verification only)

- [ ] **Step 1: Run all five CI gates locally**

  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-targets
  cargo check --workspace --all-targets --features "fastly cloudflare spin"
  cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
  ```

  Expected: every command PASS.

- [ ] **Step 2: Run the docs lint**

  ```bash
  cd docs && npm ci && npx prettier --check '**/*.md' && npx eslint .
  ```

  Expected: PASS.

- [ ] **Step 3: Run every smoke against the worktree**

  ```bash
  ./scripts/smoke_test_config.sh axum
  ./scripts/smoke_test_config.sh cloudflare
  ./scripts/smoke_test_config.sh spin
  ./scripts/smoke_test_kv.sh cloudflare
  ./scripts/smoke_test_kv.sh fastly
  ./scripts/smoke_test_kv.sh spin
  ./scripts/smoke_test_secrets.sh axum
  ./scripts/smoke_test_secrets.sh fastly
  ./scripts/smoke_test_secrets.sh spin
  ./scripts/smoke_test_config_key_override.sh axum
  ./scripts/smoke_test_config_key_override.sh fastly
  ./scripts/smoke_test_config_key_override.sh spin
  ```

  Expected: every smoke PASS.

- [ ] **Step 4: Verify the worktree is clean after smokes**

  Run: `git status --porcelain`
  Expected: empty output. If any files appear, they are by definition gitignored (Cloudflare/Fastly/Spin manifests, `.dev.vars`, `.edgezero/`, etc.). Double-check that nothing slipped past the gitignore.

- [ ] **Step 5: Final commit if anything was touched**

  ```bash
  git status
  # If clean, the plan is complete. If not, commit the residue with
  # a descriptive message.
  ```

---

## Done

After Task 43, the branch satisfies every spec MUST:

- `--local` is wired through CLI, trait, dispatch matrix, dry-run staging, scaffold, run_serve, and four adapters.
- Cloudflare/Fastly/Spin manifests are gitignored generated state; Axum's `axum.toml` stays tracked.
- Path containment is enforced on both `provision --local` and `config push --local`.
- Typed validation is the single `run_typed_preflight` helper called from validate, push, diff, and provision.
- Spin's `.env` carries `__NAME` lines AND lowercased `SPIN_VARIABLE_*` placeholders; runtime-config / spin.toml labels are asserted aligned.
- Cloudflare merge precedence (deployed → existing local → placeholder) and preview-namespace handling (separate `preview_kv_namespaces` map; v1 omits when unknown) are locked.
- `edgezero new` provisions every selected adapter (untyped, because scaffolded secrets ship commented out).
- Smoke scripts warm up via `provision --local` before any push/boot.
- Docs and CI are updated to reflect the new model.
