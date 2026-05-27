# EdgeZero CLI Extensions — Full Design

**Date:** 2026-05-19
**Status:** Approved design (single-spec form), pending implementation plan
**Branch:** `docs/extensible-cli-library-spec`
**Baseline assumption:** PR #253 (`feat/spin-store-support`) is merged —
the Spin adapter has `SpinKvStore` / `SpinConfigStore` / `SpinSecretStore`
and is a first-class store-capable adapter.

This single spec covers the full effort:

- a **hard-cutoff manifest schema rewrite** introducing a logical-store /
  per-adapter-mapping model for KV / secrets / config,
- the matching runtime rewrite — `ConfigStore` becomes async, the
  Cloudflare config backend moves from `[vars]` to KV, bound store
  handles are introduced, `Kv` / `Secrets` / `Config` extractors gain
  named-store support, and `Hooks` / `ConfigStoreMetadata` / the `app!`
  macro become id-keyed,
- turning `edgezero-cli` into an extensible library,
- a per-service typed app-config file with `#[derive(AppConfig)]`,
  `#[secret]` / `#[secret(store_ref)]` annotations, and environment
  variable override resolution,
- four new commands (`auth`, `provision`, `config validate`, `config push`),
- generator extensions to scaffold the new pieces,
- and an `app-demo` overhaul that exercises every new capability across
  all four adapters (axum, cloudflare, fastly, spin) end-to-end.

There is **no backward compatibility** with the pre-rewrite manifest
schema or runtime store API. The legacy store fields (`name`, legacy
`adapters` overrides, `[stores.config.defaults]`) become hard
validation errors immediately. Every in-tree project is migrated as
part of the work; external projects do a one-time migration following
the published guide. No compatibility shims, no dual-schema parsing.

The work ships as **one pull request with eight stages** — one stage
per sub-project, in the §16 order. The design decisions live here
together.

---

## 1. Goal

Let downstream projects (e.g. a future `myapp` from `edgezero new
myapp`) build their own CLI binary that:

- Reuses any subset of edgezero's built-in commands (`build`, `deploy`,
  `demo`, `new`, `serve`; after this effort also `auth`, `provision`,
  `config validate`, `config push`). The subcommand that runs the
  example app locally on axum is named `demo` — the name `dev` is
  **reserved** for a future dev-workflow command and is intentionally
  not used by this effort.
- Adds their own subcommands.
- Owns the binary name, `about` text, and top-level help.

Alongside the extensibility substrate, ship:

- A **multi-store manifest model**: the app declares logical stores it
  uses (`[stores.kv] ids = ["foo", "bar"]`); for each store kind an
  adapter is _Multi-capable_ for, it maps every logical id to a
  platform-specific `name`, with room for adapter-specific tuning.
  Stores are addressed in code by logical id. Per-adapter, per-kind
  **capability rules** (§6.6) constrain what is valid — some adapters
  support multiple named stores of a kind, others only a single flat
  one, and the per-adapter mapping block is required for the former and
  forbidden for the latter.
- A **typed per-service app-config file** (`myapp.toml`) with a
  Rust-defined schema, validated by `config validate`, uploaded by
  `config push`. `#[secret]` / `#[secret(store_ref)]` fields are
  skipped during push.
- **Environment-variable override resolution** for app config (§6.10).
- **Async `ConfigStore`** and the **Cloudflare config backend on KV**
  so `config push` reaches the runtime without redeploying.
- **Bound store handles** so callers don't pass store names around.
- **Refactored `Kv` / `Secrets` / `Config` extractors** resolving the
  default store or a named one (§6.8).
- Platform credential and resource management (`auth`, `provision`)
  delegated to each adapter crate's `Adapter::execute` impl — the
  CLI carries no adapter-name strings, and CI stays hermetic
  because the adapter crates choose their own implementation
  (shell-out, HTTP, SDK) and own their tests.
- A generator that scaffolds a new project complete with `<name>-cli`,
  `<name>.toml`, `<name>-core/src/config.rs`, and an `edgezero.toml`
  using the new schema.
- An `app-demo` overhaul exercising all of the above across all four
  adapters end-to-end.

The default `edgezero` binary keeps its existing subcommands' names and
flags; new subcommands are added.

## 2. Non-goals

- No runtime command registry; no PATH-based external subcommand
  discovery.
- No edgezero-managed credentials. `auth` delegates to `wrangler` /
  `fastly` / `spin`.
- No direct REST API calls; everything goes through the platform's
  native CLI.
- No environment-sectioned app-config. The file is a flat typed
  struct — top-level keys are struct fields, no `[config]` /
  `[config.production]` wrapper. (Env-var _override_ is in scope;
  per-environment _files_ are not.)
- No live-platform CI smoke tests. Each adapter crate ships its own
  per-adapter unit tests; the CLI's orchestration tests use fixture
  manifests (`auth-login = "echo logged in"` and the like).
- **No backward compatibility** with the old manifest schema or runtime
  store API. A pre-rewrite `edgezero.toml` is a hard load error.
- No dynamic Spin variable provider integration (Vault, Fermyon Cloud
  variable provider). `config push --adapter spin` writes static Spin
  variables; live cloud variable push is a future enhancement.

## 3. Architecture overview

```mermaid
graph TB
    Lib["<b>edgezero-cli (lib)</b><br/>pub *Args + pub run_*<br/>internal: adapter dispatch (registry) / generator"]

    Macros["<b>edgezero-macros</b><br/>#[derive(AppConfig)]<br/>#[secret] / #[secret(store_ref)]"]

    Core["<b>edgezero-core</b><br/>app_config: load_app_config&lt;C&gt; (toml + env overlay)<br/>manifest: logical-id stores + per-adapter map + capability rules<br/>async ConfigStore + Bound*Store handles<br/>RequestContext: id-keyed bound store accessors<br/>Hooks / ConfigStoreMetadata: id-keyed static metadata<br/>extractor: Kv / Secrets / Config (default or named)"]

    Lib --> EZ["<b>edgezero</b> (default bin)"]
    Lib --> ADC["<b>app-demo-cli</b> (example)<br/>all built-ins + Auth/Provision/Config"]
    Lib --> MAC["<b>myapp-cli</b> (downstream)"]

    ADC --> ADCore["<b>app-demo-core</b><br/>#[derive(AppConfig)] AppDemoConfig<br/>nested section + #[secret] + #[secret(store_ref)]"]
    MAC --> MACore["<b>myapp-core</b><br/>#[derive(AppConfig)] MyappConfig"]

    Macros -.AppConfigMeta impl.-> ADCore
    Macros -.AppConfigMeta impl.-> MACore
    Core -.traits + APIs.-> ADCore
    Core -.traits + APIs.-> MACore
```

Key contracts:

- **Substrate**: each built-in command is a `(pub *Args, pub run_*)`
  pair. Non-subcommand `*Args` derive `Default`; subcommand-wrapping
  `AuthArgs` does not (§6.11).
- **Multi-store manifest model**: §6.6, rewritten outright. Per-adapter
  per-kind capability rules drive validation.
- **Async `ConfigStore`**: `ConfigStore::get` is `async fn`
  (`#[async_trait(?Send)]`, WASM-safe). Cascades through **all four**
  adapter config-store impls.
- **Bound store handles**: only `RequestContext` yields them (binding
  needs per-request adapter state).
- **Static store metadata**: `Hooks` / `ConfigStoreMetadata` are
  compile-time, id-keyed store _metadata_ (emitted by `app!`). Adapters
  consume them at request setup to build runtime registries.
- **Cloudflare config on KV**; **Spin config / secrets on flat Spin
  variables** (§6.7).
- **Extractors**: `Kv` / `Secrets` / `Config` resolve default or named.
- **Typed app-config + secrets**: §6.8. **Env-var override**: §6.10.
- **Shell-out isolation**: each adapter crate owns its native CLI
  invocation in its `Adapter::execute` impl (build/deploy/serve plus
  auth login/logout/status). The CLI carries zero adapter-name
  strings. Adapters that shell out share the
  `edgezero_adapter::cli_support::run_native_cli(program, args,
install_hint)` helper so the "missing binary on PATH /
  non-zero-exit" handling is uniform.

## 4. End-state public API surface

The arg structs live in a **`pub mod args`**, not a crate-root
re-export. A crate-root `pub use args::{...}` would trip
`clippy::pub_use` (the `restriction` group is `-D`-denied
workspace-wide), so the supported API is `edgezero_cli::args::BuildArgs`
etc. The `run_*` functions stay at the crate root. Downstream code
writes `use edgezero_cli::args::BuildArgs;` and
`use edgezero_cli::run_build;`.

