# Axum (Native)

Run EdgeZero applications natively using Axum and Tokio for local development, testing, and container deployments.

## Overview

The Axum adapter provides:

- **Local development server** - Fast iteration without Wasm compilation
- **Native testing** - Run tests with standard `cargo test`
- **Container deployments** - Deploy to any platform supporting native binaries

## Project Setup

When scaffolding with `edgezero new my-app`, the Axum adapter includes:

```
crates/my-app-adapter-axum/
├── Cargo.toml
├── axum.toml
└── src/
    └── main.rs
```

### Entrypoint

The Axum entrypoint wires the adapter:

```rust
use edgezero_adapter_axum::dev_server::run_app;
use my_app_core::App;

fn main() -> anyhow::Result<()> {
    run_app::<App>()
}
```

`run_app` installs `simple_logger` (unless `owns_logging = true`), builds the app, and reads bind /
store / logging config at runtime from `EDGEZERO__*` environment
variables (see [the migration guide](../manifest-store-migration.md)).
The portable store metadata baked into `App` by the `app!` macro
drives which logical stores are exposed; no `edgezero.toml` needs to
be loaded by the runtime.

## Development Server

Run your project locally on the Axum adapter:

```bash
edgezero serve --adapter axum
```

This starts a server at `http://127.0.0.1:8787` with standard logging to stdout.

### Manual Start

Run the Axum entrypoint directly:

```bash
# Using the CLI
edgezero serve --adapter axum

# Or directly with cargo
cargo run -p my-app-adapter-axum
```

## Building

Build a native release binary:

```bash
# Using the CLI
edgezero build --adapter axum

# Or directly
cargo build -p my-app-adapter-axum --release
```

The binary is placed in `target/release/my-app-adapter-axum`.

## Proxy Client

The Axum adapter provides a native HTTP client for proxying:

```rust
use edgezero_adapter_axum::proxy::AxumProxyClient;
use edgezero_core::proxy::ProxyService;

let client = AxumProxyClient::try_new()?;
let response = ProxyService::new(client).forward(request).await?;
```

This uses `reqwest` under the hood for outbound HTTP requests. `try_new` is
fallible because it builds a `reqwest::Client`; it returns a `reqwest::Error` if
the TLS backend cannot be initialised on the host.

## Logging

The Axum adapter's `run_app` helper installs `simple_logger` at the level read from
`EDGEZERO__LOGGING__LEVEL`, falling back to `info` when the variable is unset or
unparseable. It does not read `edgezero.toml`, and `echo_stdout` has no effect on
the runtime. To install a different logger, set `owns_logging = true` on your `app!`
declaration so `run_app` skips its own logger, then install yours in `main`. Wiring
`App::build_app()` and `AxumDevServer` by hand remains the fallback if you also need
to control the bind address or store setup.

::: tip Logging status
`run_app` wires logging automatically; custom entrypoints should install a logger explicitly.
:::

## Testing

The Axum adapter enables standard Rust testing:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use edgezero_core::app::Hooks;
    use edgezero_core::http::{Request, Method};

    #[tokio::test]
    async fn test_handler() {
        let app = App::build_app();
        let router = app.router();

        let request = Request::builder()
            .method(Method::GET)
            .uri("/hello")
            .body(Body::empty())
            .unwrap();

        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
    }
}
```

Run tests:

```bash
cargo test -p my-app-core
cargo test -p my-app-adapter-axum
```

## KV Storage

Each declared `[stores.kv]` id resolves to a `redb`-backed store on disk under
`.edgezero/`, so values persist across dev-server restarts. The file name is
derived from the platform store name, which comes from
`EDGEZERO__STORES__KV__<ID>__NAME` or defaults to the logical id:

```
.edgezero/kv-<slug>-<hash>.redb
```

The database file grows over time and does not shrink after deletions. To reclaim
space, delete the file in `.edgezero/`; the data is lost. See [KV Storage](/guide/kv)
for the portable API.

## Secret Store

A declared `[stores.secrets]` id resolves to an `EnvSecretStore`, which looks up each
secret name verbatim in the process environment. Axum lists `secrets` in its
`single_store_kinds`, so only one secrets id may be declared:

```bash
API_KEY=mysecret edgezero serve --adapter axum
```

## Config Store

For local development, each declared `[stores.config]` id resolves to a
local-file config store backed by `.edgezero/local-config-<id>.json`.
The portable manifest carries no inline defaults — the
pre-rewrite `[stores.config.defaults]` table is gone (see
[the migration guide](../manifest-store-migration.md)).

```toml
[stores.config]
ids     = ["app_config"]
# default = "app_config"   # required when ids.len() > 1
```

```jsonc
// .edgezero/local-config-app_config.json — what `<your-cli> config push` writes.
// The outer object maps the selected config key (defaults to the logical store
// id; override with `--key`, e.g. for a `staging` blob) to ONE BlobEnvelope,
// JSON-encoded as a string (see the blob migration guide for the envelope's
// fields). `data` holds the typed config verbatim; `#[secret]` fields store
// their key NAMES, which the runtime resolves at request time — never the
// secret values.
{
  "app_config": "{\"version\":1,\"generated_at\":\"…\",\"sha256\":\"…\",\"data\":{\"greeting\":\"hello\",\"feature\":{\"new_checkout\":false},\"service\":{\"timeout_ms\":1500},\"api_token\":\"demo_api_token\",\"vault\":\"default\"}}",
}
```

Typed apps read the whole config in one shot with the `AppConfig<C>` extractor,
which parses the envelope and resolves `#[secret]` fields before handing you `cfg`:

```rust
async fn handler(AppConfig(cfg): AppConfig<MyConfig>) -> Result<Response, EdgeError> {
    let greeting = &cfg.greeting;
    // …
}
```

The lower-level `Config` extractor / `ctx.config_store(id)` exposes the raw
key/value store — `store.get("app_config")` returns the envelope string, and a
hand-seeded flat file returns individual values. Do not pass raw user input
straight to `store.get(…)` in production handlers; validate or allowlist keys
first.

Seed the per-id files with `<your-cli> config push --adapter axum` (the typed
flow — the bundled `edgezero config push` errors), which writes the same
`.edgezero/local-config-<id>.json` files the runtime reads — no shell-out, no
server to authenticate against.

## Container packaging

`edgezero new` generates a workspace-root `Dockerfile` and `.dockerignore` when
Axum is registered in the CLI build. The recipe builds only the native Axum
application binary with `--release --locked`, using a GNU/Linux Rust builder and
compatible Debian-slim runtime. The base images have reviewed digest pins;
package installation still needs network access and is not a byte-reproducible
build claim.

### Prepare the application

Generated EdgeZero dependencies may point into the developer's local checkout.
Those paths are not portable Docker inputs. Before building, edit the root
`[workspace.dependencies]` entries to use the same approved release tag or
commit for every EdgeZero crate. For example:

```toml
# Replace the placeholder with a reviewed release compatible with this app.
edgezero-core = { git = "https://github.com/stackpop/edgezero.git", tag = "<approved-release>", default-features = false }
edgezero-adapter-axum = { git = "https://github.com/stackpop/edgezero.git", tag = "<approved-release>", default-features = false }
edgezero-cli = { git = "https://github.com/stackpop/edgezero.git", tag = "<approved-release>" }
```

Apply that ref to the other generated EdgeZero adapter entries too. Preserve
member feature settings, including Axum's `axum` feature and the operator CLI's
default features. Keep relative paths between the application's own crates.
There is no new dependency-selection flag; this is ordinary manifest preparation.

```sh
cargo generate-lockfile
cargo metadata --locked --format-version 1
# Review dependency sources, then commit the app manifests and Cargo.lock.
```

Do not copy EdgeZero's workspace lockfile. Scaffolding stays offline; these
explicit preparation commands may fetch dependencies. Docker requires a present,
current application lockfile and never repairs dependencies during the build.

The builder compiler must match the exact Rust pin in the generated
`.tool-versions`. An optional `rust-toolchain.toml` must have a matching quoted
`channel = "X.Y.Z"` under `[toolchain]`. The small container check supports that
conventional format, not arbitrary TOML/channel syntax; legacy `rust-toolchain`
files are rejected. Update the declarations, Docker Rust version and reviewed
builder digest together when upgrading. The image uses the installed compiler
without downloading toolchain-file targets/components. It selects the native
host target explicitly rather than inheriting a consumer's WASM build target.

### Build and run

Run from the application workspace root, which must retain all workspace member
manifests, `edgezero.toml` and required build-time sources/assets:

```sh
docker build -t my-app:local .
docker run --rm --read-only --cap-drop=ALL \
  --security-opt=no-new-privileges:true \
  -p 127.0.0.1:8787:8787 my-app:local
# From another terminal:
curl --fail http://127.0.0.1:8787/
```

The runtime contains the binary, system CA trust and required native libraries,
not the source workspace, Rust, Node or the operator CLI. It runs as UID/GID
10001:10001 with exec startup and binds `0.0.0.0:8787` inside the container. The
publication above is localhost-only. EXPOSE does not publish ports, and the
read-only/capability settings are Docker run options, not Dockerfile guarantees.
To change the internal port, set `EDGEZERO__ADAPTER__PORT` and adjust the published
port mapping. Local development still defaults to localhost.

The default generated app declares no stores and needs no writable mount. Apps
using current embedded stores must provide `/app/.edgezero`, owned/readable as
needed by UID 10001. Provision permissions outside startup; do not run a root
chown wrapper or use world-writable state. Config-only mounts can be read-only.
Embedded redb needs one writer; a shared volume does not establish multi-replica
safety. Runtime configuration, secrets, signing keys and mutable state remain
external inputs. The image does not generate them.

