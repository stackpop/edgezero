# Shared request timing collector and middleware

Date: 2026-09-28
Status: EdgeZero review API revision implemented and locally verified. Renewed Trusted Server integration and joint validation remain required before release.
Scope: `edgezero-core` and coordinated Trusted Server adoption, shipped together.
Plan: [Implementation plan](../plans/2026-09-28-request-timing.md).

The initial candidate is [EdgeZero #389](https://github.com/stackpop/edgezero/pull/389)
at `9c03cc59300363ae5339fc970dded08ede563e98`, adopted by
[Trusted Server #1121](https://github.com/IABTechLab/trusted-server/pull/1121)
at `2791ceb46908e6af04ccaae0c10b29d35336a9f4`. The linked PRs record completed
migration and joint validation for those exact revisions, not later API changes.
Merge, release/tag, final dependency pins and tag-backed validation remain pending.

## Why this change

Trusted Server PR [#1121](https://github.com/IABTechLab/trusted-server/pull/1121)
adds request-timing middleware in both its Cloudflare and Spin adapters. The two
implementations are identical. Aram's
[blocking review comment](https://github.com/IABTechLab/trusted-server/pull/1121#discussion_r4090027047)
asks for the generic collector and middleware to live in EdgeZero, while keeping
auction-specific state in Trusted Server. He also accepts an interim move of the
duplicated middleware into `trusted-server-core`.

This is a cross-repository API change, not just moving a struct. EdgeZero cannot
depend on Trusted Server, and replacing the collector must not reset the clock
used by auction diagnostics or change telemetry and header output.

Initial investigation used EdgeZero `567964158e4f8bd0d52321b9801de44966422e1b`,
locally tagged `v0.0.8`. Before creating the implementation worktree, local `main`
was fast-forwarded to fetched `origin/main` at
`98930917d96cb665c7255f36ca8bbd61fc7539e1`. The intervening changes leave the timing
integration points in core middleware, router, context, and Cargo dependencies
unchanged. The middleware guide changed and must be edited against this new base.

The initial Trusted Server baseline was `f951955f537b89392dcb863fca7a01a5f6fa4846`,
with six EdgeZero dependencies pinned to `v0.0.7`. The migration at `2791ceb`
replaced those with the exact `9c03cc59` candidate. See the plan for historical
evidence and the remaining review-update and release gates.

## Approved decisions

1. Applications use typed phase enums. EdgeZero stores bounded indexed slots;
   Trusted Server's typed facade owns the enum-to-slot mapping. No string registry
   or new public phase trait is needed for the first release.
2. A small application-defined payload shares the collector's clock and mutex.
   Narrow synchronous update and snapshot operations preserve compound facts.
3. Install one concrete EdgeZero handle type in request extensions. Trusted Server
   adds domain methods through a facade over that same handle, not a second clock.
4. Keep `Server-Timing` rendering and exposure policy in Trusted Server for the
   first release. A generic renderer is out of scope.
5. Ship EdgeZero and Trusted Server adoption as one coordinated delivery. Do not
   perform a temporary Trusted Server-only middleware move.

## Goals

- Provide one portable per-request clock and a reusable collector in EdgeZero.
- Attach a fresh collector through shared middleware only when one is absent.
- Preserve existing application marks, compound updates, and consistent snapshots.
- Keep collection independent from exposing timings in headers, logs, or bodies.
- Let Trusted Server remove its duplicate middleware without changing its public
  diagnostic payloads or access-log schema.

## Non-goals

- Moving auction concepts, UUIDs, EC storage names, or Trusted Server privacy
  policy into EdgeZero.
- Moving the other duplicated Trusted Server middlewares in this change.
- Automatically instrumenting every adapter, route, or streamed body.
- Changing router dispatch, error rendering, macro expansion, manifest schema, or extractors.
- Adding tracing export, dynamic metric registration, retries, or a public clock
  injection framework.
- Changing the browser's SSAT classification or comparing browser and server clocks.

## Ownership

| Concern | Owner after migration |
| --- | --- |
| Portable monotonic origin and cloneable request handle | EdgeZero |
| Bounded phase durations, saturating accumulation, drop-time spans | EdgeZero |
| Headers-ready and request-complete marks, response byte count | EdgeZero |
| Nonblocking collection and consistent snapshot mechanics | EdgeZero |
| Insert-if-absent middleware and configurable exclusion mechanism | EdgeZero |
| Eight-phase enum and mapping to generic storage | Trusted Server |
| Auction wait placement, UUID, dispatch/resolve/commit semantics | Trusted Server |
| `ts-*` header names, ordering, and existing serialization | Trusted Server |
| Header enablement, cache privacy checks, browser activation gates | Trusted Server |
| Where each adapter starts and finishes measurement | Application/adapter integration |

The reviewer also identifies `server_timing_value` as movable. The approved scope
keeps Trusted Server's renderer in place for the first extraction: its names and
header-bearing phase list are application policy. A generic renderer would add
validation and ordering contracts that are unnecessary for this migration.
Explain this narrower scope in the review response; do not claim that the whole
of the reviewer's suggested extraction moved upstream.

## Clock and lifecycle contract

T0 is the instant the request collector is created at its configured application
or adapter boundary. It is not browser `navigationStart`, socket acceptance, or
an assumed uniform platform ingress timestamp. Existing adapters start after
different amounts of prologue work. The migration preserves those boundaries.

Every clone, phase span, generic lifecycle mark, and application milestone must
refer to that same origin. Wrapping an existing handle must not allocate another
clock or an independent copy of its mutable state. The immutable origin lives
outside the state mutex, so `elapsed()` is available during contention.

On [deployed Cloudflare Workers](https://developers.cloudflare.com/workers/runtime-apis/performance/),
timers advance only after I/O and CPU-only phases may record zero. Local
Wrangler/workerd and Chromium tests do not reproduce that restriction. A shared
origin does not imply uniform CPU-time resolution across providers.

```mermaid
flowchart TD
    A[Adapter receives request] --> B{Collector already attached?}
    B -->|Yes| C[Reuse existing handle and T0]
    B -->|No| D[Configured attachment boundary]
    D --> E{Request excluded?}
    E -->|Yes| F[Continue without installing a collector]
    E -->|No| G[Create request-local collector]
    C --> H[Middleware and application record timings]
    G --> H
    H --> I[Application marks headers ready at its terminal boundary]
    I --> J[Application emits permitted header timing]
    J --> K[Application records body completion where observable]
    K --> L[Application renders permitted access telemetry]
```

The diagram describes ownership, not a new automatic finalization pipeline.
Header rendering occurs at headers-ready; later body marks can appear only in
later snapshots such as access telemetry.

### Middleware behavior

The shared middleware is opt-in and attachment-only:

1. Preserve a collector of the configured concrete type if already present.
2. Otherwise evaluate the application's exclusion policy.
3. For an eligible request, create and insert a fresh collector.
4. Pass the context to `next.run` and preserve its result unchanged.

Default configuration excludes no paths. Trusted Server's Cloudflare and Spin
registrations configure an exact `/health` path exclusion for every method,
including requests with query strings on that path. Exclusion prevents
installation, not removal of a preinstalled collector. Duplicate middleware
installation must neither reset T0 nor erase recorded facts.

Preserve the existing adapter differences: Axum also excludes `/health` for every
method, while Fastly short-circuits only `GET /health` before collector creation.
Non-GET Fastly health requests continue through normal timing setup. Normalizing
that policy across adapters would be a separate behavior change.

Keep the exclusion API small. A static path list is sufficient for the current
Cloudflare/Spin consumers. `new()` and `with_excluded_paths` are const, so a
manifest can name an exported middleware const. Each exclusion call replaces the
previous list. Owned paths or predicates remain deferred until a concrete need emerges. Fastly keeps its existing method-aware entry-point check.
Neither collector nor application payload belongs in `RouterBuilder::with_state`:
that state is shared across requests and can overwrite same-type extensions.
Use optional typed extension lookup, not `State<RequestTimings<...>>`: excluded
or absent handles otherwise fail extraction with a 500.

Register the middleware before consumers that need timing. Trusted Server's
client-IP sanitization must remain first; extraction must not change that order.

### Boundaries the middleware cannot cover

EdgeZero matches a route before running middleware. Unmatched 404/405 requests
bypass it. `RouterService::oneshot` also renders dispatch errors outside the
middleware chain. Returning a response does not mean a streaming body has been
consumed or sent.

Consequently:

- Axum retains its outer terminal timing service and error-response coverage.
  For non-excluded requests, the migrated service retrieves the configured
  concrete handle if present, creating and inserting one only when absent. Both
  the handler and terminal renderer use that handle. The regression at `2791ceb`
  protects the fix to the baseline wrapper's unconditional replacement. The
  health bypass remains unchanged.
- Fastly retains its early collector creation, app-build span, header emission,
  streaming duration, and successfully written-byte accounting.
- Cloudflare and Spin adopt the shared attachment middleware without gaining
  `Server-Timing` emission incidentally.
- No middleware return or guard drop automatically stamps request completion.

A cancelled phase span can record work up to its drop. That is not evidence that
the request, response body, or auction completed. Panic-abort environments do not
guarantee destructor execution.

## Collector shape

`RequestTimings<const N: usize, D = ()>` is a cloneable handle backed by one
`Arc<Shared<N, D>>`. `Shared` holds the immutable origin beside a mutex protecting
generic lifecycle fields, a bounded phase array and application-owned payload.
Only elapsed reads bypass that mutex; compound updates and projections still
use one lock.

The payload lets Trusted Server keep auction definitions in its own crate while
sharing synchronization with the generic fields. For example, recording auction
wait currently updates both a phase total and its placement under one lock.
Snapshots read these together. Splitting them across two locks would change that
contract even if both collectors shared T0.

Generic operations should cover:

- Construction, elapsed time from T0, and cheap handle cloning.
- Saturating phase accumulation and a span that records elapsed time on drop.
- First-successful-write `headers_ready` and `request_complete` elapsed marks.
- Last-write response byte count.
- A consistent read of generic fields and application payload.
- A short synchronous update of a phase and application facts under the same lock.

Construction with `D::default()` is sufficient for middleware installation. Core
operations need not require every payload to implement `Default` or `Clone`.
Manual handle cloning must not accidentally impose `D: Clone`. Installed handles
must satisfy request-extension `Clone + Send + Sync + 'static` requirements;
middleware futures remain `?Send`.

Trusted Server can keep its existing domain methods behind a facade over the
concrete EdgeZero handle. Middleware and consumers must use the same extension
type. A facade that reads the generic handle is acceptable; a wrapper type that
expects an independently installed extension is not. The first API implementation
slice must include a compiled usage fixture of this boundary before adapter
migration begins.

### Phase representation alternatives

| Option | Benefits | Costs |
| --- | --- | --- |
| Application enum implementing an EdgeZero phase trait | Typed generic calls and application-owned names | Must define count, unique mapping, and invalid mappings |
| Fixed indexed slots, with an application enum facade | Preserves bounded storage and current TS mapping with a small API | Raw indices need bounds handling |
| String-keyed phases | Convenient arbitrary labels | Requires allocation, cardinality limits, duplicate and overflow policy |

Selected: fixed indexed slots inside EdgeZero, with typed phase enums at
application call sites. `N` is chosen at compile time, not from a request. Invalid
indices must be rejected without panicking or mutating state, and the caller must
be able to distinguish them from a successful write. Do not silently alias them
to another phase. `TimingError::InvalidSlot` reports that rejection.

### Synchronization and failure behavior

Preserve nonblocking `try_lock` behavior for mutable state. A contended update
drops the whole sample; a contended snapshot reports unavailable. `elapsed()`
reads the immutable origin without locking and cannot fail from contention.
Trusted Server maps unavailable projections to its existing all-None snapshot or
omitted header. Do not retry, block, or substitute zero for missing facts.

Phase durations are sums, not the union of overlapping intervals. Concurrent
spans on one slot can total more than wall-clock time; use one span around a join
to measure concurrent latency.

Any application update callback must be synchronous, short, and non-reentrant.
It cannot retain a guard across an await or call a lock-taking collector method
from inside itself. Lock-free `elapsed()` is safe inside callbacks. One lock makes
successful compound updates and snapshots
consistent; it does not provide rollback if a callback panics partway through.

The baseline TS collector recovered poisoned locks because its state was simple
counters and optional facts. The generic API preserves best-effort recovery,
with callbacks required to leave valid data even on unwind. Panics propagate and
may leave partially changed facts; recovery provides neither rollback nor payload
validation. This is not unconditional infallibility for arbitrary user code.

## Trusted Server compatibility requirements

These are migration acceptance criteria, not application features to add to EdgeZero.

| Existing behavior | Required outcome |
| --- | --- |
| Repeated phase durations | Saturating addition; missing differs from recorded zero |
| Headers-ready, completion, auction milestones and UUID | First successful write wins |
| Response bytes and auction placement | Last successful write wins |
| Auction wait | Accumulate duration and update placement in one operation |
| Snapshot durations | Whole milliseconds, truncated and saturated to `u32::MAX` |
| Header durations | One decimal place; existing names, order and omission rules |
| Header emission | Mark headers-ready even when emission is disabled; append rather than overwrite |
| Cache policy | Retain existing private/no-store predicate; never change cacheability |
| Browser timing payload | Preserve field names, activation gates and omitted-value behavior |
| Access telemetry | Preserve field names, nullability, UUID join key and placement encoding |

Auction UUID and dispatch marking remain independent. An attempted auction may
have an ID without a dispatch mark; an abandoned auction may have dispatch but no
resolution. Missing samples can also result from contention. Do not reinterpret
missing timing as proof that no auction occurred.

The page-bids path already omits diagnostics when the adapter collector is
missing. The initial-document path lacks the equivalent guard. Aram identified
that as a nonblocking
[follow-up](https://github.com/IABTechLab/trusted-server/pull/1121#discussion_r4090027050),
not a currently reachable production failure. Track it separately with an
initial-document missing-collector regression test; no browser classification
change is required by that finding.

## Delivery sequence

```mermaid
flowchart TD
    A[Confirm API details and authorize implementation] --> B[Implement EdgeZero collector and middleware]
    B --> C[Integrate TS against the candidate EdgeZero revision]
    C --> D[Validate both repositories before release]
    D --> E[Approve EdgeZero merge and git tag]
    E --> F[Pin all TS EdgeZero dependencies to that tag]
    F --> G[Revalidate without local overrides]
    G --> H[Approve coordinated TS delivery]
```

1. Resolve the implementation checkpoints below and review the linked plan before
   starting code changes.
2. Implement in small tested steps, starting with
   `crates/edgezero-core/src/request_timing.rs`, its declaration in `src/lib.rs`,
   and shared middleware in `src/middleware.rs`. Colocate tests. Add usage docs in
   `docs/guide/middleware.md`. Router changes are not expected.
3. Prepare Trusted Server adoption against the candidate EdgeZero revision using
   temporary exact git-revision pins to a pushed candidate. Pin all six TS dependencies to that same commit and verify one core
   package identity, without developer-specific paths. Replace collector mechanics
   and the Cloudflare/Spin middleware copies; preserve the TS domain facade and
   Fastly/Axum boundaries.
   Validate both repositories before considering the upstream release ready.
4. With separate publication approval, merge and tag EdgeZero through its normal
   git-tag process. Do not assume a tag number or promise crates.io publication;
   the inspected workspace has `publish = false`.
5. Update all six EdgeZero pins in TS `Cargo.toml` and review regenerated
   `Cargo.lock`, including changes since `v0.0.7` unrelated to timing. Remove local
   overrides and revalidate against the released tag before TS delivery.

Ship together means coordinated readiness and dependent releases, not simultaneous
Git operations across repositories. EdgeZero's tag must exist before the final TS
pin can resolve. Temporary exact pushed git-revision pins support joint PR
validation only; replace them with the approved release tag before final TS
delivery. Neither side is considered complete without verified TS adoption.
No intermediate TS-only middleware move, unrelated lock-helper cleanup, or other
adapter middleware extraction is included.

## Verification plan

### Collector contracts

Use deterministic durations and private test-only origin control where needed;
no public clock trait is required just for testing. Cover:

- Clone sharing, independent requests, and preservation of a pre-aged T0.
- Absent versus recorded-zero phases, accumulation, saturation and invalid slots.
- First-write marks, last-write byte counts, and explicit lifecycle completion.
- Compound phase/payload updates and consistent snapshots.
- Contention dropping whole state operations, lock-free elapsed reads across
  threads and inside callbacks, and poison behavior under the chosen contract.
- Span drop and cancellation without fabricated request completion.

### Middleware and consumer contracts

Exercise the real router interface with `futures::executor::block_on`. Verify
insert-if-absent, repeated installation, exact health exclusion, preinstalled
handles on excluded paths, ordering, short circuits, and unchanged errors.
Pin the documented unmatched-route boundary rather than implying blanket coverage.
Test GET and non-GET `/health`, with and without query strings, against each
adapter's preserved method policy. For Axum, attach a pre-aged collector in an
upstream service and prove that its origin and recorded phases survive both
handler access and private-response header rendering. This regression should
fail against the baseline unconditional replacement and pass after the wrapper fix.

Before replacing TS mechanics, establish its existing output fixtures. Retain
private-header append/enable/cache tests, Axum terminal error coverage, Fastly
streaming byte counts, and both document and page-bids diagnostic gates. Prove
that a pre-aged collector yields nonzero pre-dispatch time without changing the
origin during extraction. Verify all four adapters still deliver the same handle
to their core consumers.

### Commands for implementation

EdgeZero:

```sh
cargo test -p edgezero-core
cargo test -p edgezero-core --doc
cargo test --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Also run the repository's adapter WASM CI matrix, including Fastly
`wasm32-wasip1` and Cloudflare `wasm32-unknown-unknown`, plus existing generated-app
and demo checks relevant to a public core API change. Compile checks do not prove
runtime clock behavior; distinguish native execution, WASM compilation, and any
platform clock smoke tests in the implementation report. Run the shared attachment
scenario natively as well as in all three adapter harnesses. Separate advancement
tests use native sleep or bounded polling in Viceroy, Wasmtime and Chromium, with
an independent wall-clock deadline and an iteration cap. They do not establish
deployed Cloudflare CPU-time behavior.

Trusted Server uses its target-matched `cargo test-fastly`, `cargo test-axum`,
`cargo test-cloudflare`, `cargo test-spin`, and `./scripts/test-cli.sh` commands.
Run `cargo fmt --all -- --check` and applicable adapter clippy aliases, including
WASM variants. Retain cross-adapter integration coverage and run JS/browser
regressions for auction diagnostics. Do not use bare workspace-wide Cargo tests
across its incompatible targets.

For the internal spec/plan, inspect links, Markdown structure, and the final diff.
`docs/superpowers/**` is excluded from both the published site and the normal docs
Prettier check, so a passing site check would not validate this document.

## Approved API contract

- `RequestTimings<const N: usize, D = ()>` holds one `Arc<Shared<N,D>>`, with
  immutable `t0` outside the mutex protecting `Inner<N,D>`.
  `with_data(D)` needs neither `Default` nor `Clone`; `new`/`Default` need
  `D: Default`. Manual handle cloning never needs `D: Clone`. Extension and
  middleware payloads need `D: Send + 'static`, not `D: Sync`.
- Non-exhaustive `TimingError::{InvalidSlot, Unavailable}` distinguishes invalid
  slots from contention. `record`, lifecycle marks and `set_response_bytes` return
  `Result<(), TimingError>`; lock-free `elapsed()` returns `Duration`.
  Repeated first-write marks return `Ok(())` without replacing the value.
- Snapshot lifecycle fields are `headers_ready`, `request_complete` and
  `response_bytes`. Marks use `mark_headers_ready()` and `mark_request_complete()`.
  They record elapsed durations at explicit application boundaries, not automatic
  proof of streamed-body completion.
- `update_data(FnOnce(&mut D, Duration) -> R)` and
  `record_with(slot, duration, FnOnce(&mut D, Duration) -> R)` return
  `Result<R, TimingError>`. Duration is elapsed from the immutable origin.
  `record_with` validates its slot before acquiring the lock or invoking the
  callback, then accumulates the phase and invokes the callback under one lock.
- `snapshot(FnOnce(TimingSnapshot<'_, N, D>) -> R)` projects copied generic
  fields/current elapsed and borrowed `&D` under that same lock. Neither the
  borrowed payload nor the guard can escape; payload cloning is not required.
  `TimingSnapshot<'data, N, D = ()>` is non-exhaustive to permit later fields.
- Contention invokes no callback and changes no state. Callbacks are synchronous,
  short and non-reentrant. Panics propagate, may leave phase/payload partially
  changed, and poison the mutex. Poison recovery is best-effort, **not rollback**
  or payload validation; callbacks must preserve payload validity on unwind.
- `span(slot)` returns `Result<PhaseSpan<N,D>, TimingError>` after bounds
  validation. Drop attempts a best-effort record, discarding contention; it never
  marks request completion.
- `RequestTimingMiddleware<N,D>` defaults to no exclusions and offers const
  `new()` and `with_excluded_paths(&'static [&'static str])`, enabling named-const
  manifest registration. The setter replaces the previous list. The TS facade
  reads exactly `RequestTimings<8, TsPayload>` from extensions rather than
  installing itself.
- `RequestTimings` implements `Debug` without a payload bound or lock acquisition,
  and omits application data. Phase guards must be bound to named variables to
  measure the enclosing scope.

## Deferred follow-ups

These review suggestions remain outside this release's scope:

- [Contention diagnostics](https://github.com/stackpop/edgezero/pull/389#discussion_r4162404464):
  explicit operations already report `Unavailable`. A future counter needs
  read/write and consistency rules; a request-wide count cannot attribute a
  missing phase or prove that no auction occurred.
- [Runtime-config exclusions](https://github.com/stackpop/edgezero/pull/389#discussion_r4162404477):
  current consumers use static paths. Any owned-path API should preserve const
  configuration where practical and specify allocation and validation behavior.
- [Response propagation or an outer service](https://github.com/stackpop/edgezero/pull/389#discussion_r4162404480):
  current outer layers preinstall and retain a clone. Successful-response
  extensions alone would not cover unmatched routes, rendered errors or every
  adapter conversion. A wrapper must preserve application measurement boundaries.
- [Explicit span finish/discard](https://github.com/stackpop/edgezero/pull/389#discussion_r4162634076):
  manual `Instant` plus `record_with` handles conditional and compound recording
  today. Do not use `mem::forget` to discard a span; it leaks the shared handle.
  Future finish/cancel methods must avoid duplicate Drop recording and release
  the handle, with compound finishing considered separately.
- [Optional timing extractor](https://github.com/stackpop/edgezero/pull/389#discussion_r4162634050):
  use optional typed extension lookup today. A future extractor must preserve
  missing-handle behavior without creating router-wide collector state.

## Remaining release checkpoints

1. Update the six dependency-facing TS facade accesses for the renamed EdgeZero
   fields/methods while preserving TS facade and wire names. Refresh all six
   temporary dependency pins to one pushed candidate and verify one core identity.
2. Revalidate both repositories against that exact candidate. Earlier joint
   evidence at `9c03cc59`/`2791ceb` does not cover this API revision.
3. Approve the EdgeZero merge/tag through its normal release process, then replace
   temporary TS pins with that tag and perform final tag-backed validation.

Publication, merge, releases, GitHub replies and thread resolution require their
own authorization; this record does not grant it.

## Source references

EdgeZero implementation and runtime tests:

- `crates/edgezero-core/src/request_timing.rs`: shared collector and lifecycle API.
- `crates/edgezero-core/tests/request_timing_consumer.rs`: public facade and native scenario.
- `crates/edgezero-core/tests/support/request_timing.rs`: shared adapter fixture.
- `crates/edgezero-macros/tests/app_macro.rs`: named-const manifest registration.
- `crates/edgezero-core/src/middleware.rs`: `Middleware`, `Next`, `RequestLogger`.
- `crates/edgezero-core/src/router.rs`: route matching, state injection, and
  error-to-response conversion.
- `crates/edgezero-core/src/context.rs`: public request-extension access.
- `crates/edgezero-core/Cargo.toml`: existing `web-time` dependency.
- `.github/workflows/test.yml` and `.github/workflows/format.yml`: target matrices.

Trusted Server initial-baseline sources, before the migration at `2791ceb`:

- [Collector and application rendering](https://github.com/IABTechLab/trusted-server/blob/f951955f537b89392dcb863fca7a01a5f6fa4846/crates/trusted-server-core/src/request_timing.rs).
- [Axum terminal timing service](https://github.com/IABTechLab/trusted-server/blob/f951955f537b89392dcb863fca7a01a5f6fa4846/crates/trusted-server-adapter-axum/src/timing.rs).
- [Fastly request and delivery boundaries](https://github.com/IABTechLab/trusted-server/blob/f951955f537b89392dcb863fca7a01a5f6fa4846/crates/trusted-server-adapter-fastly/src/main.rs).
- [Publisher diagnostics](https://github.com/IABTechLab/trusted-server/blob/f951955f537b89392dcb863fca7a01a5f6fa4846/crates/trusted-server-core/src/publisher.rs).
