# Pluggable Introspection Routes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> ## ⚠️ EXECUTION STATUS — READ FIRST
>
> **Tasks 1–9 below are COMPLETE and committed.** Do NOT re-implement them.
>
> **Tasks 2 and 3 describe the ORIGINAL architecture — UNCONDITIONAL per-request
> injection with handlers reading `ctx.introspection()` directly. That approach
> was SUPERSEDED.** They are retained only as the historical record of what was
> committed and then evolved. Do NOT implement Tasks 2/3 "as written"; the
> committed code has since been changed by the Addendum.
>
> **The ACTIVE, authoritative implementation path is the "Addendum (2026-07-02)"
> at the end of this document** — opt-in, per-route **gated** injection driven by
> `#[action(manifest|routes)]`, with typed extractors for access. It matches the
> rewritten spec. When the Addendum conflicts with the main body, the Addendum
> governs. See the spec's "Design evolution" section for the full path.

**Goal:** Add three reusable core introspection handlers — `edgezero_core::introspection::{manifest, config, routes}` — that any app binds via `[[triggers.http]]`, default-mounted at `/_<app-name>/{manifest,config,routes}`.

**Architecture (final — see Addendum):** `Manifest` gains `Serialize`; the `app!` macro bakes it to JSON via `RouterService::builder().with_manifest_json(...)`. Handlers opt into introspection data with an atomic `#[action(manifest)]` / `#[action(routes)]` parameter, which expands them to capability-carrying handler structs; `add_route` reads `DynHandler::needs_introspection()` and flags the `RouteEntry`; `RouterInner::dispatch` injects a shared `Arc<IntrospectionData>` **only for flagged routes**. `ManifestJson`/`RouteTable` extractors read it; `config` uses the default config store (no injection). `app!` and `edgezero.toml` never learn about introspection. The legacy `enable_route_listing` machinery and `/__edgezero/routes` are removed.

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
    // Enums serialize to their wire strings, not Rust variant names.
    assert_eq!(json["triggers"]["http"][0]["methods"][0], "GET");
}

#[test]
fn serializes_enums_with_wire_casing() {
    let toml = r#"
[app]
name = "t"

[[triggers.http]]
id = "r"
path = "/"
methods = ["POST"]
handler = "t::h::r"
body-mode = "buffered"

[logging.axum]
level = "info"
"#;
    let manifest: Manifest = toml::from_str(toml).unwrap();
    let json = serde_json::to_value(&manifest).unwrap();
    assert_eq!(json["triggers"]["http"][0]["methods"][0], "POST");
    // `body_mode` is serde-renamed to `body-mode` (manifest.rs:243), so the
    // serialized key is `body-mode`, NOT `body_mode`.
    assert_eq!(json["triggers"]["http"][0]["body-mode"], "buffered");
    assert_eq!(json["logging"]["axum"]["level"], "info");
}
```

(The `body-mode` key matches the `#[serde(rename = "body-mode")]` on
`ManifestHttpTrigger::body_mode`. Verify the `[logging.<adapter>]` shape against
manifest.rs before running.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgezero-core serializes_` (one filter matching both `serializes_manifest_and_redacts_secret_values` and `serializes_enums_with_wire_casing` — Cargo takes a single test-name filter)
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

Add near `ManifestEnvironment` (manifest.rs:276). Use an **owned** redacted
struct so serde's `skip_serializing_if` fn signatures match (a `&[String]` field
would make `Vec::is_empty` fail to type-check; an `&Option<_>` field would make
`Option::is_none` fail). Cloning is cheap and only happens at serialize time:

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
    struct RedactedBinding {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        adapters: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        name: String,
        // `value` intentionally omitted.
    }

    let mut seq = serializer.serialize_seq(Some(secrets.len()))?;
    for binding in secrets {
        seq.serialize_element(&RedactedBinding {
            adapters: binding.adapters.clone(),
            description: binding.description.clone(),
            env: binding.env.clone(),
            name: binding.name.clone(),
        })?;
    }
    seq.end()
}
```

- [ ] **Step 5a: Add manual `Serialize` impls for the enums**

`HttpMethod` (:581), `BodyMode` (:639), and `LogLevel` (:669) have hand-written
`Deserialize` impls that accept wire strings (`"GET"`, `"buffered"`, `"info"`).
A derived `Serialize` would emit variant names (`Get`/`Buffered`/`Info`) —
**wrong**. Add manual impls that mirror deserialization. Do NOT add `Serialize`
to their derive lists. `Serialize` has no defaulted methods, so no
`#[expect(clippy::missing_trait_methods)]` is needed (unlike the `Deserialize`
impls). Add after each enum's existing impl block:

