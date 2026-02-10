# EdgeZero

Production-ready toolkit for portable edge HTTP workloads. Write once, deploy to Fastly Compute, Cloudflare Workers, or native Axum servers.

## Quick Start

```bash
# Install the CLI
cargo install --path crates/edgezero-cli

# Create a new project
edgezero new my-app
cd my-app

# Start the dev server
edgezero dev

# Test it
curl http://127.0.0.1:8787/
```

## Documentation

Full documentation is available at **[stackpop.github.io/edgezero](https://stackpop.github.io/edgezero/)**.

- [Getting Started](https://stackpop.github.io/edgezero/guide/getting-started) - Project setup and first steps
- [Architecture](https://stackpop.github.io/edgezero/guide/architecture) - How EdgeZero works
- [Configuration](https://stackpop.github.io/edgezero/guide/configuration) - `edgezero.toml` reference
- [CLI Reference](https://stackpop.github.io/edgezero/guide/cli-reference) - All CLI commands

## Supported Platforms

| Platform           | Target                   | Status |
| ------------------ | ------------------------ | ------ |
| Fastly Compute     | `wasm32-wasip1`          | Stable |
| Cloudflare Workers | `wasm32-unknown-unknown` | Stable |
| Axum (Native)      | Host                     | Stable |

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.
