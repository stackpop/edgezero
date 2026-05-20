# EdgeZero CLI Extensions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `edgezero-cli` into an extensible library, rewrite the manifest store schema and runtime to a multi-store model, add `auth` / `provision` / `config validate` / `config push` commands, and update `app-demo` to exercise it all across axum / cloudflare / fastly / spin.

**Architecture:** One PR, eight sequential commits. Commit 1 extracts the CLI library substrate. Commit 2 is an atomic manifest + runtime rewrite (hard cutoff — no backward compatibility). Commits 3–7 add app-config and the four commands. Commit 8 makes `app-demo` the full-capability showcase and audits docs.

**Tech Stack:** Rust 1.95 (edition 2021), `clap` (derive), `serde` / `toml` / `serde_json`, `validator`, `async-trait` (`?Send`, WASM-safe), `handlebars` (templates), proc-macros (`edgezero-macros`), VitePress docs.

**Spec:** `docs/superpowers/specs/2026-05-19-cli-extensions-design.md` — read it first. Section references (§) below point into it.

---

## Preconditions (do before commit 2)

- [ ] **PR #253 (`feat/spin-store-support`) is merged into the working branch.** The current branch has **no** Spin store support — `crates/edgezero-adapter-spin/src/` has no `config_store.rs` / `key_value_store.rs` / `secret_store.rs`, and `lib.rs` explicitly rejects `[stores.*]` for spin. Commit 2 wires `SpinKvStore` / `SpinConfigStore` / `SpinSecretStore` into the multi-store runtime; they must exist first. Commit 1 does **not** need PR #253. Verify with: `ls crates/edgezero-adapter-spin/src/` shows the three store files before starting commit 2.
- [ ] Working on branch `docs/extensible-cli-library-spec` (or a fresh feature branch off it). The spec lives in `docs/superpowers/`, which is gitignored — keep using `git add -f` for spec/plan files only.

## Codebase facts this plan relies on

- `edgezero-cli` is a binary-only crate today; `main.rs` holds private `handle_*` fns; `cli` feature gates `clap`.
- `ConfigStore::get` is **synchronous** today (`config_store.rs`). `KvStore` is already async. `SecretStore` (`get_bytes`) is async, uses `bytes::Bytes`.
- The KV handle type is `KvHandle`; config is `ConfigStoreHandle`; secrets is `SecretHandle`.
- `RequestContext` exposes `config_store() -> Option<ConfigStoreHandle>`, `kv_handle() -> Option<KvHandle>`, `secret_handle() -> Option<SecretHandle>` — all singular.
- Axum KV is `PersistentKvStore` (redb-backed, `.edgezero/kv.redb`).
- `examples/app-demo` is a **separate workspace**, excluded from the root workspace; CI does not currently build or test it.
- CI: `.github/workflows/test.yml` runs `cargo test --workspace --all-targets`, `cargo check --workspace --all-features`, and per-adapter wasm `--test contract`. `.github/workflows/format.yml` runs `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and ESLint/Prettier on `docs/`.

## File structure (created / modified across the 8 commits)

```
crates/edgezero-cli/
  Cargo.toml                    # M: lib target implicit via src/lib.rs; new deps
  src/lib.rs                    # C (commit 1): public API
  src/main.rs                   # M (commit 1): thin wrapper
  src/args.rs                   # M: standalone *Args structs; commits 4-7 add args
  src/demo_server.rs            # M (commit 1): renamed from dev_server.rs
  src/runner.rs                 # C (commit 5): CommandSpec + CommandRunner
  src/auth.rs                   # C (commit 5)
  src/provision.rs              # C (commit 6)
  src/config.rs                 # C (commit 7): validate + push
  src/generator.rs              # M (commits 1, 3): scaffold <name>-cli, <name>.toml
  src/templates/cli/            # C (commit 1)
  src/templates/app/            # C (commit 3)
  src/templates/root/edgezero.toml.hbs   # M (commit 2): new store schema
  src/templates/core/src/config.rs.hbs   # C (commit 3)
  tests/lib_consumer.rs         # C (commit 1)
crates/edgezero-core/src/
  manifest.rs                   # M (commit 2): store schema rewrite + capability rules
  config_store.rs               # M (commit 2): async trait
  key_value_store.rs            # M (commit 2): KvError::Unsupported + LimitExceeded
  secret_store.rs               # M (commit 2): bound-handle wrapper
  context.rs                    # M (commit 2): id-keyed Bound*Store accessors
  extractor.rs                  # M (commit 2): Kv/Secrets/Config default()/named()
  app.rs                        # M (commit 2): Hooks + id-keyed ConfigStoreMetadata (Hooks lives in app.rs, no separate hooks.rs)
  app_config.rs                 # C (commit 3)
crates/edgezero-macros/src/
  lib.rs                        # M (commit 3): AppConfig derive export
  app_config.rs                 # C (commit 3): derive impl
  app.rs                        # M (commit 2): emit id-keyed metadata
crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/
  {config_store,key_value_store,secret_store}.rs   # M (commit 2): multi-store registries
examples/app-demo/
  Cargo.toml                    # M (commit 1): add app-demo-cli member
  edgezero.toml                 # M (commit 2): new schema
  app-demo.toml                 # C (commit 3)
  crates/app-demo-cli/          # C (commit 1, extended 4-8)
  crates/app-demo-core/src/config.rs       # C (commit 3)
  crates/app-demo-core/src/handlers.rs     # M (commits 2, 8)
docs/guide/                     # M: many pages per §6.12
docs/guide/manifest-store-migration.md   # C (commit 2)
docs/guide/cli-walkthrough.md            # C (commit 8)
docs/.vitepress/config.mts      # M (commits 2, 8): sidebar
```

---

# Commit 1 — Extensible `edgezero-cli` library + generator + `app-demo-cli` skeleton

Spec §7. No PR #253 dependency. Goal: `edgezero-cli` becomes lib + bin; the `demo` subcommand replaces `dev`; the generator scaffolds `<name>-cli`; a handwritten `app-demo-cli` exists.

### Task 1.1: Promote `Command` variant fields into standalone `*Args` structs

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs`

- [ ] **Step 1: Write failing test** in `args.rs` `#[cfg(test)] mod tests` — assert `BuildArgs`, `DeployArgs`, `ServeArgs` exist, are `Default`, and parse:

```rust
#[test]
fn build_args_default_and_mutate() {
    let mut a = BuildArgs::default();
    a.adapter = "fastly".to_string();
    assert_eq!(a.adapter, "fastly");
}
```

- [ ] **Step 2: Run** `cargo test -p edgezero-cli args::tests::build_args_default_and_mutate` — expect FAIL (`BuildArgs` not found).

- [ ] **Step 3: Implement.** Add `#[derive(clap::Args, Debug, Default)] #[non_exhaustive]` structs `BuildArgs { adapter: String, adapter_args: Vec<String> }`, `DeployArgs { adapter: String, adapter_args: Vec<String> }`, `ServeArgs { adapter: String }` carrying the exact `#[arg(...)]` attributes currently inline in the `Command` enum variants. Keep `NewArgs` as-is (already standalone). Rewrite `Command` to: `Build(BuildArgs)`, `Deploy(DeployArgs)`, `Demo`, `New(NewArgs)`, `Serve(ServeArgs)`. Note: `Demo` is the renamed `Dev` (see Task 1.3).

- [ ] **Step 4: Run** `cargo test -p edgezero-cli args::` — expect PASS. Update the existing `parses_build_command_with_passthrough_args` test to destructure `Command::Build(BuildArgs { adapter, adapter_args })`.

- [ ] **Step 5: Commit** is deferred — commit 1 lands as one commit after Task 1.7. Stage progress only.

### Task 1.2: Create `lib.rs`, move handlers, rewrite `main.rs`

**Files:**
- Create: `crates/edgezero-cli/src/lib.rs`
- Modify: `crates/edgezero-cli/src/main.rs`

- [ ] **Step 1:** Create `lib.rs` under `#![cfg(feature = "cli")]`-style gating consistent with the crate. Declare the private modules (`mod adapter; mod args; mod generator; mod scaffold; #[cfg(feature = "edgezero-adapter-axum")] mod demo_server;`). Move `init_cli_logger`, `load_manifest_optional`, `ensure_adapter_defined`, `store_bindings_message`, `log_store_bindings`, and the handler bodies from `main.rs`. Rename `handle_build`→`run_build`, `handle_deploy`→`run_deploy`, `handle_serve`→`run_serve`; add `run_new` wrapping `generator::generate_new`; `run_demo` (Task 1.3). `pub use args::{Args, BuildArgs, Command, DeployArgs, NewArgs, ServeArgs};`. Public signatures: `pub fn run_build(args: &BuildArgs) -> Result<(), String>` etc.

- [ ] **Step 2:** Move the `#[cfg(test)] mod tests` from `main.rs` into `lib.rs` unchanged (they test the moved fns).

- [ ] **Step 3:** Rewrite `main.rs` to ~25 lines: `use edgezero_cli::{...}; fn main() { edgezero_cli::init_cli_logger(); match Args::parse().cmd { Command::Build(a) => exit_on_err(edgezero_cli::run_build(&a)), ... Command::Demo => exit_on_err(edgezero_cli::run_demo()), ... } }`. Keep the `#[cfg(not(feature = "cli"))]` fallback `main`.

- [ ] **Step 4: Run** `cargo test -p edgezero-cli` — expect PASS (all relocated tests green).

- [ ] **Step 5: Run** `cargo build -p edgezero-cli` and `./target/debug/edgezero --help` — expect the same five subcommands (with `demo` instead of `dev`).

### Task 1.3: Rename `dev` → `demo`

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs`, `crates/edgezero-cli/src/main.rs`, `crates/edgezero-cli/src/lib.rs`
- Rename: `crates/edgezero-cli/src/dev_server.rs` → `crates/edgezero-cli/src/demo_server.rs`

- [ ] **Step 1:** `git mv crates/edgezero-cli/src/dev_server.rs crates/edgezero-cli/src/demo_server.rs`. Inside it, rename `pub fn run_dev()` → `pub fn run_demo() -> Result<(), String>` — change the return type: `Ok(())` on graceful shutdown, `Err(String)` on bind failure. Update internal references.

- [ ] **Step 2:** In `args.rs`, the `Command` enum variant is `Demo` (done in Task 1.1). In `lib.rs` declare `#[cfg(feature = "edgezero-adapter-axum")] mod demo_server;` and `pub use demo_server::run_demo;` (feature-gated). Add the non-axum fallback: `run_demo` errors "built without edgezero-adapter-axum".

- [ ] **Step 3:** Update `CLAUDE.md`'s `cargo run -p edgezero-cli --features dev-example -- dev` reference is doc-only — leave the `dev-example` feature name as-is (out of scope) but the invocation becomes `-- demo`. (Doc fix happens in Task 1.7.)

- [ ] **Step 4: Run** `cargo test -p edgezero-cli && cargo build -p edgezero-cli` — expect PASS; `./target/debug/edgezero demo --help` works.

### Task 1.4: Extend the generator to scaffold `<name>-cli`

**Files:**
- Modify: `crates/edgezero-cli/src/generator.rs`, `crates/edgezero-cli/src/scaffold.rs`
- Create: `crates/edgezero-cli/src/templates/cli/Cargo.toml.hbs`, `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`
- Modify: `crates/edgezero-cli/src/templates/root/Cargo.toml.hbs`

- [ ] **Step 1: Write failing test** in `generator.rs` tests: `generate_new` into a `tempfile::TempDir` produces `crates/<name>-cli/Cargo.toml` and `crates/<name>-cli/src/main.rs`, and the root `Cargo.toml` `members` list contains `crates/<name>-cli`.

- [ ] **Step 2: Run** the test — expect FAIL.

- [ ] **Step 3: Implement.** Add `templates/cli/Cargo.toml.hbs` (package `{{name}}-cli`, depends on `edgezero-cli` with default features, `clap` derive, `log`). Add `templates/cli/src/main.rs.hbs` — the canonical downstream pattern: a `clap::Parser` `Args` with a `Cmd` `Subcommand` enum listing all five built-ins (`Build(BuildArgs)`, `Deploy(DeployArgs)`, `Demo`, `New(NewArgs)`, `Serve(ServeArgs)`), `main` dispatching to `edgezero_cli::run_*`. Register the new templates in `scaffold.rs::register_templates`. In `generator.rs`, render the cli crate and append `crates/{{name}}-cli` to the root `Cargo.toml` members.