```rust
// crates/edgezero-cli/src/lib.rs  (feature = "cli")

/// CLI argument structs — a `pub mod`, addressed as `edgezero_cli::args::*`.
pub mod args;
// args:: { Args, Command, AuthArgs, AuthSub, BuildArgs, ConfigPushArgs,
//          ConfigValidateArgs, DeployArgs, NewArgs, ProvisionArgs, ServeArgs }

pub fn init_cli_logger();

pub fn run_build(args: &args::BuildArgs) -> Result<(), String>;
pub fn run_deploy(args: &args::DeployArgs) -> Result<(), String>;
pub fn run_new(args: &args::NewArgs) -> Result<(), String>;
pub fn run_serve(args: &args::ServeArgs) -> Result<(), String>;
#[cfg(feature = "edgezero-adapter-axum")]
pub fn run_demo() -> Result<(), String>;  // `demo` subcommand; Ok on graceful shutdown

pub fn run_auth(args: &args::AuthArgs) -> Result<(), String>;
pub fn run_provision(args: &args::ProvisionArgs) -> Result<(), String>;

pub fn run_config_validate(args: &args::ConfigValidateArgs) -> Result<(), String>;
pub fn run_config_validate_typed<C>(args: &args::ConfigValidateArgs) -> Result<(), String>
where
    C: serde::de::DeserializeOwned + validator::Validate
       + ::edgezero_core::app_config::AppConfigMeta;

pub fn run_config_push(args: &args::ConfigPushArgs) -> Result<(), String>;
pub fn run_config_push_typed<C>(args: &args::ConfigPushArgs) -> Result<(), String>
where
    C: serde::de::DeserializeOwned + validator::Validate + serde::Serialize
       + ::edgezero_core::app_config::AppConfigMeta;
```

From `edgezero-core`:

```rust
// app_config module
pub trait AppConfigMeta { const SECRET_FIELDS: &'static [SecretField]; }
pub struct SecretField { pub name: &'static str, pub kind: SecretKind }
pub enum SecretKind { KeyInDefault, StoreRef }

// Loader options. Default = env overlay on.
pub struct AppConfigLoadOptions { pub env_overlay: bool }
impl Default for AppConfigLoadOptions { /* env_overlay: true */ }

// Simple forms apply the env overlay (the default).
pub fn load_app_config<C>(path: &std::path::Path, app_name: &str)
    -> Result<C, AppConfigError>
where C: serde::de::DeserializeOwned + validator::Validate + AppConfigMeta;
pub fn load_app_config_raw(path: &std::path::Path, app_name: &str)
    -> Result<toml::Value, AppConfigError>;

// Explicit-options forms — `--no-env` calls these with env_overlay: false.
pub fn load_app_config_with_options<C>(
    path: &std::path::Path, app_name: &str, opts: &AppConfigLoadOptions,
) -> Result<C, AppConfigError>
where C: serde::de::DeserializeOwned + validator::Validate + AppConfigMeta;
pub fn load_app_config_raw_with_options(
    path: &std::path::Path, app_name: &str, opts: &AppConfigLoadOptions,
) -> Result<toml::Value, AppConfigError>;
// The simple forms delegate to the *_with_options forms with
// AppConfigLoadOptions::default().

// async config store trait
#[async_trait(?Send)]
pub trait ConfigStore {
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError>;
}

// Bound store handles — wrap provider handle + resolved platform name.
pub struct BoundKvStore { /* ... */ }
pub struct BoundConfigStore { /* ... */ }
pub struct BoundSecretStore { /* ... */ }
impl BoundConfigStore { pub async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError>; }
impl BoundKvStore     { /* async CRUD */ }
impl BoundSecretStore {
    pub async fn get(&self, key: &str) -> Result<Option<bytes::Bytes>, SecretError>;
    pub async fn require_str(&self, key: &str) -> Result<String, SecretError>;
}

// RequestContext store API — returns BOUND, per-request handles.
impl RequestContext {
    pub fn kv_store(&self, id: &str) -> Option<BoundKvStore>;
    pub fn kv_store_default(&self) -> Option<BoundKvStore>;
    pub fn config_store(&self, id: &str) -> Option<BoundConfigStore>;
    pub fn config_store_default(&self) -> Option<BoundConfigStore>;
    pub fn secret_store(&self, id: &str) -> Option<BoundSecretStore>;
    pub fn secret_store_default(&self) -> Option<BoundSecretStore>;
}

// Hooks / ConfigStoreMetadata: static, compile-time, id-keyed store
// metadata (no bound handles).
```

From `edgezero-macros`:

```rust
#[proc_macro_derive(AppConfig, attributes(secret))]
pub fn derive_app_config(input: TokenStream) -> TokenStream { /* ... */ }
```

## 5. End-state file layout

```
crates/edgezero-cli/
  Cargo.toml
  src/
    lib.rs / main.rs / args.rs / adapter.rs / scaffold.rs / demo_server.rs
    generator.rs              # extended: scaffolds <name>-cli + <name>.toml + <name>-core/src/config.rs
    auth.rs / provision.rs / config.rs   # NEW command impls (thin delegates to adapter::execute)
    templates/{core,root,cli,app}/       # cli/ + app/ new; root edgezero.toml.hbs rewritten

crates/edgezero-core/src/
  manifest.rs                 # store schema rewritten outright; capability rules
  context.rs                  # store accessors id-keyed, return Bound*Store
  app_config.rs               # NEW: AppConfigMeta + SecretField/Kind + loaders w/ env overlay
  config_store.rs             # ConfigStore trait becomes async
  key_value_store.rs / secret_store.rs   # bound-handle wrappers; secret keeps bytes::Bytes
  extractor.rs                # Kv / Secrets / Config refactored to default-or-named
  hooks.rs / app.rs           # id-keyed static store metadata

crates/edgezero-macros/src/
  lib.rs                      # ADD #[proc_macro_derive(AppConfig, attributes(secret))]
  app_config.rs               # NEW derive impl
  app.rs                      # app! macro emits id-keyed ConfigStoreMetadata

# All FOUR adapters' store impls touched in sub-project #2:
crates/edgezero-adapter-{axum,cloudflare,fastly,spin}/src/{config_store,key_value_store,secret_store}.rs
# Cloudflare config_store: [vars] -> KV. Spin already has Spin* stores (PR #253);
# they are wired into the multi-store registry + async ConfigStore here.

examples/app-demo/
  Cargo.toml                  # adds crates/app-demo-cli
  app-demo.toml               # NEW typed config: nested section + #[secret] + #[secret(store_ref)]
  edgezero.toml               # rewritten to the new schema; all four adapters declare stores
  crates/
    app-demo-core/src/config.rs    # NEW AppDemoConfig
    app-demo-core/src/handlers.rs  # handlers read config + named kv across adapters
    app-demo-cli/             # NEW
    app-demo-adapter-*/       # store-setup rewrites (all four)

docs/guide/{cli-walkthrough,manifest-store-migration}.md   # NEW
docs/.vitepress/config.mts    # UPDATED sidebar (note: .mts, not .ts)
```

## 6. Cross-cutting designs

### 6.1 Command spawning — sub-project #5 (revised)

