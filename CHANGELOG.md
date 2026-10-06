# Changelog

## Unreleased

### Portable HTTP And Request Lifecycles (PR #275)

The changes below cover [PR #275](https://github.com/stackpop/edgezero/pull/275), including its
implementation and review follow-ups. This is a hard API migration: retired APIs and enum variants
are removed rather than retained as compatibility aliases. Provider limitations are published
separately from the portable API and must be considered when declaring required capabilities.

### Outbound HTTP And Batches

- Replace the proxy-client/service API with `HttpClient`, `OutboundHttpClient`, `OutboundRequest`,
  and `OutboundResponse`. Standard adapters inject the client into request extensions; handlers
  access it through `RequestContext::http_client()`. Missing manual wiring remains explicit.
- Add buffered and streamed uploads/responses, absolute deadlines, request-body bounds, explicit
  cache policy, and HTTP authority override independent of URI routing and TLS identity where
  supported. Automatic redirects and implicit provider response decoding are disabled.
- Request construction is fallible and validates absolute HTTP(S) targets. Dispatch accepts GET,
  HEAD, POST, PUT, PATCH, DELETE, and OPTIONS; GET/HEAD reject nonempty bodies. Ordinary `Host`
  headers are normalized away; use the explicit authority override instead.
- Default to a 1 MiB buffered response, an 8 MiB upload cap, a 32 MiB decoder budget, a 24-bit Brotli
  window limit, and a 30-second exchange budget when no timeout/deadline is supplied. Response-mode
  setters are last-write-wins; `stream_response()` opts into streamed delivery. Encoded, decoded,
  header, and chunk limits are separate opt-in controls.
- Accept streamed upload sources under the request cap and deadline, but collect them before
  provider dispatch on Axum and Cloudflare. Cloudflare no longer forwards upload chunks directly
  to Fetch as the old proxy implementation did; this API does not promise wire-streamed uploads.
- Expose `OutboundCachePolicy::{PlatformDefault, Bypass}`. Bypass disables provider caching on
  Cloudflare/Fastly and is a no-op on Axum/Spin, which do not install an outbound response cache.
- Add completion-driven fanout through `start_batch_until`, indexed `next`, explicit `cancel`,
  ordered `collect`, and `send_all_until`. Batch slots require buffered requests and responses;
  streaming exchanges use `send`.
- Return per-slot elapsed time and outcomes, preserving input indices while yielding observed
  completion order. Timing uses one batch-entry snapshot and includes preflight, provider setup,
  complete body collection, and guest-observation delays rather than claiming pure transport RTT.
- Define `observation_cutoff` as the point where batch observation stops, even if individual slots
  retain request budget. Earlier cutoffs are valid partial-result operations, not request timeouts.
- Distinguish `Completed`, `Cutoff`, and typed batch failure. Only cutoff permits unresolved slots
  in successful collection; `OutboundBatchFailure` retains both the precise error and previously
  collected, index-aligned slots. Cancellation releases the driver and returns unresolved indices.
- Require clean driver EOF before `Completed`, including zero-slot drivers. Trailing failures,
  duplicate/out-of-range indices, premature EOF, and cutoff after all slots resolve are rejected;
  poisoned batches cannot resume driver polling. External adapters have a documented, public
  `OutboundBatchDriverEvent` / `OutboundBatch::from_driver` construction protocol.
- Axum, Cloudflare, and Spin drive complete eligible exchanges concurrently. Fastly dispatches
  eligible inputs before harvesting ready pending handles, preserves handle-to-slot identity, and
  uses deterministic dynamic-backend identities that include exact timer budgets. An early
  `BatchCutoff`-attributed provider phase timeout terminates only its slot unless the actual
  observation cutoff has expired.

### Encoding, Limits, And Error Classification

- Enforce independent encoded-transport, decoded-output, and final buffered-response caps.
  Decoded caps apply to identity and EdgeZero-decoded bodies, not raw passthrough encodings.
  Checks occur incrementally before appending excess output; response collection retains the
  configured bounds and original absolute deadline.
- Add `into_bytes_bounded`, `into_bytes_bounded_until`, `json_bounded`, and `json_bounded_until`
  for explicit response consumption. The `_until` helpers check the retained application clock
  cooperatively; pending-stream cancellation still depends on the adapter's deadline wrapper.
- Add response-header byte/count caps with typed overflow origins. Accounting includes every
  adapter-visible upstream field before normalization and the synthetic `x-edgezero-proxy` field.
  Preserve repeated headers, including `Set-Cookie`, and strip hop-by-hop fields consistently.
- Decode single bare `gzip`, zlib-wrapped `deflate`, and `br` codings. Validate multi-member gzip
  through transport EOF, reject trailing data after deflate/Brotli completion, and remove compressed
  representation headers only when EdgeZero decodes. Request bodies retain their declared encoding
  without implicit recompression.
- Replace bare `ContentEncoding::Passthrough` with `Passthrough(PassthroughReason)`, distinguishing
  `Stacked`, `Unsupported(token)`, and `Malformed` while retaining encoded bodies and headers.
- Reject excessive Brotli window bits before decoder allocation. Publish pinned Brotli and flate
  decoder memory-charge contracts and enforce the codec-independent `max_decoder_bytes` budget.
  Add opt-in `max_chunk_bytes` rechunking as an emitted-item bound, not a receive-allocation bound.
- Add `Deadline`, `DispatchBudget`, the EdgeZero-owned `MonotonicInstant` alias, and injectable
  `MonotonicClock`. Ingress, outbound clients, deferred response drains, config extraction, and
  egress use one application clock domain. Equality expiry wins; far-future deadlines are clamped
  and remaining dispatch budgets cannot grow beyond their entry duration. Backwards batch terminal
  observations and Fastly dispatch samples fail closed.
- Add typed timeout provenance (`BudgetSource`, including `BatchCutoff`), gateway transport/decode
  reasons, response-limit reasons, and store-extraction reasons. Distinguish initial unreachable
  failures, midstream transport failures, protocol failures, and content-coding layers without
  requiring message parsing where the provider exposes those distinctions. Opaque Cloudflare Fetch
  failures retain `BadGatewayReason::Unspecified`. Preserve timeout causes through decoder error
  conversion; upstream/response-limit failures use 502 and exhausted exchange budgets use 504.
- Use category-only public diagnostics for provider-backed failures so URLs, credentials, and
  provider error details cannot escape in framework HTTP error bodies.

### Ingress Admission And Routing

- Resolve routes before admission and body polling on all four standard adapters. Publish stable
  `RouteId`, `RouteMetadata`, and `RouteResolution::{Matched, MethodNotAllowed, NotFound}`, including
  optional manifest-sourced route `class` metadata.
- Stamp request start with the application clock, expose absolute read deadlines, and retain an
  opaque `IngressGrant` through admitted processing. Request contexts can consume the grant once.
- Replace eager unbounded inbound reads with bounded body consumption and deadline-aware wrappers.
  Cloudflare and Spin race pending asynchronous reads with timers; Axum owns its connection-local
  read future. Fastly retains its synchronous-hostcall limitation.
- Add cached, fallible `RequestContext::body_bytes(max)` and bounded JSON/form consumption.
  Failed or cancelled drains poison the body cell so later readers cannot retry a partially consumed
  source. Default extractors are bounded; `ValidatedJsonWithin<T, MAX>` and
  `ValidatedFormWithin<T, MAX>` let handlers select their own compile-time caps.
- Add `ReadBodyBeforeFallback` for bounded discard before canonical 404/405. It carries the
  application grant, byte cap, read deadline, and buffered `on_exceeded` / `on_timeout` responses.
  Overflow or timeout takes precedence without invoking a route handler or middleware; refusal
  remains the zero-read overload path.
- Add non-response `AdmissionDecision::Abort`. It starts no response lifecycle, invokes no handler,
  middleware, or completion callback, and polls no body. Axum closes the owned HTTP/1 connection;
  other adapters delegate the final transport effect to their providers.
- Add defensive normalized request-target/header/framing checks and typed accounting summaries.
  These do not claim access to raw parser-boundary memory or ambiguous raw framing.
- Emit a sorted, deduplicated `Allow` header for canonical 405 responses, including successful
  fallback drains. Add an application-controlled framework-error renderer while preserving status,
  `Allow`, and the effective config `Retry-After` policy after custom rendering. Applications remain
  responsible for bounding their renderer's output.

### Response Egress And Application Assembly

- Carry every response-producing admission outcome through `ResponseEgressEnvelope` and one
  `ResponseEgressAttempt`, retaining the application clock, policy, and response-scoped completion
  resource until the exactly-once terminal transition.
- Add non-clone `ResponseEgressCompletion`, generic `late_bound` resource installation, and
  left-to-right `join` composition. Completion runs before the global observer; abandonment and
  competing terminal paths release resources without duplicate reports.
- Refusal/fallback responses retain their admission-decision-owned completion resources. Detached
  normalized errors use `DetachedResponseEgressDecision::{Send { completion, deadline }, Abort}`;
  the application factory can reserve resources or abort without creating a response. Raw parser
  failures remain outside the application lifecycle.
- Support per-response absolute and egress-start-relative deadlines through
  `ResponseEgressDeadline`. An absolute `at(deadline)` can include preceding application queue time;
  `after(Duration)` resolves in `ResponseEgressEnvelope::begin` and excludes time before egress
  starts. Preserve the selected deadline through policy, framing, conversion, and delivery.
- Replace response-object handoff and whole-stream buffering with adapter-owned response pumps:
  Axum's Hyper connection, Cloudflare's writer/abort lifetime, Fastly's manual `stream_to_client`,
  and Spin's owned WASI response/body/result writers. Account accepted payload prefixes and release
  sources on timeout, disconnect, source failure, and terminal transport errors.
- Share response framing rules for HEAD, body-forbidden statuses, declared-length validation,
  hop-by-hop stripping, unsupported upgrades, and bounded pre-commit error fallbacks. Fastly manual
  framing is enabled only when a body is actually transmitted.
- Validate Axum exact-length stream EOF before handing Hyper the final frame; preceding frames
  remain incremental. Validate transmitted zero-length streams before handoff under the original
  deadline. Excess/late-error tails fail, pending/empty tails stay deadline-bounded, detached finite
  fallbacks retain GET payloads, and HEAD remains bodyless. Registration expiry selects 504 rather
  than a conversion 500.
- Make assembly core-owned and fallible through `App::build::<A>(platform)` and
  `Hooks::configure(&mut App) -> Result<(), EdgeError>`. Install target metadata before configuration,
  propagate hook errors unchanged, and reject successful configuration that changes any platform
  fact, provenance, or unknown reason. Add `app!(..., configure = ...)` with a fallible callback
  whose errors propagate through `App::build`. Axum completes assembly before listener binding.
- Add Fastly's closed `run_app_with_hooks` lifecycle for raw-request preparation and routed-response
  finalization while retaining adapter-owned delivery and returning routed hook state only after
  terminal delivery.

### Bounded Config And Secret Extraction

- Add `ConfigExtractionLimits` and typed bounded config/secret reads with per-value, backend-byte,
  and cumulative extraction caps under one extraction-start-relative timeout and application clock.
  Fastly chunk reconstruction and secret hydration share the same cumulative accounting/deadline.
- Bound JSON depth and node count before materializing a config envelope as `serde_json::Value`,
  so nested or oversized structural shapes are rejected before allocating the retained tree.
- Distinguish missing binding/value/secret, malformed envelope, unsupported envelope version,
  integrity failure, deserialization, validation, overflow, deadline, and provider-read failures
  through `StoreExtractionReason` and typed store errors rather than the previous coarse extraction
  classifications.
- Require provider implementations of `ConfigStore::get_bounded` and
  `SecretStore::get_bytes_bounded`; there is no default unbounded-read-then-check implementation.
  Return `BoundedStoreRead` with explicit backend bytes and an optional value.
- Preserve declared Cloudflare/Fastly config bindings when opening fails by installing typed failing
  handles. Optional missing-config policy cannot reinterpret a broken declared binding as absence.
  Defer Spin config-store opening until bounded extraction, after admission, so unrelated routes
  can still dispatch and opening participates in extraction timing.
- Race asynchronous Cloudflare/Spin reads with platform timers and recheck injected-clock expiry
  around ready results. Synchronous Axum/Fastly provider calls remain non-preemptible; byte and
  timeout controls do not certify provider allocation or finite cancellation latency.

### Platform resource metadata

- Add `Adapter::platform_metadata()` and application-visible `PlatformMetadata` with typed
  `PlatformFact::{Known, Unknown}` values, provenance, and unknown reasons for memory ceilings,
  concurrent live inbound population, and host ingress/framing memory accounting.
- Distinguish per-instance from per-execution ceilings and separate stack allowances. Publish
  canonical Axum, Cloudflare, Fastly, generic Spin, and opt-in Akamai Functions profiles; generic
  Spin remains runtime-configured rather than inheriting a hosted quota.
- Add checked, scope-aware `validate_memory_envelope` with `Fits`, `Exceeds`, and typed
  `Indeterminate` results. Reject scope mismatch and arithmetic overflow; unknown platform facts
  remain indeterminate rather than being replaced with guessed population or parser-memory bounds.

Applications and adapter implementations must migrate to the current platform-fact and response
construction APIs. The migration record includes APIs from earlier PR revisions as well as the
pre-PR release surface, so consumers pinned to intermediate commits can update directly. No
compatibility aliases are retained.

| Removed or changed API                                      | Replacement                                                                               |
| ----------------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `MemoryCeilingSource`                                       | `PlatformResourceSource` inside `PlatformFact::Known`                                     |
| `MemoryCeiling::new(total, scope, stack, source)`           | `MemoryCeiling::new(total, scope, stack)` wrapped in `PlatformFact::known(value, source)` |
| `MemoryCeiling::source()`                                   | Match or inspect the enclosing `PlatformFact`                                             |
| `PlatformMetadata::new(Option<MemoryCeiling>)`              | `PlatformMetadata::new(memory, inbound_population, host_ingress_accounting)`              |
| `PlatformMetadata::memory_ceiling() -> Option<_>`           | `PlatformMetadata::memory_ceiling() -> PlatformFact<_>`                                   |
| `Adapter::memory_ceiling()`                                 | `Adapter::platform_metadata()`                                                            |
| overridable `Hooks::build_app()` and infallible `configure` | `App::build::<A>(platform)` plus fallible `Hooks::configure`                              |
| four-argument `OutboundResponse::new`                       | five-argument constructor requiring the application `MonotonicClock`                      |

### Logging Ownership And Boot Diagnostics

`owns_logging = true` now explicitly owns backend installation, backend filters, and the facade
maximum before entering an adapter runner. `resolve_logging_level(&EnvConfig)` shares runtime
level resolution. Managed Axum/Fastly loggers retain `edgezero::boot` warnings and errors at
`Warn` even when ordinary logging is `Error` or `Off`. Cloudflare/Spin initializers remain no-ops.

Fastly `runtime_env_config` now returns `FastlyRuntimeConfig` with an `env` field and bounded
deferred diagnostics. Custom callers must initialize logging and call `emit_boot_diagnostics()`;
the former direct `EnvConfig` return is removed, with no compatibility alias. Managed runners
initialize logging before application configuration. Axum now returns logger-setup errors instead
of ignoring them; preinstalled backends must use `owns_logging = true`.

Axum's `run_app_with_preflight` supports checks after logger setup and before configuration or
listener binding. The bundled demo uses it without first installing the CLI logger. Terminal CLI
errors remain visible on stderr regardless of runtime logging level.

### CLI, Capabilities, Demo, And Generator

- Add typed manifest capabilities and `Adapter::capability` with `Native`, `BoundedCooperative`,
  `BestEffort`, and `Unsupported`. Runtime-producing CLI operations validate required support
  before side effects; optional support emits boot diagnostics without blocking execution.
  Unknown names, duplicates, overlap, and malformed baked contracts fail closed.
- Add canonical outbound-host declarations, with an omitted declaration defaulting to HTTPS only
  and explicit `"*"` enabling HTTP and HTTPS. These configure platform plumbing, not destination
  authorization. Spin's selected component must match the canonical host contract.
- Pin CLI execution to the resolved application root, platform manifest, and component through
  `AdapterExecutionTarget` / `execute_target`, including manifest-defined shell commands. Reject
  ambiguous or escaping manifest selection instead of checking one project and executing another.
  Fastly deployment honors the selected manifest path.
- Migrate checked-in demos and generated applications to the new client, buffered fanout,
  passthrough classification, response limits, clocks, ingress policy, and fallible assembly.
  Include explicit missing-client 501 behavior and partial-result/failure examples.
- Keep runtime setup in reusable adapter runners rather than copying transport or logging code
  into templates. The demo illustrates completion composition and late-bound ownership; generated
  scaffolds retain finite admission/egress policies with core-owned empty lifecycle tokens.
- Gate `handlebars` behind the existing `edgezero-cli/cli` feature so runtime-only library consumers
  do not pull scaffolding or implicitly enable its `serde_json/preserve_order` dependency feature.

### Additional Hard Migrations

| Removed or changed API                                                                                | Replacement                                                                                                             |
| ----------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `ProxyClient`, `ProxyHandle`, `ProxyRequest`, `ProxyResponse`, `ProxyService`, adapter `*ProxyClient` | `OutboundHttpClient`, `HttpClient`, `OutboundRequest`, `OutboundResponse`, client forwarding, adapter `*OutboundClient` |
| Infallible proxy request constructors and tuple `into_parts()`                                        | Fallible `OutboundRequest` constructors / `from_parts` and structured `OutboundRequestParts`                            |
| Proxy request extension access and `body_mut()`                                                       | Application-owned state and outbound request builders / structured parts                                                |
| `Body::from_stream` with arbitrary external errors                                                    | Typed `EdgeError` items, or `Body::from_external_stream` for external errors                                            |
| Synchronous/unbounded request-context body access                                                     | Async `body_bytes(max)`, `json_within(max)`, `form_within(max)`; fallible `take_body()` / `into_request()`              |
| `EdgeError::MethodNotAllowed.allowed: String`                                                         | Structured `Vec<Method>` used to render the mandatory `Allow` header                                                    |
| `config_out_of_date_from_serde`                                                                       | `store_deserialization_from_serde` and `EdgeError::StoreExtraction { reason, .. }`                                      |
| Store implementations with only unbounded reads                                                       | Required `ConfigStore::get_bounded` / `SecretStore::get_bytes_bounded` implementations                                  |
| Decoder helpers without a memory policy                                                               | Codec helpers accepting decoder budgets and Brotli window policy                                                        |
| Response-returning Fastly entrypoints and `#[fastly::main]`                                           | An undecorated `main` calling a send-owning adapter runner                                                              |
| Transport-blind response converters and stream-buffer fallback constants                              | Adapter-owned response-egress coordinators                                                                              |
| Fastly `runtime_env_config(...) -> EnvConfig`                                                         | `FastlyRuntimeConfig`, using `.env` and `emit_boot_diagnostics()` after logger setup                                    |

Earlier PR revisions additionally require these migrations:

| Superseded intermediate API                                      | Current API                                                                       |
| ---------------------------------------------------------------- | --------------------------------------------------------------------------------- |
| `send_all` returning bare positional outcomes                    | `start_batch_until` / `next` / `cancel` / `collect`, or fallible `send_all_until` |
| Batch `cutoff` parameter described as a batch deadline           | Explicit `observation_cutoff` that stops observation regardless of slot budgets   |
| Bare `ContentEncoding::Passthrough`                              | `ContentEncoding::Passthrough(PassthroughReason)`                                 |
| Codec-specific `max_brotli_decoder_bytes`                        | Codec-independent `max_decoder_bytes`                                             |
| Admission refusal/fallback without response completion ownership | Decision-owned `ResponseEgressCompletion` transferred through ingress and egress  |
| Detached completion-only factory                                 | `DetachedResponseEgressDecision::Send { completion, deadline }` or `Abort`        |

### Dependencies And Toolchain

- Hard-cut platform SDKs to Fastly `0.13.1` (SDK/shared/sys/logger), Cloudflare `worker 0.8.5`, and
  `spin-sdk 7.0.0`, adapting Spin to its WASI HTTP 0.3 response/body/result resource protocol.
- Pin native outbound dependencies (`reqwest 0.13.4`, `url 2.5.8`) and audited decoder graphs,
  including `async-compression 0.4.43`, `compression-codecs 0.4.38`, `brotli-decompressor 5.0.1`,
  and `flate2 1.1.9`. Decoder charge changes require explicit pin/audit updates, not lockfile-only
  assumptions. Spin's SQLite config writer is reverified against Spin 4.1.0.
- Update runtime/tool pins to Spin CLI `4.1.0`, Viceroy `0.21.0`, and Wasmtime `48.0.2`; generated
  projects select corresponding SDK generations and applicable platform CLI/Viceroy pins. Root
  workspace dependencies use exact SDK pins. Rust `1.95.0`, Node `24.12.0`, and Fastly CLI `15.1.0`
  remain the workspace toolchain.

### Tests, Documentation, And Maintenance

- Expand native and provider contract coverage for deadlines, decoding, limits, batch ordering,
  driver failure/cancellation, admission/fallback precedence, clocks, framing, and resource release.
  Add Hyper raw-socket, Spin SDK-resource, Fastly closed-lifecycle, and demo logging regressions.
- Gate native/WASM feature combinations independently, include `test-utils` runtime paths, and
  require named nonzero test sentinels. Consolidate adapter clippy into one workflow matrix while
  retaining distinct per-target check names, including native Axum.
- Verify generated all-adapter projects and the excluded demo workspace. Fetch both locked
  dependency graphs before offline audits; enforce decoder dependency, JSON-map, documentation,
  public-rustdoc, and retired-API contracts in CI.
- Update outbound, ingress, egress, and configuration specs/plans, capability matrices, migration
  guides, and examples. Replace ordinary hand-built JSON values/fixtures with `serde_json::json!`,
  retaining raw input only for syntax, exact-byte, static-input, and serialization-free fallback
  requirements.

### Capability And Evidence Boundaries

- Axum alone reports native outbound and inbound-read deadline enforcement. Cloudflare outbound
  and streamed-upload deadlines remain `BestEffort` pending deployed host-observed cancellation
  evidence; Fastly and Spin retain their documented phase-timer/teardown limitations.
- Fastly fanout is dispatch-before-harvest, not guaranteed simultaneous overlap. Synchronous
  registration/body drain and bounded handle-selection groups limit completion order, cancellation,
  and slot isolation; dynamic backends require provider enablement. A 25 ms dispatch-slack guard can
  reject a still-live slot before synchronous cold registration, including heterogeneous hosts and
  budgets. Cloudflare header fidelity and authority override remain `BestEffort`; Spin authority
  override is rejected because routing and authority cannot be separated through its provider API.
- Response-egress abort, backpressure, completion, and write deadlines remain `BestEffort` on all
  adapters. An exactly-once guest lifecycle report does not prove end-client receipt or a finite
  provider teardown interval. Configuration-read deadlines also remain `BestEffort` everywhere.
- Raw ingress head limits/framing guarantees, complete outbound resource accounting, and complete
  config-read allocation bounds remain `Unsupported`. Provider parsing, materialization, SDK copies,
  hidden informational fields/trailers, allocator overhead, and host buffering are not certified by
  guest-visible byte, header, chunk, or decoder-policy caps.

See [the capability guide](docs/guide/capabilities.md) for the exact per-target matrix, accounting
boundaries, canonical platform facts, and evidence required before promoting support.
