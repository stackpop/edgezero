# Configuration

The `edgezero.toml` manifest describes an EdgeZero application, providing a single source of truth for routing, middleware, adapters, and environment configuration.

## Overview

New workspaces scaffolded with `edgezero new` include this manifest by default. The manifest drives both runtime routing and CLI commands.

```toml
[app]
name = "my-app"
version = "0.1.0"
kind = "http"
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
version = "0.1.0"
kind = "http"
entry = "crates/demo-core"
middleware = ["edgezero_core::middleware::RequestLogger"]
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Display name for the application |
| `version` | No | Semantic version |
| `kind` | No | Application type (currently only `http`) |
| `entry` | Yes | Path to the core crate containing handlers |
| `middleware` | No | List of middleware to apply globally |

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

| Field | Required | Description |
|-------|----------|-------------|
| `id` | No | Stable identifier for tooling |
| `path` | Yes | URI template (`{param}` for params, `{*rest}` for catch-all) |
| `methods` | No | Allowed HTTP methods (defaults to `GET`) |
| `handler` | Yes | Path to handler function |
| `adapters` | No | Which adapters expose this route (empty = all) |
| `body-mode` | No | `buffered` or `stream` |

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

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Variable name in application |
| `env` | No | Environment key (defaults to `name`) |
| `value` | No | Default value |
| `adapters` | No | Limit to specific adapters |

Variables with a default `value` are injected when running CLI commands.

### Secrets

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Secret name in application |
| `env` | No | Environment key (defaults to `name`) |
| `adapters` | No | Limit to specific adapters |

Secrets must be present in the environment; missing secrets abort CLI commands with an error.

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

| Field | Description |
|-------|-------------|
| `crate` | Path to adapter crate |
| `manifest` | Path to provider manifest (fastly.toml, wrangler.toml) |

### Build Configuration

| Field | Description |
|-------|-------------|
| `target` | Rust compilation target |
| `profile` | Build profile (`release`, `dev`) |
| `features` | Cargo features to enable |

### Commands

| Field | Description |
|-------|-------------|
| `build` | Command for `edgezero build --adapter <name>` |
| `serve` | Command for `edgezero serve --adapter <name>` |
| `deploy` | Command for `edgezero deploy --adapter <name>` |

When commands are omitted, the CLI falls back to built-in adapter helpers.

### Logging

| Field | Adapters | Description |
|-------|----------|-------------|
| `endpoint` | Fastly | Log endpoint name |
| `level` | All | Log level: `trace`, `debug`, `info`, `warn`, `error`, `off` |
| `echo_stdout` | Fastly | Mirror logs to stdout |

## Full Example

```toml
[app]
name = "my-app"
version = "0.1.0"
kind = "http"
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

[adapters.fastly.adapter]
crate = "crates/my-app-adapter-fastly"
manifest = "crates/my-app-adapter-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"

[adapters.fastly.commands]
build = "cargo build --release --target wasm32-wasip1 -p my-app-adapter-fastly"
serve = "fastly compute serve -C crates/my-app-adapter-fastly"
deploy = "fastly compute deploy -C crates/my-app-adapter-fastly"

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
build = "cargo build --release --target wasm32-unknown-unknown -p my-app-adapter-cloudflare"
serve = "wrangler dev --config crates/my-app-adapter-cloudflare/wrangler.toml"
deploy = "wrangler publish --config crates/my-app-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.logging]
level = "info"

[adapters.axum.adapter]
crate = "crates/my-app-adapter-axum"
manifest = "crates/my-app-adapter-axum/axum.toml"

[adapters.axum.commands]
build = "cargo build --release -p my-app-adapter-axum"
serve = "cargo run -p my-app-adapter-axum"
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
- Creates the `App` struct with `build()` method

### ManifestLoader

Load the manifest programmatically:

```rust
use edgezero_core::manifest::ManifestLoader;

let manifest = ManifestLoader::load("edgezero.toml")?;
println!("App name: {}", manifest.app.name);
```

## Validation

`ManifestLoader` validates:
- Non-empty trigger paths and handlers
- Well-formed logging levels
- Required fields present

Errors are surfaced at startup or during macro expansion.

## Next Steps

- Learn about [CLI commands](/guide/cli-reference)
- Explore [adapter-specific configuration](/guide/adapters/overview)