The original sketch placed a `CommandSpec` / `CommandRunner` trait
in `crates/edgezero-cli/src/runner.rs` for CLI-side dispatch
testability. Sub-project #5 (`auth`) demonstrated the wrong split:
hard-coding `("cloudflare", AuthAction::Login) => ("wrangler",
&["login"])` inside the CLI duplicated the adapter-name knowledge
that `build` / `deploy` / `serve` deliberately keep out of
`edgezero-cli` (they read commands from the manifest first, then
fall back to the adapter crate's `Adapter::execute`).

`auth` follows the same path. `AdapterAction` in
`edgezero-adapter::registry` extends to:

```rust
#[non_exhaustive]
pub enum AdapterAction {
    AuthLogin, AuthLogout, AuthStatus,
    Build, Deploy, Serve,
}
```

Each `edgezero-adapter-*` crate implements the new variants in its
own `Adapter::execute` impl (cloudflare shells out to `wrangler`,
axum no-ops, …). The CLI dispatches via the existing
`adapter::execute(adapter, action, manifest, args)` machinery; the
manifest's `[adapters.<name>.commands].auth-{login,logout,status}`
keys are per-project overrides at the same precedence as `build` /
`deploy` / `serve`.

The standalone `CommandRunner` / `MockCommandRunner` types are not
built. Each adapter crate is responsible for its own implementation
mechanism (shell, HTTP, SDK) and its own testability. The CLI
orchestration is covered by the same manifest-override fixture
pattern `build` / `deploy` / `serve` already use.

### 6.2 Error model

All public `run_*` return `Result<(), String>`. Binaries log and exit.

### 6.3 Feature gates

- `cli` (default) gates clap + public API.
- `edgezero-adapter-{axum,fastly,cloudflare,spin}` (all default) gate
  each adapter's dispatch path.

### 6.4 Config key model and platform encoding

App config can be nested (`service: ServiceConfig { timeout_ms }`).
`config push` **flattens nested structs into hierarchical keys** — it
does not store JSON blobs for nested structs. The canonical,
handler-facing key form is **dotted**: `service.timeout_ms`.

Genuine compound _values_ (arrays, maps — not nested structs) are
JSON-encoded into a single string value; the key stays flat.

Each platform's config store has different key constraints, so the key
form is translated per adapter:

| Adapter    | Stored key form for `service.timeout_ms`                                                             |
| ---------- | ---------------------------------------------------------------------------------------------------- |
| axum       | `service.timeout_ms` (local JSON file; dots fine)                                                    |
| cloudflare | `service.timeout_ms` (KV key; arbitrary strings)                                                     |
| fastly     | `service.timeout_ms` (config-store key; dots fine)                                                   |
| spin       | `service__timeout_ms` (Spin variable; see §6.7 — dots and uppercase are invalid Spin variable names) |

The translation is an **adapter-internal detail**. Handlers always use
the canonical dotted form: `ctx.config_store_default()?.get("service.timeout_ms")`.
The Spin config-store impl translates `.` → `__` on the way in/out;
the others pass through. `config push` writes the platform-native form
for the target adapter.

### 6.5 Typed vs raw config serialization

**Validate (both flavours):** TOML syntax OK; the file's top-level
table parses. Typed additionally: deserialises into `C`; runs
`C::validate()`; for each `SecretField`, value is a non-empty string,
and `StoreRef` values appear in `[stores.secrets].ids`. Validate does
not require `Serialize` and performs no `to_value` check.

**Push (both flavours):** all validate checks run first as a strict
pre-flight. Then each leaf field is serialised to a string: `String`
as-is; `bool`/numbers via `to_string()`; arrays/maps via
`serde_json::to_string`; `Option::None` / `Value::Null` skipped;
nested structs flattened into dotted keys (§6.4). `SECRET_FIELDS`
skipped (typed only). Typed additionally: asserts
`serde_json::to_value(&c)` is `Value::Object` (else error before any
runner call); honors `#[serde(rename)]`, `#[serde(skip_serializing*)]`;
supports `#[serde(flatten)]` on non-secret fields. Raw: the parsed
root `toml::Value` tree, same rules, no `Validate`, no secret
skipping.

**Unknown fields:** serde ignores them unless `C` has
`#[serde(deny_unknown_fields)]`. The generator template emits it.

### 6.6 Manifest schema, environment config, and capability rules

`edgezero.toml` is **portable, non-adapter-specific, and never compiled
into the binary**. It declares what the app _is_ — not how any platform
runs it. Adapter-specific runtime config is supplied at runtime through
`EDGEZERO__*` environment variables. There is no legacy shape; a
manifest using the pre-rewrite `[stores.<kind>] name` /
`[stores.config.defaults]` / `[adapters.*.stores.*]` fields is a **hard
load error** pointing at `docs/guide/manifest-store-migration.md`.

**`edgezero.toml` — portable schema:**

```toml
[app]
name = "my-app"

[[triggers.http]]
id      = "root"
path    = "/"
methods = ["GET"]
handler = "my_app_core::handlers::root"

[environment]
# portable env-var / secret declarations

[stores.kv]
ids     = ["sessions", "cache"]
default = "sessions"          # REQUIRED when ids.len() > 1

[stores.config]
ids     = ["app_config"]      # default optional when exactly one id

[stores.secrets]
ids     = ["default"]
```

`[stores.<kind>]` declares **logical store ids only** — the portable
fact that "this app uses a KV store called `sessions`". No platform
names, no per-adapter tuning, and **no per-adapter store / runtime
tables**.

The top-level `[adapters.<name>]` table is retained for adapter
discovery and shell-command wiring (`crate`, `build`, `commands`,
`logging`); what the hard-cutoff removes is the per-adapter
**store** subtable (`[adapters.<name>.stores.*]`) and the per-adapter
**runtime** tuning subtable (anything other than the four blocks
listed). `[adapters.<name>.adapter] host`/`port` survives as a hint
the CLI translates into `EDGEZERO__ADAPTER__HOST`/`PORT` env vars
when spawning a child process; the runtime itself reads only the env
vars (§6.6 below).

| Field                     | Role                                                                           |
| ------------------------- | ------------------------------------------------------------------------------ |
| `[stores.<kind>].ids`     | logical ids (`Vec<String>`, non-empty)                                         |
| `[stores.<kind>].default` | resolved default; **required when `ids.len() > 1`**, else resolves to `ids[0]` |

The `app!` macro consumes `edgezero.toml` at **compile time** and
codegens routing plus the logical store registry into the `App` /
`Hooks` type. The manifest text is **not** embedded — `include_str!` is
gone; only the derived code is. The manifest and the `app!` macro are
optional: a project may build `App` programmatically, so a downstream
binary compiles with no `edgezero.toml` present.

**Adapter-specific config — `EDGEZERO__*` environment variables.**
Platform store names, store tuning, bind host/port, and logging are
resolved at **runtime** from environment variables. `__` (double
underscore) separates key-path segments:

| Variable                                | Role                                      | Default         |
| --------------------------------------- | ----------------------------------------- | --------------- |
| `EDGEZERO__STORES__<KIND>__<ID>__NAME`  | platform name for logical store `<id>`    | the logical id  |
| `EDGEZERO__STORES__<KIND>__<ID>__<KEY>` | free-form adapter tuning for store `<id>` | —               |
| `EDGEZERO__ADAPTER__HOST`               | bind host (axum)                          | `127.0.0.1`     |
| `EDGEZERO__ADAPTER__PORT`               | bind port (axum)                          | `8787`          |
| `EDGEZERO__LOGGING__LEVEL`              | log level                                 | adapter default |

`<KIND>` ∈ `KV` / `CONFIG` / `SECRETS`; `<ID>` is the upper-cased
logical id. Absent variables fall back to the listed defaults — an
adapter binary runs with **zero env vars set**, using each logical id
as its own platform name.

**Adapter × kind capability matrix.** Each (adapter, kind) pair has a
capability:

| Adapter    | KV               | Config                  | Secrets                 |
| ---------- | ---------------- | ----------------------- | ----------------------- |
| axum       | Multi (local)    | Multi (local files)     | Single (env vars)       |
| cloudflare | Multi (KV ns)    | Multi (KV ns)           | Single (worker secrets) |
| fastly     | Multi (KV store) | Multi (config store)    | Multi (secret store)    |
| spin       | Multi (KV label) | Single (flat variables) | Single (flat variables) |

- **Multi**: the adapter supports multiple named stores of that kind;
  each logical id resolves to its own platform store via
  `EDGEZERO__STORES__<KIND>__<ID>__NAME` (or the id default).
- **Single**: the adapter has exactly one flat store of that kind;
  every logical id maps to that one store, and per-id `NAME` variables
  are ignored.

**Capability validation** — declaring two config ids while targeting an
adapter that is `Single` for config (Spin) — is performed by `config
validate` (§10) and `provision` (§12). It is no longer expressible as
an in-manifest error: the manifest carries no per-adapter blocks.

**Runtime resolution:** each adapter builds a
`StoreRegistry<H> { by_id: BTreeMap<String, H>, default_id: String }`
at request setup, keyed by logical id, platform names resolved from
`EDGEZERO__STORES__*` (or the id default). For `Single` (adapter, kind)
pairs every id maps to the one flat store.

### 6.7 Spin store semantics

PR #253 makes Spin store-capable, but Spin's model differs from
Cloudflare/Fastly and the spec must encode that explicitly.

**KV — label-backed, multi-store.** `SpinKvStore` is backed by
`spin_sdk::key_value`. Each logical KV id maps to a Spin KV store
**label** via `[adapters.spin.stores.kv.<id>].name`. Multiple labels
are fine. The runtime adapter opens each configured label and
registers it by logical id.

- **TTL is unsupported.** `spin_sdk::key_value` has no expiry. The
  `BoundKvStore` surface still exposes `put_*_with_ttl` (used by other
  adapters). On Spin, those operations **must return a deterministic
  error**, never silently store the value without expiry. The current
  `KvError` enum has **no `Unsupported` variant** — **stage 2 adds
  `KvError::Unsupported`** and its `EdgeError` mapping. Because an
  unsupported operation is not a client mistake, it maps to a
  5xx-class `EdgeError` (the exact constructor — `EdgeError::internal`
  or a dedicated one — is pinned in stage 2). The Spin KV contract
  test asserts this error.
- **Listing is capped.** `SpinKvStore` carries a `max_list_keys` cap
  and must error rather than silently truncate when exceeded. A store
  growing beyond a cap is a server/limit condition, not a malformed
  client request, so PR #253's current `KvError::Validation` (which an
  adapter may map to HTTP 400) is the wrong variant. **Resolved here,
  not left open: stage 2 adds `KvError::LimitExceeded`** (5xx-class
  `EdgeError` mapping, like `Unsupported`) and the Spin KV listing
  path returns it when `max_list_keys` is exceeded, replacing
  `Validation` for this case. Stage 2 also tests the pagination logic
  directly (not only the cap error).

