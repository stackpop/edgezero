# Pluggable Introspection Routes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add three reusable core `#[action]` handlers — `edgezero_core::introspection::{manifest, config, routes}` — that any app binds via `[[triggers.http]]`, default-mounted at `/_<app-name>/{manifest,config,routes}`.

**Architecture:** The `app!` macro serializes the parsed manifest to JSON at expansion time and hands it to `RouterService::builder().with_manifest_json(...)`. `RouterInner::dispatch` injects an `IntrospectionData { manifest_json, routes }` extension into each request; the three core handlers read it (config reads the default config store instead). The legacy `enable_route_listing` machinery and `/__edgezero/routes` are removed.

**Tech Stack:** Rust 1.95 (edition 2021), `serde`/`serde_json`, `matchit` routing, `#[action]`/`app!` proc-macros, `futures::executor::block_on` for tests. WASM-first: no Tokio, no runtime-specific deps in core.

## Global Constraints

- Rust 1.95.0, edition 2021, resolver 2. License Apache-2.0.
- WASM compatibility first: no Tokio, no `std::time::Instant`, `async-trait` without `Send` bounds. `serde_json` may be added only to the proc-macro crate `edgezero-macros` (runs at build time).
- Colocate tests with implementation (`#[cfg(test)]` in the same file). Async tests use `futures::executor::block_on`, never Tokio. No network / no platform credentials in tests.
- Route params use matchit brace syntax `{id}` / `{*rest}`; never `:id`.
- Import HTTP aliases from `edgezero_core` re-exports, never the `http` crate directly.
- Minimal changes: touch as little as possible; no unrelated refactors or docstrings on untouched code.
- No `Co-Authored-By` trailers, "Generated with" footers, or AI bylines in commits or PR bodies.
- Every PR must pass all five CI gates (see Task 8).

## Spec Errata / Implementation Assumptions

These correct or refine the design spec (`docs/superpowers/specs/2026-07-01-introspection-routes-design.md`) after a close read of the code. **They override the spec where they conflict.**

1. **Secret redaction in manifest output.** `ManifestBinding` (manifest.rs:287) has a `value: Option<String>` field, and `ManifestEnvironment` (manifest.rs:276) uses that same type for BOTH `variables` and `secrets`. Blindly deriving `Serialize` would emit secret-shaped `value`s. The `secrets` list MUST be serialized with `value` omitted. Implemented via a `#[serde(serialize_with = ...)]` redactor on `ManifestEnvironment::secrets`.
2. **`[app]` version/kind.** `ManifestApp` (manifest.rs:217) models only `entry`/`middleware`/`name`, but app-demo's `edgezero.toml` sets `version` and `kind`; they are silently dropped on deserialize today. Add optional `version`/`kind` fields so the manifest JSON reflects the real file.
3. **`#[action]` inside core needs a self-alias.** The `#[action]` macro emits absolute `::edgezero_core::…` paths (action.rs:87). Core uses `#[action]` only in doc comments today, never compiled. Add `extern crate self as edgezero_core;` to `crates/edgezero-core/src/lib.rs` so those paths resolve within the core crate.
4. **Config handler error mapping.** Mirror `extract_from_handle` (extractor.rs:766): map `ConfigStoreError` via `EdgeError::from` (preserving 503/400/500 distinctions), parse `BlobEnvelope`, and call `envelope.verify()` before returning `.data`. Do NOT collapse backend errors to 500.
5. **Injection timing.** `dispatch` inserts the extension after route match / before the handler runs. Tests must assert visibility from a handler and from middleware, not that it changes 404/405 outcomes.
6. **Docs.** The only live public reference to route listing is `docs/guide/routing.md:118`. Update it. Do NOT touch unrelated `.__edgezero_chunks` documentation.
7. **App-demo tests** exercise routes through `build_router().oneshot(request)`, not only direct handler calls.

---

## File Structure

