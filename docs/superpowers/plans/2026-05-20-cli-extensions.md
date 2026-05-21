# EdgeZero CLI Extensions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `edgezero-cli` into an extensible library, rewrite the manifest store schema and runtime to a multi-store model, add `auth` / `provision` / `config validate` / `config push` commands, and update `app-demo` to exercise it all across axum / cloudflare / fastly / spin.

**Architecture:** One PR, eight sequential stages. Stage 1 extracts the CLI library substrate. Stage 2 is an atomic manifest + runtime rewrite (hard cutoff — no backward compatibility). Stages 3–7 add app-config and the four commands. Stage 8 makes `app-demo` the full-capability showcase and audits docs.

**Tech Stack:** Rust 1.95 (edition 2021), `clap` (derive), `serde` / `toml` / `serde_json`, `validator`, `async-trait` (`?Send`, WASM-safe), `handlebars` (templates), proc-macros (`edgezero-macros`), VitePress docs.

**Spec:** `docs/superpowers/specs/2026-05-19-cli-extensions-design.md` — read it first. Section references (§) below point into it.

---

## Preconditions (do before stage 2)

- [ ] **PR #253 (`feat/spin-store-support`) is merged into the working branch.** The current branch has **no** Spin store support — `crates/edgezero-adapter-spin/src/` has no `config_store.rs` / `key_value_store.rs` / `secret_store.rs`, and `lib.rs` explicitly rejects `[stores.*]` for spin. Stage 2 wires `SpinKvStore` / `SpinConfigStore` / `SpinSecretStore` into the multi-store runtime; they must exist first. Stage 1 does **not** need PR #253. Verify with: `ls crates/edgezero-adapter-spin/src/` shows the three store files before starting stage 2.
- [ ] Working on branch `feature/extensible-cli` (stacked on `chore/strict-clippy` / PR #257). The spec and plan live in `docs/superpowers/`, which is gitignored — keep using `git add -f` for spec/plan files only.

## Status

- **Stage 1 — DONE.** Landed as `1d582dd` (extensible `edgezero-cli`
  library + generator + `app-demo-cli`) plus follow-up `06f4b72`
  (`demo` is example-only; `serve --adapter axum` runs the axum
  adapter). §7 below is kept for reference — do **not** re-do it.
- **Stages 2–8 — pending.** Stage 2 is gated on PR #253.

## Codebase facts this plan relies on

- `edgezero-cli` is a binary-only crate today; `main.rs` holds private `handle_*` fns; `cli` feature gates `clap`.
- `ConfigStore::get` is **synchronous** today (`config_store.rs`). `KvStore` is already async. `SecretStore` (`get_bytes`) is async, uses `bytes::Bytes`.
- The KV handle type is `KvHandle`; config is `ConfigStoreHandle`; secrets is `SecretHandle`.
- `RequestContext` exposes `config_store() -> Option<ConfigStoreHandle>`, `kv_handle() -> Option<KvHandle>`, `secret_handle() -> Option<SecretHandle>` — all singular.
- Axum KV is `PersistentKvStore` (redb-backed, `.edgezero/kv.redb`).
- `examples/app-demo` is a **separate workspace**, excluded from the root workspace; CI does not currently build or test it.
- CI: `.github/workflows/test.yml` and `format.yml` plus the docs ESLint/Prettier job. The exact gate commands are the five below.

## The full gate

Wherever a task says **"run the full gate"**, it means these exact
commands — the project's documented CI gates (`CLAUDE.md` "CI Gates" +
`.github/workflows/`). Do not substitute `--all-features` for the
feature list, or drop `--all-targets`; match CI exactly so the plan
validates the same surface CI does.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip1 --features spin
```

Plus, where the task touches adapter runtime or `app-demo`: the
per-adapter wasm `--test contract` runs (commands in Task 2.7 step 6),
`cd examples/app-demo && cargo test`, and — for doc changes — the docs
ESLint/Prettier job. Each stage's final task runs the full gate before
its `git commit`.

## File structure (created / modified across the 8 stages)

```
crates/edgezero-cli/
  Cargo.toml                    # M: lib target implicit via src/lib.rs; new deps
  src/lib.rs                    # C (stage 1): public API
  src/main.rs                   # M (stage 1): thin wrapper; M (4-7): dispatch arms for new commands
  src/args.rs                   # M: standalone *Args structs; M (4-7): new *Args + Command enum variants
  src/demo_server.rs            # M (stage 1): renamed from dev_server.rs
  src/runner.rs                 # C (stage 5): CommandSpec + CommandRunner
  src/auth.rs                   # C (stage 5)
  src/provision.rs              # C (stage 6)
  src/config.rs                 # C (stage 7): validate + push
  src/generator.rs              # M (stages 1, 3): scaffold <name>-cli, <name>.toml
  src/templates/cli/            # C (stage 1); M (stage 8): full command set
  src/templates/app/            # C (stage 3)
  src/templates/root/edgezero.toml.hbs   # M (stage 2): new store schema
  src/templates/core/src/config.rs.hbs   # C (stage 3)
  tests/lib_consumer.rs         # C (stage 1)
crates/edgezero-core/src/
  manifest.rs                   # M (stage 2): store schema rewrite + capability rules
  config_store.rs               # M (stage 2): async trait
  key_value_store.rs            # M (stage 2): KvError::Unsupported + LimitExceeded
  secret_store.rs               # M (stage 2): bound-handle wrapper
  context.rs                    # M (stage 2): id-keyed Bound*Store accessors
  extractor.rs                  # M (stage 2): Kv/Secrets/Config default()/named()
  app.rs                        # M (stage 2): Hooks + id-keyed ConfigStoreMetadata (Hooks lives in app.rs, no separate hooks.rs)
  app_config.rs                 # C (stage 3)
