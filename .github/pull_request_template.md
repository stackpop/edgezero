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
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare"`
- [ ] WASM builds: `wasm32-wasip1` (Fastly) / `wasm32-unknown-unknown` (Cloudflare)
- [ ] Manual testing via `edgezero dev`
- [ ] Other: <!-- describe -->

## Checklist

- [ ] Changes follow [CLAUDE.md](../CLAUDE.md) conventions
- [ ] No Tokio deps added to core or adapter crates
- [ ] Route params use `{id}` syntax (not `:id`)
- [ ] Types imported from `edgezero_core` (not `http` crate)
- [ ] New code has tests
- [ ] No secrets or credentials committed