```rust
impl serde::Serialize for HttpMethod {
    #[inline]
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl serde::Serialize for BodyMode {
    #[inline]
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Buffered => "buffered",
            Self::Stream => "stream",
        })
    }
}

impl serde::Serialize for LogLevel {
    #[inline]
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
```

- [ ] **Step 5: Add `Serialize` derives to the structs + wire the redactor**

Add `Serialize` to the `#[derive(...)]` on these **structs** (verify each line
against the file — they are the Deserialize-deriving manifest structs reachable
from `Manifest` output on `main`): `Manifest` (:86), `ManifestApp` (:217),
`ManifestTriggers` (:230), `ManifestHttpTrigger` (:238), `ManifestEnvironment`
(:276), `ManifestBinding` (:287), `ManifestAdapter` (:344),
`ManifestAdapterDefinition` (:368), `ManifestAdapterBuild` (:405),
`ManifestAdapterCommands` (:418), `ManifestStores` (:460), `StoreDeclaration`
(:482), `ManifestLogging` (:519), `ManifestLoggingConfig` (:527). Keep existing
`Deserialize`/`Validate`.

Do **not** add `Serialize` to the enums (Step 5a handles those manually), and do
**not** add it to the internal resolved/non-serde structs at :330, :338, :539
(they are reachable only via `#[serde(skip)]` fields — `root`,
`logging_resolved`). `toml::Value` fields (e.g. any `#[serde(flatten)]` legacy
map, if present) already implement `Serialize`.

