# Reusable application lifecycle fixtures

This standalone Cargo workspace tests provider HTTP behavior without changing
normal generated entry points. Run from the repository root:

```sh
cargo test --locked --manifest-path tests/fixtures/reusable-app/Cargo.toml -p fixture-harness -p fixture-core
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite smoke --require-runtime
./scripts/smoke_test_reusable_app.sh --adapter cloudflare --suite smoke --require-runtime
./scripts/smoke_test_reusable_app.sh --adapter spin --suite smoke --require-runtime
./scripts/smoke_test_reusable_app.sh --adapter axum --suite smoke --require-runtime
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
  --requests 20 --repetitions 1 --construction-rounds 10000 --require-runtime
```

The reported evidence directory contains `events.jsonl` and `summary.json`.
For matched probe responses, inspect `guest.instance`, `guest.ordinal`,
`guest.builds`, and `guest.configures`:

- A: a new instance, ordinal 1, and one build on every request.
- B: the same instance can have ordinal 2 and two builds.
- C: the same instance can have ordinal 2 while builds stays at one.

Compare B and C's reused-request latency to see the construction cost being
removed. The extra JSON parsing deliberately makes that cost visible; it is not
a prediction of an application's production performance. Token, path, and body
checks confirm that each request still sees its own data.

For production code, read `serve_app_with_request_extensions` in the Fastly
adapter: it owns one app and performs request setup inside the SDK callback.
Cloudflare and Spin instead expose `dispatch_app` and let the caller retain a
concrete app. Axum already retains its app. Existing entry points keep their
current behavior until an application explicitly adopts retention.

Select executables with `VICEROY_BIN`, `WORKER_BUILD_BIN`, `WRANGLER_BIN`, and
`SPIN_BIN`. Install tools explicitly; the runner never deploys or installs them.
Worker's build command also uses `worker-build` through PATH; select the same
binary there. The inspected worker-build 0.8.5 passes a flag unsupported by the
locked wasm-bindgen 0.2.122 CLI. Worker-build 0.8.3 successfully builds this fixture.
The checked-in Worker compatibility date is supported by the inspected local host.

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
  own the serving boundary and successful-only initialization. Arm B uses a fresh
  state slot per callback; C uses the retained slot. Raw request mutation/conversion, response-extension finalization,
  appended headers, progressive stream pumping with explicit flush/finish, and
  completed post-send work.

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark --require-runtime
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark --construction-rounds 10000 --require-runtime
```

Defaults are three repetitions of 100 probes per variant, with randomized order
and a recorded seed. `--construction-rounds` adds deterministic JSON parsing
inside app construction; it models expensive initialization without embedding
application business logic. This synthetic workload cannot predict adoption gains.
Initialization events record wall time and supported SDK CPU/heap observations.
SDK CPU phase readings are not valid cross-run benchmarks. Heap snapshots include
host resources and are rounded MiB values; flat readings do not rule out small
leaks. Latency quantiles use nearest rank and retain sample counts.

The smoke checks stable guest identity, increasing ordinals, build/configure counts,
request-local values, named/default binding reads, duplicate cookies, ordinary
rendered errors, idle reinitialization, supported request/lifetime/memory limits,
progressive chunks, interrupted streams, finite distinct loopback origins, and
exclusion of the terminated guest after an injected panic. Logging tests derive service-scoped
keys from an observed guest service ID and distinguish `fixture-logs ::` endpoint
records from echoed stdout. The negative control verifies that naively wrapping
`run_app` with an enabled global logger fails on its second installation.

## Local evidence and remaining limitations

Observed with Viceroy 0.17.0: A/B/C guest reuse and initialization distinctions,
custom streaming, duplicate cookies, binding reads, and later named-endpoint log
receipt pass. Configured limit 10 does not guarantee ten callbacks; observed
guests can terminate earlier. The repeated expensive-construction run completed
300 matched probes per variant. Raw run artifacts are intentionally not committed.

Axum's retained app and overlapping-request isolation pass. Its store files are
isolated under the run directory.

With Wrangler 4.83.0 and worker-build 0.8.3, sequential retention, binding reads,
same-instance overlap, and duplicate cookies pass in both modes. Verification
found an existing header-conversion bug: repeated values overwrote each other.
The converter now replaces each generated header default once, then appends
additional application values. This focused fix applies to default and retained
entry points; it does not enable retention by default.

Spin 4.0.0 demonstrates sequential reuse and overlapping requests in the same
retained instance. A run may still select separate instances; the runner reports
that run as unverified. The selected runtime's help is saved with its evidence;
the inspected version exposes no explicit request-reuse or callback-concurrency
control, so the fixture uses its defaults.

The final all-provider smoke run also passed injected required-binding recovery
in the same retained guest on Cloudflare and Spin. It exited 2 because Spin's
per-request overlap control selected different guests on all three attempts;
retained-mode overlap passed. This is incomplete observation, not an assertion
failure or evidence that the host cannot overlap requests.

### Matched local measurements

Both runs below used three repetitions of 100 probes per variant, seed 856,
release builds, and a configured request limit of 10. Standard and custom paths
are separate comparisons. Values are client completion p50 in milliseconds for
reused guests; each cell contains 249 samples. Each variant also has 51 cold
samples (A has 300 cold samples and no reused samples).

| Construction                                        | Standard B | Standard C | Custom B | Custom C |
| --------------------------------------------------- | ---------: | ---------: | -------: | -------: |
| Cheap (`--construction-rounds 0`)                   |      0.232 |      0.197 |    0.225 |    0.194 |
| Synthetic expensive (`--construction-rounds 10000`) |      7.698 |      0.208 |    7.667 |    0.204 |

Evidence directories: `18d5774746f44488-d6d5-0` and
`18d57754ea36d748-e01c-0`. B rebuilt on each callback; C initialized once per
observed guest. The observed guest span was at most six requests despite the
configured limit of ten. Rounded heap snapshots peaked at 2 MiB and showed zero
change over these spans. The synthetic result demonstrates removal of fixture
construction work; it does not predict another application's gain. Small cheap
workload differences and SDK CPU samples are not production performance claims.

The longer-limit run (`18d57768669e26e8-ebd5-0`) completed 300 probes per variant
with `--max-requests 100` and exited 0. The host still admitted at most six
requests per observed guest. Retained standard and custom samples again peaked
at 2 MiB with zero observed change. Thus the larger configured limit did not
provide a longer per-instance memory observation; long-lived growth remains
unverified in this environment.

The ceiling is explained by Viceroy 0.17.0's `session.rs`: its
`NEXT_REQ_ACCEPT_MAX` constant permits five subsequent requests after the first.
The installed CLI and local-server configuration expose no override. A larger
SDK limit cannot extend this host ceiling. A patched host would be a separate,
instrumented experiment and would not establish deployed reuse behavior.

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
fingerprints. Fingerprints identify artifacts; they are not security checksums.
A larger, still bounded observation run can use:

```sh
./scripts/smoke_test_reusable_app.sh --adapter fastly --suite benchmark \
  --requests 300 --repetitions 1 --max-requests 100 --require-runtime
