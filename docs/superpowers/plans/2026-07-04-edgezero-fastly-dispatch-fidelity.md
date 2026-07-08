# EdgeZero P0-C — Fastly `run_app` Dispatch Fidelity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring Fastly's `run_app` to parity with hand-written custom dispatch: preserve multi-value response headers (`Set-Cookie`), let an app opt out of the adapter's logger init, and add a pre-dispatch hook that reads raw-`fastly::Request` signals (JA4 / H2 / client IP) into the core request's extensions.

**Architecture:** Three independent Fastly-adapter changes plus one cross-adapter `Hooks` method. C1 swaps `set_header`→`append_header` on the (fresh) response and fixes the proxy-response conversion to append per value. C2 adds a platform-neutral `Hooks::owns_logging()` that every adapter's entrypoint gates its logger init on, with an `app!(owns_logging = …)` macro argument. C3 adds `run_app_with_request_extensions` that runs an app closure against a scratch `Extensions` **before** request conversion, then `extend`s it into the core request.

**Tech Stack:** Rust 1.95, edition 2021. `edgezero-adapter-fastly` (behind the `fastly` feature), `edgezero-core` (`Hooks` trait, `http::Extensions`), `edgezero-macros` (`app!`). `http` crate `HeaderMap`/`Extensions` via `edgezero_core::http`.

**Source spec:** `docs/superpowers/specs/2026-07-03-edgezero-fastly-dispatch-and-appstate-design.md` (P0-C), verified against `65afbd3`.

## Global Constraints

- **Rust 1.95.0**, edition 2021.
- **Strict clippy gate** — `[workspace.lints.clippy] restriction = { level = "deny" }`. Every restriction lint is an ERROR. The ones that bite here:
  - `missing_trait_methods` — an `impl Trait` may not inherit a defaulted method. Adding `Hooks::owns_logging()` forces the **macro-emitted** `impl Hooks` to emit it explicitly (the two in-file `Hooks` test stubs already carry `#[expect(clippy::missing_trait_methods)]`, so they need no change).
  - `arbitrary_source_item_ordering` — module items / struct fields / enum variants must be alphabetical; place new test fns and struct fields in the correct position, don't append.
  - `min_ident_chars` — no single-char identifiers.
  - `absolute_paths` — import types; don't inline 3-segment paths.
  - `impl_trait_in_params` — use named generics, not `impl Trait` params.
  - `assertions_on_result_states` — in tests use `.unwrap()/.unwrap_err()/.expect()` or `assert_eq!`, not `assert!(x.is_ok())`.
  - `needless_raw_strings` — plain string unless it needs `"`/`#`.
- **Test targets/features (CRITICAL — the fastly adapter is a wasm-only crate; verified empirically):**
  - `--features fastly` pulls in `libfastly`, which references undefined Compute@Edge hostcall symbols on the host. **`cargo test -p edgezero-adapter-fastly --features fastly` from the workspace root FAILS TO LINK** (`ld: symbol(s) not found`). Do not use it.
  - The crate ships a per-package `crates/edgezero-adapter-fastly/.cargo/config.toml` that sets `build.target = "wasm32-wasip1"` and a **Viceroy** runner — **but only when cargo is invoked from inside the crate directory.** So run all fastly-adapter tests as:
    ```
    (cd crates/edgezero-adapter-fastly && cargo test --features fastly --lib <filter>)
    ```
    This builds to `wasm32-wasip1` and runs the test binary under Viceroy (0.17.0, pinned in `.tool-versions`), which provides the hostcalls — so `FastlyResponse::from_status`/`append_header`/`get_header_all`, `FastlyRequest::new`/`get_url_str`, and even `get_client_ip_addr` all work at runtime. **Every `cargo test -p edgezero-adapter-fastly --features fastly …` command in the tasks below MUST be run in this `(cd crates/edgezero-adapter-fastly && cargo test --features fastly …)` form** — the commands are written the short way for brevity.
  - `crates/edgezero-adapter-fastly/tests/contract.rs` is `#![cfg(all(feature = "fastly", target_arch = "wasm32"))]` (the same wasm/Viceroy path). CI runs it via `cargo test -p edgezero-adapter-fastly --features fastly --target wasm32-wasip1 --test contract`.
  - `cargo clippy`/`cargo check --features fastly` type-check on the **host** (no link), so clippy runs from the workspace root as usual.
- **CI gates (all must pass):**
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  3. `cargo test --workspace --all-targets`
  4. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
  5. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- **No backward-compat constraint** — prefer the cleanest breaking change; update every in-tree call site in the same PR.
- **Shared with P0-D:** Task 4 reworks the `app!` `AppArgs` grammar into keyword arguments (adding `owns_logging`). The sibling P0-D plan (`2026-07-04-edgezero-app-macro-state.md`) **extends that same grammar** with a `state` key — execute this P0-C plan first so P0-D builds on the keyword-arg framework.

---

## File Structure

| File | Responsibility | Task |
| ---- | -------------- | ---- |
| `crates/edgezero-adapter-fastly/src/response.rs` | `from_core_response`: `set_header`→`append_header` + host test | 1 |
| `crates/edgezero-adapter-fastly/src/proxy.rs` | `convert_response`: append per value; `build_fastly_request` request-side note + host tests | 2 |
| `crates/edgezero-core/src/app.rs` | `Hooks::owns_logging()` default + trait test | 3 |
| `crates/edgezero-macros/src/app.rs` | emit `fn owns_logging()`; then (Task 4) rework `AppArgs` to keyword grammar + `owns_logging =` | 3, 4 |
| `crates/edgezero-adapter-{fastly,cloudflare,axum,spin}/src/…` | gate each logger init on `!A::owns_logging()` | 3 |
| `crates/edgezero-adapter-fastly/src/lib.rs` | `run_app_with_request_extensions` + gate logger | 3, 5 |
| `crates/edgezero-adapter-fastly/src/request.rs` | thread the pre-dispatch closure; scratch-`Extensions`-then-`extend` around `into_core_request` + host test | 5 |

