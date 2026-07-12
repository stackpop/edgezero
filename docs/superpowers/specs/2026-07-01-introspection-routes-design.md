# Design: Pluggable Introspection Routes (manifest / config / routes)

**Date:** 2026-07-01 (architecture finalized 2026-07-02)
**Status:** Approved — implementation in progress
**Scope:** `edgezero-core`, `edgezero-macros`, `examples/app-demo`, `edgezero-cli` templates

> **Note on history.** This spec describes the **final** architecture only:
> **opt-in, per-route, per-capability gated injection driven by
> `#[action(manifest|routes)]`, with typed extractors for access.** Earlier drafts
> injected data on every request (or via a single blanket gate / bundle); those
> were superseded. The "Design evolution" section at the end records the path.
> This document governs.

## Summary

Three reusable, framework-supplied HTTP handlers let any EdgeZero app expose its
own metadata at runtime:

| Handler path                             | Emits                                                   |
| ---------------------------------------- | ------------------------------------------------------- |
| `edgezero_core::introspection::manifest` | The full manifest as JSON (baked at compile time)       |
| `edgezero_core::introspection::config`   | The default config-store envelope `.data` (secret-safe) |
| `edgezero_core::introspection::routes`   | `[{ "method", "path" }]` from the live route index      |

Apps bind them like any other route via `[[triggers.http]]`, choosing their own
paths. There is **no** `[introspection]` manifest section and **no** dedicated
builder API. `app-demo` and every generated app ship the three routes pre-wired
under `/_<app-name>/{manifest,config,routes}`, as plain trigger rows a developer
can edit or delete.

This design also **removes** the legacy built-in route-listing machinery
(`enable_route_listing`, `enable_route_listing_at`, `DEFAULT_ROUTE_LISTING_PATH`,
`/__edgezero/routes`, `RouteListingEntry`, `build_listing_response`) in favor of
the bindable `routes` handler.

## Motivation

Today there is no runtime way to inspect what an app *is*:

- The **manifest** is compile-time only. `Manifest` derives `Deserialize` +
  `Validate` but not `Serialize`, and the portable-store rewrite removed the
  `run_app(include_str!("edgezero.toml"), …)` shape, so a running adapter binary
  no longer carries the manifest.
- The **app config** is reachable at runtime through the config store, but only
  via the typed `AppConfig<C>` extractor, which resolves secrets and needs the
  app's concrete config type.
- The only built-in introspection is a route listing at `/__edgezero/routes`,
  wired through a bespoke builder method rather than the normal routing path.

We want a single, consistent, "bind it yourself" mechanism for all three — that
costs nothing for the ~100% of requests that are not introspection calls, and
that only provisions the *specific* data each handler asks for.

## Key Decisions

1. **Manifest output** — bake the full manifest as JSON. `Manifest` gains
   `Serialize`; the `app!` macro serializes the parsed manifest at expansion time
   and hands the JSON string to the router (`with_manifest_json`).
