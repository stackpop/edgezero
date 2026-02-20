# CLAUDE.md — EdgeZero

## Project Overview

EdgeZero is a portable HTTP workload toolkit in Rust. Write once, deploy to
Fastly Compute, Cloudflare Workers, or native Axum servers. The codebase is a
Cargo workspace with 7 crates under `crates/`, an example app under
`examples/app-demo/`, a VitePress documentation site under `docs/`, and CI
workflows under `.github/workflows/`.

## Workspace Layout

```
crates/
  edgezero-core/              # Core: routing, extractors, middleware, proxy, body, errors
  edgezero-macros/            # Proc macros: #[action], #[app]
  edgezero-adapter/           # Adapter registry and traits
  edgezero-adapter-fastly/    # Fastly Compute bridge (wasm32-wasip1)
  edgezero-adapter-cloudflare/# Cloudflare Workers bridge (wasm32-unknown-unknown)
  edgezero-adapter-axum/      # Axum/Tokio bridge (native, dev server)
  edgezero-cli/               # CLI: new, build, deploy, dev, serve
examples/app-demo/            # Reference app with all 3 adapters (excluded from workspace)
docs/                         # VitePress documentation site (Node.js)
scripts/                      # Build/deploy/test helper scripts
```

## Toolchain & Versions

- **Rust**: 1.91.1 (from `.tool-versions`)
- **Node.js**: 24.12.0 (for docs site only)
- **Fastly CLI**: v13.0.0
- **Edition**: 2021
- **Resolver**: 2
- **License**: Apache-2.0

## Build & Test Commands

```sh
# Full workspace test (primary CI command)
cargo test --workspace --all-targets

# Test a specific crate
cargo test -p edgezero-core
cargo test -p edgezero-adapter-fastly
cargo test -p edgezero-cli

# Lint & format (must pass CI)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Feature compilation check
cargo check --workspace --all-targets --features "fastly cloudflare"

# Run the demo dev server
cargo run -p edgezero-cli --features dev-example -- dev

# Docs site
cd docs && npm ci && npm run dev
```

**Always run `cargo test` after touching code.** Use `-p <crate-name>` for
faster iteration on a single crate.

## Compilation Targets

| Adapter    | Target                   | Notes                              |
| ---------- | ------------------------ | ---------------------------------- |
| Fastly     | `wasm32-wasip1`          | Requires Viceroy for local testing |
| Cloudflare | `wasm32-unknown-unknown` | Requires `wrangler` for dev/deploy |
| Axum       | Native (host triple)     | Standard Tokio runtime             |

## Coding Conventions

### Routing

Use matchit 0.8+ brace syntax for path parameters:

```rust
// CORRECT
"/resource/{id}"
"/static/{*rest}"

// WRONG — legacy colon syntax is not supported
"/resource/:id"
```

### Handlers

Use the `#[action]` macro for all new handlers:

```rust
use edgezero_core::{action, Json, Path, Query, ValidatedJson, Response, EdgeError};

#[action]
async fn my_handler(
    Json(body): Json<MyPayload>,
    Path(params): Path<MyParams>,
) -> Result<Response, EdgeError> {
    // handler body
}
```

- Import HTTP type aliases from `edgezero_core` (`Method`, `StatusCode`,
  `HeaderMap`, etc.) — never from the `http` crate directly.
- Extractors implement `FromRequest`. Use `ValidatedJson<T>` / `ValidatedPath<T>` /
  `ValidatedQuery<T>` for automatic `validator` crate integration.
- `RequestContext` can be destructured: `RequestContext(ctx): RequestContext`.

### Error Handling

- Use `EdgeError` with semantic constructors: `EdgeError::validation()`,
  `EdgeError::internal()`, etc.
- Map to provider-specific errors only at the adapter boundary.
- Prefer `Result<Response, EdgeError>` as handler return type.

### Middleware

Implement the `Middleware` trait. Chain via `Next::run()`:

```rust
struct MyMiddleware;
impl Middleware for MyMiddleware {
    async fn handle(&self, ctx: RequestContext, next: Next) -> Result<Response, EdgeError> {
        // before
        let response = next.run(ctx).await?;
        // after
        Ok(response)
    }
}
```

### Proxy

Use `ProxyService` with adapter-specific clients (`FastlyProxyClient`,
`CloudflareProxyClient`). Keep proxy logic provider-agnostic in core.

### Logging

- Adapter-specific init: `edgezero_adapter_fastly::init_logger()`,
  `edgezero_adapter_cloudflare::init_logger()`.
- Use `simple_logger` for local/Axum builds.
- Use the `log` / `tracing` facade, not direct dependencies.

### Style Rules

- **WASM compatibility first**: avoid Tokio and runtime-specific deps in core
  and adapter crates. Use `async-trait` without `Send` bounds. Use `web-time`
  instead of `std::time::Instant`.