---

## Task 1: C1 — multi-value response headers in `from_core_response`

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/response.rs` (`from_core_response` at `response.rs:28-30`; add a host test in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `from_core_response(response: edgezero_core::http::Response) -> Result<fastly::Response, EdgeError>` (existing); `response_builder()`, `Body` (test-module imports already present).
- Produces: unchanged signature; behavior now preserves duplicate header values.

- [x] **Step 1: Write the failing host test**

Add to the `#[cfg(test)] mod tests` in `crates/edgezero-adapter-fastly/src/response.rs`, placed in alphabetical position among the test fns (before `stream_body_is_written_to_fastly_response`). The module already imports `super::*`, `Body`, `response_builder`.

```rust
    #[test]
    fn multi_value_set_cookie_survives_conversion() {
        // http::response::Builder::header APPENDS, so this is two Set-Cookie values.
        let response = response_builder()
            .status(200)
            .header("set-cookie", "a=1")
            .header("set-cookie", "b=2")
            .body(Body::empty())
            .expect("response");

        let fastly_response = from_core_response(response).expect("fastly response");

        let cookies: Vec<String> = fastly_response
            .get_header_all("set-cookie")
            .map(|value| value.to_str().expect("utf8").to_owned())
            .collect();
        assert_eq!(cookies, vec!["a=1".to_owned(), "b=2".to_owned()]);
    }
```

- [x] **Step 2: Run it — verify it fails**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib multi_value_set_cookie 2>&1 | tail -20`
Expected: FAIL — the collected `cookies` is `["b=2"]` (the loop's `set_header` replaced the first value).

- [x] **Step 3: Swap `set_header` → `append_header`**

In `crates/edgezero-adapter-fastly/src/response.rs`, change the header loop (`response.rs:28-30`):

```rust
    for (name, value) in &parts.headers {
        fastly_response.set_header(name.as_str(), value.as_bytes());
    }
```

to:

```rust
    // `append_header` preserves multi-value headers (e.g. N `Set-Cookie`). The
    // response starts empty (`from_status`) and `http::HeaderMap` iteration
    // yields one entry per value, so appending is unconditionally correct.
    for (name, value) in &parts.headers {
        fastly_response.append_header(name.as_str(), value.as_bytes());
    }
```

- [x] **Step 4: Run it — verify it passes**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib multi_value_set_cookie 2>&1 | tail -8`
Expected: PASS. Also run the whole module: `cargo test -p edgezero-adapter-fastly --features fastly --lib response 2>&1 | tail -8` → all pass.

- [x] **Step 5: Lint**

Run: `cargo clippy -p edgezero-adapter-fastly --all-targets --features fastly -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [x] **Step 6: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/response.rs
git commit -m "fix(fastly): preserve multi-value response headers (Set-Cookie) in from_core_response"
```

---

## Task 2: C1 — multi-value headers in the proxy response conversion

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/proxy.rs` (`convert_response` at `proxy.rs:67-71`; `build_fastly_request` at `proxy.rs:53`; add host tests)

**Interfaces:**
- Consumes: `convert_response(fastly_response: &mut fastly::Response) -> edgezero_core::proxy::ProxyResponse` (existing, private); `HeaderName` from `edgezero_core::http`.
- Produces: `convert_response` preserving duplicate origin response headers.

- [x] **Step 1: Write the failing host test**

Add to the `#[cfg(test)] mod tests` in `crates/edgezero-adapter-fastly/src/proxy.rs` (module already imports `super::*`, `block_on`). Place alphabetically among the existing `stream_handles_*` tests (before `stream_handles_brotli`).

```rust
    #[test]
    fn convert_response_preserves_multi_value_set_cookie() {
        let mut fastly_response = FastlyResponse::from_status(200);
        fastly_response.append_header("set-cookie", "a=1");
        fastly_response.append_header("set-cookie", "b=2");

        let proxy_response = convert_response(&mut fastly_response);

        let cookies: Vec<String> = proxy_response
            .headers()
            .get_all("set-cookie")
            .into_iter()
            .map(|value| value.to_str().expect("utf8").to_owned())
            .collect();
        assert_eq!(cookies, vec!["a=1".to_owned(), "b=2".to_owned()]);
    }
```

- [x] **Step 2: Run it — verify it fails**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib convert_response_preserves 2>&1 | tail -20`
Expected: FAIL — `cookies` is `["a=1"]` (or one value): `get_header` returns the first value and `HeaderMap::insert` replaces.

- [x] **Step 3: Fix `convert_response` to append every value**

In `crates/edgezero-adapter-fastly/src/proxy.rs`, change the header loop (`proxy.rs:67-71`):

```rust
    for header in fastly_response.get_header_names() {
        if let Some(value) = fastly_response.get_header(header) {
            proxy_response.headers_mut().insert(header, value.clone());
        }
    }
```

to:

```rust
    // Preserve multi-value ORIGIN response headers (e.g. Set-Cookie): read ALL
    // values per name and append, instead of first-value + insert (which
    // replaced). `get_header_names()` yields `&HeaderName`, usable for both
    // `get_header_all` and `append`.
    for name in fastly_response.get_header_names() {
        for value in fastly_response.get_header_all(name) {
            proxy_response.headers_mut().append(name, value.clone());
        }
    }