- [ ] **Step 4: Run** the generator test — expect PASS.

- [ ] **Step 5: Manual check:** generate into an explicit fresh temp dir and build it — do **not** assume the project lands in CWD. Example:

```bash
TMP="$(mktemp -d)"
cargo run -p edgezero-cli -- new throwaway --dir "$TMP"
# cd into the generated project root (confirm the exact path the generator
# prints — `--dir` is "the directory to create the app in"):
cd "$TMP"/* 2>/dev/null || cd "$TMP"
cargo check --workspace
cd - && rm -rf "$TMP"
```

Expected: `cargo check --workspace` in the generated project succeeds.

### Task 1.5: Add the handwritten `app-demo-cli` crate

**Files:**
- Create: `examples/app-demo/crates/app-demo-cli/Cargo.toml`, `examples/app-demo/crates/app-demo-cli/src/main.rs`, `examples/app-demo/crates/app-demo-cli/tests/help.rs`
- Modify: `examples/app-demo/Cargo.toml`

- [ ] **Step 1:** Add `"crates/app-demo-cli"` to `examples/app-demo/Cargo.toml` `members`. Add `edgezero-cli = { path = "../../../../crates/edgezero-cli" }` to that workspace's `[workspace.dependencies]`.

- [ ] **Step 2:** Write `app-demo-cli/Cargo.toml` — `name = "app-demo-cli"`, `publish = false`, `[lints] workspace = true`, deps `edgezero-cli = { workspace = true }`, `clap = { version = "4", features = ["derive"] }`, `log = { workspace = true }`.

- [ ] **Step 3:** Write `app-demo-cli/src/main.rs` mirroring the generated `templates/cli/src/main.rs.hbs` pattern — all five built-ins, no custom subcommands yet. `#[command(name = "app-demo-cli", about = "app-demo edge CLI")]`.

- [ ] **Step 4:** Write `tests/help.rs`: `Args::try_parse_from(["app-demo-cli", "--help"])` returns the clap help error (not a panic). Since `Args` is private to `main.rs`, instead spawn the built binary: `assert_cmd`-style or `std::process::Command::new(env!("CARGO_BIN_EXE_app-demo-cli")).arg("--help")` exits 0 and stdout contains `build`, `deploy`, `demo`, `new`, `serve`.

- [ ] **Step 5: Run** `cd examples/app-demo && cargo test -p app-demo-cli` — expect PASS.

### Task 1.6: External-consumer integration test

**Files:**
- Create: `crates/edgezero-cli/tests/lib_consumer.rs`

- [ ] **Step 1: Write the test:** `use edgezero_cli::{BuildArgs, run_build};` — construct `let mut a = BuildArgs::default(); a.adapter = "fastly".into();`, write a minimal `edgezero.toml` into a `tempfile::TempDir`, set `EDGEZERO_MANIFEST`, call `run_build(&a)`, assert `Ok` (mirror the existing `handle_build_executes_manifest_command` test's manifest fixture).

  **Env-mutation guard (required).** `EDGEZERO_MANIFEST` is process-global; concurrent tests mutating it flake. Two rules: (a) restore the variable with an RAII guard — copy the `EnvOverride` struct from `edgezero-cli`'s existing `main.rs`/`lib.rs` tests (it saves the prior value in `new` and restores it in `Drop`); (b) keep `tests/lib_consumer.rs` to **exactly one** `#[test]`, so there is no in-binary parallelism on the env var. If a second env-touching test is ever added to this file, gate both with a shared `std::sync::Mutex` guard (the same `manifest_guard()` pattern the crate's unit tests use) — do not rely on `--test-threads=1`.

- [ ] **Step 2: Run** `cargo test -p edgezero-cli --test lib_consumer` — expect PASS. This proves the public API is usable from outside the crate.

### Task 1.7: Commit-1 documentation + commit

**Files:**
- Modify: `docs/guide/cli-reference.md`, `docs/guide/getting-started.md`, `CLAUDE.md`

- [ ] **Step 1:** In `cli-reference.md` rename `dev` → `demo` and add a short "Building your own CLI" section pointing at the `edgezero-cli` library + the `<name>-cli` scaffold. In `getting-started.md` note that `edgezero new` now also scaffolds `<name>-cli`. In `CLAUDE.md` change the `dev` invocation example to `demo`.

- [ ] **Step 2: Run** the full gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-targets`, and `cd examples/app-demo && cargo test`. All green.

- [ ] **Step 3: Commit:**

```bash
git add crates/edgezero-cli examples/app-demo docs/guide/cli-reference.md docs/guide/getting-started.md CLAUDE.md
git commit -m "Extensible edgezero-cli library + generator + app-demo-cli; rename dev->demo"
```

---

# Commit 2 — Manifest + runtime rewrite (atomic, all four adapters)

Spec §8, §6.6, §6.7, §6.9. **Requires PR #253.** This is the largest commit and the review hotspot. Hard cutoff — legacy store schema is removed outright.

### Task 2.1: Rewrite the manifest store schema

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs`

- [ ] **Step 1: Write failing tests** for the new schema in `manifest.rs` tests: a manifest with `[stores.kv] ids = ["a","b"]\ndefault = "a"` plus `[adapters.cloudflare.stores.kv.a] name = "A"` etc. parses; `ids = []` errors; `default` missing with two ids errors; `default` not in `ids` errors; a `Single`-pair per-id block errors; a legacy `[stores.kv] name = "X"` errors with a message containing `manifest-store-migration`.

- [ ] **Step 2: Run** — expect FAIL.

