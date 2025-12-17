# edgezero.toml Manifest

The `edgezero.toml` file describes an EdgeZero application, mirroring the
ergonomics of Spin's manifest while remaining provider agnostic. New workspaces
scaffolded with `edgezero new` now include this manifest by default.

## Top-level structure

```toml
[app]
name = "demo"
version = "0.1.0"
kind = "http"
entry = "crates/demo-core"
middleware = ["edgezero_core::middleware::RequestLogger"]

[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "demo_core::handlers::root"
adapters = ["fastly", "cloudflare"]
body-mode = "buffered"

[environment]

[[environment.variables]]
name = "API_BASE_URL"
env = "API_BASE_URL"
value = "https://example.com/api"

[[environment.secrets]]
name = "API_TOKEN"
adapters = ["fastly", "cloudflare"]
env = "API_TOKEN"

[adapters.fastly.adapter]
crate = "crates/demo-adapter-fastly"
manifest = "crates/demo-adapter-fastly/fastly.toml"

[adapters.fastly.build]
target = "wasm32-wasip1"
profile = "release"

[adapters.fastly.commands]
build = "cargo build --release --target wasm32-wasip1 -p demo-adapter-fastly"
serve = "fastly compute serve -C crates/demo-adapter-fastly"
deploy = "fastly compute deploy -C crates/demo-adapter-fastly"

[adapters.fastly.logging]
endpoint = "stdout"
level = "info"
echo_stdout = true


[adapters.cloudflare.adapter]
crate = "crates/demo-adapter-cloudflare"
manifest = "crates/demo-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.build]
target = "wasm32-unknown-unknown"
profile = "release"

[adapters.cloudflare.commands]
build = "cargo build --release --target wasm32-unknown-unknown -p demo-adapter-cloudflare"
serve = "wrangler dev --config crates/demo-adapter-cloudflare/wrangler.toml"
deploy = "wrangler publish --config crates/demo-adapter-cloudflare/wrangler.toml"

[adapters.cloudflare.logging]
level = "info"
```

### `[app]`

Metadata about the application: the display name, the crate that exposes the router (`entry`),
and an optional `middleware = ["path::to::Middleware"]` list of zero-argument constructors
that are registered before routes are added. Each entry must resolve to a type or function
implementing `edgezero_core::middleware::Middleware`, letting global behaviour (logging, CORS, auth guards,
etc.) live alongside the manifest instead of being hard-wired in Rust.

### `app.middleware`

Manifest-driven equivalent of `RouterService::builder().middleware(...)`. Middleware are applied
in order before the request is handed to route handlers. For example:

```toml
[app]
name = "app-demo"
entry = "crates/app-demo-core"
middleware = [
  "edgezero_core::middleware::RequestLogger",
  "app_demo_core::cors::Cors"
]
```

Each item must be publicly accessible and expose a unit struct or zero-argument constructor that
implements `Middleware`.

### `[[triggers.http]]`

Defines HTTP routes and their handlers. Fields:

- `id`: Stable identifier for the route (optional but useful for tooling).
- `path`: URI template understood by `edgezero-core`.
- `methods`: Allowed HTTP methods (defaults to `GET` if omitted).
- `handler`: Path to the handler function (for reference/documentation).
- `adapters`: Which adapters expose the route. Empty means “all adapters”.
- `body-mode`: Either `buffered` or `stream` to document expected behaviour.

### `[environment]`

Declares environment variables and secrets shared across adapters. Each entry
supports a human-friendly description, the upstream environment key (`env`,
defaulting to the `name`), an optional default `value`, and a provider filter.
When running provider commands through `edgezero-cli`, variables with a default
`value` are injected into the child process and secrets must already be present
in the environment; missing secrets will cause the command to abort with a
helpful error message.

### `[adapters.<name>]`

Describes how a provider adapter is built and invoked.

- `[adapters.<name>.adapter]`: Points at the adapter crate and any provider
  manifest (e.g. `fastly.toml`, `wrangler.toml`).
- `[adapters.<name>.build]`: Build target, profile, and optional feature list.
- `[adapters.<name>.commands]`: Convenience commands for build/serve/deploy.

The EdgeZero CLI will, when present, run these commands for `build`, `serve`,
and `deploy` before falling back to the adapter's built-in behaviour. That lets
you customise provider tooling (e.g. add flags) without recompiling the CLI.

### `[adapters.<provider>.logging]`

Optional logging configuration nested under each adapter. Current fields:

- `endpoint` (Fastly only): Name passed to `init_logger` (defaults to `stdout`).
- `level`: Log level (`trace`, `debug`, `info`, `warn`, `error`, `off`). Defaults to `info`.
- `echo_stdout` (Fastly only): Whether to mirror logs to stdout. Defaults to `true`.

The Fastly adapter in the demo looks these values up before installing its
logger, and the CLI scaffolding emits the same pattern for new projects. Other
adapters can obtain provider-specific settings via
`Manifest::logging_or_default("provider")`, which guarantees a concrete log
level while leaving optional values available for provider-specific defaults at
runtime.

Manifest parsing lives in `edgezero-core::manifest`, and CLI commands now verify
that a provider is declared before invoking adapter-specific tooling. Additional
provider metadata (extra environment bindings, secrets per provider, extra
commands) can be layered under these sections without breaking existing tooling
thanks to permissive deserialisation defaults.

`ManifestLoader` validates basic manifest constraints (non-empty trigger paths
and handlers, well-formed logging levels, etc.) so mistakes are caught early at
startup or during macro expansion.

`edgezero-core::ManifestLoader` provides a shared parser so applications can load
the manifest at runtime. The demo app uses this loader to build its router from
the manifest, and the CLI reuses the same types when executing provider
commands.

## CLI integration

`edgezero build|serve|deploy --adapter <name>` looks up the provider entry in
`edgezero.toml`. If a `[adapters.<name>.commands]` block supplies a `build`,
`serve`, or `deploy` command, the CLI executes it from the manifest directory.
This allows each adapter to decide how artifacts are produced (for example,
invoking `cargo build --target wasm32-wasip1` for Fastly or `wrangler dev` for
Cloudflare). When commands are omitted, the CLI falls back to the built-in
helpers shipped with the adapters (currently the Fastly adapter).

The example app under `examples/app-demo` ships an `edgezero.toml` manifest that
drives both runtime routing and CLI commands. `app-demo-core` reads the manifest
at startup to register HTTP routes (rather than hard-coding paths in Rust), and
running `edgezero build --adapter fastly` from the workspace root invokes the
Fastly build command specified in the manifest.

## Generating routers via macro

Use `edgezero_core::app!("path/to/edgezero.toml", AppName);` inside your
crate to generate a `Hooks` implementation and `build_router` function directly
from the manifest. The `AppName` argument is optional; when omitted the macro
emits a struct named `App`. The macro understands the HTTP trigger list (including
methods and handler paths) and emits the wiring automatically. It also accepts
crate-qualified handler paths such as `app_demo_core::handlers::root`, rewriting
them to the local `crate::…` form the compiler expects. The demo app’s `lib.rs`
shows the minimal usage:

```rust
mod handlers;

edgezero_core::app!("../../edgezero.toml");
```

Handlers referenced in the manifest can therefore use either `crate::` or the
crate name prefix; both get normalised during macro expansion.
