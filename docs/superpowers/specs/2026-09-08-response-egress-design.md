# EdgeZero Response Egress Design

> **Status:** Approved implementation contract for provider-owned response lifetimes.
> **Owner:** Core response lifecycle plus adapter-owned transport boundaries.
> **Migration:** Atomic hard cut. No buffered compatibility path is retained.

## 1. Scope and boundary

This design owns the lifetime after router dispatch returns a core `Response` and before the
adapter has either reached its strongest observable completion boundary or terminally aborted
the response. It covers one absolute response-write deadline enforced to the adapter's
declared capability boundary, demand-driven body production, client cancellation where
observable, post-commit abort behavior, payload-byte accounting, one terminal report, one
response-scoped completion slot, and one global observer notification.

It does not extend an outbound fetch deadline, classify upstream failures, or change outbound
response decoding. Those end when the outbound adapter returns an `OutboundResponse`, as
defined by the [outbound HTTP design](2026-05-21-outbound-http-design.md). Inbound request
admission and body-read deadlines are owned by the
[inbound-body design](2026-08-22-inbound-body-design.md).

Provider-owned buffers, kernel buffers, TCP acknowledgement, peer receipt, and complete
provider/runtime allocation accounting remain outside this contract. Every capability claim
names its exact observable boundary and must not imply one of those excluded guarantees.

This design replaced the historical converter-owned response lifetime, where Axum, Fastly,
and Spin materialized `Body::Stream` and Cloudflare reported terminal state before its actual
JavaScript writer boundary. The current adapters use the owned coordinators in §5. Returning a
response object is not an egress completion event.

## 2. Design choice

Each adapter owns a runtime-specific body pump and transport lifetime. Core supplies the
portable clock, policy, immutable response metadata, and exactly-once attempt guard. There is
no cross-adapter writer or sink trait: Hyper polling, Workers streams, synchronous Fastly
handles, and WASI stream writers have materially different progress and cancellation models.

Merely replacing collection with each SDK's convenient stream wrapper is insufficient. Such
wrappers hide one or more of deadline wakeup, accepted-byte accounting, consumer cancellation,
finish, or abort. The standard EdgeZero entrypoint must retain ownership through the strongest
boundary the provider exposes.

One adapter-local coordinator exclusively owns each `ResponseEgressAttempt`. Bodies, timers,
request signals, connection supervisors, and platform writers submit events to that owner;
they do not own independent attempt guards. The coordinator serializes competing terminal
events and releases all source and transport resources after the first terminal transition.

Every final client response produced by an admission decision uses the owned transport path,
including successful handlers, handler errors, framework 404/405 responses, bounded fallback
responses, and admission refusals. Errors that arise after `PreparedIngress` exists but before
normal dispatch completes consume that proof through `App::admitted_error_egress`; the envelope
retains the app clock, request metadata, policy, observer, response-scoped completion, deadline,
and exactly-once terminal semantics.
An adapter must not turn source construction, core request conversion, or dispatch failure into a
provider error or detached response after this boundary.

An admission refusal uses detached ingress policy and a no-op global observer, but carries the
response-scoped completion supplied by that refusal. Normalized head-validation failures and
admission-policy errors invoke the application's detached-egress decision factory. The default
factory returns:

```rust
DetachedResponseEgressDecision::Send {
    completion: ResponseEgressCompletion::empty(),
    deadline: head.write_deadline_after(DEFAULT_RESPONSE_WRITE_BUDGET),
}
```

The factory receives only bounded failure metadata and runs exactly once before any envelope is
created; it does not run middleware, the handler, the application egress policy, or the global
observer.
`Send { completion, deadline }` uses the same detached transport path with that completion and a
mandatory absolute deadline in the request's injected monotonic-clock domain. It cannot extend the
default response-egress policy because `begin()` enforces the earlier bound. `Abort` creates no
response, envelope, attempt, completion callback, observer call, handler call, middleware call, or
body poll and reaches the adapter's existing non-response abort/error boundary. Normalized
validation invokes the factory before routing and admission; a policy error invokes it after route
resolution and the failing policy callback. Admission-selected detached responses, including
refusals, retain their existing response-owned deadline path and are outside this factory contract.
`AdmissionDecision::Abort` produces no response, starts no egress attempt, and invokes neither
completion nor observer. Host/parser-generated responses and platform-to-core head-conversion
failures emitted before EdgeZero can construct a normalized head remain outside this lifecycle,
even when request start was already sampled. They are governed by the raw-ingress and adapter
conversion contracts.

## 3. Core contract

### 3.1 Policy and timing

`ResponseEgressEnvelope` retains the exact `App::monotonic_clock()` clone used for ingress and
standard outbound dispatch. `begin()` snapshots `egress_started_at` from that clock immediately
before its first response-conversion operation and returns the same clock with the response,
policy, and attempt. The adapter obtains one finite absolute deadline:

```rust
pub const DEFAULT_RESPONSE_WRITE_BUDGET: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub struct ResponseEgressDeadline { /* private */ }

impl ResponseEgressDeadline {
    pub const fn after(duration: Duration) -> Self;
    pub const fn at(deadline: Deadline) -> Self;
}

#[derive(Clone, Copy, Debug)]
pub struct ResponseEgressPolicy {
    pub write_deadline: Deadline,
}
```

