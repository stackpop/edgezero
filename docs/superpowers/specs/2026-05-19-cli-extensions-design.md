# EdgeZero CLI Extensions — Full Design

**Date:** 2026-05-19
**Status:** Approved design (single-spec form), pending implementation plan
**Branch:** `docs/extensible-cli-library-spec`

This single spec covers the full effort: turning `edgezero-cli` into an
extensible library, defining a per-service app-config file, adding four
new commands (`auth`, `provision`, `config validate`, `config push`),
extending the project generator to scaffold the new pieces, and updating
`app-demo` to exercise everything end-to-end.

The work is organised into seven sub-projects so it can ship in seven
incremental PRs, but the design decisions live here together so reviewers
see the full picture in one place.

---

## 1. Goal

Let downstream projects (e.g. a future `myapp` created by `edgezero new
myapp`) build their own CLI binary that:

- Reuses any subset of edgezero's built-in commands (today: `build`,
  `deploy`, `dev`, `new`, `serve`; after this effort: also `auth`,
  `provision`, `config validate`, `config push`).
- Adds their own subcommands.
- Owns the binary name, `about` text, and top-level help.

Alongside the extensibility substrate, ship:

- A typed per-service app-config file (e.g. `myapp.toml`) whose schema is
  defined by the downstream app as a Rust struct, validated at lint time
  by `config validate`, and uploaded to the platform config store by
  `config push`.
- Platform credential and resource management (`auth`, `provision`) that
  shells out to each platform's official CLI tool, with all shell-out
  calls wrapped in a mockable `CommandRunner` trait so CI can stay
  hermetic.
- A generator that scaffolds a new project complete with its own
  `<name>-cli` crate (using the lib substrate) and a stub `<name>.toml`
  app-config file.
- An `app-demo` overhaul that demonstrates the finished system:
  `app-demo.toml` with typed `AppDemoConfig`, `app-demo-cli` exposing
  every built-in plus the new commands, and one `app-demo-core` handler
  that reads a config value from the config store at runtime (proving
  the push-then-read flow).

The default `edgezero` binary keeps working unchanged.

## 2. Non-goals

- No runtime command registry (`inventory` / `linkme`-style); no
  PATH-based external subcommand discovery.
- No edgezero-managed credentials. `auth` delegates entirely to
  `wrangler` / `fastly` / `spin`; we store nothing.
- No direct REST API calls to platforms. All platform interactions go
  through the platform's official CLI tool.
- No environment-sectioned app-config (`[config.production]`,
  `[config.staging]`). Single `[config]` table per file; multi-environment
  workflows are deferred until a real need surfaces.
- No live-platform CI smoke tests. All tests run against a mock
  `CommandRunner`.
- No `app-demo` overhaul beyond what is needed to demonstrate the new
  features. Existing handlers, the `app!` macro, and the manifest
  schema stay as they are except for the additive changes called out
  below.

## 3. Architecture overview

```
                ┌─────────────────────────────┐
                │     edgezero-cli (lib)      │
                │  ─────────────────────────  │
                │  pub *Args + pub run_*      │
                │  internal: CommandRunner    │
                │  internal: adapter/gen/...  │
                └────────────┬────────────────┘
                             │ used by
       ┌─────────────────────┼──────────────────────┐
       │                     │                      │
┌──────┴───────┐    ┌────────┴─────────┐   ┌────────┴────────┐
│   edgezero   │    │   app-demo-cli   │   │   myapp-cli     │
│   (bin)      │    │   (example)      │   │   (downstream)  │
│              │    │                  │   │                 │
│ default      │    │ all built-ins +  │   │ subset of       │
│ binary;      │    │ Auth/Provision/  │   │ built-ins +     │
│ all built-   │    │ Config typed on  │   │ custom typed    │
│ ins; no app  │    │ AppDemoConfig    │   │ AppConfig       │
│ struct       │    │                  │   │                 │
└──────────────┘    └─────────┬────────┘   └─────────────────┘
                              │
                  ┌───────────┴────────────┐
                  │  app-demo-core         │
                  │   pub struct           │
                  │   AppDemoConfig:       │
                  │     Deserialize +      │
                  │     Validate           │
                  └────────────────────────┘
```