Review `.dockerignore` before building. It excludes common credentials, local
state, Git and host build output, but cannot identify custom secret filenames.
Keep required manifests and assets. Private build credentials need an explicit
app-owned BuildKit secret/SSH setup, never COPY, ARG or ENV.

### Optional application asset builds

Node is not required for the generic template. Applications that embed JS during
Rust compilation can add a separate pinned asset stage before the Rust builder.
Trusted Server is one such consumer. Its app-owned extension can start with:

```dockerfile
FROM node:24.12.0-bookworm-slim AS assets
WORKDIR /assets/lib
COPY crates/trusted-server-js/lib/ ./
RUN npm ci && npm run build

# In the existing Rust builder, before Cargo compilation:
# COPY --from=assets /assets/dist /build/crates/trusted-server-js/dist
# ENV TSJS_SKIP_BUILD=1
```

Choose Node from the consumer's declared pin, retain its npm lockfile, and build
fresh bundles rather than reuse host dist output. Trusted Server's build.rs
embeds those bundles; after the explicit asset build, `TSJS_SKIP_BUILD=1` avoids
an implicit npm retry. Select its native package `trusted-server-adapter-axum`
and binary `trusted-server-axum`, not its Fastly default member or operator CLI.
Check bundle completeness and embedded bytes. This customization is guidance,
not a claim that Trusted Server's bind/config/health behavior or image has been
validated. Node stays out of the final runtime.

### Packaging verification and limits

From an EdgeZero checkout, the opt-in smoke accepts a **disposable generated
workspace** after the preparation above:

```sh
./scripts/test_generated_axum_container.sh /path/to/container-probe
```

It requires Docker, Python 3.11+, curl, OpenSSL, readelf and preparation network access.
It tests actual context exclusions, final filesystem/native libraries, UID,
read-only root, dropped capabilities, explicit mount permissions, published-port
HTTP and the real proxy's controlled TLS trust and hostname rejection. It uses
private test CA material, not production secrets or public upstreams, and cleans
up its containers/network/image. Failure diagnostics are retained in a temporary
directory, with private test keys removed.

For unmerged framework changes, a separate disposable fixture can explicitly
stage EdgeZero sources and point test-only dependencies within the context.
Include all needed workspace manifests and the CLI's optional app-demo source
path. Resolve that fixture's own lockfile. This local-source evidence is not a
substitute for a standalone consumer with portable Git references.

These are packaging checks, not production certification. The current runtime
still needs accepted PR #275 integration and the lifecycle/config work tracked
by #392/#396 for readiness, required-config failure, durable restart and bounded
SIGTERM draining. A response from `/` is not application readiness, and exec
startup alone does not implement signal handling. #399 owns native amd64/arm64
CI and publishing guidance; #400 owns Docker/Kubernetes/cloud operations.
Application owners publish their own images. No public EdgeZero image is needed.

## Configuration

Configure the Axum adapter in `edgezero.toml`. See [Configuration](/guide/configuration) for the full
manifest reference.

The `axum.toml` file is used by the Axum CLI helper to locate the crate and carry a
default port. `edgezero serve --adapter axum` resolves the bind address with this
precedence, highest first: the `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`
environment variables, then `[adapters.axum.adapter]` in `edgezero.toml`, then
`axum.toml`, then `127.0.0.1:8787`. The CLI passes the resolved address to the child
process as `EDGEZERO__ADAPTER__HOST` / `EDGEZERO__ADAPTER__PORT`. Running the binary
directly bypasses that resolution: it reads only those two environment variables and
otherwise falls back to `127.0.0.1:8787`.

## Development Workflow

A typical development workflow:

1. **Run locally**: `edgezero serve --adapter axum`
2. **Make changes** to handlers in `my-app-core`
3. **Test locally** with curl or browser
4. **Run tests**: `cargo test`
5. **Build for edge**: `edgezero build --adapter fastly`
6. **Deploy**: `edgezero deploy --adapter fastly`

## Differences from Edge Adapters

| Aspect      | Axum           | Fastly/Cloudflare |
| ----------- | -------------- | ----------------- |
| Compilation | Native         | Wasm              |
| Cold start  | ~0ms           | ~0ms (Wasm)       |
| Memory      | Unlimited      | 128MB typical     |
| Filesystem  | Full access    | Sandboxed         |
| Network     | Direct         | Backend/fetch     |
| Concurrency | Multi-threaded | Single-threaded   |

::: tip Development Parity
While Axum provides a convenient development environment, always test on actual edge platforms before deploying. Provider-specific behaviour such as store backends and request context differs on the real targets.
:::

## Next Steps

- Deploy to [Fastly Compute](/guide/adapters/fastly) for production
- Deploy to [Cloudflare Workers](/guide/adapters/cloudflare) as an alternative
- Explore [Configuration](/guide/configuration) for manifest options
