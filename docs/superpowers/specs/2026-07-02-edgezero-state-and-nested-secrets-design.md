# EdgeZero — `State<T>` extractor + nested/array `#[secret]` support

- **Status:** Draft for edgezero maintainer review
- **Date:** 2026-07-02
- **Author:** trusted-server team (spec), to be implemented by an **edgezero** developer
- **Target repo:** `github.com/stackpop/edgezero` (crates `edgezero-core`, `edgezero-macros`)
- **Consumed by:** trusted-server "move completely to EdgeZero" migration; nothing in trusted-server can start until these two upstream primitives land

---

## 1. Why this spec exists

trusted-server is migrating fully onto EdgeZero. Two later phases of that migration are **blocked on primitives that EdgeZero does not yet expose**:

1. **Handlers → extractors** (trusted-server Phase 4) needs a way to pass **app-owned shared state** (`Arc<AppState>`: `Settings`, `AuctionOrchestrator`, `IntegrationRegistry`) into `#[action]` extractor-style handlers. EdgeZero's extractors are all **request-derived** — there is no `State`/`Extension` extractor and no `RequestContext` accessor for app state.

2. **Secret externalization** (trusted-server Phase 3) needs `#[derive(AppConfig)]` + the runtime secret walk to resolve `#[secret]` fields that live **nested inside sub-structs and arrays**. Today both are restricted to **top-level scalar `String`** fields. trusted-server's `Settings` is deeply nested (per-integration secret fields, arrays of partners), so the current model cannot express its secrets. The existing `TrustedServerAppConfig` wrapper documents this explicitly with `SECRET_FIELDS = &[]` and a comment that nested/array extraction "needs support tracked separately."

Both are small, self-contained additions to `edgezero-core` + `edgezero-macros`. They are **independent of each other** and can land in either order or in parallel.

### Goals

- Add a `State<T>` extractor (and the router plumbing to populate it) so any EdgeZero app can hand app-owned state to extractor handlers.
- Extend `#[derive(AppConfig)]` and the runtime `secret_walk` to resolve `#[secret]` fields nested in sub-structs and (optionally) arrays, keyed by a **field path** rather than a single top-level name.

### Non-goals

- No changes to trusted-server in this phase (that is Phases 1–5 of the umbrella plan).
- No change to the blob-envelope format, canonical-form hashing, or the push/validate CLI flow beyond what nested secret metadata requires.
- No change to the `app!` macro's manifest-driven routing (trusted-server builds its router imperatively via `Hooks::routes()`, so `State<T>` is wired through the `RouterBuilder`, not the manifest).

---

## 2. Current mechanics (verified against `main`)

### 2.1 Request extensions are the transport for everything per-request

`RequestContext` wraps an `http`-style `Request`; every per-request handle is read out of `request.extensions()`:

```rust
// crates/edgezero-core/src/context.rs
pub fn kv_store_default(&self) -> Option<BoundKvStore> {
    self.request.extensions().get::<KvRegistry>().and_then(StoreRegistry::default)
}
```

Extractors follow the same shape (`crates/edgezero-core/src/extractor.rs`):

```rust
#[async_trait(?Send)]
impl FromRequest for Kv {
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.request().extensions().get::<KvRegistry>().cloned().map(Kv)
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!("no kv store configured ...")))
    }
}
```

Registries get **inserted into `request.extensions_mut()` by the adapter** just before dispatch (reference: `edgezero-adapter-axum/src/service.rs` → `router.oneshot(core_request)`; `edgezero-adapter-fastly/src/request.rs::dispatch_with_registries`).

### 2.2 The router owns dispatch and builds the context

```rust
// crates/edgezero-core/src/router.rs
impl RouterInner {
    async fn dispatch(&self, request: Request) -> Result<Response, EdgeError> {
        match self.find_route(&method, &path) {
            RouteMatch::Found(entry, params) => {
                let ctx = RequestContext::new(request, params); // <-- request is owned here
                let next = Next::new(&self.middlewares, entry.handler.as_ref());
                next.run(ctx).await
            }
            ...
        }
    }
}
```

`RouterBuilder` (also in `router.rs`) already holds `middlewares`, `routes`, `route_info`. It is the natural owner of app state because **trusted-server builds its router imperatively inside `Hooks::routes()`**, where `Arc<AppState>` is in scope. Adapters are generic over `A: Hooks` and never see the concrete state type, so state cannot be injected adapter-side.