```

(If the installed `fastly` SDK's `get_header_names()` yields owned `HeaderName` rather than `&HeaderName`, bind `for name in …` then call `get_header_all(&name)` / `append(&name, …)`. Confirm by the compiler; behavior is identical.)

- [x] **Step 4: Run it — verify it passes**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib convert_response_preserves 2>&1 | tail -8`
Expected: PASS. Run the module: `cargo test -p edgezero-adapter-fastly --features fastly --lib proxy 2>&1 | tail -8` → all pass.

- [x] **Step 5: Request-side audit — switch to `append_header` for consistency**

Origin-bound request duplicate headers are rare, but for consistency and to remove the same class of latent bug, change the request-build loop in `build_fastly_request` (`proxy.rs:53`):

```rust
        fastly_request.set_header(name.as_str(), value.clone());
```

to:

```rust
        fastly_request.append_header(name.as_str(), value.clone());
```

Leave the explicit `Host` line (`proxy.rs:57`) as `set_header` — it is a single computed value that must replace, not append. Add a one-line comment above the loop:

```rust
    // Append (not set) so a multi-value client header survives; `Host` below is
    // set explicitly as a single value.
```

- [x] **Step 6: Run + lint**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib proxy 2>&1 | tail -8`
Expected: PASS.
Run: `cargo clippy -p edgezero-adapter-fastly --all-targets --features fastly -- -D warnings 2>&1 | tail -5`
Expected: clean.

- [x] **Step 7: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/proxy.rs
git commit -m "fix(fastly): preserve multi-value headers in proxy response/request conversion"
```

---

## Task 3: C2 — `Hooks::owns_logging()` + gate every adapter's logger init

This task adds the trait method and wires it everywhere **atomically** — because adding a defaulted `Hooks` method breaks the macro-emitted impl under `missing_trait_methods` until the macro emits it. The `app!(owns_logging = …)` *argument* (grammar rework) is Task 4; here the macro emits a hardcoded `false`.

**Files:**
- Modify: `crates/edgezero-core/src/app.rs` (add `owns_logging()` to `Hooks`, `app.rs:104-143`; add a trait test)
- Modify: `crates/edgezero-macros/src/app.rs` (emit `fn owns_logging() -> bool { false }` in the `Hooks` impl, `app.rs:165-183`)
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs` (`run_app:117`, `run_app_with_config:205-208`)
- Modify: `crates/edgezero-adapter-cloudflare/src/lib.rs` (`run_app:105`)
- Modify: `crates/edgezero-adapter-axum/src/dev_server.rs` (`run_app:343`)
- Modify: `crates/edgezero-adapter-spin/src/lib.rs` (`run_app:115`)

**Interfaces:**
- Produces: `Hooks::owns_logging() -> bool` (default `false`). Consumed by all four `run_app` entrypoints and by Task 4 (macro argument).

- [x] **Step 1: Write the failing trait test**

Add to the `#[cfg(test)] mod tests` in `crates/edgezero-core/src/app.rs`, placed alphabetically among the existing test fns. `DefaultHooks` (defined in that module, `app.rs:163`) overrides only `routes`/`stores`, so it should report the default.

```rust
    #[test]
    fn default_hooks_do_not_own_logging() {
        assert!(!DefaultHooks::owns_logging());
    }
```

- [x] **Step 2: Run it — verify it fails**

Run: `cargo test -p edgezero-core --lib default_hooks_do_not_own_logging 2>&1 | tail -15`
Expected: FAIL — `no method named owns_logging found`.

- [x] **Step 3: Add `owns_logging()` to the `Hooks` trait**

In `crates/edgezero-core/src/app.rs`, add to the `Hooks` trait. **`arbitrary_source_item_ordering` (restriction = deny) enforces alphabetical trait methods**, so place `owns_logging` between `name` and `routes` (order: `build_app`, `configure`, `name`, `owns_logging`, `routes`, `stores`) — not adjacent to `configure`:

```rust
    /// When `true`, an adapter's `run_app` skips its own logger initialization;
    /// the app is responsible for installing a `log` backend. Default `false`.
    #[must_use]
    #[inline]
    fn owns_logging() -> bool {
        false
    }
```

- [x] **Step 4: Emit `owns_logging` from the `app!` macro**

In `crates/edgezero-macros/src/app.rs`, add to the emitted `impl edgezero_core::app::Hooks` block (`app.rs:165-183`) — after `configure`, mirroring the explicit-defaults pattern the file already documents at `app.rs:154-158`:

```rust
            fn configure(_app: &mut edgezero_core::app::App) {}

            fn owns_logging() -> bool {
                false
            }
```

Update the `missing_trait_methods` comment at `app.rs:154-158` to include `owns_logging` in the list of explicitly-emitted defaults:

```rust
    // The emitted `Hooks` impl below explicitly defines `configure`,
    // `owns_logging`, and `build_app` even though their bodies mirror the trait
    // defaults. This is required because `missing_trait_methods` (restriction =
    // deny) forbids relying on trait defaults in the impl. If those Hooks
    // defaults change, update these emitted bodies to match.
```

- [x] **Step 5: Gate the four adapter logger-init sites**

Each adapter's `run_app` (and Fastly's `run_app_with_config`) must skip its logger init when `A::owns_logging()`:

**Fastly** — `crates/edgezero-adapter-fastly/src/lib.rs`, `run_app` (`lib.rs:117`):
```rust
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
```
and the identical block in `run_app_with_config` (`lib.rs:205-208`) — same `&& !A::owns_logging()`.