**Config — flat Spin variables, single-store.** `SpinConfigStore` is
backed by `spin_sdk::variables`. Spin has **one** flat variable
namespace per component — there is no notion of multiple named config
stores. Therefore `[stores.config].ids` must have exactly one id for
any project targeting Spin (enforced by the §6.6 capability check),
and a `[adapters.spin.stores.config.*]` block is a validation error
(Single capability, §6.6).

Spin variable names must match `^[a-z][a-z0-9_]*$` — lowercase,
starting with a letter, alphanumeric + underscore. **This is Spin's
own rule** (see the Spin manifest reference,
<https://spinframework.dev/manifest-reference>), not an EdgeZero-added
restriction; the EdgeZero config-store impl simply conforms to it. The
impl translates the canonical dotted key (`service.timeout_ms`) to a
Spin variable (`service__timeout_ms`); a dotted or uppercase key
reaching the real Spin backend yields `InvalidName`.

**Secrets — flat Spin variables, single-store, manual declaration.**
`SpinSecretStore` is also backed by `spin_sdk::variables` — the **same
flat namespace** as Spin config. `store_name` passed to `get_bytes` is
ignored (the adapter logs a debug line when non-empty).
`[stores.secrets].ids` must have exactly one id for a Spin project,
and `[adapters.spin.stores.secrets.*]` is a validation error.

Spin **secret variables are declared manually** by the developer in
`spin.toml` (as `[variables]` entries with `secret = true`, bound via
`[component.<component>.variables]`). Neither `provision` nor `config
push` writes secret variables — `config push` skips `SECRET_FIELDS`,
and the secret key names are not reliably knowable: a
`#[secret(store_ref)]` field's runtime key (e.g.
`ctx.secret_store(&cfg.vault)?.require_str("active")`) is code-local,
appearing in neither the manifest nor `<name>.toml`. The CLI cannot
infer it, so secret-variable declaration stays with the developer.
The `cli-walkthrough.md` doc shows the required `spin.toml` entries.

**Config/secret variable collision check (replaces an over-strong
guarantee).** Spin config and secret variables share one flat
namespace, so their _effective Spin variable names_ must not collide.
The earlier claim that distinct struct fields guarantee this is wrong:
a `#[secret]` field's **value** (not its Rust field name) is the
secret key, so a config key `api_token` and a `#[secret]` field whose
value is `"api_token"` would collide. When `spin` is in the adapter
set, `config validate` computes the effective Spin variable name set —
{flattened config keys} ∪ {`#[secret]` field values} — each after
`.`→`__` lowercase translation, and **errors on any duplicate**.
`#[secret(store_ref)]` runtime keys are code-local and outside this
check; the walkthrough doc warns the developer to keep them clear of
config keys.

**Spin component discovery.** Writing `[component.<component>.*]`
tables (for KV labels in `provision`, for variable bindings in `config
push`) needs the **component id**, not just the `spin.toml` path.
`[adapters.spin.adapter].manifest` points at `spin.toml`, which may
declare several components. Resolution rule:

- The CLI parses `spin.toml` and enumerates `[component.*]` ids.
- If exactly one component exists, it is used.
- If more than one exists, `[adapters.spin.adapter]` **must** carry an
  explicit `component = "<id>"` field; otherwise the command errors.
- An explicit `component` that does not match any `[component.*]` id
  is an error.

`config validate` performs this resolution as part of `--strict`
checks when `spin` is in the adapter set, so the failure surfaces
before `provision` / `config push` run.

**Implication for app config targeting Spin.** If the adapter set
includes `spin`, `config validate` additionally checks that every
flattened config key, after `.`→`__` translation, matches
`^[a-z][a-z0-9_]*$` — i.e. config field names must be lowercase
snake_case. This is consistent with idiomatic serde field naming.

### 6.8 Secret annotations via `#[derive(AppConfig)]`

```rust
#[derive(Debug, Deserialize, Serialize, Validate, AppConfig)]
#[serde(deny_unknown_fields)]
pub struct AppDemoConfig {
    pub greeting: String,
    #[validate(nested)]
    pub feature: FeatureConfig,          // nested — flattens to `feature.new_checkout`
    #[validate(nested)]
    pub service: ServiceConfig,          // nested section (env-overridable, §6.10)

    #[secret]                            // key inside the resolved default secret store
    pub api_token: String,

    #[secret(store_ref)]                 // logical store id in [stores.secrets].ids
    pub vault: String,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(deny_unknown_fields)]
pub struct FeatureConfig {
    pub new_checkout: bool,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[validate(range(min = 100, max = 60000))]
    pub timeout_ms: u32,
}
```

`#[validate(nested)]` is required for the outer `validate()` to
recurse into the inner structs — without it the inner `range` /
`length` rules silently no-op. Stage 4's `app-demo-cli config
validate` would otherwise accept any nested value.

The derive emits `impl AppConfigMeta` with a `SECRET_FIELDS` array.

**Constraints (compile errors):** `#[secret]` / `#[secret(store_ref)]`
only on scalar string fields; error if combined with
`#[serde(flatten)]` / `#[serde(rename)]` / `#[serde(skip*)]`;
`#[secret(x)]` with `x` outside `{store_ref}` is an error;
`SECRET_FIELDS` uses the Rust field name verbatim.

**Validate:** `KeyInDefault` — value non-empty + `[stores.secrets]`
declared. `StoreRef` — value appears in `[stores.secrets].ids`.
**Push:** both kinds skipped.

**Interaction with the secrets capability matrix.** Axum, Cloudflare,
and Spin are all `Single` for secrets (§6.6) — only Fastly is `Multi`.
So any project whose adapter set includes axum, cloudflare, or spin
can declare exactly **one** secrets id (the capability check forces
`[stores.secrets].ids.len() == 1`). For such a project — which
includes any all-four-adapter app — every `#[secret(store_ref)]`
field's value must be that single secrets id; there is no other valid
target. `#[secret(store_ref)]` only buys multiple distinct secret
stores on a Fastly-only project. `config validate` already enforces
"value ∈ `[stores.secrets].ids`", so a wrong id fails validation; the
walkthrough doc calls this out explicitly.

**Runtime usage:**

```rust
// #[secret] (KeyInDefault):
let token = ctx.secret_store_default()?.require_str(&cfg.api_token).await?;
// #[secret(store_ref)] (StoreRef) — on an all-four-adapter app,
// cfg.vault is necessarily the single declared secrets id:
let token = ctx.secret_store(&cfg.vault)?.require_str("active").await?;
```

### 6.9 Extractor design

`Kv` / `Secrets` / `Config` extractors yield a per-request registry
handle; the handler picks the store by id at the call site (no
const-generic `&'static str`, unsupported on stable Rust 1.95):

```rust
pub struct Kv(KvRegistryHandle);
impl Kv {
    pub fn default(&self) -> Option<BoundKvStore>;
    pub fn named(&self, id: &str) -> Option<BoundKvStore>;
}
// Secrets / Config identical in shape.
```

The only in-tree consumers of the old single-store extractors are the
`app-demo` handlers, updated in sub-project #2.

### 6.10 App-config environment-variable resolution

`load_app_config` / `load_app_config_raw` resolve in two layers:
(1) the file's top-level table from `<name>.toml` (no `[config]`
wrapper — the file is the typed struct directly); (2) env-var
overrides.

**Env vars override existing keys only.** An env var overrides a value
only if that key already exists in the parsed tree (the loader infers
the type from the existing TOML value and parses the env string
accordingly — there is no pre-deserialization reflection over `C`).
To make a key env-overridable it must appear in `<name>.toml`.

**Env var naming.** `<APP_NAME>__<SECTION>__…__<KEY>`. `<APP_NAME>` is
`[app].name` uppercased with `-`→`_`. `__` separates every nesting
level; a single `_` is literal.

**Deterministic, ambiguity-rejecting matching.** Each config key is
transformed to its env-segment form (uppercase, `_` left as-is) and
compared exactly. Two sibling keys mapping to the same segment is an
`AppConfigError`.

**Type coercion.** The env string is parsed against the existing TOML
value's type; parse failure → `AppConfigError`.