2. **Config output** — emit the raw config-store `BlobEnvelope.data`. Generic
   (core needs no knowledge of the app's typed config `C`) and secret-safe:
   secret fields stay as unresolved key-name references; resolution happens only
   inside the typed `AppConfig<C>` extractor.
3. **Wiring** — plain `[[triggers.http]]` bindings to stable core handler paths.
   No `[introspection]` section, no builder enable-API, and the `app!` macro does
   **not** inspect handler paths.
4. **Paths** — per-app namespace `/_<app-name>/{manifest,config,routes}` (single
   underscore); just the default paths written into the templates.
5. **Access via typed extractors that are also the injected payloads.** Handlers
   that need data declare it in their signature, matching the
   `Json`/`Path`/`AppConfig` idiom:
   - `ManifestJson(pub Arc<str>)` — the baked manifest JSON (used by `manifest`).
   - `RouteTable(pub Arc<[RouteInfo]>)` — the live route index (used by `routes`).
   Each derives `Clone`, is what the router injects into the request, and its
   `FromRequest::from_request` clones its own type back out (`500` if absent).
   `config` takes `RequestContext` and uses neither.
6. **Opt-in, per-capability gated injection driven by an atomic `#[action(...)]`
   parameter.** The router injects each capability's payload **only** for routes
   whose handler opted into that specific capability — never for general traffic,
   and never more than the handler asked for. The opt-in is atomic all the way
   down:
   - `#[action(manifest)]` / `#[action(routes)]` / `#[action(manifest, routes)]`
     declare exactly which data the handler consumes. `#[action]` (no params) is
     unchanged.
   - Each param maps 1:1 to a field of `IntrospectionNeeds { manifest, routes }`,
     reported by the handler via `DynHandler::introspection_needs()`.
   - `dispatch` injects `ManifestJson` iff `needs.manifest`, and `RouteTable` iff
     `needs.routes`. A `manifest`-only route never carries the route table, and
     vice versa.
   No process-global state, no unstable specialization, no bundle struct, and no
   `app!`/`edgezero.toml` change.
7. **Remove route listing** — delete the `enable_route_listing` machinery and
   `/__edgezero/routes`.

## Architecture

### Data flow

```
compile time                              runtime
------------                              -------
edgezero.toml
  │  app!() parses Manifest
  │  serde_json::to_string(&manifest)
  ▼
build_router()
  builder.with_manifest_json("{…}")       RouterService::oneshot(req)
  builder.get(path, introspection::routes) └─ RouterInner::dispatch(req)
        │                                       │  find_route(req) → RouteEntry
        ▼                                       │  needs = entry.introspection_needs
RouterInner {                                   │  if needs.manifest && manifest_json:
  manifest_json: Option<Arc<str>>,        ──────┼────►  insert ManifestJson(clone)
  route_index:   Arc<[RouteInfo]>,        ──────┼────►  if needs.routes: insert RouteTable(clone)
}                                               ▼
                                          handler runs; extractor clones its
                                          own type out of the request:
                                            manifest → ManifestJson(json)
                                            routes   → RouteTable(index)
                                            config   → default config store (no injection)
```

- **The opt-in is on the handler.** `#[action(manifest)]` / `#[action(routes)]`
  expand the handler to a capability-carrying struct whose
  `introspection_needs()` sets the matching field(s). `add_route` reads that and
  stores `IntrospectionNeeds` on the `RouteEntry`.
- **manifest**: parsed at compile time, re-serialized to JSON by the macro, held
  as `Option<Arc<str>>` on the router, injected (as `ManifestJson`) only for
  routes that asked for `manifest`, returned verbatim. No runtime TOML dependency.
- **routes**: injected (as `RouteTable`) only for routes that asked for `routes`;
  projected at request time to `[{method, path}]`.
- **config**: read from the default config store; needs no injection.

### Component 1 — `Manifest: Serialize` (`edgezero-core/src/manifest.rs`)  *(done)*

`Serialize` added to `Manifest` and every nested struct that appears in output.
The enums `HttpMethod`/`BodyMode`/`LogLevel` get **manual** `Serialize` impls
mirroring their manual `Deserialize` (wire strings `"GET"`/`"buffered"`/`"info"`,
and `body_mode` serializes as the renamed key `body-mode`). Secret **values** are
never serialized: `environment.secrets` entries omit `value` via a
`serialize_with` redactor; `environment.variables` keep it. Internal fields
(`root`, `logging_resolved`) stay `#[serde(skip)]`.

### Component 2 — `#[action]` atomic opt-in + capability-carrying handlers (`edgezero-macros/src/action.rs`)

`#[action]` gains an **optional atomic parameter list** naming the introspection
data the handler consumes:

- **`#[action]`** (no params) — unchanged. Expands to a handler **fn**; via the
  existing `Fn` blanket `impl DynHandler` its `introspection_needs()` returns the
  default (all-false).
- **`#[action(manifest)]`, `#[action(routes)]`, `#[action(manifest, routes)]`** —
  expand the handler to a **unit struct** with its own `impl DynHandler`, whose
  `introspection_needs()` returns an `IntrospectionNeeds` with exactly the named
  fields set. (A fn can't carry per-item data past type-erasure into
  `Arc<dyn DynHandler>`; a struct can. Only opt-in handlers become structs; every
  other handler stays a fn, untouched.)

The macro parses the params as a comma-separated ident list, validates each
against the known set `{ manifest, routes }`, and emits `compile_error!` on an
unknown ident. The set is extensible (future atomic capabilities are new idents
+ new `IntrospectionNeeds` fields).

Generated struct (paths absolute, matching the existing macro):

```rust
#inner_fn                              // the user's body, unchanged

#(#attrs)*
#[allow(non_camel_case_types)]
#vis struct #ident;

impl ::edgezero_core::handler::DynHandler for #ident {
    #[inline]
    fn call(&self, __ctx: ::edgezero_core::context::RequestContext)
        -> ::edgezero_core::http::HandlerFuture {
        ::std::boxed::Box::pin(async move {
            #(#extract_stmts)*                // <Ty as FromRequest>::from_request(&__ctx).await?
            let result = #inner_ident(#(#arg_idents),*).await;
            ::edgezero_core::responder::Responder::respond(result)
        })
    }
    #[inline]
    fn introspection_needs(&self) -> ::edgezero_core::handler::IntrospectionNeeds {
        ::edgezero_core::handler::IntrospectionNeeds { manifest: #manifest_lit, routes: #routes_lit }
    }
}
```

where `#manifest_lit` / `#routes_lit` are the `bool` literals derived from the
parsed params.

### Component 3 — Router gating (`edgezero-core/src/{handler,router,context}.rs`)

**`handler.rs`** — the capability value type + the reporting method:

```rust
/// Which introspection payloads a route's handler needs injected at dispatch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntrospectionNeeds {
    pub manifest: bool,
    pub routes: bool,
}

impl IntrospectionNeeds {
    #[must_use]
    pub fn any(self) -> bool {
        self.manifest || self.routes
    }
}

pub trait DynHandler: Send + Sync {
    fn call(&self, ctx: RequestContext) -> HandlerFuture;

    /// Introspection payloads a route bound to this handler needs. Default is
    /// none; `#[action(manifest|routes)]` handlers override it.
    fn introspection_needs(&self) -> IntrospectionNeeds {
        IntrospectionNeeds::default()
    }
}
```

The `Fn` blanket `impl DynHandler` needs no change (inherits the default).

**`router.rs`:**

- `RouteEntry` gains `introspection_needs: IntrospectionNeeds` (`Copy`; copied in
  the manual `Clone`/`clone_from`).
- `add_route` reads it from the boxed handler at registration:
  ```rust
  let boxed = handler.into_handler();
  let introspection_needs = boxed.introspection_needs();
  router.insert(path, RouteEntry { handler: boxed, introspection_needs });
  ```
- `RouterInner` keeps `manifest_json: Option<Arc<str>>` and
  `route_index: Arc<[RouteInfo]>` (no bundle struct).
  `RouterBuilder::with_manifest_json(impl Into<Arc<str>>)` (set by the `app!`
  macro) supplies the JSON.
- `dispatch` injects per capability, after matching:
  ```rust
  match self.find_route(&method, &path) {
      RouteMatch::Found(entry, params) => {
          let needs = entry.introspection_needs;
          let mut request = request;
          if needs.manifest {
              if let Some(json) = &self.manifest_json {
                  request.extensions_mut()
                      .insert(crate::introspection::ManifestJson(Arc::clone(json)));
              }
          }
          if needs.routes {
              request.extensions_mut()
                  .insert(crate::introspection::RouteTable(Arc::clone(&self.route_index)));
          }
          let ctx = RequestContext::new(request, params);
          let next = Next::new(&self.middlewares, entry.handler.as_ref());
          next.run(ctx).await
      }
      // MethodNotAllowed / NotFound unchanged
  }
  ```

**`context.rs`** — a single `pub(crate)` accessor the extractors share (there is
no public `introspection()` accessor and no `IntrospectionData` type):

```rust
pub(crate) fn extension<T>(&self) -> Option<T>
where
    T: Clone + Send + Sync + 'static,
{
    self.request.extensions().get::<T>().cloned()
}
```

### Component 4 — `edgezero_core::introspection` module (`edgezero-core/src/introspection.rs`)

The extractors (which are also the injected payloads) and three handlers:

```rust
#[derive(Clone)]
pub struct ManifestJson(pub Arc<str>);

