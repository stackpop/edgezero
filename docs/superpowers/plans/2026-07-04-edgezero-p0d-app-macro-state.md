# EdgeZero P0-D — `app!` App-State Injection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a fully-macro app provide app-owned shared state (`Arc<AppState>`) to `#[action]` handlers via `app!("edgezero.toml", state = <expr>)`, so `State<T>` works without a hand-written `Hooks::routes()`.

**Architecture:** The router already owns per-request state injection (PR #306: `RouterBuilder::with_state` + a `state_extensions: Extensions` bag that dispatch `extend`s into every request). So P0-D is macro-only: the `app!`-generated `build_router()` calls `.with_state(<expr>)` — one builder call next to its existing `.with_manifest_json(...)`. No adapter change, no new `Hooks` method, no state carrier.

**Tech Stack:** Rust 1.95, edition 2021. `edgezero-macros` (`app!`), `examples/app-demo` (worked example). Reuses `edgezero-core` `RouterBuilder::with_state` / `State<T>` unchanged.

**Source spec:** `docs/superpowers/specs/2026-07-03-edgezero-p0cd-fastly-dispatch-and-appstate-design.md` (P0-D), verified against `65afbd3`.

## Global Constraints

- **Rust 1.95.0**, edition 2021. Strict clippy gate (`restriction = deny`): watch `arbitrary_source_item_ordering` (order struct fields / test fns), `min_ident_chars`, `absolute_paths`, `impl_trait_in_params`, `assertions_on_result_states`, `needless_raw_strings`, `missing_trait_methods`.
- **DEPENDS ON P0-C.** This plan **extends the `app!` `AppArgs` keyword-argument grammar** introduced by the P0-C plan (`2026-07-04-edgezero-p0c-fastly-dispatch-fidelity.md`, Task 4). Execute P0-C first. After P0-C, `AppArgs` is `struct AppArgs { app_ident: Option<Ident>, owns_logging: Option<bool>, path: LitStr }` with a keyword-arg parser whose `match key.to_string()` has an `owns_logging` arm and an `_ => unknown key` arm; Task 1 below adds a `state` field + arm. If P0-C has not landed, do it first — do not reintroduce the grammar here.
- **Reused, unchanged API** (from PR #306, `crates/edgezero-core/src/router.rs`): `RouterBuilder::with_state<T>(self, value: T) -> Self where T: Clone + Send + Sync + 'static`; the router injects `state_extensions` into every request at dispatch. `State<T>` (`crates/edgezero-core/src/extractor.rs`) extracts it; an unregistered `T` → `500`.
- **CI gates (all must pass):** `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-targets`; `cargo check --workspace --all-targets --features "fastly cloudflare spin"`; `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`; and `(cd examples/app-demo && cargo test)`.
- **No backward-compat constraint.** `state` is optional; omitting it leaves `build_router()` byte-identical to today.

---

## File Structure

| File | Responsibility | Task |
| ---- | -------------- | ---- |
| `crates/edgezero-macros/src/app.rs` | `AppArgs` gains `state: Option<syn::Expr>`; parser adds the `state` arm; `build_router()` emits `.with_state(#state)`; `AppArgs` unit tests | 1 |
| `examples/app-demo/crates/app-demo-core/src/lib.rs` | crate-root `DemoState` + `app_state()`; `app!(…, state = crate::app_state())` | 2 |
| `examples/app-demo/crates/app-demo-core/src/handlers.rs` | a `State<Arc<DemoState>>` `#[action]` handler + host test through `build_router()` | 2 |
| `examples/app-demo/edgezero.toml` | route for the state-demo handler | 2 |

---

## Task 1: `app!(state = <expr>)` — parse + emit `.with_state(...)`

**Files:**
- Modify: `crates/edgezero-macros/src/app.rs` (`AppArgs` struct + parser `state` arm; `expand_app` emits `.with_state(#state_expr)` in `build_router()`, `app.rs:185-191`; add `AppArgs` state unit tests)

**Interfaces:**
- Consumes (from P0-C): the `AppArgs` keyword-arg parser with its `match key.to_string()` dispatch.
- Produces: `AppArgs.state: Option<syn::Expr>`; `build_router()` emits `builder = builder.with_state(#state_expr);` when `state` is present.

- [ ] **Step 1: Write the failing `AppArgs` state unit tests**

Add to the `#[cfg(test)] mod tests` in `crates/edgezero-macros/src/app.rs` (which after P0-C already has `use super::AppArgs;`, `use syn::parse_str;`, and the `app_args_*` tests). Place alphabetically among the `app_args_*` fns. To assert the parsed expression, render it with `quote`:

```rust
    #[test]
    fn app_args_parses_state_expr() {
        let args: AppArgs = parse_str(r#""edgezero.toml", state = crate::app_state()"#).expect("parse");
        let rendered = args.state.map(|expr| quote::quote!(#expr).to_string());
        assert_eq!(rendered, Some("crate :: app_state ()".to_owned()));
        assert!(args.app_ident.is_none());
        assert_eq!(args.owns_logging, None);
    }

    #[test]
    fn app_args_parses_state_with_app_ident_and_owns_logging() {
        let args: AppArgs = parse_str(
            r#""edgezero.toml", MyApp, state = crate::app_state(), owns_logging = true"#,
        )
        .expect("parse");
        assert_eq!(args.app_ident.map(|ident| ident.to_string()), Some("MyApp".to_owned()));
        assert_eq!(args.owns_logging, Some(true));
        assert!(args.state.is_some());
    }

    #[test]
    fn app_args_rejects_duplicate_state() {
        let err = parse_str::<AppArgs>(r#""edgezero.toml", state = a(), state = b()"#)
            .expect_err("duplicate state");
        assert!(err.to_string().contains("duplicate `state`"), "got: {err}");
    }
```

- [ ] **Step 2: Run — verify they fail**

Run: `cargo test -p edgezero-macros --lib app_args_parses_state 2>&1 | tail -20`
Expected: FAIL — `AppArgs` has no `state` field, and `state = …` hits the unknown-key arm ("unknown `app!` argument `state`").

- [ ] **Step 3: Add the `state` field + parser arm**

In `crates/edgezero-macros/src/app.rs`, add `state` to the `AppArgs` struct (alphabetical field order: `app_ident`, `owns_logging`, `path`, `state`):

```rust
struct AppArgs {
    app_ident: Option<Ident>,
    owns_logging: Option<bool>,
    path: LitStr,
    state: Option<syn::Expr>,
}
```

In `impl Parse`, add a `state` local (`let mut state: Option<syn::Expr> = None;` next to the `owns_logging` local), add the `state` arm to the `match key.to_string().as_str()`, and include `state` in the returned `Self`:

```rust
                    "state" => {
                        if state.is_some() {
                            return Err(syn::Error::new(key.span(), "duplicate `state` argument"));
                        }
                        state = Some(input.parse::<syn::Expr>()?);
                    }
                    "owns_logging" => {
                        if owns_logging.is_some() {
                            return Err(syn::Error::new(key.span(), "duplicate `owns_logging` argument"));
                        }
                        let value: syn::LitBool = input.parse()?;
                        owns_logging = Some(value.value);
                    }
                    other => {
                        return Err(syn::Error::new(
                            key.span(),
                            format!("unknown `app!` argument `{other}`; expected `state` or `owns_logging`"),
                        ));
                    }
```

and the constructor:

```rust
        Ok(Self { app_ident, owns_logging, path, state })
```

- [ ] **Step 4: Emit `.with_state(...)` in `build_router()`**

In `expand_app`, before the `quote!` block, turn the optional state expression into optional emitted tokens (place near the other `let …` bindings). Use `Option<&Expr>` mapped to a `TokenStream2` so the call is emitted only when present:

```rust
    let state_call = args.state.as_ref().map(|state_expr| {
        quote! { builder = builder.with_state(#state_expr); }
    });
```

Then insert `#state_call` into the emitted `build_router()` (`app.rs:185-191`), right after the `with_manifest_json` line:

```rust
        pub fn build_router() -> edgezero_core::router::RouterService {
            let mut builder = edgezero_core::router::RouterService::builder();
            builder = builder.with_manifest_json(#manifest_json_lit);
            #state_call
            #(#middleware_tokens)*
            #(#route_tokens)*
            builder.build()
        }
```

(`Option<TokenStream2>` implements `ToTokens` — `None` emits nothing, so omitting `state` leaves `build_router()` unchanged.)

- [ ] **Step 5: Run the unit tests — verify they pass**

Run: `cargo test -p edgezero-macros --lib app_args_ 2>&1 | tail -12`
Expected: PASS — the new `app_args_parses_state*` / `app_args_rejects_duplicate_state` plus the P0-C `app_args_*` tests.

- [ ] **Step 6: Confirm app-demo (no `state`) is unchanged + lint**

Run: `(cd examples/app-demo && cargo test -p app-demo-core 2>&1 | tail -5)`
Expected: PASS (app-demo's `app!` has no `state` yet, so `build_router()` is byte-identical).
Run: `cargo clippy -p edgezero-macros --all-targets --all-features -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/edgezero-macros/src/app.rs
git commit -m "feat(macros): app!(state = <expr>) emits RouterBuilder::with_state for macro apps"
```

---

## Task 2: app-demo worked example + end-to-end `State<T>` test

Prove the whole chain: `app!(state = crate::app_state())` → generated `build_router()` calls `.with_state(...)` → dispatch injects → a `State<Arc<DemoState>>` handler reads it. This runs **host-side** through `build_router()` (no adapter/hostcalls), mirroring the existing `crate::build_router()` test at `handlers.rs:436`.

**Files:**
- Modify: `examples/app-demo/crates/app-demo-core/src/lib.rs` (define `DemoState` + `app_state()` at the **crate root**; add `state = crate::app_state()` to `app!`)
- Modify: `examples/app-demo/crates/app-demo-core/src/handlers.rs` (add the `State` handler + host test)
- Modify: `examples/app-demo/edgezero.toml` (route for the handler)

**Interfaces:**
- Consumes: `edgezero_core::extractor::State`, `edgezero_core::action`, `RouterService::oneshot` (from #306), `crate::build_router()` (macro-generated).
- Produces: crate-root `crate::{DemoState, app_state}`; `crate::handlers::state_demo` handler.

> **Design note (why crate root, not a `state` module):** app-demo is its own workspace with a stricter lint set — `pub_use` and `module_name_repetitions` are **denied** (unlike the root workspace which allows them). A `pub mod state;` + `pub use crate::state::app_state;` trips `pub_use`, and `DemoState`/`app_state` inside a module named `state` trip `module_name_repetitions`. Defining both at the crate root (in `lib.rs`) avoids all three with no `#[allow]`, and `crate::app_state()` / `crate::DemoState` resolve directly where the macro emits them.

- [ ] **Step 1: Define `DemoState` + `app_state()` at the crate root and wire `app!`**

In `examples/app-demo/crates/app-demo-core/src/lib.rs`, add the state types at the crate root (after the `pub mod` declarations) and add the `state` argument to the `app!` invocation (`lib.rs:11`):

```rust
pub mod handlers;

use std::sync::Arc;

/// App-owned shared state for the `app!(..., state = ...)` demonstration,
/// handed to handlers via `State<Arc<DemoState>>`.
#[derive(Debug)]
pub struct DemoState {
    /// A greeting the handler echoes, proving the value reached the handler.
    pub greeting: String,
}

/// Constructs the shared app state. Referenced by `app!(..., state = crate::app_state())`.
#[must_use]
#[inline]
pub fn app_state() -> Arc<DemoState> {
    Arc::new(DemoState {
        greeting: "hello from app state".to_owned(),
    })
}

edgezero_core::app!("../../edgezero.toml", state = crate::app_state());
```

(`#[inline]` is required by `missing_inline_in_public_items`; `#[derive(Debug)]` keeps the public struct debuggable.)

- [ ] **Step 2: Add the `State` handler**

In `examples/app-demo/crates/app-demo-core/src/handlers.rs`, add `State` to the existing `edgezero_core::extractor::{…}` import and `use std::sync::Arc;`, then add the handler (`crate::DemoState` is a 2-segment path — fine under `absolute_paths`). The return type is `Result<Text<String>, EdgeError>` to match the file's other text handlers (e.g. `secrets_echo`); `#[action]` wraps it via `Responder`:

```rust
#[action]
pub async fn state_demo(
    State(state): State<Arc<crate::DemoState>>,
) -> Result<Text<String>, EdgeError> {
    Ok(Text::new(state.greeting.clone()))
}
```

(If `handlers.rs` already imports `Arc` / an `action` alias, reuse those rather than re-importing — keep the file's existing style.)

- [ ] **Step 3: Register the route in the manifest**

In `examples/app-demo/edgezero.toml`, add an HTTP trigger for the handler, matching the existing `[[triggers.http]]` entries' exact keys (`id`, `path`, `methods`, `handler`, `adapters`):

```toml
[[triggers.http]]
id = "state-demo"
path = "/state-demo"
methods = ["GET"]
handler = "app_demo_core::handlers::state_demo"
adapters = ["axum", "cloudflare", "fastly", "spin"]
description = "Reads app-owned state via State<Arc<DemoState>> (app!(state = ...))"
```

- [ ] **Step 4: Write the end-to-end host test**

Add to the `#[cfg(test)] mod tests` in `examples/app-demo/crates/app-demo-core/src/handlers.rs` (the module that already contains the `crate::build_router()` test at `handlers.rs:436`; reuse its imports for `request_builder`/`Body`/`block_on` — mirror that test). Place the fn alphabetically.

```rust
    #[test]
    fn state_demo_handler_reads_app_state_through_macro_router() {
        use edgezero_core::body::Body;
        use edgezero_core::http::{request_builder, Method, StatusCode};
        use futures::executor::block_on;

        // build_router() is macro-generated and now calls `.with_state(crate::app_state())`.
        let service = crate::build_router();

        let request = request_builder()
            .method(Method::GET)
            .uri("/state-demo")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.body().as_bytes().expect("buffered"),
            b"hello from app state"
        );
    }
```

- [ ] **Step 5: Run the e2e test**

Run: `(cd examples/app-demo && cargo test -p app-demo-core state_demo_handler_reads_app_state 2>&1 | tail -12)`
Expected: PASS — the macro-generated router injected `Arc<DemoState>`, the `State` extractor resolved it, and the handler echoed `greeting`. (First run may FAIL if the route/handler/state wiring is incomplete; fix until green.)

- [ ] **Step 6: Full app-demo + workspace verification**

Run: `(cd examples/app-demo && cargo test 2>&1 | tail -8)`
Expected: PASS (all four adapter example crates still build; app-demo-core tests green).
Run: `cargo test -p edgezero-macros 2>&1 | tail -5`
Expected: macros tests green.
Run root clippy AND app-demo clippy separately — `examples/app-demo` is its **own workspace**, `exclude`d from the root workspace (`Cargo.toml:12`), so root `--workspace` does NOT lint it:
```
cargo clippy --workspace --all-targets --all-features -- -D warnings
(cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)
```
Expected: both clean (the new handler's fields are read by the assertion, so no `dead_code` suppression is needed).

- [ ] **Step 7: Commit**

```bash
git add examples/app-demo/crates/app-demo-core/src/lib.rs \
        examples/app-demo/crates/app-demo-core/src/handlers.rs \
        examples/app-demo/edgezero.toml
git commit -m "docs(app-demo): app!(state = ...) + State<T> handler example with end-to-end test"
```

---

## Final verification (all P0-D tasks)

- [ ] **Run every CI gate:**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
(cd examples/app-demo && cargo test)
# app-demo is its OWN workspace (root `exclude`s it), so lint it separately:
(cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)
```

Expected: all green.

## Acceptance criteria (spec §P0-D acceptance)

1. A macro app declaring `app!("...", state = <expr>)` can extract `State<T>` (where `T` is the expression's value type) in an `#[action]` handler — proven host-side via app-demo's macro-generated `build_router()` (Task 2). Because the router's dispatch injection is platform-neutral, this holds on all four adapters with no adapter change.
2. An app that provides no `state` is unaffected — `build_router()` is byte-identical without the argument (Task 1 Step 6); `State<T>` for an unregistered `T` still returns the existing 500.
3. `app-demo` gains a small `app!(..., state = ...)` + `State<T>` handler example (Task 2).
4. **No `edgezero-core` `Hooks`/adapter change** — the diff is confined to `edgezero-macros` + the `app-demo` example (the acceptance signal that the simpler, router-reusing design was taken).

## Self-review notes (spec coverage + type consistency)

- §P0-D "The gap" / "Design (revised)" → Task 1 (macro-only `.with_state` emission, reusing #306's `state_extensions`).
- §`app!` argument grammar (`state = <expr>` as `syn::Expr`, emitted verbatim; duplicate/unknown-key errors; coexistence with app ident + `owns_logging`) → Task 1, extending P0-C's parser. Field/arm names match P0-C's `AppArgs` (`app_ident`, `owns_logging`, `path`, `state`).
- §P0-D acceptance (macro app extracts `State<T>`; no-state unaffected; app-demo example; no adapter/Hooks change) → Task 2 + the "no adapter change" acceptance line.
- `state = <expr>` is an expression emitted verbatim (`state = crate::app_state()`, not a bare fn path) — consistent with Task 1's parser (`syn::Expr`) and the emitted `.with_state(#state_expr)`.