```

`--max-requests` defaults to 10 and accepts 1–1000. Host eviction can shorten any
observed guest lifetime. Rounded MiB snapshots and a short plateau cannot prove
absence of small leaks or bounded growth for an arbitrary application.

## Evidence requiring a different environment or API

These are explicitly unverified, not assertions counted as passed:

| Case                                                                                   | Available evidence / limitation                                                                                                                                                                                                                             |
| -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Optional runtime-store open fails, then recovers in the same guest                     | Production host tests inject degraded then successful configuration snapshots; actual required-registry recovery is exercised above. The fixed optional store has no transient-open-failure control in the inspected local runtime.                         |
| `EdgeError` itself fails to render                                                     | The current renderer builds fixed valid statuses/headers from JSON values. Its fallible signature remains preserved, but there is no public input or injection hook that triggers this failure. OOM/traps would not demonstrate a returned rendering error. |
| Unavailable heap hostcall                                                              | The installed runtime supplies it. Preflight and fallback are implemented; no documented fault-injection control was found to demonstrate unavailable-hostcall behavior.                                                                                    |
| Full standard callback/send/cleanup timing                                             | The public retained helper has no post-send callback. Conversion observations and SDK summary timing are labeled separately; unavailable phases stay unknown.                                                                                               |
| Deployed eviction, endpoint handles, resource accounting, capacity, and workload gains | Require separately authorized deployment and application telemetry. Local CPU samples are not cross-run CPU benchmarks; finite origin tests do not establish service-wide capacity.                                                                         |

No deployment is performed by these fixtures.

## Persistent-state audit

Core's production recursion depth is thread-local and guarded by
`SecretFieldsRecursionGuard`. Added tests verify normal scope cleanup and host
unwinding followed by fresh entry. Host unwinding does not prove cleanup after a
terminating WASM trap. Canonical-form instrumentation and environment locks are
test-only. Fastly's missing-store warning sets are bounded; their eviction tests
already cover repeated names. Suppression persists with reuse, so warning counts
must not be interpreted as failure counts. No request data belongs in these caches.

Restore the existing entry point and remove the concrete retained owner to roll
back. The default A fixture exercises the original path; limit one alone is not a
replacement for that rollback test.
