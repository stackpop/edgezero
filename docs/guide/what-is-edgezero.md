# What is EdgeZero?

EdgeZero is a production-ready toolkit for writing an HTTP workload once and deploying it across multiple edge providers. The core stays runtime-agnostic so it compiles cleanly to WebAssembly targets (Fastly Compute@Edge, Cloudflare Workers) and to native hosts (Axum/Tokio) without code changes.

## Key Features

EdgeZero provides developers with:

- **Portable HTTP workloads** - Write your business logic once using the shared `edgezero-core` primitives, then compile to any supported target
- **Multiple deployment targets** - Deploy to Fastly Compute@Edge, Cloudflare Workers, or native Axum servers from the same codebase
- **Type-safe extractors** - Use ergonomic extractors like `Json<T>`, `Path<T>`, and `ValidatedQuery<T>` for clean handler code
- **Streaming support** - Stream responses progressively with `Body::stream` for long-lived or chunked responses
- **Proxy helpers** - Forward traffic upstream with built-in `ProxyRequest` and `ProxyService` abstractions
- **CLI tooling** - Scaffold projects, run dev servers, and deploy with the `edgezero` CLI

## How It Works

EdgeZero separates your application into layers:

1. **Core logic** - Your handlers and business logic live in a shared crate that depends only on `edgezero-core`
2. **Adapters** - Thin bridge crates translate provider-specific request/response types into the portable model
3. **Entrypoints** - Minimal main functions that wire the adapter to your core app

This architecture means you can:

- Develop locally with the Axum adapter's dev server
- Test your handlers in isolation without provider SDKs
- Deploy the same logic to multiple edge platforms

## Supported Platforms

| Platform            | Target                   | Status |
| ------------------- | ------------------------ | ------ |
| Fastly Compute@Edge | `wasm32-wasip1`          | Stable |
| Cloudflare Workers  | `wasm32-unknown-unknown` | Stable |
| Axum/Tokio (native) | Native host              | Stable |

## Use Cases

- **Edge APIs** - Low-latency JSON APIs running close to users
- **Proxy services** - Request forwarding with header manipulation
- **A/B testing** - Edge-side traffic splitting and experimentation
- **Content transformation** - HTML/CSS rewriting at the edge
- **Multi-cloud deployment** - Avoid vendor lock-in by targeting multiple providers

## Next Steps

Continue to [Getting Started](/guide/getting-started) to set up your first EdgeZero project.