#[async_trait(?Send)]
impl FromRequest for ManifestJson {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.extension::<ManifestJson>()
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("manifest introspection data not available")))
    }
}

#[derive(Clone)]
pub struct RouteTable(pub Arc<[RouteInfo]>);

#[async_trait(?Send)]
impl FromRequest for RouteTable {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.extension::<RouteTable>()
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("route-table introspection data not available")))
    }
}

#[action(manifest)]
pub async fn manifest(ManifestJson(json): ManifestJson) -> Result<Response, EdgeError> {
    json_response(StatusCode::OK, Body::text(json.to_string()))
}

#[action(routes)]
pub async fn routes(RouteTable(table): RouteTable) -> Result<Response, EdgeError> {
    let views: Vec<RouteView> = table.iter()
        .map(|r| RouteView { method: r.method().as_str().to_owned(), path: r.path().to_owned() })
        .collect();
    json_response(StatusCode::OK, Body::json(&views).map_err(EdgeError::internal)?)
}

#[action]
pub async fn config(ctx: RequestContext) -> Result<Response, EdgeError> {
    let binding = ctx.config_store_default_binding()
        .ok_or_else(|| EdgeError::not_found("no default config store registered"))?;
    let raw = binding.handle.get(&binding.default_key).await
        .map_err(EdgeError::from)?                    // preserves 503/400/500
        .ok_or_else(|| EdgeError::not_found("no config blob in default store"))?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)
        .map_err(|e| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {e}")))?;
    envelope.verify().map_err(|e| EdgeError::internal(anyhow::anyhow!("envelope verification failed: {e}")))?;
    json_response(StatusCode::OK, Body::json(&envelope.into_data()).map_err(EdgeError::internal)?)
}
```

`RouteView { method: String, path: String }` (derives `Serialize`) is the JSON
shape for `routes`. `Response` is imported from `crate::http`. Because `dispatch`
constructs `ManifestJson`/`RouteTable`, `router.rs` imports them from
`crate::introspection` (a same-crate module reference — no crate-level cycle).

### Component 5 — `app!` macro (`edgezero-macros/src/app.rs`)  *(done, unchanged by the gating work)*

- Serialize the parsed manifest (`serde_json::to_string`, `compile_error!` on
  failure) and emit `builder = builder.with_manifest_json(<lit>)` as the first
  builder mutation in `build_router()`.
- Emit `const _: &[u8] = include_bytes!(<abs manifest path>);` so Cargo treats
  `edgezero.toml` as a build input.
- Route registration is ordinary `builder.get(path, handler)` / `route(...)`; the
  macro does **not** inspect handler paths. Gating comes entirely from the
  handler's `introspection_needs()`.

### Component 6 — Removals  *(done)*

Deleted from `router.rs`: `DEFAULT_ROUTE_LISTING_PATH`, `enable_route_listing`,
`enable_route_listing_at`, the `route_listing_path` field + listing branch in
`build()`, `build_listing_response`, `RouteListingEntry`, and all `route_listing_*`
tests. No workspace references to `/__edgezero/routes` remain.

### Component 7 — Templates + app-demo (default bindings)  *(done)*

Three `[[triggers.http]]` rows in `examples/app-demo/edgezero.toml` and in
`crates/edgezero-cli/src/templates/root/edgezero.toml.hbs` (using `/_{{name}}/…`
and `{{{adapter_list}}}`), bound to `edgezero_core::introspection::{manifest,config,routes}`.
No template handler code is generated — the handlers live in core.

## Interfaces (summary)

| Unit                          | Public surface                                                    |
| ----------------------------- | ----------------------------------------------------------------- |
| `IntrospectionNeeds`          | `{ manifest: bool, routes: bool }`, `Copy`, `Default`, `any()`    |
| `DynHandler`                  | `fn introspection_needs(&self) -> IntrospectionNeeds { default }` |
| `RouterBuilder`               | `with_manifest_json(impl Into<Arc<str>>)`                         |
| `RequestContext`              | `pub(crate) extension::<T>() -> Option<T>` (no public accessor)   |
| `introspection::ManifestJson` | `pub Arc<str>`; `Clone`; `FromRequest`                            |
| `introspection::RouteTable`   | `pub Arc<[RouteInfo]>`; `Clone`; `FromRequest`                    |
| `introspection::{manifest,routes}` | `#[action(manifest)]` / `#[action(routes)]` GET → JSON       |
| `introspection::config`       | `#[action]` GET → JSON (default config store)                     |

