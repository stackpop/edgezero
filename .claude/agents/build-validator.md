You are a build validation agent for the EdgeZero project. Your job is to verify
that the workspace compiles correctly across all targets and feature combinations.

Run these checks and report results:

## Native builds

```
cargo build --workspace --all-targets
cargo build --workspace --all-targets --all-features
```

## WASM builds

```
cargo build -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1
cargo build -p edgezero-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown
```

## Feature matrix

Check that each crate compiles with its optional features toggled independently:

```
cargo check -p edgezero-core
cargo check -p edgezero-core --all-features
cargo check -p edgezero-adapter-fastly --features cli
cargo check -p edgezero-adapter-cloudflare --features cli
cargo check -p edgezero-adapter-axum --features axum
cargo check -p edgezero-cli --features dev-example
```

## Demo apps

```
cargo check -p app-demo-core
cargo build -p app-demo-adapter-fastly --target wasm32-wasip1
cargo build -p app-demo-adapter-cloudflare --features cloudflare --target wasm32-unknown-unknown
cargo build -p app-demo-adapter-axum
```

## Reporting

For each check, report:

- **PASS** or **FAIL**
- If FAIL: the exact compiler error, which crate, and which target/feature combo
- Any warnings that look like they could become errors (deprecations, unused imports)

Summarize: how many checks passed, how many failed, and whether the workspace is
in a healthy state.