An application inserts one `ResponseEgressDeadline` into the returned response extensions.
`ResponseEgressDeadline::at` preserves an already-computed absolute cutoff in the application's
`MonotonicClock` domain, including a queue-through-egress budget.
`ResponseEgressDeadline::after` selects an egress-start-relative deadline without requiring a
response category in the global policy callback. `begin()` samples `egress_started_at` once from
the envelope's injected clock, removes the extension, and resolves `after(duration)` from that
exact sample before invoking the callback. The duration is clamped to `DEADLINE_FAR_FUTURE`, and
checked-add overflow fails closed to `egress_started_at`. `begin()` exposes the resolved absolute
value through `ResponseEgressHead::application_deadline()` and enforces the earlier of it and the
callback-selected `write_deadline`. The retired ambiguous `new()` constructor and unresolved
`deadline()` accessor do not exist.

Every normalized-error `DetachedResponseEgressDecision::Send` carries such an absolute deadline
as a mandatory named field. `App` inserts it into the existing `ResponseEgressDeadline` extension
path before constructing detached egress. `begin()` enforces the earlier of the factory deadline
and the default response-egress policy, so neither can extend the other.
`DetachedResponseEgressHead::write_deadline_after`
constructs a deadline in the captured request's injected monotonic-clock domain by anchoring the
duration to `request_start`, clamping it to `DEADLINE_FAR_FUTURE`, and failing closed to
`request_start` if checked addition overflows. The default factory supplies an empty completion and
`DEFAULT_RESPONSE_WRITE_BUDGET` measured from request start. Admission-selected detached responses
retain the response-owned deadline path described above and do not use the normalized-error
factory contract.

`App` owns a synchronous response-egress policy callback configured through
`Hooks::configure`. The callback receives immutable response-head metadata plus the captured
request start, optional route metadata, and optional application deadline; it cannot inspect,
consume, or mutate the body.
The portable default is
`Deadline::at_instant(egress_started_at + DEFAULT_RESPONSE_WRITE_BUDGET)`.

The adapter normalizes every callback result to no later than
`egress_started_at + DEADLINE_FAR_FUTURE` using checked addition. An already-expired result is
a valid immediate deadline. Arithmetic overflow while deriving the clamp fails before commit
as `ConversionError`; it does not create an unbounded write. It then minimum-clamps that result
against the optional response-carried deadline. Neither path can extend an application deadline.

The deadline measures total response-egress time. It is never reset by response conversion,
first-byte wait, source readiness, backpressure, chunk boundaries, flushes, close, finish, or
trailers. Equality is expired. Before each operation and immediately after that operation
becomes ready, expiry wins over simultaneous progress.

If a native operation reports accepted bytes at or after the deadline, the coordinator first
accounts that accepted prefix and then terminalizes as `DeadlineExceeded`. The prefix is never
retried. Deadline precedence controls the terminal cause, not whether completed native progress
is recorded.

Every application timestamp and terminal report uses the envelope's injected monotonic clock.
An adapter declaring Native deadline support arms a provider timer from the remaining duration
in that same clock domain. Once that timer wakes, expiry is authoritative even if a frozen test
clock has not advanced; the adapter uses the deadline instant as the minimum terminal timestamp.
That timer must not depend on another source poll or write-ready notification to wake. An
adapter unable to preempt a provider operation checks before and after each operation,
documents the non-preemptible interval, and declares `BestEffort`.

### 3.2 Terminal report

The app may install one observer. The adapter invokes it synchronously exactly once per
response-egress attempt:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressOutcome {
    ClientDisconnected,
    Completed,
    ConversionError,
    DeadlineExceeded,
    HostHandoff,
    RequestCancelled,
    SourceError,
    TransportError,
    Unspecified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressBodyKind {
    Application,
    Fallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseEgressFallbackDisposition {
    Aborted,
    Completed,
}

#[derive(Clone, Debug)]
pub struct ResponseEgressReport {
    pub bytes_written: u64,
    pub body_kind: ResponseEgressBodyKind,
    pub elapsed: Duration,
    pub fallback_disposition: Option<ResponseEgressFallbackDisposition>,
    pub outcome: ResponseEgressOutcome,
    pub request_start: MonotonicInstant,
    pub route: Option<RouteMetadata>,
}

pub struct ResponseEgressCompletion { /* non-clone callback owner */ }

pub struct DetachedResponseEgressHead<'head> {
    /* request method, request start, status, and stable error kind */
}

#[non_exhaustive]
pub enum DetachedResponseEgressDecision {
    Abort,
    Send {
        completion: ResponseEgressCompletion,
        deadline: Deadline,
    },
}

impl DetachedResponseEgressHead<'_> {
    pub fn write_deadline_after(&self, duration: Duration) -> Deadline;
}

impl ResponseEgressCompletion {
    pub fn empty() -> Self;

    pub fn join(self, other: Self) -> Self;

    pub fn late_bound<T>() -> (ResponseEgressResource<T>, Self)
    where
        T: Send + 'static;