**Scope.** `config validate` and `config push` both see env-resolved
values; `--no-env` disables the overlay. `--no-env` is implemented by
calling `load_app_config_with_options` (§4) with
`AppConfigLoadOptions { env_overlay: false }`; the default (no flag)
uses the simple `load_app_config` form (overlay on). The axum demo
server (the `demo` subcommand) resolves via the same path.

Note the deliberate consistency: the env separator (`__`) is the same
as the Spin config-key separator (§6.4/§6.7).

### 6.11 `Default` on `*Args`

Non-subcommand `*Args` derive `Default` (external construction despite
`#[non_exhaustive]`). Subcommand-wrapping `AuthArgs` does not (a
defaulted required subcommand could leak into a real auth path);
external tests construct it via `clap::Parser::try_parse_from`.

### 6.12 Documentation updates (definition-of-done for every stage)

This effort changes the manifest schema, the runtime store API, the
CLI surface, and the `dev`→`demo` subcommand. The VitePress docs site
under `docs/guide/` has existing pages describing all of these, which
go stale. **Updating documentation is part of every stage's
definition-of-done** — a stage that changes user-facing behaviour
updates the affected `docs/guide/` pages _in the same stage_, so the
PR never has a docs-lag window. The docs CI (ESLint + Prettier on
`docs/`) must pass.

Affected existing pages and the stage that owns each update:

| Page                                                  | What changes                                                                                                                                                      | Stage      |
| ----------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------- |
| `docs/guide/cli-reference.md`                         | `dev`→`demo` rename; `edgezero-cli` as a library; new `auth` / `provision` / `config` commands                                                                    | 1, 5, 6, 7 |
| `docs/guide/configuration.md`                         | new `[stores]` logical-id schema + per-adapter mapping + capability rules; removal of `[stores.config.defaults]`; the `<name>.toml` app-config file + env overlay | 2, 3       |
| `docs/guide/kv.md`                                    | multi-store model, `ctx.kv_store(id)` / bound handles, `Kv` extractor `default()`/`named()`                                                                       | 2          |
| `docs/guide/handlers.md`                              | extractor refactor; async `ConfigStore`; reading config/secrets by logical id                                                                                     | 2          |
| `docs/guide/getting-started.md`                       | generator now scaffolds `<name>-cli` and `<name>.toml`                                                                                                            | 1, 3       |
| `docs/guide/adapters/cloudflare.md`                   | config store moves `[vars]` → KV                                                                                                                                  | 2          |
| `docs/guide/adapters/overview.md` + Spin adapter docs | Spin store semantics (KV labels, flat-variable config/secrets)                                                                                                    | 2          |
| `docs/guide/architecture.md`                          | light review — store/adapter description                                                                                                                          | 2          |

New pages (created in their owning stage):

- `docs/guide/manifest-store-migration.md` — stage 2 (how to migrate a
  pre-rewrite `edgezero.toml`).
- `docs/guide/cli-walkthrough.md` — stage 8 (full `myapp` loop).

Stage 8 additionally performs a **documentation audit**: grep the
`docs/` tree for stale references (old manifest store keys, the `dev`
subcommand, the old single-store runtime API) and confirm none remain;
verify every page is listed in the `docs/.vitepress/config.mts`
sidebar. The audit is a checklist item in stage 8's ship gate.

---

## 7. Sub-project 1 — Extensible `edgezero-cli` library + generator + `app-demo-cli` skeleton

**Goal:** establish the substrate.

**Source changes:** promote `Command` variant fields into
`#[derive(clap::Args)]` structs (`#[non_exhaustive]`, `Default` per
§6.11); add `lib.rs` with `run_*` handlers; shrink `main.rs`; move
existing tests to `lib.rs`; extend the generator to scaffold
`crates/<name>-cli`; add the handwritten `examples/app-demo/crates/
app-demo-cli` parallel.

The `dev` subcommand is renamed to **`demo`** — it runs the example
app locally on axum, which is a demo workflow, not a dev workflow; the
name `dev` is reserved for a future dev-workflow command. Stage 1
renames the CLI's `dev_server` module to `demo_server`, the public
function `run_dev` to `run_demo`, and the `Command::Dev` variant to
`Command::Demo`. `run_demo` returns `Result<(), String>` (consistent
with the other `run_*` functions) — `Ok(())` on graceful shutdown,
`Err(String)` on startup failure (e.g. port bind). It is **not**
`-> !` — the demo server is allowed to return. The current
`dev_server::run_dev()` returns `()`; stage 1 adjusts that boundary.
(The `edgezero-adapter-axum` crate's own internal `dev_server` module
is not user-facing and is left as-is.)

**Tests:** existing tests pass post-relocation; `tests/lib_consumer.rs`;
`app-demo-cli/tests/help.rs`; generator structure test.

**Ship gate:** existing `edgezero` commands keep the same flags;
`app-demo-cli --help` shows the four downstream built-ins (`build`, `deploy`, `new`, `serve`); `edgezero new
throwaway-app && cargo check --workspace` succeeds.

## 8. Sub-project 2 — Manifest + runtime rewrite (atomic, all four adapters)

**Goal:** the big atomic sub-project. The manifest becomes portable and
non-adapter-specific (§6.6), adapter config moves to `EDGEZERO__*`
environment variables, and the runtime store API is rewritten. With a
hard cutoff these ship together as one stage (stage 2 of the
eight-stage PR).

**Scope:**

- **Manifest → portable schema:** rewrite `ManifestStores` to the §6.6
  portable schema — `[stores.<kind>]` carries only logical `ids` /
  `default`. The `[adapters.*]` store/runtime tables are removed.
  Legacy fields are a hard load error.
- **`EDGEZERO__*` env-config layer:** a new `edgezero-core` module
  parses `EDGEZERO__`-prefixed environment variables (`__` nesting)
  into adapter runtime config — store platform names + tuning, bind
  host/port, logging. Absent variables fall back to defaults (§6.6).
- **No compiled-in manifest:** `run_app` drops its `manifest_src`
  parameter on all four adapters. The `app!` macro bakes the portable
  config (routes + logical store registry) into the `App` / `Hooks`
  type; `run_app::<A>()` reads it from `A` and layers `EDGEZERO__*` env
  config on top. `include_str!("edgezero.toml")` is removed everywhere.
- **`ConfigStore` async:** `get` becomes `async`
  (`#[async_trait(?Send)]`).
- **New `KvError` variants:** add `KvError::Unsupported` (Spin TTL
  writes, §6.7) and `KvError::LimitExceeded` (Spin listing past
  `max_list_keys`, §6.7), each with a 5xx-class `EdgeError` mapping.
- **Bound handles:** `BoundKvStore` / `BoundConfigStore` /
  `BoundSecretStore`; `RequestContext` accessors id-keyed, with
  `_default()` helpers.
- **Static metadata:** `Hooks` / `ConfigStoreMetadata` rewritten to
  id-keyed metadata; `app!` macro emits them from the portable schema.
- **Adapter store rewrites — ALL FOUR adapters:** each builds a
  `StoreRegistry` keyed by logical id, platform names resolved from
  `EDGEZERO__STORES__*` (or the id default):
  - **axum:** local KV registry; config from
    `.edgezero/local-config-<id>.json` (§15); secrets from env vars.
  - **cloudflare:** KV registry; **config rewritten `[vars]` → KV**
    with async reads; secrets from worker secrets.
  - **fastly:** KV / config / secret store registries.
  - **spin:** wire `SpinKvStore` (label registry, `max_list_keys`
    respected), `SpinConfigStore` (single flat-variable store, `.`→`__`
    key translation), `SpinSecretStore` (single flat-variable store)
    into the registry; KV labels come from
    `EDGEZERO__STORES__KV__<ID>__NAME`, not hardcoded defaults.
- **Extractors:** `Kv` / `Secrets` refactored to `default()` /
  `named()`; `Config` extractor added.
- **`[stores.config.defaults]` removed** (hard error). Replaced by the
  axum config-store file flow (§15). The axum dev-server config seeding
  is removed.
- **Migrate in-tree:** `examples/app-demo/edgezero.toml` rewritten to
  the portable schema (≥2 KV ids `sessions`+`cache`; one config id;
  one secrets id). The app-demo adapter crates' `EDGEZERO__*` env
  config lives in their run configuration. `app-demo` handlers are
  migrated **only for the store-accessor change** — `ctx.kv_store(id)`
  / `config_store` / the refactored `Kv` / `Secrets` / `Config`
  extractors. Stage 2 does **not** introduce `AppDemoConfig` or any
  typed-app-config handler work: that lands in stage 3 (§9). This keeps
  stage 2 independently buildable.
- **`docs/guide/manifest-store-migration.md`** published.