**Cloudflare** — `crates/edgezero-adapter-cloudflare/src/lib.rs`, `run_app` (`lib.rs:105`):
```rust
    if !A::owns_logging() {
        drop(init_logger());
    }
```

**Axum** — `crates/edgezero-adapter-axum/src/dev_server.rs`, `run_app` (`dev_server.rs:343`):
```rust
    if !A::owns_logging() {
        let _logger_init = SimpleLogger::new().with_level(level).init();
    }
```

**Spin** — `crates/edgezero-adapter-spin/src/lib.rs`, `run_app` (`lib.rs:115`) — Spin's `init_logger` is a no-op, but gate it so the neutral flag is honest everywhere:
```rust
    if !A::owns_logging() {
        drop(init_logger());
    }
```

- [x] **Step 6: Run the trait test + workspace build**

Run: `cargo test -p edgezero-core --lib default_hooks_do_not_own_logging 2>&1 | tail -8`
Expected: PASS.
Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin" 2>&1 | tail -5`
Expected: succeeds (macro emission satisfies `missing_trait_methods`; all four adapters compile).

- [x] **Step 7: Lint + WASM checks**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -5`
Expected: clean (the two `Hooks` test stubs already carry `#[expect(clippy::missing_trait_methods)]`, so the new defaulted method needs no change there).
Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin 2>&1 | tail -3`
Expected: succeeds.

- [x] **Step 8: Commit**

```bash
git add crates/edgezero-core/src/app.rs crates/edgezero-macros/src/app.rs \
        crates/edgezero-adapter-fastly/src/lib.rs \
        crates/edgezero-adapter-cloudflare/src/lib.rs \
        crates/edgezero-adapter-axum/src/dev_server.rs \
        crates/edgezero-adapter-spin/src/lib.rs
git commit -m "feat: Hooks::owns_logging() opt-out gated in all four adapter run_app entrypoints"
```

---

## Task 4: C2 — `app!(owns_logging = <bool>)` argument (keyword-arg grammar)

Rework `AppArgs` from "path + optional ident" into the keyword-argument grammar the spec defines (§`app! argument grammar`), implementing the `owns_logging` key. This is the **shared grammar framework** the P0-D `state` key extends.

**Files:**
- Modify: `crates/edgezero-macros/src/app.rs` (`AppArgs` struct + `impl Parse`, `app.rs:12-31`; emit `fn owns_logging() -> bool { #bool }`, `app.rs:170`; add `AppArgs` unit tests to the `#[cfg(test)] mod tests`)
- Create: `crates/edgezero-macros/tests/app_macro.rs` (real-`app!` integration test asserting the emitted `owns_logging()`)
- Create: `crates/edgezero-macros/tests/fixtures/owns_logging.toml` (minimal fixture manifest)

**Interfaces:**
- Consumes: `syn::{LitStr, Ident, LitBool, Token}`, `syn::parse::{Parse, ParseStream}`.
- Produces: `struct AppArgs { path: LitStr, app_ident: Option<Ident>, owns_logging: Option<bool> }` with a keyword-arg parser. **P0-D adds `state: Option<syn::Expr>` to this same struct and parser.**

- [x] **Step 1: Write the failing `AppArgs` unit tests**

Add a focused unit-test module for the parser. Put it in the existing `#[cfg(test)] mod tests` in `crates/edgezero-macros/src/app.rs` (which currently imports only `parse_handler_path`). Add `use super::AppArgs;` and `use syn::parse_str;`. Tests (place alphabetically among the existing `parse_handler_path_*` fns):

```rust
    #[test]
    fn app_args_parses_path_only() {
        let args: AppArgs = parse_str(r#""edgezero.toml""#).expect("parse");
        assert_eq!(args.path.value(), "edgezero.toml");
        assert!(args.app_ident.is_none());
        assert_eq!(args.owns_logging, None);
    }

    #[test]
    fn app_args_parses_path_and_app_ident() {
        let args: AppArgs = parse_str(r#""edgezero.toml", MyApp"#).expect("parse");
        assert_eq!(args.app_ident.map(|ident| ident.to_string()), Some("MyApp".to_owned()));
        assert_eq!(args.owns_logging, None);
    }

    #[test]
    fn app_args_parses_owns_logging_true() {
        let args: AppArgs = parse_str(r#""edgezero.toml", owns_logging = true"#).expect("parse");
        assert_eq!(args.owns_logging, Some(true));
        assert!(args.app_ident.is_none());
    }

    #[test]
    fn app_args_parses_app_ident_then_keyword() {
        let args: AppArgs =
            parse_str(r#""edgezero.toml", MyApp, owns_logging = false"#).expect("parse");
        assert_eq!(args.app_ident.map(|ident| ident.to_string()), Some("MyApp".to_owned()));
        assert_eq!(args.owns_logging, Some(false));
    }

    #[test]
    fn app_args_rejects_unknown_key() {
        let err = parse_str::<AppArgs>(r#""edgezero.toml", bogus = true"#).expect_err("unknown key");
        assert!(err.to_string().contains("unknown `app!` argument `bogus`"), "got: {err}");
    }

    #[test]
    fn app_args_rejects_duplicate_key() {
        let err = parse_str::<AppArgs>(r#""edgezero.toml", owns_logging = true, owns_logging = false"#)
            .expect_err("duplicate");
        assert!(err.to_string().contains("duplicate `owns_logging`"), "got: {err}");
    }

    #[test]
    fn app_args_rejects_ident_after_keyword() {
        let err = parse_str::<AppArgs>(r#""edgezero.toml", owns_logging = true, MyApp"#)
            .expect_err("ident after keyword");
        assert!(
            err.to_string().contains("must come immediately after the manifest path"),
            "got: {err}"
        );
    }
```

- [x] **Step 2: Run — verify they fail**

Run: `cargo test -p edgezero-macros --lib app_args_ 2>&1 | tail -20`
Expected: FAIL — the current `AppArgs` has no `owns_logging` field and rejects `owns_logging = true` with "unexpected tokens".

- [x] **Step 3: Rework `AppArgs` + `impl Parse`**

Replace `crates/edgezero-macros/src/app.rs:12-31` with:

```rust
// `#[derive(Debug)]` is required: the `app_args_rejects_*` unit tests use
// `.expect_err(..)`, whose `Ok` arm (`AppArgs`) must be `Debug`.
#[derive(Debug)]
struct AppArgs {
    app_ident: Option<Ident>,
    owns_logging: Option<bool>,
    path: LitStr,
}

impl Parse for AppArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let path: LitStr = input.parse()?;
        let mut app_ident: Option<Ident> = None;
        let mut owns_logging: Option<bool> = None;
        let mut seen_keyword = false;

        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;

            // Keyword argument: `Ident = Value`.
            if input.peek(Ident) && input.peek2(Token![=]) {
                let key: Ident = input.parse()?;
                input.parse::<Token![=]>()?;
                seen_keyword = true;
                match key.to_string().as_str() {
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
                            format!("unknown `app!` argument `{other}`; expected `owns_logging`"),
                        ));
                    }
                }
                continue;
            }

            // Bare identifier: the optional custom App type name, only before keywords.
            if input.peek(Ident) {
                if seen_keyword || app_ident.is_some() {
                    return Err(input.error(
                        "the custom App identifier must come immediately after the manifest path, before keyword arguments",
                    ));
                }
                app_ident = Some(input.parse::<Ident>()?);
                continue;
            }

            return Err(input.error("expected a custom App identifier or `key = value` argument"));
        }

        if !input.is_empty() {
            return Err(input.error("unexpected tokens after app! macro arguments"));
        }
        Ok(Self { app_ident, owns_logging, path })
    }
}
```

Add `LitBool` isn't needed as an import (used as `syn::LitBool`); ensure `Ident`, `LitStr`, `Token`, `Parse`, `ParseStream` are already imported at the top of the file (they are — `use syn::{... Ident, LitStr, Token};` and `use syn::parse::{Parse, ParseStream};`).

> **Note for P0-D:** the `match key.to_string()` arm is the extension point — P0-D adds a `"state" => { … }` arm and a `state: Option<syn::Expr>` field. The "expected `owns_logging`" message becomes "expected `state` or `owns_logging`" then.

- [x] **Step 4: Emit the parsed `owns_logging` value**

In `expand_app`, before the `quote!` block, compute the bool literal (default `false`). Near the other `let …_lit` bindings (around `app.rs:130-152`):

```rust
    let owns_logging_lit = args.owns_logging.unwrap_or(false);
