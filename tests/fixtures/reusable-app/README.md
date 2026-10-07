# Reusable application lifecycle fixtures

This standalone Cargo workspace tests provider HTTP behavior without changing
normal generated entry points. Run from the repository root:

```sh
cargo test --locked --manifest-path tests/fixtures/reusable-app/Cargo.toml -p fixture-harness -p fixture-core
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite smoke
./scripts/smoke_test_reusable_app.sh --adapter cloudflare --suite smoke
./scripts/smoke_test_reusable_app.sh --adapter spin --suite smoke
./scripts/smoke_test_reusable_app.sh --adapter axum --suite smoke
```

The driver is the native Rust `fixture-harness` crate. Its HTTP clients, controlled
loopback backend, process management, and evidence checks run outside the WASM
applications. The shell script only launches it through Cargo.

## Understand the change in five minutes

Think of the app as the route table plus any state you attach to it. A request
still brings its own headers, body, extensions, and provider resources.

```text
Existing Fastly entry point:
  request 1 → initialize → build app → dispatch → sandbox ends
  request 2 → initialize → build app → dispatch → sandbox ends

Opt-in retained Fastly app, when the host reuses the sandbox:
  request 1 → initialize → build app → fresh request setup → dispatch
  request 2 →                        fresh request setup → dispatch
  request 3 →                        fresh request setup → dispatch
```

Responses should behave the same. The visible difference is fewer construction
calls, which can reduce latency when initialization is expensive. Reuse is a host
decision: the next request may always need to initialize again.

Start with these entry points in `crates/fixture-fastly/src/bin/`:

| Experiment | Source                   | What survives between callbacks?   |
| ---------- | ------------------------ | ---------------------------------- |
| A          | `single_request.rs`      | Nothing from the previous sandbox  |
| B          | `rebuild_per_request.rs` | Sandbox globals; app is rebuilt    |
| C          | `retained_app.rs`        | Sandbox globals and the app/router |

The `custom_` versions use the same three lifetimes with request preparation,
response finalization, adapter-owned streaming, and post-send work. A/B/C remain
short labels in experiment reports only.

With the pinned Viceroy installed, run a small synthetic comparison:

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark \
  --requests 20 --repetitions 1 --construction-rounds 10000
```

The reported evidence directory contains `events.jsonl` and `summary.json`.
For matched probe responses, inspect `guest.instance`, `guest.ordinal`,
`guest.builds`, and `guest.shared`:

- A: a new instance, ordinal 1, and one build on every request.
- B: the same instance can have ordinal 2 and two builds.
- C: the same instance can have ordinal 2 while builds stays at one.

Compare B and C's reused-request latency to see the construction cost being
removed. The extra JSON parsing deliberately makes that cost visible; it is not
a prediction of an application's production performance. Token, path, and body
checks confirm that each request still sees its own data. Shared state equals the
ordinal in retained probe sequences and one in per-request modes.

For production code, read `serve_app` and `serve_app_with_hooks` in the Fastly
adapter: they retain one successfully configured app and perform fresh request
setup inside each SDK callback. Construction uses
`App::build::<A>(FASTLY_PLATFORM) -> Result<App, EdgeError>`, not an infallible hook builder.
Cloudflare and Spin instead expose `dispatch_app` and let the caller retain a
concrete app. Axum already retains its app. Existing entry points keep their
lifecycle defaults until an application explicitly adopts retention.

Builds use `$CARGO` (falling back to `cargo`) and pin `CARGO_TARGET_DIR` to this
workspace's `target/`, regardless of external Cargo target-dir settings. Update
this workspace's lockfile alongside the root and app-demo lockfiles when bumping
shared dependencies, and keep its Fastly SDK requirement aligned with the root.

Select executables with `VICEROY_BIN`, `WORKER_BUILD_BIN`, `WRANGLER_BIN`, and
`SPIN_BIN`. Install tools explicitly; the runner never deploys or installs them.
The PR pins Fastly SDK 0.13.1, Viceroy 0.21.0, `worker` 0.8.5, and `spin-sdk` 7.0.0
using its `wasip3` re-export; the repository pins Spin CLI 4.1.0. Use a worker-build
version compatible with the locked worker/wasm-bindgen graph rather than assuming
an older builder remains compatible. The runner builds
once with the selected `WORKER_BUILD_BIN`, then starts Wrangler with a snapshot
that disables automatic rebuilding. Axum's executable path is read from Cargo's
artifact output, so configured build targets cannot select a stale default binary.

The runner binds loopback servers, uses synthetic values, seeds only local stores,
and creates an ignored `.runs/<unique-id>` directory. An explicit `--output` must
be empty to prevent evidence loss. Generated provider artifacts are ignored. Do
not expose these diagnostic applications publicly. Their synthetic request tokens
are fixture identities, not a production request-ID generation scheme.

Exit 0 means the requested implemented assertions passed; 1 means a build or
assertion failed; 2 means required runtime evidence is unsupported/unverified.
Raw JSONL, runtime logs, and a summary are retained. The summary includes only
matched probe samples, separated into cold/reused cohorts. SDK attempts, guest
completion, and client completion are distinct. Missing final summaries after a
crash are not zero attempted requests.

## Fastly comparisons

The provider/application boundary is documented in
[`Custom lifecycle contract`](../../../docs/guide/adapters/fastly.md#custom-lifecycle-contract).
This workspace uses path dependencies on the checkout under test. The Fastly
smoke suite additionally runs a fresh custom guest through health, a handled
initialization failure, another health request, successful initialization, and
retained reuse. The sequence must share one guest to establish recovery; early
guest retirement is reported as unverified. Router-produced finalization metadata
carries each request's token, so finalization also checks response-extension
preservation and isolation rather than merely setting a constant header.

- A: original single-request helper.
- B: `Serve`, once-only logging, app construction on each callback.
- C: production retained-app helper.
- Custom A/B/C: EdgeZero `lifecycle::run_custom` / `serve_custom` and `Sandbox<App>`
  own the serving boundary and successful-only initialization. Per-request mode uses a fresh
  state slot per callback; C uses the retained slot. Callbacks return `Result<(), E>`
  and own delivery by explicitly sending native replies or invoking
  `send_request_with_registries_and_hooks(app, stores, req, &runtime.env, prepare, finalize)`.
  The preparation hook mutates the native request and extensions; the routed-response
  finalizer runs before egress policy and framing. The adapter owns appended headers,
  progressive body writes, and terminal close/abandon without whole-body collection.
  Routed finalizer state is available as `Some` only after terminal delivery;
  detached admission responses skip finalization and return `None`. Complete
  post-send work before returning. Neither wrapper requests a synthetic SDK send;
  `run_custom` returns the callback result directly.

Callbacks that dispatch the app read `runtime_env_config` as a `FastlyRuntimeConfig`, use its
`env` for logger and registry setup, and emits deferred boot diagnostics after
the logging setup decision. Native request bodies, extensions, store handles,
and pending work are not retained with the app.

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark --construction-rounds 10000
```