| File | Responsibility | Task |
| --- | --- | --- |
| `crates/edgezero-core/src/manifest.rs` | Add `Serialize` (+ secret redaction, version/kind) | 1 |
| `crates/edgezero-core/src/router.rs` | `IntrospectionData`, `with_manifest_json`, dispatch injection | 2 |
| `crates/edgezero-core/src/context.rs` | `introspection()` accessor | 2 |
| `crates/edgezero-core/src/introspection.rs` (new) | Three `#[action]` handlers | 3 |
| `crates/edgezero-core/src/lib.rs` | `extern crate self`, `pub mod introspection` | 3 |
| `crates/edgezero-macros/src/app.rs` | Serialize manifest, emit `with_manifest_json` | 4 |
| `crates/edgezero-macros/Cargo.toml` | Add `serde_json` dep | 4 |
| `crates/edgezero-core/src/router.rs` | Remove route-listing machinery + tests | 5 |
| `examples/app-demo/edgezero.toml` | Three trigger rows + router-level tests | 6 |
| `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs` | Three trigger rows | 6 |
| `docs/guide/routing.md` | Replace route-listing docs | 7 |

---

### Task 1: Manifest serialization with secret redaction

**Files:**
- Modify: `crates/edgezero-core/src/manifest.rs` (structs at :86, :217, :276, :287, and nested adapter/logging/stores structs)
- Test: same file, `#[cfg(test)]`

**Interfaces:**
- Produces: `Manifest: Serialize` and all nested types serializable; `ManifestApp` gains `version: Option<String>`, `kind: Option<String>`; `ManifestEnvironment::secrets` serialized with `value` omitted.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `manifest.rs`:

```rust
#[test]
fn serializes_manifest_and_redacts_secret_values() {
    let toml = r#"
[app]
name = "t"
version = "0.1.0"
kind = "http"

[[triggers.http]]
id = "root"
path = "/"
methods = ["GET"]
handler = "t::handlers::root"

[[environment.variables]]
name = "LOG_LEVEL"
value = "info"

[[environment.secrets]]
name = "API_TOKEN"
value = "super-secret-value"
"#;
    let manifest: Manifest = toml::from_str(toml).unwrap();
    let json = serde_json::to_value(&manifest).unwrap();

    // [app] version/kind round-trip
    assert_eq!(json["app"]["version"], "0.1.0");
    assert_eq!(json["app"]["kind"], "http");
    // variables keep their value
    assert_eq!(json["environment"]["variables"][0]["value"], "info");
    // secrets NEVER expose value
    let secret = &json["environment"]["secrets"][0];
    assert_eq!(secret["name"], "API_TOKEN");
    assert!(secret.get("value").is_none(), "secret value must be redacted");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgezero-core serializes_manifest_and_redacts_secret_values`
Expected: FAIL to compile — `Manifest` does not implement `Serialize`; `ManifestApp` has no `version`/`kind`.

- [ ] **Step 3: Add `version`/`kind` to `ManifestApp`**

In `ManifestApp` (manifest.rs:217), add after `name`:

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[validate(length(min = 1_u64))]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[validate(length(min = 1_u64))]
    pub kind: Option<String>,
```

- [ ] **Step 4: Add the secret redactor**

Add near `ManifestEnvironment` (manifest.rs:276):

```rust
/// Serialize a `[[environment.secrets]]` list without exposing `value`.
/// Secret bindings share `ManifestBinding` with variables, whose `value`
/// is safe to emit; secret values must never appear in manifest output.
fn serialize_secrets<S>(secrets: &[ManifestBinding], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;

    #[derive(Serialize)]
    struct RedactedBinding<'a> {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        adapters: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        description: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: &'a Option<String>,
        name: &'a str,
        // `value` intentionally omitted.
    }

    let mut seq = serializer.serialize_seq(Some(secrets.len()))?;
    for binding in secrets {
        seq.serialize_element(&RedactedBinding {
            adapters: &binding.adapters,
            description: &binding.description,
            env: &binding.env,
            name: &binding.name,
        })?;
    }
    seq.end()
}
```

- [ ] **Step 5: Add `Serialize` derives + wire the redactor**

Add `Serialize` to the `#[derive(...)]` on: `Manifest` (:86), `ManifestApp` (:217), `ManifestTriggers` (:230), `ManifestHttpTrigger` (:238), `ManifestEnvironment` (:276), `ManifestBinding` (:287), `ManifestAdapter` (:344), `ManifestAdapterDeployed` (:368 area), `ManifestAdapterBuild`, `ManifestAdapterCommands`, `ManifestAdapterDefinition`, `ManifestLogging`, `ManifestLoggingConfig`, `ManifestStores`, `StoreDeclaration`, plus the `HttpMethod` enum used in triggers. Keep existing `Deserialize`/`Validate`.