- [ ] **Step 3: Implement** per §6.6. Replace `ManifestStores`, `ManifestConfigStoreConfig`, `ManifestKvConfig`, `ManifestSecretsConfig`, and the `Manifest*AdapterConfig` types with:
  - `ManifestStores { kv: Option<LogicalStore>, config: Option<LogicalStore>, secrets: Option<LogicalStore> }` where `LogicalStore { ids: Vec<String>, default: Option<String> }`.
  - `ManifestAdapter` (the `[adapters.<x>]` struct) gains `stores: Option<AdapterStoresConfig>`. `AdapterStoresConfig { kv/config/secrets: BTreeMap<String /*id*/, AdapterStoreMapping> }`, `AdapterStoreMapping { name: String, #[serde(flatten)] extras: BTreeMap<String, toml::Value> }`.
  - The Spin `component` field goes on the **`[adapters.<x>.adapter]` definition struct** — the one that already carries `crate` and `manifest` — **not** on the top-level `ManifestAdapter`. Adding it to `ManifestAdapter` would make the accepted TOML `[adapters.spin] component = "..."`, which is wrong; it must be `[adapters.spin.adapter] component = "..."` (§6.7). Confirm the struct name by reading `manifest.rs` (the struct deserialized from `[adapters.<x>.adapter]`); add `component: Option<String>` there.
  - A `Capability { Multi, Single }` and a const fn `capability(adapter: &str, kind: StoreKind) -> Capability` encoding the §6.6 matrix.
  - Validation in `ManifestLoader`: non-empty `ids`; `default` rules; capability check (any `Single` adapter for a kind ⇒ `ids.len() == 1`); per-id mapping required for `Multi` pairs / forbidden for `Single` pairs; Cloudflare `name` JS-identifier check; Spin KV label check.
  - Detect legacy keys (`name`/`enabled`/`defaults`/`adapters` under `[stores.*]`) via a `#[serde(deny_unknown_fields)]` or an explicit reject, emitting an error pointing at `docs/guide/manifest-store-migration.md`.
  - Add resolver helpers: `resolved_default(kind) -> &str`, `store_name(adapter, kind, id) -> Option<&str>`.

- [ ] **Step 4: Run** `cargo test -p edgezero-core manifest` — expect PASS. Existing manifest tests that used the old schema are rewritten to the new schema (this is a hard cutoff — old-schema tests are replaced, not kept).

### Task 2.2: New `KvError` variants

**Files:**
- Modify: `crates/edgezero-core/src/key_value_store.rs`

- [ ] **Step 1: Write failing test:** assert `KvError::Unsupported` and `KvError::LimitExceeded` exist and that their `EdgeError` conversion yields a 5xx status.

- [ ] **Step 2: Run** — expect FAIL.

- [ ] **Step 3: Implement.** Add `Unsupported { message: String }` and `LimitExceeded { message: String }` to `KvError`. Map both to a 5xx-class `EdgeError` in the existing `KvError → EdgeError` conversion (an unsupported op / a store-too-large condition is not a client error).

- [ ] **Step 4: Run** — expect PASS.

### Task 2.3: Make `ConfigStore` async

**Files:**
- Modify: `crates/edgezero-core/src/config_store.rs`, and every `ConfigStore` impl (all four adapters + any in-core test stores)

- [ ] **Step 1: Implement.** Change the trait to `#[async_trait(?Send)] pub trait ConfigStore: Send + Sync { async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError>; }`. Make `ConfigStoreHandle::get` async. Update the `config_store_contract_tests!` macro so generated tests `.await` the calls (they already run under `futures::executor::block_on` per project convention).

- [ ] **Step 2:** Update every `ConfigStore` impl in the four adapters to `async fn get` (the bodies stay; only the signature + any awaits change). This is mechanical but compile-driven — `cargo build` will list every site.

- [ ] **Step 3: Run** `cargo build --workspace` — drive to zero errors.

### Task 2.4: Bound store handles + id-keyed `RequestContext` + `StoreRegistry`

**Files:**
- Modify: `crates/edgezero-core/src/context.rs`, `config_store.rs`, `key_value_store.rs`, `secret_store.rs`

- [ ] **Step 1: Implement** per §4. Add `BoundKvStore`, `BoundConfigStore`, `BoundSecretStore` — each wraps the provider handle plus the resolved platform name; `BoundConfigStore::get` async; `BoundSecretStore::get -> Result<Option<bytes::Bytes>, SecretError>` + `require_str`. Add `StoreRegistry<H> { by_id: BTreeMap<String, H>, default_id: String }`. Replace `RequestContext::config_store()/kv_handle()/secret_handle()` with `kv_store(id)/kv_store_default()`, `config_store(id)/config_store_default()`, `secret_store(id)/secret_store_default()` returning `Option<Bound*Store>`. The context stores three `StoreRegistry` values in its `Extensions`.

- [ ] **Step 2: Write tests** in `context.rs`: a registry with two ids returns `Some` for each, `None` for an unknown id; `*_default()` resolves the `default_id`.

- [ ] **Step 3: Run** `cargo test -p edgezero-core context` — expect PASS.

### Task 2.5: Id-keyed `Hooks` / `ConfigStoreMetadata` + `app!` macro

**Files:**
- Modify: `crates/edgezero-core/src/app.rs` (`Hooks` + `ConfigStoreMetadata` both live here — there is no separate `hooks.rs`), `crates/edgezero-macros/src/app.rs`, `crates/edgezero-macros/src/manifest_definitions.rs`

- [ ] **Step 1: Implement.** `ConfigStoreMetadata` becomes a registry: one entry per logical config id, each carrying the per-adapter `name` map. `Hooks` exposes store **metadata** (ids, resolved default, per-adapter names) per kind — **not** bound handles. Update the `app!` macro to emit the id-keyed metadata from the new manifest schema (`manifest_definitions.rs` is where the macro reads the manifest).

- [ ] **Step 2: Write a macro test:** the generated `ConfigStoreMetadata` registry matches a fixture manifest's `[stores.config].ids`.

- [ ] **Step 3: Run** `cargo test -p edgezero-core && cargo test -p edgezero-macros` — expect PASS.

### Task 2.6: Refactor `Kv` / `Secrets` extractors + add `Config`

**Files:**
- Modify: `crates/edgezero-core/src/extractor.rs`

- [ ] **Step 1: Implement** per §6.9. `Kv` / `Secrets` / new `Config` each become a per-request registry handle with `.default() -> Option<Bound*Store>` and `.named(id) -> Option<Bound*Store>`. Update their `FromRequest` impls to extract the corresponding `StoreRegistry` from the context.

- [ ] **Step 2: Write tests:** a handler-style test resolving `kv.default()` and `kv.named("sessions")`.

- [ ] **Step 3: Run** `cargo test -p edgezero-core extractor` — expect PASS.

### Task 2.7: Rewrite all four adapter store impls for multi-store

**Files:**
- Modify: `crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/{config_store,key_value_store,secret_store}.rs` and each adapter's request-setup code.