Key contracts:

- **Substrate**: each built-in command is a `(pub *Args, pub run_*)` pair
  in `edgezero-cli`. Downstream `Subcommand` enums opt in by listing the
  variants they want. Opt-out is omission.
- **Typed app-config**: downstream defines a `#[derive(Deserialize,
  Validate)]` struct; downstream CLI passes that type as a generic
  parameter to `run_config_validate_typed::<C>` and
  `run_config_push_typed::<C>`. The non-typed `run_config_validate` /
  `run_config_push` are also exposed for the default `edgezero` binary
  (which validates only TOML syntax and the `edgezero.toml` schema).
- **Shell-out isolation**: every subprocess call goes through a private
  `CommandRunner` trait. Tests inject a `MockCommandRunner` that records
  invocations and returns scripted outputs. CI never touches a real
  platform.
- **Generator**: `edgezero new <name>` produces a workspace with
  `crates/<name>-core`, `crates/<name>-cli`, per-adapter crates,
  `<name>.toml` app-config stub, and `edgezero.toml`. The new
  `<name>-cli` uses the lib substrate verbatim.

## 4. End-state public API surface

Final shape after all seven sub-projects ship:

```rust
// crates/edgezero-cli/src/lib.rs  (feature = "cli")

// Re-exports of arg structs (all #[non_exhaustive] for forward-compat)
pub use args::{
    AuthArgs, AuthSub, BuildArgs, ConfigPushArgs, ConfigValidateArgs,
    DeployArgs, NewArgs, ProvisionArgs, ServeArgs,
};

pub fn init_cli_logger();

// Built-in commands from the original CLI
pub fn run_build(args: &BuildArgs) -> Result<(), String>;
pub fn run_deploy(args: &DeployArgs) -> Result<(), String>;
pub fn run_new(args: &NewArgs) -> Result<(), String>;
pub fn run_serve(args: &ServeArgs) -> Result<(), String>;
#[cfg(feature = "edgezero-adapter-axum")]
pub fn run_dev() -> !;

// New commands
pub fn run_auth(args: &AuthArgs) -> Result<(), String>;
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String>;

// Config commands: untyped (default edgezero binary) and typed (downstream)
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String>;
pub fn run_config_validate_typed<C>(args: &ConfigValidateArgs) -> Result<(), String>
where
    C: serde::de::DeserializeOwned + validator::Validate;

pub fn run_config_push(args: &ConfigPushArgs) -> Result<(), String>;
pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String>
where
    C: serde::de::DeserializeOwned + validator::Validate + serde::Serialize;
```

Internal modules (`adapter`, `generator`, `scaffold`, `dev_server`,
`runner`, `provision`, `auth`, `config`) all stay private. Only the
symbols above are public.

## 5. End-state file layout

```
crates/edgezero-cli/
  Cargo.toml                  # lib + bin
  src/
    lib.rs                    # public API; declares private modules
    main.rs                   # thin wrapper for the default edgezero bin
    args.rs                   # all pub *Args structs + private Args/Command
    adapter.rs                # (unchanged, private)
    generator.rs              # extended: also scaffolds <name>-cli + <name>.toml
    scaffold.rs               # (unchanged-ish, private)
    dev_server.rs             # (unchanged, private; feature-gated)
    runner.rs                 # NEW: CommandRunner trait + Real/Mock impls
    auth.rs                   # NEW: auth subcommand impl (uses runner)
    provision.rs              # NEW: provision impl (uses runner)
    config.rs                 # NEW: validate + push impl (uses runner)
    templates/
      core/                   # (existing)
      root/                   # (existing; edgezero.toml.hbs updated)
      cli/                    # NEW: templates for <name>-cli
        Cargo.toml.hbs
        src/main.rs.hbs
      app/                    # NEW: <name>.toml.hbs stub app-config
  tests/
    lib_consumer.rs           # NEW: external-consumer compile test

crates/edgezero-core/src/
  app_config.rs               # NEW: generic load_app_config<C>(path)
  manifest.rs                 # (unchanged for this effort)

examples/app-demo/
  Cargo.toml                  # adds crates/app-demo-cli to members
  app-demo.toml               # NEW: typed app config
  crates/
    app-demo-core/
      src/config.rs           # NEW: pub struct AppDemoConfig
      src/handlers.rs         # one handler reads from config store
    app-demo-cli/             # NEW
      Cargo.toml
      src/main.rs             # full Cmd enum: all built-ins + Auth/Provision/Config
      tests/help.rs           # smoke test
    app-demo-adapter-*/       # (unchanged)
```