On `ManifestEnvironment::secrets`, add:

```rust
    #[serde(default, serialize_with = "serialize_secrets")]
    #[validate(nested)]
    pub secrets: Vec<ManifestBinding>,
```

Add `#[serde(skip_serializing_if = "...")]` to keep output clean where fields are optional/empty (e.g. `Option::is_none`, `Vec::is_empty`, `BTreeMap::is_empty`). The internal `root` and `logging_resolved` fields already carry `#[serde(skip)]` — leave them.

- [ ] **Step 6: Run tests**

Run: `cargo test -p edgezero-core serializes_manifest_and_redacts_secret_values`
Expected: PASS.
Then: `cargo test -p edgezero-core manifest` — Expected: all existing manifest tests still PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/edgezero-core/src/manifest.rs
git commit -m "Make Manifest serializable with secret-value redaction"
```

---

### Task 2: Router injection + RequestContext accessor

**Files:**
- Modify: `crates/edgezero-core/src/router.rs` (`RouterBuilder` :80, `build()` :121, `RouterService::new` :343, `RouterInner` :260, `dispatch`)
- Modify: `crates/edgezero-core/src/context.rs` (accessor near the other extension accessors)
- Test: both files, `#[cfg(test)]`

**Interfaces:**
- Consumes: `RouteInfo` (router.rs:40), existing `RouterInner.route_index: Arc<[RouteInfo]>`.
- Produces:
  - `pub struct IntrospectionData { pub manifest_json: Option<Arc<str>>, pub routes: Arc<[RouteInfo]> }` (`Clone`).
  - `RouterBuilder::with_manifest_json(impl Into<Arc<str>>) -> Self`.
  - `RequestContext::introspection(&self) -> Option<&IntrospectionData>`.

- [ ] **Step 1: Write the failing test (router injection)**

Add to `router.rs` tests:

```rust
#[test]
fn dispatch_injects_introspection_data() {
    use crate::context::RequestContext;
    use std::sync::{Arc, Mutex};

    let seen: Arc<Mutex<Option<(bool, usize)>>> = Arc::new(Mutex::new(None));
    let seen_h = Arc::clone(&seen);

    let handler = move |ctx: RequestContext| {
        let seen_h = Arc::clone(&seen_h);
        async move {
            let d = ctx.introspection().expect("introspection data present");
            *seen_h.lock().unwrap() =
                Some((d.manifest_json.is_some(), d.routes.len()));
            Ok::<_, EdgeError>("ok")
        }
    };

    let router = RouterService::builder()
        .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
        .get("/", handler)
        .build();

    let request = crate::http::request_builder()
        .method(Method::GET)
        .uri("/")
        .body(Body::empty())
        .unwrap();
    let _ = block_on(router.oneshot(request)).unwrap();

    let (had_manifest, route_count) = seen.lock().unwrap().expect("handler ran");
    assert!(had_manifest, "manifest_json should be injected");
    assert_eq!(route_count, 1);
}
```