- [ ] **Step 1: axum.** Build `StoreRegistry` for each kind from `[adapters.axum.stores.*]`. KV stays `PersistentKvStore` (redb) — **one separate redb file per logical id**, file stem from the per-adapter mapping `[adapters.axum.stores.kv.<id>].name`: `.edgezero/kv-<name>.redb`. (Axum KV is `Multi`, so every id has a `name`.) Distinct files prevent multi-store collapsing into one backing file. Config store reads `.edgezero/local-config-<id>.json` (the file commit 7 writes); absent ⇒ empty. Secrets from env vars (Single).

- [ ] **Step 2: cloudflare.** KV registry. **Config rewritten from `[vars]` to KV** (§6.9) — `CloudflareConfigStore` does an async `env.<NAMESPACE>.get(key)`; one namespace per config id. Secrets from worker secrets (Single).

- [ ] **Step 3: fastly.** KV / config / secret store registries (all `Multi`).

- [ ] **Step 4: spin.** Wire `SpinKvStore` (label registry, honor `max_list_keys`, return `KvError::LimitExceeded` past the cap, `KvError::Unsupported` for TTL writes), `SpinConfigStore` (single flat-variable store, `.`→`__` lowercase key translation), `SpinSecretStore` (single flat-variable store, `store_name` ignored). Stop rejecting `[stores.*]` for spin in `lib.rs`. Labels come from `[adapters.spin.stores.kv.*].name`.

- [ ] **Step 5:** Update each adapter's contract-test invocations to the id-keyed factory shape; add a Spin TTL→`Unsupported` contract test and a Spin listing-cap→`LimitExceeded` test; add a Cloudflare config-from-KV async round-trip test (wasm-bindgen-test).

- [ ] **Step 6: Run** `cargo test --workspace --all-targets`, then the per-adapter wasm contract tests with the **exact** runner / target / feature each adapter's CI job uses (`.github/workflows/test.yml` `adapter-wasm-tests` matrix — match it, do not improvise):
  - **cloudflare:** target `wasm32-unknown-unknown`, runner `wasm-bindgen-test-runner` —
    `cargo test -p edgezero-adapter-cloudflare --target wasm32-unknown-unknown --features cloudflare --test contract`
  - **fastly:** target `wasm32-wasip1`, runner Viceroy (version pinned in `.tool-versions`) —
    `cargo test -p edgezero-adapter-fastly --target wasm32-wasip1 --features fastly --test contract`
  - **spin:** target `wasm32-wasip1`, runner Wasmtime —
    `cargo test -p edgezero-adapter-spin --target wasm32-wasip1 --features spin --test contract`

  The runner for each target is configured in the workspace `.cargo/config.toml`. If the exact feature flags or runner config differ from the above, defer to `.github/workflows/test.yml` as the source of truth and update this step to match. All green.

### Task 2.8: Migrate `app-demo` + write the migration guide

**Files:**
- Modify: `examples/app-demo/edgezero.toml`, `examples/app-demo/crates/app-demo-core/src/handlers.rs`, `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Create: `docs/guide/manifest-store-migration.md`

- [ ] **Step 1:** Rewrite `examples/app-demo/edgezero.toml` to the new schema: `[stores.kv] ids = ["sessions","cache"]\ndefault = "sessions"`; one config id (`app_config`); one secrets id (`default`); per-adapter `[adapters.<X>.stores.kv.<id>]` blocks for axum/cloudflare/fastly/spin; no Spin per-id blocks for config/secrets (Single). Remove `[stores.config.defaults]`.

- [ ] **Step 2:** Migrate `app-demo` handlers to id-keyed accessors — **store-accessor change only** (`ctx.kv_store("sessions")`, `ctx.config_store_default()`, the refactored `Kv`/`Secrets`/`Config` extractors). Do **not** introduce `AppDemoConfig` here (commit 3).

- [ ] **Step 3:** Rewrite `templates/root/edgezero.toml.hbs` to the new schema so `edgezero new` produces a valid manifest.

- [ ] **Step 4:** Write `docs/guide/manifest-store-migration.md` — old shape → new shape, worked example, the capability matrix.

- [ ] **Step 5: Run** `cd examples/app-demo && cargo test && cargo build --workspace` — green.

### Task 2.9: Commit-2 docs + commit

**Files:**
- Modify: `docs/guide/configuration.md`, `kv.md`, `handlers.md`, `adapters/cloudflare.md`, `adapters/overview.md`, `architecture.md`, `docs/.vitepress/config.mts`

- [ ] **Step 1:** Update each page per §6.12 — new `[stores]` schema + capability rules + the removal of `[stores.config.defaults]` (`configuration.md`); multi-store + bound handles + extractor `default()/named()` (`kv.md`, `handlers.md`); `[vars]`→KV config (`adapters/cloudflare.md`); Spin store semantics (`adapters/overview.md`); light review (`architecture.md`). Add `manifest-store-migration.md` to the sidebar in `config.mts`.

- [ ] **Step 2: Run** the full gate (all of `.github/workflows/test.yml` + `format.yml` commands, including the docs ESLint/Prettier and the wasm gates) — green.

- [ ] **Step 3: Commit:** `git commit -m "Manifest + runtime rewrite: multi-store schema, async ConfigStore, all four adapters"`

---

# Commit 3 — App-config schema, derive macro, env-overlay loader

Spec §9, §6.7, §6.8, §6.10.

### Task 3.1: `edgezero-core::app_config` module

**Files:**
- Create: `crates/edgezero-core/src/app_config.rs`; Modify: `crates/edgezero-core/src/lib.rs`

- [ ] **Step 1: Write failing tests:** valid `<name>.toml` loads; missing file, bad TOML, missing `[config]` table, validator failure each produce a distinct `AppConfigError`.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** per §4. Types: `AppConfigMeta` trait with `const SECRET_FIELDS: &'static [SecretField]`; `SecretField { name, kind }`; `SecretKind { KeyInDefault, StoreRef }`; `AppConfigError`; `AppConfigLoadOptions { env_overlay: bool }` with `Default` = `{ env_overlay: true }`.

  Loader API — **one consistent shape, no hidden bool param.** The simple functions apply the env overlay (the default); the `_with_options` variants take `AppConfigLoadOptions` explicitly:
  - `load_app_config<C>(path, app_name) -> Result<C, AppConfigError>` — overlay on.
  - `load_app_config_with_options<C>(path, app_name, opts: &AppConfigLoadOptions) -> Result<C, AppConfigError>`.
  - `load_app_config_raw(path, app_name) -> Result<toml::Value, AppConfigError>` — overlay on.
  - `load_app_config_raw_with_options(path, app_name, opts: &AppConfigLoadOptions) -> Result<toml::Value, AppConfigError>`.

  The simple functions delegate to the `_with_options` form with `AppConfigLoadOptions::default()`. `--no-env` (Tasks 4.1 / 7.1) calls the `_with_options` variant with `env_overlay: false`. `load_app_config*` parses the `[config]` table, applies the env overlay when `opts.env_overlay`, then (typed) deserializes + `validate()`. `pub mod app_config;` in `lib.rs`.

