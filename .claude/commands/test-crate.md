Run tests for a specific crate. Usage: /test-crate <crate-name>

Run `cargo test -p $ARGUMENTS` and report results. If no crate name is provided, ask which crate to test from the workspace members:

- edgezero-core
- edgezero-macros
- edgezero-adapter
- edgezero-adapter-fastly
- edgezero-adapter-cloudflare
- edgezero-adapter-axum
- edgezero-cli