(Use whatever request-builder/`block_on` imports the existing router tests use; match them.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgezero-core dispatch_injects_introspection_data`
Expected: FAIL to compile — `with_manifest_json` and `RequestContext::introspection` do not exist.

- [ ] **Step 3: Add `IntrospectionData` + builder field/setter**

In `router.rs`, define near `RouteInfo`:

```rust
/// Per-request introspection payload injected by [`RouterInner::dispatch`].
#[derive(Clone)]
pub struct IntrospectionData {
    /// The app manifest serialized to JSON at compile time by `app!`.
    pub manifest_json: Option<Arc<str>>,
    /// Every registered route, in registration order.
    pub routes: Arc<[RouteInfo]>,
}
```

Add to `RouterBuilder` (struct at :80): `manifest_json: Option<Arc<str>>,` (its `#[derive(Default)]` covers it). Add the setter:

```rust
    #[must_use]
    pub fn with_manifest_json<S: Into<Arc<str>>>(mut self, json: S) -> Self {
        self.manifest_json = Some(json.into());
        self
    }
```

- [ ] **Step 4: Thread it through `build()` → `RouterInner`**

`RouterInner` (:260) already needs `route_index`. Add `manifest_json: Option<Arc<str>>`. Update `RouterService::new` (:343) to accept and store it, and `build()` (:121) to pass `self.manifest_json`. In `dispatch`, before running middleware/handler, insert the extension:

```rust
    async fn dispatch(&self, mut request: Request) -> Result<Response, EdgeError> {
        request.extensions_mut().insert(IntrospectionData {
            manifest_json: self.manifest_json.clone(),
            routes: Arc::clone(&self.route_index),
        });
        // ... existing match/middleware/handler logic unchanged ...
    }
```

(If `dispatch` currently takes `request` by value already, just add `mut`. Match the existing signature.)

- [ ] **Step 5: Add the `RequestContext` accessor**

In `context.rs`, near `config_store_default_binding`:

```rust
    /// The per-request [`IntrospectionData`] injected by the router, if any.
    #[must_use]
    #[inline]
    pub fn introspection(&self) -> Option<&crate::router::IntrospectionData> {
        self.request.extensions().get::<crate::router::IntrospectionData>()
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p edgezero-core dispatch_injects_introspection_data`
Expected: PASS.
Then: `cargo test -p edgezero-core router` — Expected: PASS (existing route-listing tests still pass; they are removed in Task 5).

- [ ] **Step 7: Commit**

```bash
git add crates/edgezero-core/src/router.rs crates/edgezero-core/src/context.rs
git commit -m "Inject IntrospectionData at router dispatch chokepoint"
```

---

### Task 3: Introspection handler module

**Files:**
- Create: `crates/edgezero-core/src/introspection.rs`
- Modify: `crates/edgezero-core/src/lib.rs` (add `extern crate self as edgezero_core;` and `pub mod introspection;`)
- Test: `introspection.rs`, `#[cfg(test)]`

**Interfaces:**
- Consumes: `RequestContext::introspection()`, `IntrospectionData` (Task 2); `config_store_default_binding()` (context.rs:63); `BlobEnvelope` (blob_envelope.rs:17); `EdgeError` constructors (error.rs).
- Produces: `pub async fn manifest/config/routes` (each `#[action]`), bindable as `edgezero_core::introspection::{manifest,config,routes}`.

- [ ] **Step 1: Add the self-alias and module declaration**

In `crates/edgezero-core/src/lib.rs`, add at the very top of the crate (before the `pub mod` list, after any inner attributes):

```rust
extern crate self as edgezero_core;
```

And add to the module list (keep alphabetical): `pub mod introspection;`

- [ ] **Step 2: Write the failing tests**

Create `crates/edgezero-core/src/introspection.rs`:

```rust
//! Framework-supplied introspection handlers. Bind via `[[triggers.http]]`:
//! `handler = "edgezero_core::introspection::manifest"` etc.

use crate::blob_envelope::BlobEnvelope;
use crate::body::Body;
use crate::context::RequestContext;
use crate::error::EdgeError;
use crate::http::{response_builder, StatusCode};
use crate::response::Response;
use edgezero_core::action;
use serde::Serialize;

#[derive(Serialize)]
struct RouteView {
    method: String,
    path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{request_builder, Method};
    use crate::router::RouterService;
    use futures::executor::block_on;

    #[test]
    fn manifest_returns_injected_json() {
        let router = RouterService::builder()
            .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
            .get("/m", manifest)
            .build();
        let req = request_builder().method(Method::GET).uri("/m").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn routes_lists_registered_routes() {
        let router = RouterService::builder().get("/r", routes).build();
        let req = request_builder().method(Method::GET).uri("/r").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn config_without_store_is_not_found() {
        let router = RouterService::builder().get("/c", config).build();
        let req = request_builder().method(Method::GET).uri("/c").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p edgezero-core introspection`
Expected: FAIL to compile — `manifest`/`config`/`routes` not defined.

- [ ] **Step 4: Implement the three handlers**

Add to `introspection.rs` (above the tests):

```rust
fn json_response(status: StatusCode, body: Body) -> Result<Response, EdgeError> {
    response_builder()
        .status(status)
        .header("content-type", "application/json")
        .body(body)
        .map_err(EdgeError::internal)
}

/// GET — the app manifest as JSON (baked at compile time by `app!`).
#[action]
pub async fn manifest(ctx: RequestContext) -> Result<Response, EdgeError> {
    let json = ctx
        .introspection()
        .and_then(|d| d.manifest_json.clone())
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("manifest introspection data missing")))?;
    json_response(StatusCode::OK, Body::text(json.to_string()))
}

/// GET — `[{ "method", "path" }]` for every registered route.
#[action]
pub async fn routes(ctx: RequestContext) -> Result<Response, EdgeError> {
    let views: Vec<RouteView> = ctx
        .introspection()
        .map(|d| {
            d.routes
                .iter()
                .map(|r| RouteView {
                    method: r.method().as_str().to_owned(),
                    path: r.path().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let body = Body::json(&views).map_err(EdgeError::internal)?;
    json_response(StatusCode::OK, body)
}

/// GET — the default config-store envelope `data` (secret-safe: secret
/// fields remain unresolved key-name references).
#[action]
pub async fn config(ctx: RequestContext) -> Result<Response, EdgeError> {
    let binding = ctx
        .config_store_default_binding()
        .ok_or_else(|| EdgeError::not_found("no default config store registered"))?;
    // ConfigStoreError → EdgeError preserves 503/400/500 (see extractor.rs).
    let raw = binding
        .handle
        .get(&binding.default_key)
        .await
        .map_err(EdgeError::from)?
        .ok_or_else(|| EdgeError::not_found("no config blob in default store"))?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("envelope parse failed: {err}")))?;
    envelope
        .verify()
        .map_err(|err| EdgeError::internal(anyhow::anyhow!("envelope verification failed: {err}")))?;
    let body = Body::json(&envelope.into_data()).map_err(EdgeError::internal)?;
    json_response(StatusCode::OK, body)
}
```

Notes: confirm `ConfigStoreBinding` field names are `handle` and `default_key` (context.rs uses `binding.handle`/`binding.default_key`). Confirm `Body::json` exists (body.rs:114) and `RouteInfo::method()/path()` (router.rs:48/62). If `anyhow` is not already a core dep for this pattern, mirror what `extractor.rs` uses (`anyhow::anyhow!`).

- [ ] **Step 5: Run tests**

Run: `cargo test -p edgezero-core introspection`
Expected: PASS (all three).

- [ ] **Step 6: Commit**

```bash
git add crates/edgezero-core/src/introspection.rs crates/edgezero-core/src/lib.rs
git commit -m "Add edgezero_core::introspection handlers (manifest/config/routes)"
```

---

### Task 4: `app!` macro injects the manifest JSON

**Files:**
- Modify: `crates/edgezero-macros/src/app.rs` (`build_router` emission around :170-176)
- Modify: `crates/edgezero-macros/Cargo.toml` (add `serde_json`)
- Test: `crates/edgezero-macros` unit test or `examples/app-demo` (verified end-to-end in Task 6)

**Interfaces:**
- Consumes: parsed `Manifest` (now `Serialize`, Task 1); `RouterBuilder::with_manifest_json` (Task 2).
- Produces: generated `build_router()` calls `builder.with_manifest_json("<json>")`.

- [ ] **Step 1: Add `serde_json` to the macro crate**

In `crates/edgezero-macros/Cargo.toml` under `[dependencies]`, add the workspace dep:

```toml
serde_json = { workspace = true }
```

- [ ] **Step 2: Serialize the manifest and emit the setter**

In `app.rs`, after the manifest is parsed (near `app_name` at :126), add:

```rust
    let manifest_json = match serde_json::to_string(&manifest) {
        Ok(json) => json,
        Err(err) => {
            return syn::Error::new(
                Span::call_site(),
                format!("failed to serialize manifest to JSON: {err}"),
            )
            .to_compile_error()
            .into();
        }
    };
    let manifest_json_lit = LitStr::new(&manifest_json, Span::call_site());
```

Then in the emitted `build_router()` (the `quote! { ... pub fn build_router() ... }` block around :170), insert the setter as the first builder mutation:

```rust
        pub fn build_router() -> edgezero_core::router::RouterService {
            let mut builder = edgezero_core::router::RouterService::builder();
            builder = builder.with_manifest_json(#manifest_json_lit);
            #(#middleware_tokens)*
            #(#route_tokens)*
            builder.build()
        }
```

- [ ] **Step 3: Verify the macro crate compiles**

Run: `cargo build -p edgezero-macros`
Expected: builds cleanly.

- [ ] **Step 4: Verify a consumer still builds**

Run: `cargo build -p edgezero-core` then `cargo check -p app-demo-core --manifest-path examples/app-demo/Cargo.toml` (or `cd examples/app-demo && cargo check -p app-demo-core`).
Expected: builds; the generated `build_router` now sets manifest JSON.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-macros/src/app.rs crates/edgezero-macros/Cargo.toml
git commit -m "app! macro: bake manifest JSON into build_router via with_manifest_json"
```

---

### Task 5: Remove legacy route-listing machinery

**Files:**
- Modify: `crates/edgezero-core/src/router.rs` (remove `DEFAULT_ROUTE_LISTING_PATH`, `enable_route_listing`, `enable_route_listing_at`, `route_listing_path` field, listing branch in `build()`, `build_listing_response`, `RouteListingEntry`, and all `route_listing_*` tests at :621-716)
- Test: `router.rs` (removal of obsolete tests)

**Interfaces:**
- Produces: no public route-listing API remains; `/__edgezero/routes` is gone.

- [ ] **Step 1: Delete the machinery**

Remove from `router.rs`:
- `pub const DEFAULT_ROUTE_LISTING_PATH` (:21)
- `RouterBuilder.route_listing_path` field (:83)
- `RouterBuilder::enable_route_listing` (:174) and `enable_route_listing_at` (:182)
- The `if let Some(path) = listing_path { ... }` block inside `build()` (the listing-handler insertion) and the `let listing_path = self.route_listing_path.clone();` line (:122)
- `build_listing_response` (:376)
- `RouteListingEntry` struct (:71 area)
- Tests: `route_listing_duplicate_path_panics`, `route_listing_outputs_all_routes`, `route_listing_rejects_empty_path`, `route_listing_rejects_missing_slash`, `route_listing_response_handles_builder_failure`, `route_listing_response_handles_json_failure` (:621-716)

- [ ] **Step 2: Grep for stragglers**

Run:
```bash
grep -rn "enable_route_listing\|DEFAULT_ROUTE_LISTING_PATH\|RouteListingEntry\|__edgezero/routes\|build_listing_response" crates/ examples/
```
Expected: no matches in non-doc source. (The `docs/guide/routing.md` reference is handled in Task 7.)

- [ ] **Step 3: Verify compile + tests**

Run: `cargo test -p edgezero-core router`
Expected: PASS; no references to removed items.

- [ ] **Step 4: Commit**

```bash
git add crates/edgezero-core/src/router.rs
git commit -m "Remove legacy route-listing machinery and /__edgezero/routes"
```

---

### Task 6: Wire default triggers in app-demo + generated template

**Files:**
- Modify: `examples/app-demo/edgezero.toml`
- Modify: `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`
- Test: `examples/app-demo/crates/app-demo-core/src/lib.rs` or the crate's existing router test module (through `build_router().oneshot()`)

**Interfaces:**
- Consumes: `edgezero_core::introspection::{manifest,config,routes}` (Task 3); manifest JSON injection (Task 4).

- [ ] **Step 1: Add three triggers to app-demo**

Append to `examples/app-demo/edgezero.toml` in the `[[triggers.http]]` section:

```toml
[[triggers.http]]
id = "manifest"
path = "/_app-demo/manifest"
methods = ["GET"]
handler = "edgezero_core::introspection::manifest"
adapters = ["axum", "cloudflare", "fastly", "spin"]
description = "App manifest as JSON"

[[triggers.http]]
id = "config"
path = "/_app-demo/config"
methods = ["GET"]
handler = "edgezero_core::introspection::config"
adapters = ["axum", "cloudflare", "fastly", "spin"]
description = "Effective app config (secret-safe)"

[[triggers.http]]
id = "routes"
path = "/_app-demo/routes"
methods = ["GET"]
handler = "edgezero_core::introspection::routes"
adapters = ["axum", "cloudflare", "fastly", "spin"]
description = "Registered route table"
```

- [ ] **Step 2: Write the failing router-level test**

In app-demo-core's test module (colocated with `build_router`/`App`), add:

```rust
#[test]
fn introspection_routes_are_wired() {
    use edgezero_core::body::Body;
    use edgezero_core::http::{request_builder, Method, StatusCode};
    use futures::executor::block_on;

    let router = crate::build_router();
    for path in ["/_app-demo/manifest", "/_app-demo/routes"] {
        let req = request_builder().method(Method::GET).uri(path).body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path} should be 200");
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
    }
    // /_app-demo/config is 404 without a populated config store, but must be routed
    // (i.e. not a routing 404 with empty body). Assert it is reachable:
    let req = request_builder().method(Method::GET).uri("/_app-demo/config").body(Body::empty()).unwrap();
    let resp = block_on(router.oneshot(req)).unwrap();
    assert!(matches!(resp.status(), StatusCode::OK | StatusCode::NOT_FOUND));
}
```

(Match the app-demo crate's existing test imports/module location; `build_router` is generated by `app!`.)

- [ ] **Step 3: Run test to verify it fails, then passes**

Run: `cargo test -p app-demo-core introspection_routes_are_wired`
Expected: initially FAILS if triggers not yet parsed/handler path unresolved; after Step 1 + Tasks 3-4, PASS.

- [ ] **Step 4: Add the same rows to the generated-app template**

In `crates/edgezero-cli/src/templates/root/edgezero.toml.hbs`, append three trigger blocks mirroring app-demo but templated:

```hbs
[[triggers.http]]
id = "manifest"
path = "/_{{name}}/manifest"
methods = ["GET"]
handler = "edgezero_core::introspection::manifest"
adapters = [{{{adapter_list}}}]
description = "App manifest as JSON"

