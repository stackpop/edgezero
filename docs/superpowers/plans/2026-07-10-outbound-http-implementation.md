# Outbound HTTP — Implementation Skeleton & Decisions Register

> **STATUS: all 13 review findings are now resolved *in the spec*. Recommend one verification review before authoring phase plans.**
>
> Two prior rounds declared "ready" and were both overturned, so this document does **not** self-certify. Every decision below is `LOCKED` **and written into the spec** (earlier rounds recorded decisions in this register but never landed them — that is what produced the false "ready" signals). What changed, and the evidence behind it, is in [Blocking Decisions](#blocking-decisions).
>
> **Three resolutions came from source-verified investigations** (crates downloaded and **sha256-checked against `Cargo.lock`** before reading):
> - **Spin SDK 6 / WASI 0.3** — the spec's `OutputStream::{subscribe, check_write, write}` upload loop **does not exist**; `wasi:io` is deleted in WASI 0.3. Worse, the SDK's high-level `send` spawns an **uncancellable** body pump, so a stalled source would pin the component task alive forever. Resolved with a hand-built `wasi:http` request (§4.4).
> - **Cloudflare `worker` 0.8.3 + workerd** — `set-cookie` **is** preservable (the repo's pinned `compatibility_date` already enables it); the collapse was EdgeZero's own `HeaderMap::insert`. Found a **second** collapse bug and a **panic hazard** besides.
> - **Spin host-permission sync** — a validation hook in `SpinCliAdapter::execute` would be **dead code**: scaffolded projects always shell out via `[adapters.spin.commands]` and never reach the adapter trait.
>
> **Do not** re-derive these from memory; the spec now carries the verified detail.

**Goal:** Implement the EdgeZero Outbound HTTP design ([`docs/superpowers/specs/2026-05-21-outbound-http-design.md`](../specs/2026-05-21-outbound-http-design.md)) — a portable outbound client (`send` + concurrent `send_all`), deadline/budget primitives, bounded buffering, a nine-capability declaration + enforcement model, and canonical URI accessors — across all four adapters.

**Architecture:** Strict dependency order: **core primitives** (error/body/time/outbound) → **context + extractors** → **capability + manifest + registry** → **CLI gates** → **portable adapters (Axum/Cloudflare/Spin)** → **Fastly** (its own high-risk workstream). Core and adapter crates stay WASM-compatible (no tokio/reqwest/fastly/worker/spin-sdk in core; `async-trait` without `Send`; `web-time` not `std::time::Instant`). Provider errors map only at the adapter boundary.

**Tech Stack:** Rust 1.95 (edition 2021, resolver 2), `matchit`, `async-trait`, `serde`/`serde_json`, `validator`, `web-time`, `futures`, `clap`, `handlebars`. Per-adapter SDKs behind feature gates: `reqwest` (Axum), `worker` (Cloudflare), `fastly` (Fastly), `spin-sdk` 6 / WASI 0.3 (Spin).

## Global Constraints

- **Toolchain:** Rust `1.95.0`, edition `2021`, resolver `2`, license `Apache-2.0`.
- **WASM-first:** no `tokio`/`reqwest`/`fastly`/`worker`/`spin-sdk` deps in `edgezero-core`, `edgezero-adapter`, `edgezero-macros`, or app/library crates. Core stays `default-features = false`. `async-trait` **without** `Send`; `web-time` not `std::time::Instant`.
- **Tests colocated** (`#[cfg(test)]` same file); async tests use `futures::executor::block_on`, never Tokio. No test needs network or platform credentials.
- **No direct `http` crate imports** in application code. Routing uses `matchit` brace syntax.
- **No back-compat shims.** `proxy → outbound` is a breaking, repo-wide rename; downstream (`examples/app-demo`, templates, docs) migrate in the same change. **"Mechanical" is a misnomer** — it *removes* APIs (`ProxyHandle::client()`, mutable body access, extension accessors); treat as a breaking-API migration.
- **Public constants, verbatim:** `DEFAULT_MAX_RESPONSE_BYTES = 1 MiB`, `DEFAULT_OUTBOUND_REQUEST_BODY_BYTES = 8 MiB`, `DEFAULT_INBOUND_JSON_BYTES = 8 MiB`, `DEFAULT_INBOUND_FORM_BYTES = 1 MiB`, `DEFAULT_NO_DEADLINE_BUDGET = 30 s`, `DEADLINE_FAR_FUTURE = 7 days`, `BATCH_DISPATCH_SLACK_MAX = 25 ms`, `AXUM_RESPONSE_STREAM_BUFFER_BYTES = FASTLY_RESPONSE_STREAM_BUFFER_BYTES = 16 MiB`.
- **CI gates:** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-targets`; `cargo check --workspace --all-targets --features "fastly cloudflare spin"`; `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`. **Plus** (currently omitted from spec §5.5, must be added): the generated-project gate and the separate `app-demo` gate — both are directly broken by the rename.

---

## Blocking Decisions

Each decision below blocked one or more phases. **All seven are now `LOCKED`** — resolved, spec amended, safe to plan against. Each records the *verified* conflict (with file:line evidence), the resolution, and the spec sections changed.

### D1 — Manifest/macro type ownership & serialization · `LOCKED`

**Conflict (verified):** `Manifest` derives `Serialize` (`manifest.rs:5`) and `app!` bakes it into introspection JSON (`app.rs:1,53`), so capability types **must** derive `Serialize` or the crate won't compile. Worse, `edgezero-core/Cargo.toml:12` depends on `edgezero-macros`, while `edgezero-macros/src/manifest_definitions.rs:10` **textually `include!`s** core's `manifest.rs`. Core→macros→core is a **cycle**: the macros crate can *never* `use edgezero_core::capability::Capability`, and a `use crate::capability::…` inside `manifest.rs` resolves to `edgezero_macros::capability` in the include context and fails.

**Resolution:** Define `Capability`, `CapabilitySupport`, `ManifestCapabilities`, and `ManifestOutboundCapability` **inline in `manifest.rs`** (the `include!`d file), each deriving `Serialize + Deserialize`. `edgezero-core` re-exports them (`pub use manifest::{Capability, CapabilitySupport};`). **No standalone `capability.rs` module.** Every touch of `manifest.rs` must be followed by `cargo build -p edgezero-macros` as a gate.

**Blocks:** Phase 3.

### D2 — CLI gate scope & typed entry points · `LOCKED`

**Conflict (verified):** The spec gated the wrong functions. `run_config_push` (`config.rs:248`) is a **v1 stub that returns an error**; real writeback is `run_config_push_typed`. `ConfigValidateArgs` (`args.rs:379`) has **no `adapter` field** — the spec's `ConfigValidateArgs::adapter` does not exist. The adapter-selecting `run_config_diff_typed` (`config.rs:390`) is omitted from the gate set entirely.

**Resolution:** Gate the **typed** entry points — `run_config_push_typed`, `run_config_diff_typed`, `run_config_validate` — plus `run_provision` and the `execute(..)` gate. `config validate` is adapter-less: it loops **all configured adapters** in `[adapters]`. `config diff` **is** gated (6th sibling). Spec §3.5.3 gate table updated accordingly.

**`demo` manifest source — RESOLVED.** `run_demo()` (`demo_server.rs:24`) runs the compiled app directly and takes **no path or loader**, so a file-based gate is impossible. **Locked:** the `demo` gate consults the **manifest metadata baked in by the `app!` macro** — the same `Manifest` `app!` already serializes for introspection — *not* a manifest file on disk. Deterministic, needs no new CLI argument, works for the compiled-in demo app. Consequence: **`ensure_capabilities` takes an already-parsed `&Manifest` (or `None`), not a `ManifestLoader` path.** (Spec §3.5.3 gate table updated.)

**Blocks:** Phase 4.

### D3 — Spin streaming API · `LOCKED`

**Conflict (verified):** Spin's response path is concretely **buffered** — `spin/lib.rs:36` imports `FullBody`; `SpinFullResponse`, `AppExt::dispatch`, `request::dispatch*`, `from_core_response`, and `run_app` all carry the fully-buffered `Response<FullBody<Bytes>>` shape. Claiming `lazy-streamed-response-passthrough = Native` on Spin therefore requires **public API migrations** (alias + signature changes), not just an outbound-client change. Separately, the spec's low-level output-stream algorithm is a **WASI-0.2** readiness-poll shape and does not transfer verbatim to **Spin SDK 6 / WASI 0.3**.

**Resolution:** Spin's `lazy-streamed-response-passthrough` is **downgraded to `BestEffort`** for this change. Its response converter performs **bounded buffered passthrough** — drain the wrapped `Body::Stream` to `Bytes` within a new `SPIN_RESPONSE_STREAM_BUFFER_BYTES` constant (16 MiB, mirroring Axum and Fastly); over-cap → `bad_gateway` (502). **Cloudflare is now the only `Native` adapter** for lazy passthrough. The streamable-public-API migration + WASI-0.3 rewrite is deferred to its own change (**spec §8 risk 13**).

**Scope note:** this affects only the **response-out** direction. Spin's outbound **streamed-upload** path is unchanged and stays `Native` for `streamed-upload-deadlines`.

**Spec changes:** §3.5.2 matrix (Spin cell → `BestEffort⁷`) + new **footnote 7** · §3.4.1 lazy grouping · §4.4 (`Native` for **eight of nine**) · §5.4 (three lazy rows rebucketed) · §7 `response.rs` · §8 risk 13.

**Blocks:** Phase 5 (Spin).

### D4 — Cloudflare header & method portability · `LOCKED`

**Conflict (verified):** Worker headers are exposed as **strings** (`cloudflare/proxy.rs:91`) — the original response octets have already crossed WebIDL conversion. The adapter therefore **cannot** reliably detect whether upstream bytes were invalid UTF-8, nor guarantee byte-faithful non-ASCII round-trips. Separately (`proxy.rs:123`), CF silently maps unsupported methods to **GET**, and `fetch` restricts methods and forbids GET/HEAD bodies — the spec defined **no portable method/body contract**.

**Resolution (headers): document an explicit Cloudflare degradation.** The portable header rule stands as written for Axum / Fastly / Spin (which expose raw bytes). On **Cloudflare only**, (a) invalid-UTF-8 detection is unavailable — "drop the invalid value, keep valid siblings" degrades to whatever the runtime already decided; (b) byte-faithful non-ASCII round-trips are **not guaranteed**. Narrowing the portable contract to ASCII-only was **rejected** — it would degrade the three adapters that *can* do this correctly. The §5.4 non-ASCII round-trip row is asserted on Axum/Fastly/Spin and is **best-effort on CF**.

**Resolution (method/body): a portable preflight contract, no silent coercion.**
- **Supported methods** (all adapters): `GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS`.
- **Non-portable method** → core-preflight `bad_request` (400) on **every** adapter — uniform failure, not "works on Fastly, becomes GET on CF".
- **`GET`/`HEAD` with a non-empty body** → core-preflight `bad_request` (400) everywhere (CF's `fetch` forbids it; EdgeZero normalises to the strictest platform).
- **No silent coercion, ever.** An adapter MUST NOT rewrite the method to make a request sendable. CF's GET coercion is **removed** — it is a correctness bug (a `DELETE` was being issued as a `GET`).

Preflight runs in core, so single `send` and a `send_all` slot produce the identical error with index alignment preserved.

**Spec changes:** §3.1.4 (*Cloudflare degradation* + **Method and request-body portability**) · §4.2 (two new bullets) · §5.4 (four new/amended rows).

**Blocks:** Phase 5 (Cloudflare).

### D5 — Fastly passthrough cap & error mapping · `LOCKED`

**Conflict (verified):** §7 and §8 drained the buffered fallback "within `max_response_bytes`" — a **request-level** field that is unavailable at the response converter (`OutboundResponse` carries only status/headers/body). Meanwhile §3.5 already used the adapter constant. Additionally, Fastly's current code maps `wait()` failures to `internal`, with **no normative `SendErrorCause` mapping**.

**Resolution:** Buffered passthrough uses **`FASTLY_RESPONSE_STREAM_BUFFER_BYTES` (16 MiB)** everywhere (spec §7/§8 corrected). Add a normative `SendErrorCause` mapping: **timeout → `gateway_timeout` (504)**; **DNS / TLS / connection-refused → `bad_gateway` (502)**; **adapter contract bug → `internal`** (reserved, per §3.4.3).

**Blocks:** Phase 6.

### D6 — `RequestContext` construction contract · `LOCKED`

**Conflict (verified):** `RequestContext::new(request: Request, params: PathParams)` (`context.rs:143`) is the constructor every adapter calls. The spec restructures the struct to `{ path_params, parts, body: BodyCell }` — all **private** — yet tells adapters to construct it "from parts and a body cell", specifying no constructor.

**Resolution:** **Keep `RequestContext::new(Request, PathParams)` as the public constructor** and split the `Request` into `parts` + `BodyCell` **internally**. Adapters keep calling `new(..)` unchanged; no new public constructor, no exposed `BodyCell`. Accessors (`parts()`, `parts_mut()`, `body_bytes`, `json_within`, `form_within`, `take_body`, `into_request`) are the only new surface.

**Blocks:** Phase 2.

### D7 — Executable test seams & acceptance gates · `LOCKED`

**Conflict:** Several Tier 2 Fastly tests assume seams the spec never defined. Tier 1's `MockOutboundClient` **cannot** prove adapter-specific `send_all` behaviour (each adapter implements it independently). §5.5 omitted the **generated-project** and **`app-demo`** CI gates, both broken by the rename. Capability diagnostics pointed users at footnotes under `docs/superpowers/`, which is `srcExclude`d from the published VitePress site.

**Resolution:** All in scope, now specified in spec §5.5.
- **Fastly test seams are first-class Phase-6 deliverables:** a `#[cfg(test)]` **registered-backend-map inspector** (identity / collision / one-backend-per-canonical-tuple rows); an **injectable clock / dispatch-overhead hook** between `dispatch_budget` and SDK arming (without it `BATCH_DISPATCH_SLACK_MAX` is untestable — a handler-side sleep runs *before* `batch_now` is captured); **SDK call/chunk counters** (`did_dispatch()`, chunk-write count) so "deadline expired during drain → 504 **and** no upstream send" can assert the *absence* of a dispatch.
- **Two CI gates added** (hard, not optional): the **generated-project** gate (a freshly scaffolded project per adapter must build and reference no `Proxy*` symbol) and the **`examples/app-demo`** gate (separate workspace + `Cargo.lock`; breaks on both the rename **and** the §3.4.5 context restructure).
- **Capability diagnostics** must link to a **published** page under `docs/guide/`, not to `docs/superpowers/` footnotes.

**Blocks:** Phase 6 acceptance criteria.

### Resolved during this pass (no longer blocking)

- **EdgeError migration completeness** — spec §7 now specifies both variants in full: `kind_str()` (`"bad_gateway"` / `"gateway_timeout"`), `message()`, `status()` (502/504), `inner()` → `None`, and the `IntoResponse` JSON shape, with exhaustive-match tests. *(Was: statuses only.)*
- **Stale baseline** — spec §1 header corrected: #269 is **merged** (`e483723`); the tree has since gained typed config-push, introspection routes, and expanded CI.
- **CF/Spin over-cap mapping** — corrected: **request**-body over-cap → `bad_request` (400); **response**-body over-cap (decompressed) → `bad_gateway` (502), per the global response-overflow rule.
- **`dispatch_budget` input access** — `time.rs` is a sibling module of `outbound.rs`; `OutboundRequest::{timeout, deadline}` are private. Resolution: expose **`pub(crate)` accessors** on `OutboundRequest` for `timeout`/`deadline`/`max_response_bytes`, consumed by `dispatch_budget`. *(To be reflected in spec §3.3.2.)*

### Still to be reflected in the spec (follow-up edits)

Compression behaviour for `identity` / unknown encodings / casing / stacked encodings / `Accept-Encoding` negotiation · resident-memory equation scoped to **EdgeZero-owned** body buffers (excludes SDK and adapter-boundary copies) · Spin host-rendering synchronization lifecycle (updating `edgezero.toml` after scaffolding does **not** update an existing `spin.toml`, especially via shell-overridden commands) · the security-relevant Spin default-host broadening (HTTPS wildcard → HTTP **+** HTTPS wildcard) must be made explicit.

---

## Phase Skeleton

Phases are listed in dependency order. All decisions are `LOCKED` **and landed in the spec**.

| Phase | Subsystem | Depends on | Decisions (all in spec) | Risk |
| --- | --- | --- | --- | --- |
| 1 | Core primitives: `error`, `body`, `time`, `outbound` | — | D8 `pub(crate)` budget accessors · D9 dispatch-time `validate_for_dispatch` · D10 `StoredError` reconstruction | Med |
| 2 | `RequestContext` + extractors | 1 | D6 `new(Request, PathParams)` preserved, splits internally; `BodyCell` never public | High (blast radius) |
| 3 | Capability + manifest + registry | 1 | D1 capability types **inline in `manifest.rs`** + `Serialize` (core→macros is a cycle; `manifest.rs` is `include!`d) | Med |
| 4 | CLI capability gates | 3 | D2 six gates on the **typed** entry points; `demo` gates on `Hooks::manifest_json()` | Med |
| 5 | Adapters: Axum / Cloudflare / Spin | 1,2,3 | D3 Spin hand-built `wasi:http` (upload `Native`; lazy response `BestEffort`) · D4/D11 CF `append` ×2 + panic fix + documented comma-join loss · D12 Spin sync: provision writes, build validates **at `adapter.rs::execute`** | Med |
| 6 | Fastly | 1,2,3,5 | D5 `SendErrorCause` → 504/502 · D7 seams behind **`test-utils`** (not `#[cfg(test)]`) · D13 tiers by owning crate; only **Axum Tier 3 blocks** | **High** |

**Cross-cutting (affects every phase):** there is **no shared `send_all`** — each adapter implements it independently, so Tier 1's mock cannot prove it. The portability claim is carried by a **reusable conformance suite** exported from core under `test-utils` and run against **every** adapter in Tier 2 (§5.2).

**Phase 1** is the only phase fully unblocked and is the correct place to start once you're ready. Its scope: `EdgeError` 502/504 (full surface per D-resolved §7), `Body::Stream` error type `anyhow::Error → EdgeError`, `into_bytes_bounded` **pre-append** checked accounting, the pure `time.rs` (`Deadline`, `DispatchBudget`, `dispatch_budget` + `pub(crate)` accessors per the resolved item above, and the three constants), and the `proxy.rs → outbound.rs` rewrite (`OutboundHttpClient`, `HttpClient`, `OutboundRequest`, `OutboundResponse`, `ResponseMode`, canonical URI accessors). No adapter or runtime deps.

**Fastly stays a distinct workstream.** Its `send_all` is a **dispatch-all-then-harvest engine**, not `join_all(send)` — the current `send()` (`fastly/proxy.rs:21`) dispatches and immediately `wait()`s, which serializes. This is the single highest-risk requirement in the spec.

---

## Global Completion Conditions

1. **Rename sweep is an explicit gate** — `rg -n 'ProxyClient|ProxyHandle|ProxyRequest|ProxyResponse|ProxyService|proxy_handle|src/proxy\.rs' crates/ examples/ docs/ templates` returns nothing live. Treat as a **breaking-API migration**, not a mechanical rename (removed: `ProxyHandle::client()`, mutable body access, extension accessors).
2. **`RequestContext` sweep** — no live callers of `ctx.request()/request_mut()/json()/form()`.
3. **Templates / app-demo / docs migrated**; `edgezero new` scaffolds the outbound API.
4. **All CI gates green** — including the **generated-project** and **`app-demo`** gates (D7).
5. **Capability enforcement covers every adapter-selecting command** — build/serve/deploy/auth/provision/config push (typed)/**config diff (typed)**/config validate (all adapters)/demo (pending D2 sub-item).

## Coverage Surfaces

- **Core:** URI canonicalization, header normalization, repeated headers, `send_all` stream rejection, index alignment, empty batches, pre-append bounds, 502/504 mapping, deadline boundary cases, `BodyCell` state transitions (incl. **cancelled-drain → `Poisoned`** via drop guard), extractor limits, manifest host parsing (IPv6 accepted).
- **Adapter contracts:** concurrency, partial-batch failure, no redirects, non-2xx success, upload/download limits, per-phase deadline expiry, decompressed limits, repeated headers, invalid response headers, streamed response behaviour, `send` ≡ single-slot `send_all`. **Note:** Tier 1 cannot prove adapter-specific `send_all` — those assertions must live in Tier 2/3 (D7).
- **CLI:** gates run **before** shell overrides and side effects; every required/optional support-state combination; missing-registry policy; all six gate sites at their **typed** entry points.