### 2.3 `#[derive(AppConfig)]` is top-level + scalar-`String`-only

```rust
// crates/edgezero-macros/src/app_config.rs
fn is_scalar_string_type(ty: &Type) -> bool { /* accepts bare `String` only */ }
// scan_field() calls enforce_scalar_string_type() -> rejects Option<String>, Vec<_>, nested structs.
// The emitted SECRET_FIELDS is a flat array of top-level Rust field names:
const SECRET_FIELDS: &'static [SecretField] = &[SecretField { name: "api_token", kind: KeyInDefault }];
```

`SecretField` (`crates/edgezero-core/src/app_config.rs`) is `{ kind: SecretKind, name: &'static str }`, where `name` is a single top-level key.

### 2.4 The runtime `secret_walk` only navigates the top level

```rust
// crates/edgezero-core/src/extractor.rs
async fn secret_walk<C>(ctx: &RequestContext, data: &mut serde_json::Value) -> Result<(), EdgeError> {
    let data_obj = data.as_object_mut().ok_or(...)?;      // top-level object only
    for field in C::SECRET_FIELDS {
        let key_name = data_obj.get(field.name)...;         // top-level get by flat name
        // ... resolve secret value, then:
        data_obj.insert(field.name, resolved_value);        // top-level insert
    }
}
```

Called from `extract_from_handle::<C>()`, which does fetch → `BlobEnvelope::verify()` → `secret_walk` → `serde_path_to_error::deserialize` → `cfg.validate()`. The split between push-time (`validate_excluding_secrets`, values are key names) and runtime (`cfg.validate()`, values resolved) must be preserved.

---

## 3. Workstream A — `State<T>` extractor

### 3.1 Public API additions

**`crates/edgezero-core/src/router.rs` — `RouterBuilder`:**

```rust
impl RouterBuilder {
    /// Register a value that is cloned into every request's extensions
    /// before dispatch, making it available to the `State<T>` extractor
    /// and to `RequestContext`-based handlers.
    ///
    /// Typically `T = Arc<AppState>`. Last write wins for a given `T`.
    #[must_use]
    pub fn with_state<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    { /* push a type-erased inserter (see 3.2) */ self }
}
```

**`crates/edgezero-core/src/extractor.rs` — new extractor:**

```rust
/// Extractor for app-owned shared state registered via
/// `RouterBuilder::with_state`. Resolves by type from request extensions.
pub struct State<T>(pub T);

#[async_trait(?Send)]
impl<T> FromRequest for State<T>
where
    T: Clone + Send + Sync + 'static,
{
    async fn from_request(ctx: &RequestContext) -> Result<Self, EdgeError> {
        ctx.request()
            .extensions()
            .get::<T>()
            .cloned()
            .map(State)
            .ok_or_else(|| EdgeError::internal(anyhow::anyhow!(
                "no `State<{}>` registered — call RouterBuilder::with_state(..)",
                core::any::type_name::<T>()
            )))
    }
}
// + Deref/DerefMut/into_inner to mirror the other extractors.
```

Because `State<T>: FromRequest`, it works inside `#[action]` with **no change to `edgezero-macros/src/action.rs`** (the macro already emits `<Ty as FromRequest>::from_request(&ctx).await?` for every non-`RequestContext` argument).

Handler ergonomics (trusted-server side, illustrative only):

```rust
#[action]
pub async fn handle_auction(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuctionRequest>,
) -> Result<Response, EdgeError> { /* state.settings, state.orchestrator, ... */ }
```

### 3.2 Router plumbing

Add to `RouterBuilder` a list of type-erased inserters and thread them into `RouterInner`:

```rust
type StateInserter = Arc<dyn Fn(&mut http::Extensions) + Send + Sync>;

#[derive(Default)]
pub struct RouterBuilder {
    // ... existing fields ...
    state_inserters: Vec<StateInserter>,
}

pub fn with_state<T>(mut self, value: T) -> Self
where T: Clone + Send + Sync + 'static {
    self.state_inserters.push(Arc::new(move |ext: &mut http::Extensions| {
        ext.insert(value.clone());
    }));
    self
}
```

In `RouterInner::dispatch`, apply inserters to the owned request **before** building the context:

```rust
RouteMatch::Found(entry, params) => {
    let mut request = request;
    for inserter in &self.state_inserters {
        inserter(request.extensions_mut());
    }
    let ctx = RequestContext::new(request, params);
    let next = Next::new(&self.middlewares, entry.handler.as_ref());
    next.run(ctx).await
}
```