- [ ] **Step 4: Run** — PASS.

### Task 3.2: `AppConfig` derive macro

**Files:**
- Create: `crates/edgezero-macros/src/app_config.rs`; Modify: `crates/edgezero-macros/src/lib.rs`

- [ ] **Step 1a: Add the `trybuild` dev-dependency.** Compile-fail tests need `trybuild`; `crates/edgezero-macros/Cargo.toml` currently has only `tempfile` under `[dev-dependencies]`. Add `trybuild = "1"` to `[dev-dependencies]` there (and to `[workspace.dependencies]` in the root `Cargo.toml` if the workspace pins dev-deps centrally — check first and follow the existing convention).

- [ ] **Step 1b: Write macro tests** in `crates/edgezero-macros/tests/app_config_derive.rs`: empty `SECRET_FIELDS` with no annotation; one `KeyInDefault` from `#[secret]`; one `StoreRef` from `#[secret(store_ref)]`; both kinds. Add a `trybuild` compile-fail harness — `let t = trybuild::TestCases::new(); t.compile_fail("tests/ui/*.rs");` — with one `tests/ui/*.rs` fixture per rejected case: `#[secret]` + `#[serde(flatten)]`, `#[secret]` + `#[serde(rename)]`, `#[secret(bogus)]`, `#[secret]` on a non-scalar field. Each fixture has a matching `.stderr` golden file (generate with `TRYBUILD=overwrite` once the `compile_error!` messages are final).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement.** `#[proc_macro_derive(AppConfig, attributes(secret))]` in `lib.rs` delegating to `app_config::derive`. The impl scans fields for `#[secret]` / `#[secret(store_ref)]`, enforces the §6.7 constraints with `compile_error!`, and emits `impl ::edgezero_core::app_config::AppConfigMeta` with the `SECRET_FIELDS` array (Rust field name verbatim).

- [ ] **Step 4: Run** — PASS.

### Task 3.3: Env-overlay resolution

**Files:**
- Modify: `crates/edgezero-core/src/app_config.rs`

- [ ] **Step 1: Write tests:** `APP_DEMO__GREETING` overrides a top-level key; `APP_DEMO__SERVICE__TIMEOUT_MS` overrides a nested key; type coercion against the existing TOML value; a non-parseable value errors; two sibling keys mapping to the same env segment errors; `load_app_config_with_options` with `AppConfigLoadOptions { env_overlay: false }` skips the overlay entirely.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** per §6.10: walk the parsed `[config]` tree; for each existing key compute `<APP_NAME>__<SECTION>__…__<KEY>` (uppercase, `-`→`_`, `__` separators); look up the env var; coerce to the existing value's type; reject ambiguous sibling mappings.

- [ ] **Step 4: Run** — PASS.

### Task 3.4: Generator templates for app-config

**Files:**
- Create: `crates/edgezero-cli/src/templates/app/<name>.toml.hbs`, `crates/edgezero-cli/src/templates/core/src/config.rs.hbs`
- Modify: `crates/edgezero-cli/src/generator.rs`, `scaffold.rs`

- [ ] **Step 1:** `app/<name>.toml.hbs` — a `[config]` table with `greeting` and a nested `[config.service]` section. `core/src/config.rs.hbs` — `<NameUpperCamel>Config` with `#[derive(Deserialize, Serialize, Validate, AppConfig)]` + `#[serde(deny_unknown_fields)]`, a `greeting` field, a nested `service` field, **one plain `#[secret]` field**, and a commented-out `#[secret(store_ref)]` example (§6.8 — the generated template does not include `store_ref` live).

- [ ] **Step 2:** Render both in `generate_new`; register in `scaffold.rs`.

- [ ] **Step 3: Write/extend the generator test** to assert `<name>.toml` and `<name>-core/src/config.rs` are produced.

- [ ] **Step 4: Run** the generator test — PASS.

### Task 3.5: `app-demo` app-config + commit

**Files:**
- Create: `examples/app-demo/app-demo.toml`, `examples/app-demo/crates/app-demo-core/src/config.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`, `docs/guide/configuration.md`, `getting-started.md`

- [ ] **Step 1:** Write `app-demo.toml` — `[config]` with `greeting`, `feature_new_checkout`, a `[config.service]` with `timeout_ms`, `api_token` (a `#[secret]` value), `vault` (a `#[secret(store_ref)]` value = the single secrets id). Write `app-demo-core/src/config.rs` — `AppDemoConfig` with the §6.8 shape (nested `ServiceConfig`, one `#[secret]`, one `#[secret(store_ref)]`). Export it from `lib.rs`.

- [ ] **Step 2: Write a round-trip test** in `app-demo-core`: `load_app_config::<AppDemoConfig>` against `app-demo.toml` succeeds; `AppDemoConfig::SECRET_FIELDS` has the expected two entries; an env var overrides the nested value.

- [ ] **Step 3:** Update `configuration.md` (app-config file + env overlay) and `getting-started.md` (generator now emits `<name>.toml`).

- [ ] **Step 4: Run** the full gate. **Commit:** `git commit -m "App-config schema, #[derive(AppConfig)] macro, env-overlay loader"`

---

# Commit 4 — `config validate` command

Spec §10. New: `ConfigValidateArgs`, `run_config_validate`, `run_config_validate_typed`.

