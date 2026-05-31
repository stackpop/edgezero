## Summary

<!-- 1-3 bullet points describing what this PR does and why -->

-

## Changes

<!-- Which crates/files were modified and what changed in each -->

| Crate / File | Change |
| ------------ | ------ |
|              |        |

## Closes

<!-- Link to the issue this PR resolves. Every PR should have a ticket. -->
<!-- Use "Closes #123" syntax to auto-close the issue when merged. -->

Closes #

## Test plan

<!-- How did you verify this works? Check all that apply -->

- [ ] `cargo test --workspace --all-targets`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] WASM builds: `wasm32-wasip1` (Fastly) / `wasm32-wasip2` (Spin) / `wasm32-unknown-unknown` (Cloudflare)
- [ ] `examples/app-demo` workspace: `cd examples/app-demo && cargo test --workspace --all-targets`
- [ ] Docs build: `cd docs && npm run lint && npm run format && npm run build`
- [ ] Manual testing via `edgezero serve --adapter axum` (the pre-rewrite `edgezero-cli dev` was renamed; see [cli-reference](docs/guide/cli-reference.md#edgezero-demo))
- [ ] Other: <!-- describe -->

## Checklist

- [ ] Changes follow [CLAUDE.md](/CLAUDE.md) conventions
- [ ] No Tokio deps added to core or adapter crates
- [ ] Route params use `{id}` syntax (not `:id`)
- [ ] Types imported from `edgezero_core` (not `http` crate)
- [ ] Store wiring goes through `KvRegistry` / `ConfigRegistry` / `SecretRegistry` (not the legacy single-handle setters) — see spec §6.6
- [ ] New code has tests
- [ ] No secrets or credentials committed