    pub fn new<Complete>(complete: Complete) -> Self
    where
        Complete: FnOnce(&ResponseEgressReport) + Send + 'static;
}

pub struct ResponseEgressResource<T> { /* private */ }

impl<T> ResponseEgressResource<T>
where
    T: Send + 'static,
{
    pub fn install(&self, resource: T) -> Result<(), ResponseEgressResourceInstallError>;
}

#[non_exhaustive]
pub enum ResponseEgressResourceInstallError {
    AlreadyInstalled,
    Closed,
}
```

An application that must retain a response-scoped resource until terminal egress constructs one
non-clone `ResponseEgressCompletion` whose `FnOnce` callback owns that resource. The completion is
not attached to a `Response` and never enters `http::Extensions`; it is a separate ownership token
carried by `AdmissionDecision`, `PreparedIngress`, `ResponseEgressEnvelope`, and finally the sole
`ResponseEgressAttempt`. Consequently response cloning, replacement, error rendering, and
framework-generated 404/405 responses cannot duplicate or silently detach the resource.

Every response-producing admission decision supplies exactly one completion. Applications that
do not need a resource use `ResponseEgressCompletion::empty()`. `Admit` and
`ReadBodyBeforeFallback` transfer it into `PreparedIngress`; `App::dispatch_admitted` removes it
before handing the remaining ingress state to the router and attaches it to whichever response
dispatch produces. That includes handler success, handler error, canonical 404/405, fallback exact
cap, fallback overflow, fallback timeout, and post-admission adapter failure. `Refuse` transfers its
completion directly into detached egress. Normalized pre-admission failures obtain one completion
when the application-configured detached-egress decision factory selects `Send`, or no response
when it selects `Abort`; every `Send` also supplies its mandatory absolute deadline, and the default
uses an empty completion plus `DEFAULT_RESPONSE_WRITE_BUDGET` from request start. Raw parser
failures for which EdgeZero cannot construct
`DetachedResponseEgressHead` remain outside this lifecycle and must not be counted as covered.

`ResponseEgressCompletion::join` consumes a left and right completion and returns one non-clone
owner. Its terminal callback invokes the left child first and the right child second, passing both
the same borrowed `ResponseEgressReport`; the joined owner executes at most once. If the joined
owner is abandoned before terminal egress, dropping it drops both children and releases both sets
of captured resources without fabricating a report. Each child is invoked through its own panic
boundary on unwind-capable targets, so a panic in the left callback is isolated and does not
suppress the right callback. On panic-abort targets, the process or instance may terminate before
the right callback runs. Composition does not strengthen the adapter's documented completion
boundary or any provider completion guarantee.

A resource may be acquired after admission. The application calls
`ResponseEgressCompletion::late_bound::<T>()` during admission, puts the returned non-clone
`ResponseEgressResource<T>` installer in `IngressGrant`, and transfers the paired completion into
the admission decision. The handler takes the typed grant and calls `install(&self, resource)`.
Installation consumes the resource. A duplicate returns `AlreadyInstalled`; installation after
terminal completion or pre-`begin()` abandonment returns `Closed`. Both failures drop the rejected
resource before returning and never return ownership to the caller. The typed install error
implements `std::error::Error` so application handlers can propagate it through their normal error
conversion without inspecting text.

The paired completion owns a private close guard. Terminal completion closes the slot and takes the
installed resource before releasing the internal mutex. Dropping the completion without invoking
its callback performs the same close-and-take operation, even if the installer remains alive. The
slot therefore rejects later installation and releases an installed resource on envelope
abandonment without fabricating a terminal report. The installer is not cloneable; the internal
state is `Send + Sync` for `T: Send` and does not require `T: Sync`. Capturing only a temporary
handler-local guard remains incorrect because it releases the permit before response transmission.
This late-binding pattern does not make the completion cloneable and does not put either owner in
response extensions.

`ResponseEgressEnvelope::begin()` moves the completion into the sole
`ResponseEgressAttempt` before policy evaluation or framing. The attempt retains it through
application writes, fallback writes, cancellation, adapter errors, and guard drop. The first
terminal transition stores the report in `AttemptState::Terminal`, takes the callback, and invokes
it exactly once. A later signal observes terminal state and cannot complete twice. If an envelope
is dropped before `begin()`, the completion and captured resource are dropped without fabricating
a terminal report. Standard adapters must call `begin()` exactly once for every response envelope.
`ResponseEgressEnvelope` exposes no `into_response()` escape hatch: obtaining a transmittable
response requires `begin()`, which first transfers the completion into the attempt. Dropping an
unbegun envelope is permitted only as cancellation before transmission; it releases the resource
without exposing the response or inventing a report.

The response-scoped completion runs before the global observer. Each completion child and the
observer are isolated by independent unwind boundaries: a completion panic is logged with a
category-only message and does not prevent a later joined child or the global observer from
receiving the report; an observer panic does not re-enter the completion callback. This isolation
exists only on unwind-capable targets. Panic-abort targets retain the trusted-callback limitation
below and may terminate before a later child or observer runs. No callback can alter the response
or terminal outcome.

`ResponseEgressBodyKind::{Application, Fallback}` identifies which bounded wire body the
reported byte count describes. Application bytes and fallback bytes are never mixed because
a fallback is permitted only before application headers commit. `fallback_disposition` is
`None` for the application body and records whether a selected fallback reached its finish
boundary or was aborted.

`bytes_written` is payload bytes accepted across the adapter's documented guest-visible
write boundary. It excludes headers and protocol framing and does not claim wire delivery.
Checked addition is mandatory; overflow terminalizes as `TransportError`. A partial native
write accounts only its accepted prefix and never retries that prefix.

`elapsed` is terminal monotonic time minus `egress_started_at`. If the injected clock moves
backward, the report records zero, classifies `Unspecified`, logs the clock fault, and still
completes exactly once.

Neither completion nor observer callbacks can alter the client response. EdgeZero emits only a
category-level message for a caught callback panic. Rust invokes the application-owned process
panic hook before `catch_unwind` returns, so panic-hook output is outside EdgeZero's
diagnostic-redaction guarantee; applications whose panic payloads may contain secrets must install
an appropriate redacting hook during startup. On panic-abort targets, including the pinned Fastly
WASM build, a policy panic occurs before terminal state and terminates the instance without a
report. A completion or observer panic occurs after terminal state is stored and any fallback has
settled, but terminates the instance before later callbacks can be guaranteed. Application
callbacks are trusted not to panic there; only unwind-capable targets can exercise the contained
panic and callback-ordering contract.

`Completed` means core EOF and any supported trailers reached the adapter's documented
successful finish boundary. `HostHandoff` means the provider accepted the complete
guest-visible stream but exposes no trustworthy transport-finish callback.
`ClientDisconnected` requires a provider signal proved to mean downstream response-consumer
loss. A request or task cancellation with weaker provenance is `RequestCancelled`. The old
converter-only `ResponseReturned` outcome is removed because no retained entrypoint ends its
lifecycle at response-object construction.

### 3.3 Exactly-once state machine

The portable guard remains public to adapters, non-clone, and `#[doc(hidden)]`:

```text
Initial -> Writing -> Terminal(Completed)
Initial -> Terminal(failure)
Writing -> Terminal(failure or HostHandoff)
```

The adapter-local coordinator may have more detailed states such as `HeadReady`, `Committed`,
`SourceEof`, and `Finishing`, but only that coordinator may mutate the core attempt. EOF,
finish, source error, transport error, deadline, explicit cancellation, disconnect, converter
failure, connection loss, and guard drop all compete through one terminal transition. Later
terminal events perform resource cleanup only and cannot replace the original cause.

Dropping an `Initial` guard reports `ConversionError`. Dropping a `Writing` guard reports
`ClientDisconnected` only when the provider supplied a stable disconnect/cancel signal;
otherwise it reports `TransportError`. Empty and `Body::Once` responses enter `Writing` and
produce one lifecycle report just like streamed responses.

The existing non-clone core attempt remains the only terminal guard and owns the mandatory
response-scoped completion slot; an empty slot is represented by
`ResponseEgressCompletion::empty()`, not `Option`. `begin()` changes so
policy panic/normalization failure returns the still-live attempt, clock, and failure cause to
the adapter instead of notifying before a fallback is written. The attempt records whether
application or fallback bytes are being written. Adapters may add private event/coordinator
types but must not add a shared runtime writer abstraction, make `BodyStream` require `Send`,
or attach provider capability declarations to individual reports.

### 3.4 Fallible application assembly

Application configuration is startup policy and must not require `expect` or panic to reject
invalid limits. The `Hooks` contract is a hard cut to a fallible builder:

```rust
pub trait Hooks {
    fn build_app() -> Result<App, EdgeError> {
        let mut app = App::with_name(Self::routes(), Self::name());
        Self::configure(&mut app)?;
        Ok(app)
    }

    fn configure(_app: &mut App) -> Result<(), EdgeError> {
        Ok(())
    }
}
```

The `app!` macro emits the same signatures. A supplied `configure = <expr>` callback must return
`Result<(), EdgeError>` and its result is returned directly rather than discarded. No adapter
continues with a partially configured `App`:

- Axum builds before binding its listener and returns an `anyhow::Error` retaining the
  `EdgeError` source.
- Cloudflare builds at the start of each host invocation, before core request conversion,
  admission, body polling, or dispatch. It logs only the stable `EdgeError::kind()` and returns a
  fixed `worker::Error::RustError("application configuration failed")` so private diagnostics are
  not wire-visible.
- Fastly builds after fixed runtime/logger setup but before receiving, converting, admitting, or
  dispatching the client request. Its top-level error retains the `EdgeError` source under the
  fixed `"application configuration failed"` context.
- Spin builds at the start of each host invocation before core request conversion, admission,
  body polling, or dispatch. Its `anyhow::Error` retains the `EdgeError` source under the same
  fixed context.

Cloudflare, Fastly, and Spin are per-invocation build boundaries, not process-start boundaries.
No compatibility `build_app_unchecked`, infallible callback coercion, or panic fallback remains.
The demo, generated template, hand-written `Hooks` implementations, guides, and tests migrate
atomically.

## 4. Write, commit, and abort semantics

### 4.1 Commit boundary

