# Configuration

The `edgezero.toml` manifest describes an EdgeZero application, providing a single source of truth for routing, middleware, adapters, and environment configuration.

## Overview

New workspaces scaffolded with `edgezero new` include this manifest by default. The manifest drives both runtime routing and CLI commands.

```toml
[app]
name = "my-app"
entry = "crates/my-app-core"
middleware = ["edgezero_core::middleware::RequestLogger"]

[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "my_app_core::handlers::root"

[adapters.fastly]
# Fastly-specific configuration

[adapters.cloudflare]
# Cloudflare-specific configuration
```

## App Section

The `[app]` section defines application metadata:

```toml
[app]
name = "demo"
entry = "crates/demo-core"
middleware = ["edgezero_core::middleware::RequestLogger"]
```

| Field        | Required | Description                                                          |
| ------------ | -------- | -------------------------------------------------------------------- |
| `name`       | No       | Display name for the application (defaults to "EdgeZero App")        |
| `entry`      | No       | Path to the core crate containing handlers (recommended for tooling) |
| `version`    | No       | Reserved for future compatibility; currently ignored                 |
| `kind`       | No       | Reserved for future compatibility; currently ignored                 |
| `middleware` | No       | List of middleware to apply globally                                 |

### Middleware

Manifest-driven middleware are applied in order before routes:

```toml
[app]
middleware = [
  "edgezero_core::middleware::RequestLogger",
  "my_app_core::cors::Cors"
]
```

Each item must be:

- A publicly accessible path
- Either a unit struct or zero-argument constructor
- Implementing `edgezero_core::middleware::Middleware`

## HTTP Triggers

The `[[triggers.http]]` array defines routes:

```toml
[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "my_app_core::handlers::root"

[[triggers.http]]
id = "echo"
path = "/echo/{name}"
methods = ["GET", "POST"]
handler = "my_app_core::handlers::echo"
adapters = ["fastly", "cloudflare"]
body-mode = "buffered"
```

| Field         | Required | Description                                                  |
| ------------- | -------- | ------------------------------------------------------------ |
| `id`          | No       | Stable identifier for tooling                                |
| `path`        | Yes      | URI template (`{param}` for params, `{*rest}` for catch-all) |
| `methods`     | No       | Allowed HTTP methods (defaults to `GET`)                     |
| `handler`     | No       | Path to handler function (required for `app!` route wiring)  |
| `adapters`    | No       | Intended adapter filter (metadata; `app!` currently ignores) |
| `description` | No       | Human-readable description for docs or tooling               |
| `body-mode`   | No       | `buffered` or `stream`                                       |

::: tip Adapter filters
The `adapters` field is currently metadata for tooling; `app!` wires all triggers regardless of adapter.
:::

## Environment Section

Declare environment variables and secrets:

```toml
[environment]

[[environment.variables]]
name = "API_BASE_URL"
env = "API_BASE_URL"
value = "https://example.com/api"

[[environment.secrets]]
name = "API_TOKEN"
adapters = ["fastly", "cloudflare"]
env = "API_TOKEN"
```

### Variables

| Field         | Required | Description                          |
| ------------- | -------- | ------------------------------------ |
| `name`        | Yes      | Variable name in application         |
| `description` | No       | Human-readable description           |
| `env`         | No       | Environment key (defaults to `name`) |
| `value`       | No       | Default value                        |
| `adapters`    | No       | Limit to specific adapters           |

Variables with a default `value` are injected when running CLI commands.

### Secrets

| Field         | Required | Description                          |
| ------------- | -------- | ------------------------------------ |
| `name`        | Yes      | Secret name in application           |
| `description` | No       | Human-readable description           |
| `env`         | No       | Environment key (defaults to `name`) |
| `adapters`    | No       | Limit to specific adapters           |

Secrets must be present in the environment; missing secrets abort CLI commands with an error.

These declarations are for CLI and deployment workflows. To expose a runtime
secret store to request handlers, configure `[stores.secrets]`.

## Runtime Secret Stores

Use `[stores.secrets]` when your application reads secrets at request time via
the `Secrets` extractor. This is separate from `[[environment.secrets]]`:

- `[[environment.secrets]]` declares required environment variables for CLI commands
- `[stores.secrets]` enables runtime secret lookup during request handling

```toml
[stores.secrets]
ids     = ["default"]            # one id per logical secret store
# default = "default"            # required when ids.len() > 1
```

