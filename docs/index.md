---
layout: home

hero:
  name: "EdgeZero"
  text: "Write Once, Deploy Everywhere"
  tagline: "Production-ready toolkit for portable edge HTTP workloads"
  actions:
    - theme: brand
      text: Get Started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/stackpop/edgezero

features:
  - title: Write Once, Deploy Anywhere
    details: Build your HTTP workload once with runtime-agnostic core code that compiles to WebAssembly or native targets without changes.
  - title: Fastly Compute Support
    details: Deploy to Fastly Compute@Edge with zero-cold-start WASM binaries using the wasm32-wasip1 target.
  - title: Cloudflare Workers Support
    details: Run on Cloudflare Workers with seamless wrangler integration and wasm32-unknown-unknown compilation.
  - title: Native Development (Axum)
    details: Develop locally with a full-featured Axum/Tokio dev server, then deploy to containers or native hosts.
  - title: Type-Safe Extractors
    details: Use ergonomic extractors like Json<T>, Path<T>, and ValidatedQuery<T> with the #[action] macro for clean handler code.
  - title: Streaming & Proxying
    details: Stream responses progressively with Body::stream and forward traffic upstream with built-in proxy helpers.
---