## 6. Cross-cutting designs

### 6.1 `CommandRunner` trait (sub-project #4 introduces; #5 and #6 reuse)

```rust
// crates/edgezero-cli/src/runner.rs (private)
pub(crate) trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput>;
}

pub(crate) struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) struct RealCommandRunner;
impl CommandRunner for RealCommandRunner { /* std::process::Command */ }

#[cfg(test)]
pub(crate) struct MockCommandRunner { /* recorded expectations */ }
```

The trait is **private to the crate**. Public command functions
(`run_auth`, `run_provision`, `run_config_push`) use a private
`*_with` inner function:

```rust
pub fn run_auth(args: &AuthArgs) -> Result<(), String> {
    run_auth_with(&RealCommandRunner, args)
}

fn run_auth_with<R: CommandRunner>(runner: &R, args: &AuthArgs) -> Result<(), String> {
    // shell out via runner
}

#[cfg(test)]
mod tests {
    fn it_logs_into_cloudflare() {
        let mock = MockCommandRunner::expect(&[("wrangler", &["login"])]);
        run_auth_with(&mock, &AuthArgs { adapter: "cloudflare".into(), sub: AuthSub::Login }).unwrap();
    }
}
```

Public surface stays clean (`run_auth(&args)`); tests bypass to inject
the mock. No public trait, no semver risk on the mock.

### 6.2 Error model

All public `run_*` functions return `Result<(), String>`. This matches
the existing pattern in `edgezero-cli` today. Error formatting is the
function's responsibility; callers (binaries) log and exit.

### 6.3 Feature gates

- `cli` (default) — gates clap and the whole public API.
- `edgezero-adapter-{axum,fastly,cloudflare,spin}` (default) — gate the
  matching adapter dispatch paths in build / deploy / serve / provision /
  auth / config push.
- The new `auth`, `provision`, and `config-push` paths do not introduce
  new feature flags. They are part of `cli`. Per-adapter logic inside
  them is gated on the existing adapter features.

### 6.4 Generic typed config — why two flavours per `config` command

The default `edgezero` binary cannot know the user's `AppConfig` type, so
its `config validate` / `config push` operate in a non-typed mode that
only checks TOML syntax and serialises to a flat string map. Downstream
binaries that know their type call the `_typed::<C>` variants and get
full schema validation via `validator::Validate`.

This is one shared pair of `*Args` structs and two public functions per
command. Not a perfect surface (two names), but the alternative —
type-erasing the schema check via a trait object — costs more in
complexity than the duplication saves.

### 6.5 Test strategy summary

- Existing CLI tests move alongside their handlers.
- New tests are added per sub-project for that sub-project's surface.
- Every test that would touch a platform uses `MockCommandRunner`.
- One external-consumer integration test (`tests/lib_consumer.rs`)
  exercises the public API as a downstream binary would.
- `examples/app-demo/crates/app-demo-cli/tests/help.rs` smoke-tests the
  generated/handwritten downstream pattern.

---

## 7. Sub-project 1 — Extensible `edgezero-cli` library + generator + `app-demo-cli` skeleton