The portable `[stores.<kind>]` schema declares **logical ids only**.
Platform names are resolved at runtime from
`EDGEZERO__STORES__SECRETS__<ID>__NAME` (defaulting to the logical id
when unset). Migrating from the pre-rewrite `name` /
`[stores.secrets.adapters.*]` form? See
[the migration guide](./manifest-store-migration.md).

### Adapter Behavior

| Adapter    | Capability                       | Notes                                                                            |
| ---------- | -------------------------------- | -------------------------------------------------------------------------------- |
| axum       | Single (env vars)                | Every declared id maps to the same env-backed store                              |
| cloudflare | Single (Worker Secrets)          | Per-id `NAME` variables are ignored                                              |
| fastly     | Multi (Fastly Secret Store)      | Each id opens its own platform store via `EDGEZERO__STORES__SECRETS__<ID>__NAME` |
| spin       | Single (flat Spin `[variables]`) | Per-id `NAME` variables are ignored                                              |

If `[stores.secrets]` is omitted, the `Secrets` extractor is not attached and
the runtime `secret_store` accessors on `RequestContext` return `None`.

## Stores Section

Use `[stores.config]` for small read-only runtime configuration such as feature flags, JWKS metadata,
or service settings:

```toml
[stores.config]
ids     = ["app_config"]         # one id per logical config store
# default = "app_config"         # required when ids.len() > 1
```

The portable schema is symmetric across `[stores.kv]`, `[stores.config]`,
and `[stores.secrets]`: declare logical `ids` only; resolve platform
names at runtime via `EDGEZERO__STORES__<KIND>__<ID>__NAME`. The
pre-rewrite `name`, `enabled`, `[stores.config.defaults]`, and
`[stores.config.adapters.*]` fields are a hard load error — see
[the migration guide](./manifest-store-migration.md).

Runtime behavior by adapter:

- Fastly reads from a Fastly Config Store resource link, one per id.
- Cloudflare reads from a KV namespace, one per id, asynchronously.
- Axum reads from `.edgezero/local-config-<id>.json` per logical id
  (one file per declared config id). Seed entries with
  `config push --adapter axum`, which writes the same file the
  runtime reads (creates `.edgezero/` on first use). No shell-out;
  the file is human-editable for ad-hoc tweaks.