Each adapter documents its response-header commit operation. Before commit, conversion failure
or an expired deadline may be replaced by an adapter-private bounded 500 or 504 response. The
same coordinator and attempt retain transport ownership while writing that fallback. They
account fallback bytes, record `ResponseEgressBodyKind::Fallback`, and report the original
`ConversionError` or `DeadlineExceeded` cause only after the fallback reaches finish or abort.
The fallback does not create another observed attempt or invoke application policy recursively.

Because the application deadline may already be expired, the fixed fallback body uses a
separate internal one-second safety budget beginning when fallback conversion starts. That
budget is not an extension of the application response deadline. If fallback construction,
write, or finish fails, the adapter aborts and retains the original causal outcome; the report's
byte count, body kind, and fallback disposition still disclose the accepted fallback prefix and
its failed finish.

The fallback head passes through the same request-method and response-status framing
normalization as an application response. A `HEAD` fallback writes no payload, reports zero
bytes with `ResponseEgressBodyKind::Fallback`, and records completion or abort at the
platform's empty-body finish boundary.

After commit, status and headers are immutable. Deadline, source failure, cancellation, and
write failure drop the source and staged chunk, invoke the strongest platform abort/reset/error
primitive, and report the first terminal cause. No adapter appends an error payload to a
partially written response. A shared body/framing wrapper drops its source and any native reader
state before making a terminal `Err` item visible; cleanup never depends on the adapter polling
that wrapper again or dropping it after observing the error.

### 4.2 Backpressure and memory

Platform demand drives source polling. EdgeZero retains at most one source chunk plus explicitly
documented platform staging. A source is not polled again until the current chunk has been
fully accepted, partially accepted with its remainder retained, or released during teardown.

`Body::Once` remains one already-materialized core allocation. This design adds no response
size limit and removes the converter-only 16 MiB collection limits. Outbound `Buffered` mode
and its `collect_response_stream*` helpers remain because they implement a separate caller-
selected outbound fetch contract; response-egress converters no longer call them.

### 4.3 Cancellation safety

Dropping a future is not assumed to cancel a provider operation unless the pinned API contract
or a behavioral probe establishes that fact. An adapter must retain ownership of pending
operation state until cancellation is acknowledged or the containing response/connection is
destroyed. Partial progress reported during cancellation is accounted exactly once. A losing
operation never owns or directly mutates the attempt.

The earliest causal failure remains authoritative when abort/close also fails. Teardown errors
are logged with bounded category-only diagnostics and do not replace that outcome.

### 4.4 Response framing and body suppression

Owning a lower-level transport boundary also makes EdgeZero responsible for consistent final
response framing. Each adapter carries the request method into its private egress context,
validates the final response head before commit, and applies this precedence order:

1. Parse every `Content-Length` field and comma-separated member as an unsigned decimal `u64`
   after trimming optional whitespace. Empty, signed, overflowing, or otherwise invalid members
   fail as `ConversionError`; repeated members must all have the same value and normalize to one
   field.
2. Final 1xx responses and protocol upgrades are unsupported and fail before commit. A
   successful response to `CONNECT` is rejected because EdgeZero does not own a tunnel
   lifecycle.
3. Status-specific suppression runs before method-specific suppression. A `204` response drops
   the body and always removes `Content-Length`. A `205` response drops the body and rejects a
   nonzero declared length; explicit zero is permitted. A `304` response drops the body and may
   preserve an explicitly supplied representation length.
4. For every other status, a `HEAD` response drops the body without polling it and may preserve
   an explicitly supplied representation `Content-Length`; that value is not compared with the
   unsent body.
5. Only responses whose payload will be transmitted enter body-length validation.

After suppression is resolved, every adapter applies these common field rules:

- EdgeZero does not forward an application-supplied `Transfer-Encoding`; the selected platform
  transport owns transfer framing.
- For a response whose payload will be transmitted, `Content-Length` on `Body::Once` must equal
  the exact body length before commit. A mismatch is `ConversionError` and produces the bounded
  pre-commit fallback.
- A streamed body with `Content-Length` is checked incrementally. Exceeding the declared length
  aborts immediately; early EOF aborts instead of presenting a clean truncated response.
- A streamed body without `Content-Length` uses the provider's native streaming framing.
- Core `Body` has no trailer variant. An application-supplied `Trailer` declaration is therefore
  unsupported and fails before commit. Future trailer support requires an explicit declared
  trailer shape in the core response contract.

Repeated end-to-end fields are appended rather than replaced. Every adapter that receives an
application `Connection` field parses its comma-separated tokens and removes every nominated
field in addition to the standard hop-by-hop fields and the de facto `Proxy-Connection` field
before provider conversion. A provider
that rejects or hides `Connection` earlier documents that equivalent boundary. Invalid
connection tokens fail before commit as `ConversionError`. These rules are shared semantics
implemented separately with each provider's structured header API; adapters do not serialize
raw header blocks.

## 5. Adapter designs

### 5.1 Axum: owned Hyper HTTP/1 boundary

The standard Axum adapter stops returning an erased `axum::body::Body`. It runs an EdgeZero-
owned Hyper HTTP/1 accept/connection loop on a Tokio `LocalSet`, allowing the portable
non-`Send` core stream to remain on one local executor. A private Hyper service converts
`Request<Incoming>` into the existing core request pipeline and returns a private response
body implementing `http_body::Body` directly.

