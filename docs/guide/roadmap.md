# Roadmap

This page captures upcoming EdgeZero work and longer-term bets. Items here are directional and may
shift as the roadmap evolves.

## Near-Term Priorities

- Tooling parity: extend `edgezero-cli` with template/plugin style commands (similar to Spin
  templates) to streamline new app scaffolds and provider-specific wiring.
- CLI parity backlog: add `edgezero --list-adapters`, standardize exit codes, search up for
  `edgezero.toml`, respect `RUST_LOG` for dev output, and bake in hot reload for `edgezero dev`.
- Adapter behavior matrix: document which adapters buffer bodies, which preserve streaming, and
  where proxy headers/automatic decompression apply so expectations match runtime behavior.
- Example coverage: add focused guides for `axum.toml`, manifest `description` fields, logging
  precedence, and route listing + body-mode behavior to reduce ambiguity.
- Spin support: add first-class Spin adapter support and document how EdgeZero manifests mirror
  Spin-compatible deployments.
- Provider additions: prototype a third adapter (e.g. AWS Lambda@Edge or Vercel Edge Functions)
  using the stabilized adapter API to validate cross-provider abstractions.

## Completed (Recent)

- Adapter stability: formalised the provider adapter contract and shipped shared docs + integration
  tests so new targets plug in safely.
- Manifest ergonomics: established the `edgezero.toml` schema and CLI scaffolding for route
  triggers, env/secrets, and build targets.
- Documentation baseline: published a single-source-of-truth docs set aligned with current APIs
  (App::build_app entrypoints, adapter dispatch signatures, middleware signature, proxy handle
  usage).
- Platform focus: Fastly Compute@Edge and Cloudflare Workers are the primary edge targets, with Axum
  serving local development and native deployment needs.
- Core contracts: request/response mapping rules are now captured in the adapter contract docs.

## Open Design Questions (for later pickup)

- Minimum Rust version (MSRV) target and upgrade cadence.
- Caching/edge-specific headers: how much to standardize (e.g., Surrogate-Control)?
- Config overlays: strategy for environment-specific overrides in `edgezero.toml`.
- Contribution guidelines and governance model.