Notes:
- `RouterInner` gains a `state_inserters: Vec<StateInserter>` field; `RouterService::new` takes it from the builder.
- Insertion happens **after** the adapter has already inserted the store registries into the same extensions map (different `TypeId`s, no collision). If an app ever registers a `T` that an adapter also inserts, last-write-wins and the router runs last — document this; it is not expected in practice.
- Cost is one `Arc` clone (or one `T::clone`) per registered state per request — negligible for `Arc<AppState>`.
- The route-listing internal handler and middleware are unaffected.

### 3.3 Naming decision (needs maintainer sign-off)

Two reasonable names for the same mechanism:

- **`State<T>`** — matches the app-state use case and reads naturally at call sites. This is what trusted-server asked for. **Recommended.**
- **`Extension<T>`** — matches axum's *runtime-resolved* semantics exactly (this mechanism is axum's `Extension`, not axum's compile-time-typed `State`). More honest about behavior; more familiar to axum users.

**Recommendation:** ship `State<T>` + `with_state` as the primary API. Optionally add `Extension<T>` + `with_extension` as thin aliases if the maintainer wants the axum-accurate name available too. Avoid shipping both as first-class with divergent behavior.

### 3.4 Tests (edgezero)

- Unit (`extractor.rs`): `State<T>` resolves a registered `Arc<Foo>`; returns `EdgeError::internal` (500) when unregistered; `Deref` works.
- Unit (`router.rs`): a handler taking `State<Arc<Counter>>` sees the value after `with_state`; two different `T`s coexist; re-registering the same `T` is last-write-wins.
- Integration: `#[action] fn h(State(s): State<Arc<S>>, Query(q): Query<Q>)` compiles and runs (proves macro composition with an existing extractor).
- Concurrency: two in-flight requests each get an independent clone (no cross-request bleed).

### 3.5 Docs

- `docs/guide/handlers.md`: add a "Sharing app state" section showing `RouterBuilder::with_state` + `State<T>`.
- Rustdoc on `State`, `with_state` with the `Arc<AppState>` example and the last-write-wins note.

---

## 4. Workstream B — nested/array `#[secret]` support

### 4.1 Problem statement

`#[secret]` must be expressible on fields **below the root**, e.g.:

```rust
#[derive(AppConfig, Deserialize, Validate)]
struct Settings {
    #[validate(nested)] integrations: IntegrationSettings,
    #[validate(nested)] partners: Vec<Partner>,
}
struct IntegrationSettings { #[validate(nested)] datadome: DataDome }
struct DataDome { #[secret] server_side_key: String }   // path: integrations.datadome.server_side_key
struct Partner   { #[secret] api_key: String }          // path: partners[*].api_key
```

Today this fails to compile (`enforce_scalar_string_type` rejects the containing types at the root; the derive never recurses).

### 4.2 Metadata model: path-qualified secret fields

Extend `SecretField` (`crates/edgezero-core/src/app_config.rs`) to carry a **path** instead of a single `name`. The path is a sequence of segments; each segment is either a named field or an array wildcard. **Segments are owned** (`Cow<'static, str>` / `Vec<_>`), not `&'static`, and the field carries an `optional` flag — both settled by §8 (cross-crate recursion cannot build `&'static` paths, and the runtime must tell "required-missing → error" from "optional-absent → skip"):

```rust
pub enum SecretPathSegment {
    Field(std::borrow::Cow<'static, str>),   // object key (Rust field name, verbatim)
    ArrayEach,                               // every element of an array
}

pub struct SecretField {
    pub kind: SecretKind,
    pub path: Vec<SecretPathSegment>,        // was: name: &'static str
    pub optional: bool,                      // #[secret] on Option<String>
}
```

Because paths are owned and must compose across crates, `AppConfigMeta` changes from an associated `const SECRET_FIELDS` to a method `fn secret_fields() -> Vec<SecretField>` (§8 / B-3): a parent prepends its `Field`/`ArrayEach` segment onto each child's `secret_fields()`.

- A top-level scalar keeps working: its path is `vec![Field("api_token")]` (length 1) — **backward compatible in behavior**, though the struct/trait shape changes (see 4.6 for the compat call-out).
- `store_ref` siblings referenced by `SecretKind::KeyInNamedStore { store_ref_field }` are resolved **relative to the same parent object** as the secret field (i.e. sibling within the innermost containing object). Document this scoping rule explicitly.