The adapter owns two coordinated scopes:

1. A per-response coordinator owns the core stream, current chunk, deadline, byte accounting,
   EOF/finish state, and core attempt.
2. A per-connection supervisor owns the Hyper connection future, socket wrapper, and an ordered
   registry of response coordinator handles. Each entry records identity, deadline, commit
   state, and EOF/flush marker. The supervisor can terminate the connection while Hyper is
   blocked below `poll_frame` flushing bytes to the socket.

The body accounts a payload frame when Hyper accepts it from `poll_frame`. It reports source
EOF to the connection supervisor. Completion is eligible only after a subsequent proved Hyper
framing/flush boundary has accepted all response bytes into the OS socket. It never means peer
receipt. A raw-socket test must prove that the selected boundary occurs once per response;
until then the adapter reports `HostHandoff` and completion remains `BestEffort`.

HTTP/1 post-commit timeout or source failure closes the connection to preserve framing. This
may cancel pipelined responses on that connection. The supervisor never mutates a core attempt;
it submits an ordered connection event to each registered coordinator. The coordinator whose
deadline caused closure retains `DeadlineExceeded`; other committed or queued coordinators
that have no earlier terminal cause settle as `TransportError`. Already-terminal coordinators
ignore the event. Hyper does not currently enable HTTP/2 in this adapter. Future HTTP/2 support
requires per-stream reset and completion ownership; a connection-wide abort is not acceptable
for multiplexed streams.

The public transport-blind `EdgeZeroAxumService` embedding path is removed. Public serving APIs
must accept or create a listener and retain EdgeZero connection ownership. There is no
buffered compatibility service.

### 5.2 Cloudflare: owned Workers stream writer

Cloudflare retains the standard `#[event(fetch)]` entrypoint but replaces the generic
`Response::from_stream` lifecycle wrapper with an adapter-owned Workers stream bridge.
The request's `AbortSignal` is captured before the request body is moved, and generated plus
demo Wrangler manifests enable `enable_request_signal`.

For a transmitted response with declared `Content-Length`, including `Body::Stream`, the
adapter owns both sides of a native fixed-length stream so Cloudflare preserves and enforces
that length. A known `Body::Once` without an explicit length may use its exact length. An
unknown-length stream uses a native transform stream. The adapter retains the writable side
and returns the readable side in the response. A task registered with the request `Context`
owns the body source, writer, request signal, clock, deadline timer, and attempt.

The writer waits for platform demand, writes one chunk, and accounts the chunk only after the
write promise resolves. It does not poll another source item while a write is pending. The
same task always polls an independent deadline timer, so no downstream pull is required for
expiry. On clean EOF it closes the writer before reporting `HostHandoff`; Workers exposes no
socket-flush or response-pump completion callback.

Request abort and readable-side cancellation are complementary signals. Until deployed
evidence proves that `Request.signal` means downstream response-consumer loss for a specific
topology, it is `RequestCancelled`, not `ClientDisconnected`. A generic write rejection without
that signal is `TransportError`. Deadline or cancellation aborts the writable stream, drops
pending source state, and terminalizes exactly once. A losing write promise must not remain
able to account bytes or report completion after abort.

High-water-mark behavior, fixed-length semantics, request-signal behavior, and runtime
cancellation require deployed Workers evidence before promotion to `Native`.

### 5.3 Fastly: manual send-owning entrypoint

Fastly removes `#[fastly::main]` from generated and demo applications. EdgeZero initializes
the SDK, acquires the client request, dispatches the app, converts the response head, commits
it, pumps the body, and finishes or abandons the native response before returning:

```rust
fn main() -> Result<(), fastly::Error> {
    edgezero_adapter_fastly::run_app::<App>()
}
```

All public dispatch variants become send-owning terminal operations. No public
response-returning converter remains. The adapter constructs a low-level `ResponseHandle` and
commits with `ResponseHandle::stream_to_client(BodyHandle::new())`, obtaining an unbuffered
`StreamingBodyHandle`. It does not use the high-level `StreamingBody`, whose `BufWriter` may
flush buffered data while attempting to finish or abandon.

Each chunk is fully written before the next source poll. Native short writes account their
accepted prefix and retain only the unwritten suffix. Zero-length progress is a
`TransportError`. Clean EOF invokes `finish`; `finish` returning success reports `Completed`
at the precisely documented boundary "Fastly host accepted body-handle close." It does not
claim that background transmission or peer receipt completed. Source/deadline failure invokes
`abandon` or drops the handle so its native drop guard abandons it. Generic native write errors
are `TransportError` because the SDK exposes no stable client-disconnect reason.

Source polling and native writes are synchronous and cannot be preempted by an independent
timer. The adapter checks the absolute deadline before and after each source poll, write, and
finish operation. Response-write deadlines therefore have a `BestEffort` ceiling with the
pinned Fastly API. `StreamingBodyHandle::finish` consumes Fastly's only body handle before it
reports success or failure. A finish failure is therefore terminal and cannot be followed by a
second explicit `abandon`; EdgeZero preserves the finish failure as the first cause, reports it
exactly once, and relies on the consumed handle's host-side close semantics. This limitation is
part of Fastly's `BestEffort` abort and completion classifications.

