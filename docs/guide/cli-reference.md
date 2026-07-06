# CLI Reference

The `edgezero` CLI provides commands for scaffolding, development, building, and deployment.

## Installation

Follow the [Getting Started](/guide/getting-started) guide to install the CLI.

## Commands

### edgezero new

Scaffold a new EdgeZero project:

```bash
edgezero new <name> [options]
```

**Arguments:**

- `<name>` - Project name (used for directory and crate names)

**Options:**

- `--dir <path>` - Directory to create the project in (default: current directory)

**Examples:**

```bash
# Create project with all registered adapters
edgezero new my-app

# Create in a specific directory
edgezero new my-app --dir /path/to/projects
```

**Generated structure:**

```
my-app/
├── Cargo.toml
├── edgezero.toml
├── crates/
│   ├── my-app-core/
│   ├── my-app-cli/
│   ├── my-app-adapter-fastly/
│   ├── my-app-adapter-cloudflare/
│   ├── my-app-adapter-axum/
│   └── my-app-adapter-spin/
```

The scaffolder includes all adapters registered at CLI build time, plus a
`my-app-cli` crate — your project's own CLI binary built on the `edgezero-cli`
library.

### edgezero demo

Run the bundled `app-demo` example locally on the axum dev server. This is a
**contributor-only** command — it depends on the in-repo `examples/app-demo`
crate and is compiled only under the `demo-example` feature, so it is not part
of an installed `edgezero` binary:

```bash
cargo run -p edgezero-cli --features demo-example -- demo
# Server starts at http://127.0.0.1:8787
```

`edgezero demo` always runs the built-in example — it does not read your
project's `edgezero.toml` or delegate to its adapters. To run **your project's**
axum adapter, use `edgezero serve --adapter axum` (which runs
`[adapters.axum.commands].serve` from `edgezero.toml`).

> The subcommand is named `demo` — the name `dev` is reserved for a future
> dev-workflow command.

### edgezero build

Build for a specific adapter:

```bash
edgezero build --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`, `spin`, `axum`)

**Examples:**

```bash
# Build for Fastly
edgezero build --adapter fastly

# Build for Cloudflare
edgezero build --adapter cloudflare

# Build for Spin
edgezero build --adapter spin

# Build native binary
edgezero build --adapter axum
```

The command executes the `build` command from `[adapters.<name>.commands]` in `edgezero.toml`, or falls back to the built-in adapter helper.

Any arguments after `--` are forwarded to the adapter command:

```bash
edgezero build --adapter fastly -- --flag value
```

### edgezero serve

Run the provider-specific local server:

```bash
edgezero serve --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`, `spin`, `axum`)

**Examples:**

```bash
# Run Fastly's Viceroy
edgezero serve --adapter fastly

# Run Wrangler dev server
edgezero serve --adapter cloudflare

# Run Spin dev server
edgezero serve --adapter spin

# Run native Axum server
edgezero serve --adapter axum
```

**Provider behavior:**

- **Fastly**: Runs `fastly compute serve`
- **Cloudflare**: Runs `wrangler dev`
- **Spin**: Runs `spin up`
- **Axum**: Runs `cargo run -p <adapter-crate>`

### edgezero deploy

Deploy to production:

```bash
edgezero deploy --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`, `spin`)

**Examples:**

```bash
# Deploy to Fastly
edgezero deploy --adapter fastly

# Deploy to Cloudflare
edgezero deploy --adapter cloudflare

# Deploy to a Spin runtime
edgezero deploy --adapter spin
```

**Provider behavior:**

- **Fastly**: Runs `fastly compute deploy`
- **Cloudflare**: Runs `wrangler deploy`
- **Spin**: Runs `spin deploy`

::: warning
The `axum` adapter doesn't support `deploy` - use standard container/binary deployment instead.
:::

### edgezero config validate