### Task 4.1: `config validate` implementation

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs` (add `ConfigValidateArgs` + a `ConfigCmd` subcommand enum), `crates/edgezero-cli/src/lib.rs`
- Create: `crates/edgezero-cli/src/config.rs`

- [ ] **Step 1: Write failing tests** with fixtures for each failure mode (§10): valid passes; bad TOML; missing `[config]`; unknown field (struct with `deny_unknown_fields`); type mismatch; validator-rule failure; empty `#[secret]`; `#[secret(store_ref)]` value not in `[stores.secrets].ids`; missing per-adapter mapping; the three Spin checks (key syntax, collision — typed-only, component discovery).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ConfigValidateArgs { manifest, app_config, strict, no_env }` (`#[derive(clap::Args, Default, Debug)] #[non_exhaustive]`). `run_config_validate` (raw) and `run_config_validate_typed<C: DeserializeOwned + Validate + AppConfigMeta>` in `config.rs`. Raw does TOML + manifest checks + Spin key-syntax + component discovery; typed adds deserialize + `validate()` + secret checks + the collision check. Both run manifest `ManifestLoader` validation; `--strict` adds capability completeness + handler-path checks.

- [ ] **Step 4: Run** — PASS.

### Task 4.2: Wire `app-demo-cli config validate` + docs + commit

**Files:**
- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs`, `docs/guide/cli-reference.md`

- [ ] **Step 1:** Add a `Config(ConfigCmd)` arm to `app-demo-cli`'s `Cmd` enum with a `ConfigCmd { Validate(ConfigValidateArgs) }` (push added in commit 7). Dispatch `Validate` to `edgezero_cli::run_config_validate_typed::<AppDemoConfig>`.

- [ ] **Step 2:** Document `config validate` in `cli-reference.md`.

- [ ] **Step 3: Run** the full gate; `cd examples/app-demo && cargo run -p app-demo-cli -- config validate --strict` exits 0. **Commit:** `git commit -m "config validate command (raw + typed)"`

---

# Commit 5 — `auth` command (+ `CommandRunner`)

Spec §11, §6.1.

### Task 5.1: `CommandRunner` infrastructure

**Files:**
- Create: `crates/edgezero-cli/src/runner.rs`; Modify: `lib.rs`

- [ ] **Step 1: Write a test** using `MockCommandRunner` — assert a recorded `CommandSpec` matches `{ program: "echo", args: ["hi"], cwd: None, ... }`.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** per §6.1: private `CommandSpec<'a>`, `CommandRunner` trait, `CommandOutput`, `RealCommandRunner` (`std::process::Command`), `#[cfg(test)] MockCommandRunner`.

- [ ] **Step 4: Run** — PASS.

### Task 5.2: `auth` command + docs + commit

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs` (`AuthArgs`, `AuthSub`), `lib.rs`
- Create: `crates/edgezero-cli/src/auth.rs`
- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs`, `docs/guide/cli-reference.md`

- [ ] **Step 1: Write tests:** for each (adapter, sub) pair a `MockCommandRunner` expectation asserting the exact `CommandSpec` (per the §11 table); tool-not-found and non-zero-exit cases.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement.** `AuthArgs { sub: AuthSub }` — `#[derive(clap::Args, Debug)] #[non_exhaustive]`, **no `Default`** (§6.11). `AuthSub { Login{adapter}, Logout{adapter}, Status{adapter} }`. `run_auth` → `run_auth_with(&RealCommandRunner, args)` dispatching per the §11 table. Add `Auth(AuthArgs)` to `app-demo-cli`'s `Cmd`.

- [ ] **Step 4: Run** — PASS. Document `auth` in `cli-reference.md`.

- [ ] **Step 5: Run** the full gate. **Commit:** `git commit -m "auth command + CommandRunner infrastructure"`

---

# Commit 6 — `provision` command

Spec §12, §13 (Fastly contract).

### Task 6.1: `provision` implementation

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs` (`ProvisionArgs`), `lib.rs`
- Create: `crates/edgezero-cli/src/provision.rs`

- [ ] **Step 1: Write tests:** per-(adapter, kind) `MockCommandRunner` expectations with scripted stdout; golden ID-extraction parsers; temp-fixture writeback verified for `wrangler.toml`, `fastly.toml`, and the Spin `key_value_stores` array in `spin.toml`; axum no-op output asserted; `--dry-run` invokes nothing.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ProvisionArgs { manifest, adapter, dry_run }`. `run_provision` per the §12 per-adapter table: axum no-op; cloudflare `wrangler kv namespace create` + `wrangler.toml` `[[kv_namespaces]]` writeback; fastly `fastly <kind>-store create` + `[setup.*]`/`[local_server.*]` `fastly.toml` writeback; spin KV-label `spin.toml` writeback only (component resolved per §6.7).

- [ ] **Step 4: Run** — PASS. Add `Provision(ProvisionArgs)` to `app-demo-cli`'s `Cmd`. Document `provision` in `cli-reference.md`.

- [ ] **Step 5: Run** the full gate. **Commit:** `git commit -m "provision command (cloudflare/fastly/spin writeback, axum no-op)"`

---

# Commit 7 — `config push` command

Spec §13, §6.4, §6.5.

### Task 7.1: `config push` implementation

**Files:**
- Modify: `crates/edgezero-cli/src/args.rs` (`ConfigPushArgs`, extend `ConfigCmd`), `lib.rs`, `crates/edgezero-cli/src/config.rs`

- [ ] **Step 1: Write tests:** typed + raw; per-adapter mock-runner/fixture with golden payloads; secret fields absent; missing native-manifest id (cloudflare) → clear error; Spin `.`→`__` translation; Spin writes both `spin.toml` tables; Spin component-resolution failure errors; `--store` selection; `--dry-run` invokes nothing; the §13 "validate passes, push serialization fails" cases; the Spin `spin.toml` golden test (strongest-first validation ladder, §13).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ConfigPushArgs { manifest, adapter, store, app_config, no_env, dry_run }`. `run_config_push` / `run_config_push_typed<C: ... + Serialize>`: strict pre-flight validation, load app-config, flatten + serialize per §6.4/§6.5 (skip `SECRET_FIELDS`), resolve target id, push per the §13 per-adapter table (axum local JSON file; cloudflare `wrangler kv bulk put`; fastly `config-store-entry create`; spin both `spin.toml` tables).

- [ ] **Step 4: Run** — PASS.

### Task 7.2: Wire `app-demo-cli config push` + docs + commit

**Files:**
- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs`, `docs/guide/cli-reference.md`, `configuration.md`

- [ ] **Step 1:** Extend `ConfigCmd` with `Push(ConfigPushArgs)`; dispatch to `run_config_push_typed::<AppDemoConfig>`.