```

Change the emitted `fn owns_logging()` (added in Task 3, currently `{ false }`) to use it:

```rust
            fn owns_logging() -> bool {
                #owns_logging_lit
            }
```

(`bool` implements `quote::ToTokens`, so `#owns_logging_lit` emits `true`/`false`.)

- [x] **Step 5: Run the unit tests — verify they pass**

Run: `cargo test -p edgezero-macros --lib app_args_ 2>&1 | tail -12`
Expected: PASS — all seven `app_args_*` tests.

- [x] **Step 6: Add a real-`app!` macro-emission integration test (spec requirement)**

The spec's C2 acceptance requires proving the macro *emits* `owns_logging() == true` for `app!(…, owns_logging = true)` — not just that the grammar parses. `edgezero-macros` dev-depends on `edgezero-core` (`crates/edgezero-macros/Cargo.toml:31`) which re-exports `app!` (`crates/edgezero-core/src/lib.rs:42`), and the macro resolves the manifest path against the invoking crate's `CARGO_MANIFEST_DIR` (`app.rs:243`), so an integration test can invoke `app!` with a checked-in fixture manifest.

First create a minimal fixture manifest, `crates/edgezero-macros/tests/fixtures/owns_logging.toml`:

```toml
[app]
name = "owns-logging-fixture"
```

Then create the integration test `crates/edgezero-macros/tests/app_macro.rs`:

```rust
//! Integration coverage: `app!(..., owns_logging = true)` emits a `Hooks` impl
//! whose `owns_logging()` returns `true`. The manifest path resolves against
//! this crate's `CARGO_MANIFEST_DIR` (backticks required — `doc_markdown`), so
//! the fixture is `tests/fixtures/...`.

// The macro emits `pub struct OwnedLoggingApp;`, a `Hooks` impl, and a free
// `build_router()` at this module scope.
edgezero_core::app!("tests/fixtures/owns_logging.toml", OwnedLoggingApp, owns_logging = true);

// `#[test]` must live in a `#[cfg(test)] mod tests` (the `tests_outside_test_module`
// restriction lint), and `Hooks` must be imported (not a 3-segment path — `absolute_paths`).
#[cfg(test)]
mod tests {
    use edgezero_core::app::Hooks as _;

