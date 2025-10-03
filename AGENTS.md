# Agent Notes

This repository is currently tended by an automated assistant (Codex) that helps
craft the AnyEdge workspace. A few conventions are worth remembering when the
agent is asked to make additional changes:

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

## Testing
always run `cargo test` after touching code. Individual crates
  can be scoped via `cargo test -p <crate-name>` when a partial run is faster.
## Routing syntax
route definitions must use matchit 0.8 style
  parameters (`/resource/{id}` or `/static/{*rest}`); we intentionally do not
  support legacy `:id` syntax.
## Adapters – Fastly and Cloudflare adapters expose request context helpers
  in `context.rs` and translation helpers in `request.rs` / `response.rs`.
  Tests for adapter behaviour live beside those modules.

## Examples
the demo crates under `examples/app-demo/` share router
  logic via `app-demo-core`. Local smoke testing flows through
  `cargo run -p anyedge-cli --features dev-example -- dev`, which serves the demo
  router on http://127.0.0.1:8787. Build provider targets with
  `app-demo-adapter-fastly` / `app-demo-adapter-cloudflare` when you need Fastly
  or Cloudflare binaries.

## Style– prefer colocating tests with implementation modules, favour
async/await-friendly code that compiles to Wasm, and avoid runtime-specific
dependencies like Tokio.
- Use the HTTP aliases exported from `anyedge_core` (`Method`, `StatusCode`,
  `HeaderMap`, etc.) instead of importing types directly from the `http` crate.
- Prefer the `#[anyedge_core::action]` macro for new handlers so extractor
  arguments (`Json<T>`, `Query<T>`, `ValidatedJson<T>`, etc.) stay declarative.
  Extractors live under `anyedge_core::` and integrate with the `validator`
  crate for `Validated*` variants.

## Logging
- Platform logging helpers live in the adapters: use   `anyedge_adapter_fastly::init_logger()` / `anyedge_adapter_cloudflare::init_logger()` and
  fall back to something like `simple_logger` for local builds.

## Proxy helpers
- Use `anyedge_core::proxy::ProxyService` with the adapter clients
  (`anyedge_adapter_fastly::FastlyProxyClient`, `anyedge_adapter_cloudflare::CloudflareProxyClient`)
  when wiring proxy routes so streaming and compression handling stay consistent.
- Keep synthetic local proxy behaviour lightweight so examples can run without
  Fastly/Cloudflare credentials; rely on the proxy test clients in `anyedge-core` for unit coverage.

When in doubt, keep changes minimal, document behaviour in `README.md`, and
ensure the workspace stays Wasm-friendly.