- [ ] **Step 2:** Document `config push` in `cli-reference.md`; cross-reference from `configuration.md`.

- [ ] **Step 3: Run** the full gate. **Commit:** `git commit -m "config push command (per-adapter, secret-skipping, env overlay)"`

---

# Commit 8 — `app-demo` integration polish + docs audit

Spec §15, §6.12.

### Task 8.1: Full `app-demo` capability exercise

**Files:**
- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs`, `examples/app-demo/crates/app-demo-core/src/handlers.rs`, `examples/app-demo/edgezero.toml`, `examples/app-demo/app-demo.toml`, `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`

- [ ] **Step 1:** Confirm `app-demo-cli`'s `Cmd` has all five built-ins + `Auth` + `Provision` + `Config(Validate|Push)`. Ensure handlers exercise: two named KV ids (`sessions`, `cache`) via `Kv::named`; async `config_store_default().get("greeting")`; the nested `service.timeout_ms`; both secret forms. Add the manual Spin secret-variable declarations to `app-demo-adapter-spin/spin.toml` (`secret = true`, bound under `[component.<component>.variables]`).

- [ ] **Step 2: Write integration tests** in `app-demo`: `config validate --strict` exits 0; `config push --adapter axum` writes `.edgezero/local-config-app_config.json` and a running demo server returns `greeting` on `/config/greeting`; `config push --adapter spin --dry-run` **prints** the would-be `__`-encoded keys and the would-be content of both `spin.toml` tables — and the test asserts the on-disk `spin.toml` is **unchanged** (dry-run never mutates); an env-override test asserts `APP_DEMO__SERVICE__TIMEOUT_MS` takes effect.

  **Demo-server lifecycle (required, to keep the e2e test non-flaky):**
  - **Port:** do not hard-code `8787`. Bind an ephemeral port — either bind `127.0.0.1:0` and read back the assigned port, or pick a free port in the test and pass it to the server. Concurrent CI jobs must not collide.
  - **Readiness:** after spawning the server, poll `GET /` (or a health route) with a short retry loop — e.g. up to ~50 attempts, 100ms apart (~5s budget) — and only proceed once a request succeeds. Never use a bare `sleep`.
  - **Teardown:** spawn the server as a child process and kill it in an RAII guard (a struct that holds the `Child` and calls `.kill()` + `.wait()` in `Drop`), so it is reaped even when an assertion fails or panics. Also clean up the `.edgezero/local-config-*.json` files the test wrote.

- [ ] **Step 3: Run** `cd examples/app-demo && cargo test` — PASS.

### Task 8.2: CI wiring for the `app-demo` loop

**Files:**
- Modify: `.github/workflows/test.yml` (or `scripts/run_tests.sh`)

- [ ] **Step 1:** CI does not currently build `app-demo`. Add a job/step that runs `cd examples/app-demo && cargo test`. Prefer expressing the end-to-end axum loop **as a Rust integration test inside `app-demo`** (the Task 8.1 `app-demo` integration test) rather than as raw shell in the workflow — the Rust test already owns ephemeral-port binding, the readiness poll, and RAII teardown (Task 8.1 step 2). The CI job then just needs `cargo test`; it does not hand-roll `start server / curl / kill` in YAML, which is where shell-based e2e steps go flaky. Keep this job off the wasm matrix — axum only, no live external calls.

- [ ] **Step 2:** If any loop step must stay as a shell step in the workflow (e.g. invoking the built `app-demo-cli` binary), it must still: select a free port (not a hard-coded one), poll readiness before curl-ing, and `kill` the server in a `trap`/`always()` cleanup so a failed assertion never leaves an orphan process. Mirror the Task 8.1 lifecycle rules.

- [ ] **Step 3: Run** the workflow logic locally to confirm the loop passes and leaves no orphan processes or `.edgezero/` artifacts.

### Task 8.3: Walkthrough doc + documentation audit + commit

**Files:**
- Create: `docs/guide/cli-walkthrough.md`; Modify: `docs/.vitepress/config.mts`, any pages still stale

- [ ] **Step 1:** Write `docs/guide/cli-walkthrough.md` — the full `myapp` loop (`new`, `auth`, `provision`, `config validate`, `config push`, `deploy`, `demo`), an env-override example, all four adapters, the manual Spin secret-variable `spin.toml` entries, the explicit `[adapters.spin.adapter].component` form. Add it + `manifest-store-migration.md` to the `config.mts` sidebar.

- [ ] **Step 2: Documentation audit** (§6.12): `grep -rn` the `docs/` tree for stale references — old `[stores.*]` keys (`stores.config.defaults`, `[stores.kv] name`), the `dev` subcommand, the old singular store API (`config_store()` with no arg, `kv_handle`, `secret_handle`). Fix every hit. Confirm every page in the §6.12 table was updated and every page is in the sidebar.

- [ ] **Step 3: Run** the complete gate: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-targets`, `cargo check --workspace --all-features`, all three wasm contract jobs, `cd examples/app-demo && cargo test`, and the docs ESLint/Prettier. All green.

- [ ] **Step 4: Commit:** `git commit -m "app-demo full-capability showcase + documentation audit"`

---

## Self-review notes

- **Spec coverage:** §7→C1, §8/§6.6/§6.7/§6.9→C2, §9/§6.8/§6.10→C3, §10→C4, §11/§6.1→C5, §12→C6, §13/§6.4/§6.5→C7, §15/§6.12→C8. §6.3 (feature gates) is honored throughout. §6.11 (`Default` on `*Args`) is in Tasks 1.1, 4.1, 5.2, 6.1, 7.1. §6.12 docs are in every commit's final task.
- **Precondition:** PR #253 is a hard precondition for commit 2 — called out at the top and in the commit-2 header.
- **Bisectability:** each commit ends with a green-gate step before its commit step; commit 1 needs no PR #253; commit 2's axum config tests seed the JSON fixture directly (Task 2.7 step 1 — "absent ⇒ empty"; tests write the file).
- **Known drift risk:** commits 3–8's exact code depends on the `Bound*Store` / `StoreRegistry` shapes finalized in commit 2. Re-read commit 2's actual output before executing each later commit; adjust signatures to match.
- **`app-demo` in CI:** Task 8.2 adds the missing CI wiring — the spec's §15 ship gate assumed CI exercises `app-demo`, which it does not today.