> **Note (branch drift):** the earlier design exploration ran against the
> `feature/provision-local-impl` checkout, which has an extra
> `ManifestAdapterDeployed` struct and an adapter `deployed` field. Those do
> **not** exist on `main` (this worktree's base) — do not reference them.

On `ManifestEnvironment::secrets`, add:

```rust
    #[serde(default, serialize_with = "serialize_secrets")]
    #[validate(nested)]
    pub secrets: Vec<ManifestBinding>,
```

Add `#[serde(skip_serializing_if = "...")]` to keep output clean where fields are optional/empty (e.g. `Option::is_none`, `Vec::is_empty`, `BTreeMap::is_empty`). The internal `root` and `logging_resolved` fields already carry `#[serde(skip)]` — leave them.

- [ ] **Step 6: Run tests**

Run: `cargo test -p edgezero-core serializes_` (one filter matching both `serializes_manifest_and_redacts_secret_values` and `serializes_enums_with_wire_casing` — Cargo takes a single test-name filter)
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

Also add a middleware-visibility test (errata #5 requires proving both handler
and middleware see the injected data, since injection happens before the
middleware chain runs):

```rust
#[test]
fn middleware_sees_introspection_data() {
    use crate::context::RequestContext;
    use crate::middleware::{Middleware, Next};
    use std::sync::{Arc, Mutex};

    struct Probe(Arc<Mutex<bool>>);
    #[async_trait::async_trait(?Send)]
    impl Middleware for Probe {
        async fn handle(&self, ctx: RequestContext, next: Next) -> Result<Response, EdgeError> {
            *self.0.lock().unwrap() = ctx.introspection().is_some();
            next.run(ctx).await
        }
    }

    let saw = Arc::new(Mutex::new(false));
    let router = RouterService::builder()
        .with_manifest_json("{}")
        .middleware(Probe(Arc::clone(&saw)))
        .get("/", |_ctx: RequestContext| async { Ok::<_, EdgeError>("ok") })
        .build();
    let request = crate::http::request_builder()
        .method(Method::GET).uri("/").body(Body::empty()).unwrap();
    let _ = block_on(router.oneshot(request)).unwrap();
    assert!(*saw.lock().unwrap(), "middleware should see introspection data");
}
```

(Match the exact `Middleware`/`Next` import paths and `async_trait` usage the
existing middleware tests in this crate use.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p edgezero-core introspection_data` (one filter matching both `dispatch_injects_introspection_data` and `middleware_sees_introspection_data`)
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

Run: `cargo test -p edgezero-core introspection_data` (one filter matching both `dispatch_injects_introspection_data` and `middleware_sees_introspection_data`)
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
// NOTE: `Response` is an HTTP alias exported from `crate::http`, NOT
// `crate::response` (response.rs itself imports it from crate::http).
use crate::http::{response_builder, Response, StatusCode};
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
    use crate::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use crate::http::{request_builder, Method};
    use crate::router::RouterService;
    use crate::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
    use async_trait::async_trait;
    use futures::executor::block_on;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // A config store returning a fixed result for `get`, used to drive the
    // config handler's status-code mapping. Mirrors the pattern in
    // extractor.rs::config_extractor_resolves_from_registry.
    struct StubStore(Result<Option<String>, ConfigStoreError>);
    #[async_trait(?Send)]
    impl ConfigStore for StubStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            match &self.0 {
                Ok(v) => Ok(v.clone()),
                Err(ConfigStoreError::Unavailable { .. }) => {
                    Err(ConfigStoreError::unavailable("down"))
                }
                Err(ConfigStoreError::InvalidKey { .. }) => {
                    Err(ConfigStoreError::invalid_key("bad"))
                }
                Err(_) => Err(ConfigStoreError::internal(anyhow::anyhow!("boom"))),
            }
        }
    }

    // Collect a buffered response body into JSON (introspection responses are
    // always `Body::Once`). `Body::to_json` works on the buffered variant.
    fn body_json(resp: crate::http::Response) -> serde_json::Value {
        resp.into_body().to_json().expect("buffered JSON body")
    }

    // Build a request carrying a default ConfigRegistry backed by `store`, and
    // drive it THROUGH THE ROUTER via `oneshot` (which maps handler `EdgeError`
    // to a response internally — so we neither import `IntoResponse` nor unwrap
    // an error path by hand).
    fn run_config(store: StubStore) -> crate::http::Response {
        let registry: ConfigRegistry = StoreRegistry::new(
            [(
                "default".to_owned(),
                ConfigStoreBinding {
                    handle: ConfigStoreHandle::new(Arc::new(store)),
                    default_key: "default".to_owned(),
                },
            )]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
            "default".to_owned(),
        );
        let router = RouterService::builder().get("/c", config).build();
        let mut request = request_builder()
            .method(Method::GET)
            .uri("/c")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(registry);
        block_on(router.oneshot(request)).unwrap()
    }

    fn valid_envelope_json(data: serde_json::Value) -> String {
        // Build a real envelope so sha/version are correct.
        serde_json::to_string(&BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned())).unwrap()
    }

    #[test]
    fn manifest_returns_injected_json() {
        let router = RouterService::builder()
            .with_manifest_json("{\"app\":{\"name\":\"t\"}}")
            .get("/m", manifest)
            .build();
        let req = request_builder().method(Method::GET).uri("/m").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
        // Body is the injected manifest JSON verbatim.
        assert_eq!(body_json(resp), serde_json::json!({ "app": { "name": "t" } }));
    }

    #[test]
    fn routes_lists_registered_routes() {
        let router = RouterService::builder().get("/r", routes).build();
        let req = request_builder().method(Method::GET).uri("/r").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Shape: [{ "method", "path" }] — the /r route itself is present.
        let body = body_json(resp);
        let arr = body.as_array().expect("routes array");
        assert!(arr.iter().any(|e| e["method"] == "GET" && e["path"] == "/r"));
    }

    #[test]
    fn config_without_store_is_not_found() {
        let router = RouterService::builder().get("/c", config).build();
        let req = request_builder().method(Method::GET).uri("/c").body(Body::empty()).unwrap();
        let resp = block_on(router.oneshot(req)).unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_happy_path_returns_envelope_data_secret_safe() {
        let data = serde_json::json!({ "greeting": "hi", "api_token": "demo_api_token" });
        let resp = run_config(StubStore(Ok(Some(valid_envelope_json(data)))));
        assert_eq!(resp.status(), StatusCode::OK);
        // Raw envelope `data` verbatim: the secret field holds the KEY NAME,
        // never a resolved value.
        let body = body_json(resp);
        assert_eq!(body["greeting"], "hi");
        assert_eq!(body["api_token"], "demo_api_token");
    }

    #[test]
    fn config_missing_blob_is_not_found() {
        let resp = run_config(StubStore(Ok(None)));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn config_backend_unavailable_maps_503() {
        let resp = run_config(StubStore(Err(ConfigStoreError::unavailable("x"))));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn config_invalid_key_maps_400() {
        let resp = run_config(StubStore(Err(ConfigStoreError::invalid_key("x"))));
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn config_backend_internal_maps_500() {
        let resp = run_config(StubStore(Err(ConfigStoreError::internal(anyhow::anyhow!("x")))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_malformed_envelope_maps_500() {
        let resp = run_config(StubStore(Ok(Some("not json".to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_sha_mismatch_maps_500() {
        // Valid JSON envelope shape but wrong sha → verify() fails.
        let bad = r#"{"data":{"a":1},"generated_at":"t","sha256":"deadbeef","version":1}"#;
        let resp = run_config(StubStore(Ok(Some(bad.to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn config_unknown_version_maps_500() {
        let bad = r#"{"data":{},"generated_at":"t","sha256":"x","version":99}"#;
        let resp = run_config(StubStore(Ok(Some(bad.to_owned()))));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
```

Notes: the `StubStore::0 = Err(...)` arm is matched by variant, so the three
`ConfigStoreError` constructors (`unavailable`, `invalid_key`, `internal`) must
match config_store.rs:177-199. `body_json` relies on `Body::to_json` (body.rs)
and `http::Response::into_body`; both exist. The malformed/sha/version cases are
driven by raw strings so they don't depend on the stub's error arm.

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

- [ ] **Step 3: Build and test the macro crate**

Run: `cargo build -p edgezero-macros`
Expected: builds cleanly.
Then: `cargo test -p edgezero-macros`
Expected: PASS — the existing `app_config_derive` + `tests/ui` trybuild suite
still passes (CLAUDE.md requires `cargo test` after any code change).

> **Macro-output coverage:** the spec calls for macro expansion coverage
> (spec:303). The `app!` output for this change (`with_manifest_json(<json>)`) is
> exercised end-to-end in **Task 6**: app-demo's `introspection_routes_are_wired`
> test builds the real `app!`-generated `build_router()` and asserts
> `/_app-demo/manifest` returns the baked JSON with `[app].name == "app-demo"`.
> That is a stronger check than a string-match expansion test, so no separate
> trybuild case is added for the positive path. (trybuild remains the right tool
> only for compile-fail cases, none of which this change introduces.)

- [ ] **Step 4: Verify a consumer still builds**

`examples/app-demo` is `exclude`d from the root workspace (Cargo.toml:12), so it must be built from its own directory:

Run: `cargo build -p edgezero-core` then `cd examples/app-demo && cargo check -p app-demo-core`
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

In app-demo-core's test module (colocated with `build_router`/`App`), add. A
routing miss ALSO returns 404 via `oneshot` (router.rs), so an `OK | NOT_FOUND`
assertion would pass even if `/config` were never wired. Instead, **seed a
`ConfigRegistry`** so a wired `/config` route returns 200, proving the trigger
exists, and assert the raw envelope `data` exposes the key-name (never a
resolved secret value):

```rust
#[test]
fn introspection_routes_are_wired() {
    use edgezero_core::body::Body;
    use edgezero_core::config_store::{ConfigStore, ConfigStoreError, ConfigStoreHandle};
    use edgezero_core::http::{request_builder, Method, StatusCode};
    use edgezero_core::store_registry::{ConfigRegistry, ConfigStoreBinding, StoreRegistry};
    use edgezero_core::blob_envelope::BlobEnvelope;
    use async_trait::async_trait;
    use futures::executor::block_on;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let router = crate::build_router();

    // manifest: 200 + JSON body whose [app].name is "app-demo".
    let req = request_builder().method(Method::GET).uri("/_app-demo/manifest").body(Body::empty()).unwrap();
    let resp = block_on(router.oneshot(req)).unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("content-type").unwrap(), "application/json");
    let manifest_body: serde_json::Value = resp.into_body().to_json().unwrap();
    assert_eq!(manifest_body["app"]["name"], "app-demo");

    // routes: 200 + [{method,path}] including the root route.
    let req = request_builder().method(Method::GET).uri("/_app-demo/routes").body(Body::empty()).unwrap();
    let resp = block_on(router.oneshot(req)).unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let routes_body: serde_json::Value = resp.into_body().to_json().unwrap();
    let arr = routes_body.as_array().expect("routes array");
    assert!(arr.iter().any(|e| e["method"] == "GET" && e["path"] == "/"));

    // /config: seed a default config store with a valid envelope so a wired
    // route returns 200 (a routing miss would be 404, proving nothing).
    struct FixedStore(String);
    #[async_trait(?Send)]
    impl ConfigStore for FixedStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, ConfigStoreError> {
            Ok(Some(self.0.clone()))
        }
    }
    let data = serde_json::json!({ "greeting": "hi", "api_token": "demo_api_token" });
    let blob = serde_json::to_string(&BlobEnvelope::new(data, "2026-01-01T00:00:00Z".to_owned())).unwrap();
    let registry: ConfigRegistry = StoreRegistry::new(
        [(
            "app_config".to_owned(),
            ConfigStoreBinding {
                handle: ConfigStoreHandle::new(Arc::new(FixedStore(blob))),
                default_key: "app_config".to_owned(),
            },
        )].into_iter().collect::<BTreeMap<_, _>>(),
        "app_config".to_owned(),
    );
    let mut req = request_builder().method(Method::GET).uri("/_app-demo/config").body(Body::empty()).unwrap();
    req.extensions_mut().insert(registry);
    let resp = block_on(router.oneshot(req)).unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/config should be wired and 200 with a store");
    // Raw envelope `data`: secret field holds the KEY NAME, not a resolved value.
    let config_body: serde_json::Value = resp.into_body().to_json().unwrap();
    assert_eq!(config_body["api_token"], "demo_api_token");
    assert_eq!(config_body["greeting"], "hi");
}
```

(Match the app-demo crate's existing test imports/module location; `build_router` is generated by `app!`. Confirm app-demo's default config store id is `app_config` per its `[stores.config]`.)

- [ ] **Step 3: Run test to verify it fails, then passes**

`examples/app-demo` is excluded from the root workspace, so run from its directory:

Run: `cd examples/app-demo && cargo test -p app-demo-core introspection_routes_are_wired`
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
- Modify: `docs/guide/roadmap.md:16` ("route listing + body-mode behavior")

**Interfaces:** none (documentation only).

- [ ] **Step 1: Locate the references**

Run: `grep -rn "route listing\|__edgezero/routes\|enable_route_listing" docs/guide`
Expected: `docs/guide/routing.md` (~:118) and `docs/guide/roadmap.md:16`. Scope the
grep to `docs/guide` (NOT `docs/`) so it does not match this plan and the spec
under `docs/superpowers`. Do NOT touch any `.__edgezero_chunks` docs (unrelated).

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

- [ ] **Step 2b: Update roadmap.md**

In `docs/guide/roadmap.md:16`, the "Example coverage" bullet ends with "…logging
precedence, and route listing + body-mode behavior". Replace "route listing"
with "introspection routes" so it reads "…logging precedence, and introspection
routes + body-mode behavior".

- [ ] **Step 3: Verify no stale references remain**

Run: `grep -rn "enable_route_listing\|__edgezero/routes\|route listing" docs/guide`
Expected: no matches under `docs/guide` (scope to `docs/guide`, not `docs/`, so
the plan/spec under `docs/superpowers` are not matched; `.__edgezero_chunks` is a
different token and should not appear).

- [ ] **Step 4: Commit**

```bash
git add docs/guide/routing.md docs/guide/roadmap.md
git commit -m "Docs: replace route-listing with introspection routes"
```

> **Out-of-scope, flagged for decision (review finding #8):** CLI docs
> (`docs/guide/cli-reference.md:241`, `docs/guide/cli-walkthrough.md:153`) state
> that typed `config push` "strips secret fields", which reportedly contradicts
> the key-name envelope model (`examples/app-demo/.../config_flow.rs:206`). This
> is a **pre-existing** inaccuracy about `config push` semantics, independent of
> introspection routes, and the push behavior itself has not been re-verified
> here. It is intentionally excluded from this plan. If desired, correct it in a
> separate change after confirming the actual push behavior.

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
Expected: all pass (manifest/router/introspection). **Note:** the root workspace
`exclude`s `examples/app-demo` (Cargo.toml:12), so this does NOT run the
app-demo tests — those are covered by Step 3b (matching CI's separate job).

- [ ] **Step 3b: app-demo tests (separate workspace)**

Run: `cd examples/app-demo && cargo test --workspace --all-targets`
Expected: all pass, including `introspection_routes_are_wired`.

- [ ] **Step 3c: Generated-project build (template surface)**

Task 6 edits the generated-app template, so exercise CI's ignored end-to-end
scaffold-and-build test (test.yml):

Run: `cargo test -p edgezero-cli --test generated_project_builds -- --ignored`
Expected: a project scaffolded from the template (now with the three
introspection triggers) compiles.

- [ ] **Step 3d: Nested AppConfig audit (template + app-demo surface)**

The template and app-demo are both audited by CI (test.yml):

Run: `cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates`
Expected: passes (the introspection triggers add no nested `AppConfig`).

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
- Manifest→JSON (baked, Serialize): Task 1 + Task 4. ✓ (errata: secret redaction, version/kind, manual enum Serialize with wire casing)
- Config→envelope data (secret-safe, verify): Task 3, with full status-code coverage (200/404/400/503/500×3). ✓
- Routes→live index: Task 3. ✓
- Router-chokepoint injection (no global/no adapter changes): Task 2, with handler + middleware visibility tests. ✓
- `RequestContext::introspection()`: Task 2. ✓
- `#[action]` self-alias: Task 3 Step 1. ✓
- Remove `enable_route_listing`/`/__edgezero/routes`: Task 5. ✓
- Templates + app-demo default triggers under `/_<app-name>/…`: Task 6 (config test seeds a registry to prove wiring). ✓
- Docs update: Task 7 (cli-doc drift flagged out-of-scope). ✓
- CI gates incl. separate app-demo workspace: Task 8. ✓

**Review findings applied (round 1):** enum serialization + casing test (blocking); owned `RedactedBinding` + removed nonexistent `ManifestAdapterDeployed` (blocking); `Response` from `crate::http` (blocking); full config status-code tests (high); app-demo config test seeds a registry and asserts 200 (high); `cd examples/app-demo` for excluded-workspace commands (high); middleware-visibility test (medium); cli-doc drift flagged, not silently absorbed (medium).

**Review findings applied (round 3):** single-filter `cargo test` commands (Cargo takes one test-name filter) in Tasks 1 & 2 (high); Task 7 stale-doc greps scoped to `docs/guide` so they don't match the plan/spec under `docs/superpowers` (high); Task 7 also updates `docs/guide/roadmap.md:16` "route listing" phrasing (medium); Task 4 now runs `cargo test -p edgezero-macros` per CLAUDE.md, with macro-output coverage delegated to Task 6's end-to-end assertion rather than a brittle expansion test (medium).

**Review findings applied (round 2):** real body assertions for manifest (equality), routes (`[{method,path}]` shape), and config (`api_token` key-name present, secret-safe) in Tasks 3 & 6 (high); `run_config` now routes through `oneshot` so no `IntoResponse` import is needed, and unused test imports dropped (high); `body-mode` serde-rename fixed in the casing test (high); `ConfigStoreError::Internal → 500` test added (medium); Task 8 gains generated-project-build (`--ignored`) and nested-AppConfig-audit steps matching CI's template-surface jobs (medium).

**Placeholder scan:** No TBD/TODO; every code step shows real code. Verification notes ("confirm `ConfigStoreError` constructor names", "verify `{{{adapter_list}}}` name", "match body-collection helper") are drift guardrails, not missing content.

**Type consistency:** `IntrospectionData { manifest_json: Option<Arc<str>>, routes: Arc<[RouteInfo]> }`, `with_manifest_json(impl Into<Arc<str>>)`, and `introspection() -> Option<&IntrospectionData>` are used identically across Tasks 2/3/6. Handler names `manifest`/`config`/`routes` match the trigger `handler = "edgezero_core::introspection::…"` strings in Task 6. Manual enum `Serialize` (Task 1 Step 5a) matches the `Deserialize` wire forms.

---

## Addendum (2026-07-02): ACTIVE PATH — fully-atomic `#[action(manifest|routes)]` opt-in

**This supersedes Tasks 2 & 3.** Task 9 (extractors + handler refactor) is
committed (`0feb194`). The tasks below add **fully-atomic, per-capability gated
injection** matching the rewritten spec: the handler declares exactly which data
it needs; the router injects each payload independently, only for routes that
asked. No `IntrospectionData` bundle, no `ctx.introspection()`, no
`needs_introspection` bool, no `app!`/`edgezero.toml` change.

**Verified contract (from investigation):**
- `DynHandler: Send + Sync { fn call(&self, RequestContext) -> HandlerFuture; }`, blanket-impl'd for `F: Fn(RequestContext) -> Fut`. Object-safe with an added defaulted method. `HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, EdgeError>> + 'static>>` (no `Send`).
- `http::Extensions` requires inserted types be `Clone + Send + Sync + 'static`.
- Handlers run only via `DynHandler::call`; only `manifest`/`routes` become structs; neither is called directly → zero blast radius.

Order is chosen so **every commit compiles green** (the gating swap lands last,
after the handlers are opted in while unconditional injection is still active).

### Task 10a: `IntrospectionNeeds` + `DynHandler::introspection_needs` (edgezero-core/src/handler.rs)

- [ ] **Step 1 — add the value type + defaulted method** (additive; compiles green):
```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IntrospectionNeeds {
    pub manifest: bool,
    pub routes: bool,
}

impl IntrospectionNeeds {
    #[must_use]
    pub fn any(self) -> bool { self.manifest || self.routes }
}

pub trait DynHandler: Send + Sync {
    fn call(&self, ctx: RequestContext) -> HandlerFuture;

    /// Introspection payloads a route bound to this handler needs injected.
    /// Default none; `#[action(manifest|routes)]` handlers override.
    fn introspection_needs(&self) -> IntrospectionNeeds {
        IntrospectionNeeds::default()
    }
}
```
  The `Fn` blanket impl needs no change.
- [ ] **Step 2 — verify:** `cargo build -p edgezero-core`; `cargo fmt -p edgezero-core`; `cargo clippy -p edgezero-core --all-targets -- -D warnings`.
- [ ] **Step 3 — commit:** `Add IntrospectionNeeds + DynHandler::introspection_needs`

### Task 10b: `#[action(manifest|routes)]` param parser + struct codegen (edgezero-macros/src/action.rs)

- [ ] **Step 1 (RED) — macro unit tests.** Replace `rejects_attribute_arguments` with `rejects_unknown_param` (`#[action(bogus)]` → output contains `unknown #[action] parameter`); add `manifest_param_emits_struct` (`#[action(manifest)]` → output contains `struct` + `DynHandler` + `introspection_needs`); keep `wraps_async_function`. Run `cargo test -p edgezero-macros` → the two new/changed tests FAIL.
- [ ] **Step 2 — parse params** (replace the `!attr.is_empty()` rejection):
```rust
use syn::parse::Parser as _;
use syn::punctuated::Punctuated;

let params: Punctuated<syn::Ident, syn::Token![,]> = if attr.is_empty() {
    Punctuated::new()
} else {
    match Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated.parse2(attr.clone()) {
        Ok(p) => p,
        Err(err) => return err.to_compile_error(),
    }
};
let mut manifest_cap = false;
let mut routes_cap = false;
for param in &params {
    if param == "manifest" { manifest_cap = true; }
    else if param == "routes" { routes_cap = true; }
    else {
        return syn::Error::new(param.span(),
            format!("unknown #[action] parameter `{param}`; supported: manifest, routes"))
            .to_compile_error();
    }
}
let is_struct = manifest_cap || routes_cap;
```
- [ ] **Step 3 — branch codegen.** When `!is_struct`: emit exactly today's fn form (unchanged). When `is_struct`:
```rust
quote! {
    #inner_fn

    #(#attrs)*
    #[allow(non_camel_case_types)]
    #vis struct #ident;

    impl ::edgezero_core::handler::DynHandler for #ident {
        #[inline]
        fn call(&self, __ctx: ::edgezero_core::context::RequestContext)
            -> ::edgezero_core::http::HandlerFuture {
            ::std::boxed::Box::pin(async move {
                #(#extract_stmts)*
                let result = #inner_ident(#(#arg_idents),*).await;
                ::edgezero_core::responder::Responder::respond(result)
            })
        }
        #[inline]
        fn introspection_needs(&self) -> ::edgezero_core::handler::IntrospectionNeeds {
            ::edgezero_core::handler::IntrospectionNeeds { manifest: #manifest_cap, routes: #routes_cap }
        }
    }
}
```
  (`#manifest_cap`/`#routes_cap` interpolate as `true`/`false` bool literals.)
- [ ] **Step 4 (GREEN):** `cargo test -p edgezero-macros`; `cargo fmt --all`; `cargo clippy -p edgezero-macros --all-targets -- -D warnings`.
- [ ] **Step 5 — commit:** `#[action]: accept atomic (manifest|routes) params; emit capability-carrying handler struct`

### Task 10c: opt the handlers in (edgezero-core/src/introspection.rs)  — stays green under the still-unconditional path

- [ ] **Step 1 — opt in.** `manifest` → `#[action(manifest)]`; `routes` → `#[action(routes)]`; `config` stays `#[action]`. (The extractors still read `ctx.introspection()` and injection is still unconditional at this point, so behavior is unchanged — the handlers merely become structs reporting `introspection_needs`, which the router does not yet consult.)
- [ ] **Step 2 — verify:** `cargo test -p edgezero-core introspection` (all pass unchanged); `cargo fmt`; `cargo clippy -p edgezero-core --all-targets -- -D warnings`.
- [ ] **Step 3 — commit:** `Opt manifest/routes into introspection via #[action(manifest|routes)]`

### Task 10d: the atomic gating swap (edgezero-core/src/{router,context,introspection}.rs) — all in one commit so it compiles

- [ ] **Step 1 (RED) — router tests.** Rewrite `dispatch_injects_introspection_data` and `middleware_sees_introspection_data` to register a LOCAL probe struct (a closure can not report `introspection_needs`), and add a negative test. Probe builds its response with `response_builder` (no `IntoResponse` import needed):
```rust
struct ManifestProbe(std::sync::Arc<std::sync::Mutex<Option<bool>>>);
impl crate::handler::DynHandler for ManifestProbe {
    fn call(&self, ctx: RequestContext) -> crate::http::HandlerFuture {
        let cell = std::sync::Arc::clone(&self.0);
        Box::pin(async move {
            *cell.lock().unwrap() =
                Some(ctx.extension::<crate::introspection::ManifestJson>().is_some());
            crate::http::response_builder()
                .status(crate::http::StatusCode::OK)
                .body(crate::body::Body::empty())
                .map_err(EdgeError::internal)
        })
    }
    fn introspection_needs(&self) -> crate::handler::IntrospectionNeeds {
        crate::handler::IntrospectionNeeds { manifest: true, routes: false }
    }
}
```
  - flagged route: `.with_manifest_json("{}").get("/", ManifestProbe(cell))` → cell records `true`.
  - middleware test: same flagged route + a probe middleware asserting `ctx.extension::<ManifestJson>().is_some()` (injection happens before middleware).
  - negative test `plain_route_gets_no_manifest`: `.get("/x", |_ctx: RequestContext| async { … })` (a plain closure) → the handler records `ctx.extension::<crate::introspection::ManifestJson>().is_none()`.
  Run (single filter per command — Cargo takes ONE positional filter):
  `cargo test -p edgezero-core router` (fails: `introspection_needs`/`extension`/gating absent).
- [ ] **Step 2 — `RouteEntry` flag.** Add `introspection_needs: IntrospectionNeeds` (`Copy`); copy it in the manual `Clone`/`clone_from`.
- [ ] **Step 3 — read it in `add_route`:**
```rust
let boxed = handler.into_handler();
let introspection_needs = boxed.introspection_needs();
router.insert(path, RouteEntry { handler: boxed, introspection_needs })
    .unwrap_or_else(|err| panic!("duplicate route definition for {path}: {err}"));
```
- [ ] **Step 4 — remove the bundle; per-capability inject in `dispatch`.** Delete the `IntrospectionData` struct and the unconditional insert at the top of `dispatch`. `RouterInner` keeps `manifest_json: Option<Arc<str>>` + `route_index`. Inside `RouteMatch::Found(entry, params)`:
```rust
let needs = entry.introspection_needs;
let mut request = request;
if needs.manifest {
    if let Some(json) = &self.manifest_json {
        request.extensions_mut().insert(crate::introspection::ManifestJson(Arc::clone(json)));
    }
}
if needs.routes {
    request.extensions_mut().insert(crate::introspection::RouteTable(Arc::clone(&self.route_index)));
}
let ctx = RequestContext::new(request, params);
```
- [ ] **Step 5 — context accessor.** In `context.rs`, remove `introspection()`; add:
```rust
pub(crate) fn extension<T>(&self) -> Option<T>
where
    T: Clone + Send + Sync + 'static,
{
    self.request.extensions().get::<T>().cloned()
}
```
- [ ] **Step 6 — extractors as payloads.** In `introspection.rs`: add `#[derive(Clone)]` to `ManifestJson` and `RouteTable`; rewrite their `from_request` to clone their own type out of the request:
```rust
async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
    ctx.extension::<ManifestJson>()   // (RouteTable in the other impl)
        .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("manifest introspection data not available")))
}
```
- [ ] **Step 7 (GREEN):** `cargo test -p edgezero-core router`; `cargo test -p edgezero-core introspection`; `cargo test -p edgezero-core`; `cargo fmt`; `cargo clippy -p edgezero-core --all-targets -- -D warnings`. Confirm `manifest_without_baked_json_is_500` still holds (route opted into `manifest`, but `manifest_json` is `None` → nothing injected → extractor 500).
- [ ] **Step 8 — commit:** `Gate introspection injection per capability via IntrospectionNeeds`

### Task 11: full verification + whole-branch review

Run each on its own line (single positional filter per `cargo test`):
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace --all-targets`
- [ ] `cd examples/app-demo && cargo test --workspace --all-targets` (other handlers unchanged; `introspection_routes_are_wired` still passes: `manifest`/`routes` opted in, `config` via store)
- [ ] `cargo test -p edgezero-cli --test generated_project_builds -- --ignored` (template handlers are plain `#[action]` → still fns)
- [ ] `cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates`
- [ ] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
- [ ] `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
- [ ] Whole-branch review of the addendum commits, then push to #300.