### 5.4 Spin: raw WASI response and owned writer task

Spin retains the official `#[http_service]` entrypoint. Its macro accepts a raw WASIp3
response, so replacing the export mechanism would duplicate version-sensitive SDK machinery
without improving lifecycle ownership.

`run_app`, `AppExt`, and request dispatch return the concrete raw response type re-exported by
`spin_sdk::wasip3`. The adapter bypasses `http_into_wasi_response`, constructs `BodyWriter`
and `Response::new` itself, and spawns one SDK-owned task. The task owns the core body,
`StreamWriter`, result writer, response-result reader, clock, deadline, and attempt for their
entire lifetime.

The task races source polling and each individual `StreamWriter::write` against the absolute
deadline. It does not use `write_all` or `send_http_body`, because the lower-level write result
is required to account a transferred prefix during cancellation. Clean EOF closes the stream,
writes the successful body result, and then observes the response-result future. Failure or
deadline drops source state and publishes one bounded WASI error result; it never appends a
post-commit error body.

On deadline, the task retains the pending write future and writer until cancellation is
acknowledged or destroying the response stream makes further progress impossible. Any prefix
reported during cancellation is accounted before cleanup. The pending operation emits events
to the coordinator and cannot access the attempt directly.

The current Spin host does not prove that the response-result future corresponds to network
completion. Until host source or deployed evidence proves that boundary, successful guest
termination reports `HostHandoff`, and completion remains `BestEffort`.

All raw types and task spawning come through the pinned `spin_sdk::wasip3` re-export. A
separate direct `wasip3` dependency is removed if no other live code requires it.

## 6. Hard-cut migration and deletion

The implementation is atomic. It removes rather than deprecates:

- `AXUM_RESPONSE_STREAM_BUFFER_BYTES`, `FASTLY_RESPONSE_STREAM_BUFFER_BYTES`, and
  `SPIN_RESPONSE_STREAM_BUFFER_BYTES`;
- Axum's collecting response converter and transport-blind public service path;
- Fastly's response-returning `run_app`, extension/config variants, service dispatch,
  registry dispatch, public `from_core_response`, and `#[fastly::main]` scaffolding;
- Spin's `SpinFullResponse`, `FullBody` converter, public `from_core_response`, and matching
  `AppExt`/dispatch signatures;
- Cloudflare's generic response-egress stream wrapper and all premature
  pre-enqueue accounting branches;
- the converter-only `ResponseEgressOutcome::ResponseReturned` variant and its zero-byte special
  case;
- public `ResponseEgressEnvelope::into_response()` and every test/helper that extracts a response
  without beginning and terminally settling its attempt;
- any response-extension-based completion carrier or cloneable shared callback slot; completion is
  a direct, non-clone admission/envelope/attempt ownership chain;
- infallible `Hooks::configure` and `Hooks::build_app` signatures, callback-result discards, and
  application-side `expect` calls used only to bridge fallible startup policy;
- obsolete tests, fixtures, imports, dependencies, capability footnotes, and adapter-guide
  text that describe buffered egress; and
- the superseded Fastly-only response-egress plan.

The generic `collect_response_stream*` functions remain only where live outbound buffered
fetch APIs use them. Historical design and completed plan documents remain records; current
specifications and guides identify this hard cut as the active contract.

The demo adapters, adapter-owned Handlebars templates, generator assertions/snapshots,
generated-project compile fixtures, API documentation, capability matrices, and legacy API
contract script change in the same patch series. A repository check rejects production,
template, example, and current-guide references to removed symbols or buffering constants.

## 7. Capability policy

`Native` means implementation plus target-appropriate evidence demonstrates the exact stated
boundary without a documented behavioral deviation. Deterministic unit tests prove EdgeZero
mechanics; they do not by themselves prove provider behavior. `BestEffort` means the lifecycle
is implemented but a provider boundary is weaker or not yet proved. An app requiring a Native
capability rejects both `BestEffort` and `Unsupported` before serving traffic.

The implemented declarations are conservative:

| Capability                           | Axum       | Cloudflare | Fastly     | Spin       |
| ------------------------------------ | ---------- | ---------- | ---------- | ---------- |
| `response-egress-abort`              | BestEffort | BestEffort | BestEffort | BestEffort |
| `response-egress-backpressure`       | BestEffort | BestEffort | BestEffort | BestEffort |
| `response-egress-completion`         | BestEffort | BestEffort | BestEffort | BestEffort |
| `response-write-deadlines`           | BestEffort | BestEffort | BestEffort | BestEffort |
| `lazy-streamed-response-passthrough` | Native     | Native     | BestEffort | BestEffort |

Axum raw-socket tests prove deadline wakeup and deterministic collateral pipeline attribution,
but Hyper frame acceptance is not per-response socket or client completion. Cloudflare writer
acceptance and close lack deployed disconnect/completion proof. Fastly cannot preempt synchronous
source polls or hostcalls. Spin cancellation does not prove finite host teardown. These are the
reasons the four egress rows remain `BestEffort`; implementation intent alone would not qualify.