Validate `edgezero.toml` together with the typed `<name>.toml` app
config (see [Application config](/guide/configuration#application-config)).

```bash
edgezero config validate [--manifest <path>] [--app-config <path>] [--strict] [--no-env]
```

**Arguments:**

- `--manifest <path>` — manifest path (default: `edgezero.toml`).
- `--app-config <path>` — typed app-config path (default: `<app_name>.toml` next to the manifest).
- `--strict` — additionally check capability-aware completeness for the declared adapter set (spec §6.6) and well-formed Rust handler paths.
- `--no-env` — skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the app config. By default the validator reads the overlay so it sees the same values the runtime would.

**Two flavours:**

- The default `edgezero` binary runs the **raw** validator — manifest + app-config TOML/schema + the two Spin checks that don't need the typed struct (key syntax, component discovery).
- A downstream CLI built on `edgezero-cli` that owns its app-config struct (e.g. `app-demo-cli`) runs the **typed** validator: everything the raw flow does, plus the typed deserialise, `validator` rules, the `#[secret]` / `#[secret(store_ref)]` checks, and the Spin config / secret namespace collision check.

**Examples:**

```bash
# Raw flow on the default binary — manifest + Spin key syntax.
edgezero config validate

# Strict mode on a downstream CLI — typed deserialise + secrets +
# capability completeness for the declared adapter set.
app-demo-cli config validate --strict
```

**Exit codes:** `0` on success, non-zero with a one-line diagnostic on the first failure (the loader / validator returns early at the first mismatch).

### edgezero config push

Push the resolved `<name>.toml` app-config into the target adapter's
config store (spec §13). Same dispatch shape as the other commands:
each adapter crate owns its own implementation, the CLI is a thin
delegate.

```bash
edgezero config push --adapter <name> [--manifest <path>] [--app-config <path>] [--store <id>] [--no-env] [--local] [--runtime-config <path>] [--dry-run]
```

**Arguments:**

- `--adapter <name>` — target adapter (`axum`, `cloudflare`, `fastly`, `spin`).
- `--manifest <path>` — manifest path (default: `edgezero.toml`).
- `--app-config <path>` — typed app-config path (default: `<app_name>.toml` next to the manifest).
- `--store <id>` — logical config-store id to push to. Defaults to `[stores.config].default` (or the only declared id when `[stores.config].ids` has length 1).
- `--no-env` — skip the `<APP_NAME>__…__<KEY>` env-var overlay when loading the app config. By default the loader reads the overlay so the push sends the same values the runtime would.
- `--local` — push into the adapter's local-emulator state instead of the live platform. Fastly edits `[local_server.config_stores]` in `fastly.toml` (Viceroy reads it on startup); Cloudflare runs `wrangler kv bulk put --local` so writes land in `.wrangler/state`; Spin forces SQLite-direct against `<spin.toml dir>/.spin/sqlite_key_value.db` even when the manifest's deploy command targets Fermyon Cloud (the runtime-config `[key_value_store.<label>].type` is also ignored for SQLite path resolution); Axum is local-only already so it's a no-op there.
- `--runtime-config <path>` — adapter runtime configuration file. Currently only consumed by Spin, which reads `[key_value_store.<label>]` stanzas to dispatch per-backend (`type = "spin"` → SQLite-direct, `redis` / `azure_cosmos` / other → error pointing at the native backend CLI). Default: `runtime-config.toml` next to the adapter manifest. Ignored by the Fermyon Cloud branch — cloud pushes consult only `spin.toml`'s `[application].name`.
- `--dry-run` — print the would-be operations without performing them. No file writes, no shell-outs.

**Two flavours (same split as `config validate`):**

- The default `edgezero` binary **does not push** — `config push` on the bundled binary always errors with a pointer to the typed downstream CLI. The blob app-config model needs the app's typed `AppConfig<C>` (for validation and canonical serialisation), which the bundled binary doesn't embed.
- A downstream CLI built on `edgezero-cli` that owns its app-config struct (e.g. `app-demo-cli`) runs the **typed** push: strict pre-flight validation (`validator::Validate`, secret presence, store-ref membership, adapter checks), then serialises the struct into a single [`BlobEnvelope`](./blob-app-config-migration.md) — one JSON `{ version, generated_at, sha256, data }` value written under one config-store key. `data` carries **every field VERBATIM, including `#[secret]` / `#[secret(store_ref)]` fields**: their value at rest is the operator-supplied key NAME (e.g. `"demo_api_token"`), which the runtime `AppConfig<C>` extractor swaps for the resolved secret at request time. Nothing is flattened or stripped, and the blob never contains resolved secret bytes.

**Per-adapter behaviour:** every adapter writes the **single blob envelope** as one `(key, envelope_json)` config-store entry (the store-resolution and shell mechanics below are unchanged; see [the blob migration guide](./blob-app-config-migration.md#per-adapter-mechanics) for the authoritative per-adapter blob details, including Fastly's oversized-envelope chunking).

| `--adapter`  | Behaviour                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `axum`       | Writes the envelope JSON to `.edgezero/local-config-<id>.json` (the file `AxumConfigStore` reads back). Creates `.edgezero/` on first use. No shell-out.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `cloudflare` | Reads the namespace id from `wrangler.toml` (matched by `binding = <platform-name>`, where `<platform-name>` resolves from `EDGEZERO__STORES__CONFIG__<ID>__NAME` or falls back to the logical `<id>`), writes the single-entry bulk file (`[{"key": "<key>", "value": "<envelope_json>"}]`), and runs `wrangler kv bulk put <tempfile> --namespace-id=<id>` (`--remote` live, `--local` against `.wrangler/state`). Errors with "did you run `provision`?" if the binding is absent.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `fastly`     | Resolves the platform config-store id on demand via `fastly config-store list --json` (matched by `name = <platform-name>`, where `<platform-name>` resolves from `EDGEZERO__STORES__CONFIG__<ID>__NAME` or falls back to the logical `<id>`), then upserts the envelope with `fastly config-store-entry update --store-id=<id> --key=<key> --upsert --stdin`. `--upsert` makes re-runs idempotent. Errors with "did you run `provision`?" if the store name isn't found. Oversized envelopes are auto-chunked (see [the blob migration guide](./blob-app-config-migration.md#fastly)).                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `spin`       | Reads `runtime-config.toml` (default: next to `spin.toml`, override with `--runtime-config <path>`) to dispatch per-backend. **`--local` forces SQLite-direct** writes into `<spin.toml dir>/.spin/sqlite_key_value.db` (Spin's local KV file) regardless of manifest deploy config; non-`default` labels still require a `[key_value_store.<label>]` stanza or the dispatcher refuses to write a file Spin can't read. Otherwise, if `[adapters.spin.commands].deploy` shells to `spin deploy` / `spin cloud deploy`, push writes the single envelope entry via `spin cloud key-value set --app <APP> --label <LABEL> <KEY>=<envelope_json>`. `<APP>` from `[application].name` in spin.toml; `<LABEL>` is the env-resolved platform label that must be pre-linked to a cloud KV store (`spin cloud link key-value`); auth via `spin cloud login`. Otherwise dispatches on `runtime-config.toml`'s `[key_value_store.<label>].type`: `type = "spin"` → SQLite-direct (still requires the stanza for non-`default` labels); `type = "redis"` / `azure_cosmos` / unknown → error pointing at the backend's native CLI. SQLite writer uses Spin's vendored `spin_key_value(store, key, value)` schema (drift-tested at build time). |

**Examples:**

```bash
# The bundled binary cannot push — this errors with a pointer to the
# downstream CLI (the blob model needs the typed AppConfig<C>).
edgezero config push --adapter axum

# Typed push from a downstream CLI — runs strict validation, then writes
# one blob envelope (every field verbatim, incl. #[secret] key names).
app-demo-cli config push --adapter axum --dry-run
```

**Exit codes:** `0` on success, non-zero with a one-line diagnostic on the first failure.

### edgezero provision

Create the platform resources backing the `[stores.<kind>].ids` the
manifest declares — KV namespaces, config stores, secret stores
(spec §12). Same dispatch shape as the other commands: each adapter
crate owns its own implementation, the CLI is a thin delegate.

```bash
edgezero provision --adapter <name> [--manifest <path>] [--dry-run]
```

**Per-adapter behaviour:**

| `--adapter`  | Behaviour                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `axum`       | Local-only — prints one note per declared store id and exits 0 (KV in-memory; config in `.edgezero/local-config-<id>.json`).                                                                                                                                                                                                                                                                                                                                                                                                |
| `cloudflare` | For each KV id + config id: shells out to `wrangler kv namespace create <platform-name>` (where `<platform-name>` resolves from `EDGEZERO__STORES__<KIND>__<ID>__NAME` or falls back to the logical `<id>`), parses the namespace id from stdout, appends `[[kv_namespaces]] binding = "<platform-name>", id = "<extracted>"` to `wrangler.toml` (idempotent on the binding name; preserves existing entries and comments). Secrets are runtime-managed via `wrangler secret put` — no-op.                                  |
| `fastly`     | For each KV / config / secret id: shells out to `fastly <kind>-store create --name=<platform-name>` (using the same `<platform-name>` resolution), then appends `[setup.<kind>_stores.<platform-name>]` and `[local_server.<kind>_stores.<platform-name>]` tables to `fastly.toml`. Idempotent: if the setup table is already present the id is skipped (no shell-out, no edit). Store IDs are not persisted — `config push` resolves them on demand.                                                                       |
| `spin`       | Pure `spin.toml` editing — no shell-out (Spin KV stores are runtime-resolved). For each declared KV id AND each declared `[stores.config]` id (both KV-backed at runtime), appends the platform-resolved label to the resolved `[component.<component>].key_value_stores = [...]` array (idempotent on the label). Secret variables are still manual: `[stores.secrets]` ids get a `nothing to do here` status line and the operator declares `[variables].<name> = { secret = true }` + the per-component binding by hand. |

**`--dry-run`** prints what each adapter _would_ do without
performing it. For `axum` the output is identical to a real run
(there's nothing to actually perform). For `cloudflare`,
`fastly`, and `spin`, dry-run does not invoke any native CLI and
does not edit the adapter manifest.

The `cloudflare` flow requires `wrangler` on `PATH` and
`[adapters.cloudflare.adapter].manifest` pointing at the project's
`wrangler.toml`. Re-running after a successful provision is safe:
existing `binding`s are detected and skipped.

The `fastly` flow requires `fastly` on `PATH` and
`[adapters.fastly.adapter].manifest` pointing at the project's
`fastly.toml`. Re-running is safe: provision skips any id whose
`[setup.<kind>_stores.<id>]` block already exists in the manifest.

The `spin` flow needs no native CLI but does require
`[adapters.spin.adapter].manifest` pointing at the project's
`spin.toml`. If `spin.toml` declares more than one `[component.*]`,
`[adapters.spin.adapter].component = "<id>"` selects which one
receives the KV labels (single-component manifests resolve
implicitly).

### edgezero auth

Sign in, sign out, or check session against the adapter's native
auth surface. `EdgeZero` stores no credentials of its own — `auth`
delegates to the adapter, which decides whether to shell out to the
platform CLI, hit an HTTP API, or no-op (spec §11).

```bash
edgezero auth login  --adapter <name>
edgezero auth logout --adapter <name>
edgezero auth status --adapter <name>
```

Dispatch follows the same path as `build` / `deploy` / `serve`:
the CLI looks up `[adapters.<name>.commands].auth-login` (or
`auth-logout` / `auth-status`) in `edgezero.toml` first; if absent,
it delegates to the adapter crate's built-in implementation.

**Adapter built-ins:**

| `--adapter`  | `login`                 | `logout`                | `status`              |
| ------------ | ----------------------- | ----------------------- | --------------------- |
| `axum`       | no-op (no remote auth)  | no-op                   | no-op                 |
| `cloudflare` | `wrangler login`        | `wrangler logout`       | `wrangler whoami`     |
| `fastly`     | `fastly profile create` | `fastly profile delete` | `fastly profile list` |
| `spin`       | `spin cloud login`      | `spin cloud logout`     | `spin cloud info`     |

**Per-project override** — pin to a script or a different binary in
`edgezero.toml` (same precedence as `build` / `deploy` / `serve`
overrides):

```toml
[adapters.cloudflare.commands]
auth-login  = "./scripts/cf-login.sh"
auth-status = "wrangler whoami --json"
```

The native CLI must be on `PATH`; a missing binary surfaces with an
install hint. A non-zero exit propagates with its stderr verbatim.

::: tip Axum is local-only
`auth --adapter axum` is intentionally a no-op — the native dev
server reads secrets from process env vars (`EDGEZERO__STORES__SECRETS__<ID>__…`),
not from a remote auth provider.
:::

## Environment Variables

The CLI respects these environment variables:

| Variable            | Description                                 |
| ------------------- | ------------------------------------------- |
| `EDGEZERO_MANIFEST` | Path to manifest (default: `edgezero.toml`) |

## Working Directory

All commands expect to run from the project root where `edgezero.toml` is located. If the file is
missing, the CLI falls back to built-in adapters (when compiled in) instead of manifest-driven
commands.

## Adapter Discovery

Adapters register themselves via the `edgezero-adapter` registry at build time. There is currently
no `edgezero --list-adapters` command; the scaffolder includes all adapters that were compiled in.

Built-in adapters (default CLI build):

- `fastly` - Fastly Compute@Edge
- `cloudflare` - Cloudflare Workers
- `spin` - Fermyon Spin
- `axum` - Native Axum/Tokio

## Troubleshooting

### Missing Wasm Target

```
error: target may not be installed
```

Install the required target:

```bash
rustup target add wasm32-wasip1            # For Fastly
rustup target add wasm32-wasip2            # For Spin
rustup target add wasm32-unknown-unknown   # For Cloudflare
```

### Manifest Not Found

If you rely on manifest-driven commands, ensure `edgezero.toml` exists or set `EDGEZERO_MANIFEST`.
When no manifest is present, the CLI falls back to built-in adapter implementations (if compiled
in) instead of using manifest commands.

### Provider CLI Not Found

```
error: fastly: command not found
```

Install the provider CLI:

- Fastly: https://developer.fastly.com/learning/compute/
- Cloudflare: `npm install -g wrangler`
- Spin: https://spinframework.dev/

## Building Your Own CLI

`edgezero-cli` is published as a library as well as a binary. Every downstream
command is exposed as a `(*Args, run_*)` pair (`BuildArgs` / `run_build`,
`DeployArgs` / `run_deploy`, `NewArgs` / `run_new`, `ServeArgs` / `run_serve`),
so a downstream project can build its own CLI binary that reuses any subset of
the built-ins and adds its own subcommands:

```rust
use clap::{Parser, Subcommand};
use edgezero_cli::args::{BuildArgs, DeployArgs};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Build(BuildArgs),       // reuse the built-in
    Deploy(DeployArgs),     // reuse the built-in
    Migrate,                // your own subcommand
}

fn main() {
    edgezero_cli::init_cli_logger();
    let result = match Args::parse().cmd {
        Cmd::Build(args) => edgezero_cli::run_build(&args),
        Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
        Cmd::Migrate => run_migrate(),
    };
    // ...
}
```

`edgezero new <name>` scaffolds exactly this pattern into a `crates/<name>-cli`
crate, and `examples/app-demo/crates/app-demo-cli` is the in-tree reference.

## Next Steps

- Configure your project with [edgezero.toml](/guide/configuration)
- Deploy to [Fastly](/guide/adapters/fastly) or [Cloudflare](/guide/adapters/cloudflare)