crates/edgezero-macros/src/
  lib.rs                        # M (stage 3): AppConfig derive export
  app_config.rs                 # C (stage 3): derive impl
  app.rs                        # M (stage 2): emit id-keyed metadata
crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/
  {config_store,key_value_store,secret_store}.rs   # M (stage 2): multi-store registries
examples/app-demo/
  Cargo.toml                    # M (stage 1): add app-demo-cli member
  edgezero.toml                 # M (stage 2): new schema
  app-demo.toml                 # C (stage 3)
  crates/app-demo-cli/          # C (stage 1, extended 4-8)
  crates/app-demo-core/src/config.rs       # C (stage 3)
  crates/app-demo-core/src/handlers.rs     # M (stages 2, 8)
docs/guide/                     # M: many pages per §6.12
docs/guide/manifest-store-migration.md   # C (stage 2)
docs/guide/cli-walkthrough.md            # C (stage 8)
docs/.vitepress/config.mts      # M (stages 2, 8): sidebar
```

---

# Stage 1 — Extensible `edgezero-cli` library + generator + `app-demo-cli` skeleton ✅ DONE (`1d582dd`, `06f4b72`)

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

- [ ] **Step 5: Commit** is deferred — stage 1 lands as one commit after Task 1.7. Stage progress only.

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

- [ ] **Step 3: Implement.** Add `templates/cli/Cargo.toml.hbs` (package `{{name}}-cli`, depends on `edgezero-cli` with default features, `clap` derive, `log`). Add `templates/cli/src/main.rs.hbs` — the canonical downstream pattern: a `clap::Parser` `Args` with a `Cmd` `Subcommand` enum listing the four downstream built-ins (`Build(BuildArgs)`, `Deploy(DeployArgs)`, `New(NewArgs)`, `Serve(ServeArgs)`), `main` dispatching to `edgezero_cli::run_*`. Register the new templates in `scaffold.rs::register_templates`. In `generator.rs`, render the cli crate and append `crates/{{name}}-cli` to the root `Cargo.toml` members.

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

- [ ] **Step 1:** Add `"crates/app-demo-cli"` to `examples/app-demo/Cargo.toml` `members`. Add `edgezero-cli = { path = "../../crates/edgezero-cli" }` to that workspace's `[workspace.dependencies]` — the path is relative to the workspace manifest (`examples/app-demo/Cargo.toml`), matching the existing `edgezero-core = { path = "../../crates/edgezero-core" }` line.

- [ ] **Step 2:** Write `app-demo-cli/Cargo.toml` — `name = "app-demo-cli"`, `publish = false`, `[lints] workspace = true`, deps `edgezero-cli = { workspace = true }`, `clap = { version = "4", features = ["derive"] }`, `log = { workspace = true }`.

- [ ] **Step 3:** Write `app-demo-cli/src/main.rs` mirroring the generated `templates/cli/src/main.rs.hbs` pattern — the four downstream built-ins, no custom subcommands yet. `#[command(name = "app-demo-cli", about = "app-demo edge CLI")]`.

- [ ] **Step 4:** Write `tests/help.rs`: `Args::try_parse_from(["app-demo-cli", "--help"])` returns the clap help error (not a panic). Since `Args` is private to `main.rs`, instead spawn the built binary: `assert_cmd`-style or `std::process::Command::new(env!("CARGO_BIN_EXE_app-demo-cli")).arg("--help")` exits 0 and stdout contains `build`, `deploy`, `new`, `serve`.

- [ ] **Step 5: Run** `cd examples/app-demo && cargo test -p app-demo-cli` — expect PASS.

### Task 1.6: External-consumer integration test

**Files:**

- Create: `crates/edgezero-cli/tests/lib_consumer.rs`

- [ ] **Step 1: Write the test:** `use edgezero_cli::{BuildArgs, run_build};` — construct `let mut a = BuildArgs::default(); a.adapter = "fastly".into();`, write a minimal `edgezero.toml` into a `tempfile::TempDir`, set `EDGEZERO_MANIFEST`, call `run_build(&a)`, assert `Ok` (mirror the existing `handle_build_executes_manifest_command` test's manifest fixture).

  **Env-mutation guard (required).** `EDGEZERO_MANIFEST` is process-global; concurrent tests mutating it flake. Two rules: (a) restore the variable with an RAII guard — copy the `EnvOverride` struct from `edgezero-cli`'s existing `main.rs`/`lib.rs` tests (it saves the prior value in `new` and restores it in `Drop`); (b) keep `tests/lib_consumer.rs` to **exactly one** `#[test]`, so there is no in-binary parallelism on the env var. If a second env-touching test is ever added to this file, gate both with a shared `std::sync::Mutex` guard (the same `manifest_guard()` pattern the crate's unit tests use) — do not rely on `--test-threads=1`.

- [ ] **Step 2: Run** `cargo test -p edgezero-cli --test lib_consumer` — expect PASS. This proves the public API is usable from outside the crate.

### Task 1.7: Stage-1 documentation + commit

**Files:**

- Modify: `docs/guide/cli-reference.md`, `docs/guide/getting-started.md`, `CLAUDE.md`

