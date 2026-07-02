# Design: Pluggable Introspection Routes (manifest / config / routes)

**Date:** 2026-07-01 (architecture finalized 2026-07-02)
**Status:** Approved — implementation in progress
**Scope:** `edgezero-core`, `edgezero-macros`, `examples/app-demo`, `edgezero-cli` templates

> **Note on history.** This spec was rewritten on 2026-07-02 to describe the
> **final** architecture only: **opt-in, per-route gated injection driven by
> `#[action(manifest|routes)]`, with typed extractors for access.** Earlier drafts
> described unconditional per-request injection with handlers reading
> `ctx.introspection()` directly; that approach was superseded (it taxed all
> traffic for endpoints hit rarely). The "Design evolution" section at the end
> records the path taken. Where any older wording survives elsewhere, this
> document governs.

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
costs nothing for the ~100% of requests that are not introspection calls.

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
5. **Access via typed extractors** — handlers that need injected data declare it
   in their signature, matching the `Json`/`Path`/`AppConfig` idiom:
   - `ManifestJson(pub Arc<str>)` — the baked manifest JSON (used by `manifest`).
   - `RouteTable(pub Arc<[RouteInfo]>)` — the live route index (used by `routes`).
   Both implement `FromRequest`, read the injected `IntrospectionData` via
   `ctx.introspection()`, and return `500` if it is absent. `config` takes
   `RequestContext` and uses neither.
6. **Opt-in, per-route gated injection driven by `#[action(...)]`** — the router
   injects `IntrospectionData` **only for routes whose handler opted in**, never
   for general traffic. The opt-in is an atomic `#[action]` parameter and the
   capability rides the handler to registration (details in Component 2/3). No
   process-global state, no unstable specialization, no `app!`/`edgezero.toml`
   change.
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
        ▼                                       │  if entry.needs_introspection:
RouterInner {                                   │      req.extensions.insert(
  introspection: Arc<IntrospectionData>,  ──────┼────►    Arc::clone(introspection))
}  (built once)                                 ▼
                                          handler runs; extractor reads
                                          ctx.introspection():
                                            manifest → ManifestJson(json)
                                            routes   → RouteTable(index)
                                            config   → default config store (no injection)
```

- **The opt-in is on the handler.** `#[action(manifest)]` / `#[action(routes)]`
  expand the handler to a capability-carrying struct (below). `add_route` reads
  that capability and flags the `RouteEntry`. `dispatch` injects the shared
  `Arc<IntrospectionData>` only for flagged routes.
- **manifest**: parsed at compile time, re-serialized to JSON by the macro,
  baked into `build_router()`, held on the router's `IntrospectionData`, injected
  for `manifest`-flagged routes, returned verbatim. No runtime TOML dependency.
- **routes**: projected at request time from the live route index in
  `IntrospectionData`.
- **config**: read from the default config store; needs no injection.

### Component 1 — `Manifest: Serialize` (`edgezero-core/src/manifest.rs`)  *(done)*

`Serialize` added to `Manifest` and every nested struct that appears in output.
The enums `HttpMethod`/`BodyMode`/`LogLevel` get **manual** `Serialize` impls
mirroring their manual `Deserialize` (wire strings `"GET"`/`"buffered"`/`"info"`,
and `body_mode` serializes as the renamed key `body-mode`). Secret **values** are
never serialized: `environment.secrets` entries omit `value` via a
`serialize_with` redactor; `environment.variables` keep it. Internal fields
(`root`, `logging_resolved`) stay `#[serde(skip)]`.

### Component 2 — `#[action]` opt-in + capability-carrying handlers (`edgezero-macros/src/action.rs`)

`#[action]` gains an **optional atomic parameter list** naming the introspection
data the handler needs:

- **`#[action]`** (no params) — unchanged. Expands to a handler **fn**, which via
  the existing `Fn` blanket `impl DynHandler` reports `needs_introspection() == false`.
- **`#[action(manifest)]`, `#[action(routes)]`, `#[action(manifest, routes)]`** —
  expand the handler to a **unit struct** with its own `impl DynHandler` whose
  `needs_introspection()` returns `true`. (A fn can't carry a per-item flag past
  type-erasure into `Arc<dyn DynHandler>`; a struct can. Only opt-in handlers
  become structs; every other handler stays a fn.)