    #[test]
    fn app_macro_emits_owns_logging_true() {
        assert!(super::OwnedLoggingApp::owns_logging());
    }
}
```

Run: `cargo test -p edgezero-macros --test app_macro 2>&1 | tail -12`
Expected: PASS — proves the macro emitted `fn owns_logging() -> bool { true }`. (Only one `app!` per test file — it emits a free `build_router()` that would collide across invocations; the default `owns_logging() == false` path is covered by app-demo below.)

- [x] **Step 7: Verify app-demo (real `app!`, no keyword args) still emits owns_logging=false**

app-demo's `app!("../../edgezero.toml")` (`examples/app-demo/crates/app-demo-core/src/lib.rs:11`) uses no keyword args, so its generated `owns_logging()` returns `false`.

Run: `(cd examples/app-demo && cargo test -p app-demo-core 2>&1 | tail -5)`
Expected: PASS. (Do NOT add a process-global logger "call run_app twice" test — the macro-emission test above + the gate code review are the deterministic coverage.)

- [x] **Step 8: Lint + commit**

Run: `cargo clippy -p edgezero-macros --all-targets --all-features -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add crates/edgezero-macros/src/app.rs \
        crates/edgezero-macros/tests/app_macro.rs \
        crates/edgezero-macros/tests/fixtures/owns_logging.toml
git commit -m "feat(macros): app!(owns_logging = <bool>) keyword argument + AppArgs keyword grammar"
```

---

## Task 5: C3 — pre-dispatch raw-request hook (`run_app_with_request_extensions`)

Add a Fastly `run_app` variant taking an app closure that reads the raw `fastly::Request` (JA4 / H2 / etc.) into a scratch `Extensions` **before** `into_core_request` consumes the request; the scratch bag is `extend`ed into the core request after conversion. `run_app` becomes the no-hook wrapper.

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/request.rs` (thread the closure through `dispatch_with_registries`/`dispatch_with_handles`; scratch-`extend` around `into_core_request` at `request.rs:284`; update the OTHER `dispatch_with_handles` caller `FastlyService::dispatch` at `request.rs:145` to pass a no-op; add a host unit test)
- Modify: `crates/edgezero-adapter-fastly/src/lib.rs` (`run_app_with_request_extensions`; `run_app` delegates to it with a no-op). `run_app_with_config` is unaffected by the closure threading — it dispatches via `FastlyService::dispatch` (updated in Step 3), not `dispatch_with_registries`.

**Interfaces:**
- Consumes: `into_core_request(req: FastlyRequest) -> Result<Request, EdgeError>` (`request.rs:445`, consumes `req` by value); `edgezero_core::http::Extensions`.
- Produces:
  - `request.rs`: `dispatch_with_registries<F>(app, req, config_meta, kv_meta, secret_meta, env, extend: F) where F: FnOnce(&FastlyRequest, &mut Extensions)` — an added final `extend` parameter; `dispatch_with_handles<F>` likewise.
  - `lib.rs`: `pub fn run_app_with_request_extensions<A, F>(req: fastly::Request, extend: F) -> Result<fastly::Response, fastly::Error> where A: Hooks, F: FnOnce(&fastly::Request, &mut Extensions)`.

- [x] **Step 1: Write the failing host unit test (the scratch-bag mechanism)**

Unit-test the closure/scratch-bag seam directly via a small extracted helper. (This runs under Viceroy from the crate dir, per Global Constraints; the *full* `run_app_with_request_extensions` → `into_core_request` → handler integration is best covered by a `contract.rs` test — see the note after the handler-visible test.)

Add to the `#[cfg(test)] mod synthesis_tests` in `crates/edgezero-adapter-fastly/src/request.rs`. It builds a `FastlyRequest` and asserts the closure populated the scratch bag:

```rust
    #[test]
    fn apply_request_extend_populates_scratch_from_raw_request() {
        use edgezero_core::http::Extensions;

        #[derive(Clone, Debug, PartialEq)]
        struct Ja4(String);

        let raw = FastlyRequest::new(fastly::http::Method::GET, "http://example.test/");
        let scratch = apply_request_extend(&raw, |req, extensions| {
            // A real closure would call req.get_tls_ja4(); here we derive from a
            // host-safe signal (the URL) to avoid a hostcall in the unit test.
            let marker = req.get_url_str().to_owned();
            extensions.insert(Ja4(marker));
        });

        assert_eq!(
            scratch.get::<Ja4>(),
            Some(&Ja4("http://example.test/".to_owned()))
        );
    }
```

Also add a **handler-visible** host test — this proves the spec's requirement (§128) that a value stashed by the pre-dispatch hook is readable by a handler. It exercises the second half of the C3 chain host-side (the scratch bag `extend`ed into a core request → dispatched → handler reads it), which `dispatch_with_handles` can't be host-tested through because `into_core_request` calls the `get_client_ip_addr()` hostcall. Add alongside the test above:

```rust
    #[test]
    fn extended_request_extensions_are_visible_to_handler() {
        use edgezero_core::body::Body;
        use edgezero_core::context::RequestContext;
        use edgezero_core::error::EdgeError;
        use edgezero_core::http::{request_builder, Extensions, Method, StatusCode};
        use edgezero_core::router::RouterService;
        use futures::executor::block_on;

        #[derive(Clone)]
        struct Ja4(String);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let ja4 = ctx
                .request()
                .extensions()
                .get::<Ja4>()
                .map_or_else(|| "missing".to_owned(), |value| value.0.clone());
            Ok(ja4)
        }

        // Mirror what `dispatch_with_handles` does: a scratch bag built from the
        // raw request is `extend`ed into the core request before dispatch.
        let mut scratch = Extensions::default();
        scratch.insert(Ja4("t13d1516h2".to_owned()));

        let mut request = request_builder()
            .method(Method::GET)
            .uri("/ja4")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().extend(scratch);

        let service = RouterService::builder().get("/ja4", handler).build();
        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"t13d1516h2");
    }
```