## 8. Test and evidence matrix

Every adapter gets deterministic lifecycle tests before provider integration:

| Surface      | Required proof                                                                                                                                                                                                     |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Timing       | One resolved absolute deadline covers conversion, first byte, source waits, writes, backpressure, and finish. Equality expiry wins; no chunk resets the budget. Frozen/backward clocks still settle exactly once. A response-owned `at` cutoff remains absolute; `after` is resolved exactly once from the injected `egress_started_at` sample, clamps to `DEADLINE_FAR_FUTURE`, and fails closed to that sample on overflow. Every normalized-error `Send` supplies a mandatory absolute deadline in the request's injected clock domain; its relative construction anchors to `request_start`, clamps to `DEADLINE_FAR_FUTURE`, and fails closed to request start on overflow. The default uses `DEFAULT_RESPONSE_WRITE_BUDGET` from request start. The effective bound is the earlier of the application deadline and default egress policy, so neither can extend the other. Admission-selected detached responses retain their response-owned deadline path outside the factory contract. |
| Completion   | Empty, buffered, streamed EOF, source failure, transport failure, timeout, disconnect, conversion failure, never-polled body, and guard drop each notify once. Competing terminal events still produce one report. The exact decision-owned completion survives normal dispatch, handler/routing errors, canonical 404/405, bounded fallback outcomes, refusal, policy/framing failure, adapter fallback, and guard drop; it runs before the global observer. Joined completions receive the same borrowed report at most once in left-to-right order and release both resources if abandoned. Child panics remain independently isolated only on unwind-capable targets; panic-abort may terminate before the right callback. A dropped pre-begin envelope releases its resource without fabricating a report. Normalized pre-admission validation invokes the detached-egress decision factory exactly once: `Send { completion, deadline }` supplies exactly one completion and one mandatory deadline, with an empty completion by default, while `Abort` constructs no response or attempt and invokes no completion, observer, handler, middleware, or body poll. Raw parser failures remain outside the lifecycle contract. Explicit ingress abort likewise starts no attempt and invokes no callback. Compile-time tests prove the completion is non-clone and cannot be stored in response extensions. |
| Accounting   | Exact payload totals, zero-byte response, checked overflow, short/partial writes, cancellation prefixes, and no headers/framing in the count.                                                                      |
| Commit       | Pre-commit failure may synthesize a bounded fallback; post-commit failure aborts without rewriting status or appending a body.                                                                                     |
| Backpressure | Holding one platform write prevents the next source poll and bounds EdgeZero staging to one chunk.                                                                                                                 |
| Teardown     | Source drop and native abort/reset/error/close occur on every supported failure path. A terminal stream error releases its source/native state before yielding `Err`, without a repoll or wrapper drop. A losing future cannot mutate terminal state. |
| Headers      | Repeated fields, including `Set-Cookie`, survive the new low-level conversion paths.                                                                                                                               |
| Framing      | `HEAD`, body-forbidden statuses, content-length exact/short/long bodies, removed transfer encoding, unsupported upgrades, and early EOF behave identically across adapters.                                        |
| Migration    | Removed APIs fail to compile; generated and demo applications compile only against the new entrypoints, response types, and fallible configure signature. Every adapter proves a failing configure result prevents request conversion, body polling, admission, and dispatch. Axum additionally proves no listener bind and a retained `EdgeError` source. Fastly proves no client-request receive and a retained source under the fixed context. Spin proves the retained source and fixed context. Cloudflare proves stable-kind-only logging and the fixed public `worker::Error`. |

Target evidence adds:

- Axum `tokio::io::duplex` and raw TCP tests for no reader, slow reader, disconnect before
  first byte, disconnect mid-body, blocked-socket deadline, normal EOF, keep-alive,
  pipelining collateral cancellation, and per-response flush association.
- Cloudflare WASM tests for fixed/unknown-length streams, demand, writer rejection,
  readable cancellation, `Request.signal`, no-demand expiry, source release, and one terminal
  settlement. Deployed probes cover direct and service-binding topologies.
- Fastly scripted sink tests for short/zero writes, finish/abandon errors, source failure, and
  synchronous deadline crossings. Viceroy exercises the undecorated manual entrypoint.
- Spin scripted WASI writer tests for partial progress, cancelled writes, result-writer errors,
  response-result timing, source release, and exactly one exported HTTP handler.

Deployed probes record provider/runtime versions and the source revision. They separately
observe source polls, first-byte timing, slow-reader pressure, disconnect, pre-commit timeout,
post-commit timeout, normal finish, byte accounting, and terminal reports. CI may keep
credentialed probes protected, but capability documentation links the latest evidence used
for any `Native` promotion.

## 9. Security and observability

Observer reports contain no body bytes, header values, target addresses, or unbounded error
strings. Route metadata uses the registered pattern, never dynamic path values. Platform logs
may include a bounded static outcome label and byte count; provider/source details stay in
redacted diagnostic logs.

Fallback responses use fixed public messages and do not serialize provider errors. No adapter
claims that an egress deadline bounds provider buffers, kernel buffers, peer receipt, or total
isolate memory. Those exclusions remain explicit in the public capability guide.
