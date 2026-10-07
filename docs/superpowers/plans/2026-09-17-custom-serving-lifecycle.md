# Custom serving lifecycle

> Historical implementation record, reconciled with PR #275's outbound hard-cut.
> Its former SDK response-sending and manual conversion choices are superseded.
> The [outbound HTTP design](../specs/2026-05-21-outbound-http-design.md) remains
> authoritative for admission, owned response egress, and capabilities; no
> compatibility aliases restore removed APIs. Verification below predates this
> reconciliation and is not evidence of a current rerun.

## Objective

Provide reusable lifecycle mechanics for custom Fastly dispatch without taking
over application-specific initialization, response finalization, or streaming.
Keep the default entry points unchanged.

## Implementation

1. Add feature-independent `lifecycle::Sandbox<T>` with successful-only lazy
   initialization, attempted-callback and build counters, and a separate setup guard.
   Preserve unconstrained initialization errors, including fallback routers.
2. Add Fastly-gated `serve_custom` and `run_custom`. Delegate receive limits and
   terminal result accounting to the SDK, not response sending. Callbacks return
   `Result<(), E>` after owned delivery; escaping errors stop serving without an
   SDK-generated response or resend. Single mode does not enter the serving loop.
3. Replace the custom fixture's local state slot and retry bookkeeping with this
   API. Use `App::build::<A>(FASTLY_PLATFORM) -> Result<App, EdgeError>` and retain
   only successful construction. Route preparation/finalization through
   `serve_app_with_hooks` or canonical send-owning request hooks, preserving lazy
   streaming, duplicate headers, fresh registries/admission/egress, and post-send work.
4. Document ownership and migration, including request-scoped resources and
   pre-commit versus post-commit errors.
5. Run host state tests, workspace checks, WASM fixture checks, and the local
   Fastly smoke suite. Independently review the implementation and consumer fit.

## Other adapters

Cloudflare and Spin expose `dispatch_app::<A>` with metadata from `A::stores()`
and freshly resolved request resources. Their callers own retention compatible
with host concurrency and invocation lifetime. Axum constructs its app once for
the running server. Fastly's bounded receive loop is not portable to these hosts;
this extension does not introduce shared mutable singleton state into them.

Build with `App::build::<A>(PLATFORM)`; `Hooks::configure` is fallible after target
metadata is installed. Cloudflare dispatch takes `(&App, Request, Env, Context)`
and returns `Result<Response, worker::Error>`. Spin dispatch takes `(&App, SpinRequest)`
and returns `anyhow::Result<SpinResponse>`, the raw WASI response backed by owned
egress. Neither dispatcher builds an app or installs logging. Never cache an
initialization failure or retain request registries, admission resources, or
delivery coordinators. Pair the retained app with its construction `Hooks` type
and selected platform.

## Acceptance

Health callbacks count but skip initialization. A failed build is returned and
not retained; the next attempt can succeed and subsequent requests reuse it.
Successful setup survives application retries. The custom runtime fixture must
observe recovery and reuse in the same guest, with progressive streaming,
duplicate cookies, finalization metadata and post-commit errors preserved.
Local evidence does not establish deployed eviction or long-lived memory bounds.

## Historical verification

Implemented and independently reviewed against the SDK sending contract and a
custom consumer's ownership requirements. Workspace tests: 1,435 passed, one
existing ignored test. Fixture unit tests: 19 passed. Workspace and fixture
Clippy, Fastly WASM compilation, adapter feature checks, Spin WASM check, Rust
formatting, and documentation format/lint passed.

The Fastly smoke suite passed under Viceroy 0.17.0. Its recovery sequence observed
five callbacks in one instance: statuses 200/503/200/200/200 and initialization
attempts 0/1/1/2/2. Only the fourth callback built the retained app successfully;
the fifth reused it. Separate client records and the initialization runtime log
establish that sequence; it is not included in the aggregate guest phase summary.
Progressive streaming, duplicate headers, response finalization, and post-commit
error fixtures also passed. These are local compatibility observations, not
deployed lifecycle or comparative performance evidence.