> **Complete-path coverage (wasm/Viceroy, optional in this plan):** the *full* raw-`fastly::Request` → `run_app_with_request_extensions` → `into_core_request` → handler path can only run under Viceroy (`into_core_request`'s `get_client_ip_addr()` hostcall). If a Viceroy toolchain is available, add a test to `crates/edgezero-adapter-fastly/tests/contract.rs` (already `#![cfg(all(feature = "fastly", target_arch = "wasm32"))]`) that dispatches a request through `run_app_with_request_extensions::<TestApp, _>(req, |raw, ext| ext.insert(Ja4(raw.get_url_str().into())))` and asserts a handler read the value. Mark it clearly as wasm/Viceroy-only; it is not run by the host `cargo test` gate.

- [x] **Step 2: Run — verify they fail**

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib apply_request_extend 2>&1 | tail -15`
Expected: FAIL — `apply_request_extend` does not exist. (The `extended_request_extensions_are_visible_to_handler` test also compiles against the same feature set; it exercises only public core APIs and will pass once the crate builds — its value is documenting/locking the handler-visible contract.)

- [x] **Step 3: Add the `apply_request_extend` helper + thread the closure through dispatch**

In `crates/edgezero-adapter-fastly/src/request.rs`, add the helper near `dispatch_with_handles` (import `Extensions`: add `Extensions` to the existing `use edgezero_core::http::{request_builder, Request};` line at `request.rs:11`):

```rust
/// Run an app-provided closure against a scratch `Extensions` populated from the
/// RAW `fastly::Request` (JA4 / H2 / etc.), BEFORE `into_core_request` consumes
/// the request. Returns the scratch bag to be `extend`ed into the core request.
fn apply_request_extend<F>(req: &FastlyRequest, extend: F) -> Extensions
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let mut scratch = Extensions::default();
    extend(req, &mut scratch);
    scratch
}
```

Change `dispatch_with_handles` (`request.rs:279-286`) to take the closure and merge the scratch bag after conversion:

```rust
fn dispatch_with_handles<F>(
    app: &App,
    req: FastlyRequest,
    stores: Stores,
    extend: F,
) -> Result<FastlyResponse, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    // Read raw-request signals into a scratch bag BEFORE conversion consumes `req`.
    let scratch = apply_request_extend(&req, extend);
    let mut core_request = into_core_request(req).map_err(|err| map_edge_error(&err))?;
    core_request.extensions_mut().extend(scratch);
    dispatch_core_request(app, core_request, stores)
}
```

(`dispatch_core_request` is unchanged; it already takes `mut core_request` and inserts the registries + runs the router.)

**`dispatch_with_handles` has a SECOND caller** — `FastlyService::dispatch` (`request.rs:145`, the service-builder API used by the wasm/Viceroy contract tests). It calls `dispatch_with_handles` directly and will not compile after the signature change. Update that call site (`request.rs:145-153`) to pass a no-op closure:

```rust
        dispatch_with_handles(
            self.app,
            req,
            Stores {
                config_store,
                kv,
                secrets,
                ..Default::default()
            },
            |_req, _extensions| {},
        )
```

(`FastlyService` is the no-hook service API; it does not expose a raw-request hook, so a no-op is correct. If a service-level hook is ever wanted, add it as a separate change.)

Change `dispatch_with_registries` (`request.rs:288-316`) to take and forward the closure:

```rust
pub(crate) fn dispatch_with_registries<F>(
    app: &App,
    req: FastlyRequest,
    config_meta: Option<StoreMetadata>,
    kv_meta: Option<StoreMetadata>,
    secret_meta: Option<StoreMetadata>,
    env: &EnvConfig,
    extend: F,
) -> Result<FastlyResponse, FastlyError>
where
    F: FnOnce(&FastlyRequest, &mut Extensions),
{
    let kv_registry = build_kv_registry(kv_meta, env)?;
    let config_registry = build_config_registry(config_meta, env);
    let secret_registry = build_secret_registry(secret_meta, env);
    dispatch_with_handles(
        app,
        req,
        Stores {
            config_registry,
            kv_registry,
            secret_registry,
            ..Default::default()
        },
        extend,
    )
}
```

- [x] **Step 4: Add `run_app_with_request_extensions` + make `run_app` delegate**

In `crates/edgezero-adapter-fastly/src/lib.rs`, replace the `run_app` body's `dispatch_with_registries(...)` call (`lib.rs:122`) so `run_app` delegates to the new variant with a no-op closure, and add the new public fn. Import `Extensions`: add `use edgezero_core::http::Extensions;` near the other imports (guarded under the same `#[cfg(feature = "fastly")]` scope as `run_app`).

```rust
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app<A: Hooks>(req: fastly::Request) -> Result<fastly::Response, fastly::Error> {
    run_app_with_request_extensions::<A, _>(req, |_req, _extensions| {})
}

/// Like [`run_app`], but runs `extend` against a scratch [`Extensions`] populated
/// from the raw `fastly::Request` (TLS JA4, H2 fingerprint, client IP, …) before
/// the request is converted; the scratch values are merged into the core
/// request's extensions and are visible to middleware and the `State`/extractor
/// layer.
#[cfg(feature = "fastly")]
#[inline]
pub fn run_app_with_request_extensions<A, F>(
    req: fastly::Request,
    extend: F,
) -> Result<fastly::Response, fastly::Error>
where
    A: Hooks,
    F: FnOnce(&fastly::Request, &mut Extensions),
{
    let stores = A::stores();
    let env = env_config_from_runtime_dictionary(stores);
    let logging = logging_from_env(&env);
    if logging.use_fastly_logger && !A::owns_logging() {
        let endpoint = logging.endpoint.as_deref().unwrap_or("stdout");
        init_logger(endpoint, logging.level, logging.echo_stdout)?;
    }
    let app = A::build_app();
    request::dispatch_with_registries(
        &app,
        req,
        stores.config,
        stores.kv,
        stores.secrets,
        &env,
        extend,
    )
}
```

