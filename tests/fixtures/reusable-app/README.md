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

The `custom_` versions use the same three lifetimes with manual streaming and
response finalization. A/B/C remain short labels in experiment reports only.

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

For production code, read `serve_app_with_request_extensions` in the Fastly
adapter: it owns one app and performs request setup inside the SDK callback.
Cloudflare and Spin instead expose `dispatch_app` and let the caller retain a
concrete app. Axum already retains its app. Existing entry points keep their
lifecycle defaults until an application explicitly adopts retention.

Builds use `$CARGO` (falling back to `cargo`) and pin `CARGO_TARGET_DIR` to this
workspace's `target/`, regardless of external Cargo target-dir settings. Update
this workspace's lockfile alongside the root and app-demo lockfiles when bumping
shared dependencies, and keep its Fastly SDK requirement aligned with the root.

Select executables with `VICEROY_BIN`, `WORKER_BUILD_BIN`, `WRANGLER_BIN`, and
`SPIN_BIN`. Install tools explicitly; the runner never deploys or installs them.
Use a worker-build version compatible with the locked wasm-bindgen CLI
(worker-build 0.8.3 is compatible with the current lockfile). The runner builds
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
[`Custom lifecycle compatibility contract`](../../../docs/guide/adapters/fastly.md#custom-lifecycle-compatibility-contract).
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
  state slot per callback; C uses the retained slot. Raw request mutation/conversion, response-extension finalization,
  appended headers, progressive stream pumping with explicit flush/finish, and
  completed post-send work.

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

## Runtime and measurement limits

Raising `--max-requests` cannot extend the pinned Viceroy's per-guest request
ceiling (see the Fastly guide). Verify deployed eviction and long-lived memory
behavior separately. The runner checks the Viceroy version against
`.tool-versions` and reports a mismatch as unverified.

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

- Panic inside `MeasuredApp::build_app`, followed by a request in another guest.
- A truncated upstream body used as the inbound body, failing `into_core_request`.
- An erroring core body stream, failing buffered response conversion.
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
Missing phase events remain unknown. Standard helpers expose conversion-lifetime
samples; custom callbacks expose response commitment and guest completion. These
are different observation points. A standard request-start sample occurs after
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

| Case                                                                                   | Available evidence / limitation                                                                                                                                                          |
| -------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Optional runtime-store open fails, then recovers in the same guest                     | Host tests inject degraded then successful configuration snapshots; required-registry recovery is exercised above. Local runtimes cannot make the fixed optional store fail transiently. |
| `EdgeError` itself fails to render                                                     | The renderer builds fixed valid statuses/headers from JSON values; no public input or injection hook triggers a returned rendering error.                                                |
| Unavailable heap hostcall                                                              | Only the preflight is implemented. Under a memory limit, a failed heap snapshot makes the SDK stop after the first request. Local runtimes supply the hostcall and cannot disable it.    |
| Full standard callback/send/cleanup timing                                             | The public retained helper has no post-send callback. Conversion observations and SDK summary timing are labeled separately; unavailable phases stay unknown.                            |
| Deployed eviction, endpoint handles, resource accounting, capacity, and workload gains | Require separately authorized deployment and application telemetry. Local CPU samples are not cross-run CPU benchmarks; finite origin tests do not establish service-wide capacity.      |

No deployment is performed by these fixtures.