**Goal:** establish the substrate. After this ships, downstream projects
can build their own CLI against the lib using only the existing five
built-ins. Default `edgezero` is unchanged for users.

**Source changes:**

- `crates/edgezero-cli/src/args.rs` — promote each `Command` variant's
  inline fields into a standalone `#[derive(clap::Args)]` struct
  (`#[non_exhaustive]`). `NewArgs` already exists. The internal
  `Command` enum becomes:

  ```rust
  pub enum Command {
      Build(BuildArgs),
      Deploy(DeployArgs),
      Dev,
      New(NewArgs),
      Serve(ServeArgs),
  }
  ```

- `crates/edgezero-cli/src/lib.rs` (new) — declares the private modules,
  moves `init_cli_logger`, `load_manifest_optional`,
  `ensure_adapter_defined`, `store_bindings_message`, `log_store_bindings`,
  and the five handlers (renamed `handle_*` → `run_*`).
- `crates/edgezero-cli/src/main.rs` — shrinks to ~20 lines, dispatches
  to the public `run_*` functions.
- Existing CLI tests move from `main.rs` to `lib.rs`. No assertion
  changes.
- **Generator update**: `generator.rs` and `templates/` extended so that
  `edgezero new <name>` also produces:
  - `crates/<name>-cli/Cargo.toml` (depends on `edgezero-cli` with
    default features + clap + log)
  - `crates/<name>-cli/src/main.rs` (uses all five built-ins via the lib
    substrate; same shape as the canonical downstream example in §3)
  - Root `Cargo.toml.hbs` updated to include `crates/<name>-cli` in
    workspace members.
  - `templates/cli/` directory created to hold the new Handlebars
    templates.
  - **No app-config file yet** — `<name>.toml` arrives in sub-project #2.
- `examples/app-demo/crates/app-demo-cli` (new crate, handwritten —
  parallel to what the generator will produce):
  - Added to `examples/app-demo/Cargo.toml` `members` list.
  - `edgezero-cli = { path = "../../../../crates/edgezero-cli" }` added
    to that workspace's `[workspace.dependencies]` (mirroring the
    existing `edgezero-core` pattern in that file).
  - `src/main.rs` mirrors the canonical downstream pattern, all five
    built-ins, no custom subcommands yet.

**Tests:**

- All existing CLI tests pass after relocation.
- New `crates/edgezero-cli/tests/lib_consumer.rs`: external-consumer
  integration test constructing `BuildArgs` and invoking `run_build`
  against a temp-dir manifest.
- New `examples/app-demo/crates/app-demo-cli/tests/help.rs`:
  `Args::try_parse_from(["app-demo-cli", "--help"])` exits with help
  output and no panic.
- New generator test verifies `generate_new("test-app", ...)` produces
  `crates/test-app-cli/Cargo.toml` and `src/main.rs` referencing the
  right names.

**CI:** all four existing gates (`fmt`, `clippy -D warnings`,
`cargo test`, feature `cargo check`). Spin wasm32 gate unaffected.

**Ship gate:** `edgezero --help` output identical; `app-demo-cli --help`
prints the five built-ins; `edgezero new throwaway-app && cd
throwaway-app && cargo check --workspace` succeeds.

## 8. Sub-project 2 — App-config schema and generic loader

**Goal:** define the file format for per-service app config and the
generic loader the CLI uses.

**Source changes:**

- `crates/edgezero-core/src/app_config.rs` (new):

  ```rust
  use serde::de::DeserializeOwned;
  use validator::Validate;

  #[derive(Debug)]
  pub struct AppConfigError(String);
  impl std::fmt::Display for AppConfigError { /* ... */ }
  impl std::error::Error for AppConfigError {}

  pub fn load_app_config<C>(path: &std::path::Path) -> Result<C, AppConfigError>
  where
      C: DeserializeOwned + Validate,
  {
      // 1. Read file.
      // 2. Parse TOML into a wrapper { config: C }.
      //
      //    File shape:
      //
      //      [config]
      //      key = "value"
      //      ...
      //
      // 3. Run C::validate().
      // 4. Return C.
  }

  // For the non-typed (default-binary) path:
  pub fn load_app_config_raw(path: &std::path::Path)
      -> Result<std::collections::BTreeMap<String, toml::Value>, AppConfigError>;
  ```

  `app_config` is `pub use`d from `edgezero-core`'s `lib.rs`. No new
  workspace deps (serde, validator, toml are already there).

