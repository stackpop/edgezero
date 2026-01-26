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
- `--adapters <list>` - Comma-separated adapters to include (default: `fastly,cloudflare,axum`)

**Examples:**

```bash
# Create project with all adapters
edgezero new my-app

# Create project with specific adapters
edgezero new my-app --adapters fastly,axum

# Create project with only Cloudflare
edgezero new my-app --adapters cloudflare
```

**Generated structure:**

```
my-app/
├── Cargo.toml
├── edgezero.toml
├── crates/
│   ├── my-app-core/
│   ├── my-app-adapter-fastly/    (if --adapters includes fastly)
│   ├── my-app-adapter-cloudflare/ (if --adapters includes cloudflare)
│   └── my-app-adapter-axum/      (if --adapters includes axum)
```

### edgezero dev

Start the local development server:

```bash
edgezero dev [options]
```

**Options:**
- `--port <port>` - Port to listen on (default: 8787)
- `--host <host>` - Host to bind to (default: 127.0.0.1)

**Examples:**

```bash
# Start dev server with defaults
edgezero dev

# Start on custom port
edgezero dev --port 3000

# Bind to all interfaces
edgezero dev --host 0.0.0.0
```

The dev server uses the Axum adapter and reads configuration from `edgezero.toml`.

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

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Log level for dev server |
| `EDGEZERO_MANIFEST` | Path to manifest (default: `edgezero.toml`) |

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Missing configuration |
| 3 | Build failure |
| 4 | Missing adapter |

## Working Directory

All commands expect to run from the project root where `edgezero.toml` is located. The CLI searches up the directory tree for the manifest if not found in the current directory.

## Adapter Discovery

Adapters register themselves via the `edgezero-adapter` registry. The CLI discovers available adapters at runtime:

```bash
# List available adapters
edgezero --list-adapters
```

Built-in adapters:
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

```
error: edgezero.toml not found
```

Ensure you're in the project root or set `EDGEZERO_MANIFEST`:
```bash
EDGEZERO_MANIFEST=/path/to/edgezero.toml edgezero dev
```

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