## Error Handling

- **manifest** / **routes**: the extractor returns `500 internal` if its payload
  is absent from the request — i.e. the route did not opt into that capability
  (`#[action(manifest)]` / `#[action(routes)]` missing), or, for `manifest`, the
  app never called `with_manifest_json` (`manifest_json` is `None`, so `dispatch`
  injects nothing).
- **config**: no default config store → `404`; no blob → `404`; `ConfigStoreError`
  mapped via `EdgeError::from` (503 unavailable / 400 invalid-key / 500 internal);
  malformed or unverifiable envelope → `500`.

## Testing Strategy

Colocated `#[cfg(test)]`, `futures::executor::block_on` (no Tokio), no network.

- **macros**: `#[action]` (no params) still emits a fn; `#[action(manifest)]`
  emits a struct impl'ing `DynHandler` with `introspection_needs()` setting
  `manifest: true`; `#[action(bogus)]` is a compile error. Plus a
  **compile/behavioral** test in core proving `.get("/m", manifest)` accepts the
  unit-struct handler value and runs end to end (`oneshot` → 200).
- **handler.rs / router.rs**: default `introspection_needs()` is all-false for fn
  handlers; a `manifest`-flagged route injects `ManifestJson` (handler +
  middleware can read it) and does **not** inject `RouteTable`; a `routes`-flagged
  route is the mirror; a plain route injects neither
  (`ctx.extension::<ManifestJson>().is_none()`).
