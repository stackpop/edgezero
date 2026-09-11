# EdgeZero Response Egress Design

> **Status:** Draft implementation gate for bounded client-response writes.
> **Owner:** Core response lifecycle plus adapter response converters.

## 1. Scope and boundary

This design owns the lifetime after router dispatch returns a core `Response` and before the
adapter has either completed the platform response handoff or terminally aborted it. It
covers absolute write deadlines, backpressure, client disconnect, abort behavior, and one
terminal completion notification.

It does not extend an outbound fetch deadline, classify upstream failures, or change
outbound response decoding. Those end when the outbound adapter returns an
`OutboundResponse`, as defined by the
[outbound HTTP design](2026-05-21-outbound-http-design.md). Inbound request admission and
body-read deadlines are owned by the
[inbound-body design](2026-08-22-inbound-body-design.md).

Current converters implement the core policy/observer guard and converter-level absolute deadline,
but they do not provide the full transport lifecycle: Axum and Fastly synchronously collect a core
stream, Spin materializes it into `FullBody`, and Cloudflare adopts a stream without a certified
transport completion boundary. Axum emits `ResponseReturned`
with zero written bytes after conversion but before returning the response to Hyper. Returning
a platform response object is not evidence that Hyper accepted it or that bytes reached the
client.

## 2. Core contract

### 2.1 Policy and timing

`ResponseEgressEnvelope` retains the exact `App::monotonic_clock()` clone used for ingress and
standard outbound dispatch. `begin()` snapshots `egress_started_at` from that clock immediately
before its first response-conversion operation and returns the same clock with the response,
policy, and attempt. The adapter then obtains one finite absolute deadline:

```rust
pub const DEFAULT_RESPONSE_WRITE_BUDGET: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub struct ResponseEgressPolicy {
    pub write_deadline: Deadline,
}
```

`App` owns a synchronous response-egress policy callback configured through
`Hooks::configure`. The callback receives immutable response-head metadata plus the captured
request start and optional route metadata; it cannot consume the body. The head exposes
status, version, visible headers, `request_start`, and `Option<&RouteMetadata>` through
borrowed accessors. It contains no body handle and cannot mutate the response. The portable
default is `Deadline::at_instant(egress_started_at + DEFAULT_RESPONSE_WRITE_BUDGET)`.

The adapter normalizes every callback result to no later than
`egress_started_at + DEADLINE_FAR_FUTURE`, using checked addition. An already-expired result
is a valid immediate deadline. Arithmetic overflow while deriving the clamp fails before
commit as `ConversionError`; it does not silently create an unbounded write.

The deadline is absolute and never reset by response conversion, first-byte wait,
backpressure, chunk boundaries, flushes, trailers, or platform finish. Before every source
poll/platform write and immediately after either side becomes ready, the adapter compares
the same deadline. Equality is expired; deadline expiry wins simultaneous readiness.
Every callback sample, deadline comparison, body-collection check, and terminal report uses the
returned clock. Low-level converters that bypass `App` explicitly use `MonotonicClock::default()`;
they are not app-clock propagation paths. Wall-clock time is not used.

### 2.2 Terminal report

The app may install one observer. The adapter invokes it synchronously exactly once per
response conversion attempt:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressOutcome {
    ClientDisconnected,
    Completed,
    ConversionError,
    DeadlineExceeded,
    HostHandoff,
    ResponseReturned,
    SourceError,
    TransportError,
    Unspecified,
}

#[derive(Clone, Debug)]
pub struct ResponseEgressReport {
    pub bytes_written: u64,
    pub elapsed: Duration,
    pub outcome: ResponseEgressOutcome,
    pub request_start: MonotonicInstant,
    pub route: Option<RouteMetadata>,
}