- [ ] **Step 1:** In `cli-reference.md` rename `dev` → `demo` and add a short "Building your own CLI" section pointing at the `edgezero-cli` library + the `<name>-cli` scaffold. In `getting-started.md` note that `edgezero new` now also scaffolds `<name>-cli`. In `CLAUDE.md` change the `dev` invocation example to `demo`.

- [ ] **Step 2: Run the full gate** (the five commands in "The full gate" above) plus `cd examples/app-demo && cargo test`. All green.

- [ ] **Step 3: Commit:**

```bash
git add crates/edgezero-cli examples/app-demo docs/guide/cli-reference.md docs/guide/getting-started.md CLAUDE.md
git commit -m "Extensible edgezero-cli library + generator + app-demo-cli; rename dev->demo"
```

---

# Stage 2 — Manifest + runtime rewrite (atomic, all four adapters)

Spec §8, §6.6, §6.7, §6.9. **Requires PR #253.** This is the largest stage and the review hotspot. Hard cutoff — legacy store schema is removed outright.

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

- [ ] **Step 1: axum.** Build `StoreRegistry` for each kind from `[adapters.axum.stores.*]`. KV stays `PersistentKvStore` (redb) — **one separate redb file per logical id**, file stem from the per-adapter mapping `[adapters.axum.stores.kv.<id>].name`: `.edgezero/kv-<name>.redb`. (Axum KV is `Multi`, so every id has a `name`.) Distinct files prevent multi-store collapsing into one backing file. Config store reads `.edgezero/local-config-<id>.json` (the file stage 7 writes); absent ⇒ empty. Secrets from env vars (Single).

- [ ] **Step 2: cloudflare.** KV registry. **Config rewritten from `[vars]` to KV** (§6.9) — `CloudflareConfigStore` does an async `env.<NAMESPACE>.get(key)`; one namespace per config id. Secrets from worker secrets (Single).

- [ ] **Step 3: fastly.** Fastly is `Multi` for **all three** kinds (KV, config, secrets) — the only adapter that is. Build a `StoreRegistry<H>` per kind from `[adapters.fastly.stores.<kind>.*]`:
  - **KV:** one Fastly KV store per logical id, opened by the per-id `name`. The existing `FastlyKvStore` is constructed once per id; the registry maps `<id>` → handle.
  - **Config:** one Fastly config store per logical id, opened by the per-id `name`. The existing `FastlyConfigStore` becomes per-id; `get` stays async after the §6.4 trait change.
  - **Secrets:** one Fastly secret store per logical id, opened by the per-id `name`.
  - For every kind, an absent per-id `name` mapping is already a manifest-validation error (§6.6); the adapter setup can rely on each declared id having a `name`.
  - Resolution: at request setup the adapter reads the `Hooks` store metadata, opens each `(kind, id)` Fastly resource by its `name`, and inserts the three `StoreRegistry` values into the context.
  - **Tests:** the Fastly contract suite must cover **two logical stores of each kind** (e.g. `[stores.kv] ids = ["a", "b"]`) and assert `ctx.kv_store("a")` / `ctx.kv_store("b")` resolve to distinct stores, `ctx.kv_store("missing")` is `None`, and `kv_store_default()` resolves the manifest default — same id-keyed contract-factory shape as the other adapters (Step 5). Run under Viceroy on `wasm32-wasip1`.

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

- [ ] **Step 2:** Migrate `app-demo` handlers to id-keyed accessors — **store-accessor change only** (`ctx.kv_store("sessions")`, `ctx.config_store_default()`, the refactored `Kv`/`Secrets`/`Config` extractors). Do **not** introduce `AppDemoConfig` here (stage 3).

- [ ] **Step 3:** Rewrite `templates/root/edgezero.toml.hbs` to the new schema so `edgezero new` produces a valid manifest.

- [ ] **Step 4:** Write `docs/guide/manifest-store-migration.md` — old shape → new shape, worked example, the capability matrix.

- [ ] **Step 5: Run** `cd examples/app-demo && cargo test && cargo build --workspace` — green.

### Task 2.9: Stage-2 docs + commit

**Files:**

- Modify: `docs/guide/configuration.md`, `kv.md`, `handlers.md`, `adapters/cloudflare.md`, `adapters/overview.md`, `architecture.md`, `docs/.vitepress/config.mts`

- [ ] **Step 1:** Update each page per §6.12 — new `[stores]` schema + capability rules + the removal of `[stores.config.defaults]` (`configuration.md`); multi-store + bound handles + extractor `default()/named()` (`kv.md`, `handlers.md`); `[vars]`→KV config (`adapters/cloudflare.md`); Spin store semantics (`adapters/overview.md`); light review (`architecture.md`). Add `manifest-store-migration.md` to the sidebar in `config.mts`.

- [ ] **Step 2: Run** the full gate (all of `.github/workflows/test.yml` + `format.yml` commands, including the docs ESLint/Prettier and the wasm gates) — green.

- [ ] **Step 3: Commit:** `git commit -m "Manifest + runtime rewrite: multi-store schema, async ConfigStore, all four adapters"`

---

# Stage 3 — App-config schema, derive macro, env-overlay loader

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

- Create: `crates/edgezero-macros/src/app_config.rs`; Modify: `crates/edgezero-macros/src/lib.rs`, `crates/edgezero-core/src/lib.rs`