**Array support (B-1) — resolved: implement `ArrayEach` from day one.** The problem statement's own inventory includes an array secret (`partners[*].api_key`), and §8 [B, HIGH] requires an object-only plan *only* if trusted-server's `Settings` audit confirms no secret leaves inside arrays. Absent that confirmation, arrays are in scope from the start; the derive and the runtime walk both handle `ArrayEach` (see the implementation plan `docs/superpowers/plans/2026-07-02-edgezero-nested-secrets.md`).

### 4.3 Derive changes (`crates/edgezero-macros/src/app_config.rs`)

Recurse into fields whose type is itself an `AppConfig`-derived struct (or a `Vec<_>`/`[_]` of one), accumulating the path:

1. Keep the current scan for direct `#[secret]` fields, but emit `path = [Field(name)]` instead of `name`.
2. Add recursion: for a field annotated to recurse (see B-2 below), descend into the referenced type and prefix every `SecretField` it produces with `Field(field_name)` (or `Field(field_name), ArrayEach` for a `Vec`).
3. Preserve all existing compile-time guards (no `#[serde(rename/flatten/skip*)]` on the path, no container `rename_all` when any secret exists) **along the entire path**, since a rename anywhere desyncs the JSON key from the emitted path segment.

**Open design question (B-2): how does the derive know which fields to recurse into?** The macro sees only syntax, not resolved types, so it cannot know a field's type also derives `AppConfig`. Two options:

- **(Recommended) Explicit opt-in attribute**, e.g. `#[app_config(nested)]` on the containing field (and `#[app_config(nested)]` on a `Vec<T>` field for array recursion). Mirrors `#[validate(nested)]`, is unambiguous, and keeps the derive purely syntactic. The sub-struct must itself derive `AppConfig` (enforced at runtime via the `AppConfigRoot` marker / a generated const assertion).
- **(Alternative) Type-name heuristic** — recurse into any field whose type path "looks like" a struct. Rejected: brittle, silently wrong for third-party types, and can't see through aliases.

With explicit opt-in, the derive emits, for each nested field, code that references the sub-struct's own `SECRET_FIELDS` and prefixes the path — so recursion composes without the parent macro needing the child's fields:

```rust
// sketch of emitted metadata for `integrations: IntegrationSettings` (#[app_config(nested)])
// concatenate, at const-eval where possible, or via a generated fn:
//   prefix [Field("integrations")] onto each of <IntegrationSettings as AppConfigMeta>::SECRET_FIELDS
```

> Implementation note: `&'static [SecretField]` concatenation across crates is not trivial at `const` if paths must be `&'static [SecretPathSegment]`. Two viable lowerings: (a) generate a `const` block per struct that hand-builds the full flattened array literal by inlining child segments the macro can see through the opt-in type path; or (b) change `AppConfigMeta` from an associated `const` to an associated `fn secret_fields() -> Vec<SecretField>` (owned, path segments `Vec<...>`), letting recursion build owned vectors at runtime. **Recommendation:** prefer (b) — a `fn secret_fields() -> Cow<'static, [SecretField]>` — because it makes cross-crate recursion straightforward and the walk runs once per request anyway (allocation cost negligible vs. the network fetch). Flag this as **B-3** for maintainer decision, since it changes the `AppConfigMeta` trait shape.

Also relax `enforce_scalar_string_type` to additionally accept `Option<String>` on `#[secret]` fields (optional secrets are common in real config); an absent/`None` optional secret is skipped by the walk rather than erroring. Keep rejecting non-string scalar types.

### 4.4 Runtime `secret_walk` changes (`crates/edgezero-core/src/extractor.rs`)

Replace the top-level-only loop with a **path navigator**:

```rust
// For each SecretField, walk `data` along field.path:
//   Field(name)  -> descend into object key `name`
//   ArrayEach    -> iterate every element of the current array, applying the
//                   remainder of the path to each
// At the leaf: the string value is a secret KEY NAME; resolve it via the
// existing SecretKind logic (KeyInDefault / KeyInNamedStore / StoreRef) and
// replace in place. `store_ref_field` is looked up in the leaf's PARENT object.
```

