# Design: Pluggable Introspection Routes (manifest / config / routes)

**Date:** 2026-07-01
**Status:** Approved — ready for implementation planning
**Scope:** `edgezero-core`, `edgezero-macros`, `examples/app-demo`, `edgezero-cli` templates

## Summary

Provide three reusable, framework-supplied HTTP handlers that let any EdgeZero
app expose its own metadata at runtime:

| Handler path                            | Emits                                             |
| --------------------------------------- | ------------------------------------------------- |
| `edgezero_core::introspection::manifest` | The full manifest as JSON (baked at compile time) |
| `edgezero_core::introspection::config`   | The default config-store envelope `.data` (secret-safe) |
| `edgezero_core::introspection::routes`   | `[{ "method", "path" }]` from the live route index |

These are ordinary handlers. Apps wire them like any other route via
`[[triggers.http]]` in `edgezero.toml`, choosing their own paths. There is **no**
special manifest section and **no** dedicated builder API. `app-demo` and every
generated app ship with the three routes pre-wired under a per-app namespace
`/_<app-name>/{manifest,config,routes}` (e.g. `/_app-demo/manifest`), but those
are plain trigger rows a developer can edit or delete.

This design also **removes** the existing built-in route-listing machinery
(`enable_route_listing`, `enable_route_listing_at`, `DEFAULT_ROUTE_LISTING_PATH`,
`/__edgezero/routes`, `RouteListingEntry`, `build_listing_response`) in favor of
the new bindable `routes` handler.

## Motivation

Today there is no runtime way to inspect what an app *is*:

- The **manifest** is compile-time only. `Manifest` derives `Deserialize` +
  `Validate` but not `Serialize`, and the portable-store rewrite removed the
  `run_app(include_str!("edgezero.toml"), …)` shape, so a running adapter binary
  no longer carries the manifest.
- The **app config** is reachable at runtime through the config store, but only
  via the typed `AppConfig<C>` extractor, which resolves secrets and requires the
  app's concrete config type.
- The only built-in introspection is an opt-in route listing at
  `/__edgezero/routes`, wired through a bespoke builder method
  (`enable_route_listing`) rather than the normal routing path.

We want a single, consistent, "bind it yourself" mechanism for all three.

## Key Decisions (resolved during design)

1. **Manifest output** — bake the full manifest as JSON. `Manifest` gains
   `Serialize`; the `app!` macro serializes the parsed manifest at expansion time
   and hands the JSON string to the router.