- Spin reads a `spin_sdk::key_value::Store` per id, one label per
  declared `[stores.config]` id (multi-store). Labels must be declared
  in `spin.toml`'s `[component.<id>].key_value_stores` — `provision`
  writes them automatically. Seed entries via `config push --adapter
spin`, which dispatches per-backend by reading `runtime-config.toml`:
  `type = "spin"` → direct write into the local `.spin/sqlite_key_value.db`;
  Fermyon Cloud deploys → shell `spin cloud key-value set`; other
  backend types (redis, azure_cosmos) print an actionable error
  pointing at the backend's native CLI. See
  [Spin adapter](/guide/adapters/spin#seeding-the-store) for the
  full resolution order.

When `[stores.config]` is present, the `app!` macro bakes the portable
store registry into `Hooks::stores()`. Adapter `run_app` helpers build
a per-request `ConfigRegistry` and inject it into request extensions so
handlers can call `ctx.config_store("app_config")` (or
`ctx.config_store_default()`).

Treat config-store keys like API surface: validate or allowlist any user-controlled lookup before
calling `ctx.config_store_default()?.get(...)`.

## Application config

`edgezero.toml` describes the _shape_ of the app — routes, adapters,
stores. A separate `<name>.toml` file (e.g. `my-app.toml`, sitting
alongside `edgezero.toml`) carries the _typed values_ the app reads at
request time: feature flags, timeouts, the keys it uses to look up
secrets. `edgezero new` generates both, plus a `<Name>Config` struct
in `crates/<name>-core/src/config.rs` that the file deserialises into.

```rust
// crates/my-app-core/src/config.rs
use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
pub struct MyAppConfig {
    pub greeting: String,
    pub service: ServiceConfig,

    #[secret]
    pub api_token: String,
}

#[derive(Debug, Deserialize, Serialize, Validate)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    #[validate(range(min = 100, max = 60_000))]
    pub timeout_ms: u32,
}
```

```toml
# my-app.toml — loaded into MyAppConfig
greeting = "hello from my-app"
api_token = "demo_api_token"   # key into the default secret store

[service]
timeout_ms = 1500
```

The file's top-level table maps 1:1 to the struct — no `[config]`
wrapper. `deny_unknown_fields` makes typos in the TOML a hard load
error rather than a silent drop.

### Loading the config

```rust
use edgezero_core::app_config::load_app_config;

let cfg: MyAppConfig =
    load_app_config(std::path::Path::new("my-app.toml"), "my-app")?;
```

The function deserialises, runs the `validator` rules (e.g.
`#[validate(range(...))]`), and returns the typed struct.

### Secret annotations

| Attribute              | Meaning                                                                                        |
| ---------------------- | ---------------------------------------------------------------------------------------------- |
| `#[secret]`            | The field's value is a **key inside the default secret store** declared by `[stores.secrets]`. |
| `#[secret(store_ref)]` | The field's value is a **logical store id** that must appear in `[stores.secrets].ids`.        |

Only bare `String` fields can carry a `#[secret]` annotation;
combining it with `#[serde(flatten)]`, `#[serde(rename)]`, or
`#[serde(skip)]` is a compile error. The `config validate` command
(see [CLI reference](/guide/cli-reference)) checks that every
`#[secret(store_ref)]` value matches a declared id.

Resolve secrets at request time from the secret store:

```rust
// #[secret] field — key in the default store
let token = ctx
    .secret_store_default()?
    .require_str(&cfg.api_token)
    .await?;

// #[secret(store_ref)] field — value names the store itself
let value = ctx
    .secret_store(&cfg.vault)?
    .require_str("active")
    .await?;
```

### Environment-variable overlay

Every key in `<name>.toml` can be overridden at runtime by an env
var named `<APP_NAME>__<SECTION>__…__<KEY>` (uppercase, with `-` in
the app name replaced by `_`, segments joined by a double-underscore).
The overlay only applies to keys **already present in the file** —
it can't introduce new ones — and the existing TOML value's type
drives how the env string is coerced (`"true"` / `"false"` for
`bool`, parsed integers for numeric fields, etc.).

```sh
# Override the nested service.timeout_ms key:
MY_APP__SERVICE__TIMEOUT_MS=2500 \
    cargo run -p my-app-adapter-axum
```

The env-segment translation is uppercase-only — it does **not**
substitute `-` for `_`, so dashed and underscored TOML keys remain
distinct env segments. The only way two siblings collapse is when
they differ only in letter case (e.g. `greeting_a` and `GREETING_A`,
both uppercasing to `GREETING_A`). That case is rejected as an
`EnvOverlay` error before any override is applied, so a
misconfiguration leaves the file values intact.

## Adapters Section

Each adapter has its own configuration block:

```toml
[adapters.fastly.adapter]
crate = "crates/demo-adapter-fastly"
manifest = "crates/demo-adapter-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"

[adapters.fastly.commands]
build = "cargo build --release --target wasm32-wasip1 -p demo-adapter-fastly"
serve = "fastly compute serve -C crates/demo-adapter-fastly"
deploy = "fastly compute deploy -C crates/demo-adapter-fastly"

[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true
```

### Adapter Metadata

| Field      | Description                                            |
| ---------- | ------------------------------------------------------ |
| `crate`    | Path to adapter crate                                  |
| `manifest` | Path to provider manifest (fastly.toml, wrangler.toml) |

### Build Configuration

| Field      | Description                      |
| ---------- | -------------------------------- |
| `target`   | Rust compilation target          |
| `profile`  | Build profile (`release`, `dev`) |
| `features` | Cargo features to enable         |

### Commands

| Field    | Description                                    |
| -------- | ---------------------------------------------- |
| `build`  | Command for `edgezero build --adapter <name>`  |
| `serve`  | Command for `edgezero serve --adapter <name>`  |
| `deploy` | Command for `edgezero deploy --adapter <name>` |

When commands are omitted, the CLI falls back to built-in adapter helpers.

### Logging

Logging can be configured per adapter under `[adapters.<name>.logging]` or via a top-level
`[logging.<name>]` block. If both are present, the adapter-specific block takes precedence.

| Field         | Adapters     | Description                                                 |
| ------------- | ------------ | ----------------------------------------------------------- |
| `endpoint`    | Fastly       | Log endpoint name                                           |
| `level`       | All          | Log level: `trace`, `debug`, `info`, `warn`, `error`, `off` |
| `echo_stdout` | Fastly, Axum | Mirror logs to stdout                                       |

Note: Cloudflare logging is not wired to a built-in logger yet.

## Full Example

```toml
[app]
name = "my-app"
entry = "crates/my-app-core"
middleware = [
  "edgezero_core::middleware::RequestLogger",
  "my_app_core::middleware::Cors"
]

[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "my_app_core::handlers::root"

[[triggers.http]]
id = "echo"
path = "/echo/{name}"
methods = ["GET"]
handler = "my_app_core::handlers::echo"

[[triggers.http]]
id = "api"
path = "/api/{*rest}"
methods = ["GET", "POST", "PUT", "DELETE"]
handler = "my_app_core::handlers::api_proxy"
body-mode = "stream"

[environment]

[[environment.variables]]
name = "API_URL"
value = "https://api.example.com"

[[environment.secrets]]
name = "API_KEY"

[stores.secrets]
ids = ["default"]

[adapters.fastly.adapter]
crate = "crates/my-app-adapter-fastly"
manifest = "crates/my-app-adapter-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"

[adapters.fastly.commands]
build = "fastly build -C crates/my-app-adapter-fastly"
deploy = "fastly compute deploy -C crates/my-app-adapter-fastly"
serve = "fastly compute serve -C crates/my-app-adapter-fastly"

[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true

[adapters.cloudflare.adapter]
crate = "crates/my-app-adapter-cloudflare"
manifest = "crates/my-app-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.build]
target = "wasm32-unknown-unknown"
profile = "release"

[adapters.cloudflare.commands]
build = "wrangler deploy --dry-run --cwd crates/my-app-adapter-cloudflare"
deploy = "wrangler deploy --cwd crates/my-app-adapter-cloudflare"
serve = "wrangler dev --cwd crates/my-app-adapter-cloudflare"

[adapters.cloudflare.logging]
level = "info"

[adapters.axum.adapter]
crate = "crates/my-app-adapter-axum"
manifest = "crates/my-app-adapter-axum/axum.toml"
host = "127.0.0.1"
port = 8787

[adapters.axum.commands]
build = "cargo build --release -p my-app-adapter-axum"
serve = "cargo run -p my-app-adapter-axum"
```

Axum bind-address precedence is:

1. `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT` (canonical;
   read directly by the runtime). The pre-rewrite
   `EDGEZERO_HOST` / `EDGEZERO_PORT` shim is gone — rename any CI
   scripts or local overrides to the canonical double-underscore
   form.
2. `edgezero.toml` `[adapters.axum.adapter]` `host` / `port` (the CLI
   translates these into `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`
   when spawning the subprocess; if a canonical env var is already set,
   it wins)
3. `axum.toml` `[adapter]` `host` / `port` when launching through the
   Axum adapter CLI wrapper
4. default `127.0.0.1:8787`

Example override:

```sh
EDGEZERO__ADAPTER__HOST=0.0.0.0 EDGEZERO__ADAPTER__PORT=3000 \
    cargo run -p my-app-adapter-axum
```

## Using the Manifest

### app! Macro

Generate router wiring from the manifest:

```rust
// In your core crate's lib.rs
mod handlers;

edgezero_core::app!("../../edgezero.toml");
```

The macro:

- Parses HTTP triggers
- Generates route registration
- Wires middleware from the manifest
- Bakes portable store metadata (`Hooks::stores()`) from `[stores.kv]`, `[stores.config]`, and `[stores.secrets]` when present
- Creates the `App` struct that implements `Hooks` (use `App::build_app()`)

### ManifestLoader

Load the manifest programmatically:

```rust
use edgezero_core::manifest::ManifestLoader;
use std::path::Path;

let manifest = ManifestLoader::from_path(Path::new("edgezero.toml"))?;
let app_name = manifest
    .manifest()
    .app
    .name
    .as_deref()
    .unwrap_or("EdgeZero App");
println!("App name: {}", app_name);
```

## Validation

`ManifestLoader` validates:

- Non-empty string fields when present (names, paths, commands)
- Supported HTTP methods and `body-mode` values
- Well-formed logging levels and adapter logging config

Errors are surfaced at startup or during macro expansion.

## Next Steps

- Learn about [CLI commands](/guide/cli-reference)
- Explore [adapter-specific configuration](/guide/adapters/overview)
