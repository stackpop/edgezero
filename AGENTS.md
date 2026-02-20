# Agent Notes

**Before doing anything else, read `CLAUDE.md` in this repository root.** It
contains all project conventions, coding standards, build commands, workflow
rules, and CI requirements. Everything in `CLAUDE.md` applies to you.

This file exists because Codex looks for `AGENTS.md` by convention. All shared
rules are maintained in `CLAUDE.md` to avoid duplication and drift. If you
cannot access `CLAUDE.md`, the critical rules are summarized below as a
fallback.

## Fallback Summary

If `CLAUDE.md` is unavailable, these are the minimum rules:

1. Write a plan to `TODO.md` before coding. Get approval first.
2. Keep changes minimal — every change should impact as little code as possible.
3. Run `cargo test` after every code change.
4. Run `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
5. Run `cargo check --workspace --all-targets --features "fastly cloudflare"`.
6. Use matchit `{id}` syntax, never legacy `:id`.
7. Use `#[action]` macro for handlers, import types from `edgezero_core`.
8. Don't add Tokio deps to core/adapter crates — WASM compatibility first.
9. Append a review section to `TODO.md` when done (summary, assumptions,
   unresolved issues, UTC timestamp).
