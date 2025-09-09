# AnyEdge TODO

High-level backlog and decisions to drive the next milestones.

## Queue (near-term)
- [ ] CLI: `anyedge build --provider fastly` to package a Fastly Compute@Edge artifact
- [ ] CLI: `anyedge deploy --provider fastly` using Fastly CLI/API
- [ ] Fastly demo: add optional backend example to validate `req.send("backend")`
- [ ] Core: `Response::json<T: serde::Serialize>` behind `serde` feature
- [x] Core: helper to fetch all header values for a name (multi-value support)
- [ ] Docs: document header/method/status mapping to `http` crate
- [x] Docs: clean stale references to removed hello example from README/targets
- [ ] Docs: add RouteOptions + streaming policy section (examples done; expand README section)
- [ ] CI: add `fmt`, `clippy`, and `test` workflows
- [ ] Cloudflare demo: add minimal `wrangler` example and verify `wrangler dev`
- [ ] Cloudflare streaming: map `Response::with_chunks` to `ReadableStream` with backpressure
- [ ] Core proxy: add async fetch facade (feature `async-client`) and implement Cloudflare proxy
- [ ] Fastly proxy: add tests for backend send + header/body mapping

## Milestones
- [ ] Fastly adapter MVP
  - [x] Map Fastly Request -> `anyedge-core::Request`
  - [x] Map `anyedge-core::Response` -> Fastly Response (headers/body)
  - [x] Handle binary/text bodies and content-length
  - [x] Example: runnable Fastly demo (`examples/anyedge-fastly-demo`) with wasm32-wasip1 target and explicit logger init
  - [x] Streaming: native streaming on Fastly via `stream_to_client` (wasm32-wasip1), buffered fallback elsewhere
- [ ] Cloudflare Workers adapter MVP
  - [x] Basic mapping: Workers Request -> AnyEdge Request
  - [x] Basic mapping: AnyEdge Response -> Workers Response (buffered bodies)
  - [ ] Streaming behavior (ReadableStream) and backpressure
  - [ ] Example: deploy sample with `wrangler`
- [ ] CLI
  - [x] `anyedge new <name>`: scaffold an app (lib with `build_app()`)
  - [ ] `anyedge build --provider fastly|cloudflare`
  - [ ] `anyedge deploy --provider fastly|cloudflare`
  - [ ] `anyedge dev` improvements: better HTTP parsing, hot-reload
- [ ] Config
  - [ ] `anyedge.toml`: app name, routes, provider-specific settings
  - [ ] Secrets/ENV strategy and provider bindings
- [ ] Observability
  - [ ] Logging levels; feature-gated tracing
  - [ ] Metrics hooks; request timing middleware
- [ ] Router
  - [ ] Wildcards (`*`), optional segments, query params helpers
  - [x] HEAD/OPTIONS helpers; 405 handling
  - [ ] 501 handling
- [ ] Responses
  - [ ] JSON helper (serde optional feature)
  - [x] Streaming/chunked support
- [ ] Errors
  - [ ] Unified error type in core; map provider errors to HTTP 5xx
- [ ] Docs
  - [ ] Provider guides (Fastly, Cloudflare)
  - [ ] CLI reference
  - [ ] Example cookbook
- [ ] CI
  - [ ] Run `cargo fmt`, `cargo clippy`, and tests

## Open Design Questions (for later pickup)
- Provider priorities: focus on Fastly Compute@Edge, then Cloudflare Workers.
- Minimum Rust version (MSRV) target.
- Async story: keep core sync or introduce async features (Tokio) behind flags?
- Request/Response mapping rules (header casing, multi-value headers, binary bodies).
- Caching/edge-specific headers: how much to standardize (e.g., Surrogate-Control)?
- Dev UX: integrate a local hyper server behind a feature vs. keeping zero-deps TCP server.
- Packaging/deploy: preferred tooling (Fastly CLI/API; AWS SAM/CDK or native Lambda tooling).
- Config format: TOML/JSON/YAML; env overlays.
- License and contribution guidelines.