2. **Config output** — emit the raw config-store `BlobEnvelope.data`. This is
   generic (core needs no knowledge of the app's typed config `C`) and
   secret-safe: secret fields appear as unresolved key-name references, never
   resolved values (resolution only happens inside the typed `AppConfig<C>`
   extractor).
3. **Wiring** — plain `[[triggers.http]]` bindings referencing stable core
   handler paths. No `[introspection]` manifest section; no builder methods.
4. **Paths** — per-app namespace `/_<app-name>/{manifest,config,routes}`
   (single underscore). These are just the default paths written into the
   templates; the developer controls them.
5. **Injection, not a global** — the app-specific data (manifest JSON + route
   index) is injected into the request at the shared router dispatch chokepoint
   in core. No process-global state; no per-adapter changes.
6. **Remove route listing** — delete the entire `enable_route_listing` machinery
   and `/__edgezero/routes`.
7. **Typed extractors for access (revised 2026-07-02)** — handlers access the
   injected data through independent typed extractors, not `ctx.introspection()`.
   See the Revision section.

> **Revision — 2026-07-02: independent typed extractors + per-route gated injection.**
> Two changes to Decision 5 and Component 4:
>
> **(1) Access via independent typed extractors.** The two handlers that need
> introspection data declare the dependency via extractors, matching the
> `Json`/`Path`/`AppConfig` idiom, instead of reaching into `ctx.introspection()`:
> - `ManifestJson(pub Arc<str>)` — the baked manifest JSON. Used by `manifest`.
> - `RouteTable(pub Arc<[RouteInfo]>)` — the live route index. Used by `routes`.
> Both implement `FromRequest`, read the injected `IntrospectionData` via
> `ctx.introspection()`, and error (500 internal) if it is absent. `config` takes
> `RequestContext` and uses neither — it reads the default config store.
>
> **(2) Gated injection, driven by an explicit `#[action]` opt-in
> (supersedes Decision 5's "every request").** The router no longer injects
> `IntrospectionData` unconditionally. The capability is declared on the handler
> and rides it to registration — no `app!` change, no `edgezero.toml` change, no
> process-global, no unstable specialization:
>
> - **`#[action]` gains an optional parameter list.** `#[action]` (no args) is
>   **unchanged** — it still expands to a handler fn, and via the `Fn` blanket
>   `impl DynHandler` reports `needs_introspection() == false`. `#[action(introspection)]`
>   expands the handler to a **unit struct** with its own `impl DynHandler` whose
>   `needs_introspection()` returns `true`. The parameter list is future-proofed
>   (`#[action(introspection, …)]`); unknown params are a compile error. A fn
>   can't carry the flag past type-erasure, which is why opt-in handlers become
>   structs — but that only affects handlers that opt in (here: `manifest`,
>   `routes`); every other handler stays a fn, untouched.
> - **`DynHandler` gains `fn needs_introspection(&self) -> bool { false }`.**
> - **The router reads the flag generically at registration.** `add_route` calls
>   `boxed.needs_introspection()` and stores it on `RouteEntry`. `RouterInner`
>   precomputes a single `Arc<IntrospectionData>` at `build()`; `dispatch` inserts
>   a clone **only after matching a flagged route**. Non-introspection traffic
>   (virtually all requests) pays nothing. Each router owns its payload, so tests
>   and multiple apps in one process stay independent.
> - **The `ManifestJson`/`RouteTable` extractors are unchanged** — they remain the
>   *access* mechanism (read `ctx.introspection()`); `#[action(introspection)]` is
>   the *opt-in* that causes the data to be injected. `config` stays `#[action]`
>   and uses neither.
>
> **No app-facing change.** App authors write the same `[[triggers.http]]` row;
> `manifest`/`routes` carry `#[action(introspection)]` in core, so the flag is
> intrinsic to those handlers. There is NO `[introspection]` manifest section, no
> per-route knob in `edgezero.toml`, and `app!` emits ordinary `builder.get(...)`
> — it never inspects handler paths.
>
> Where this conflicts with Decision 5 ("every request") or Component 4 ("handler
> reads `ctx.introspection()`"), the Revision governs.

## Architecture

### Data flow

```
compile time                          runtime (per request)
------------                          ---------------------
edgezero.toml
  │  app!() macro parses Manifest
  │  serde_json::to_string(&manifest)
  ▼
build_router()
  builder.with_manifest_json("{…}")   RouterService::oneshot(req)
        │                               └─ RouterInner::dispatch(req)
        ▼                                    │  req.extensions_mut().insert(
RouterInner { manifest_json,                 │      IntrospectionData {
              route_index, … }               │        manifest_json, routes })
                                             ▼
                                    handler reads ctx.introspection()
                                      manifest → returns baked JSON
                                      routes   → projects route index
                                      config   → reads default config store
```

- **manifest**: parsed at compile time, re-serialized to JSON by the macro,
  baked as a string literal into `build_router()`, stored on `RouterInner`,
  injected into each request, returned verbatim. No runtime TOML dependency.
- **routes**: derived at request time from the live route index already held by
  `RouterInner` (the actually-registered routes, not a manifest projection).
- **config**: read at request time from the default config store; independent of
  the manifest JSON.

### Component 1 — `Manifest: Serialize` (`edgezero-core/src/manifest.rs`)

Add `Serialize` to the derive list on `Manifest` and every nested struct that
must appear in the output (`ManifestApp`, `ManifestTriggers`,
`ManifestHttpTrigger`, `ManifestEnvironment`, `ManifestBinding`,
`ManifestAdapter` and its sub-structs, `ManifestLogging*`, `ManifestStores`,
`StoreDeclaration`, etc.).

- Internal-only fields already carry `#[serde(skip)]` (`root`,
  `logging_resolved`) and stay out of the output.
- Secret **values** are never stored in the manifest — only binding
  declarations (name / env / description) — so serialized output is secret-safe.
- Verify round-trip is not required; this is a one-way (serialize-for-output)
  addition. Existing `Deserialize`/`Validate` behavior is unchanged.

### Component 2 — Router injection (`edgezero-core/src/router.rs`)

New public struct carrying the per-request introspection payload:

```rust
#[derive(Clone)]
pub struct IntrospectionData {
    pub manifest_json: Option<Arc<str>>,
    pub routes: Arc<[RouteInfo]>,
}
```

Changes:

- `RouterInner` gains `manifest_json: Option<Arc<str>>`.
- `RouterBuilder` gains `manifest_json: Option<Arc<str>>` plus a setter:
  ```rust
  pub fn with_manifest_json<S: Into<Arc<str>>>(mut self, json: S) -> Self { … }
  ```
  `build()` threads it into `RouterService::new(...)` / `RouterInner`.
- `RouterInner::dispatch(mut req)` inserts the extension **before** middleware and
  routing:
  ```rust
  req.extensions_mut().insert(IntrospectionData {
      manifest_json: self.manifest_json.clone(),
      routes: Arc::clone(&self.route_index),
  });
  ```
  `route_index` is already an `Arc<[RouteInfo]>`, so the clone is cheap.

### Component 3 — `RequestContext` accessor (`edgezero-core/src/context.rs`)

```rust
#[inline]
pub fn introspection(&self) -> Option<&IntrospectionData> {
    self.request.extensions().get::<IntrospectionData>()
}
```

Mirrors the existing extension-backed accessors (`config_store*`, `kv_store*`).

### Component 4 — `edgezero_core::introspection` module (new file)

Three handlers written with `#[action]`, plus a small JSON shape for `routes`.

```rust
/// GET — full manifest as JSON.
#[action]
pub async fn manifest(ctx: RequestContext) -> Result<Response, EdgeError> {
    let json = ctx
        .introspection()
        .and_then(|d| d.manifest_json.clone())
        .ok_or_else(|| EdgeError::internal("manifest introspection data not available"))?;
    // application/json, body = json verbatim
}

/// GET — [{ "method", "path" }] for every registered route.
#[action]
pub async fn routes(ctx: RequestContext) -> Result<Response, EdgeError> {
    let routes = ctx.introspection().map(|d| &d.routes) /* → Vec<RouteEntryView> */;
    // application/json
}

/// GET — the default config-store envelope `data` (secret-safe).
#[action]
pub async fn config(ctx: RequestContext) -> Result<Response, EdgeError> {
    let binding = ctx.config_store_default_binding()
        .ok_or_else(|| EdgeError::not_found("no default config store registered"))?;
    // read raw blob at binding.default_key via binding.handle
    // parse BlobEnvelope, emit envelope.data as application/json
}
```

Notes:

- `RouteEntryView { method: String, path: String }` replaces the removed
  `RouteListingEntry`.
- `config` reads the raw blob string from the config-store handle (the same read
  `extract_from_handle` performs) and parses `BlobEnvelope`; it does **not** run
  secret resolution or typed deserialization.
- Error mapping: absent manifest → `500` internal (should not happen once wired);
  missing config store or missing blob → `404`.
- The handlers must be reachable by the `app!` macro's `parse_handler_path`,
  which already resolves arbitrary `a::b::c` paths (it resolves
  `app_demo_core::handlers::root` today), so `edgezero_core::introspection::…`
  resolves the same way.

### Component 5 — `app!` macro (`edgezero-macros/src/app.rs`)

- After parsing the manifest, serialize it: `serde_json::to_string(&manifest)`.
  On serialization error, emit a `compile_error!`.
- Emit one added line in the generated `build_router()`:
  ```rust
  pub fn build_router() -> edgezero_core::router::RouterService {
      let mut builder = edgezero_core::router::RouterService::builder();
      builder = builder.with_manifest_json(#manifest_json_lit);
      #(#middleware_tokens)*
      #(#route_tokens)*
      builder.build()
  }
  ```
- No route wiring for introspection (routes come from `[[triggers.http]]`).
- `edgezero-macros` needs `serde_json` as a (build-time) dependency; `Manifest`
  must be `Serialize` (Component 1).

### Component 6 — Removals

Delete from `edgezero-core/src/router.rs`:

- `pub const DEFAULT_ROUTE_LISTING_PATH`
- `RouterBuilder::enable_route_listing`, `RouterBuilder::enable_route_listing_at`
- `RouterBuilder.route_listing_path` field and the listing branch inside `build()`
- `build_listing_response`
- `RouteListingEntry`
- All associated unit tests (`route_listing_*`)

Grep the workspace for any other references (docs, examples, adapter code) and
remove/update them so nothing depends on `/__edgezero/routes`.

### Component 7 — Templates (default bindings)

Add three trigger rows, wired to the core handlers, under `/_<app-name>/…`.

`examples/app-demo/edgezero.toml`:

```toml
[[triggers.http]]
id = "manifest"
path = "/_app-demo/manifest"
methods = ["GET"]
handler = "edgezero_core::introspection::manifest"
description = "App manifest as JSON"

[[triggers.http]]
id = "config"
path = "/_app-demo/config"
methods = ["GET"]
handler = "edgezero_core::introspection::config"
description = "Effective app config (secret-safe)"

[[triggers.http]]
id = "routes"
path = "/_app-demo/routes"
methods = ["GET"]
handler = "edgezero_core::introspection::routes"
description = "Registered route table"
```

`crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`: the same three rows,
using `path = "/_{{name}}/manifest"` etc. and the same `edgezero_core::introspection::*`
handlers. (`{{name}}` is the sanitized app name already used elsewhere in the
template.)

No template handler code is generated — the handlers live in core.

## Interfaces (summary)

| Unit                    | Public surface                                             | Depends on                          |
| ----------------------- | ---------------------------------------------------------- | ----------------------------------- |
| `IntrospectionData`     | `{ manifest_json: Option<Arc<str>>, routes: Arc<[RouteInfo]> }` | `RouteInfo`                    |
| `RouterBuilder`         | `with_manifest_json(impl Into<Arc<str>>)`                  | —                                   |
| `RequestContext`        | `introspection() -> Option<&IntrospectionData>`            | request extensions                  |
| `introspection::manifest` | `#[action]` GET → JSON                                    | `ctx.introspection()`               |
| `introspection::routes` | `#[action]` GET → JSON                                     | `ctx.introspection()`               |
| `introspection::config` | `#[action]` GET → JSON                                     | default config store, `BlobEnvelope`|

## Error Handling

- **manifest** absent from `IntrospectionData`: `500 internal` (indicates a
  wiring bug; always present once the macro sets it).
- **config**: no default config store → `404 not found`; no blob at
  `default_key` → `404`; malformed envelope → `500 internal`.
- **routes**: `IntrospectionData` absent → empty list is acceptable, or `500`;
  chosen behavior: return an empty array rather than error, since routes are
  always injected by dispatch.

## Testing Strategy

Colocated `#[cfg(test)]`, `futures::executor::block_on` (no Tokio), no network.

- **router.rs**: dispatch test asserting an `IntrospectionData` extension is
  present in the request seen by a handler, with the expected route index and
  `manifest_json`. Remove old `route_listing_*` tests.
- **introspection module**:
  - `manifest` returns the injected JSON with `application/json`.
  - `routes` returns the projected `[{method, path}]`.
  - `config` returns `BlobEnvelope.data` from a stub config store; `404` when no
    store is registered; `404` when the blob is missing.
- **macro (`edgezero-macros`)**: trybuild/expansion assertion that
  `with_manifest_json(...)` is emitted with valid JSON for a sample manifest.
- **app-demo**: extend router/handler tests to hit `/_app-demo/manifest`,
  `/_app-demo/config`, `/_app-demo/routes` and assert shapes.

## CI Gates (unchanged)

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-targets`
4. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
5. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`

## Constraints & Non-Goals

- **WASM-first**: no Tokio, no runtime-specific deps added. `Arc`, `serde_json`,
  and `Once*`-free injection are all WASM-safe. `serde_json` is added only to
  `edgezero-macros` (a proc-macro crate that runs at build time).
- **No auth/gating in this iteration**: endpoints are exposed wherever the app
  binds them. Because they are plain triggers, a developer who does not want them
  simply omits the rows. Config output is already secret-safe. Access control
  (e.g. dev-only, header-gated) is a possible follow-up, out of scope here.
- **Single-app assumption**: `manifest_json` is per-`RouterService`, so multiple
  distinct apps in one process each carry their own — no shared/global state and
  no cross-app leakage.
- **No `[introspection]` manifest section** and **no builder-based enable API** —
  explicitly rejected in favor of plain `[[triggers.http]]` bindings.

## File-Change Checklist (for planning)

- [ ] `crates/edgezero-core/src/manifest.rs` — add `Serialize` derives.
- [ ] `crates/edgezero-core/src/router.rs` — `IntrospectionData`,
      `with_manifest_json`, dispatch injection; remove route-listing machinery +
      tests.
- [ ] `crates/edgezero-core/src/context.rs` — `introspection()` accessor.
- [ ] `crates/edgezero-core/src/introspection.rs` — new module, three handlers.
- [ ] `crates/edgezero-core/src/lib.rs` — export `introspection`.
- [ ] `crates/edgezero-macros/src/app.rs` — serialize manifest, emit
      `with_manifest_json`; add `serde_json` dep.
- [ ] `examples/app-demo/edgezero.toml` — three trigger rows.
- [ ] `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs` — three trigger
      rows using `{{name}}`.
- [ ] Workspace grep — purge remaining `/__edgezero/routes` /
      `enable_route_listing` references (docs, examples, adapters).