The macro validates each param against the known set `{ manifest, routes }` and
emits `compile_error!` on an unknown ident. The set is extensible (future atomic
capabilities are new idents). The atomic names are the declarative surface; since
`IntrospectionData` is one cheap `Arc` bundle, all recognized capabilities
currently collapse to the single `needs_introspection()` gate (room to split
payloads later without an attribute change).

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
    fn needs_introspection(&self) -> bool { true }
}
```

### Component 3 — Router gating + accessor (`edgezero-core/src/{handler,router,context}.rs`)

- **`DynHandler`** gains `fn needs_introspection(&self) -> bool { false }`
  (object-safe; the `Fn` blanket impl inherits the default).
- **`RouteEntry`** gains `needs_introspection: bool`. `add_route` reads it from
  the boxed handler at registration:
  ```rust
  let boxed = handler.into_handler();
  let needs_introspection = boxed.needs_introspection();
  router.insert(path, RouteEntry { handler: boxed, needs_introspection });
  ```
- **`RouterInner`** holds a precomputed `introspection: Arc<IntrospectionData>`,
  built once in `build()` from `self.manifest_json` + the route index.
  `RouterBuilder::with_manifest_json(impl Into<Arc<str>>)` (set by the `app!`
  macro) supplies the JSON.
- **`RouterInner::dispatch`** injects only for flagged routes, after matching:
  ```rust
  match self.find_route(&method, &path) {
      RouteMatch::Found(entry, params) => {
          let mut request = request;
          if entry.needs_introspection {
              request.extensions_mut().insert(Arc::clone(&self.introspection));
          }
          let ctx = RequestContext::new(request, params);
          let next = Next::new(&self.middlewares, entry.handler.as_ref());
          next.run(ctx).await
      }
      // MethodNotAllowed / NotFound unchanged
  }
  ```
- **`RequestContext::introspection()`** reads the `Arc`:
  ```rust
  pub fn introspection(&self) -> Option<&crate::router::IntrospectionData> {
      self.request.extensions().get::<std::sync::Arc<crate::router::IntrospectionData>>()
          .map(|arc| arc.as_ref())
  }
  ```

`IntrospectionData` is the injected payload:
```rust
#[derive(Clone)]
pub struct IntrospectionData {
    pub manifest_json: Option<Arc<str>>,
    pub routes: Arc<[RouteInfo]>,
}
```

### Component 4 — `edgezero_core::introspection` module (`edgezero-core/src/introspection.rs`)

The extractors and three handlers:

```rust
pub struct ManifestJson(pub Arc<str>);
#[async_trait(?Send)]
impl FromRequest for ManifestJson {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.introspection().and_then(|d| d.manifest_json.clone()).map(ManifestJson)
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("manifest introspection data not available")))
    }
}

pub struct RouteTable(pub Arc<[RouteInfo]>);
#[async_trait(?Send)]
impl FromRequest for RouteTable {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.introspection().map(|d| RouteTable(Arc::clone(&d.routes)))
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
shape for `routes`. `Response` is imported from `crate::http`.

### Component 5 — `app!` macro (`edgezero-macros/src/app.rs`)  *(done, unchanged by the gating work)*

- Serialize the parsed manifest (`serde_json::to_string`, `compile_error!` on
  failure) and emit `builder = builder.with_manifest_json(<lit>)` as the first
  builder mutation in `build_router()`.
- Emit `const _: &[u8] = include_bytes!(<abs manifest path>);` so Cargo treats
  `edgezero.toml` as a build input (rebuild on manifest change).
- Route registration is ordinary `builder.get(path, handler)` / `route(...)`; the
  macro does **not** inspect handler paths. Gating comes entirely from the
  handler's `needs_introspection()`.

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

| Unit                       | Public surface                                                  |
| -------------------------- | --------------------------------------------------------------- |
| `IntrospectionData`        | `{ manifest_json: Option<Arc<str>>, routes: Arc<[RouteInfo]> }`  |
| `DynHandler`               | `fn needs_introspection(&self) -> bool { false }`                |
| `RouterBuilder`            | `with_manifest_json(impl Into<Arc<str>>)`                        |
| `RequestContext`           | `introspection() -> Option<&IntrospectionData>`                  |
| `introspection::ManifestJson` | `FromRequest`; `pub Arc<str>`                                 |
| `introspection::RouteTable`   | `FromRequest`; `pub Arc<[RouteInfo]>`                         |
| `introspection::{manifest,routes}` | `#[action(manifest)]` / `#[action(routes)]` GET → JSON      |
| `introspection::config`    | `#[action]` GET → JSON (default config store)                    |

## Error Handling

- **manifest** / **routes**: extractor returns `500 internal` if
  `IntrospectionData` is absent (route not opted in) or, for `manifest`, if
  `manifest_json` is `None` (no `with_manifest_json`).
- **config**: no default config store → `404`; no blob → `404`; `ConfigStoreError`
  mapped via `EdgeError::from` (503 unavailable / 400 invalid-key / 500 internal);
  malformed or unverifiable envelope → `500`.

## Testing Strategy

Colocated `#[cfg(test)]`, `futures::executor::block_on` (no Tokio), no network.

- **macros**: `#[action]` (no params) still emits a fn; `#[action(manifest)]`
  emits a struct impl'ing `DynHandler` with `needs_introspection() == true`;
  `#[action(bogus)]` is a compile error. Plus a **compile/behavioral** test in
  core that a struct handler registers and runs: `.get("/m", manifest)` →
  `oneshot` → 200 (proves the unit-struct-as-handler-value path works end to end).
- **handler.rs / router.rs**: `needs_introspection()` default is false for fn
  handlers; a flagged route injects `IntrospectionData` (handler + middleware see
  it); a non-flagged route does **not** (`ctx.introspection().is_none()`).
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
- **No process-global state**: `IntrospectionData` is per-`RouterService`, so
  tests and multiple apps in one process stay independent.
- **No `[introspection]` manifest section, no builder enable-API, no `app!`
  handler-path inspection** — the opt-in lives on `#[action(...)]`.

## Design evolution (for reviewers)

1. First cut: inject `IntrospectionData` on **every** request; handlers read
   `ctx.introspection()`. Rejected — taxes all traffic for rarely-hit endpoints.
2. Considered a process-global (`OnceLock`) source — rejected: one-manifest-
   per-process breaks unit tests and adds shared mutable state.
3. Considered `app!` recognizing the `edgezero_core::introspection::` handler
   namespace to flag routes — rejected as a fragile string-match hack.
4. **Final:** the opt-in is an atomic `#[action(manifest|routes)]` parameter;
   the capability rides the handler to registration via
   `DynHandler::needs_introspection()`; the router gates injection per route.
   Typed extractors (`ManifestJson`/`RouteTable`) are the access mechanism.
   No global, no `app!` hack, no unstable specialization, and `#[action]` (no
   params) is 100% unchanged so only `manifest`/`routes` become structs.
