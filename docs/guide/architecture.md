# Architecture

EdgeZero is organized as a Cargo workspace with distinct crates for core functionality, platform adapters, and tooling.

## Workspace Layout

```
edgezero/
├── crates/
│   ├── edgezero-core/           # Core routing, extractors, middleware
│   ├── edgezero-macros/         # Procedural macros (#[action], app!)
│   ├── edgezero-adapter/        # Shared adapter traits and registry
│   ├── edgezero-adapter-fastly/ # Fastly Compute@Edge bridge
│   ├── edgezero-adapter-cloudflare/ # Cloudflare Workers bridge
│   ├── edgezero-adapter-axum/   # Native Axum/Tokio bridge
│   └── edgezero-cli/            # CLI for scaffolding and dev server
└── examples/
    └── app-demo/                # Reference application
```

## Core Crate

`edgezero-core` provides the runtime-agnostic foundation:

- **Routing** - `RouterService` with path parameter matching via `matchit`
- **Request/Response** - Portable `http::Request` and `http::Response` types
- **Body** - Unified body type supporting buffered and streaming modes
- **Extractors** - `Json<T>`, `Path<T>`, `Query<T>`, `Form<T>`, `Headers`, and `Validated*` variants
- **Middleware** - Composable middleware chain with async support
- **Manifest** - `edgezero.toml` parsing and validation
- **Compression** - Shared gzip/brotli stream decoders

Handlers in your core crate only depend on `edgezero-core`, keeping them portable.

## Macros Crate

`edgezero-macros` provides compile-time code generation:

- **`#[action]`** - Transforms async functions into handlers with automatic extractor wiring
- **`app!`** - Generates router setup from your `edgezero.toml` manifest

Example usage:

```rust
// In your core crate's lib.rs
mod handlers;

edgezero_core::app!("../../edgezero.toml");
```

## Adapter Crates

Adapters translate between provider-specific types and the portable core model:

### edgezero-adapter-fastly

- Converts Fastly `Request` to `edgezero_core::http::Request`
- Maps core responses back to Fastly `Response`
- Provides `FastlyRequestContext` for accessing Fastly-specific APIs
- Implements `FastlyProxyClient` for upstream requests

### edgezero-adapter-cloudflare

- Converts Workers `Request` to core request
- Maps responses to Workers `Response`
- Provides `CloudflareRequestContext` for Workers APIs
- Implements `CloudflareProxyClient` for fetch operations

### edgezero-adapter-axum

- Wraps `RouterService` in Axum/Tokio services
- Powers the local development server
- Supports native container deployments

## CLI Crate

`edgezero-cli` provides the `edgezero` binary:

- **`edgezero new`** - Scaffolds a new project with templates
- **`edgezero dev`** - Runs the local Axum dev server
- **`edgezero build`** - Builds for a specific adapter target
- **`edgezero serve`** - Runs provider-specific local servers (Viceroy, wrangler dev)
- **`edgezero deploy`** - Deploys to production

## Data Flow

```
┌─────────────────────────────────────────────────────────────┐
│                     Provider Runtime                         │
│  (Fastly Compute / Cloudflare Workers / Axum Server)        │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                        Adapter                               │
│  - into_core_request(): Provider Request → Core Request     │
│  - from_core_response(): Core Response → Provider Response  │
│  - dispatch(): Full request lifecycle                       │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     edgezero-core                           │
│  - RouterService matches routes                             │
│  - Middleware chain executes                                │
│  - Handler runs with extracted params                       │
│  - Response built and returned                              │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Your Core Crate                          │
│  - handlers.rs: Business logic                              │
│  - lib.rs: App definition via app! macro                    │
└─────────────────────────────────────────────────────────────┘
```

## Feature Flags

Adapter crates use feature flags to gate provider SDKs and CLI integration:

| Feature       | Crate                       | Purpose                                |
| ------------- | --------------------------- | -------------------------------------- |
| `fastly`      | edgezero-adapter-fastly     | Fastly SDK integration                 |
| `cloudflare`  | edgezero-adapter-cloudflare | Workers SDK integration                |
| `cli`         | adapter crates              | Register adapters and scaffolding data |
| `dev-example` | edgezero-cli                | Bundled demo app for development       |

## Next Steps

- Learn about the [Adapter Contract](/guide/adapters/overview) for extending EdgeZero
- Explore [Configuration](/guide/configuration) to customize your app
