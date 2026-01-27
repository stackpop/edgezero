# Roadmap

This page captures upcoming EdgeZero work and longer-term bets. Items here are directional and may
shift as the roadmap evolves.

## Roadmap (2025-09-24)

- Adapter stability: formalise the provider adapter contract (request/response mapping, streaming
  guarantees, proxy hooks) and capture it in shared docs + integration tests so new targets plug in
  safely.
- Provider additions: prototype a third adapter (e.g. AWS Lambda@Edge or Vercel Edge Functions)
  using the stabilized adapter API to validate cross-provider abstractions.
- Manifest ergonomics: evolve `edgezero.toml` to mirror Spin’s manifest convenience (route triggers,
  env/secrets, build targets) while remaining provider-agnostic; update CLI scaffolding accordingly.
- Tooling parity: extend `edgezero-cli` with template/plugin style commands (similar to Spin
  templates) to streamline new app scaffolds and provider-specific wiring.
- CLI parity backlog: add `edgezero --list-adapters`, standardize exit codes, search up for
  `edgezero.toml`, respect `RUST_LOG` for dev output, and bake in hot reload for `edgezero dev`.
- Documentation parity: publish a single-source-of-truth docs set aligned with current APIs
  (App::build_app entrypoints, adapter dispatch signatures, middleware signature, proxy handle
  usage) and keep it in sync with code changes.
- Example coverage: add focused guides for `axum.toml`, manifest `description` fields, logging
  precedence, and route listing + body-mode behavior to reduce ambiguity.
- Adapter contract alignment: document which adapters buffer bodies, which preserve streaming, and
  where proxy headers/automatic decompression apply so expectations match runtime behavior.
- Spin support: add first-class Spin adapter support and document how EdgeZero manifests map to
  Spin-compatible deployments.

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
