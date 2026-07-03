# EdgeZero `State<T>` Extractor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `State<T>` extractor plus `RouterBuilder::with_state<T>` so any EdgeZero app can hand app-owned shared state (typically `Arc<AppState>`) to `#[action]` extractor-style handlers.

**Architecture:** Mirror PR #300's introspection-injection mechanism. `RouterBuilder::with_state<T>` records a type-erased closure that clones the value into `request.extensions_mut()`. `RouterInner::dispatch` runs those closures on the owned request just before `RequestContext::new`, right after the existing introspection inserts. A new `State<T>: FromRequest` extractor reads the value back by type from request extensions — so `#[action]` composes it with zero macro changes.

**Tech Stack:** Rust 1.95, edition 2021. `edgezero-core` (WASM-compatible: `async-trait(?Send)`, no Tokio), `edgezero-macros` (proc macros), `http` crate via the `crate::http` facade only.

## Base branch

- **Implementation branch:** `worktree-state-nested-secrets-spec-review`.
- **Base:** **PR #300** ("pluggable introspection routes", branch `worktree-feature+introspection-routes`, head `2efa2da`) has already been **merged into this branch** (merge commit `051a9ad`). Every router line number below is from that merged tree and was verified live (`RouterBuilder` at `router.rs:71`, `build(self)` at `:110`, `with_manifest_json` at `:192`, `RouterInner` at `:198`, `dispatch(&self, mut request)` at `:206`, `RequestContext::new(request, params)` at `:227`, `RouterService::new` at `:297`). If the branch is later rebased and these drift, re-confirm before editing.
- This plan shares its branch with the sibling **nested `#[secret]`** plan (`2026-07-02-edgezero-nested-secrets.md`). The only file both touch is `crates/edgezero-core/src/extractor.rs`, in disjoint regions (this plan appends the `State<T>` extractor; the other rewrites `secret_walk`). Either order is safe.

## Global Constraints

- **Rust 1.95.0**, edition 2021, resolver 2 (from `.tool-versions` / root `Cargo.toml`).
- **WASM-compat:** no Tokio, no `std::time::Instant`; extractors use `#[async_trait(?Send)]`. Async tests use `futures::executor::block_on`, never Tokio.
- **HTTP facade:** never import from the `http` crate directly. Use `crate::http::{...}` (the `Extensions` alias is `crate::http::Extensions`, defined at `crates/edgezero-core/src/http.rs:25`).
- **Colocate tests** in `#[cfg(test)] mod tests` in the same file as the implementation.
- **CI gates (all must pass):**
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  3. `cargo test --workspace --all-targets`
  4. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
  5. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- **Naming decision (locked):** ship `State<T>` + `with_state` only. Do **not** add an `Extension<T>` alias or a `RequestContext::state::<T>()` accessor (YAGNI; trusted-server needs neither). No crate-root `pub use` of `State` — consumers reference `edgezero_core::extractor::State` (matches how every other extractor is reached today).

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `crates/edgezero-core/src/extractor.rs` | The `State<T>` extractor + `Deref`/`DerefMut`/`into_inner` + unit tests | Modify (append) |
| `crates/edgezero-core/src/router.rs` | `StateInserter` alias, `RouterBuilder::with_state`, thread `state_inserters` through `build()` → `RouterService::new` → `RouterInner`, apply in `dispatch` + router tests | Modify |
| `crates/edgezero-macros/tests/action_state.rs` | Integration test proving `#[action]` composes `State<T>` with `Query<T>` end-to-end | Create |
| `crates/edgezero-macros/Cargo.toml` | Add `futures` dev-dependency (for `block_on` in the integration test) | Modify |
| `docs/guide/handlers.md` | "Sharing app state" section | Modify (append) |

---

## Task 1: `State<T>` extractor

**Files:**
- Modify: `crates/edgezero-core/src/extractor.rs` (append extractor after the existing extractors; append tests inside the existing `#[cfg(test)] mod tests` at the end of the file)