- **introspection module**: `manifest` returns injected JSON; `routes` returns
  `[{method,path}]`; `config` returns envelope `data` and the full status matrix
  (200/404/400/503/500×3); `manifest` with no baked JSON → 500.
- **app-demo**: `/_app-demo/{manifest,routes}` → 200 + body shape; `/config` with
  a seeded `ConfigRegistry` → 200 + secret-safe key-name.

## CI Gates

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-targets` (+ `cd examples/app-demo && cargo test --workspace --all-targets`)
4. `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`
5. `cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates`
6. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
7. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`

## Constraints & Non-Goals

- **WASM-first**: no Tokio, no runtime-specific deps. `serde_json` is added only
  to `edgezero-macros` (build-time proc-macro crate).
- **No auth/gating of exposure in this iteration**: endpoints are reachable
  wherever bound; config output is secret-safe, and `/manifest` emits
  `environment.variables[].value` (not secrets) — documented so operators don't
  store secrets in `[environment.variables]`. Access control is a follow-up.
- **No process-global state**: `manifest_json` / `route_index` are
  per-`RouterService`, so tests and multiple apps in one process stay independent.
- **No `[introspection]` manifest section, no builder enable-API, no `app!`
  handler-path inspection** — the opt-in lives on `#[action(...)]`.

## Design evolution (for reviewers)

1. First cut: inject a bundle on **every** request; handlers read
   `ctx.introspection()`. Rejected — taxes all traffic for rarely-hit endpoints.
2. Considered a process-global (`OnceLock`) source — rejected: one-manifest-
   per-process breaks unit tests and adds shared mutable state.
3. Considered `app!` recognizing the `edgezero_core::introspection::` handler
   namespace to flag routes — rejected as a fragile string-match hack.
4. Considered a single `DynHandler::needs_introspection() -> bool` + one
   `IntrospectionData` bundle — rejected: inconsistent with the atomic
   `#[action(manifest|routes)]` params, and it over-provisions (a `manifest`-only
   route would carry the route table).
5. **Final:** fully atomic. `#[action(manifest|routes)]` → `IntrospectionNeeds`
   (per-capability bools) reported by `DynHandler::introspection_needs()`; the
   router injects each capability's payload independently, only for routes that
   asked for it. The extractors `ManifestJson`/`RouteTable` are themselves the
   injected payloads. No global, no `app!` hack, no bundle, no unstable
   specialization; `#[action]` (no params) is 100% unchanged so only
   `manifest`/`routes` become structs.
