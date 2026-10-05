# Pre-dispatch interception and inbound request fidelity

**Status:** Approved for implementation after independent review; capability evidence and remaining gates are recorded in the matching plan.
**Date:** 2026-10-05.
**Baseline:** EdgeZero main `98930917d96cb665c7255f36ca8bbd61fc7539e1`.
**Branch:** `spec/pre-dispatch-request-fidelity`.
**Implementation plan:** [Matching plan](../plans/2026-10-05-pre-dispatch-request-fidelity.md).

## 1. Problem and outcome

Applications need to intercept a reserved namespace independently of the registered method/path routes, before router-owned state, introspection, request context and middleware run. A consumer must be able to authenticate a reserved request and return its own complete response even when its method has no registered handler.

At the planning baseline the router performs `find_route` first. Middleware therefore cannot reliably authenticate a reserved request before router-generated 404/405 errors. Adapter conversion also changes information that a strict consumer may need: Cloudflare reduces extension methods to GET and re-encodes header strings as UTF-8; some runtimes normalize URLs or combine repeated fields before EdgeZero sees them.

Provide one optional generic pre-dispatch hook, preserve runtime-visible method/target/header data without additional avoidable loss, and expose explicit fidelity information. Preserve the concrete `RouterService` and existing routes when no hook is installed. This is toolkit infrastructure, not an implementation of Trusted Server tracing.

## 2. Consumer requirements and references