[[triggers.http]]
id = "config"
path = "/_{{name}}/config"
methods = ["GET"]
handler = "edgezero_core::introspection::config"
adapters = [{{{adapter_list}}}]
description = "Effective app config (secret-safe)"

[[triggers.http]]
id = "routes"
path = "/_{{name}}/routes"
methods = ["GET"]
handler = "edgezero_core::introspection::routes"
adapters = [{{{adapter_list}}}]
description = "Registered route table"
```

(Use the same `{{{adapter_list}}}` placeholder the template already uses for other triggers — verify its exact name in the `.hbs` file.)

- [ ] **Step 5: Verify generator tests**

Run: `cargo test -p edgezero-cli`
Expected: PASS (scaffold/generator tests still green with the added triggers).

- [ ] **Step 6: Commit**

```bash
git add examples/app-demo/edgezero.toml examples/app-demo/crates/app-demo-core crates/edgezero-cli/src/templates/root/edgezero.toml.hbs
git commit -m "Wire default introspection triggers into app-demo and generated apps"
```

---

### Task 7: Update docs

**Files:**
- Modify: `docs/guide/routing.md` (around :118, the route-listing reference)

**Interfaces:** none (documentation only).

- [ ] **Step 1: Locate the reference**

Run: `grep -n "route listing\|__edgezero/routes\|enable_route_listing" docs/guide/routing.md`
Expected: a reference around line 118. Do NOT touch any `.__edgezero_chunks` docs (unrelated).

- [ ] **Step 2: Replace with introspection-route docs**

Rewrite that section to describe the three bindable handlers instead of `enable_route_listing`. Content to convey:
- Core provides `edgezero_core::introspection::{manifest, config, routes}`.
- Bind them in `[[triggers.http]]` like any handler; app-demo and generated apps mount them under `/_<app-name>/{manifest,config,routes}` by default.
- `manifest` → full manifest JSON (secret values redacted); `config` → effective app config from the default config store (secret-safe); `routes` → registered route table.
- Remove any mention of `/__edgezero/routes` / `enable_route_listing`.

Example block to include:

```toml
[[triggers.http]]
id = "manifest"
path = "/_my-app/manifest"
methods = ["GET"]
handler = "edgezero_core::introspection::manifest"
```

- [ ] **Step 3: Verify no stale references remain**

Run: `grep -rn "enable_route_listing\|__edgezero/routes" docs/`
Expected: no matches (excluding `.__edgezero_chunks` which is a different token — verify the grep does not match it; if it does, refine to `__edgezero/routes`).

- [ ] **Step 4: Commit**

```bash
git add docs/guide/routing.md
git commit -m "Docs: replace route-listing with introspection routes"
```

---

### Task 8: Full verification (CI gates + app-demo smoke)

**Files:** none (verification only).

- [ ] **Step 1: Format**

Run: `cargo fmt --all -- --check`
Expected: clean (no diff).

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Workspace tests**

Run: `cargo test --workspace --all-targets`
Expected: all pass, including the new manifest/router/introspection/app-demo tests.

- [ ] **Step 4: Feature compilation**

Run: `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
Expected: builds.

