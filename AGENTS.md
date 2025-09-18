# AnyEdge Agents Guide

This guide orients coding agents (Codex CLI, etc.) working in this repository. It summarises the architecture, preferred workflows, and DRY rules so changes land in the right crate without duplicating logic.

## Standard Workflow
1. First think through the problem, read the codebase for relevant files, and write a plan to the file: TODO.md. If the TODO.md file does not yet exist, go ahead and create it.
2. The plan should have a list of todo items that you can check off as you complete them
3. Before you begin working, show me the full plan of the work to be done and get my approval before commencing any coding work
4. Then, begin working on the todo items, marking them as complete as you go.
5. As you complete each todo item, give me a high level explanation of what changes you made
6. If you cannot complete a todo, mark it as blocked in TODO.md and explain why.
7. Make every task and code change you do as simple as possible. We want to avoid making any massive or complex changes. Every change should impact as little code as possible. 
8. Finally, add a review section to the TODO.md file with a summary of the changes you made, assumptions made, any unresolved issues or errors you couldn't fix, and any other relevant information. Add the date and time showing when the job was finished.

## Tools

### Context7 MCP
- Use Context7 MCP to supplement local codebase knowledge with up-to-date library documentation and coding examples

### Playwright MCP
- use the Playwright MCP in order to check and validate any user interface changes you make
- you can also use this MCP to check the web browser console for any error messages which may come up
- iterate up to a maximum of 5 times for any one particular feature; an iteration = one cycle of code change + Playwright re-run
- if you can't get the feature to work after 5 iterations then you're likely stuck in a loop and you should stop and let me know that you've hit your max of 5 iterations for that feature
- Save any failing test cases or console errors in a debug.md file so we can review them together.

## Fundamentals

- **Core lives in `crates/anyedge-core`.** Request/Response types, router, middleware, logging facade, and route policies originate here. New framework behaviour almost always belongs in this crate.
- **Adapters stay thin.**
  - `crates/anyedge-fastly` maps Fastly requests/responses, logging, and proxy calls to core types. Feature flag: `fastly`.
  - `crates/anyedge-cloudflare` handles Workers request/response translation. Feature flag: `cloudflare`.
  - `crates/anyedge-std` provides stdout logging for native/dev targets.
- **Tooling**
  - `crates/anyedge-controller` provides typed extractors, responders, and the controller DSL powered by `anyedge-macros` (`#[action]`).
  - `crates/anyedge-cli` ships the dev server (`anyedge-cli -- dev`) and scaffolding WIP commands.
  - `examples/app-demo` hosts the shared controller-based demo (core + Fastly + Cloudflare crates under `crates/`).

## DRY Rules

1. **Single source of truth:** put shared logic (routing, HTTP helpers, constants, proxy behaviour) in `anyedge-core`. Adapters should only translate provider APIs and call into the core.
2. **Reuse helpers:** if multiple handlers/tests need the same behaviour (e.g., path parsing, header shaping), extract a helper in the relevant core module instead of copying code across crates.
3. **Keep templates centralised:** embed demo assets using `include_*` in the core/example crates—never inline repeated markup inside adapters.
4. **Tests follow the logic:** add unit tests next to the code in `anyedge-core` (router/http/middleware). Adapter tests should verify translation glue, not re-test business logic.
5. **Configuration invariants belong with parsing:** when adding options, expose them from a single module and read them from adapters rather than re-validating per target.
6. **Update docs together:** touch `README.md`, this guide, and `TODO.md` whenever semantics change to prevent stale instructions.

## File Map

- Routing & policy – `crates/anyedge-core/src/router.rs`, `src/app.rs`.
- HTTP abstractions – `crates/anyedge-core/src/http.rs`.
- Middleware & logging – `crates/anyedge-core/src/middleware.rs`, `src/logging.rs`.
- Proxy facade – `crates/anyedge-core/src/proxy.rs`.
- Fastly adapter – `crates/anyedge-fastly/src/app.rs`, `src/http.rs`, `src/logging.rs`, `src/proxy.rs`.
- Cloudflare adapter – `crates/anyedge-cloudflare/src/app.rs`, `src/http.rs`.
- Stdout logger – `crates/anyedge-std/src/lib.rs`.
- CLI entry – `crates/anyedge-cli/src/main.rs`, plus supporting modules (`args.rs`, `dev_server.rs`).
- Demos – see `examples/` directories for runnable provider examples.

## Preferred Workflows

1. **Develop in core first:** add functionality/tests in `anyedge-core`, then expose it through adapters if required.
2. **Run checks:**
   - `cargo fmt`
   - `cargo clippy --workspace`
   - `cargo test --workspace`
3. **Targeted testing:**
   - `cargo test -p anyedge-core` for router/http changes.
   - `cargo test -p anyedge-fastly` or `-p anyedge-cloudflare` when modifying adapters.
   - `fastly compute serve` / `wrangler dev` inside demo folders for manual validation.
4. **Logging:** initialise once per process (`anyedge_fastly::init_logger`, `anyedge_std::init_logger`). Avoid duplicate setup in individual handlers.

## Behavioural Policies

- HEAD mirrors GET headers and clears the body across providers.
- OPTIONS replies with `204` and an `Allow` header generated by the router.
- `RouteOptions::streaming()` removes `Content-Length` and coerces buffered responses into streaming iterators.
- `RouteOptions::buffered()` rejects streaming handlers with HTTP 500 to signal misconfiguration.
- Adapter glue must preserve header casing semantics (use `http` crate conversions) and duplicate headers.
- For proxy support, register once with `anyedge_core::Proxy::set`; Fastly adapter exposes `register_proxy` to wire native backends.

## TODO & Roadmap

Review `TODO.md` before larger tasks; it tracks streaming follow-ups, CLI features, and proxy enhancements. When closing items or adding new work, keep that file and this guide in sync.

Sticking to this guide keeps the codebase coherent across providers and prevents duplicated logic from sinking into adapter crates.