**Macro availability — chosen route: re-export through `edgezero-core`.**
`edgezero-core` already re-exports the `action` and `app` proc-macros
from `edgezero-macros` (handlers do `use edgezero_core::action`).
`AppConfig` follows the _same_ route: the derive is defined in
`edgezero-macros` and **re-exported from `edgezero-core`** so consumers
write `use edgezero_core::AppConfig`. Consequence: a crate that derives
`AppConfig` needs **only `edgezero-core`** as a dependency for the
macro — no direct `edgezero-macros` dependency. (`#[derive(Validate)]`
and `#[validate(...)]` still need the `validator` crate directly — see
Task 3.4 / 3.5.)

- [ ] **Step 1a: Add the `trybuild` dev-dependency.** Compile-fail tests need `trybuild`; `crates/edgezero-macros/Cargo.toml` currently has only `tempfile` under `[dev-dependencies]`. Add `trybuild = "1"` to `[dev-dependencies]` there (and to `[workspace.dependencies]` in the root `Cargo.toml` if the workspace pins dev-deps centrally — check first and follow the existing convention).

- [ ] **Step 1b: Write macro tests** in `crates/edgezero-macros/tests/app_config_derive.rs`: empty `SECRET_FIELDS` with no annotation; one `KeyInDefault` from `#[secret]`; one `StoreRef` from `#[secret(store_ref)]`; both kinds. Add a `trybuild` compile-fail harness — `let t = trybuild::TestCases::new(); t.compile_fail("tests/ui/*.rs");` — with one `tests/ui/*.rs` fixture per rejected case: `#[secret]` + `#[serde(flatten)]`, `#[secret]` + `#[serde(rename)]`, `#[secret(bogus)]`, `#[secret]` on a non-scalar field. Each fixture has a matching `.stderr` golden file (generate with `TRYBUILD=overwrite` once the `compile_error!` messages are final).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement.** `#[proc_macro_derive(AppConfig, attributes(secret))]` in `edgezero-macros/src/lib.rs` delegating to `app_config::derive`. The impl scans fields for `#[secret]` / `#[secret(store_ref)]`, enforces the §6.7 constraints with `compile_error!`, and emits `impl ::edgezero_core::app_config::AppConfigMeta` with the `SECRET_FIELDS` array (Rust field name verbatim). **Also re-export it from `edgezero-core/src/lib.rs`** — `pub use edgezero_macros::AppConfig;` — next to the existing `action` / `app` re-exports, so downstream code uses `edgezero_core::AppConfig`.

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
- Modify: `crates/edgezero-cli/src/templates/core/Cargo.toml.hbs`, `crates/edgezero-cli/src/generator.rs`, `scaffold.rs`

- [ ] **Step 1: Add a `NameUpperCamel` key to the generator Handlebars context.** The config templates name the struct `{{NameUpperCamel}}Config` (e.g. `my-app` → `MyAppConfig`), and the CLI template in stage 8 reuses the same key. The generator's Handlebars data today exposes only `name`, `proj_core`, `proj_core_mod`, `proj_mod` (`generator.rs`).

  Derivation — **must yield a valid Rust type identifier** (the result is used as `{{NameUpperCamel}}Config`, a `struct` name):
  1. Start from the **sanitized** crate name (reuse `sanitize_crate_name` from `scaffold.rs`, so it stays consistent with the crate name).
  2. Split on `-` and `_`; drop empty segments (this naturally absorbs a leading `_` that `sanitize_crate_name` may have inserted).
  3. Upper-case the first character of each segment, lower-case the rest; join.
  4. **If the result is empty, or its first character is not an ASCII letter** (e.g. the project name started with a digit, giving something like `123App`), prefix it with `App`. A Rust type name cannot begin with a digit.

  Insert the result under the context key `NameUpperCamel`. Add a unit test covering: `my-app` → `MyApp`; `foo` → `Foo`; `a_b-c` → `ABC`; `_foo` → `Foo` (empty leading segment dropped); `123-app` → `App123App` (digit-leading → `App` prefix). This key lands here in stage 3 because `config.rs.hbs` is its first consumer; stage 8's `templates/cli/` reuses it.

- [ ] **Step 2:** `app/<name>.toml.hbs` — a `[config]` table with `greeting` and a nested `[config.service]` section. `core/src/config.rs.hbs` — `{{NameUpperCamel}}Config` with `#[derive(serde::Deserialize, serde::Serialize, validator::Validate, edgezero_core::AppConfig)]` + `#[serde(deny_unknown_fields)]`, a `greeting` field, a nested `service` field, **one plain `#[secret]` field**, and a commented-out `#[secret(store_ref)]` example (§6.8 — the generated template does not include `store_ref` live).

- [ ] **Step 3: Update `templates/core/Cargo.toml.hbs` deps + the workspace-dep seed.** The generated config struct needs `validator` (for `#[derive(Validate)]` / `#[validate(...)]`) and `serde` with the `derive` feature. The `AppConfig` derive comes via the `edgezero-core` re-export (Task 3.2) — the core template already depends on `edgezero-core`, so **no `edgezero-macros` dependency is added**. Add `validator = { workspace = true }` to `templates/core/Cargo.toml.hbs` (it currently lacks it); confirm `serde` is present with `features = ["derive"]`. Because the generated project is itself a workspace, a `workspace = true` dep only resolves if the generated **root** `Cargo.toml` lists it: add `validator` to the generator's workspace-dependency seed (the `seed_workspace_dependencies` function / data in `generator.rs` — confirm the exact name by reading the file; it seeds the generated root `[workspace.dependencies]` and does **not** include `validator` today). Match whatever version-pin the seed already uses for `serde` etc.