**Tests:** manifest round-trip + validation (non-empty ids; default
required when `ids.len() > 1`; pre-rewrite manifest → hard error with
migration message); `EDGEZERO__*` env-layer parsing (nesting, defaults,
store-name resolution); `run_app` builds and runs with no manifest file
and zero env vars; id-keyed contract-test factories across all four
adapters; cross-adapter named-KV test; Cloudflare config-from-KV async
round-trip; Spin config `.`→`__` translation test; **Spin TTL write
returns `KvError::Unsupported`** (contract test); Spin KV listing-cap
pagination test; `Kv`/`Secrets`/`Config` extractor tests; `app!` macro
metadata registry test.

**Bisectability — config seeding before `config push` exists.** Stage
2 removes `[stores.config.defaults]` and makes the axum config store
read `.edgezero/local-config-<id>.json`, but `config push` (which
_writes_ that file) does not land until stage 7, and `edgezero demo`'s
auto-regeneration of the file depends on the stage-3 loader and the
stage-7 resolve-and-write step. So between stage 2 and stage 7:

- The axum config store's backing-file **contract** is what stage 2
  establishes; stage 2 does not need anything to _produce_ the file.
- Stage 2's axum config-store tests **write the JSON fixture file
  directly** in test setup (a temp-dir fixture) — they exercise the
  read path without depending on `config push`.
- `app-demo`'s stage-2 state: if no fixture file is present the axum
  config store is empty (the documented "absent → empty" behaviour).
  Any stage-2 `app-demo` test that asserts a config value seeds the
  fixture file itself. The full `config push` → running-demo-server
  read-back end-to-end test lands in stage 8.

This keeps stage 2 independently buildable and testable.

**Ship gate:** multi-store handlers work on axum, cloudflare, fastly,
and spin; async config reads work; an adapter binary builds and runs
with no `edgezero.toml` and zero env vars (falling back to defaults);
all five CI gates green (including the wasm32 spin gate).

## 9. Sub-project 3 — App-config schema, derive macro, env-overlay loader

**Goal:** the `<name>.toml` format, `#[derive(AppConfig)]`, and the
generic loader with env-var overlay (§6.10).

**Source changes:** `edgezero-core::app_config`; `edgezero-macros`
`AppConfig` derive + `#[proc_macro_derive]` export; generator
templates for `<name>.toml` (with a nested `[service]` table at the
root — no `[config]` wrapper) and `<name>-core/src/config.rs` (with
`#[serde(deny_unknown_fields)]`); `examples/app-demo/app-demo.toml`

- `app-demo-core/src/config.rs`.

**Generated template vs the `app-demo` example — deliberately
different.** The **generated** `<name>-core/src/config.rs` (what
`edgezero new` scaffolds) is the _common-case_ starting point: a
`greeting` field, a nested `[service]` table (to exercise the env
overlay), and a single plain `#[secret]` field as the common
secret pattern. It does **not** include `#[secret(store_ref)]` —
`store_ref` only buys multiple secret stores on a Fastly-only project
(§6.8), so putting it in every fresh scaffold would teach the edge
case as the default. A commented line in the template shows how to add
`#[secret(store_ref)]` if needed. The **`app-demo` example** is the
opposite: it deliberately exercises _everything_, so its
`app-demo-core/src/config.rs` includes a nested section, one
`#[secret]`, **and** one `#[secret(store_ref)]` — `app-demo` is the
full-capability showcase, not a representative new project.

**Tests:** `load_app_config` (valid, missing file, bad TOML,
validator failure); env-overlay tests (top-level, nested `__`, type
coercion, parse failure, ambiguous key → error, `--no-env`);
round-trip for `AppDemoConfig`; macro tests for all §6.8
compile-error constraints.

**Ship gate:** `AppDemoConfig::SECRET_FIELDS` matches; `load_app_config`
succeeds; `APP_DEMO__SERVICE__TIMEOUT_MS` overrides the nested value
in a test.

## 10. Sub-project 4 — `config validate` command

```rust
#[derive(clap::Args, Default, Debug)]
#[non_exhaustive]
pub struct ConfigValidateArgs {
    #[arg(long, default_value = "edgezero.toml")] pub manifest: PathBuf,
    #[arg(long)] pub app_config: Option<PathBuf>,
    #[arg(long)] pub strict: bool,
    #[arg(long)] pub no_env: bool,
}
```

Bound: `DeserializeOwned + Validate + AppConfigMeta` (no `Serialize`).

App-config validation: TOML syntax; deserialises into `C`; types;
`validator` rules; unknown fields rejected when `C` opts in;
`#[secret]` non-empty; `#[secret(store_ref)]` in
`[stores.secrets].ids`. **When `spin` is in the adapter set**, three
additional Spin checks (all per §6.7):

1. every flattened config key, `.`→`__` translated, matches
   `^[a-z][a-z0-9_]*$` — **typed and raw** (both flavours have the
   config keys);
2. the effective Spin variable name set — {flattened config keys} ∪
   {`#[secret]` field values}, after `.`→`__` translation — has no
   duplicate (config/secret namespace collision check). **Typed
   only** — `#[secret]` fields are identified via
   `AppConfigMeta::SECRET_FIELDS`, which the raw flavour does not
   have. `run_config_validate` (raw) cannot tell which keys are
   secrets, so it performs check 1 and check 3 but **not** check 2;
   its diagnostics say so. The collision check is therefore guaranteed
   only for the typed path, which is the one downstream CLIs wire up;
3. Spin component discovery resolves (exactly one `[component.*]` in
   `spin.toml`, or an explicit, matching `[adapters.spin.adapter]
.component`) — **typed and raw** (manifest-based, no struct
   needed).

Manifest: `ManifestLoader` checks; under `--strict`, capability-aware
completeness and well-formed handler paths.

**Tests:** dedicated fixtures per failure mode incl. all three Spin
checks above (key-syntax, collision, component discovery); env-overlay
on/off.

**Ship gate:** `app-demo-cli config validate --strict` exits 0;
corrupted fixtures fail with expected messages.

## 11. Sub-project 5 — `auth` command (adapter-trait dispatch)

```rust
#[derive(clap::Args, Debug)]      // NO Default — §6.11
#[non_exhaustive]
pub struct AuthArgs { #[command(subcommand)] pub sub: AuthSub }

#[derive(clap::Subcommand, Debug)]
pub enum AuthSub {
    Login  { #[arg(long)] adapter: String },
    Logout { #[arg(long)] adapter: String },
    Status { #[arg(long)] adapter: String },
}
```