- **Colocate tests** with implementation modules (`#[cfg(test)]` in the same file).
- **Async tests** use `futures::executor::block_on` (not Tokio) for WASM compat.
- **Minimal changes**: every change should impact as little code as possible.
  Avoid unnecessary refactoring, docstrings on untouched code, or premature abstractions.
- **Feature gates**: platform-specific code goes behind `fastly`, `cloudflare`,
  or `axum` features. Core stays `default-features = false` for WASM targets.
- **No direct `http` crate imports** in application code — use `edgezero_core` re-exports.

## Adapter Pattern

Each adapter follows the same structure:

- `context.rs` — platform-specific request context
- `request.rs` — platform request → core request conversion
- `response.rs` — core response → platform response conversion
- `proxy.rs` — platform-specific proxy client
- `logger.rs` — platform-specific logging init
- `cli.rs` — build/deploy commands (behind `cli` feature)

Contract tests live in `tests/contract.rs` within each adapter crate.

## Manifest (`edgezero.toml`)

The manifest drives routing, env bindings, and per-adapter build/deploy config.
Key sections: `[app]`, `[[triggers.http]]`, `[environment]`, `[adapters]`.

## CI Gates

Every PR must pass:

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-targets`
4. `cargo check --workspace --all-targets --features "fastly cloudflare"`

Docs CI additionally runs ESLint + Prettier on the `docs/` directory.

## Standard Workflow

1. **Read & plan**: think through the problem, read the codebase for relevant
   files, and present a plan as a checklist inline in the conversation.
2. **Get approval first**: show the full plan and get approval before commencing
   any coding work.
3. **Implement incrementally**: work through the checklist items. Make every
   task and code change as simple as possible — every change should impact as
   little code as possible.
4. **Test after every change**: run `cargo test` (or scoped `-p <crate>`) after
   touching any code.
5. **Explain as you go**: after completing each item, give a high-level
   explanation of what changes you made.
6. **If blocked**: explain what's blocking and why.

## Verification & Quality

- **Verify, don't assume**: after implementing a change, prove it works. Run
  tests, check `cargo clippy`, and compare behavior against `main` when relevant.
  Don't say "it works" without evidence.
- **Plan review**: for complex tasks, review your own plan as a staff engineer
  would before implementing. Ask: is this the simplest approach? Does it touch
  too many files? Are there edge cases?
- **Escape hatch**: if an implementation is going sideways after multiple
  iterations, step back and reconsider. Scrap the approach and implement the
  simpler solution rather than patching a flawed design.
- **Use subagents**: for tasks spanning multiple crates or requiring broad
  codebase exploration, use subagents to parallelize investigation and keep the
  main context clean.

## Slash Commands

Custom commands live in `.claude/commands/`:

| Command           | Purpose                                            |
| ----------------- | -------------------------------------------------- |
| `/check-ci`       | Run all 4 CI gate checks locally                   |
| `/test-all`       | Run full workspace test suite                      |
| `/test-crate`     | Run tests for a specific crate                     |
| `/review-changes` | Staff-engineer-level review of uncommitted changes |
| `/verify`         | Prove current changes work vs main                 |

## Available MCPs

- **Context7 MCP**: use for up-to-date library docs and coding examples.

## Key Files Reference

| Purpose            | Path                                    |
| ------------------ | --------------------------------------- |
| Workspace manifest | `Cargo.toml`                            |
| Core crate entry   | `crates/edgezero-core/src/lib.rs`       |
| Router             | `crates/edgezero-core/src/router.rs`    |
| Extractors         | `crates/edgezero-core/src/extractor.rs` |
| Action macro       | `crates/edgezero-macros/src/action.rs`  |
| CLI entry          | `crates/edgezero-cli/src/main.rs`       |
| Demo app           | `examples/app-demo/`                    |
| Demo manifest      | `examples/app-demo/edgezero.toml`       |
| CI tests           | `.github/workflows/test.yml`            |
| CI format/lint     | `.github/workflows/format.yml`          |
| Docs site          | `docs/`                                 |
| Test script        | `scripts/run_tests.sh`                  |
| Roadmap            | `ROADMAP.md`                            |

## Dependencies Philosophy

- Workspace-level dependency management via `[workspace.dependencies]` in root `Cargo.toml`.
- Minimal, carefully curated for WASM compatibility.
- `Cargo.lock` is committed for reproducible builds.
- Key crates: `matchit` (routing), `tower` (middleware), `async-trait`,
  `async-compression`, `serde`/`serde_json`, `validator`, `clap` (CLI),
  `handlebars` (templates).

## What NOT to Do

- Don't use legacy `:id` route syntax — always use `{id}`.
- Don't import from `http` crate directly — use `edgezero_core` re-exports.
- Don't add Tokio dependencies to core or adapter crates.
- Don't write tests that require a network connection or platform credentials.
- Don't make large, sweeping refactors — keep changes minimal and focused.
- Don't commit without running `cargo test` first.
- Don't skip `cargo fmt` and `cargo clippy` — CI will reject the PR.