- [ ] **Step 4:** Render both new templates in `generate_new`; register them in `scaffold.rs`.

- [ ] **Step 5: Write/extend the generator test** to assert `<name>.toml` and `<name>-core/src/config.rs` are produced, the struct name is `{{NameUpperCamel}}Config` for the test project name, **and** that the generated `<name>-core` builds (the seeded `validator` dep resolves and `edgezero_core::AppConfig` is in scope) — `cargo check -p <name>-core` in the scaffolded project.

- [ ] **Step 6: Run** the generator test — PASS.

### Task 3.5: `app-demo` app-config + commit

**Files:**

- Create: `examples/app-demo/app-demo.toml`, `examples/app-demo/crates/app-demo-core/src/config.rs`
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs`, `examples/app-demo/crates/app-demo-core/Cargo.toml` (verify deps), `docs/guide/configuration.md`, `getting-started.md`

- [ ] **Step 1:** Write `app-demo.toml` — `[config]` with `greeting`, `feature_new_checkout`, a `[config.service]` with `timeout_ms`, `api_token` (a `#[secret]` value), `vault` (a `#[secret(store_ref)]` value = the single secrets id). Write `app-demo-core/src/config.rs` — `AppDemoConfig` with the §6.8 shape (nested `ServiceConfig`, one `#[secret]`, one `#[secret(store_ref)]`), deriving `serde::{Deserialize, Serialize}`, `validator::Validate`, `edgezero_core::AppConfig`. Export it from `lib.rs`. **Verify `app-demo-core/Cargo.toml` deps:** it must have `edgezero-core` (for the `AppConfig` re-export), `validator`, and `serde` with `derive`. `app-demo-core` already depends on all three today — confirm and add any that are somehow missing. No `edgezero-macros` dependency is needed (macro comes via the `edgezero-core` re-export, Task 3.2).

- [ ] **Step 2: Write a round-trip test** in `app-demo-core`: `load_app_config::<AppDemoConfig>` against `app-demo.toml` succeeds; `AppDemoConfig::SECRET_FIELDS` has the expected two entries; an env var overrides the nested value.

- [ ] **Step 3:** Update `configuration.md` (app-config file + env overlay) and `getting-started.md` (generator now emits `<name>.toml`).

- [ ] **Step 4: Run** the full gate. **Commit:** `git commit -m "App-config schema, #[derive(AppConfig)] macro, env-overlay loader"`

---

# Stage 4 — `config validate` command

Spec §10. New: `ConfigValidateArgs`, `run_config_validate`, `run_config_validate_typed`.

### Task 4.1: `config validate` implementation

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` (add `ConfigValidateArgs` + a `ConfigCmd` subcommand enum), `crates/edgezero-cli/src/lib.rs`
- Create: `crates/edgezero-cli/src/config.rs`

- [ ] **Step 1: Write failing tests** with fixtures for each failure mode (§10): valid passes; bad TOML; missing `[config]`; unknown field (struct with `deny_unknown_fields`); type mismatch; validator-rule failure; empty `#[secret]`; `#[secret(store_ref)]` value not in `[stores.secrets].ids`; missing per-adapter mapping; the three Spin checks (key syntax, collision — typed-only, component discovery).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ConfigValidateArgs { manifest, app_config, strict, no_env }` (`#[derive(clap::Args, Default, Debug)] #[non_exhaustive]`). `run_config_validate` (raw) and `run_config_validate_typed<C: DeserializeOwned + Validate + AppConfigMeta>` in `config.rs`. Raw does TOML + manifest checks + Spin key-syntax + component discovery; typed adds deserialize + `validate()` + secret checks + the collision check. Both run manifest `ManifestLoader` validation; `--strict` adds capability completeness + handler-path checks.

- [ ] **Step 4: Run** — PASS.