pub trait ResponseEgressObserver: Send + Sync + 'static {
    fn complete(&self, report: &ResponseEgressReport);
}
```

`bytes_written` is payload bytes successfully handed across the strongest guest-visible
platform write boundary. It is not TCP bytes, does not include headers/framing, and does not
claim peer acknowledgement. Checked addition is mandatory; arithmetic overflow terminates
as `TransportError`. `elapsed` is terminal monotonic time minus `egress_started_at`. If an
injected test clock moves backward, report zero, classify `Unspecified`, log the clock fault,
and still complete exactly once.

The observer must not panic and cannot affect the client response. Adapter code wraps the
terminal call in the project's platform-appropriate panic/error containment where available;
observer failure is logged after the lifecycle state is terminal and never triggers a second
notification.

`HostHandoff` is the honest terminal outcome when guest code can observe only that the
platform accepted a response object/body, not delivery or finish. It still closes the guest
attempt and triggers exactly one report, but it is not `Completed` and does not satisfy a
Native `response-egress-completion` requirement.

`ResponseReturned` is weaker: guest code constructed and returned the platform response,
but the host-owned send begins later or is otherwise unobservable. It reports
`bytes_written = 0`, does not imply host acceptance, and cannot satisfy any of the four
response-egress capabilities. Fastly's `#[fastly::main]` and Spin's `#[http_service]`
entrypoint wrappers are in this category because delivery occurs after the generated guest
function returns.

### 2.3 Exactly-once state machine

Each conversion owns one public, non-clone, adapter-facing completion guard. The type may be
`#[doc(hidden)]`, but it cannot be crate-private because adapter crates own and drive it:

```text
Initial -> Writing -> Completed
Initial -> Terminal(failure)
Writing -> Terminal(failure)
```

The terminal transition stores one `ResponseEgressReport` before invoking the observer.
EOF/finish, source error, transport error, deadline, explicit cancellation, client
disconnect, converter failure, and guard drop all compete through that one transition.
Later terminal signals are ignored. Dropping an `Initial` guard is `ConversionError`;
dropping a `Writing` guard before successful finish is `ClientDisconnected` when the host
reported cancellation/disconnect, otherwise `TransportError`. A guard disarmed by successful
completion performs no work in `Drop`.

One report is required for empty and `Body::Once` responses too. A converter failure before
the platform accepts response headers may synthesize the adapter's minimal 500 response, but
the original attempt reports `ConversionError`. The fallback must use an adapter-private
minimal platform response and must not recursively enter the guarded converter or create a
second observed attempt.

## 3. Write and abort semantics

### 3.1 Commit boundary

Each adapter documents its response-header commit point. Before commit, a source/conversion
error or already-expired deadline may be replaced by a typed 500/504 response. After commit,
status and headers are immutable: the adapter aborts/resets/closes the response using the
strongest platform primitive and reports the terminal cause. It must not append an error body
to a partially written success response.

Deadline expiry maps to a 504 only when no response bytes/headers have committed. The 504 is
constructed through an adapter-private minimal fallback and handed to the host without
starting a second observed attempt or recursively applying another write deadline. The
original attempt reports `DeadlineExceeded` exactly once. If that immediate fallback cannot
be constructed/accepted, the adapter aborts; it does not replace the original report with a
second terminal cause.

### 3.2 Backpressure and memory

For streaming adapters, platform demand drives source polling. At most one source chunk plus
documented platform staging may be retained by the EdgeZero wrapper. A source is not polled
again until the previous chunk is accepted or released. `Pending` must arrange a wake for
both platform readiness and deadline expiry; a frozen application clock or a source that
never wakes cannot suppress the adapter's independent timer wake.

`Body::Once` remains one already-materialized core allocation. This design does not impose a
new response-size cap; the producer's existing body limits remain responsible for that
allocation. An adapter that must buffer `Body::Stream` cannot claim streaming backpressure
and must enforce a separate finite converter collection cap before allocation grows.

### 3.3 Cancellation and finish

On deadline, disconnect, source error, or write error, the adapter:

1. stops polling the core body;
2. drops/cancels the source stream;
3. aborts, resets, or closes the platform response with its strongest documented primitive;
4. releases current/staged chunks; and
5. transitions the guard exactly once.

`Completed` means core EOF (and trailers if support is later added) was accepted by the
strongest guest-visible platform finish boundary. It does not mean the remote peer received
or acknowledged every byte. A target that cannot observe finish cannot claim Native
completion notification.

## 4. Adapter requirements and capabilities

The shared capability ladder gains four response-egress cells:

| Capability | Axum | Cloudflare | Fastly | Spin |
| --- | --- | --- | --- | --- |
| `response-egress-abort` | Unsupported until connection-level reset/drop tests pass | Unsupported until deployed cancel is observed | Unsupported; guest returns before host delivery | Unsupported; guest returns before host delivery |
| `response-egress-backpressure` | Unsupported until the non-`Send` bridge and socket-pressure tests pass | Unsupported until deployed pull/cancel behavior is observed | Unsupported while converter collects the stream | Unsupported while converter collects into `FullBody` |
| `response-egress-completion` | Unsupported until a proved transport completion boundary exists | Unsupported until a deployed finish/cancel probe passes | Unsupported | Unsupported |
| `response-write-deadlines` | Unsupported until a connection-level abort timer is proved | Unsupported until a deployed timer/abort probe passes | Unsupported | Unsupported |

