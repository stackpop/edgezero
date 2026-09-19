# Custom serving lifecycle

## Objective

Provide reusable lifecycle mechanics for custom Fastly dispatch without taking
over application-specific initialization, response finalization, or streaming.
Keep the default entry points unchanged.

## Implementation

1. Add feature-independent `lifecycle::Sandbox<T>` with successful-only lazy
   initialization, attempted-callback and build counters, and a separate setup guard.
   Preserve unconstrained initialization errors, including fallback routers.
2. Add Fastly-gated `serve_custom` and `run_custom`. Delegate limits and result
   sending to the SDK exactly once. Single mode does not enter the serving loop.
3. Replace the custom fixture's local state slot and retry bookkeeping with this
   API. Keep explicit streaming, duplicate headers, finalization, and post-send work.
4. Document ownership and migration, including request-scoped resources and
   pre-commit versus post-commit errors.
5. Run host state tests, workspace checks, WASM fixture checks, and the local
   Fastly smoke suite. Independently review the implementation and consumer fit.

## Other adapters

Cloudflare and Spin already expose `dispatch_app` with explicit store metadata
and freshly resolved request resources. Their callers own retention compatible
with host concurrency and invocation lifetime. Axum constructs its app once for
the running server. Fastly's bounded receive loop is not portable to these hosts;
this extension does not introduce shared mutable singleton state into them.

## Acceptance

Health callbacks count but skip initialization. A failed build is returned and
not retained; the next attempt can succeed and subsequent requests reuse it.
Successful setup survives application retries. The custom runtime fixture must
observe recovery and reuse in the same guest, with progressive streaming,
duplicate cookies, finalization metadata and post-commit errors preserved.
Local evidence does not establish deployed eviction or long-lived memory bounds.

## Verification

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
