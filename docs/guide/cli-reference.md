# CLI Reference

The `edgezero` CLI provides commands for scaffolding, development, building, and deployment.

## Installation

Install from the workspace:

```bash
cargo install --path crates/edgezero-cli
```

Or from a published crate:

```bash
cargo install edgezero-cli
```

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
- `--local-core` - Use local path dependency for edgezero-core (development only)

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
│   ├── my-app-adapter-fastly/
│   ├── my-app-adapter-cloudflare/
│   └── my-app-adapter-axum/
```

The scaffolder includes all adapters registered at CLI build time.

### edgezero dev

Start the local development server:

```bash
edgezero dev
```

**Example:**

```bash
edgezero dev
# Server starts at http://127.0.0.1:8787
```

If `edgezero.toml` defines an Axum adapter command, `edgezero dev` delegates to it. Otherwise it
starts the built-in dev server (default routes).

### edgezero build

Build for a specific adapter:

```bash
edgezero build --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`, `axum`)

**Examples:**

```bash
# Build for Fastly
edgezero build --adapter fastly

# Build for Cloudflare
edgezero build --adapter cloudflare

# Build native binary
edgezero build --adapter axum
```

The command executes the `build` command from `[adapters.<name>.commands]` in `edgezero.toml`, or falls back to the built-in adapter helper.

### edgezero serve

Run the provider-specific local server:

```bash
edgezero serve --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`, `axum`)

**Examples:**

```bash
# Run Fastly's Viceroy
edgezero serve --adapter fastly

# Run Wrangler dev server
edgezero serve --adapter cloudflare

# Run native Axum server
edgezero serve --adapter axum
```

**Provider behavior:**

- **Fastly**: Runs `fastly compute serve`
- **Cloudflare**: Runs `wrangler dev`
- **Axum**: Runs `cargo run -p <adapter-crate>`

### edgezero deploy

Deploy to production:

```bash
edgezero deploy --adapter <name>
```

**Arguments:**

- `--adapter <name>` - Target adapter (`fastly`, `cloudflare`)

**Examples:**

```bash
# Deploy to Fastly
edgezero deploy --adapter fastly

# Deploy to Cloudflare
edgezero deploy --adapter cloudflare
```

**Provider behavior:**

- **Fastly**: Runs `fastly compute deploy`
- **Cloudflare**: Runs `wrangler publish`

::: warning
The `axum` adapter doesn't support `deploy` - use standard container/binary deployment instead.
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
- `axum` - Native Axum/Tokio

## Troubleshooting

### Missing Wasm Target

```
error: target may not be installed
```

Install the required target:

```bash
rustup target add wasm32-wasip1      # For Fastly
rustup target add wasm32-unknown-unknown  # For Cloudflare
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

## Next Steps

- Configure your project with [edgezero.toml](/guide/configuration)
- Deploy to [Fastly](/guide/adapters/fastly) or [Cloudflare](/guide/adapters/cloudflare)