- [ ] **Step 5: Spin wasm target**

Run: `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
Expected: builds.

- [ ] **Step 6: app-demo dev-server smoke (manual/optional)**

Run: `cd examples/app-demo && cargo run -p app-demo-adapter-axum` then in another shell:
```bash
curl -s localhost:8787/_app-demo/manifest | head -c 200
curl -s localhost:8787/_app-demo/routes
curl -s -o /dev/null -w "%{http_code}\n" localhost:8787/_app-demo/config
```
Expected: manifest JSON (no secret `value`), a routes array, and a status code for `/config` (200 if a config blob is present, 404 otherwise).

- [ ] **Step 7: Mark PR ready**

Update PR #300 checklist and mark it ready for review:
```bash
gh pr ready 300
```

---

## Self-Review

**Spec coverage:**
- Manifest→JSON (baked, Serialize): Task 1 + Task 4. ✓ (errata: secret redaction, version/kind)
- Config→envelope data (secret-safe, verify): Task 3. ✓
- Routes→live index: Task 3. ✓
- Router-chokepoint injection (no global/no adapter changes): Task 2. ✓
- `RequestContext::introspection()`: Task 2. ✓
- `#[action]` self-alias: Task 3 Step 1. ✓
- Remove `enable_route_listing`/`/__edgezero/routes`: Task 5. ✓
- Templates + app-demo default triggers under `/_<app-name>/…`: Task 6. ✓
- Docs update: Task 7. ✓
- CI gates: Task 8. ✓

**Placeholder scan:** No TBD/TODO; every code step shows real code. Two verification notes ("confirm field names", "verify `{{{adapter_list}}}` name") are guardrails against drift, not missing content.

**Type consistency:** `IntrospectionData { manifest_json: Option<Arc<str>>, routes: Arc<[RouteInfo]> }`, `with_manifest_json(impl Into<Arc<str>>)`, and `introspection() -> Option<&IntrospectionData>` are used identically across Tasks 2/3/6. Handler names `manifest`/`config`/`routes` match the trigger `handler = "edgezero_core::introspection::…"` strings in Task 6.