- Preserve exact error semantics: missing/non-string leaf → `EdgeError::config_out_of_date` with the **dotted path** (e.g. `integrations.datadome.server_side_key`, `partners[3].api_key`) as the field hint. This improves on today's single-name hint.
- `Option<String>` secret that is absent → skip (no error).
- `StoreRef` leaves are still skipped (value is a store id).
- Keep the push/runtime validation split intact (`validate_excluding_secrets` at push; `cfg.validate()` at runtime after resolution). Push-time secret detection also reflects over the new path metadata.

### 4.5 CLI touchpoints (`edgezero-cli`)

`run_config_validate_typed` / `run_config_push_typed` / `run_config_diff_typed` reflect over `SECRET_FIELDS` (now paths) to know which fields hold key-names vs. values. Update those reflections to walk paths. `build_config_envelope` is unchanged (it serializes the typed struct verbatim; secret leaves already hold key names at push time). Verify the Spin lowercase-secret-name collision check still operates over the new path metadata.

### 4.6 Backward compatibility

- **Behavioral:** existing top-level `#[secret]` configs (e.g. `app-demo`'s `api_token`) resolve identically — their path is length 1.
- **Source-level (breaking within edgezero):** `SecretField.name: &'static str` → `SecretField.path: &'static [SecretPathSegment]` (and possibly `AppConfigMeta::SECRET_FIELDS` const → `secret_fields()` fn, per B-3). Every in-tree consumer (`secret_walk`, the CLI reflections, tests, `app-demo`) updates in the same PR. No external consumers exist yet besides `app-demo`. Provide a helper `SecretField::dotted_path() -> String` for error messages and CLI output.

### 4.7 Tests (edgezero)

- Derive UI tests (trybuild, matching the existing `crates/edgezero-macros/tests/ui/` style):
  - nested `#[app_config(nested)]` object with a `#[secret]` leaf compiles; emits the expected path.
  - `#[secret]` on `Option<String>` compiles; on `Vec<String>`/non-string still errors.
  - `#[serde(rename)]` anywhere along a secret path errors.
  - nested field annotated `#[app_config(nested)]` whose type does not derive `AppConfig` errors clearly.
  - (if arrays land) `Vec<Sub>` with `#[app_config(nested)]` emits `ArrayEach`.
- Runtime `secret_walk` tests: nested object leaf resolves from default store; nested `KeyInNamedStore` resolves `store_ref` sibling in the same parent; absent `Option` secret is skipped; missing required nested leaf errors with the dotted path; (if arrays) each element resolved independently.
- End-to-end `AppConfig<C>` extractor test with a 2-level nested secret over an `InMemorySecretStore`.

### 4.8 Docs

- `docs/guide/configuration.md` (and the blob-app-config spec/guide): document nested/array `#[secret]`, the `#[app_config(nested)]` opt-in, the `store_ref` sibling scoping rule, and the dotted-path error format.

---

## 5. Sequencing, dependencies, acceptance

- **A and B are independent.** Either can land first; both are prerequisites for trusted-server work (A → TS Phase 4 extractors; B → TS Phase 3 secret externalization).
- **Suggested order:** B first (it is the higher-risk design and gates the operator-facing secret migration), A alongside or after. Not a hard requirement.

**Acceptance criteria (edgezero CI gates apply):**

1. `cargo fmt` / clippy clean across `edgezero-core`, `edgezero-macros`, all adapters.
2. New unit + UI + integration tests (3.4, 4.7) pass.
3. `app-demo` still builds and serves on all four adapters; its top-level `#[secret]` still resolves.
4. `edgezero-cli` `config validate/push/diff` operate correctly over a config with a nested secret.
5. Rustdoc + guide updates (3.5, 4.8) merged.

---

## 6. Risks & open questions

| ID | Question | Recommendation |
|----|----------|----------------|
| B-1 | Are there secrets inside **arrays** in `Settings`, or only nested objects? | Audit `Settings` in TS Phase 3 scoping; design `SecretPathSegment::ArrayEach` in from day one but implement only if needed. |
| B-2 | How does the derive decide which fields to recurse into? | Explicit `#[app_config(nested)]` opt-in on the field (mirrors `#[validate(nested)]`); reject the type-heuristic alternative. |
| B-3 | Keep `AppConfigMeta::SECRET_FIELDS` as an associated `const`, or switch to `fn secret_fields()`? | Switch to a `fn` returning owned/`Cow` path segments — makes cross-crate recursion tractable; per-request cost is negligible. Maintainer decision as it reshapes the public trait. |
| A-1 | Name the extractor `State<T>` or `Extension<T>`? | `State<T>` + `with_state` primary; optionally `Extension<T>` alias. |
| A-2 | Should `State<T>` also be exposed via a `RequestContext` accessor (not just the extractor)? | Optional; add `ctx.state::<T>()` only if a non-`#[action]` call site needs it. Extractor is sufficient for TS. |
| GEN | Should this spec live in the edgezero repo instead of trusted-server? | It is filed here (trusted-server) as part of the umbrella migration; **hand a copy to the edgezero maintainer** to implement upstream, or relocate to `edgezero/docs/superpowers/specs/` if preferred. |

---

## 7. Files to touch (edgezero repo)

**Workstream A**
- `crates/edgezero-core/src/router.rs` — `RouterBuilder::with_state`, `RouterInner.state_inserters`, dispatch insertion (before `RequestContext::new`, alongside the introspection injects from PR #300).
- `crates/edgezero-core/src/extractor.rs` — `State<T>` extractor (+ `Deref`/`DerefMut`/`into_inner`). Making `State` `pub` here is sufficient; **no `lib.rs` crate-root re-export** (§8 [A, minor] — no extractor is re-exported at the crate root today; consumers use `edgezero_core::extractor::State`).
- `docs/guide/handlers.md`.

**Workstream B**
- `crates/edgezero-core/src/app_config.rs` — `SecretPathSegment`, reshaped `SecretField`, `AppConfigMeta` (const→fn per B-3), `dotted_path()` helper.
- `crates/edgezero-macros/src/app_config.rs` — recursion, `#[app_config(nested)]` parsing, relaxed scalar rule for `Option<String>`, path-aware guards.
- `crates/edgezero-core/src/extractor.rs` — path-navigating `secret_walk`.
- `crates/edgezero-cli/src/config.rs` — path-aware secret reflection in validate/push/diff.
- `crates/edgezero-macros/tests/ui/*` + core/CLI tests.
- `docs/guide/configuration.md`, blob-app-config guide.

---

## 8. Maintainer-review corrections (verified against `origin/main` @ `42843b1`, 2026-07-02)

A line-by-line review of every claim in §2–§7 against the actual code. **Both workstreams are implementable as designed; no blockers.** The current-mechanics claims (§2.1–§2.4) are all accurate. The items below are corrections and design call-outs to fold in before implementation.

### Verified accurate

- **A:** `RequestContext` wraps `Request` and reads handles from `request.extensions()` (`context.rs:13`, `kv_store_default` at `context.rs:123`); `FromRequest for Kv` matches the §2.1 quote (`extractor.rs:480`); `RequestContext::new(request, params)` (`context.rs:131`) and the owned-`request` dispatch site (`router.rs:267`, `RouteMatch::Found` at `router.rs:73`) make the "insert into `extensions_mut()` before `RequestContext::new`" plan sound. `RouterBuilder` derives `Default` (`router.rs:79`), so adding `state_inserters: Vec<_>` is non-breaking. `http` is a direct dep; `Extensions::insert`'s bound really is `Clone + Send + Sync + 'static`, so the spec's `T` bound is exactly right. `#[action]` emits `<Ty as FromRequest>::from_request(&__ctx).await?` (`action.rs:85`) — so `State<T>` needs **zero** macro change. `EdgeError::internal` → 500 (`error.rs:101,190`).
- **B:** `SecretField { kind, name: &'static str }` (`app_config.rs:41`); `SecretKind::{KeyInDefault, KeyInNamedStore{store_ref_field}, StoreRef}` (`app_config.rs:53`); `SECRET_FIELDS` is an associated `const` on `AppConfigMeta` (`app_config.rs:34`); `is_scalar_string_type` accepts bare `String` only and rejects `Option<String>`/`Vec`/nested (`macros/app_config.rs:275`); the serde `rename`/`flatten`/`skip*` and container `rename_all` guards exist (`macros/app_config.rs:336,363`); `secret_walk` is top-level `as_object_mut()` + per-field get/insert (`extractor.rs:827`) inside the `extract_from_handle` chain fetch → `verify()` → `secret_walk` → `serde_path_to_error` → `cfg.validate()` (`extractor.rs:766`); `EdgeError::config_out_of_date` is the real error used at the missing-leaf path (`extractor.rs:838`); push/runtime split via `validate_excluding_secrets` (`app_config.rs:204`) vs runtime `cfg.validate()` holds; app-demo's top-level `#[secret] api_token` (`app-demo-core/src/config.rs:24`) makes the length-1-path compat claim testable.

### Corrections to fold in

- **[A, minor] §3.2 use the `http` facade, not bare `http::Extensions`.** Core routes `http` through `crate::http` (`router.rs:13` already does `use crate::http::…`; alias at `http.rs:25`). Write `type StateInserter = Arc<dyn Fn(&mut crate::http::Extensions) + Send + Sync>` and the closure param as `&mut crate::http::Extensions`. Bare `http::Extensions` compiles but violates the crate's facade rule / CLAUDE.md.
- **[A, minor] §3.1/§7 the "re-export `State` in `lib.rs`" step is inaccurate.** No extractor is re-exported at the crate root today (`lib.rs:23` is just `pub mod extractor;`); consumers use `edgezero_core::extractor::State`. Making `State` `pub` in `extractor.rs` is sufficient. A crate-root re-export is optional and, if added, must sit under the existing `#![expect(clippy::pub_use, …)]` at `lib.rs:7`. Drop it from the required file-touch list.
- **[A, nit] §3.2 completeness:** also add the field to `RouterInner` (`router.rs:260`), a param to the private `RouterService::new` (`router.rs:343`), and pass `self.state_inserters` from `build()` (`router.rs:160`).
- **[B, IMPORTANT] §4.6 consumer list omits `validate_excluding_secrets`, and it is not a mechanical rename.** `validate_excluding_secrets` (`app_config.rs:204`) removes secret-field validators via `bag.remove(field.name)` on the **top-level** error map (line ~220). For a *nested* secret the failing validator lives under the parent inside a `ValidationErrorsKind::Struct`/`List`, so a flat remove-by-key will not find it → the nested secret's push-time validator would not be excluded, breaking the push/runtime split for exactly the new case. This needs nested-`ValidationErrors` navigation (the `first_violating_field` walk at `extractor.rs:926` is the pattern to reuse), not a `.name`→`.path` swap. Add to the consumer list **and** flag as real work.
- **[B, IMPORTANT] §4.3 reword point 3.** The derive is purely syntactic and cannot see a child struct's fields, so guards cannot be enforced "along the entire path" from the parent. They hold because **each struct on the path independently derives `AppConfig` and self-enforces** — which the B-2 `#[app_config(nested)]` opt-in + `AppConfigRoot` bound is what guarantees. Reword accordingly.
- **[B, IMPORTANT] B-3 is effectively forced to option (b).** Cross-crate `const` concatenation of `&'static [SecretPathSegment]` with a parent-prepended prefix is not expressible in stable `const` (the macro cannot see the child's segments — same syntactic limit as above), so option (a) is not viable. Go with `fn secret_fields() -> Cow<'static, [SecretField]>`. Note the churn is wider than §4.6 states: **every** `impl AppConfigMeta`/`SECRET_FIELDS` site flips, including ~10 hand-rolled test impls in `app_config.rs`/`extractor.rs`/`config.rs` tests.
- **[B, minor] §4.5 point at the real reflection helpers.** The per-field top-level `raw_table.get(field.name)` lookups are in `run_adapter_typed_checks` (`config.rs:1296`) and `typed_secret_checks` (`config.rs:1338`), reached *through* `run_config_{validate,push,diff}_typed`. The Spin lowercase-collision check is `validate_typed_secrets` in `adapter-spin/src/cli.rs:514` (keys on the secret **value**, so it survives the path reshape; only the printed field name becomes a dotted path).
- **[B, minor] §4.7 the nested `KeyInNamedStore` "innermost parent" scoping is new behavior with no existing fixture.** app-demo has only `KeyInDefault` (`api_token`) and `StoreRef` (`vault`) — no `KeyInNamedStore` field — so that test needs a purpose-built fixture; it cannot lean on app-demo.
- **[B, nit] §4.2/§4.4 dotted-path rendering:** the spec uses both `partners[*]` (static metadata) and `partners[3]` (runtime error). Have `SecretField::dotted_path()` render `ArrayEach` as `[*]` for the static path and per-index `[n]` at runtime; the existing `[{idx}]` convention (`extractor.rs:959`) already matches.
- **[GEN, resolved] §6 GEN row** ("should this spec live in edgezero or trusted-server?") is now answered — it lives at `docs/superpowers/specs/` in the edgezero repo.

### Second-pass blockers (must fold into Workstream B before it is plan-ready)

A follow-up review found additional issues the first pass missed. All verified against `origin/main` @ `42843b1`.

- **[B, BLOCKER] An active CI guard forbids the exact nesting this spec introduces.** `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs` (spec 10.2.1) detects any `AppConfig`-derived struct used as a field inside another `AppConfig`-derived struct — unwrapping `Option`/`Vec`/`Box`/`Rc`/`Arc`/tuples/arrays — and CI runs it as "Nested AppConfig audit" (`.github/workflows/test.yml:58`, scanning `examples/app-demo` + `crates/edgezero-cli/src/templates`). Workstream B's whole premise (a sub-struct that itself derives `AppConfig`, opted-in via `#[app_config(nested)]`) is a violation today. The guard must be **inverted**, not deleted: "nested `AppConfig` is allowed **iff** the containing field carries `#[app_config(nested)]`." This is a required plan item, and it changes the audit binary + its tests.
- **[B, BLOCKER] Optional secrets need metadata.** §4.3 accepts `Option<String>` and §4.4 skips an absent optional, but §4.2's `SecretField` carries only `{ kind, path }`. The runtime walk and the CLI reflections cannot distinguish "required leaf missing → error" from "optional leaf absent → skip" without an explicit flag. Add `optional: bool` (or fold it into `SecretKind`).
- **[B, BLOCKER] The path model is internally inconsistent — commit to owned segments.** §4.2 shows `path: &'static [SecretPathSegment]` while §4.3/B-3 conclude nested recursion must build owned/`Cow` paths (the only viable cross-crate lowering). These contradict. Resolve by making paths owned: `secret_fields() -> Vec<SecretField>` with `path: Vec<SecretPathSegment>` (or `Cow<'static, _>`). Update §4.2 to match B-3 rather than leaving a `&'static` shape it cannot satisfy.
- **[B, HIGH] Register the helper attribute.** `crates/edgezero-macros/src/lib.rs:20` is `#[proc_macro_derive(AppConfig, attributes(secret))]` — it must become `attributes(secret, app_config)` or the `#[app_config(nested)]` opt-in fails to parse.
- **[B, HIGH] `TypedSecretEntry.field_name` is a borrowed `&'static`/`&'entry str`.** `crates/edgezero-adapter/src/registry.rs:178` borrows the field name; dotted/array paths (`partners[3].api_key`) are computed strings with no `'static` backing. Make `field_name` owned (`String`/`Cow<'static, str>`) or carry an owned label alongside the borrowed entry, and thread that through the Spin collision check.
- **[B, HIGH] Enforce container `rename_all` on nested-only parents.** `crates/edgezero-macros/src/app_config.rs:75` gates the `rename_all` guard on `!annotations.is_empty()` (direct `#[secret]` fields only). A parent whose secrets live entirely in `#[app_config(nested)]` children has empty direct annotations, so a container `#[serde(rename_all=…)]` there would silently desync the emitted Rust field-path segment from the serialized key. Extend the guard to also fire when the struct has any `#[app_config(nested)]` field.
- **[B, HIGH] Decide array scope now, not later.** The problem statement and examples (§4.1, `partners[*].api_key`) include array secrets, yet B-1 defers `ArrayEach`. Do not write an object-only plan unless trusted-server's `Settings` audit confirms **no** secret leaves inside arrays; otherwise include `ArrayEach` from the start.

### Go / No-Go

Split into **two independent implementation plans** (matching §5's "A and B are independent"):

- **Workstream A (`State<T>`) is plan-ready now** — no blockers; router insertion before `RequestContext::new` (`router.rs:267`) and the generic `#[action]` `FromRequest` call (`action.rs:87`) both fit as designed.
- **Workstream B (nested/array secrets) should start only after** folding in the second-pass blockers above — especially: inverting the nested-AppConfig CI guard, `optional` metadata, owned path segments, helper-attribute registration, owned `TypedSecretEntry` labels, nested-only `rename_all` enforcement, and a settled array scope.

### Baseline note

Local `main` (`b298bc1`, "Extract run_typed_preflight…") is a **broken divergent tip**: it deletes `config.rs` (3365 lines) while leaving `mod config;`/`pub use config::…` in `lib.rs`, so `edgezero-cli` does not compile there, and it is not an ancestor of `origin/main`. `run_typed_preflight` exists nowhere in the tree — that refactor's new files were never committed. All §4.5 claims were therefore verified against `origin/main` (`42843b1`), where `config.rs` is intact; the spec's function names are correct against that tree.