Defaults are three repetitions of 100 probes per variant, with randomized order
and a recorded seed. `--construction-rounds` adds deterministic JSON parsing
inside app construction; it models expensive initialization without embedding
application business logic. This synthetic workload cannot predict adoption gains.
Initialization events record wall time and supported SDK CPU/heap observations.
SDK CPU phase readings are not valid cross-run benchmarks. Heap snapshots include
host resources and are rounded MiB values; flat readings do not rule out small
leaks. Latency quantiles use nearest rank and retain sample counts.

The smoke checks stable guest identity, increasing ordinals, build counts,
request-local values, named/default binding reads, duplicate cookies, ordinary
rendered errors, idle reinitialization, supported request/lifetime/memory limits,
progressive chunks, interrupted streams, finite distinct loopback origins, and
exclusion of the terminated guest after an injected panic. Logging checks run on
B, C, custom C and the negative control. Custom C installs its logger with
`Sandbox::setup_once`, so they also imply setup runs once per guest across
retained callbacks. They derive service-scoped keys from an observed guest service
ID and distinguish `fixture-logs ::` endpoint records from echoed stdout. The
negative control verifies that naively wrapping `run_app` with an enabled global
logger fails on its second installation.

The interrupted-upstream case waits until the client observes its partial body before
releasing the backend abort, proving physical progressive delivery. The immediate
zero-byte source-failure case instead requires exactly one response-scoped terminal
report (`SourceError`, application body, zero accepted bytes, no fallback) and terminal
guest eviction. The host may reset before its head reaches the client; only EOF/reset
with no observed status is accepted, not timeouts, malformed responses, or replacement
error responses. Neither proof upgrades the documented egress capability guarantees.

## Runtime and measurement limits

Raising `--max-requests` does not guarantee a longer guest lifetime. Any observed
Viceroy per-guest ceiling is local evidence, not a deployed guarantee or a promise
that Viceroy 0.21.0 reproduces an older runtime's ceiling. Verify deployed eviction
and long-lived memory behavior separately. The runner checks the Viceroy version against
`.tool-versions` and reports a mismatch as unverified.

Building with `spin-sdk` 7.0.0 emits the SDK's `wasip3` WASI HTTP interface. A
successful build does not prove the selected Spin CLI 4.1.0 host can execute that
component. Retained dispatch also depends on provider-specific runtime support;
an unavailable host, interface mismatch, or missing HTTP/reuse evidence cannot
count as a passing lifecycle assertion. Do not substitute older SDKs or response
converters to make a provider check appear supported. Build/startup failures remain
failures; tools or runtime evidence the runner explicitly marks unsupported or
unverified remain exit 2.