- `crates/edgezero-cli/src/templates/app/<name>.toml.hbs` (new):

  ```toml
  # {{name}} app runtime config.
  # Values are pushed to the active config store via `edgezero config push`.
  # Service code reads them at runtime via the config store binding.

  [config]
  greeting = "hello from {{name}}"
  ```

  Generator emits this as `<name>.toml` at the project root.

- `examples/app-demo/app-demo.toml` (new, handwritten parallel):

  ```toml
  [config]
  greeting = "hello from app-demo"
  timeout_ms = 1500
  feature_new_checkout = false
  ```

- `examples/app-demo/crates/app-demo-core/src/config.rs` (new):

  ```rust
  use serde::{Deserialize, Serialize};
  use validator::Validate;

  #[derive(Debug, Deserialize, Serialize, Validate)]
  pub struct AppDemoConfig {
      #[validate(length(min = 1))]
      pub greeting: String,
      #[validate(range(min = 100, max = 60000))]
      pub timeout_ms: u32,
      pub feature_new_checkout: bool,
  }
  ```

- Generator emits a `<name>-core/src/config.rs` stub mirroring the
  pattern (struct named `<NameUpperCamel>Config`).

**Tests:**

- Unit tests for `load_app_config`: valid file, missing file, bad TOML,
  validator failure, missing `[config]` table.
- Round-trip test in `app-demo-core` that the example `app-demo.toml`
  parses into `AppDemoConfig` and passes validation.

**Ship gate:**
`edgezero_core::app_config::load_app_config::<AppDemoConfig>(Path::new("examples/app-demo/app-demo.toml"))`
succeeds in a test.

## 9. Sub-project 3 — `config validate` command

**Goal:** lint the project's TOML files locally with zero platform calls.

**Public API additions:**

```rust
pub use args::ConfigValidateArgs;
pub fn run_config_validate(args: &ConfigValidateArgs) -> Result<(), String>;
pub fn run_config_validate_typed<C>(args: &ConfigValidateArgs) -> Result<(), String>
where C: DeserializeOwned + Validate;
```

`ConfigValidateArgs`:

```rust
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigValidateArgs {
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,
    /// <name>.toml; auto-detected from [app].name if None.
    #[arg(long)]
    pub app_config: Option<PathBuf>,
    /// Also check cross-references (handlers, adapter consistency).
    #[arg(long)]
    pub strict: bool,
}
```

**Validation steps (in order):**

1. Parse `edgezero.toml` via existing `ManifestLoader`. Report TOML
   syntax errors with file/line.
2. If an app-config file is provided or auto-detected, parse it:
   - Non-typed path: `load_app_config_raw` — confirms structure.
   - Typed path: `load_app_config::<C>` — also runs `Validate`.
3. If `--strict`: cross-check that every adapter referenced in
   `[adapters.*]` has a matching `[stores.*.adapters.*]` if it overrides
   bindings, every handler path in `[[triggers.http]]` is well-formed,
   etc. (Concrete checks listed in the implementation plan.)

**Output:** human-readable diagnostics; exits 0 on success, 1 on failure.

**Tests:**

- Valid manifest passes.
- Each kind of failure (syntax, schema, validator failure, missing
  cross-reference) produces a distinct error message.
- Typed and non-typed paths covered.
- `app-demo-cli config validate` is the canonical typed integration test.

**Ship gate:** `app-demo-cli config validate` exits 0 against the
example workspace; deliberately corrupted fixtures fail.

## 10. Sub-project 4 — `auth` command