The motivating consumer is approved [Trusted Server PR #1107](https://github.com/IABTechLab/trusted-server/pull/1107), whose [design at the planning baseline](https://github.com/IABTechLab/trusted-server/blob/20f4a0cc0139eac842d1d6ff1410a1af6ee8eba8/docs/superpowers/specs/2026-09-01-mobile-ad-render-trace-endpoint-design.md) defines:

- Section 8: authentication and local handling across reserved spellings and methods before ordinary application lifecycle work.
- Section 9.2: bounded cookie inspection, duplicate detection and non-UTF-8 rejection.
- Sections 12 and 14: hardened local responses and actual adapter ordering/conversion tests.

Local source: `/Users/prk-jr/Desktop/opensource/rust/trusted-server/docs/superpowers/specs/2026-09-01-mobile-ad-render-trace-endpoint-design.md`. Local consumer plan: `/Users/prk-jr/Desktop/opensource/rust/trusted-server/docs/superpowers/plans/2026-10-05-mobile-ad-render-trace-implementation-plan.md`, tasks F0/F4.

Primary interoperability references: [Fetch header sorting/combining](https://fetch.spec.whatwg.org/#concept-header-list-sort-and-combine), [URL parsing](https://url.spec.whatwg.org/#url-parsing), and [Web IDL ByteString](https://webidl.spec.whatwg.org/#idl-ByteString). These define platform semantics, not proof of a particular deployed runtime.

## 3. Scope and alternatives

Recommended: an optional hook on `RouterBuilder` plus adapter-populated ingress metadata. It keeps manual `Hooks::routes()` and all existing adapter runners on the same concrete router and provides a single method-independent interception point.

Alternatives considered:

1. Ordinary middleware or standard-method route registration: smaller apparent edit, but it runs after lookup and cannot cover arbitrary extension methods or route-generated errors. Rejected.
2. Separate wrappers/entry-point bypasses per platform: can intercept native requests but duplicates policy and does not fit existing concrete `RouterService` seams consistently. Consumer-native shortcuts still require native integration; they do not replace the router hook.
3. Replace routing/body/bootstrap wholesale: unnecessary compatibility risk. Existing buffering is explicitly accepted by consumer section 8; no lazy-body or store-initialization redesign is included.

Included: core hook, bounded ingress metadata, lossless runtime method/header conversion, adapter-specific target capture and fidelity descriptions, native/WASM/runtime contract tests, and docs.

Excluded: trace routes/cookies/schema/CSP/authentication policy, EC/EID processing, auction evidence, telemetry, browser UI, generic hook chains, macro grammar additions, forwarding-header trust, transport deadlines/size protection, deployment, and unrelated proxy/response refactors.

## 4. Verified baseline and limits

| Surface    | Current source                                                           | Consequence                                                                                                                                                            |
| ---------- | ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Router     | `crates/edgezero-core/src/router.rs`, `RouterInner::dispatch`            | Lookup precedes state/introspection/context/middleware; no interception hook exists.                                                                                   |
| Bootstrap  | `crates/edgezero-core/src/app.rs`; `crates/edgezero-macros/src/app.rs`   | `Hooks::routes`, `App` and generated routers use `RouterService`; retain that type.                                                                                    |
| Fastly     | `crates/edgezero-adapter-fastly/src/request.rs`, `into_core_request`     | SDK URL accessor has already parsed with `url::Url`. Runtime header bytes and same-name values pass through byte-oriented construction. Body is buffered.              |
| Cloudflare | `crates/edgezero-adapter-cloudflare/src/request.rs`, `into_core_request` | `req.method()` is a closed Workers enum; extension methods become GET. `req.url()` parses the runtime URL again. Header entries become Rust strings. Body is buffered. |
| Axum       | `crates/edgezero-adapter-axum/src/request.rs`                            | `Request::from_parts` preserves the incoming `http::Uri` and `HeaderMap`. Hyper may already reject/fold fields; JSON bodies are buffered.                              |
| Spin       | `crates/edgezero-adapter-spin/src/request.rs`                            | SDK request parts contain runtime URI and byte-oriented headers. Original wire interleaving is already unavailable in `HeaderMap`. Body is buffered.                   |
| Pins       | `Cargo.lock`                                                             | This checkout pins worker 0.8.3, Fastly 0.12.1 and wasm-bindgen 0.2.122; verify again during execution. Do not substitute the consumer's worker 0.8.5 pin.             |

Fastly SDK `RequestHandle::get_url` calls `uri_get` then `Url::parse`. Its request handle getter is private; the public consuming handle round trip rebuilds the request without original client provenance, changing IP/TLS/original-header accessors. It is not an acceptable raw-target workaround. A safe borrowed SDK accessor is an external prerequisite for an original-target claim.

Fetch `Headers` may combine/sort fields. Pinned workerd uses UTF-8 header strings, unlike browser ByteString construction; copying the runtime string cannot recover invalid UTF-8 bytes replaced earlier or missing field boundaries. Hyper's HTTP/1 parser can fold identical repeated Content-Length and reject conflicting lengths before application dispatch. Therefore `HeaderMap` fidelity is not a blanket wire-multiplicity guarantee.

Existing Fastly/Cloudflare/Spin buffering and Axum JSON buffering occur before the hook. The hook guarantees ordering relative to the router/application, not relative to network receipt, SDK parsing, body buffering, logger setup or capability-handle construction. Consumer section 8 explicitly accepts pre-buffering and offers no transport deadline. Do not claim that the hook removes those limits.

## 5. Core pre-dispatch API

Add `PreDispatchHook` and `BoxPreDispatchHook` in `crates/edgezero-core/src/router.rs`:

```rust
#[async_trait::async_trait(?Send)]
pub trait PreDispatchHook: Send + Sync + 'static {
    async fn handle(
        &self,
        request: &mut Request,
    ) -> Result<Option<Response>, EdgeError>;
}

pub type BoxPreDispatchHook = Arc<dyn PreDispatchHook>;
```

`Request`/`Response` are existing `edgezero_core::http` aliases; the hook future has no Send requirement. Use the existing async-trait dependency. New public items receive the repository's normal documentation.

Add consuming builder method `RouterBuilder::pre_dispatch_hook(hook: BoxPreDispatchHook) -> Self`. Store one `Option<BoxPreDispatchHook>` in builder/inner. Default is None. A repeated builder call replaces the previous hook; this is one hook, not a new middleware chain. `RouterService` clones share the same Arc-backed instance.

The first operation in `RouterInner::dispatch` becomes:

```rust
if let Some(hook) = &self.pre_dispatch_hook {
    if let Some(response) = hook.handle(&mut request).await? {
        return Ok(response);
    }
}
```

Only after continuation may the router clone the method/path, call `find_route`, insert introspection/state, build `RequestContext` or run middleware.

### Result contract

- `Ok(Some(response))`: terminal core response returned unchanged. No route matching, state/introspection injection, `RequestContext`, route middleware or handler follows.
- `Ok(None)`: continue using the same mutated request, including method, URI, headers, extensions and remaining body.
- `Err(error)`: direct `Service::call` propagates the existing `EdgeError`; existing `RouterService::oneshot` renders it through `IntoResponse`. Neither dispatches fallback routes or silently continues. Return `Some(Response)` when exact consumer-specific error headers/body are required.
- No hook: retain all current routing/status/middleware behavior.
- Request cancellation: follow the existing future/drop behavior; introduce no tasks, timers or background work.

The hook has no implicit router-state access because it precedes injection. Consumers capture their own `Arc<AppState>` when building the hook. They own authentication, method validation, complete local response headers and any diagnostic policy. Outer adapter/consumer finalizers still exist; a terminal core response is not a promise to bypass arbitrary external finalization.

Manual builder registration is sufficient; no new `Hooks` method or `app!` argument is required.

## 6. Ingress metadata contract

Create `crates/edgezero-core/src/request.rs` and export `pub mod request`. This module defines an adapter-populated `RequestIngress` request extension, not a second request implementation.

Use private fields, Clone, and borrowed accessors. Do not derive value-bearing Debug, Serialize or Display for target/origin/metadata. These values can contain credentials or query strings. Never log them, put them in error messages, serialize them into responses, or clone all header values into a second buffer.

### Public data model

- `CapturedTarget::Complete(CompleteTarget)`, or `Unavailable(TargetUnavailable)`. The public CompleteTarget payload has private value/source/fidelity fields and borrowed getters; only the bounded capture constructor can create it. Do not expose constructible enum fields that bypass the cap.
- `TargetSource`: `TransportRequestTarget`, `RuntimeUrl`, `RuntimePathAndQuery`. Source and fidelity are independent: a runtime URL is not automatically the original request target.
- `Preservation`: `Preserved`, `Transformed`, `Unknown`, `Unavailable`.
- `TargetUnavailable`: `NotExposed`, `TooLarge`, `ReadFailed`.
- `OriginSource`: `RuntimeUri` or `TransportBinding`; provenance is explicit. RuntimeUri records the runtime URI source; TransportBinding records a trusted transport scheme combined with a single validated inbound request authority. Request authority is not operator approval or host ownership. Invalid, multiple or contradictory authority stays unavailable.
- `InboundOrigin`: validated HTTP(S) scheme and authority, bounded to 1 KiB authority bytes, captured from trusted inbound/runtime request information with `OriginSource`. Getters borrow those values. No user-info/path/query/fragment; reject invalid authority or expose unavailable origin. An Origin request header or arbitrary forwarded header cannot populate it.
- `HeaderFidelity`: independent `Preservation` values for octets, field multiplicity, same-name value order and global field order.
- `RequestIngress`: captured target, optional inbound origin, common header fidelity and small per-header fidelity overrides. `header_fidelity(&HeaderName)` returns an override or the common default; this handles Content-Length folding without implying identical behavior for Cookie.

Choose these names and semantics for implementation; constructors/accessors may follow repository naming conventions without adding speculative APIs. The plan owns the exact minimum constructor/accessor set. Do not duplicate method metadata: `Request::method()` is authoritative after lossless conversion.

HeaderFidelity axes describe original received HTTP field-value octets, field multiplicity and ordering compared with the resulting core headers, not merely a successful runtime-to-core copy. Octets mean field-value bytes, not header-name case, delimiters, HTTP framing or complete request serialization. Trimming/combining or parser/runtime loss must be reflected as Transformed, Unknown or Unavailable; Preserved requires pinned runtime/parser evidence and actual transport tests. Document and test runtime-to-core copying separately when it is all that can be established.

Capture target strings only up to 16 KiB UTF-8 bytes. A larger or failed target capture stores Unavailable without a partial string. Raw octets that cannot be represented by the verified platform string API are unavailable, not replacement-decoded. This toolkit bound is a metadata allocation bound, not a server URI acceptance limit.

Populate metadata before EdgeZero's own URL parse/normalization where possible. It preserves what the runtime actually supplies. Report stronger wire guarantees only with direct transport evidence. Missing metadata means unknown capability; consumers cannot infer full preservation from its absence.

Do not turn optional target-capture failure into an unrelated new ordinary-request rejection. Retain the current conversion/error behavior for the primary core URI; record optional metadata failure separately. Invalid original method or invalid runtime HeaderValues are conversion errors, never coerced values.

## 7. Adapter requirements

### 7.1 Fastly

- Preserve `req.get_method()` and existing byte-oriented repeated header conversion.
- Add a reusable `capture_request_ingress(&fastly::Request) -> RequestIngress` helper in the Fastly request module. A consumer calls it before native health/debug shortcuts or mutations. Record the raw-target capture result before reading the SDK URL. With the current missing accessor, retain `NotExposed`, then recover canonical origin separately from the runtime URL plus exactly one valid, consistent Host on a native client request. URL normalization is acceptable for origin acquisition only; it never proves original pathname preservation. Use `RuntimeUri` provenance and do not infer HTTP from missing TLS metadata. Supplied snapshots remain caller assertions and are retained through subsequent mutation.
- With current SDK, mark original target unavailable or runtime-normalized/unknown; do not call `get_url_str` “raw”.
- Obtain a reviewed safe SDK accessor that borrows the original request, reads the runtime target before `Url::parse` and preserves client provenance. Pin the resolved compatible SDK revision/version; do not fabricate one or add a local Cargo-cache patch.
- Do not use consuming handle round trips, clone_without_body, raw pointers or a new unsafe fastly-sys wrapper to bypass the SDK gate.
- After the accessor exists, parse scheme/authority without normalizing the captured path. Prove encoded separators/dot segments/repeated separators survive in metadata and that client IP/TLS accessors retain behavior.
- Runtime header bytes and same-name order must be tested. Original-header names/count alone do not prove value-field multiplicity; do not reconstruct fields from mismatched/FIFO guesses.
- Add `into_core_request_with_ingress` and a public `dispatch_with_registries_and_ingress` overload that retains the existing App/StoresMetadata/EnvConfig/extension-callback semantics. Add a metadata-aware conversion path for an already captured snapshot: retain that same snapshot through later native mutation/dispatch. Ordinary converters capture once and delegate. Insert adapter-captured ingress after generic scratch extension merging so it cannot be accidentally overwritten.
- Existing request-extensions callbacks can carry native facts. They are not the new router policy hook and cannot return local responses.

### 7.2 Cloudflare

Use `req.inner().method()` and parse with core `Method::from_bytes`. Preserve any valid extension token exposed by the runtime; malformed values produce a bounded conversion error. Do not pass through Workers' closed method enum for inbound conversion.

Capture `req.inner().url()` before `req.url()`/`url::Url` parsing, then preserve it separately with runtime-visible provenance. This does not recover a wire target normalized by the Fetch runtime.

For each `Headers.entries()` value, preserve the pinned Worker runtime's UTF-8 string representation with `HeaderValue::from_bytes(value.as_bytes())`. Header names retain validated construction; invalid HeaderValues produce bounded conversion errors rather than silent omission. Pinned workerd represents headers as UTF-8, which differs from browser ByteString expectations. Do not cast Unicode scalars to single bytes: that changes valid wire C3 A9 to E9 and rejects valid C4 80. Browser-created high-byte strings are not evidence of original Worker wire bytes.

Real workerd probes must assert exact valid UTF-8 preservation for C3 A9 and C4 80. A raw invalid FF is already represented by U+FFFD before Rust; its runtime UTF-8 copy EF BF BD is not the original FF. Expose conservative transformed fidelity and keep this consumer capability unresolved rather than invent lost bytes or add an ordinary-request rejection for a valid runtime string.

Web Headers has already normalized/combined entries: mark original wire multiplicity/global order unavailable or transformed according to verified behavior. Per-name value order and octet provenance must also be stated precisely. Never comma-split Origin/Cookie/control values to invent original fields.

Keep existing body buffering and Env/Context/proxy wiring intact. Distinguish browser Fetch construction constraints from actual incoming Workers HTTP behavior in tests.

### 7.3 Axum

Capture `parts.uri` before any added URL parsing and preserve `parts.headers` through `from_parts`. Source is runtime path/query or URI, not an original TCP request line. Validate inbound scheme/authority from trustworthy runtime binding; an origin-form URI without that information has unavailable origin rather than a guessed scheme.

Audit Hyper parser changes and report per-field limitations. Identical repeated Content-Length may be folded; conflicting lengths may be rejected before hook invocation. Such rejection is transport evidence, not a hook-built local response.

Keep current JSON buffering and streaming non-JSON behavior. No new Tokio dependency in core; existing Axum test infrastructure may use its existing Tokio runtime.

### 7.4 Spin

Capture `parts.uri` and byte headers before conversion. Prefer runtime URI scheme/authority supplied by the SDK/WIT; do not accept `spin-full-url` or another ordinary header as trusted without proving runtime injection and spoofing protection.

Preserve runtime method tokens and existing byte-valued header construction. The SDK has already built a HeaderMap, so global wire order cannot be promised. Record same-name multiplicity/order only at the level actually established by a transport fixture.

Keep current body buffering, store registries and runtime response conversion. Synthetic core-request contract tests alone do not prove SDK request conversion; provide an actual Spin invocation where needed.

## 8. Capability matrix and consumer integration gate

Initial investigation, not a verified release matrix:

| Property                           | Fastly                                | Cloudflare                                             | Axum                                               | Spin                         |
| ---------------------------------- | ------------------------------------- | ------------------------------------------------------ | -------------------------------------------------- | ---------------------------- |
| Arbitrary runtime method           | Byte-oriented existing method; verify | Fix closed enum loss                                   | Existing method parts; verify                      | SDK Other/from_bytes; verify |
| Target before EdgeZero parsing     | SDK raw accessor dependency           | Runtime URL exposed; wire normalization may precede it | Runtime URI parts                                  | SDK runtime URI parts        |
| Header octets before EdgeZero loss | Existing byte path                    | Copy Worker UTF-8 runtime strings                      | Existing byte HeaderMap                            | Existing byte HeaderMap      |
| Original field multiplicity        | Runtime proof required                | Fetch coalescing can make it unavailable               | Per-field parser limits, especially Content-Length | Runtime/SDK proof required   |
| Original global wire order         | Not implied by grouped iteration      | Unavailable after sorted/combined entries              | Not retained by HeaderMap                          | Not retained by HeaderMap    |

The release review must replace each required unknown with Preserved, Transformed, Unavailable or a clearly pending environment test backed by evidence. Unknown is never success.

Two completion claims are distinct:

1. **EdgeZero infrastructure complete:** hook/API, concrete conversion fixes, metadata that honestly describes remaining limits, tests and docs pass.
2. **Trusted Server F0 satisfied across all platforms:** consumer-required path/cookie/control properties are proved, or the consumer spec receives explicit approval for a supported runtime contract. EdgeZero cannot grant that approval.

If a runtime irreversibly hides required path spellings or original duplicate control fields, record the exact limitation and a proposed consumer resolution. Do not silently weaken the approved consumer spec, advertise universal original-wire preservation, disable a consumer platform without approval or report F0 complete solely because a hook exists.

Trusted Server owns early Fastly native shortcut exclusion, classification, authentication, lifecycle gates and terminal response finalization. The ingress helper must be callable before those shortcuts; changing Trusted Server's `main.rs` is outside this EdgeZero branch.

## 9. Tests and acceptance

1. No-hook routing and existing 404/405/middleware/state behavior remain identical.
2. Hook sees arbitrary methods/paths before any router lookup result; terminal success/error runs zero state/context/middleware/handler effects.
3. Continued mutations are observed by actual route lookup and handler; consumed/replaced body behavior is explicit.
4. Hook sharing across clones and a non-Send future compile/run without introducing Tokio.
5. Metadata captures at/over target cap, omits partial values, rejects spoofed origin sources and exposes conservative missing capability.
6. Cloudflare runtime extension methods and UTF-8 strings survive the real converter, not a synthetic core request.
7. Actual runtime probes distinguish platform rejection, normalization, coalescing, EdgeZero conversion and hook response.
8. Fastly raw-target/client-provenance claims depend on the reviewed SDK accessor and real Viceroy ingress evidence.
9. Existing route/response/header/store contracts and all supported target checks pass.
10. Docs include API usage, buffering/finalization limits, per-platform capability matrix and consumer handoff evidence.

Use fictional/example.com fixtures only. Never log actual headers, cookies, credentials, raw target strings or exception payloads in new diagnostic code.

## 10. Review and implementation status

This spec and its matching plan received independent review before the user authorized implementation. The matching plan records implemented surfaces, executed evidence and remaining gates. These documents are not a published API or proof of runtime preservation. The Fastly SDK accessor and any consumer-contract resolution remain explicit external gates.

The plan records independent review results after reviewer findings are resolved. Neither document authorizes publishing, deployment or downstream spec changes.

## 11. Fresh review correction

A subsequent independent review disproved the unconditional browser ByteString assumption in the Cloudflare design above. Pinned workerd exposes header strings as UTF-8; the initial scalar-to-byte converter changed valid wire C3 A9 to E9 and rejected valid C4 80 before the hook. Conversion now copies Worker UTF-8 runtime strings; the matching plan records real Worker verification before release. Invalid UTF-8 replacement remains an independent upstream limitation. This correction does not authorize changing the approved Trusted Server contract. The matching plan records the reproduction, corrective work and F0 resolution choices.

## 12. Approved consumer runtime-boundary amendment

On 2026-10-05, the user approved the independently reviewed amendment to the
existing Trusted Server trace spec and single plan, then authorized its
implementation. The original consumer guarantees described above are the
historical baseline; original-target recovery and original-wire reconstruction
are no longer prerequisites for that consumer v1.

The amended consumer classifies its application-visible URI pathname, keeps
literal-path authentication, and distinguishes runtime rejection, adapter
bootstrap/conversion failure and successful application dispatch. It rejects
still-visible reserved aliases and validates complete same-origin controls.
Runtime-visible Cookie fields are bounded before sanitation. Unproved octets
with U+FFFD, or unproved multiplicity with a literal comma, yield
`unavailable/runtime_header_ambiguous` and suppress trace capture; missing or
Unknown metadata alone permits readable marker-free sessions. The consumer
owns that policy; EdgeZero metadata does not impose it on other applications.

The SDK/runtime limits remain documented capabilities. Consumer F0 now
requires a reviewed immutable EdgeZero revision and verification against the
consumer's resolved dependency graph. Successful normal browser workflows on
all four adapters, early native ordering and terminal finalization remain
consumer acceptance work. Toolkit tests alone do not complete those gates.