These are the initial capability declarations. A cell may move to BestEffort or Native only
after its implementation and named evidence land. An app requiring Native support fails during build/serve/deploy/demo before
handling traffic. A target may expose a typed report at BestEffort while clearly documenting
that an unobservable host send can outlive guest completion.

### 4.1 Axum

The current Axum service emits `ResponseReturned` with zero written bytes after
`into_axum_response` finishes but before the converted response is returned to Hyper. This is
converter completion only: it does not observe Hyper acceptance, socket transmission, client
receipt, disconnect, or abort. It therefore provides no evidence for any of the four
response-egress capability cells, which remain `Unsupported`.

Future Axum certification work must account for core's non-`Send` `Body::Stream` while Axum's
erased response body requires `Send`; a local-executor/channel bridge is therefore required
before collection can be removed. A timer inside `poll_frame` observes Hyper demand, not socket
acceptance or flush, and cannot alone prove a response-write deadline. Native
deadline/abort/completion claims require connection-level ownership that can reset or close the
connection independently when the absolute deadline fires. That implementation must retain
the guard from header conversion until the connection-owned path accepts responsibility.
Raw-socket tests must cover a slow reader, disconnect before first byte, disconnect mid-body,
source error, deadline while Hyper holds a frame without repolling, empty body, and normal EOF.

### 4.2 Cloudflare

Wrap the worker stream rather than merely mapping chunks. The wrapper uses one independent
platform timer plus cooperative yielding, implements stream cancellation, drops the Rust
source on cancel/expiry, and completes once. Because local JavaScript/WASM clocks and mocks
may freeze unless the event loop yields, tests must include a frozen-clock fixture and the
wrapper must yield cooperatively before rechecking time. One deployed probe records
first-byte, midstream backpressure, deadline, cancel, and finish timing before any Native
claim.

### 4.3 Fastly and Spin

Their current converters materialize a body before returning it to the host and cannot
observe client delivery/finish. They enforce a finite converter collection cap and can report
pre-return source/conversion errors. Successful platform response construction reports
`ResponseReturned` exactly once with zero written bytes, but they remain Unsupported for the
four lifecycle capabilities. It must not be reported as `HostHandoff`: the generated host
entrypoint performs delivery only after guest dispatch returns.
Cooperative deadline checks around synchronous host work do not create a finite write bound.
Upgrading requires a documented host streaming/abort/finish API plus a deployed probe; local
collection tests are insufficient.

## 5. Test and evidence matrix

| Surface | Required proof |
| --- | --- |
| Timing | One absolute deadline covers conversion, first byte, inter-chunk waits, backpressure, and finish. Equality expiry wins; no per-chunk reset. |
| Completion | Empty, buffered, streamed EOF, source failure, transport failure, timeout, disconnect, conversion failure, and guard drop each notify exactly once. Competing terminal signals still produce one report. |
| Accounting | Exact payload byte totals, zero-byte response, checked overflow, and no header/framing bytes included. |
| Commit | Pre-commit failures may synthesize an error response; post-commit failures abort and never rewrite status or append an error body. |
| Backpressure | A scripted sink holding one chunk prevents a second source poll and keeps retained guest memory to one chunk. |
| Teardown | Source drop and native abort/reset/close are observed for every failure path supported by the adapter. |
| Capability | Parse/display/round-trip and fail-closed Native requirements match the four-row matrix. |
| Cloudflare timing | Frozen-clock test, cooperative yield, and one deployed timing/cancel/finish artifact. |
| Unsupported hosts | Fastly/Spin tests prove only bounded pre-return collection and never label response-object creation as client completion. |

## 6. Security and observability

Observer reports contain no body bytes, header values, target address, or unbounded error
strings. Route metadata uses the registered pattern, never dynamic path values. Platform
logs may include a bounded static outcome label and byte count; source error details remain
in local diagnostic logs subject to existing redaction rules.

No adapter may claim that an egress deadline bounds bytes buffered by the provider before
guest entry, kernel/socket buffers after guest handoff, or peer acknowledgement. Capability
documentation lists those exclusions explicitly.