**Interfaces:**
- Consumes: `crate::context::RequestContext` (has `pub(crate) fn extension<T>(&self) -> Option<T> where T: Clone + Send + Sync + 'static` at `context.rs:77`), `crate::error::EdgeError` (`EdgeError::internal(anyhow::Error) -> 500`; `err.status() -> StatusCode`), the `FromRequest` trait (`extractor.rs:21`), `std::ops::{Deref, DerefMut}` (already imported at `extractor.rs:1`).
- Produces: `pub struct State<T>(pub T)` with `impl<T: Clone + Send + Sync + 'static> FromRequest for State<T>`, plus `Deref`/`DerefMut`/`into_inner`. Consumed by Task 2 (router tests) and Task 3 (macro composition).

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)] mod tests` block at the end of `crates/edgezero-core/src/extractor.rs`. The module already imports `request_builder, Method, StatusCode` (from `crate::http`), `RequestContext`, `PathParams`, `Body`, `block_on`, and `std::sync::Arc`.

```rust
    #[derive(Clone, Debug, PartialEq)]
    struct AppStateFixture {
        name: String,
    }

    #[test]
    fn state_extractor_resolves_registered_value() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(Arc::new(AppStateFixture {
            name: "demo".to_owned(),
        }));
        let ctx = RequestContext::new(request, PathParams::default());

        let state = block_on(State::<Arc<AppStateFixture>>::from_request(&ctx))
            .expect("state present");

        // Deref: State<Arc<AppStateFixture>> -> Arc<AppStateFixture> -> AppStateFixture
        assert_eq!(state.name, "demo");
    }

    #[test]
    fn state_extractor_missing_registration_is_internal_error() {
        let request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        let ctx = RequestContext::new(request, PathParams::default());

        // `.err().expect(..)` (not `expect_err`) so we don't require
        // `State<T>: Debug` — extractors here mirror Json/Path and omit it.
        let err = block_on(State::<Arc<AppStateFixture>>::from_request(&ctx))
            .err()
            .expect("missing state must surface as an error, not a default");
        assert_eq!(err.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn state_extractor_deref_and_into_inner() {
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .expect("request");
        request.extensions_mut().insert(AppStateFixture {
            name: "x".to_owned(),
        });
        let ctx = RequestContext::new(request, PathParams::default());

        let state =
            block_on(State::<AppStateFixture>::from_request(&ctx)).expect("state present");
        assert_eq!(
            *state,
            AppStateFixture {
                name: "x".to_owned()
            }
        ); // Deref
        assert_eq!(
            state.into_inner(),
            AppStateFixture {
                name: "x".to_owned()
            }
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p edgezero-core --lib state_extractor 2>&1 | tail -20`
Expected: FAIL — compile error `cannot find type/struct State in this scope` (the extractor does not exist yet).

- [ ] **Step 3: Write the extractor**

Insert into `crates/edgezero-core/src/extractor.rs` immediately after the `Kv` extractor block (after the `impl Kv { ... }` that ends around `extractor.rs:529`), before the next extractor. `anyhow` is already used in this file; `core::any::type_name` needs no import.

```rust
/// Extractor for app-owned shared state registered via
/// [`RouterBuilder::with_state`]. Resolves by type from request extensions.
///
/// Typically `T = Arc<AppState>`. The registered value is cloned into every
/// request's extensions before dispatch; registering the same `T` twice is
/// last-write-wins.
///
/// ```ignore
/// use edgezero_core::extractor::State;
/// use std::sync::Arc;
///
/// #[edgezero_core::action]
/// async fn handle(State(state): State<Arc<AppState>>) -> Result<String, edgezero_core::error::EdgeError> {
///     Ok(state.greeting.clone())
/// }
/// ```
///
/// [`RouterBuilder::with_state`]: crate::router::RouterBuilder::with_state
pub struct State<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    #[inline]
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.extension::<T>().map(State).ok_or_else(|| {
            EdgeError::internal(anyhow::anyhow!(
                "no `State<{}>` registered -- call RouterBuilder::with_state(..) before build()",
                core::any::type_name::<T>()
            ))
        })
    }
}

impl<T> Deref for State<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for State<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<T> State<T> {
    /// Consume the extractor and return the inner value.
    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p edgezero-core --lib state_extractor 2>&1 | tail -20`
Expected: PASS — 3 tests (`state_extractor_resolves_registered_value`, `state_extractor_missing_registration_is_internal_error`, `state_extractor_deref_and_into_inner`).

- [ ] **Step 5: Lint**

Run: `cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-core/src/extractor.rs
git commit -m "feat(core): add State<T> extractor for app-owned shared state"
```

---

## Task 2: `RouterBuilder::with_state` + dispatch plumbing

**Files:**
- Modify: `crates/edgezero-core/src/router.rs` (add `StateInserter` alias, `state_inserters` field on `RouterBuilder` and `RouterInner`, `with_state` method, 5th arg through `build()`/`RouterService::new`, apply in `dispatch`; add router tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `State<T>` from Task 1 (`crate::extractor::State`), `crate::http::Extensions` (facade alias), `std::sync::Arc` (imported at `router.rs:2`).
- Produces: `RouterBuilder::with_state<T>(self, value: T) -> Self where T: Clone + Send + Sync + 'static`. Consumed by Task 3.

- [ ] **Step 1: Write the failing router tests**

Append to `crates/edgezero-core/src/router.rs`'s main `#[cfg(test)] mod tests` (the block whose imports are at `router.rs:476`, which already imports `Arc, Mutex`, `block_on`, `noop_waker_ref`, `Context, Poll`, `request_builder, Method, StatusCode`, `Body`, `RequestContext`, `EdgeError`).

```rust
    #[test]
    fn with_state_exposes_value_to_handler() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct Counter(u32);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(counter) = State::<Counter>::from_request(&ctx).await?;
            Ok(format!("count={}", counter.0))
        }

        let service = RouterService::builder()
            .with_state(Counter(9))
            .get("/count", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/count")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"count=9");
    }

    #[test]
    fn with_state_supports_multiple_distinct_types() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct A(u32);
        #[derive(Clone)]
        struct B(&'static str);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(a) = State::<A>::from_request(&ctx).await?;
            let State(b) = State::<B>::from_request(&ctx).await?;
            Ok(format!("{}-{}", a.0, b.0))
        }

        let service = RouterService::builder()
            .with_state(A(7))
            .with_state(B("hi"))
            .get("/both", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/both")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.body().as_bytes().expect("buffered"), b"7-hi");
    }

    #[test]
    fn with_state_same_type_is_last_write_wins() {
        use crate::extractor::{FromRequest as _, State};

        #[derive(Clone)]
        struct Counter(u32);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(counter) = State::<Counter>::from_request(&ctx).await?;
            Ok(format!("count={}", counter.0))
        }

        let service = RouterService::builder()
            .with_state(Counter(1))
            .with_state(Counter(2))
            .get("/c", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/c")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.body().as_bytes().expect("buffered"), b"count=2");
    }

    #[test]
    fn with_state_no_cross_request_bleed() {
        use crate::extractor::{FromRequest as _, State};
        use std::future::Future as _;

        #[derive(Clone)]
        struct Tag(&'static str);

        async fn handler(ctx: RequestContext) -> Result<String, EdgeError> {
            let State(tag) = State::<Tag>::from_request(&ctx).await?;
            Ok(tag.0.to_owned())
        }

        let service = RouterService::builder()
            .with_state(Tag("shared"))
            .get("/t", handler)
            .build();

        let req1 = request_builder()
            .method(Method::GET)
            .uri("/t")
            .body(Body::empty())
            .expect("req1");
        let req2 = request_builder()
            .method(Method::GET)
            .uri("/t")
            .body(Body::empty())
            .expect("req2");

        // Two independent in-flight requests, polled interleaved on one thread.
        let mut f1 = Box::pin(service.oneshot(req1));
        let mut f2 = Box::pin(service.oneshot(req2));
        let mut cx = Context::from_waker(noop_waker_ref());

        let mut r1 = None;
        let mut r2 = None;
        while r1.is_none() || r2.is_none() {
            if r1.is_none() {
                if let Poll::Ready(v) = f1.as_mut().poll(&mut cx) {
                    r1 = Some(v);
                }
            }
            if r2.is_none() {
                if let Poll::Ready(v) = f2.as_mut().poll(&mut cx) {
                    r2 = Some(v);
                }
            }
        }

        let resp1 = r1.unwrap().expect("resp1");
        let resp2 = r2.unwrap().expect("resp2");
        assert_eq!(resp1.body().as_bytes().expect("buffered"), b"shared");
        assert_eq!(resp2.body().as_bytes().expect("buffered"), b"shared");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p edgezero-core --lib with_state 2>&1 | tail -20`
Expected: FAIL — `no method named with_state found for struct RouterBuilder`.

- [ ] **Step 3: Add the `StateInserter` type alias**

In `crates/edgezero-core/src/router.rs`, add just above `pub struct RouterBuilder` (which is at `router.rs:71`, under its `#[derive(Default)]` at `router.rs:70`):

```rust
/// Type-erased closure that clones a registered state value into a request's
/// extensions at dispatch. See [`RouterBuilder::with_state`].
type StateInserter = Arc<dyn Fn(&mut crate::http::Extensions) + Send + Sync>;
```

- [ ] **Step 4: Add the `state_inserters` field to `RouterBuilder`**

Change the struct at `router.rs:70-76` from:

```rust
#[derive(Default)]
pub struct RouterBuilder {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_info: Vec<RouteInfo>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
}
```

to:

```rust
#[derive(Default)]
pub struct RouterBuilder {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_info: Vec<RouteInfo>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    state_inserters: Vec<StateInserter>,
}
```

- [ ] **Step 5: Add the `with_state` method**

In the `impl RouterBuilder` block, add immediately after `with_manifest_json` (which is at `router.rs:190-195`):

```rust
    /// Register a value cloned into every request's extensions before
    /// dispatch, making it available to the [`State<T>`] extractor and to
    /// `RequestContext`-based handlers.
    ///
    /// Typically `T = Arc<AppState>`. Registering the same `T` twice is
    /// last-write-wins. Cost is one `T::clone` (an `Arc` bump for
    /// `Arc<AppState>`) per registered state per request.
    ///
    /// [`State<T>`]: crate::extractor::State
    #[must_use]
    #[inline]
    pub fn with_state<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.state_inserters
            .push(Arc::new(move |ext: &mut crate::http::Extensions| {
                ext.insert(value.clone());
            }));
        self
    }
```

- [ ] **Step 6: Thread `state_inserters` through `build()`**

Change `build()` at `router.rs:108-119` from:

```rust
    pub fn build(self) -> RouterService {
        let route_index: Arc<[RouteInfo]> = Arc::from(self.route_info);

        RouterService::new(
            self.routes,
            self.middlewares,
            route_index,
            self.manifest_json,
        )
    }
```

to (add the 5th argument):

```rust
    pub fn build(self) -> RouterService {
        let route_index: Arc<[RouteInfo]> = Arc::from(self.route_info);

        RouterService::new(
            self.routes,
            self.middlewares,
            route_index,
            self.manifest_json,
            self.state_inserters,
        )
    }
```

- [ ] **Step 7: Add the field to `RouterInner` and the param to `RouterService::new`**

Change `RouterInner` at `router.rs:198-203` from:

```rust
struct RouterInner {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_index: Arc<[RouteInfo]>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
}
```

to:

```rust
struct RouterInner {
    manifest_json: Option<Arc<str>>,
    middlewares: Vec<BoxMiddleware>,
    route_index: Arc<[RouteInfo]>,
    routes: HashMap<Method, PathRouter<RouteEntry>>,
    state_inserters: Vec<StateInserter>,
}
```

Change `RouterService::new` at `router.rs:297-311` from:

```rust
    fn new(
        routes: HashMap<Method, PathRouter<RouteEntry>>,
        middlewares: Vec<BoxMiddleware>,
        route_index: Arc<[RouteInfo]>,
        manifest_json: Option<Arc<str>>,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                manifest_json,
                middlewares,
                route_index,
                routes,
            }),
        }
    }
```

to:

```rust
    fn new(
        routes: HashMap<Method, PathRouter<RouteEntry>>,
        middlewares: Vec<BoxMiddleware>,
        route_index: Arc<[RouteInfo]>,
        manifest_json: Option<Arc<str>>,
        state_inserters: Vec<StateInserter>,
    ) -> Self {
        Self {
            inner: Arc::new(RouterInner {
                manifest_json,
                middlewares,
                route_index,
                routes,
                state_inserters,
            }),
        }
    }
```

- [ ] **Step 8: Apply the inserters in `dispatch`**

In `RouterInner::dispatch` (`router.rs:206-237`), inside the `RouteMatch::Found(entry, params)` arm, add the state-insertion loop after the `needs.routes` block and before `let ctx = RequestContext::new(request, params);` (currently `router.rs:227`). The arm becomes:

```rust
            RouteMatch::Found(entry, params) => {
                // Inject only the introspection payloads this route asked for —
                // nothing for the vast majority of routes that need none.
                let needs = entry.introspection_needs;
                if needs.manifest {
                    if let Some(json) = &self.manifest_json {
                        request
                            .extensions_mut()
                            .insert(ManifestJson(Arc::clone(json)));
                    }
                }
                if needs.routes {
                    request
                        .extensions_mut()
                        .insert(RouteTable(Arc::clone(&self.route_index)));
                }
                // App-owned state registered via RouterBuilder::with_state.
                // Runs after introspection inserts; distinct TypeIds, so no
                // collision. Last registered wins for a given `T`.
                for inserter in &self.state_inserters {
                    inserter(request.extensions_mut());
                }
                let ctx = RequestContext::new(request, params);
                let next = Next::new(&self.middlewares, entry.handler.as_ref());
                next.run(ctx).await
            }
```

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test -p edgezero-core --lib with_state 2>&1 | tail -20`
Expected: PASS — 4 tests (`with_state_exposes_value_to_handler`, `with_state_supports_multiple_distinct_types`, `with_state_same_type_is_last_write_wins`, `with_state_no_cross_request_bleed`).

- [ ] **Step 10: Full crate test + lint**

Run: `cargo test -p edgezero-core 2>&1 | tail -20 && cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings 2>&1 | tail -20`
Expected: all existing + new tests PASS; clippy clean (proves the router restructure did not regress introspection tests).

- [ ] **Step 11: Commit**

```bash
git add crates/edgezero-core/src/router.rs
git commit -m "feat(core): RouterBuilder::with_state injects app state into request extensions"
```

---

## Task 3: `#[action]` composition integration test + docs

**Files:**
- Create: `crates/edgezero-macros/tests/action_state.rs`
- Modify: `crates/edgezero-macros/Cargo.toml` (add `futures` dev-dependency)
- Modify: `docs/guide/handlers.md` (append "Sharing app state" section)

**Interfaces:**
- Consumes: `State<T>` (Task 1), `RouterBuilder::with_state` (Task 2), `#[action]` (unchanged — `crates/edgezero-macros/src/action.rs:183` emits `<#ty as ::edgezero_core::extractor::FromRequest>::from_request(&__ctx).await?` for every non-`RequestContext` arg), `RouterService::oneshot` (`router.rs:316`), `Query<T>` extractor (`edgezero_core::extractor::Query`).
- Produces: nothing consumed downstream; this is the acceptance proof that the macro composes `State<T>` with another extractor.

- [ ] **Step 1: Add the `futures` dev-dependency to the macros crate**

Confirm `futures` is a workspace dependency:

Run: `grep -n 'futures = ' Cargo.toml`
Expected: a line like `futures = { version = "0.3", features = ["std", "executor"] }` under `[workspace.dependencies]`.

Then edit `crates/edgezero-macros/Cargo.toml`'s `[dev-dependencies]` (currently `edgezero-core`, `tempfile`, `trybuild`) to add `futures`:

```toml
[dev-dependencies]
# `edgezero-core` re-exports `AppConfig`; the derive tests assert
# against the trait/types over the re-export path the way downstream
# users will. Cargo allows dev-dep cycles (only the main dep edge
# matters for build ordering).
edgezero-core = { workspace = true }
futures = { workspace = true }
tempfile = { workspace = true }
trybuild = { workspace = true }
```

(If `grep` shows `futures` is not workspace-managed, use `futures = "0.3"` with `features = ["std", "executor"]` instead.)

- [ ] **Step 2: Write the failing integration test**

Create `crates/edgezero-macros/tests/action_state.rs`:

```rust
//! Integration coverage: `#[action]` composes the `State<T>` extractor with a
//! request-derived extractor (`Query<T>`) and runs end-to-end through the
//! router. Lives in `edgezero-macros/tests` because the `#[action]` macro
//! emits absolute `::edgezero_core::…` paths that only resolve when
//! `edgezero_core` is an external crate (as it is here, via the dev-dep).

#[cfg(test)]
mod tests {
    use edgezero_core::action;
    use edgezero_core::body::Body;
    use edgezero_core::error::EdgeError;
    use edgezero_core::extractor::{Query, State};
    use edgezero_core::http::{request_builder, Method, StatusCode};
    use edgezero_core::router::RouterService;
    use futures::executor::block_on;
    use serde::Deserialize;
    use std::sync::Arc;

    #[derive(Clone)]
    struct AppState {
        greeting: String,
    }

    #[derive(Deserialize)]
    struct Params {
        n: u32,
    }

    #[action]
    async fn handler(
        State(state): State<Arc<AppState>>,
        Query(params): Query<Params>,
    ) -> Result<String, EdgeError> {
        Ok(format!("{}:{}", state.greeting, params.n))
    }

    #[test]
    fn action_composes_state_and_query() {
        let service = RouterService::builder()
            .with_state(Arc::new(AppState {
                greeting: "hi".to_owned(),
            }))
            .get("/h", handler)
            .build();

        let request = request_builder()
            .method(Method::GET)
            .uri("/h?n=5")
            .body(Body::empty())
            .expect("request");

        let response = block_on(service.oneshot(request)).expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_bytes().expect("buffered"), b"hi:5");
    }
}
```

- [ ] **Step 3: Run the integration test**

Run: `cargo test -p edgezero-macros --test action_state 2>&1 | tail -20`
Expected: PASS — `action_composes_state_and_query`. (This simultaneously proves the macro needs no change: `State<T>` is dispatched by the same generic `FromRequest` line as `Query<T>`.)

- [ ] **Step 4: Add the docs section**

Append to `docs/guide/handlers.md` a new section (place it after the existing extractor documentation, before any "Next steps"/footer):

```markdown
## Sharing app state

Request-derived extractors (`Json`, `Query`, `Path`, …) cover per-request data.
For app-owned state that outlives a single request — a settings object, a
connection registry, an orchestrator — register it once on the router and read
it back with the `State<T>` extractor.

Register the value with `RouterBuilder::with_state`. It is cloned into every
request's extensions before dispatch, so `T` must be `Clone + Send + Sync +
'static` — typically an `Arc<AppState>`, where the clone is a cheap refcount
bump:

```rust
use std::sync::Arc;
use edgezero_core::extractor::State;
use edgezero_core::router::RouterService;

#[derive(Clone)]
struct AppState {
    greeting: String,
}

let state = Arc::new(AppState { greeting: "hello".into() });

let service = RouterService::builder()
    .with_state(Arc::clone(&state))
    .get("/greet", greet)
    .build();
```

Read it in any `#[action]` handler by adding a `State<T>` argument — it composes
with the other extractors:

```rust
use edgezero_core::{action, error::EdgeError};
use edgezero_core::extractor::{Query, State};
use std::sync::Arc;

#[action]
async fn greet(
    State(state): State<Arc<AppState>>,
) -> Result<String, EdgeError> {
    Ok(state.greeting.clone())
}
```

Register different types independently (`with_state(a).with_state(b)`); each is
resolved by its own type. Registering the same `T` twice is last-write-wins. If
a handler asks for a `State<T>` that was never registered, extraction fails with
a `500` — register it before `build()`.
```

- [ ] **Step 5: Full verification**

Run: `cargo test --workspace --all-targets 2>&1 | tail -20`
Expected: PASS.

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -20`
Expected: formatted; clippy clean.

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin" 2>&1 | tail -5 && cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin 2>&1 | tail -5`
Expected: both succeed (proves the core change is WASM-clean across adapters).

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-macros/tests/action_state.rs crates/edgezero-macros/Cargo.toml docs/guide/handlers.md
git commit -m "test(macros): prove #[action] composes State<T>; docs: sharing app state"
```

---

## Acceptance criteria

1. `cargo fmt` / clippy clean across `edgezero-core`, `edgezero-macros`, all adapters.
2. New unit tests (Task 1: 3), router tests (Task 2: 4), and the integration test (Task 3: 1) pass.
3. `cargo test --workspace --all-targets` green; PR #300's introspection tests still pass (proves `with_state` is additive to the injection mechanism).
4. WASM checks (`fastly cloudflare spin`; spin `wasm32-wasip2`) succeed.
5. Rustdoc on `State`, `with_state`, and the `docs/guide/handlers.md` section merged.

## Self-review notes (mapping to spec §3)

- §3.1 `State<T>` extractor + `Deref`/`into_inner` → Task 1.
- §3.2 router plumbing (`state_inserters` field, `with_state`, dispatch insertion) → Task 2, mirroring PR #300's `manifest_json` column exactly.
- §3.3 naming → `State<T>` only (no `Extension` alias), per locked decision.
- §3.4 tests: resolves registered / 500 unregistered / Deref (Task 1); handler sees value / two `T`s coexist / last-write-wins (Task 2); `#[action]` composition (Task 3); concurrency/no-bleed (Task 2).
- §3.5 docs: `docs/guide/handlers.md` + rustdoc → Task 3.
- §8 corrections folded in: facade `crate::http::Extensions` (not bare `http::Extensions`); no `lib.rs` re-export; `state_inserters` threaded through `RouterInner` + `RouterService::new` + `build()`.