### Task 4.2: Wire `config` into the default `edgezero` binary

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` (`Command` enum), `crates/edgezero-cli/src/main.rs`

The spec (§1, §8) requires the new subcommands to be available on the
**default `edgezero` binary**, not only on `app-demo-cli`. The default
binary has no app-config struct, so it uses the **raw** functions.

- [ ] **Step 1:** Add `Config(ConfigCmd)` to the default `edgezero-cli` `Command` enum in `args.rs` (the same `ConfigCmd` subcommand enum from Task 4.1; `ConfigCmd::Validate(ConfigValidateArgs)` for now, `Push` added in stage 7).

- [ ] **Step 2:** Add the dispatch arm in `main.rs`: `Command::Config(ConfigCmd::Validate(a)) => exit_on_err(edgezero_cli::run_config_validate(&a))` — the **raw** validator (the default binary has no `C`).

- [ ] **Step 3: Write a test** (in `args.rs` or an integration test): `Args::try_parse_from(["edgezero", "config", "validate", "--strict"])` parses to `Command::Config(ConfigCmd::Validate(_))`; and `cargo run -p edgezero-cli -- --help` lists `config`.

- [ ] **Step 4: Run** `cargo test -p edgezero-cli && cargo build -p edgezero-cli && ./target/debug/edgezero config validate --help` — expect PASS / the subcommand help.

### Task 4.3: Wire `app-demo-cli config validate` + docs + commit

**Files:**

- Modify: `examples/app-demo/crates/app-demo-cli/Cargo.toml`, `examples/app-demo/crates/app-demo-cli/src/main.rs`, `docs/guide/cli-reference.md`

- [ ] **Step 1: Add the `app-demo-core` dependency.** `app-demo-cli` is about to reference `AppDemoConfig`, which lives in `app-demo-core` (created in stage 3, Task 3.5). Its `Cargo.toml` so far has only `edgezero-cli` / `clap` / `log` (Task 1.5). Add `app-demo-core = { path = "../app-demo-core" }` to `app-demo-cli/Cargo.toml` (path dep within the `examples/app-demo` workspace).

- [ ] **Step 2:** Add a `Config(ConfigCmd)` arm to `app-demo-cli`'s `Cmd` enum with `ConfigCmd { Validate(ConfigValidateArgs) }` (push added in stage 7). `use app_demo_core::AppDemoConfig;` and dispatch `Validate` to `edgezero_cli::run_config_validate_typed::<AppDemoConfig>` — the **typed** validator (`app-demo-cli` knows `AppDemoConfig`).

- [ ] **Step 3:** Document `config validate` in `cli-reference.md` — note the default `edgezero` binary runs the raw validator, downstream CLIs the typed one.

- [ ] **Step 4: Run** the full gate; `cd examples/app-demo && cargo run -p app-demo-cli -- config validate --strict` exits 0; `./target/debug/edgezero config validate --strict` (raw path) also exits 0 against a fixture. **Commit:** `git commit -m "config validate command (raw + typed)"`

---

# Stage 5 — `auth` command (+ `CommandRunner`)

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

- [ ] **Step 3: Implement.** `AuthArgs { sub: AuthSub }` — `#[derive(clap::Args, Debug)] #[non_exhaustive]`, **no `Default`** (§6.11). `AuthSub { Login{adapter}, Logout{adapter}, Status{adapter} }`. `run_auth` → `run_auth_with(&RealCommandRunner, args)` dispatching per the §11 table.

- [ ] **Step 4: Run** — PASS. Document `auth` in `cli-reference.md`.

- [ ] **Step 5: Wire both binaries.** Add `Auth(AuthArgs)` to the **default `edgezero-cli` `Command` enum** (`args.rs`) and a dispatch arm in `main.rs`: `Command::Auth(a) => exit_on_err(edgezero_cli::run_auth(&a))`. Also add `Auth(AuthArgs)` to `app-demo-cli`'s `Cmd` enum and dispatch it to `run_auth`. Write a test that `Args::try_parse_from(["edgezero", "auth", "login", "--adapter", "cloudflare"])` parses and that `edgezero --help` lists `auth`.

- [ ] **Step 6: Run** the full gate; `./target/debug/edgezero auth --help` shows the `login`/`logout`/`status` subcommands. **Commit:** `git commit -m "auth command + CommandRunner infrastructure"`

---

# Stage 6 — `provision` command

Spec §12, §13 (Fastly contract).

### Task 6.1: `provision` implementation

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` (`ProvisionArgs`), `lib.rs`
- Create: `crates/edgezero-cli/src/provision.rs`

- [ ] **Step 1: Write tests:** per-(adapter, kind) `MockCommandRunner` expectations with scripted stdout; golden ID-extraction parsers; temp-fixture writeback verified for `wrangler.toml`, `fastly.toml`, and the Spin `key_value_stores` array in `spin.toml`; axum no-op output asserted; `--dry-run` invokes nothing.

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ProvisionArgs { manifest, adapter, dry_run }`. `run_provision` per the §12 per-adapter table: axum no-op; cloudflare `wrangler kv namespace create` + `wrangler.toml` `[[kv_namespaces]]` writeback; fastly `fastly <kind>-store create` + `[setup.*]`/`[local_server.*]` `fastly.toml` writeback; spin KV-label `spin.toml` writeback only (component resolved per §6.7).

- [ ] **Step 4: Run** — PASS. Document `provision` in `cli-reference.md`.

- [ ] **Step 5: Wire both binaries.** Add `Provision(ProvisionArgs)` to the **default `edgezero-cli` `Command` enum** (`args.rs`) and a dispatch arm in `main.rs`: `Command::Provision(a) => exit_on_err(edgezero_cli::run_provision(&a))`. Also add `Provision(ProvisionArgs)` to `app-demo-cli`'s `Cmd` enum, dispatched to `run_provision`. Write a test that `Args::try_parse_from(["edgezero", "provision", "--adapter", "cloudflare", "--dry-run"])` parses and that `edgezero --help` lists `provision`.

- [ ] **Step 6: Run** the full gate; `./target/debug/edgezero provision --adapter cloudflare --dry-run` runs. **Commit:** `git commit -m "provision command (cloudflare/fastly/spin writeback, axum no-op)"`

---

# Stage 7 — `config push` command

Spec §13, §6.4, §6.5.