(The logger gate here is the Task-3 `&& !A::owns_logging()`. Note: `run_app_with_config` (`lib.rs:200`) does **not** call `dispatch_with_registries` — it builds a `FastlyService` and calls `service.dispatch(req)` (`lib.rs:210-214`), which routes through `FastlyService::dispatch` → `dispatch_with_handles`, already updated with a no-op in Step 3. So `run_app_with_config` needs no change here beyond its own Task-3 logger gate. The **only** direct caller of `dispatch_with_registries` is `run_app` — now delegating through `run_app_with_request_extensions`.)

- [x] **Step 5: Confirm every caller was updated, then run the host test + build**

`dispatch_with_handles` and `dispatch_with_registries` are internal to the fastly crate but each has multiple callers. Grep to confirm none was missed before building:

Run: `grep -rn "dispatch_with_handles\|dispatch_with_registries" crates/edgezero-adapter-fastly/src/`
Expected callers, all now passing an `extend` closure: `dispatch_with_handles` ← `FastlyService::dispatch` (`request.rs:145`, no-op) and `dispatch_with_registries` (`request.rs`); `dispatch_with_registries` ← only `run_app_with_request_extensions` (`lib.rs`; `run_app` delegates to it). `run_app_with_config` reaches dispatch via `FastlyService::dispatch`, so it needs no closure change. If grep shows any `dispatch_with_handles`/`dispatch_with_registries` call without a trailing closure arg, fix it.

Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib apply_request_extend 2>&1 | tail -8`
Expected: PASS.
Run: `cargo test -p edgezero-adapter-fastly --features fastly --lib 2>&1 | tail -8`
Expected: all host unit tests PASS (existing `synthesis_tests`, response, proxy, lib).
Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin" 2>&1 | tail -5`
Expected: succeeds (the signature changes are internal to the fastly crate; the two `dispatch_with_handles` callers — `FastlyService::dispatch` and `dispatch_with_registries` — and the one `dispatch_with_registries` caller, `run_app_with_request_extensions`, are all updated. `run_app_with_config` compiles unchanged: it dispatches via `FastlyService::dispatch`).

- [x] **Step 6: Lint + commit**

Run: `cargo clippy -p edgezero-adapter-fastly --all-targets --features fastly -- -D warnings 2>&1 | tail -5`
Expected: clean.

```bash
git add crates/edgezero-adapter-fastly/src/request.rs crates/edgezero-adapter-fastly/src/lib.rs
git commit -m "feat(fastly): run_app_with_request_extensions pre-dispatch hook for raw-request signals"
```

---

## Final verification (all P0-C tasks)

- [x] **Run every CI gate:**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
# fastly adapter is wasm-only: run its tests via Viceroy FROM THE CRATE DIR
# (root `cargo test -p edgezero-adapter-fastly --features fastly` fails to link).
# Use TARGETED module filters — a blanket `--lib` run aborts (exit 134) on
# pre-existing store/hostcall tests that need specific Viceroy backend config:
(cd crates/edgezero-adapter-fastly && cargo test --features fastly --lib -- response:: proxy::)
(cd crates/edgezero-adapter-fastly && cargo test --features fastly --lib -- synthesis apply_request_extend extended_request_extensions)
(cd crates/edgezero-adapter-fastly && cargo test --features fastly --target wasm32-wasip1 --test contract)  # if Viceroy present
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
(cd examples/app-demo && cargo test)
# app-demo is its OWN workspace (root `exclude`s it at Cargo.toml:12); the macro
# change regenerates its Hooks impl (owns_logging), so lint it separately:
(cd examples/app-demo && cargo clippy --workspace --all-targets --all-features -- -D warnings)
```

Expected: all green. Note: `crates/edgezero-adapter-fastly/tests/contract.rs` (the Viceroy/wasm end-to-end incl. the C3 closure-reaches-handler path and full header round-trips) is `#![cfg(all(feature = "fastly", target_arch = "wasm32"))]` and is **not** run by the above; if a Viceroy toolchain is available, run it separately with `--features fastly --target wasm32-wasip1`.

## Acceptance criteria (spec §P0-C acceptance)

1. Multi-value `Set-Cookie` round-trips through `from_core_response` (Task 1) and `convert_response` (Task 2).
2. `Hooks::owns_logging()` gates logger init in all four adapters (Task 3); `app!(owns_logging = true)` emits `owns_logging() -> bool { true }` (Task 4).
3. A pre-dispatch closure populates a scratch `Extensions` from the raw `fastly::Request` (Task 5), the scratch is merged into the core request, and the merged value is **handler-visible** (`extended_request_extensions_are_visible_to_handler`); `run_app` (no-hook) behavior is unchanged. The full raw-request→handler path is additionally covered by an optional wasm/Viceroy `contract.rs` test.
4. `cargo fmt`/clippy clean; workspace + app-demo tests green; WASM checks pass.

## Self-review notes (spec coverage)

- C1 §26-51 (response append; proxy request + response) → Tasks 1, 2.
- C2 §53-91 (`Hooks::owns_logging()` neutral across 4 adapters + `missing_trait_methods` emission + macro arg) → Tasks 3, 4.
- C3 §93-125 (scratch-`Extensions`-before-conversion, `edgezero_core::http::Extensions`, thread through dispatch) → Task 5.
- Test target/feature clarity (spec review): every test step names `--features fastly` for host coverage and flags `contract.rs` as wasm/Viceroy-only; C2 uses deterministic `AppArgs` grammar tests + the macro-emission check, not a process-global logger test.
