# EdgeZero

Production-ready toolkit for portable edge HTTP workloads. Write once, deploy to Fastly Compute, Cloudflare Workers, Fermyon Spin, or native Axum servers.

## Quick Start

```bash
# Install the CLI
cargo install --path crates/edgezero-cli

# Create a new project
edgezero new my-app
cd my-app

# Run it locally on the Axum adapter
edgezero serve --adapter axum

# Test it
curl http://127.0.0.1:8787/
```

## Documentation

Full documentation is available at **[stackpop.github.io/edgezero](https://stackpop.github.io/edgezero/)**.

- [Getting Started](https://stackpop.github.io/edgezero/guide/getting-started) - Project setup and first steps
- [Architecture](https://stackpop.github.io/edgezero/guide/architecture) - How EdgeZero works
- [Configuration](https://stackpop.github.io/edgezero/guide/configuration) - `edgezero.toml` reference
- [CLI Reference](https://stackpop.github.io/edgezero/guide/cli-reference) - All CLI commands
- [Blob App-Config Migration](https://stackpop.github.io/edgezero/guide/blob-app-config-migration) - Typed `AppConfig<C>` extractor + `config push` / `config diff` workflow
- [Deploy with GitHub Actions](https://stackpop.github.io/edgezero/guide/deploy-github-actions) - Full-SHA-pinned Fastly deploy action

## Supported Platforms

| Platform           | Target                   | Status |
| ------------------ | ------------------------ | ------ |
| Fastly Compute     | `wasm32-wasip1`          | Stable |
| Cloudflare Workers | `wasm32-unknown-unknown` | Stable |
| Fermyon Spin       | `wasm32-wasip2`          | Preview |
| Axum (Native)      | Host                     | Stable |

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.