Cloudflare and Spin may select different guests during overlap or recovery tests;
those observations are unverified. A success requires the same instance and the
specific injected failure. Overlap checks use each request's `before.inflight`
observation for the tested pair.

Spin's `/bindings` config check reads a value the same request seeds, so it proves
only that the store label resolves. Cloudflare (`wrangler kv key put --local`) and
Axum (a seeded file) read values written before the request.

Quantiles summarize successful matched probes only. Health replies and injected
errors are excluded. Each cohort includes per-repetition sample counts and
quantiles so changes between repetitions remain visible. Pooled quantiles alone
are not a variance estimate or evidence of production gains. Rounded memory
snapshots over short guest lifetimes cannot establish absence of small leaks.

## Failure and measurement evidence

The smoke suite exercises actual adapter failures with controlled inputs:

These cases describe fixture coverage, not a recorded pass for the current pinned
SDK/runtime graph. Only correlated evidence from a run of this checkout establishes
an assertion; results from older runtimes do not certify the migrated entry points.

- Panic inside `MeasuredApp::configure` during `App::build`, followed by a request
  in another guest.
- A truncated upstream body used as the inbound body, exercising the canonical
  ingress read/error-response path rather than a public conversion helper.
- An erroring core body stream, exercising terminal response-egress failure and
  close/abandon without collecting the whole response first.
- A missing required KV selector terminating the SDK loop, with an attempted
  request summary. A separate custom policy catches that error and proves
  successful bindings and retained app state on the following request.
- Invalid HTTP framing rejected by the host before dispatch, distinguished from
  the observed adapter conversion failure.
- A response with headers and partial body followed by stream interruption and a
  correlated guest error. Connection failure alone cannot satisfy this check.
- Progressive 256-KiB streams, correlated post-send work, repeated and distinct
  loopback origins, and an elapsed lifetime limit during initialization.

`summary.json` groups observations by variant, repetition, and guest. It reports
initialization phases, SDK attempts, recorded client completion, CPU deltas,
and memory change/peak/slope/last-three-sample plateau over actual ordinals.
Missing phase events remain unknown. Historical event labels such as
`conversion_completed` identify only the instrumented resource lifetime; they
are not evidence of whole-body collection, exact response commitment, or complete
cleanup. Custom callbacks additionally record terminal delivery and guest completion
where instrumented. These are different observation points. A standard request-start sample occurs after
initialization, while the custom callback sample occurs before it.

Each run saves its lockfile, configuration, compiler/runtime identity, and binary
fingerprints. Built artifacts are copied into `<run>/artifacts/` right after the
build; runtimes serve and fingerprint those copies. Fingerprints identify artifacts; they are not security checksums.
A larger, still bounded observation run can use:

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark \
  --requests 300 --repetitions 1 --max-requests 100
```

`--max-requests` defaults to 10 and accepts 1–1000. Host eviction can shorten any
observed guest lifetime. Rounded MiB snapshots and a short plateau cannot prove
absence of small leaks or bounded growth for an arbitrary application.

## Evidence requiring a different environment or API

These are explicitly unverified, not assertions counted as passed:

| Case                                                                                   | Available evidence / limitation                                                                                                                                                                                                         |
| -------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Optional runtime-store open fails, then recovers in the same guest                     | Host tests inject degraded then successful configuration snapshots; required-registry recovery is exercised above. Local runtimes cannot make the fixed optional store fail transiently.                                                |
| Spin WASI HTTP interface or provider runtime unavailable                               | SDK compilation alone does not establish host support or reuse. Preserve build/startup failures and explicitly unsupported/unverified records; no older-component compatibility path is used.                                           |
| `EdgeError` itself fails to render                                                     | The renderer builds fixed valid statuses/headers from JSON values; no public input or injection hook triggers a returned rendering error.                                                                                               |
| Unavailable heap hostcall                                                              | Only the preflight is implemented. Under a memory limit, a failed heap snapshot makes the SDK stop after the first request. Local runtimes supply the hostcall and cannot disable it.                                                   |
| Full standard callback/send/cleanup timing                                             | The public retained helper has no post-send callback; its finalizer runs before delivery and its state is discarded. Instrumented resource observations and SDK summary timing are labeled separately; unavailable phases stay unknown. |
| Deployed eviction, endpoint handles, resource accounting, capacity, and workload gains | Require separately authorized deployment and application telemetry. Local CPU samples are not cross-run CPU benchmarks; finite origin tests do not establish service-wide capacity.                                                     |

No deployment is performed by these fixtures.