**Goal:** delegate per-adapter authentication to the native tool. No
edgezero-stored credentials.

**Public API additions:**

```rust
pub use args::{AuthArgs, AuthSub};
pub fn run_auth(args: &AuthArgs) -> Result<(), String>;
```

```rust
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct AuthArgs {
    #[arg(long)]
    pub adapter: String,            // axum | cloudflare | fastly | spin
    #[command(subcommand)]
    pub sub: AuthSub,
}

#[derive(clap::Subcommand, Debug)]
pub enum AuthSub {
    Login,
    Logout,
    Status,
}
```

**Per-adapter behaviour:**

| Adapter    | Login                   | Logout                  | Status                |
|------------|-------------------------|-------------------------|-----------------------|
| axum       | no-op (log message)     | no-op                   | always "ok"           |
| cloudflare | `wrangler login`        | `wrangler logout`       | `wrangler whoami`     |
| fastly     | `fastly profile create` | `fastly profile delete` | `fastly profile list` |
| spin       | `spin cloud login`      | `spin cloud logout`     | `spin cloud info`     |

All invocations go through `CommandRunner`. This sub-project introduces
the `runner` module (`runner.rs`).

**Tests:**

- For each (adapter, sub) pair: `MockCommandRunner` expectation. The
  mock records the exact program and args; the test asserts them.
- Error cases: tool not found (program returns ENOENT), tool returns
  non-zero exit.

**Ship gate:** with the mock runner, `run_auth` produces the exact
expected subprocess call for every (adapter, sub) pair.

## 11. Sub-project 5 — `provision` command

**Goal:** create the underlying platform resources (KV namespace, secret
store, config store) declared in `[stores.*]` of `edgezero.toml`.

**Public API additions:**

```rust
pub use args::ProvisionArgs;
pub fn run_provision(args: &ProvisionArgs) -> Result<(), String>;
```

```rust
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ProvisionArgs {
    #[arg(long)]
    pub adapter: String,
    #[arg(long)]
    pub dry_run: bool,
}
```

**Behaviour:**

For the named adapter, iterate over `[stores.kv]`, `[stores.secrets]`,
`[stores.config]` in the manifest. For each enabled store, shell out to
create the resource:

| Adapter    | KV                                | Secrets                               | Config                            |
|------------|-----------------------------------|---------------------------------------|-----------------------------------|
| axum       | no-op (local; env-backed)         | no-op                                 | no-op                             |
| cloudflare | `wrangler kv:namespace create N`  | (no-op; wrangler-managed at runtime)  | `wrangler kv:namespace create N`  |
| fastly     | `fastly kv-store create --name N` | `fastly secret-store create --name N` | `fastly config-store create --name N` |
| spin       | (Spin auto-creates KV at deploy)  | (Spin variables file)                 | (Spin variables file)             |

`--dry-run` prints the would-be commands without running them.

**Write-back to per-adapter manifests:** when Cloudflare creates a KV
namespace, the resulting ID must land in `wrangler.toml` so deploys can
bind it. The implementation parses the tool's stdout, extracts the ID,
and patches the per-adapter manifest declared in
`[adapters.<x>.adapter] manifest = "..."`. This is a documented
side-effect of `provision`.

**Tests:**

- For each (adapter, store-kind) tuple, `MockCommandRunner` expectation.
- Manifest write-back tested with a temp-dir fixture: provision runs,
  then the per-adapter manifest is re-read and contains the new ID.
- `--dry-run` produces output but does not invoke the runner.

**Ship gate:** `app-demo-cli provision --adapter cloudflare --dry-run`
prints the expected three `wrangler` invocations.

## 12. Sub-project 6 — `config push` command

**Goal:** upload `<name>.toml`'s `[config]` values to the live config
store on a given adapter.

**Public API additions:**

```rust
pub use args::ConfigPushArgs;
pub fn run_config_push(args: &ConfigPushArgs) -> Result<(), String>;
pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String>
where C: DeserializeOwned + Validate + Serialize;
```