### Task 7.1: `config push` implementation

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` (`ConfigPushArgs`, extend `ConfigCmd`), `lib.rs`, `crates/edgezero-cli/src/config.rs`

- [ ] **Step 1: Write tests:** typed + raw; per-adapter mock-runner/fixture with golden payloads; secret fields absent; missing native-manifest id (cloudflare) → clear error; Spin `.`→`__` translation; Spin writes both `spin.toml` tables; Spin component-resolution failure errors; `--store` selection; `--dry-run` invokes nothing; the §13 "validate passes, push serialization fails" cases; the Spin `spin.toml` golden test (strongest-first validation ladder, §13).

- [ ] **Step 2: Run** — FAIL.

- [ ] **Step 3: Implement** `ConfigPushArgs { manifest, adapter, store, app_config, no_env, dry_run }`. `run_config_push` / `run_config_push_typed<C: ... + Serialize>`: strict pre-flight validation, load app-config, flatten + serialize per §6.4/§6.5 (skip `SECRET_FIELDS`), resolve target id, push per the §13 per-adapter table (axum local JSON file; cloudflare `wrangler kv bulk put`; fastly `config-store-entry create`; spin both `spin.toml` tables).

- [ ] **Step 4: Run** — PASS.

### Task 7.2: Wire `config push` into both binaries + docs + commit

**Files:**

- Modify: `crates/edgezero-cli/src/args.rs` (`ConfigCmd`), `crates/edgezero-cli/src/main.rs`, `examples/app-demo/crates/app-demo-cli/src/main.rs`, `docs/guide/cli-reference.md`, `configuration.md`

- [ ] **Step 1: Default `edgezero` binary.** Extend the `ConfigCmd` enum (defined in Task 4.1, used by the default `Command::Config` arm from Task 4.2) with `Push(ConfigPushArgs)`. Add the dispatch arm in `main.rs`: `Command::Config(ConfigCmd::Push(a)) => exit_on_err(edgezero_cli::run_config_push(&a))` — the **raw** push.

- [ ] **Step 2: `app-demo-cli`.** Extend `app-demo-cli`'s `ConfigCmd` with `Push(ConfigPushArgs)`; dispatch to `run_config_push_typed::<AppDemoConfig>` — the **typed** push.

- [ ] **Step 3:** Write a test that `Args::try_parse_from(["edgezero", "config", "push", "--adapter", "axum"])` parses to `Command::Config(ConfigCmd::Push(_))` and that `edgezero config --help` lists both `validate` and `push`.

- [ ] **Step 4:** Document `config push` in `cli-reference.md` (note raw vs typed per binary); cross-reference from `configuration.md`.

- [ ] **Step 5: Run** the full gate. **Commit:** `git commit -m "config push command (per-adapter, secret-skipping, env overlay)"`

---

# Stage 8 — `app-demo` integration polish + docs audit

Spec §15, §6.12.

### Task 8.1: Full `app-demo` capability exercise

**Files:**

- Modify: `examples/app-demo/crates/app-demo-cli/src/main.rs`, `examples/app-demo/crates/app-demo-core/src/handlers.rs`, `examples/app-demo/edgezero.toml`, `examples/app-demo/app-demo.toml`, `examples/app-demo/crates/app-demo-adapter-spin/spin.toml`

- [ ] **Step 1:** Confirm `app-demo-cli`'s `Cmd` has the four downstream built-ins + `Auth` + `Provision` + `Config(Validate|Push)`. Ensure handlers exercise: two named KV ids (`sessions`, `cache`) via `Kv::named`; async `config_store_default().get("greeting")`; the nested `service.timeout_ms`; both secret forms. Add the manual Spin secret-variable declarations to `app-demo-adapter-spin/spin.toml` (`secret = true`, bound under `[component.<component>.variables]`).

- [ ] **Step 2: Write integration tests** in `app-demo`: `config validate --strict` exits 0; `config push --adapter axum` writes `.edgezero/local-config-app_config.json` and a running demo server returns `greeting` on `/config/greeting`; `config push --adapter spin --dry-run` **prints** the would-be `__`-encoded keys and the would-be content of both `spin.toml` tables — and the test asserts the on-disk `spin.toml` is **unchanged** (dry-run never mutates); an env-override test asserts `APP_DEMO__SERVICE__TIMEOUT_MS` takes effect.

  **Demo-server lifecycle (required, to keep the e2e test non-flaky):**
  - **Port:** do not hard-code `8787`. Bind an ephemeral port — either bind `127.0.0.1:0` and read back the assigned port, or pick a free port in the test and pass it to the server. Concurrent CI jobs must not collide.
  - **Readiness:** after spawning the server, poll `GET /` (or a health route) with a short retry loop — e.g. up to ~50 attempts, 100ms apart (~5s budget) — and only proceed once a request succeeds. Never use a bare `sleep`.
  - **Teardown:** spawn the server as a child process and kill it in an RAII guard (a struct that holds the `Child` and calls `.kill()` + `.wait()` in `Drop`), so it is reaped even when an assertion fails or panics. Also clean up the `.edgezero/local-config-*.json` files the test wrote.

- [ ] **Step 3: Run** `cd examples/app-demo && cargo test` — PASS.

### Task 8.2: Upgrade the generated `<name>-cli` template to the full command set

**Files:**

- Modify: `crates/edgezero-cli/src/templates/cli/Cargo.toml.hbs`, `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs`, `crates/edgezero-cli/src/generator.rs` (tests)

Stage 1 created the `<name>-cli` template with only the five base
built-ins (`auth` / `provision` / `config` did not exist yet). Now that
stages 4–7 have landed them, a freshly-scaffolded project must expose
the full command surface (spec §1: downstream CLIs reuse the
post-effort built-ins).

- [ ] **Step 1: Add the core-crate dependency to the CLI template.** The full-command template references the typed config functions with `{{NameUpperCamel}}Config`, which lives in the generated `{{name}}-core` crate. The `templates/cli/Cargo.toml.hbs` from stage 1 depends only on `edgezero-cli` / `clap` / `log` — add `{{name}}-core = { path = "../{{name}}-core" }` (path dep within the generated workspace). Without this the scaffolded CLI will not compile.

- [ ] **Step 2:** Update `templates/cli/src/main.rs.hbs` so the generated `Cmd` enum lists **all seven** commands: `Build`, `Deploy`, `New`, `Serve`, `Auth`, `Provision`, `Config(ConfigCmd { Validate, Push })`. Dispatch `build/deploy/new/serve/auth/provision` to the raw `edgezero_cli::run_*`. The `use` statement must reference the core crate's **Rust module name**, not the package name — use `use {{proj_core_mod}}::{{NameUpperCamel}}Config;` (the generator already exposes `proj_core_mod`, the hyphen-to-underscore module form; `{{name}}_core` would render `my-app_core` for `my-app`, which is invalid Rust). Dispatch the `Config` arm to the **typed** `run_config_validate_typed::<{{NameUpperCamel}}Config>` / `run_config_push_typed::<{{NameUpperCamel}}Config>` — a generated project has its own core config struct (from the Task 3.4 `config.rs.hbs` template), so the scaffold wires the typed path, matching how `app-demo-cli` does it.

- [ ] **Step 3:** Extend the generator structure test (from Task 1.4 / 3.4): the scaffolded `<name>-cli/Cargo.toml` depends on `<name>-core`; `<name>-cli/src/main.rs` contains `Auth`, `Provision`, and `Config` variants and references the typed config functions with the project's config type.

- [ ] **Step 4: Run** the generator tests, then `cargo run -p edgezero-cli -- new <tmp> --dir …` and `cargo check --workspace` in the generated project — the scaffolded CLI builds with all seven commands **and** resolves `{{NameUpperCamel}}Config` from its core crate.

### Task 8.3: CI wiring for the `app-demo` loop

**Files:**

- Modify: `.github/workflows/test.yml` (or `scripts/run_tests.sh`)

- [ ] **Step 1:** CI does not currently build `app-demo`. Add a job/step that runs `cd examples/app-demo && cargo test`. Prefer expressing the end-to-end axum loop **as a Rust integration test inside `app-demo`** (the Task 8.1 `app-demo` integration test) rather than as raw shell in the workflow — the Rust test already owns ephemeral-port binding, the readiness poll, and RAII teardown (Task 8.1 step 2). The CI job then just needs `cargo test`; it does not hand-roll `start server / curl / kill` in YAML, which is where shell-based e2e steps go flaky. Keep this job off the wasm matrix — axum only, no live external calls.

- [ ] **Step 2:** If any loop step must stay as a shell step in the workflow (e.g. invoking the built `app-demo-cli` binary), it must still: select a free port (not a hard-coded one), poll readiness before curl-ing, and `kill` the server in a `trap`/`always()` cleanup so a failed assertion never leaves an orphan process. Mirror the Task 8.1 lifecycle rules.

- [ ] **Step 3: Run** the workflow logic locally to confirm the loop passes and leaves no orphan processes or `.edgezero/` artifacts.

### Task 8.4: Walkthrough doc + documentation audit + commit

**Files:**

- Create: `docs/guide/cli-walkthrough.md`; Modify: `docs/.vitepress/config.mts`, any pages still stale

- [ ] **Step 1:** Write `docs/guide/cli-walkthrough.md` — the full `myapp` loop (`new`, `auth`, `provision`, `config validate`, `config push`, `deploy`, `demo`), an env-override example, all four adapters, the manual Spin secret-variable `spin.toml` entries, the explicit `[adapters.spin.adapter].component` form. Add it + `manifest-store-migration.md` to the `config.mts` sidebar.

- [ ] **Step 2: Documentation audit** (§6.12): `grep -rn` the `docs/` tree for stale references — old `[stores.*]` keys (`stores.config.defaults`, `[stores.kv] name`), the `dev` subcommand, the old singular store API (`config_store()` with no arg, `kv_handle`, `secret_handle`). Fix every hit. Confirm every page in the §6.12 table was updated and every page is in the sidebar.

- [ ] **Step 3: Run the full gate** (the five commands in "The full gate" above), plus all three per-adapter wasm `--test contract` runs (Task 2.7 step 6), `cd examples/app-demo && cargo test`, and the docs ESLint/Prettier job. All green.

- [ ] **Step 4: Commit:** `git commit -m "app-demo full-capability showcase + documentation audit"`

---

## Self-review notes

- **Spec coverage:** §7→C1, §8/§6.6/§6.7/§6.9→C2, §9/§6.8/§6.10→C3, §10→C4, §11/§6.1→C5, §12→C6, §13/§6.4/§6.5→C7, §15/§6.12→C8. §6.3 (feature gates) is honored throughout. §6.11 (`Default` on `*Args`) is in Tasks 1.1, 4.1, 5.2, 6.1, 7.1. §6.12 docs are in every stage's final task.
- **Precondition:** PR #253 is a hard precondition for stage 2 — called out at the top and in the stage-2 header.
- **Bisectability:** each stage ends with a green-gate step before its commit step; stage 1 needs no PR #253; stage 2's axum config tests seed the JSON fixture directly (Task 2.7 step 1 — "absent ⇒ empty"; tests write the file).
- **Known drift risk:** stages 3–8's exact code depends on the `Bound*Store` / `StoreRegistry` shapes finalized in stage 2. Re-read stage 2's actual output before executing each later stage; adjust signatures to match.
- **`app-demo` in CI:** Task 8.3 adds the missing CI wiring — the spec's §15 ship gate assumed CI exercises `app-demo`, which it does not today.
