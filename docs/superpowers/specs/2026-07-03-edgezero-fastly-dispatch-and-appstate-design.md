# EdgeZero P0-C + P0-D — Fastly `run_app` dispatch fidelity + app-state injection

- **Status:** Draft for edgezero maintainer
- **Date:** 2026-07-03
- **Target repo:** `github.com/stackpop/edgezero` (`edgezero-adapter-fastly`, `edgezero-core`, `edgezero-macros`)
- **Consumed by:** trusted-server "full convergence" migration — the decision that every adapter binary becomes the one-line `run_app::<App>` with `#[action]` handlers. These two capabilities are the remaining gaps that block Fastly (P0-C) and macro-based app state (P0-D). Independent of the earlier Phase 0 spec (State<T> + nested `#[secret]`, PR #306).
- **Verified against:** originally `6ebc29a5`; **re-verified and revised against `47a112c`** (branch `worktree-state-nested-secrets-spec-review`, PR #306 tip). The revision matters: between those commits the router's app-state layer was simplified from a `Vec<StateInserter>` of type-erased closures to a single `state_extensions: Extensions` bag, which materially simplifies P0-D (see §P0-D).

> **Maintainer-review revisions (2026-07-03..04):** folded in after verifying every claim against `47a112c`.
> - **Round 1 — P0-D re-specced** to reuse the router's existing `Extensions`-bag state injection (macro-only; no new `Hooks` method, no `AppState` carrier, no per-adapter edits — the original `StateInserter` design no longer exists). **C3 wiring corrected** — the raw-request closure runs *before* conversion against a scratch `Extensions` (conversion consumes the `fastly::Request`). **C2** gained an `app!` opt-in for fully-macro apps.
> - **Round 2 — C3 `Extensions` path** fixed to `edgezero_core::http::Extensions` (the snippet lives in the fastly adapter, not core). **C2 scope** made a decision point: `owns_logging()` is a platform-neutral `Hooks` method, so **all four** adapter entrypoints must gate their logger init on it (or C2 is scoped Fastly-only) — the files list previously only touched Fastly. **C1** extended to the proxy **response** path (`convert_response` collapses multi-value origin `Set-Cookie`). **`app!` argument grammar** fully specified (coexistence of the custom app ident with `state =` / `owns_logging =`, order, duplicate/unknown-key errors) — previously undefined.
> - **Round 3 — `owns_logging` macro emission** corrected: the macro must **always emit** `fn owns_logging() -> bool { #bool }` (not rely on the trait default), because `clippy::missing_trait_methods` (`restriction = deny`) forbids inheriting defaulted trait methods; the same lint means adding the trait method touches **every** `impl Hooks` in the workspace (noted in the C2 scope decision). Stale `state = f` acceptance wording corrected to `state = <expr>`.

---

## Why

trusted-server is converging on the canonical `app-demo` wiring: `run_app::<App>` on every adapter, `#[action]` handlers, `State<Arc<AppState>>`. Two things stop that today:

1. **Fastly `run_app` loses fidelity** that trusted-server's hand-written custom dispatch preserves: multi-value `Set-Cookie` headers, an opt-out from the per-call logger reinit, and a pre-dispatch hook to capture Fastly-only request signals (TLS JA4 / H2 fingerprint, client IP) from the raw `fastly::Request` before it is converted to the neutral core request. → **P0-C.**
2. **Macro/`run_app` apps can't inject app-owned state.** `State<T>` + `RouterBuilder::with_state` exist (PR #306) and the router injects registered state at dispatch — but the `app!` macro generates the router and never calls `with_state`, and `run_app` doesn't inject app state. So `State<Arc<AppState>>` can't reach handlers in a macro app. → **P0-D.**

**P0-D is optional** (see §4): if a downstream keeps a hand-written `Hooks::routes()` that calls `RouterBuilder::with_state`, the existing dispatch-time injection already delivers `State<T>` under `run_app` — no edgezero change needed. P0-D is required only to support app-owned state **through the `app!` macro**. It is specified here so the maintainer can choose to support the fully-macro path.

---

## P0-C — Fastly `run_app` dispatch fidelity

Three independent sub-changes in `edgezero-adapter-fastly`. Each is small and separately testable.

### C1 — Preserve multi-value response headers (`Set-Cookie`)

**Current (bug):** `crates/edgezero-adapter-fastly/src/response.rs` builds the `fastly::Response` by looping over the core response's `HeaderMap` and calling `set_header`, which **replaces** — so N `Set-Cookie` values collapse to the last one:

```rust
// response.rs (~line 28)
for (name, value) in &parts.headers {
    fastly_response.set_header(name.as_str(), value.as_bytes());
}
```

`http::HeaderMap`'s iterator yields **one entry per value** (duplicates included), and the `fastly::Response` starts empty (`FastlyResponse::from_status(...)`). So the fix is to **append** instead of set:

```rust
for (name, value) in &parts.headers {
    fastly_response.append_header(name.as_str(), value.as_bytes());
}
```

`append_header` adds without clobbering, so all `Set-Cookie` (and any other multi-value header) survive. This is unconditionally correct given a fresh response; no per-header special-casing needed.

**Two more proxy header surfaces have the same class of defect:**

1. **Proxy *request* construction** — `proxy.rs:53` uses `set_header` when building the upstream `fastly::Request`. Request-side multi-value headers are rare (`Cookie` folds to one), so this is audit-only: either switch to `append_header` for consistency or document why `set_header` is acceptable here.
2. **Proxy *response* conversion (must fix, or explicitly exclude)** — `convert_response` in `proxy.rs` collapses **origin** multi-value response headers. It iterates `fastly_response.get_header_names()` and does `proxy_response.headers_mut().insert(header, fastly_response.get_header(header)...)` — `get_header` returns only the first value and `HeaderMap::insert` replaces, so multiple upstream `Set-Cookie` collapse to one. Origin `Set-Cookie` is common (auth/session), so this matters more than the request side. Fix by reading **all** values and appending: for each `name` in `get_header_names()`, iterate `fastly_response.get_header_all(name)` and `proxy_response.headers_mut().append(name, value.clone())`. **If P0-C intends to preserve multi-value headers, this is in scope**; otherwise state explicitly that proxy-response fidelity is out of P0-C.

**Tests:** (a) a handler returns a `Response` with two `Set-Cookie` values → the converted `fastly::Response` (`get_header_all("set-cookie")`) contains both; (b) an upstream `fastly::Response` with two `Set-Cookie` → `convert_response` yields a `ProxyResponse` whose `HeaderMap` retains both.

### C2 — Let the app opt out of the `run_app` logger init

**Current:** `run_app` (`lib.rs:113`) initializes the Fastly logger unconditionally when `use_fastly_logger`:

```rust
let logging = logging_from_env(&env);
if logging.use_fastly_logger {
    init_logger(endpoint, logging.level, logging.echo_stdout)?;
}
```

An app that already owns `log`/`log-fastly` initialization (trusted-server does) cannot use `run_app` without a double-init conflict. Provide an opt-out. **Preferred:** a `Hooks` flag consulted by every adapter's `run_app`, so it is platform-neutral:

```rust
// edgezero-core/src/app.rs — Hooks
/// When `true`, the adapter's `run_app` skips its own logger
/// initialization; the app is responsible for installing a `log` backend.
/// Default `false` (adapter initializes logging as today).
fn owns_logging() -> bool { false }
```

**Scope decision (this is a platform-neutral `Hooks` method, so ALL adapters must honor it — otherwise the flag is a lie on 3 of 4 platforms).** Every adapter entrypoint that initializes a logger must gate it on `!A::owns_logging()`:
- **Fastly** — `lib.rs:117` `init_logger(...)` (the primary target).
- **Cloudflare** — `lib.rs:105` `drop(init_logger())`.
- **Axum** — `dev_server.rs:343` `SimpleLogger::new()...init()`.
- **Spin** — `lib.rs:115` `drop(init_logger())` (already a no-op, but gate it for uniformity and to keep the contract honest).

**Hidden cost of the `Hooks` method (feeds this decision):** `clippy::missing_trait_methods` (`restriction = deny`) forbids any `impl Hooks` from inheriting a defaulted method. So adding `owns_logging` to the trait — even with a default — forces **every existing `impl Hooks` in the workspace to add an explicit `fn owns_logging()`**: the `app!` macro's emitted impl (add it alongside `configure`/`build_app`), `app-demo`'s app, and every hand-written test/fixture `Hooks` impl. This is mechanical but wider than "core + 4 adapters." The Fastly-only alternative (a `run_app_without_logger::<A>` variant, no trait method) avoids this ripple entirely.

If cross-adapter wiring + the trait-impl ripple are undesirable for the first landing, the alternative is to **scope C2 explicitly to Fastly** and NOT add a core `Hooks` method — e.g. a Fastly-only `run_app_without_logger::<A>` variant. Do not ship a platform-neutral `Hooks::owns_logging()` that only Fastly consults. **Recommendation:** the neutral `Hooks` method, wired through all four entrypoints and all `Hooks` impls (mechanical), since trusted-server converges on all adapters — but the plan must enumerate every `impl Hooks` site it touches.

`run_app` becomes `if logging.use_fastly_logger && !A::owns_logging() { init_logger(...)?; }`. (Alternative: a `run_app_without_logger::<A>` variant — but the `Hooks` flag composes with the `app!` macro and applies uniformly across adapters, so prefer it.)

**Macro opt-in (required for the fully-macro path).** The `Hooks` default `owns_logging() -> false` is only overridable by a **hand-written** `Hooks` impl. A fully-macro app (the stated trusted-server target, which *does* own its `log`/`log-fastly` init) has no way to set it — the `app!` macro generates the `Hooks` impl and would emit the default. So C2 must also give `app!` an `owns_logging = true` argument (parallel to P0-D's `state = …`), e.g. `app!("edgezero.toml", owns_logging = true)`, which emits `fn owns_logging() -> bool { true }` in the generated impl. Without it, C2 only helps hand-written `Hooks` impls, not the macro path C2 exists to unblock.

**Test:** an app with `owns_logging() == true` runs `run_app` twice / after the app initialized its own logger without the init error. Add a macro-path test: `app!("…", owns_logging = true)` emits `owns_logging() == true`.

### C3 — Pre-dispatch hook for raw-request signals (JA4 / H2 / client IP)

**Current:** `run_app` → `dispatch_with_registries` → `dispatch_with_handles` (`request.rs:279`) calls `into_core_request(req)` (`request.rs:284`), which **consumes the `fastly::Request` by value**, then inserts the store registries into the core request's extensions (`request.rs:266-272`) and runs `app.router().oneshot(core_request)` (`request.rs:274`). There is **no hook** to read the *original* `fastly::Request` (whose `get_tls_ja4()`, `get_client_h2_fingerprint()`, client-IP getter are only available pre-conversion) and stash derived values into the core request's extensions. trusted-server's custom path does this before dispatch. (Note: `context.rs:19` already inserts a `FastlyContext` into the core request; C3 supplements that with app-specific signals — the plan should state whether client-IP is already captured there to avoid duplication.)

**Ordering constraint (this corrects the original sketch).** The raw signals must be **read before** `into_core_request` consumes `req`, but they must be **written into** the core request's `Extensions`, which only exist **after** conversion. So the closure cannot receive `core_req.extensions_mut()` "after conversion" — by then `req` is gone. Resolve by running the closure against a **scratch `Extensions`** before conversion, then merging it in after:

```rust
// edgezero-adapter-fastly/src/lib.rs
use edgezero_core::http::Extensions; // NOT `crate::http` — that facade is edgezero-core's;
                                     // the fastly adapter reaches it via `edgezero_core::http`.

pub fn run_app_with_request_extensions<A, F>(
    req: fastly::Request,
    extend: F,
) -> Result<fastly::Response, fastly::Error>
where
    A: Hooks,
    F: FnOnce(&fastly::Request, &mut Extensions),
{ /* same as run_app, but thread `extend` into dispatch; there:
     let mut scratch = Extensions::default();     // Extensions: Default
     extend(&req, &mut scratch);                  // BEFORE into_core_request(req)
     let mut core_req = into_core_request(req)?;
     core_req.extensions_mut().extend(scratch);   // reuse the Extensions::extend pattern
     // ... registry inserts, then router.oneshot ... */ }
```

(The neutral core request produced by `into_core_request` is `edgezero_core::http::Request`, so `edgezero_core::http::Extensions` is the matching type — the closure's bag and the core request's extensions map are the same `http::Extensions`.)

The closure runs once per request, reads the raw `fastly::Request`, and populates a scratch bag that is `extend`ed into the core request (the same `Extensions::extend` mechanism the router uses for app state). `run_app` stays as the no-hook convenience wrapper (`run_app_with_request_extensions::<A>(req, |_, _| {})`).

This requires threading the closure from `run_app_with_request_extensions` → `dispatch_with_registries` → `dispatch_with_handles` (add a generic `extend: F` parameter, or `Option<&mut dyn FnMut(&FastlyRequest, &mut Extensions)>`), with the scratch-then-extend step landing in `dispatch_with_handles` around `into_core_request`. Keep the existing `dispatch_with_registries` entry working (the no-op closure).

**Test:** a handler reads a value from extensions that only the pre-dispatch closure could have set (e.g. a synthetic `Ja4` newtype); assert it is present.

### P0-C acceptance

- Multi-value `Set-Cookie` round-trips through `run_app` (C1) — both the handler-response path (`response.rs`) and, unless explicitly excluded, the proxy-response path (`proxy.rs::convert_response`).
- An app with `owns_logging() == true` runs under `run_app` without a logger-init error (C2) — verified on Fastly and, since `owns_logging` is a platform-neutral `Hooks` method, wired through the Cloudflare / Axum / Spin entrypoints too (or C2 is explicitly scoped Fastly-only — see the C2 scope decision).
- A pre-dispatch closure can populate core-request extensions from the raw `fastly::Request` (C3).
- `app-demo` still builds/serves; existing Fastly tests green; `run_app` (no-hook) behavior unchanged for apps that don't opt in.

---

## P0-D — App-state injection for macro / `run_app` apps

### The gap

`State<T>` (`extractor.rs:550`) reads from request extensions; `RouterBuilder::with_state` (`router.rs`) registers a value in the router's `state_extensions: Extensions` bag, which dispatch clones into each request via `request.extensions_mut().extend(self.state_extensions.clone())`. That works when the app **hand-builds** its router. But the `app!` macro's generated `build_router()` (`edgezero-macros/src/app.rs:185`) only calls `.with_manifest_json(...)` — never `.with_state(...)` — so a macro app has no way to provide `State<Arc<AppState>>`.

### Design — bake app state into the router via the macro (revised)

> **Revised after the `Extensions`-bag reshape.** The original design added a `Hooks::app_state()` method returning a type-erased `AppState` carrier and applied it in **all four adapters'** `run_app`, "mirroring registry injection." That is unnecessary and was written against the removed `StateInserter` layer. Registries are injected adapter-side **because they are platform-specific handles that cannot live in the neutral router**; **app state is platform-neutral and already lives in the router** (`state_extensions`), whose dispatch injection (shipped in PR #306) delivers it to every request. So P0-D is just: **have the macro-generated router call `with_state`** — reusing the exact path the hand-written case already uses. No new `Hooks` method, no `AppState` carrier, **no adapter changes**.

**`edgezero-macros` — `app!` gains an optional `state` argument.** `build_router()` emits one extra builder call, right next to the existing `with_manifest_json`:

```rust
edgezero_core::app!("edgezero.toml", state = crate::app_state());

// in the generated build_router():
let mut builder = RouterService::builder();
builder = builder.with_manifest_json(#manifest_json_lit);
builder = builder.with_state(crate::app_state());   // #state_expr, only when `state = …` is given
// ... routes ...
```

**`state = <expr>` is a full Rust expression** evaluating to the app-owned value, emitted **verbatim** into `.with_state(<expr>)`. It must be the call/expression, **not** a bare function path — `with_state` takes the state *value*, so `state = crate::app_state` (a fn item) would pass the function, not its result. Write `state = crate::app_state()` or `state = std::sync::Arc::new(AppState::new())`. (This mirrors nothing magical: the macro does not append `()`.) `run_app` → `A::build_app()` → `routes()` → `build_router()` already runs per request (Fastly) / once at startup (Axum), and #306's dispatch injection clones the value into each request — so `State<T>` reaches `#[action]` handlers on **all four adapters** with no adapter edits. Without the `state` argument, `build_router()` is unchanged (no state), preserving current behavior.

**Single vs. multiple state types.** `with_state<T>` registers one `T` — a single `state = crate::app_state()` covers the `Arc<AppState>` case (what trusted-server / `app-demo` need). If multiple state types are ever required, allow repeated `state = a(), state = b()` (emit one `.with_state(...)` per occurrence) or add a `RouterBuilder::with_state_extensions(Extensions)` fed by an app-supplied `Extensions` bag. Default to the single-value form unless a concrete multi-type need appears. **The grammar below permits repeated `state`; whether repeats are accepted or rejected is a decision the plan must state** (see §`app!` argument grammar).

### `app!` argument grammar (governs P0-D `state` and C2 `owns_logging`)

This must be nailed down before a step-by-step plan. Today `AppArgs::parse` (`edgezero-macros/src/app.rs:12`) accepts only `app!("path")` or `app!("path", AppIdent)` and errors on any further tokens. Extend it to:

```
app!( PATH [, APP_IDENT] [, KEY = VALUE]* )
```

- **PATH** — string literal (manifest path). Required, first. Unchanged.
- **APP_IDENT** — optional bare identifier (the custom `App` type name), exactly as today. If present it must be the **first** comma item after PATH, before any `KEY = VALUE`. At most one.
- **KEY = VALUE** — zero or more keyword arguments, **order-independent** among themselves, following `APP_IDENT` if that is present. Recognized keys:
  - `state = <expr>` — a `syn::Expr`; emits `.with_state(<expr>)` in `build_router()`.
  - `owns_logging = <bool-lit>` — `true` or `false` (a `syn::LitBool`). The macro **always emits** `fn owns_logging() -> bool { #bool }` in the generated `Hooks` impl, defaulting `#bool` to `false` when the argument is omitted. It must **not** rely on the trait default: `clippy::missing_trait_methods` (`restriction = deny`, `Cargo.toml`) forbids an impl from inheriting a defaulted trait method, which is exactly why the macro already emits explicit `configure`/`build_app` bodies (`edgezero-macros/src/app.rs:154`). Emit `owns_logging` the same way.
- **Disambiguation** — after PATH, iterate comma-separated items. For each item, `peek2(Token![=])`: if the next-next token is `=`, parse `Ident = Value` as a keyword; otherwise parse a bare `Ident` as `APP_IDENT`.
- **Errors (define exact messages in the plan):**
  - Unknown key → `` unknown `app!` argument `<k>`; expected `state` or `owns_logging` ``.
  - Duplicate key → `` duplicate `<k>` argument ``. (Decide state-repeat policy: reject as duplicate, or allow N `state` args — pick one; recommend **reject duplicates** initially, single `state` only, to keep it simple.)
  - Bare ident after a keyword arg, or a second bare ident → `` the custom App identifier must come immediately after the manifest path, before keyword arguments ``.
  - Wrong value type (`state` given a non-expression, `owns_logging` given a non-bool) → a clear per-key message.
- **`AppArgs` becomes** `{ path: LitStr, app_ident: Option<Ident>, state: Option<syn::Expr>, owns_logging: Option<bool> }`; the two new fields feed `build_router()` (state) and the generated `Hooks` impl (owns_logging). Add UI/trybuild-style or unit coverage for: happy path each key, both keys, key + app ident, unknown key, duplicate key, ident-after-key.

### Alternative that needs NO macro change (document in the guide)

A downstream that keeps a **hand-written `Hooks::routes()`** can call `RouterBuilder::with_state(app_state)` there directly; the dispatch-time `state_extensions` injection then delivers it under `run_app` with zero further change. The trade-off is routes are built in Rust rather than declared in `edgezero.toml`. trusted-server may take this path — but the `state = …` macro argument is what makes app state work for the **fully macro-driven** shape `app-demo` models.

### P0-D acceptance

- A macro app declaring `app!("...", state = <expr>)` (e.g. `state = crate::app_state()`) can extract `State<T>` (where `T` is the expression's value type) in an `#[action]` handler on all four adapters.
- An app that provides no state is unaffected (`State<T>` for an unregistered `T` returns the existing "no state registered" 500).
- `app-demo` gains a small example using `app!(..., state = ...)` + a `State<T>` handler.
- **No adapter (`run_app`) or `Hooks`-trait change is required for P0-D** — the diff is confined to `edgezero-macros` (and the `app-demo` example). This is the acceptance signal that the revised, simpler design was taken.

---

## Sequencing & interaction with trusted-server Phase 1

- **P0-C is required** for trusted-server Phase 4 (Fastly `run_app`). Until it lands, trusted-server's Phase 1 keeps interim Fastly local registry builders + custom `oneshot`; those are deleted in Phase 4 once P0-C exists. Landing P0-C early lets Phase 1 skip that throwaway scaffolding.
- **P0-D is required only for the `app!`-macro path.** If trusted-server keeps hand-built `routes()` + `with_state`, P0-C alone suffices for full `run_app` convergence. Decide this before Phase 4.
- Both are independent of the nested-`#[secret]` work already in #306.

## Files to touch (edgezero)

**P0-C**
- `crates/edgezero-adapter-fastly/src/response.rs` — `set_header` → `append_header` in the handler-response loop (C1)
- `crates/edgezero-adapter-fastly/src/proxy.rs` — (a) audit/switch request-side `set_header` (`:53`); (b) fix `convert_response` to read `get_header_all` + `headers_mut().append` so multi-value **origin** response headers survive — unless proxy-response fidelity is explicitly excluded from P0-C (C1)
- `crates/edgezero-core/src/app.rs` — `Hooks::owns_logging()` default-`false` method (C2)
- `crates/edgezero-adapter-fastly/src/lib.rs` — gate `init_logger` (`:117`) on `!A::owns_logging()`; add `run_app_with_request_extensions` (C2, C3)
- `crates/edgezero-adapter-{cloudflare,axum,spin}/src/…` — gate each entrypoint's logger init on `!A::owns_logging()` (Cloudflare `lib.rs:105`, Axum `dev_server.rs:343`, Spin `lib.rs:115`) — required to keep the neutral `Hooks` flag honest (C2; omit only if C2 is scoped Fastly-only)
- `crates/edgezero-adapter-fastly/src/request.rs` — thread the pre-dispatch closure through `dispatch_with_registries`/`dispatch_with_handles`; scratch-`Extensions`-then-`extend` around `into_core_request` (C3)
- `crates/edgezero-macros/src/app.rs` — `owns_logging = true` argument so fully-macro apps can opt out of adapter logger init; per the `app!` argument grammar (C2)

**P0-D** *(revised — macro-only; the router already owns state injection)*
- `crates/edgezero-macros/src/app.rs` — optional `state = <expr>` argument (per the `app!` argument grammar); `build_router()` emits `.with_state(#state_expr)`
- `examples/app-demo/…` — small `app!(..., state = …)` + `State<T>` handler example
- *(No `edgezero-core` `Hooks`/`AppState` change and no adapter change — superseding the original list's `Hooks::app_state()`, the router `StateInserter`-exposure line, and the four-adapter edits, all of which the `Extensions`-bag reshape made unnecessary.)*

> **Note:** `crates/edgezero-macros/src/app.rs` is touched by **both** C2 (`owns_logging =`) and P0-D (`state =`), and both extend the same `AppArgs` grammar. If P0-C and P0-D are planned/shipped as separate plans (recommended — they are independent), the `app!` grammar extension is a shared prerequisite; land the `AppArgs` grammar rework once (with both keys) or sequence the plans so the second rebases on the first.