UX: `auth login --adapter cloudflare`. Dispatch follows the same
path as `build` / `deploy` / `serve`: `AdapterAction::AuthLogin` /
`AuthLogout` / `AuthStatus` extend the existing
`edgezero_adapter::registry::AdapterAction` enum, and each
`edgezero-adapter-*` crate implements the variants in its own
`Adapter::execute` impl (shell out, HTTP call, or no-op — the CLI
doesn't care). Per-project override via
`[adapters.<name>.commands].auth-{login,logout,status}` in
`edgezero.toml`, same precedence as `build` / `deploy` / `serve`.

Built-ins (each in its adapter crate):

- axum: no-op (no remote auth surface).
- cloudflare: `wrangler login/logout/whoami`.
- fastly: `fastly profile create/delete/list`.
- spin: `spin cloud login/logout/info`.

The standalone `CommandRunner` indirection originally sketched here
was dropped: each adapter chooses its own implementation mechanism
and is responsible for its own testability. The CLI's `auth.rs` is
a five-line args-to-action delegate to `adapter::execute`.

**Tests:** the orchestration test mirrors `build`/`deploy`/`serve` —
configure `[adapters.<name>.commands].auth-login = "echo logged in"`
in a fixture manifest and assert dispatch succeeds. The real native
CLIs are not exercised in CI (§13).

## 12. Sub-project 6 — `provision` command

```rust
#[derive(clap::Args, Default, Debug)]
#[non_exhaustive]
pub struct ProvisionArgs {
    #[arg(long, default_value = "edgezero.toml")] pub manifest: PathBuf,
    #[arg(long)] pub adapter: String,
    #[arg(long)] pub dry_run: bool,
}
```

Iterate every id in `[stores.<kind>].ids`. Per-adapter behaviour:

**axum** — no remote resources. `provision --adapter axum` is an
explicit no-op: it prints, for each store, "axum store `<id>` is local
(KV in-memory; config in `.edgezero/local-config-<id>.json`; secrets
from env vars) — nothing to provision." Exit 0.

**cloudflare** — for KV and config ids: `wrangler kv namespace create
<name>`; parse the namespace id from stdout; patch `wrangler.toml`
`[[kv_namespaces]] binding = "<name>"`, `id = "<extracted>"`. Secrets:
no-op (worker secrets are runtime-managed via `wrangler secret put`).

**fastly** — for each id: `fastly <kind>-store create --name=<name>`;
ensure `fastly.toml` contains `[setup.<kind>_stores.<name>]` and
`[local_server.<kind>_stores.<name>]` table entries (keyed by the
resource-link name = our `name`). Store IDs are not persisted; `config
push` resolves them on demand (§13).

**spin** — no remote `create` step (Spin KV stores and variables are
provisioned by the Spin runtime / Fermyon at deploy). `provision
--adapter spin` performs **KV-label `spin.toml` writeback only**:

- KV: ensure each KV label (`[adapters.spin.stores.kv.<id>].name`)
  appears in the resolved component's `key_value_stores` array field
  (`key_value_stores = [...]` under `[component.<component>]`).
- **Config and secret variables are NOT handled by `provision`.** The
  manifest only carries store _ids_, not app-config field keys or
  secret key names — `provision` cannot know which Spin variables to
  declare. Config-variable declaration is done by `config push
--adapter spin` (which loads `<name>.toml` and therefore knows the
  keys; see §13). Secret-variable declaration is **manual** — the
  developer declares Spin secret variables in `spin.toml` themselves
  (§6.7); the CLI never writes secret variables.

Component resolution for the KV writeback follows §6.7's rule. No
shell-out for Spin — it is pure manifest editing.

`--dry-run` prints the would-be commands and would-be manifest
edits without performing them.

**Tests:** each adapter crate owns its per-(adapter, kind) writeback
tests (temp-fixture writeback for `wrangler.toml`, `fastly.toml`,
and the Spin `key_value_stores` array in `spin.toml`; axum no-op
output asserted). The CLI's orchestration test asserts dispatch
and `--dry-run` short-circuits without invoking the adapter;
`--dry-run` performs nothing.

## 13. Sub-project 7 — `config push` command

```rust
#[derive(clap::Args, Default, Debug)]
#[non_exhaustive]
pub struct ConfigPushArgs {
    #[arg(long, default_value = "edgezero.toml")] pub manifest: PathBuf,
    #[arg(long)] pub adapter: String,
    #[arg(long)] pub store: Option<String>,         // logical config id; default resolved
    #[arg(long)] pub app_config: Option<PathBuf>,
    #[arg(long)] pub no_env: bool,
    #[arg(long)] pub dry_run: bool,
}
```

Bound: `DeserializeOwned + Validate + Serialize + AppConfigMeta`.

**Behaviour:** strict pre-flight validation; load app-config (env
overlay unless `--no-env`); flatten + serialise per §6.4/§6.5 (skip
`SECRET_FIELDS`); resolve target id (`--store` or resolved default).
Push is **split by adapter** — there is no single "resource-ID" model:

| Adapter    | Push behaviour                                                                                                                                                                                                            |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| axum       | Write resolved values to `.edgezero/local-config-<id>.json` (the file the axum config store reads, §15). No runner call.                                                                                                  |
| cloudflare | Read the namespace id from `wrangler.toml` (error "did you run `provision`?" if absent); `wrangler kv bulk put <tempfile.json> --namespace-id=<id>`. Keys in dotted form.                                                 |
| fastly     | Resolve the store id on demand: `fastly config-store list --json`, match by `<name>`; per key `fastly config-store-entry create --store-id=<id> --key=<k> --value=<v>` (`--stdin` for large values). Keys in dotted form. |
| spin       | Declare + set each config value as a Spin variable, writing **both** `spin.toml` tables (see below). Keys in `.`→`__` lowercase form (§6.7). No remote call — live Fermyon Cloud variable push is out of scope (§2).      |

**Spin `config push` writes two `spin.toml` tables.** A Spin variable
is not readable by a component unless it is both _declared_ and
_bound_. `config push --adapter spin` therefore writes:

1. `[variables].<key>` — the application-level variable declaration,
   with `default = "<resolved value>"`.
2. `[component.<component>.variables].<key>` — the component binding,
   `<key> = "{{ <key> }}"`, surfacing the application variable into the
   component. Without this, the component cannot read the variable.

If the component-bindings table is missing entries for keys this push
needs and `config push` cannot resolve the component (§6.7), it
errors rather than writing a half-configured manifest. The component
is resolved per §6.7's discovery rule. Config-variable _declaration_
lives here (not in `provision`) because only `config push` loads
`<name>.toml` and thus knows the keys. Secret variables remain manual
(§6.7) — `config push` skips `SECRET_FIELDS` and never writes secret
variables.

**Tests:** typed + raw; per-adapter mock-runner / fixture with golden
payloads; `#[secret]` / `#[secret(store_ref)]` absent from payload;
missing native-manifest id (cloudflare) → clear error; Spin key
`.`→`__` translation asserted; Spin writeback updates **both**
`[variables]` and `[component.<component>.variables]`; Spin push errors
when the component cannot be resolved; `--store` selection; `--dry-run`
performs nothing; env-overlay on vs `--no-env`. **Explicit "validate
passes, push serialization fails" cases:** non-object typed config,
unsupported compound shape, `skip_serializing_if`, `Option::None`,
`#[serde(flatten)]` on a non-secret field.

**Spin `spin.toml` golden test.** A golden-file test captures the
generated `spin.toml` after a Spin `config push` and asserts: every
written variable name matches `^[a-z][a-z0-9_]*$` (§6.7); the
generated manifest **parses** (round-trips through the same TOML /
Spin-manifest parser the runtime uses), so the `^[a-z][a-z0-9_]*$`
rule cannot silently drift from Spin's actual manifest behaviour.

**Validation strength, strongest first:** the test uses the strongest
check available in its environment. (1) If the `spin` CLI is present
(the wasm32 spin CI job already installs it), the test runs Spin's own
manifest validation against the generated file — this is authoritative
and catches semantic errors a plain TOML parse cannot. (2) Else if
`spin_sdk` exposes a manifest-validation entry point, it calls that.
(3) Otherwise it falls back to `toml` parsing + the variable-name
regex. The regex is the **floor**, not the ceiling — the
implementation prefers real Spin validation wherever it is reachable
and treats the TOML-only fallback as the weakest acceptable check.
The golden file is regenerated only on an intentional format change.

**Ship gate:** `app-demo-cli config push --adapter cloudflare
--dry-run` and `--adapter spin --dry-run` each show the expected
output; secret fields absent; Spin keys `__`-encoded.

## 14. (reserved — sub-project numbering uses the `#` column in §16)

## 15. Sub-project 8 — `app-demo` integration polish (all four adapters)

**Goal:** `app-demo` demonstrates the **full** feature set in CI across
all four adapters.

- **Extensible CLI:** `app-demo-cli` with the four downstream built-ins plus
  `Auth`, `Provision`, `Config` (`Validate` / `Push`); the `Config`
  arm wired to the **typed** functions with `AppDemoConfig`.
- **Multi-store manifest + runtime:** `edgezero.toml` declares 2 KV ids
  (`sessions`, `cache`), one config id, one secrets id, with per-adapter
  mappings for **all four** adapters (Spin KV labels included). The
  Spin capability rule is satisfied (one config id, one secrets id).
- **Multi-store runtime:** handlers read both `sessions` and `cache`
  via the `Kv` extractor's `named()`.
- **Async config:** a handler does
  `ctx.config_store_default()?.get("greeting").await?`.
- **Nested config + Spin key encoding:** `AppDemoConfig.service.
timeout_ms` is read at runtime; the Spin path proves `.`→`__`
  translation.
- **Env-var override:** an integration test sets
  `APP_DEMO__SERVICE__TIMEOUT_MS` and asserts the override.
- **Secrets:** one `#[secret]` (`api_token`) and one
  `#[secret(store_ref)]` (`vault`); a handler reads each. `app-demo`
  targets all four adapters, so `[stores.secrets].ids` has exactly one
  id (§6.6 capability rule) and the `vault` field's value **is** that
  single secrets id — the walkthrough doc explicitly shows
  `#[secret(store_ref)]` resolving to the one declared id for an
  all-four-adapter app (§6.8). `app-demo`'s `spin.toml` **manually
  declares** its Spin secret variables (with `secret = true`, bound
  under `[component.<component>.variables]`), demonstrating the §6.7
  manual-secret rule. The `app-demo-core` handler keeps its
  `#[secret(store_ref)]` runtime key clear of every config key so the
  Spin flat namespace does not collide.
- **Spin component:** `app-demo`'s `spin.toml` is single-component, so
  component discovery resolves implicitly; the walkthrough doc also
  shows the explicit `[adapters.spin.adapter].component` form.
- **`config validate` / `config push`:** CI runs `config validate
--strict` (exit 0 — including the three Spin checks of §10) then
  `config push --adapter axum` and reads the value back through a
  running axum demo server on `/config/greeting`. `config push
  --adapter spin --dry-run` is asserted to **print** the would-be
  `__`-encoded keys and the would-be content of **both** `spin.toml`
  tables — and the on-disk `spin.toml` is asserted **unchanged**
  (dry-run never mutates). The non-dry-run Spin push writing both
  tables is covered by stage 7's tests, not the dry-run assertion.
- **`auth` / `provision`:** dispatch tests in `edgezero-cli` use
  fixture manifests with `auth-login = "echo logged in"` (etc.) and
  assert that `adapter::execute` is reached for the right
  `AdapterAction`. The actual native-CLI invocation and any manifest
  writeback live in each adapter crate's own tests (temp-fixture
  writeback for `wrangler.toml`, `fastly.toml`, and the Spin
  `key_value_stores` array in `spin.toml`). Spin `provision` is
  asserted to write only the `key_value_stores` array, not
  variables.