```rust
#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigPushArgs {
    #[arg(long)]
    pub adapter: String,
    /// Auto-detect <name>.toml from [app].name if None.
    #[arg(long)]
    pub app_config: Option<PathBuf>,
    #[arg(long)]
    pub dry_run: bool,
}
```

**Behaviour:**

1. Load app-config (raw map or typed struct).
2. Serialise each top-level field to a string:
   - `String` → as-is.
   - `bool` / numbers → `to_string()`.
   - Compound types (only via the typed path) → JSON-encoded.
3. Shell out to the platform tool for bulk upload:

| Adapter    | Push                                                                       |
|------------|----------------------------------------------------------------------------|
| axum       | Write to `.edgezero/local-config.env` (gitignored).                        |
| cloudflare | `wrangler kv:bulk put <namespace-id> <tempfile.json>`                      |
| fastly     | Iterate: `fastly config-store-entry create --store-id … --key … --value …` |
| spin       | Write to the Spin variables file referenced in the spin manifest.          |

Typed variant also runs `Validate` before pushing (refuses to upload
invalid config).

**Tests:**

- Typed and non-typed paths.
- For each adapter, `MockCommandRunner` expectations including the
  exact serialised payload.
- `--dry-run` prints the serialised payload and would-be commands; does
  not invoke the runner.

**Ship gate:** `app-demo-cli config push --adapter cloudflare --dry-run`
shows the expected `wrangler kv:bulk put` invocation with the JSON
payload derived from `app-demo.toml`.

## 13. Sub-project 7 — `app-demo` integration polish

**Goal:** prove the full system works end-to-end via the example.

**Source changes (all in `examples/app-demo/`):**

- `crates/app-demo-cli/src/main.rs`: extend `Cmd` enum to include the
  new variants:

  ```rust
  #[derive(Subcommand)]
  enum Cmd {
      // Built-ins (same as sub-project #1):
      Build(BuildArgs), Deploy(DeployArgs), Dev, New(NewArgs), Serve(ServeArgs),
      // New commands:
      Auth(AuthArgs),
      Provision(ProvisionArgs),
      #[command(subcommand)]
      Config(ConfigCmd),
  }

  #[derive(Subcommand)]
  enum ConfigCmd {
      Validate(ConfigValidateArgs),
      Push(ConfigPushArgs),
  }
  ```

  Dispatch for `Config::Validate` and `Config::Push` calls the **typed**
  variants with `AppDemoConfig` as the type parameter.

- `crates/app-demo-core/src/handlers.rs`: extend one existing handler
  (e.g. `config_get`) so it reads a key via the config store binding.
  Already partly there — confirm the integration after `config push`
  pushes real data to a local axum store.

- Documentation: a new `docs/cli/walkthrough.md` page showing the full
  loop:

  1. `edgezero new myapp`
  2. `cd myapp && cargo build`
  3. `myapp-cli auth login --adapter cloudflare`
  4. `myapp-cli provision --adapter cloudflare`
  5. `myapp-cli config validate`
  6. `myapp-cli config push --adapter cloudflare`
  7. `myapp-cli deploy --adapter cloudflare`
  8. `curl https://myapp.example/config/greeting`

**Tests:**

- `app-demo-cli config validate` exits 0 against `app-demo.toml`.
- `app-demo-cli config push --adapter axum` writes a local-config file;
  the running axum dev server reads `greeting` from the config store
  and returns it on `/config/greeting`.
- The `--help` smoke test from sub-project #1 is extended to assert all
  subcommands are listed.

**Ship gate:** end-to-end demo of the full loop in CI, using
`--adapter axum` and the local file-backed config store. No live
external calls; the `axum` adapter is the substrate for verifying real
push-then-read behaviour.

---

## 14. Implementation order and milestones

Each sub-project ships as one PR. Order is the §7–§13 order. Each PR
must keep all four CI gates green; no skipping (`-D warnings` stays).

