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

### edgezero provision

Create the platform resources backing the `[stores.<kind>].ids` the
manifest declares — KV namespaces, config stores, secret stores
(spec §12). Same dispatch shape as the other commands: each adapter
crate owns its own implementation, the CLI is a thin delegate.

```bash
edgezero provision --adapter <name> [--manifest <path>] [--dry-run]
```

**Per-adapter behaviour:**

| `--adapter`  | Behaviour                                                                                                                                                                                                              |
| ------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `axum`       | Local-only — prints one note per declared store id and exits 0 (KV in-memory; config in `.edgezero/local-config-<id>.json`).                                                                                            |
| `cloudflare` | For each KV id + config id: shells out to `wrangler kv namespace create <id>`, parses the namespace id from stdout, appends `[[kv_namespaces]] binding = "<id>", id = "<extracted>"` to `wrangler.toml` (idempotent on the binding name; preserves existing entries and comments). Secrets are runtime-managed via `wrangler secret put` — no-op. |
| `fastly`     | _Coming soon._ Will shell out to `fastly <kind>-store create` and ensure `[setup.*]` / `[local_server.*]` entries in `fastly.toml`.                                                                                     |
| `spin`       | _Coming soon._ Pure `spin.toml` editing — appends each KV label to the resolved component's `key_value_stores = [...]` array.                                                                                            |

**`--dry-run`** prints what each adapter _would_ do without
performing it. For `axum` the output is identical to a real run
(there's nothing to actually perform). For `cloudflare`, dry-run
does not invoke `wrangler` and does not edit `wrangler.toml`.

The `cloudflare` flow requires `wrangler` on `PATH` and
`[adapters.cloudflare.adapter].manifest` pointing at the project's
`wrangler.toml`. Re-running after a successful provision is safe:
existing `binding`s are detected and skipped.

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
rustup target add wasm32-wasip1            # For Fastly or Spin
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