**Axum config store backing.** The axum config store is backed by
`.edgezero/local-config-<id>.json` (gitignored). `config push
--adapter axum` writes it from `<name>.toml` (env overlay applied);
the axum config store reads the same file; `edgezero demo` regenerates
it at startup. If absent, the axum config store is empty.

**Docs:** create `docs/guide/cli-walkthrough.md` (full `myapp` loop —
`new`, `auth`, `provision`, `config validate`, `config push`, `deploy`,
the `demo` subcommand, an env-override example, all four adapters,
including the manual Spin secret-variable `spin.toml` entries and the
explicit `[adapters.spin.adapter].component` form). Update
`docs/.vitepress/config.mts` so the sidebar lists `cli-walkthrough.md`
and `manifest-store-migration.md`.

**Documentation audit (§6.12).** Stage 8 finishes with a docs audit:
grep `docs/` for stale references — old `[stores.*]` manifest keys,
the `dev` subcommand, the pre-rewrite single-store runtime API — and
confirm none remain; confirm every page in §6.12's table was updated
by its owning stage; confirm the docs CI (ESLint + Prettier) passes.

**Ship gate:** CI runs the full loop on axum end-to-end; manifest /
runtime behaviour for cloudflare, fastly, and spin is covered by
contract + mock tests; the documentation audit passes with zero stale
references.

---

## 16. Implementation order and milestones

The whole effort is **a single pull request containing eight stages**,
one per sub-project, applied in this order:

| Stage | §   | Title                                                  | Risk |
| ----- | --- | ------------------------------------------------------ | ---- |
| 1     | §7  | Extensible lib + scaffold                              | M    |
| 2     | §8  | Manifest + runtime rewrite (atomic, all four adapters) | H    |
| 3     | §9  | App-config schema + derive macro + env-overlay loader  | M    |
| 4     | §10 | `config validate`                                      | L    |
| 5     | §11 | `auth` (adapter-trait dispatch)                        | M    |
| 6     | §12 | `provision`                                            | H    |
| 7     | §13 | `config push`                                          | M    |
| 8     | §15 | `app-demo` polish (all four adapters) + docs audit     | M    |

Every stage also updates the `docs/guide/` pages it makes stale
(§6.12) — documentation is part of each stage's definition-of-done,
not a deferred afterthought. Stage 8 closes with a documentation
audit.

**CI and bisectability.** CI gates the PR as a whole on its head
commit; all four gates (`fmt`, `clippy -D warnings`, `cargo test`,
feature `cargo check`) plus the wasm32 spin gate must pass there. Each
of the eight stages should nonetheless compile and pass tests on its
own so the history stays bisectable — stage boundaries are chosen so
that each is a self-contained, buildable increment. Stage 2 is the one
unavoidably large stage (the atomic manifest+runtime rewrite); the
other seven are individually small.

**Review note.** Because this is one PR, the reviewer sees all eight
stages together. The PR description should list the eight stages and
point at this spec. Reviewing stage-by-stage is recommended.
**Stage 2 is the review hotspot** — the atomic manifest+runtime
rewrite is intentionally large (the hard cutoff leaves no smaller
coherent unit), so it warrants the most reviewer attention. Its
per-adapter contract tests (§8) are the primary mitigation and should
be reviewed alongside the code.

**Highest-risk:** stage 2 — atomic manifest+runtime rewrite touching the
schema, `ConfigStore` (async), **all four** adapters' store impls, the
Cloudflare `[vars]`→KV swap, Spin store wiring, `Hooks` /
`ConfigStoreMetadata` / `app!`, and the extractors, in one stage.
Large by necessity under the hard-cutoff decision. Mitigated by
per-adapter contract tests and `app-demo` as the in-tree canary.
Stage 6 (`provision`) — shell-out + multi-file native-manifest
writeback across four adapters (`wrangler.toml`, `fastly.toml`,
`spin.toml`).

## 17. Risks and trade-offs

- **Hard manifest cutoff:** a pre-rewrite `edgezero.toml` fails to
  load with a migration-guide error. All in-tree projects migrated in
  stage 2; external projects migrate once.
- **Large atomic stage (stage 2):** unavoidable without a
  compatibility layer, which the hard-cutoff decision rejects. It is
  one stage, not one PR — the PR carries all eight.
- **Async `ConfigStore` cascade:** `get` becomes async across the
  trait and **all four** adapter impls, handlers, and the `Config`
  extractor. `#[async_trait(?Send)]` keeps WASM compatibility.
- **Cloudflare `[vars]`→KV swap:** deployed workers migrate once.
- **Spin model asymmetry:** Spin config/secrets are a single flat
  variable namespace; multi-config/multi-secret projects cannot target
  Spin. The capability matrix (§6.6) enforces this at validate time
  with a clear error. Spin config keys are `__`-encoded lowercase.
- **Spin config is build-time:** `config push --adapter spin` writes
  static `spin.toml` variables; changing them needs a redeploy. Live
  Spin variable providers are out of scope (§2).
- **Spin secret variables are manual:** the CLI never declares Spin
  secret variables (their key names are not reliably knowable, §6.7).
  A project targeting Spin must declare them in `spin.toml` by hand;
  the walkthrough doc covers this. `#[secret(store_ref)]` is the
  awkward case on Spin (single flat secret namespace, code-local
  keys) — supported, but the developer owns the `spin.toml` entries.
- **Spin KV TTL / listing-cap:** stage 2 adds two new `KvError`
  variants — `Unsupported` (Spin TTL writes) and `LimitExceeded`
  (Spin listing past `max_list_keys`) — both 5xx-class in their
  `EdgeError` mapping. Spin TTL writes return `Unsupported`
  deterministically (not silent); the Spin listing path returns
  `LimitExceeded`, replacing PR #253's `KvError::Validation` for that
  case. Both are settled in this spec, not left open.
- **Spin component discovery:** writing `[component.<name>.*]` tables
  needs the component id; single-component `spin.toml` resolves
  implicitly, multi-component requires `[adapters.spin.adapter]
.component`. `config validate --strict` surfaces a failure early.
- **Env overlay surprising `config push`:** `--no-env` is the escape
  hatch.
- **Shell-out + ID-writeback fragility:** current platform syntax
  pinned; golden parser tests; `--dry-run` available.
- **Extractor breaking change:** `Kv(handle)` → `kv.default()`; only
  in-tree consumer is `app-demo`.
- **API stability:** non-subcommand `*Args` are `#[non_exhaustive]` +
  `Default`; `AuthArgs` without `Default`.

## 18. What this spec does not cover

- Anthropic credentials, edge DNS / TLS, observability / metrics.
- Per-environment config _files_ (env-var override is in scope).
- Restructuring `app-demo-core` handlers beyond what §15 requires.
- `edgezero-core` changes beyond `app_config`, the rewritten
  `manifest` / `RequestContext` / `Hooks` / `ConfigStore` (async) /
  extractor / `ConfigStoreMetadata` / `app!` surface, and the
  Cloudflare adapter config backend.
- A migration _tool_; migration is manual via the published guide.
- Dynamic Spin variable providers (Fermyon Cloud variable push, Vault).

When all eight sub-projects ship, `edgezero new myapp` produces a
workspace with `myapp-cli`, a typed `MyappConfig`
(`#[derive(AppConfig)]`, `#[serde(deny_unknown_fields)]`, optional
`#[secret]` / `#[secret(store_ref)]`), a `myapp.toml`, and an
`edgezero.toml` using the new logical-store schema with capability-
correct store declarations. The developer authenticates, provisions,
validates, pushes config (with optional env overrides), and deploys.
At runtime the service reads config (async) and secrets by logical id
across all four adapters. `app-demo` demonstrates every capability in
CI.