| # | Title                            | Net new public symbols                                                                                                            | Risk |
|---|----------------------------------|------------------------------------------------------------------------------------------------------------------------------------|------|
| 1 | Extensible lib + scaffold        | `BuildArgs`, `DeployArgs`, `NewArgs`, `ServeArgs`, `run_build`, `run_deploy`, `run_new`, `run_serve`, `run_dev`, `init_cli_logger` | M    |
| 2 | App-config schema                | `edgezero_core::app_config::{load_app_config, load_app_config_raw, AppConfigError}`                                                | L    |
| 3 | `config validate`                | `ConfigValidateArgs`, `run_config_validate`, `run_config_validate_typed`                                                           | L    |
| 4 | `auth`                           | `AuthArgs`, `AuthSub`, `run_auth`                                                                                                  | M    |
| 5 | `provision`                      | `ProvisionArgs`, `run_provision`                                                                                                   | H    |
| 6 | `config push`                    | `ConfigPushArgs`, `run_config_push`, `run_config_push_typed`                                                                       | M    |
| 7 | `app-demo` polish + walk-through | (none) — uses everything above                                                                                                     | L    |

**Risk notes:**

- Sub-project #1 is the substrate; getting the `*Args` shape wrong here
  forces churn later. Mitigated by `#[non_exhaustive]` on every Args
  struct and the external-consumer integration test.
- Sub-project #5 (`provision`) is the highest risk: it both shells out
  and writes back to per-adapter manifest files. We constrain blast
  radius by treating manifest write-back as a separate step with its
  own tests and by supporting `--dry-run`.

## 15. Risks and trade-offs

- **API stability:** every public `*Args` struct is `#[non_exhaustive]`
  so adding fields is non-breaking. New `run_*` functions are additive.
  The `_typed::<C>` / non-typed split adds two names per `config`
  command, which is the deliberate trade — see §6.4.
- **Shell-out fragility:** the platform tools' CLI surface can change
  between versions. We pin no specific tool version; we just report a
  clear error when the tool is missing or fails. Tool versions are
  already pinned via the project's `.tool-versions` for the supported
  combinations.
- **Generator drift:** the generator produces a `<name>-cli` whose
  shape must stay in sync with the canonical pattern used by
  `app-demo-cli`. Sub-project #1 introduces a generator test that
  compares structural expectations (file existence + key tokens).
- **`provision` manifest write-back:** parsing tool stdout to extract
  resource IDs is brittle. Mitigation: each tool's parser is its own
  isolated function with golden-file tests over recorded sample
  outputs.
- **Multi-environment app-config:** explicitly out of scope (§2). When
  needed, a follow-up spec will add `[config.<env>]` support and a
  `--env` flag on `config push`/`validate`.
- **Test relocation in sub-project #1:** ~10 tests move from `main.rs`
  to `lib.rs`. Diff looks large but is mechanical; reviewers will be
  warned in the PR description.

## 16. What this spec does not cover

- Anthropic credentials, edge-network DNS / TLS, observability /
  metrics: separate concerns.
- Per-environment config (`production` vs. `staging`): explicit follow-up.
- Replacing or restructuring existing handlers in `app-demo-core` beyond
  the single one that demonstrates push-then-read.
- Any change to `edgezero-core` beyond adding the `app_config` module.

When all seven sub-projects ship, the system supports:

- `edgezero new myapp` produces a workspace ready to build with
  `myapp-cli`, a typed `MyappConfig`, and a `myapp.toml`.
- The developer logs into their platforms (`myapp-cli auth login
  --adapter X`), provisions stores (`myapp-cli provision --adapter X`),
  validates and pushes their app config (`myapp-cli config validate &&
  myapp-cli config push --adapter X`), and deploys (`myapp-cli deploy
  --adapter X`).
- At runtime, the deployed service reads its config from the platform
  config store via the existing edgezero store binding.
- The default `edgezero` binary keeps working unchanged for everyone
  who is not building their own CLI.
