# EdgeZero Nested / Array `#[secret]` Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `#[secret]` fields live below the config root — nested inside sub-structs and inside `Vec<_>` elements — resolved at runtime by a **field path** instead of a single top-level name.

**Architecture:** Reshape secret metadata from a flat `SecretField { kind, name: &'static str }` into path-qualified, **owned** `SecretField { kind, path: Vec<SecretPathSegment>, optional: bool }`, and change `AppConfigMeta` from an associated `const SECRET_FIELDS` to `fn secret_fields() -> Vec<SecretField>` so the derive can recurse across crates (a parent prepends its field/`ArrayEach` segment onto each child's `secret_fields()`). The runtime `secret_walk`, the push-time `validate_excluding_secrets`, and the CLI reflections all become path navigators. A new `#[app_config(nested)]` field opt-in drives recursion, and the existing "no nested AppConfig" CI guard is **inverted** to allow nesting only on opted-in fields. Arrays (`ArrayEach`) are included from day one.

**Tech Stack:** Rust 1.95, edition 2021. `edgezero-core` (metadata + runtime walk + push validation), `edgezero-macros` (derive recursion + attribute parsing), `edgezero-cli` (path-aware validate/push/diff + inverted CI guard binary), `edgezero-adapter` (owned secret-entry label), `edgezero-adapter-spin` (collision check over paths). `serde_json` / `toml` / `validator` 0.20.

## Base branch

- **Implementation branch:** `worktree-state-nested-secrets-spec-review`, with **PR #300 already merged** (merge commit `051a9ad`). PR #300 touches none of this plan's files, so every line number below (verified live on the merged tree) is identical to `origin/main @ 42843b1`.
- Shares its branch with the sibling **`State<T>`** plan (`2026-07-02-edgezero-state-extractor.md`). The only shared file is `crates/edgezero-core/src/extractor.rs`, edited in a disjoint region (that plan appends an extractor; this plan rewrites `secret_walk` at `extractor.rs:827`). Either order is safe.
- **This plan is the source of truth and supersedes the spec's pre-correction shapes.** The spec's §2–§7 still show stale forms that §8 (and its second-pass blockers) overrode: borrowed `&'static` path segments without `optional` (spec §4.2 / line 257), array scope framed as "open/defer" (spec B-1 / line 274), and a `State` crate-root re-export (spec §7 / line 379). Where the spec body and this plan disagree, follow the plan. (The spec body should be reconciled with §8 separately so implementers aren't handed contradictory instructions.)

## Global Constraints

- **Rust 1.95.0**, edition 2021, resolver 2.
- **WASM-compat:** no Tokio; `#[async_trait(?Send)]`; async tests use `futures::executor::block_on`.
- **HTTP facade:** never import `http` directly (not relevant to most of this plan, but holds).
- **Colocate tests** in `#[cfg(test)] mod tests`.
- **`validator` is 0.20**: `ValidationErrors::errors()` → `&HashMap<Cow<'static, str>, ValidationErrorsKind>`; `errors_mut()` → `&mut` of the same. `ValidationErrorsKind::{Field(Vec<ValidationError>), Struct(Box<ValidationErrors>), List(BTreeMap<usize, Box<ValidationErrors>>)}`. Keys are `Cow<'static, str>` and `.as_ref()` gives `&str`.
- **Push/runtime validation split is sacred:** push time uses `validate_excluding_secrets` (secret leaves hold key NAMES, so their per-field validators are skipped); runtime uses `cfg.validate()` after `secret_walk` has resolved values. Nesting must preserve this for nested/array secrets too.
- **`#[secret(store_ref)]` (`StoreRef` kind) leaves are always skipped** by the walk and kept by push validation (their value is a store id, identical at push and runtime).
- **`store_ref` sibling scoping rule:** a `KeyInNamedStore { store_ref_field }` leaf resolves its `store_ref_field` sibling **within the same innermost parent object** as the secret leaf.
- **Array metadata is `[*]`; runtime errors are `[n]`.** `SecretField::dotted_path()` renders `ArrayEach` as `[*]` (static form); `secret_walk` builds `[n]` per element at runtime. Matches the existing `format!("{path}[{idx}]")` convention at `extractor.rs:959`.
- **CI gates (all must pass):**
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  3. `cargo test --workspace --all-targets`
  4. `cargo check --workspace --all-targets --features "fastly cloudflare spin"`
  5. `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`
  6. **Nested AppConfig audit** (`.github/workflows/test.yml:58`): `cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates`

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `crates/edgezero-core/src/app_config.rs` | `SecretPathSegment` enum, reshaped owned `SecretField { kind, path, optional }`, `AppConfigMeta::secret_fields()` fn (was const), `SecretField::dotted_path()`, nested/list-aware `validate_excluding_secrets` | Modify |
| `crates/edgezero-core/src/extractor.rs` | Path-navigating `secret_walk` (Field descent + `ArrayEach` + optional skip + `KeyInNamedStore` sibling-in-parent + dotted runtime error path) | Modify (`secret_walk` region) |
| `crates/edgezero-macros/src/lib.rs` | Register the `app_config` helper attribute | Modify (1 line) |
| `crates/edgezero-macros/src/app_config.rs` | Emit `fn secret_fields()` with owned path segments + `optional`; parse `#[app_config(nested)]`; recurse (object → `Field`, `Vec` → `Field` + `ArrayEach`); relax to accept `Option<String>`; extend `rename_all` guard to nested-only parents; assert nested types derive `AppConfig` | Modify |
| `crates/edgezero-macros/tests/ui/*` + `tests/app_config_derive.rs` | UI + happy-path derive coverage for nesting/arrays/optional | Modify / create |
| `crates/edgezero-cli/src/config.rs` | Path-aware secret reflection in `run_adapter_typed_checks` + `typed_secret_checks` (TOML path navigator); flip all test `impl AppConfigMeta` const → fn | Modify |
| `crates/edgezero-adapter/src/registry.rs` | `TypedSecretEntry.field_name` → owned `String` (dotted label) | Modify |
| `crates/edgezero-adapter-spin/src/cli.rs` | Collision check consumes owned label (logic unchanged — keys on value) | Modify |
| `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs` | Invert: nested `AppConfig` allowed **iff** the field carries `#[app_config(nested)]`; add tests | Modify |
| `docs/guide/configuration.md` | Document nested/array `#[secret]`, the opt-in, sibling scoping, dotted-path errors | Modify |

**Task ordering rationale:** Task 1 is the atomic metadata reshape — it changes the trait shape and every in-tree consumer in one green step, but only ever produces **length-1** paths, so behavior is byte-identical to today. It also builds the *full* Field+`ArrayEach` navigators up front, so once the macro (Task 4) emits longer paths, nesting "just works." Tasks 2–3 add runtime + push path-awareness (still exercised only by hand-written multi-segment test fixtures). Task 4 makes the derive emit nested/array metadata. Task 5 inverts the CI guard. Task 6 makes the CLI path-aware. Task 7 is the end-to-end proof + docs.

---

## Task 1: Reshape secret metadata to owned, path-qualified fields

This is the foundation: new types, `const`→`fn` trait, `dotted_path()`, and updates to **every** in-tree consumer so the workspace stays green with identical top-level behavior (all paths length 1).

**Files:**
- Modify: `crates/edgezero-core/src/app_config.rs` (types, trait, `dotted_path`, and `validate_excluding_secrets` — flat behavior for now; nested pruning lands in Task 3)
- Modify: `crates/edgezero-core/src/extractor.rs` (`secret_walk` signature stays; body reads `C::secret_fields()` — full navigator lands in Task 2; for Task 1 keep top-level behavior but via the new shape)
- Modify: `crates/edgezero-cli/src/config.rs` (test `impl AppConfigMeta` const→fn; consumers read `field.path`/`dotted_path()` at length 1)
- Modify: `crates/edgezero-macros/src/app_config.rs` (emit `fn secret_fields()` with length-1 `Field` paths + `optional: false`)
- Modify: `crates/edgezero-adapter/src/registry.rs` (`field_name: String`)
- Modify: `crates/edgezero-adapter-spin/src/cli.rs` (consume owned label)

**Interfaces produced (relied on by all later tasks):**
```rust
// crates/edgezero-core/src/app_config.rs
pub enum SecretPathSegment { Field(std::borrow::Cow<'static, str>), ArrayEach }
pub struct SecretField { pub kind: SecretKind, pub path: Vec<SecretPathSegment>, pub optional: bool }
impl SecretField { pub fn dotted_path(&self) -> String; }        // Field→"a.b", ArrayEach→"[*]"
pub trait AppConfigMeta { fn secret_fields() -> Vec<SecretField>; } // was: const SECRET_FIELDS
// SecretKind is UNCHANGED (still Copy, store_ref_field: &'static str).
```

- [ ] **Step 1: Write the failing metadata unit tests**

Append to `crates/edgezero-core/src/app_config.rs`'s `#[cfg(test)] mod tests` (module starts near `app_config.rs:599`; it already imports `SecretField`, `SecretKind`):

```rust
    #[test]
    fn dotted_path_renders_nested_and_array_segments() {
        use super::{SecretField, SecretKind, SecretPathSegment::*};
        use std::borrow::Cow;

        let top = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![Field(Cow::Borrowed("api_token"))],
            optional: false,
        };
        assert_eq!(top.dotted_path(), "api_token");

        let nested = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                Field(Cow::Borrowed("integrations")),
                Field(Cow::Borrowed("datadome")),
                Field(Cow::Borrowed("server_side_key")),
            ],
            optional: false,
        };
        assert_eq!(nested.dotted_path(), "integrations.datadome.server_side_key");

        let array = SecretField {
            kind: SecretKind::KeyInDefault,
            path: vec![
                Field(Cow::Borrowed("partners")),
                ArrayEach,
                Field(Cow::Borrowed("api_key")),
            ],
            optional: false,
        };
        assert_eq!(array.dotted_path(), "partners[*].api_key");
    }
```

- [ ] **Step 2: Run it (fails to compile)**

Run: `cargo test -p edgezero-core --lib dotted_path_renders 2>&1 | tail -15`
Expected: FAIL — `SecretPathSegment` / `SecretField.path` / `dotted_path` do not exist.

- [ ] **Step 3: Reshape the metadata types + trait in `app_config.rs`**

Add `use std::borrow::Cow;` near the top of `crates/edgezero-core/src/app_config.rs` (with the other `use`s). Replace the `AppConfigMeta` trait (`app_config.rs:34-37`), the `SecretField` struct (`app_config.rs:41-48`), and add `SecretPathSegment` + `dotted_path`. `SecretKind` (`app_config.rs:53-69`) is **unchanged**.

```rust
/// One segment of a [`SecretField`] path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretPathSegment {
    /// An object key — a Rust field name, verbatim (no `serde(rename)`).
    Field(Cow<'static, str>),
    /// Every element of an array/`Vec` at this position.
    ArrayEach,
}

/// One field's worth of secret-annotation metadata.
///
/// The `path` locates the secret leaf from the config root. A top-level
/// scalar has a length-1 path `[Field("api_token")]`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretField {
    /// Which secret-store resolution this field participates in.
    pub kind: SecretKind,
    /// Path from the config root to the secret leaf.
    pub path: Vec<SecretPathSegment>,
    /// `true` for `#[secret]` on `Option<String>`: an absent leaf is
    /// skipped by the runtime walk instead of erroring.
    pub optional: bool,
}

impl SecretField {
    /// Human-readable dotted path for error messages and CLI output.
    /// `ArrayEach` renders as `[*]` (the static form); the runtime walk
    /// renders per-index `[n]` as it descends.
    #[must_use]
    pub fn dotted_path(&self) -> String {
        let mut out = String::new();
        for segment in &self.path {
            match segment {
                SecretPathSegment::Field(name) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(name);
                }
                SecretPathSegment::ArrayEach => out.push_str("[*]"),
            }
        }
        out
    }
}

/// Per-field metadata emitted by `#[derive(AppConfig)]`. `config validate`
/// / `config push` and the runtime secret walk reflect over this to gate
/// secret-aware behaviour.
pub trait AppConfigMeta {
    /// Every `#[secret]` / `#[secret(store_ref)]` leaf on the struct,
    /// including those reached through `#[app_config(nested)]` children,
    /// each carrying its full path from this struct's root.
    fn secret_fields() -> Vec<SecretField>;
}
```

Note: `SecretField` and `SecretPathSegment` are **no longer `Copy`** (they own a `Vec`/`Cow`). This is intentional per §8 [B, BLOCKER]. `SecretKind` stays `Copy`.

- [ ] **Step 4: Run the metadata test (passes)**

Run: `cargo test -p edgezero-core --lib dotted_path_renders 2>&1 | tail -15`
Expected: PASS. (The crate will not fully build yet — consumers still reference the old shape. Fix them in the next steps.)

- [ ] **Step 5: Update `validate_excluding_secrets` to the new shape (flat behavior preserved)**

In `crates/edgezero-core/src/app_config.rs:204-226`, the loop currently does `bag.remove(field.name)`. For Task 1, keep flat removal but source the key from the length-1 path. (Task 3 replaces this with nested/list-aware pruning.) Change the loop body:

```rust
    let bag = errors.errors_mut();
    for field in C::secret_fields() {
        if matches!(field.kind, SecretKind::StoreRef) {
            continue; // store_id field; validator stays
        }
        // Task 1: flat removal by the first path segment (length-1 paths only
        // exist until the derive emits nesting). Task 3 makes this nested-aware.
        if let Some(SecretPathSegment::Field(name)) = field.path.first() {
            bag.remove(name.as_ref());
        }
    }
```

Add `use SecretPathSegment` access (it's in the same module, reference as `SecretPathSegment::Field`).

- [ ] **Step 6: Update `secret_walk` to the new shape (top-level behavior preserved)**

In `crates/edgezero-core/src/extractor.rs:827-894`, change the import at `extractor.rs:8` to also bring in the path segment, and change the loop to source the key/field name from the length-1 path. (Task 2 replaces the whole body with a recursive navigator.) Minimal Task-1 change: replace `for field in C::SECRET_FIELDS` with `for field in C::secret_fields()`, and replace each `field.name` use with a locally computed `let leaf = field.dotted_path();` for error hints and `let leaf_key = match field.path.last() { Some(SecretPathSegment::Field(n)) => n.as_ref(), _ => /* length-1 guaranteed in Task 1 */ };` for the `data_obj.get(...)`/`insert(...)` calls. Concretely, at the top of the loop:

```rust
    for field in C::secret_fields() {
        // Task 1: top-level only — the leaf is the single Field segment.
        let leaf_key = match field.path.last() {
            Some(SecretPathSegment::Field(name)) => name.clone().into_owned(),
            _ => {
                return Err(EdgeError::internal(anyhow::anyhow!(
                    "secret field `{}` has no field leaf",
                    field.dotted_path()
                )))
            }
        };
        let hint = field.dotted_path();
        // ... below, replace `field.name` (get/insert key) with `leaf_key.as_str()`
        //     and `field.name.to_owned()` (error path arg) with `hint.clone()`.
```

Update `crates/edgezero-core/src/extractor.rs:8` from `use crate::app_config::{AppConfigMeta, SecretKind};` to `use crate::app_config::{AppConfigMeta, SecretKind, SecretPathSegment};`. Apply the `leaf_key`/`hint` substitution throughout the existing loop body (the `data_obj.get(field.name)`, the `data_obj.insert(field.name.to_owned(), ...)`, and every `field.name.to_owned()` error arg become `leaf_key.as_str()` / `hint.clone()` respectively; `store_ref_field` handling is unchanged — it's still a top-level sibling in Task 1).

- [ ] **Step 7: Flip the emitter in `edgezero-macros/src/app_config.rs` to `fn` + length-1 paths**

In `crates/edgezero-macros/src/app_config.rs`, change the per-entry emission (`app_config.rs:128-150`) and the impl block (`app_config.rs:152-166`). The entries currently emit `SecretField { name: #name_lit, kind: #kind_tokens }`; change to owned length-1 paths:

```rust
    let entries = annotations.iter().map(|annotation| {
        let name_lit = annotation.name.to_string();
        let kind_tokens = match &annotation.kind {
            SecretAnnotation::KeyInDefault => {
                quote!(::edgezero_core::app_config::SecretKind::KeyInDefault)
            }
            SecretAnnotation::StoreRef => {
                quote!(::edgezero_core::app_config::SecretKind::StoreRef)
            }
            SecretAnnotation::KeyInNamedStore { store_ref_field } => {
                let lit = syn::LitStr::new(store_ref_field, Span::call_site());
                quote!(::edgezero_core::app_config::SecretKind::KeyInNamedStore {
                    store_ref_field: #lit
                })
            }
        };
        // Task 1: length-1 Field path, non-optional. Task 4 sets `optional`
        // from Option<String> and prepends nested/array segments.
        quote! {
            ::edgezero_core::app_config::SecretField {
                kind: #kind_tokens,
                path: ::std::vec![::edgezero_core::app_config::SecretPathSegment::Field(
                    ::std::borrow::Cow::Borrowed(#name_lit)
                )],
                optional: false,
            }
        }
    });
```

And the impl block (`app_config.rs:152-166`) — change the `const` to a `fn`:

```rust
    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigMeta
            for #struct_ident #type_generics #where_clause
        {
            fn secret_fields() -> ::std::vec::Vec<::edgezero_core::app_config::SecretField> {
                ::std::vec![#(#entries),*]
            }
        }

        #[automatically_derived]
        impl #impl_generics ::edgezero_core::app_config::AppConfigRoot
            for #struct_ident #type_generics #where_clause
        {}
    })
}
```

- [ ] **Step 8: Make `TypedSecretEntry.field_name` owned**

In `crates/edgezero-adapter/src/registry.rs:174-198`, change `field_name: &'entry str` to owned `String`. **Keep `new`'s param generic as `impl Into<String>`** so the 7 existing `&str`-literal call sites in the Spin tests (`adapter-spin/src/cli.rs:1292/1307/1327/1328/1344/1345/1357/1389/1390`) keep compiling unchanged, while the CLI callers pass owned dotted labels:

```rust
#[non_exhaustive]
pub struct TypedSecretEntry<'entry> {
    /// Dotted secret-field path label (e.g. `"partners[3].api_key"`).
    pub field_name: String,
    /// Blob value — i.e. the secret-store KEY NAME.
    pub key_value: &'entry str,
    /// Logical secret-store id this key targets.
    pub store_id: &'entry str,
}

impl<'entry> TypedSecretEntry<'entry> {
    #[must_use]
    #[inline]
    pub fn new(
        store_id: &'entry str,
        field_name: impl Into<String>,
        key_value: &'entry str,
    ) -> Self {
        Self {
            field_name: field_name.into(),
            key_value,
            store_id,
        }
    }
}
```

Because `&str: Into<String>` and `String: Into<String>`, no `TypedSecretEntry::new` call site needs editing for the signature change (only the CLI callers change *what* they pass — a dotted label — in Task 6).

- [ ] **Step 9: Update the Spin collision check to the owned label**

In `crates/edgezero-adapter-spin/src/cli.rs:514-552`, the logic keys on `entry.key_value` (the secret value) — unchanged. Only the `seen` map value type shifts from `&str` (borrowing a `&'static` name) to a borrow of the owned `String`. Change the map value binding so it borrows `entry.field_name`:

```rust
        let mut seen: HashMap<String, &str> = HashMap::with_capacity(entries.len());
        for entry in entries {
            let spin_var = entry.key_value.to_ascii_lowercase();
            if !is_valid_spin_key(&spin_var) {
                let reason = spin_key_rule_violation(&spin_var);
                return Err(format!(
                    "`#[secret]` field `{field}` value `{value}` translates to Spin variable `{spin_var}`, which is not a valid Spin variable name. {reason}. Pick a `#[secret]` value that conforms.",
                    field = entry.field_name,
                    value = entry.key_value,
                ));
            }
            if let Some(prev_field) = seen.insert(spin_var.clone(), entry.field_name.as_str()) {
                return Err(format!(
                    "Spin variable `{spin_var}` would receive values from BOTH `#[secret]` field `{prev_field}` AND `#[secret]` field `{this_field}`; Spin's flat variable namespace cannot disambiguate them. Pick distinct `#[secret]` values whose lowercased forms differ.",
                    this_field = entry.field_name,
                ));
            }
        }
        Ok(())
```

(Only two edits vs. today: `entry.field_name` is now a `String` so it interpolates the same in `format!`, and the `seen.insert(..., entry.field_name)` becomes `entry.field_name.as_str()`.)

- [ ] **Step 10: Update the CLI consumers + all test `impl AppConfigMeta` sites**

In `crates/edgezero-cli/src/config.rs`, update the two runtime consumers to the new shape (still flat/length-1 in Task 1 — full path navigation lands in Task 6):

- `run_adapter_typed_checks` (`config.rs:1295-1333`): change `for field in C::SECRET_FIELDS` → `for field in C::secret_fields()`; compute `let leaf = field.dotted_path();` and a flat lookup key `let key = match field.path.last() { Some(SecretPathSegment::Field(n)) => n.as_ref(), _ => continue };`; replace `raw_table.get(field.name)` with `raw_table.get(key)`; replace `TypedSecretEntry::new(store_id, field.name, key_value)` with `TypedSecretEntry::new(store_id, leaf.clone(), key_value)`. `store_ref_field` lookups are unchanged (still `raw_table.get(store_ref_field)`, top-level in Task 1).
- `typed_secret_checks` (`config.rs:1339-1412`): same `for field in C::secret_fields()`; compute `let leaf = field.dotted_path();` and `let key = /* leaf field name as above */;`; replace `raw_table.get(field.name)` with `raw_table.get(key)`; replace every `field.name` in error messages with `leaf`.

Add `SecretPathSegment` to the import at `config.rs:28` (`use edgezero_core::app_config::{AppConfigMeta, SecretKind, SecretPathSegment};`).

Then flip **every hand-written `impl AppConfigMeta`** from `const SECRET_FIELDS: &'static [SecretField] = &[ ... ];` to `fn secret_fields() -> Vec<SecretField> { vec![ ... ] }`, converting each `SecretField { name: "x", kind: K }` literal to `SecretField { kind: K, path: vec![SecretPathSegment::Field(Cow::Borrowed("x"))], optional: false }`. Sites (all in `#[cfg(test)]`), verified line numbers:

  - `crates/edgezero-core/src/app_config.rs`: `:620`, `:1106`, `:1138`, `:1156`, `:1181`, `:1201`
  - `crates/edgezero-core/src/extractor.rs`: `:1049`, `:1062`, `:2329`, `:2370`
  - `crates/edgezero-cli/src/config.rs`: `:1649`, `:1866`, `:2204`, `:2746`, `:3315`

Worked example — `app_config.rs:1156-1161` (empty→one-field) currently:

```rust
    impl AppConfigMeta for Fixture {
        const SECRET_FIELDS: &'static [SecretField] =
            &[SecretField { name: "api_token", kind: SecretKind::KeyInDefault }];
    }
```

becomes:

```rust
    impl AppConfigMeta for Fixture {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![SecretPathSegment::Field(std::borrow::Cow::Borrowed("api_token"))],
                optional: false,
            }]
        }
    }
```

For each such test module, add `use edgezero_core::app_config::SecretPathSegment;` (or `use super::SecretPathSegment;` inside core) and `use std::borrow::Cow;` if not already present. Empty-array impls become `fn secret_fields() -> Vec<SecretField> { vec![] }`. Assertions that read `Type::SECRET_FIELDS` (e.g. the derive test in `crates/edgezero-macros/tests/app_config_derive.rs:71-126` and app-demo `config.rs:126`) change to `Type::secret_fields()` and compare against the new shape — update those in this step too (app-demo assertion detail below).

App-demo assertion at `examples/app-demo/crates/app-demo-core/src/config.rs:124-138` currently maps `AppDemoConfig::SECRET_FIELDS` `.map(|f| (f.name, f.kind))`. Change to:

```rust
        let by_path: Vec<(String, SecretKind)> = AppDemoConfig::secret_fields()
            .into_iter()
            .map(|f| (f.dotted_path(), f.kind))
            .collect();
        assert_eq!(
            by_path,
            vec![
                ("api_token".to_owned(), SecretKind::KeyInDefault),
                ("vault".to_owned(), SecretKind::StoreRef),
            ],
        );
```

The derive assertions in `app_config_derive.rs:71-126` change from comparing `SECRET_FIELDS` slices to comparing `secret_fields()` `Vec`s against `SecretField { kind, path: vec![SecretPathSegment::Field(Cow::Borrowed("..."))], optional: false }` literals (or, more simply, assert `dotted_path()` + `kind` + `optional` per entry).

- [ ] **Step 11: Build + test the whole workspace (green, behavior identical)**

Run: `cargo build --workspace --all-targets 2>&1 | tail -20`
Expected: compiles.

Run: `cargo test --workspace --all-targets 2>&1 | tail -25`
Expected: PASS — all existing secret tests still green (top-level behavior unchanged). Also run app-demo:

Run: `(cd examples/app-demo && cargo test 2>&1 | tail -15)`
Expected: PASS (`secret_fields_metadata_matches_declarations`, round-trip, config-flow).

- [ ] **Step 12: Lint + commit**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -15`
Expected: clean.

```bash
git add crates/edgezero-core/src/app_config.rs crates/edgezero-core/src/extractor.rs \
        crates/edgezero-macros/src/app_config.rs crates/edgezero-cli/src/config.rs \
        crates/edgezero-adapter/src/registry.rs crates/edgezero-adapter-spin/src/cli.rs \
        examples/app-demo/crates/app-demo-core/src/config.rs \
        crates/edgezero-macros/tests/app_config_derive.rs
git commit -m "refactor(secrets): owned path-qualified SecretField + AppConfigMeta::secret_fields()"
```

---

## Task 2: Path-navigating runtime `secret_walk` (nesting + arrays + optional)

Replace `secret_walk`'s top-level loop with a recursive navigator that descends `Field`/`ArrayEach` segments, resolves the leaf, skips absent optionals, and reports the dotted runtime path (`[n]` per index) on error. Exercised via hand-written multi-segment `impl AppConfigMeta` fixtures.

**Files:**
- Modify: `crates/edgezero-core/src/extractor.rs` (`secret_walk` at `:827`; add tests to the `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `SecretField { kind, path, optional }`, `SecretPathSegment` (Task 1); `SecretKind` (unchanged); `ctx.secret_store_default()` / `ctx.secret_store(id)` / `bound.require_str(key)` / `map_secret_error` (existing, `extractor.rs:896`); `first_violating_field`'s `[{idx}]` convention.
- Produces: a `secret_walk::<C>` that resolves nested/array leaves. Consumed by Task 7 (E2E).

- [ ] **Step 1: Write failing nested/array `secret_walk` tests**

Append to `crates/edgezero-core/src/extractor.rs` `#[cfg(test)] mod tests`. Mirror the existing `app_config_secret_walk_resolves_key_in_default_store` test (`extractor.rs:2170`) for store setup (`InMemorySecretStore`, `StoreRegistry`, inserting the secret registry into request extensions, building `ctx`). Add three fixtures + tests:

```rust
    // Nested object leaf: integrations.datadome.server_side_key
    struct NestedCfg;
    impl AppConfigMeta for NestedCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("integrations")),
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("datadome")),
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("server_side_key")),
                ],
                optional: false,
            }]
        }
    }

    #[test]
    fn secret_walk_resolves_nested_object_leaf() {
        let ctx = ctx_with_default_secret_store("dd_key", "resolved-dd"); // helper: see below
        let mut data = serde_json::json!({
            "integrations": { "datadome": { "server_side_key": "dd_key" } }
        });
        block_on(secret_walk::<NestedCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(
            data["integrations"]["datadome"]["server_side_key"],
            serde_json::json!("resolved-dd")
        );
    }

    // Array leaf: partners[*].api_key
    struct ArrayCfg;
    impl AppConfigMeta for ArrayCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("partners")),
                    SecretPathSegment::ArrayEach,
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("api_key")),
                ],
                optional: false,
            }]
        }
    }

    #[test]
    fn secret_walk_resolves_each_array_element() {
        let ctx = ctx_with_default_secret_store_map(&[("k0", "v0"), ("k1", "v1")]);
        let mut data = serde_json::json!({
            "partners": [ { "api_key": "k0" }, { "api_key": "k1" } ]
        });
        block_on(secret_walk::<ArrayCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(data["partners"][0]["api_key"], serde_json::json!("v0"));
        assert_eq!(data["partners"][1]["api_key"], serde_json::json!("v1"));
    }

    // Nested KeyInNamedStore: vaulted.token resolves against the store named by
    // its SIBLING `vaulted.vault` (the sibling-in-innermost-parent scoping rule).
    struct NamedStoreCfg;
    impl AppConfigMeta for NamedStoreCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInNamedStore { store_ref_field: "vault" },
                path: vec![
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("vaulted")),
                    SecretPathSegment::Field(std::borrow::Cow::Borrowed("token")),
                ],
                optional: false,
            }]
        }
    }

    #[test]
    fn secret_walk_resolves_nested_named_store_via_sibling_in_parent() {
        // A registry whose store id "named" maps key "tok_key" -> "TOK".
        let ctx = ctx_with_named_secret_store("named", "tok_key", "TOK");
        let mut data = serde_json::json!({
            "vaulted": { "token": "tok_key", "vault": "named" }
        });
        block_on(secret_walk::<NamedStoreCfg>(&ctx, &mut data)).expect("walk");
        assert_eq!(data["vaulted"]["token"], serde_json::json!("TOK"));
        // The store_ref sibling is left intact (it names a store, not a secret).
        assert_eq!(data["vaulted"]["vault"], serde_json::json!("named"));
    }

    #[test]
    fn secret_walk_nested_named_store_missing_sibling_errors_with_dotted_path() {
        let ctx = ctx_with_named_secret_store("named", "tok_key", "TOK");
        let mut data = serde_json::json!({ "vaulted": { "token": "tok_key" } }); // no `vault`
        let err = block_on(secret_walk::<NamedStoreCfg>(&ctx, &mut data))
            .expect_err("missing store_ref sibling");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.to_string().contains("vaulted.token"));
    }

    // Optional secret absent -> skipped (no error)
    struct OptionalCfg;
    impl AppConfigMeta for OptionalCfg {
        fn secret_fields() -> Vec<SecretField> {
            vec![SecretField {
                kind: SecretKind::KeyInDefault,
                path: vec![SecretPathSegment::Field(std::borrow::Cow::Borrowed("maybe_key"))],
                optional: true,
            }]
        }
    }

    #[test]
    fn secret_walk_skips_absent_optional_leaf() {
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "greeting": "hi" }); // no maybe_key
        block_on(secret_walk::<OptionalCfg>(&ctx, &mut data)).expect("absent optional is fine");
        assert!(data.get("maybe_key").is_none());
    }

    #[test]
    fn secret_walk_skips_null_optional_leaf() {
        // serde serializes `Option::None` as JSON `null` (the key is present,
        // not omitted). The walk must skip a null optional leaf, not error it.
        let ctx = ctx_with_default_secret_store("unused", "unused");
        let mut data = serde_json::json!({ "maybe_key": null });
        block_on(secret_walk::<OptionalCfg>(&ctx, &mut data))
            .expect("null optional is skipped, not treated as non-string");
        assert_eq!(data["maybe_key"], serde_json::json!(null)); // left untouched
    }

    #[test]
    fn secret_walk_missing_required_nested_leaf_errors_with_dotted_path() {
        let ctx = ctx_with_default_secret_store("dd_key", "resolved-dd");
        let mut data = serde_json::json!({ "integrations": { "datadome": {} } });
        let err = block_on(secret_walk::<NestedCfg>(&ctx, &mut data))
            .expect_err("missing required nested leaf");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE); // config_out_of_date -> 503 (error.rs:183)
        assert!(err.to_string().contains("integrations.datadome.server_side_key"));
    }
```

Add the small test helpers near the existing secret-walk test scaffolding (mirror `extractor.rs:2170`'s store construction). `ctx_with_default_secret_store(key, value)` builds an `InMemorySecretStore` mapping `default/{key}` → `value`, wraps it in a `StoreRegistry` with default id `"default"`, inserts the registry into a request's extensions, and returns the `RequestContext`. `ctx_with_default_secret_store_map(&[(k, v), ...])` is the multi-entry variant. `ctx_with_named_secret_store(store_id, key, value)` registers an `InMemorySecretStore` under `store_id` (mapping `{store_id}/{key}` → `value`) in the registry so `ctx.secret_store(store_id)` resolves — used by the `KeyInNamedStore` tests. (`EdgeError::config_out_of_date` → `StatusCode::SERVICE_UNAVAILABLE` per `error.rs:183`, confirmed.)

- [ ] **Step 2: Run (fails)**

Run: `cargo test -p edgezero-core --lib secret_walk_ 2>&1 | tail -25`
Expected: FAIL — nested/array data is not navigated (current walk only reads/writes top-level keys); missing-leaf message lacks the dotted path.

- [ ] **Step 3: Rewrite `secret_walk` as a recursive navigator**

Replace the body of `secret_walk` (`crates/edgezero-core/src/extractor.rs:827-894`) with a path navigator. Keep the signature (`async fn secret_walk<C>(ctx: &RequestContext, data: &mut serde_json::Value) -> Result<(), EdgeError> where C: AppConfigMeta`). New body:

```rust
    for field in C::secret_fields() {
        resolve_secret_field(ctx, data, &field, &field.path, String::new()).await?;
    }
    Ok(())
}

/// Recursively descend `remaining` path segments from `node`, resolving the
/// secret leaf(s). `rendered` is the dotted path so far (with concrete `[n]`
/// indices) for error hints.
fn resolve_secret_field<'a>(
    ctx: &'a RequestContext,
    node: &'a mut serde_json::Value,
    field: &'a SecretField,
    remaining: &'a [SecretPathSegment],
    rendered: String,
) -> std::pin::Pin<Box<dyn core::future::Future<Output = Result<(), EdgeError>> + 'a>> {
    Box::pin(async move {
        match remaining.split_first() {
            // Leaf reached: `node` is the PARENT object; the last Field is the key.
            Some((SecretPathSegment::Field(name), rest)) if rest.is_empty() => {
                resolve_leaf(ctx, node, field, name.as_ref(), &rendered).await
            }
            // Descend into an object key.
            Some((SecretPathSegment::Field(name), rest)) => {
                let next_rendered = join_field(&rendered, name.as_ref());
                match node.get_mut(name.as_ref()) {
                    // Absent optional subtree: key missing OR serialized as null.
                    None | Some(serde_json::Value::Null) if field.optional => Ok(()),
                    Some(child) => {
                        resolve_secret_field(ctx, child, field, rest, next_rendered).await
                    }
                    None => Err(EdgeError::config_out_of_date(
                        format!("missing or non-object value at `{next_rendered}`"),
                        next_rendered,
                    )),
                }
            }
            // Iterate every array element.
            Some((SecretPathSegment::ArrayEach, rest)) => {
                let Some(items) = node.as_array_mut() else {
                    if field.optional {
                        return Ok(());
                    }
                    return Err(EdgeError::config_out_of_date(
                        format!("expected an array at `{rendered}`"),
                        rendered,
                    ));
                };
                for (idx, item) in items.iter_mut().enumerate() {
                    let indexed = format!("{rendered}[{idx}]");
                    resolve_secret_field(ctx, item, field, rest, indexed).await?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    })
}

fn join_field(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Resolve one leaf: `parent` is the innermost containing object; `key` is the
/// secret field name; `store_ref_field` (for `KeyInNamedStore`) is a sibling
/// within `parent`.
async fn resolve_leaf(
    ctx: &RequestContext,
    parent: &mut serde_json::Value,
    field: &SecretField,
    key: &str,
    rendered_parent: &str,
) -> Result<(), EdgeError> {
    if matches!(field.kind, SecretKind::StoreRef) {
        return Ok(()); // store id, not a secret key
    }
    let leaf_path = join_field(rendered_parent, key);

    let Some(parent_obj) = parent.as_object_mut() else {
        if field.optional {
            return Ok(());
        }
        return Err(EdgeError::config_out_of_date(
            format!("expected an object containing `{key}` at `{rendered_parent}`"),
            leaf_path,
        ));
    };

    let key_name = match parent_obj.get(key) {
        Some(serde_json::Value::String(k)) => k.clone(),
        // An optional secret is absent when the key is MISSING *or* serialized
        // as JSON `null`. serde emits `Option::None` as `null` (and `#[secret]`
        // bans `skip_serializing_if`, so the key is never omitted), so both
        // cases must skip — not just the missing-key case.
        None | Some(serde_json::Value::Null) if field.optional => return Ok(()),
        _ => {
            return Err(EdgeError::config_out_of_date(
                format!("missing or non-string value at `{leaf_path}`"),
                leaf_path,
            ))
        }
    };

    let (bound, resolved_store_id) = match field.kind {
        SecretKind::KeyInDefault => {
            let bound = ctx.secret_store_default().ok_or_else(|| {
                EdgeError::config_out_of_date(
                    format!("secret field `{leaf_path}` has kind KeyInDefault but no default secret store is registered"),
                    leaf_path.clone(),
                )
            })?;
            let id = bound.store_name().to_owned();
            (bound, id)
        }
        SecretKind::StoreRef => return Ok(()),
        SecretKind::KeyInNamedStore { store_ref_field } => {
            let store_id_str = parent_obj
                .get(store_ref_field)
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!("missing store_ref `{store_ref_field}` for secret field `{leaf_path}`"),
                        leaf_path.clone(),
                    )
                })?
                .to_owned();
            let bound = ctx.secret_store(&store_id_str).ok_or_else(|| {
                EdgeError::config_out_of_date(
                    format!("blob declared store_ref `{store_id_str}` but [stores.secrets] has no such id"),
                    leaf_path.clone(),
                )
            })?;
            (bound, store_id_str)
        }
    };

    let secret = bound
        .require_str(&key_name)
        .await
        .map_err(|err| map_secret_error(err, &leaf_path, &resolved_store_id, &key_name))?;
    parent_obj.insert(key.to_owned(), serde_json::Value::String(secret));
    Ok(())
}
```

Notes:
- `map_secret_error` (`extractor.rs:896`) takes `field_name: &str` — pass `&leaf_path`; no signature change needed.
- The recursion uses a boxed future (WASM-safe; matches the crate's `?Send` async style) because async fns can't recurse directly.
- `KeyInNamedStore` resolves `store_ref_field` in `parent_obj` — the **innermost** parent, satisfying the sibling scoping rule for nested secrets.

- [ ] **Step 4: Run (passes)**

Run: `cargo test -p edgezero-core --lib secret_walk_ 2>&1 | tail -25`
Expected: PASS (nested object, array-each, absent-optional-skip, missing-nested-dotted-error). Also confirm the pre-existing top-level walk tests (`extractor.rs:2170`, `:2198`) still pass:

Run: `cargo test -p edgezero-core --lib app_config_secret_walk 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings 2>&1 | tail -15`

```bash
git add crates/edgezero-core/src/extractor.rs
git commit -m "feat(secrets): path-navigating secret_walk (nested objects, arrays, optionals)"
```

---

## Task 3: Nested/list-aware `validate_excluding_secrets`

Push time must skip the per-field validator of a nested/array secret leaf, whose failure lives under the parent inside `ValidationErrorsKind::Struct`/`List` — a flat `bag.remove(name)` cannot reach it (§8 [B, IMPORTANT]). Reuse the `first_violating_field` walk pattern (`extractor.rs:926`).

**Files:**
- Modify: `crates/edgezero-core/src/app_config.rs` (`validate_excluding_secrets` at `:204`; add tests)

**Interfaces:**
- Consumes: `C::secret_fields()`, `SecretField.path`, `SecretPathSegment`, validator 0.20 `ValidationErrors`/`ValidationErrorsKind`.
- Produces: nested-aware pruning. Consumed by Task 6 (CLI push over nested config) + Task 7.

- [ ] **Step 1: Write failing nested-pruning tests**

Append to `app_config.rs` `#[cfg(test)] mod tests`. Model on `validate_excluding_secrets_skips_secret_field_rules` (`app_config.rs:1148`) but with a nested struct fixture whose nested secret leaf has a failing validator (e.g. `#[validate(length(min = 100))]` on the key-name string, which is short at push time). Assert `validate_excluding_secrets` returns `Ok(())` (the nested secret's validator was pruned) while a **non-secret** nested failure still surfaces `Err`.

```rust
    #[test]
    fn validate_excluding_secrets_prunes_nested_secret_leaf_validator() {
        use validator::Validate;

        #[derive(Validate)]
        struct Inner {
            #[validate(length(min = 100))]
            server_side_key: String, // holds a short KEY NAME at push time
        }
        #[derive(Validate)]
        struct Outer {
            #[validate(nested)]
            integrations: Inner,
        }
        impl AppConfigMeta for Outer {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("integrations")),
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("server_side_key")),
                    ],
                    optional: false,
                }]
            }
        }

        let cfg = Outer {
            integrations: Inner {
                server_side_key: "dd_key".to_owned(), // 6 chars < 100
            },
        };
        // The only failure is the nested secret leaf's validator -> pruned -> Ok.
        assert!(validate_excluding_secrets(&cfg).is_ok());
    }

    #[test]
    fn validate_excluding_secrets_keeps_nested_non_secret_failures() {
        use validator::Validate;

        #[derive(Validate)]
        struct Inner {
            #[validate(length(min = 100))]
            server_side_key: String,
            #[validate(length(min = 100))]
            note: String, // NON-secret, must still fail
        }
        #[derive(Validate)]
        struct Outer {
            #[validate(nested)]
            integrations: Inner,
        }
        impl AppConfigMeta for Outer {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("integrations")),
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("server_side_key")),
                    ],
                    optional: false,
                }]
            }
        }

        let cfg = Outer {
            integrations: Inner {
                server_side_key: "dd_key".to_owned(),
                note: "short".to_owned(),
            },
        };
        assert!(validate_excluding_secrets(&cfg).is_err()); // `note` still fails
    }

    #[test]
    fn validate_excluding_secrets_prunes_array_secret_leaf_keeps_siblings() {
        use validator::Validate;

        #[derive(Validate)]
        struct Partner {
            #[validate(length(min = 100))]
            api_key: String, // secret leaf (a key NAME at push time)
            #[validate(length(min = 100))]
            label: String, // NON-secret sibling
        }
        #[derive(Validate)]
        struct Outer {
            #[validate(nested)]
            partners: Vec<Partner>,
        }
        impl AppConfigMeta for Outer {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("partners")),
                        SecretPathSegment::ArrayEach,
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("api_key")),
                    ],
                    optional: false,
                }]
            }
        }

        // Every element fails BOTH validators at push time.
        let cfg = Outer {
            partners: vec![
                Partner { api_key: "k0".to_owned(), label: "s".to_owned() },
                Partner { api_key: "k1".to_owned(), label: "s".to_owned() },
            ],
        };
        // `api_key` (secret) pruned from every List element; `label`
        // (non-secret) survives in every element -> overall Err.
        let err = validate_excluding_secrets(&cfg).expect_err("non-secret siblings still fail");
        let rendered = format!("{err:?}");
        assert!(rendered.contains("label"), "non-secret sibling must survive");
        assert!(
            !rendered.contains("api_key"),
            "secret leaf must be pruned from every array element"
        );
    }

    #[test]
    fn validate_excluding_secrets_prunes_array_all_secret_failures_to_ok() {
        use validator::Validate;

        #[derive(Validate)]
        struct Partner {
            #[validate(length(min = 100))]
            api_key: String, // the ONLY validated field, and it's the secret leaf
        }
        #[derive(Validate)]
        struct Outer {
            #[validate(nested)]
            partners: Vec<Partner>,
        }
        impl AppConfigMeta for Outer {
            fn secret_fields() -> Vec<SecretField> {
                vec![SecretField {
                    kind: SecretKind::KeyInDefault,
                    path: vec![
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("partners")),
                        SecretPathSegment::ArrayEach,
                        SecretPathSegment::Field(std::borrow::Cow::Borrowed("api_key")),
                    ],
                    optional: false,
                }]
            }
        }

        // Every element's only failure is the secret leaf -> each List element
        // clears -> the empty List is retained-out -> `partners` removed -> Ok.
        let cfg = Outer {
            partners: vec![
                Partner { api_key: "k0".to_owned() },
                Partner { api_key: "k1".to_owned() },
            ],
        };
        assert!(
            validate_excluding_secrets(&cfg).is_ok(),
            "an array branch whose only failures are secret leaves must fully prune to Ok(())"
        );
    }
```

Note on the array tests: together they prove the `ValidationErrorsKind::List` branch of `prune_secret_leaf` (Step 3) both (a) removes the secret leaf from **each** indexed element while leaving non-secret siblings, and (b) fully collapses to `Ok(())` when every element's only failure is the secret leaf (the `items.retain(..)` + `clear = items.is_empty()` path) — the `#[secret]`-in-`Vec` case the plan commits to from day one.

- [ ] **Step 2: Run (fails)**

Run: `cargo test -p edgezero-core --lib validate_excluding_secrets_prunes_nested 2>&1 | tail -20`
Expected: FAIL — the flat `bag.remove(first_segment)` removes the top-level `integrations` entry entirely (over-pruning) or fails to prune the nested leaf, so the assertion is wrong. (Either way the Task-1 flat impl is incorrect for nesting.)

- [ ] **Step 3: Implement nested-aware pruning**

Replace `validate_excluding_secrets`'s loop (`app_config.rs:204-226`) with a path-aware pruner that navigates `ValidationErrorsKind::Struct`/`List` down each secret field's path, removes the leaf validator, and prunes now-empty containers so a fully-cleared branch disappears (rather than leaving an empty `Struct`/`List` marker that would keep `errors` non-empty). The loop:

```rust
    let Err(mut errors) = result else {
        return Ok(());
    };
    for field in C::secret_fields() {
        if matches!(field.kind, SecretKind::StoreRef) {
            continue; // store_id field; validator stays
        }
        prune_secret_leaf(&mut errors, &field.path);
    }
    if errors.errors().is_empty() {
        return Ok(());
    }
    Err(errors)
}
```

The pruner peeks the segment after each `Field` so a `Field` immediately followed by `ArrayEach` is handled as one step — validator nests a `List` under the array field's key, not as a bare top-level kind. (Mirrors the navigation in `first_violating_field` at `extractor.rs:926`.) Add `use validator::ValidationErrorsKind;` locally.

```rust
fn prune_secret_leaf(errors: &mut validator::ValidationErrors, path: &[SecretPathSegment]) {
    use validator::ValidationErrorsKind;

    let Some((head, rest)) = path.split_first() else { return; };
    let SecretPathSegment::Field(name) = head else {
        // ArrayEach only appears immediately after a Field (a root is always a
        // struct), so it is consumed by the peek below, never as a head.
        return;
    };

    // Leaf.
    if rest.is_empty() {
        errors.errors_mut().remove(name.as_ref());
        return;
    }

    // Does the next segment iterate an array? If so consume it and target a List.
    let (kind_is_array, tail) = match rest.split_first() {
        Some((SecretPathSegment::ArrayEach, tail)) => (true, tail),
        _ => (false, rest),
    };

    let mut clear = false;
    match errors.errors_mut().get_mut(name.as_ref()) {
        Some(ValidationErrorsKind::Struct(inner)) if !kind_is_array => {
            prune_secret_leaf(inner, tail);
            clear = inner.errors().is_empty();
        }
        Some(ValidationErrorsKind::List(items)) if kind_is_array => {
            for inner in items.values_mut() {
                prune_secret_leaf(inner, tail);
            }
            items.retain(|_, inner| !inner.errors().is_empty());
            clear = items.is_empty();
        }
        _ => {}
    }
    if clear {
        errors.errors_mut().remove(name.as_ref());
    }
}
```

- [ ] **Step 4: Run (passes)**

Run: `cargo test -p edgezero-core --lib validate_excluding_secrets 2>&1 | tail -20`
Expected: PASS — both new tests plus the four pre-existing `validate_excluding_secrets_*` tests (`app_config.rs:1132/1148/1173/1195`).

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy -p edgezero-core --all-targets --all-features -- -D warnings 2>&1 | tail -15`

```bash
git add crates/edgezero-core/src/app_config.rs
git commit -m "feat(secrets): nested/list-aware validate_excluding_secrets pruning"
```

---

## Task 4: Derive recursion — `#[app_config(nested)]`, `Option<String>`, path emission

Make the derive actually emit nested/array/optional metadata: register the `app_config` attribute, parse `#[app_config(nested)]`, recurse into child `secret_fields()` prepending `Field` (object) or `Field` + `ArrayEach` (`Vec`), accept `Option<String>` on `#[secret]` (→ `optional: true`), extend the `rename_all` guard to nested-only parents, and assert nested types derive `AppConfig`.

**Files:**
- Modify: `crates/edgezero-macros/src/lib.rs` (`:20`)
- Modify: `crates/edgezero-macros/src/app_config.rs` (parsing, recursion, guards, optional)
- Modify/Create: `crates/edgezero-macros/tests/app_config_derive.rs` + `crates/edgezero-macros/tests/ui/*`

**Interfaces:**
- Consumes: the Task-1 emitter shape (`fn secret_fields()` returning `Vec<SecretField>` with owned paths + `optional`).
- Produces: nested/array/optional metadata for real derived structs. Consumed by Tasks 6 & 7 and app-demo (unchanged top-level app-demo still emits length-1).

- [ ] **Step 1: Register the `app_config` helper attribute**

In `crates/edgezero-macros/src/lib.rs:20`, change:

```rust
#[proc_macro_derive(AppConfig, attributes(secret))]
```

to:

```rust
#[proc_macro_derive(AppConfig, attributes(secret, app_config))]
```

- [ ] **Step 2: Write failing derive/UI tests**

Add happy-path assertions to `crates/edgezero-macros/tests/app_config_derive.rs` (a nested object emits the expected 3-segment path; a `Vec` nested field emits `Field`+`ArrayEach`; `Option<String>` sets `optional: true`). Example:

```rust
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct DataDome {
        #[secret]
        server_side_key: String,
    }
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Integrations {
        #[app_config(nested)]
        #[validate(nested)]
        datadome: DataDome,
    }
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Partner {
        #[secret]
        api_key: String,
        #[secret]
        maybe: Option<String>,
    }
    #[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Settings {
        #[app_config(nested)]
        #[validate(nested)]
        integrations: Integrations,
        #[app_config(nested)]
        #[validate(nested)]
        partners: Vec<Partner>,
    }

    #[test]
    fn nested_and_array_paths_are_emitted() {
        use edgezero_core::app_config::{AppConfigMeta as _, SecretKind};

        let mut paths: Vec<(String, SecretKind, bool)> = Settings::secret_fields()
            .into_iter()
            .map(|f| (f.dotted_path(), f.kind, f.optional))
            .collect();
        paths.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            paths,
            vec![
                ("integrations.datadome.server_side_key".to_owned(), SecretKind::KeyInDefault, false),
                ("partners[*].api_key".to_owned(), SecretKind::KeyInDefault, false),
                ("partners[*].maybe".to_owned(), SecretKind::KeyInDefault, true),
            ],
        );
    }
```

Add UI compile-fail fixtures under `crates/edgezero-macros/tests/ui/` and register them in `crates/edgezero-macros/tests/app_config_derive.rs`'s `trybuild_compile_fail_fixtures` test (`:144-159`). New fixtures (each with a `.stderr`):
  - `app_config_nested_on_non_appconfig.rs` — `#[app_config(nested)]` on a field whose type does not derive `AppConfig` → clear `AppConfigRoot`/`AppConfigMeta` trait-bound error.
  - `app_config_unknown_option.rs` — `#[app_config(bogus)]` (or a `nested` typo) errors instead of being silently ignored (proves `nested_optin` returns `Err`).
  - `secret_on_option_non_string.rs` — `#[secret]` on `Option<u32>` still errors.
  - `secret_store_ref_optional.rs` — `#[secret(store_ref)]` on `Option<String>` errors (a store id is structural; optional is disallowed — Step 6).
  - `nested_secret_serde_rename.rs` — `#[serde(rename)]` on a `#[secret]` leaf inside a nested struct still errors (guard self-enforced per struct).
  - `nested_field_serde_rename.rs` — `#[serde(rename = "...")] #[app_config(nested)] child: Child` errors (the nested *parent* field carries `rename`, which would desync its `Field(field_name)` segment — Step 4 guard).
  - `nested_parent_rename_all.rs` — a parent with only `#[app_config(nested)]` children (no direct `#[secret]`) carrying `#[serde(rename_all="kebab-case")]` errors (Step 7 guard).

Naming caution: the existing harness globs `compile_fail("tests/ui/secret_*.rs")` (`app_config_derive.rs:147`). Do **not** prefix new fixtures with `secret_` unless they are compile-fail; `secret_on_option_non_string.rs` is compile-fail so the glob covers it (don't double-register). The `app_config_*` and `nested_*` names must be listed explicitly.

Note: `app_config_derive.rs` runs `trybuild` only in that single `#[test]`; also add an `Option<String>` **pass** assertion (that it compiles + sets `optional: true`) inside the happy-path `mod tests` above — not as a UI fixture.

- [ ] **Step 3: Run (fails)**

Run: `cargo test -p edgezero-macros --test app_config_derive 2>&1 | tail -30`
Expected: FAIL — `#[app_config(nested)]` is not parsed (unknown attribute or ignored); `Option<String>` rejected by `is_scalar_string_type`; nested paths not emitted.

- [ ] **Step 4: Parse `#[app_config(nested)]` and classify fields**

In `crates/edgezero-macros/src/app_config.rs`, add a helper that detects the opt-in and extracts the recursion child type. A field is a **nested recursion** field iff it carries `#[app_config(nested)]`. Determine object-vs-array from the field type: a `Vec<T>`/`[T]` → array (child `T`, emit `Field(field)` + `ArrayEach`); anything else → object (child = the field type, emit `Field(field)`).

```rust
/// Whether `field` carries `#[app_config(nested)]`. Returns `Err` (not
/// `false`) on a malformed `#[app_config(...)]` such as `#[app_config(bogus)]`
/// or `#[app_config(nseted)]`, so a typo is a hard compile error rather than a
/// silently-ignored non-recursion (which would drop the child's secrets).
fn nested_optin(field: &Field) -> syn::Result<bool> {
    let mut found = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("app_config") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("nested") {
                found = true;
                Ok(())
            } else {
                Err(meta.error("`#[app_config(...)]` only accepts `nested`"))
            }
        })?;
    }
    Ok(found)
}

/// The child element type to recurse into and whether it is an array element.
/// `Vec<T>` / `[T]` -> (T, true); otherwise (field_ty, false).
fn nested_child_type(ty: &Type) -> (&Type, bool) {
    if let Type::Path(tp) = ty {
        if let Some(last) = tp.path.segments.last() {
            if last.ident == "Vec" {
                if let syn::PathArguments::AngleBracketed(ab) = &last.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = ab.args.first() {
                        return (inner, true);
                    }
                }
            }
        }
    }
    if let Type::Slice(s) = ty {
        return (&s.elem, true);
    }
    (ty, false)
}
```

Modify the main field loop in `expand` (`app_config.rs:62-66`) so that for each field it either (a) records a direct `#[secret]` annotation (as today) or (b) records a nested-recursion descriptor `{ field_name, child_ty, is_array }` when `nested_optin(field)?` returns `true`. Propagate the `syn::Result` with `?` so a malformed `#[app_config(...)]` becomes a compile error. A field may not be both `#[secret]` and `#[app_config(nested)]` (error if so).

**Guard the nested parent field's serde attrs.** The emitter writes `Field(Cow::Borrowed(field_name))` using the Rust field name verbatim, so a `#[serde(rename = "...")]` (or `flatten`/`skip*`) on the `#[app_config(nested)]` field itself would desync the emitted path segment from the serialized key — the exact hazard the spec forbids "anywhere on a secret path" (spec §4.3 point 3, line 282). The existing `enforce_no_disallowed_serde_attrs(field)?` (`app_config.rs:363`) already bans `rename`/`flatten`/`skip`/`skip_deserializing`/`skip_serializing`/`skip_serializing_if`. Call it on every nested-recursion field (it currently runs only on `#[secret]` fields via `scan_field`). Each struct on the path self-enforces this for its own fields, so `rename` at any level along the path is rejected by whichever struct declares that field.

- [ ] **Step 5: Emit recursion into `secret_fields()`**

Extend the emitter (Task 1's `entries`) to also emit, for each nested descriptor, a loop that prepends segments onto the child's `secret_fields()`. Change the `fn secret_fields()` body emission to build a `Vec` imperatively:

```rust
    // direct #[secret] entries (owned length-1 paths, optional from Option<String>)
    let direct_entries = /* Task 1 entries, but `optional: #is_option` per field */;

    // nested recursion pushes
    let nested_pushes = nested_descriptors.iter().map(|d| {
        let field_lit = d.field_name.to_string();
        let child_ty = &d.child_ty;
        if d.is_array {
            quote! {
                for mut __f in <#child_ty as ::edgezero_core::app_config::AppConfigMeta>::secret_fields() {
                    let mut __p = ::std::vec![
                        ::edgezero_core::app_config::SecretPathSegment::Field(::std::borrow::Cow::Borrowed(#field_lit)),
                        ::edgezero_core::app_config::SecretPathSegment::ArrayEach,
                    ];
                    __p.append(&mut __f.path);
                    __f.path = __p;
                    __out.push(__f);
                }
            }
        } else {
            quote! {
                for mut __f in <#child_ty as ::edgezero_core::app_config::AppConfigMeta>::secret_fields() {
                    let mut __p = ::std::vec![
                        ::edgezero_core::app_config::SecretPathSegment::Field(::std::borrow::Cow::Borrowed(#field_lit)),
                    ];
                    __p.append(&mut __f.path);
                    __f.path = __p;
                    __out.push(__f);
                }
            }
        }
    });

    let secret_fields_body = quote! {
        let mut __out: ::std::vec::Vec<::edgezero_core::app_config::SecretField> =
            ::std::vec![#(#direct_entries),*];
        #(#nested_pushes)*
        __out
    };
```

And the impl:

```rust
        impl #impl_generics ::edgezero_core::app_config::AppConfigMeta
            for #struct_ident #type_generics #where_clause
        {
            fn secret_fields() -> ::std::vec::Vec<::edgezero_core::app_config::SecretField> {
                #secret_fields_body
            }
        }
```

Additionally, emit an explicit **`AppConfigRoot`** assertion per nested child (spec §4.3 / B-2: the sub-struct must derive `AppConfig`, tracked via the `AppConfigRoot` marker — not merely impl `AppConfigMeta`, which a hand-rolled impl could satisfy without going through the derive). Calling `<#child_ty as AppConfigMeta>::secret_fields()` alone would accept a hand-written `AppConfigMeta` impl; the `AppConfigRoot` bound closes that gap and gives a clear error span:

```rust
        const _: () = {
            fn __assert_app_config_root<T: ::edgezero_core::app_config::AppConfigRoot>() {}
            fn __assert_nested_children() {
                #( __assert_app_config_root::<#nested_child_tys>(); )*
            }
        };
```

where `#nested_child_tys` is the list of the recursion child types (the object field type, or the `Vec`/slice element type). A nested field whose type does not derive `AppConfig` fails with "the trait bound `Child: AppConfigRoot` is not satisfied" — the `app_config_nested_on_non_appconfig.rs` UI fixture pins this message.

- [ ] **Step 6: Relax scalar rule to accept `Option<String>`; set `optional`**

Change `is_scalar_string_type`/`enforce_scalar_string_type` (`app_config.rs:265-284`) to also accept `Option<String>`, and have `scan_field` (`app_config.rs:195-219`) report whether the secret type was optional so the emitter sets `optional`. Add:

```rust
/// `Option<String>` -> Some(true); `String` -> Some(false); else None.
fn secret_string_optionality(ty: &Type) -> Option<bool> {
    if is_scalar_string_type(ty) {
        return Some(false);
    }
    if let Type::Path(tp) = ty {
        if let Some(last) = tp.path.segments.last() {
            if last.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(ab) = &last.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = ab.args.first() {
                        if is_scalar_string_type(inner) {
                            return Some(true);
                        }
                    }
                }
            }
        }
    }
    None
}
```

Replace `enforce_scalar_string_type(field)?;` (`app_config.rs:215`, called from `scan_field` after `parse_secret_kind` yields `kind`) with the optionality computation **plus a `StoreRef`-cannot-be-optional guard**:

```rust
    let optional = secret_string_optionality(&field.ty).ok_or_else(|| {
        syn::Error::new_spanned(
            &field.ty,
            "`#[secret]` may only annotate `String` or `Option<String>`",
        )
    })?;
    // A `#[secret(store_ref)]` value is a store id — structural, always
    // present. `Option<String>` there is undefined (an absent store cannot
    // resolve its dependent `KeyInNamedStore` sibling), so reject it. Optional
    // is allowed only on the secret-VALUE kinds (KeyInDefault / KeyInNamedStore).
    if optional && matches!(kind, SecretAnnotation::StoreRef) {
        return Err(syn::Error::new_spanned(
            &field.ty,
            "`#[secret(store_ref)]` may not be `Option<String>` — a store id is structural and must always be present",
        ));
    }
```

and thread `optional` into `FieldAnnotation` (add a `bool` field), then into the direct-entry emission (`optional: #optional_lit`). Keep rejecting `Vec<String>`, `Cow<'_, str>`, non-string scalars (they yield `None`). Note the runtime walk already early-returns for `StoreRef` regardless of `optional`; this compile-time guard removes the ambiguity at the source so CLI validation and the walk never see an optional store id.

- [ ] **Step 7: Extend the `rename_all` guard to nested-only parents**

The guard fires today only when `!annotations.is_empty()` (`app_config.rs:75-77`, direct `#[secret]` fields present). A parent whose secrets are all in `#[app_config(nested)]` children has no direct annotations but its own `rename_all` would still desync the emitted `Field(parent_field)` segment. Change the gate to also fire when any nested descriptor exists:

```rust
    if !annotations.is_empty() || !nested_descriptors.is_empty() {
        enforce_no_container_rename_all(&input.attrs)?;
    }
```

- [ ] **Step 8: Run (passes)**

Run: `cargo test -p edgezero-macros --test app_config_derive 2>&1 | tail -30`
Expected: PASS — happy-path nested/array/optional emission + all UI compile-fail fixtures match their `.stderr`. Regenerate `.stderr` with `TRYBUILD=overwrite cargo test -p edgezero-macros --test app_config_derive` if messages differ, then inspect the diffs for correctness before committing.

- [ ] **Step 9: Lint + commit**

Run: `cargo clippy -p edgezero-macros --all-targets --all-features -- -D warnings 2>&1 | tail -15`

```bash
git add crates/edgezero-macros/src/lib.rs crates/edgezero-macros/src/app_config.rs \
        crates/edgezero-macros/tests/app_config_derive.rs crates/edgezero-macros/tests/ui/
git commit -m "feat(macros): #[app_config(nested)] recursion, Option<String> secrets, path emission"
```

---

## Task 5: Invert the nested-AppConfig CI guard

The `check_no_nested_app_config` binary currently rejects **any** `AppConfig` struct used inside another (`.github/workflows/test.yml:58` runs it). Invert it: nesting is allowed **iff** the containing field carries `#[app_config(nested)]`. Add the tests it lacks today.

**Files:**
- Modify: `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`

**Interfaces:**
- Consumes: `syn` field attributes (the binary already parses structs with `syn::visit`).
- Produces: a guard that permits opted-in nesting; still fails on un-opted-in nesting.

- [ ] **Step 1: Write failing guard tests**

Add a `#[cfg(test)] mod tests` to `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`. The binary is behind `#![cfg(feature = "nested-app-config-check")]`, so tests run only with that feature. Test the pure helpers by parsing source snippets with `syn::parse_file` and running the collector + visitor:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn violations_in(src: &str) -> usize {
        let file = syn::parse_file(src).expect("parse");
        let mut collector = AppConfigStructCollector::default();
        syn::visit::visit_file(&mut collector, &file);
        let mut visitor = NestedAppConfigVisitor::new(&collector.app_config_structs, std::path::Path::new("t.rs"));
        syn::visit::visit_file(&mut visitor, &file);
        visitor.violations
    }

    const NESTED_WITHOUT_OPT_IN: &str = r#"
        #[derive(edgezero_core::AppConfig)] struct Inner { #[secret] k: String }
        #[derive(edgezero_core::AppConfig)] struct Outer { inner: Inner }
    "#;

    const NESTED_WITH_OPT_IN: &str = r#"
        #[derive(edgezero_core::AppConfig)] struct Inner { #[secret] k: String }
        #[derive(edgezero_core::AppConfig)] struct Outer { #[app_config(nested)] inner: Inner }
    "#;

    const NESTED_VEC_WITH_OPT_IN: &str = r#"
        #[derive(edgezero_core::AppConfig)] struct Inner { #[secret] k: String }
        #[derive(edgezero_core::AppConfig)] struct Outer { #[app_config(nested)] inner: Vec<Inner> }
    "#;

    #[test]
    fn flags_nesting_without_opt_in() {
        assert_eq!(violations_in(NESTED_WITHOUT_OPT_IN), 1);
    }

    #[test]
    fn allows_nesting_with_opt_in() {
        assert_eq!(violations_in(NESTED_WITH_OPT_IN), 0);
    }

    #[test]
    fn allows_vec_nesting_with_opt_in() {
        assert_eq!(violations_in(NESTED_VEC_WITH_OPT_IN), 0);
    }
}
```

(If the collector/visitor don't currently expose `default()`/`new()`/public fields for construction in tests, add minimal `#[derive(Default)]` / a `new` constructor / `pub(crate)` visibility as part of this task — they're in the same binary crate.)

- [ ] **Step 2: Run (fails)**

Run: `cargo test -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config 2>&1 | tail -20`
Expected: FAIL — `allows_nesting_with_opt_in` sees 1 violation (the guard flags all nesting today).

- [ ] **Step 3: Teach the visitor to honor `#[app_config(nested)]`**

In `NestedAppConfigVisitor::visit_item_struct` (`check_no_nested_app_config.rs:156-181`), before reporting a violation for a field whose type contains an `AppConfig` struct, skip it if the field carries `#[app_config(nested)]`. Add a helper mirroring the macro's detection and guard the report:

```rust
// Returns true only for a well-formed `#[app_config(nested)]`. A malformed
// `#[app_config(...)]` returns false -> the field is treated as NOT opted in,
// so the guard still FLAGS the nesting (loud CI failure) rather than silently
// waving it through. This is safe here (unlike the derive's `nested_optin`,
// which must hard-error): the guard runs only over already-compiling code, and
// the derive's strict `nested_optin` (Task 4) has already rejected any
// malformed `#[app_config(...)]` before this binary ever runs.
fn field_has_nested_optin(field: &syn::Field) -> bool {
    field.attrs.iter().any(|attr| {
        attr.path().is_ident("app_config")
            && attr
                .parse_nested_meta(|meta| {
                    if meta.path.is_ident("nested") {
                        Ok(())
                    } else {
                        Err(meta.error("unknown app_config option"))
                    }
                })
                .is_ok()
    })
}
```

and in the field loop, wrap the existing `if let Some(inner_name) = type_contains_app_config_struct(...) { self.report(...) }` so it becomes:

```rust
            if let Some(inner_name) = type_contains_app_config_struct(&field.ty, self.app_config_structs) {
                if field_has_nested_optin(field) {
                    continue; // opted in via #[app_config(nested)] — allowed
                }
                self.report(self.source_path, field, outer_ident, &field_name, &inner_name);
            }
```

(Adjust the `report` call to match the current signature at `check_no_nested_app_config.rs:138-149`.)

- [ ] **Step 4: Run (passes) + run the guard over the real trees**

Run: `cargo test -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config 2>&1 | tail -20`
Expected: PASS.

Run: `cargo run -q -p edgezero-cli --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates 2>&1 | tail -5`
Expected: `check_no_nested_app_config: OK` (app-demo has no opted-in nesting yet; still clean).

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config -- -D warnings 2>&1 | tail -15`

```bash
git add crates/edgezero-cli/src/bin/check_no_nested_app_config.rs
git commit -m "ci(secrets): allow nested AppConfig when field opts in via #[app_config(nested)]"
```

---

## Task 6: Path-aware CLI reflection (validate / push / diff over nested config)

Task 1 made the CLI consumers compile against the new shape but only navigate top-level keys. Now make `run_adapter_typed_checks` and `typed_secret_checks` navigate the raw TOML by path (Field/ArrayEach), emitting one `TypedSecretEntry` per array element with a runtime dotted label, and resolving `store_ref` siblings within the innermost parent.

**Files:**
- Modify: `crates/edgezero-cli/src/config.rs` (`run_adapter_typed_checks` at `:1295`, `typed_secret_checks` at `:1339`; add a TOML path navigator + tests)

**Interfaces:**
- Consumes: `SecretField.path`/`optional`, `toml::Value`, `TypedSecretEntry::new(store_id, String, key_value)` (Task 1).
- Produces: path-aware validate/push/diff. Consumed by acceptance (nested config validates/pushes).

- [ ] **Step 1: Write failing CLI navigation tests**

Add tests to `crates/edgezero-cli/src/config.rs` `#[cfg(test)] mod tests`, driven through the **public** `run_config_validate_typed::<C>` entry point (which calls both `typed_secret_checks` and `run_adapter_typed_checks`). `ValidationContext` has private fields and a `ManifestLoader` that is impractical to build by hand, so mirror the existing harness: write a manifest + `demo-app.toml` to a tempdir with `setup_project(manifest, app_config)` (`config.rs:1662`, returns `(TempDir, manifest_path, app_config_path)`) and pass `args_for(&manifest_path)` (`config.rs:1671`). The config type is a **real** nested `#[derive(AppConfig)]` (Task 4 is complete by Task 6), which also proves derive→CLI integration.

```rust
    // Real nested derive: integrations.datadome.server_side_key (KeyInDefault),
    // partners[*].api_key (KeyInDefault).
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct DataDome {
        #[secret]
        server_side_key: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Integrations {
        #[app_config(nested)]
        #[validate(nested)]
        datadome: DataDome,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Partner {
        #[secret]
        api_key: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct NestedCliConfig {
        #[app_config(nested)]
        #[validate(nested)]
        integrations: Integrations,
        #[app_config(nested)]
        #[validate(nested)]
        partners: Vec<Partner>,
    }

    const NESTED_MANIFEST: &str = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default"]
"#;

    #[test]
    fn validate_typed_accepts_well_formed_nested_and_array_secrets() {
        let app_config = r#"
[integrations.datadome]
server_side_key = "dd_key"

[[partners]]
api_key = "p0"

[[partners]]
api_key = "p1"
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect("well-formed nested + array secret config validates");
    }

    #[test]
    fn validate_typed_reports_dotted_path_for_empty_array_secret() {
        // partners[1].api_key is empty -> typed_secret_checks must reject it and
        // name the INDEXED dotted path.
        let app_config = r#"
[integrations.datadome]
server_side_key = "dd_key"

[[partners]]
api_key = "p0"

[[partners]]
api_key = ""
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        let err = run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect_err("empty array secret must be rejected");
        assert!(
            err.contains("partners[1].api_key"),
            "error names the indexed dotted path: {err}"
        );
    }

    #[test]
    fn validate_typed_reports_dotted_path_for_missing_nested_leaf() {
        // integrations.datadome table present but server_side_key missing.
        // (Note: serde deny_unknown_fields + required String means this also
        //  fails deserialization; assert the dotted path appears either way.)
        let app_config = r#"
[integrations.datadome]

[[partners]]
api_key = "p0"
"#;
        let (_dir, manifest_path, _) = setup_project(NESTED_MANIFEST, app_config);
        let err = run_config_validate_typed::<NestedCliConfig>(&args_for(&manifest_path))
            .expect_err("missing nested leaf must be rejected");
        assert!(
            err.contains("server_side_key"),
            "error names the missing nested leaf: {err}"
        );
    }
```

Nested `KeyInNamedStore` CLI case (proves the store_ref sibling is resolved within the innermost parent table, and the named store must be declared in `[stores.secrets].ids`):

```rust
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct Vaulted {
        #[secret(store_ref = "vault")]
        token: String,
        #[secret(store_ref)]
        vault: String,
    }
    #[derive(Debug, Deserialize, Serialize, Validate, edgezero_core::AppConfig)]
    #[serde(deny_unknown_fields)]
    struct NamedStoreCliConfig {
        #[app_config(nested)]
        #[validate(nested)]
        vaulted: Vaulted,
    }

    #[test]
    fn validate_typed_accepts_nested_named_store_with_sibling() {
        let manifest = r#"
[app]
name = "demo-app"

[adapters.axum.adapter]
crate = "crates/demo-axum"
[adapters.axum.commands]
build = "echo"
deploy = "echo"
serve = "echo"

[stores.config]
ids = ["app_config"]

[stores.secrets]
ids = ["default", "named"]
"#;
        let app_config = r#"
[vaulted]
token = "tok_key"
vault = "named"
"#;
        let (_dir, manifest_path, _) = setup_project(manifest, app_config);
        run_config_validate_typed::<NamedStoreCliConfig>(&args_for(&manifest_path))
            .expect("nested named-store secret with a declared store validates");
    }
```

- [ ] **Step 2: Factor a TOML path leaf-collector**

Add a helper that, given the raw `&toml::Value` table and a `&SecretField`, yields each resolved leaf as `(label: String, value: &str, store_ref_value: Option<&str>)`, where `label` uses concrete `[n]` indices and `store_ref_value` is resolved from the leaf's innermost parent table (for `KeyInNamedStore`). Absent optional leaves yield nothing; missing required leaves yield an error carrying the dotted label.

```rust
struct ResolvedTomlLeaf<'a> {
    label: String,
    value: &'a str,
    store_ref_value: Option<&'a str>,
}

fn collect_secret_leaves<'a>(
    root: &'a toml::Value,
    field: &SecretField,
) -> Result<Vec<ResolvedTomlLeaf<'a>>, String> {
    fn walk<'a>(
        node: &'a toml::Value,
        field: &SecretField,
        remaining: &[SecretPathSegment],
        rendered: String,
        out: &mut Vec<ResolvedTomlLeaf<'a>>,
    ) -> Result<(), String> {
        match remaining.split_first() {
            Some((SecretPathSegment::Field(name), rest)) if rest.is_empty() => {
                let parent = node.as_table().ok_or_else(|| {
                    format!("expected a table containing `{name}` at `{rendered}`")
                })?;
                let leaf_label = if rendered.is_empty() {
                    name.to_string()
                } else {
                    format!("{rendered}.{name}")
                };
                match parent.get(name.as_ref()).and_then(toml::Value::as_str) {
                    Some(value) => {
                        let store_ref_value = match field.kind {
                            SecretKind::KeyInNamedStore { store_ref_field } => {
                                parent.get(store_ref_field).and_then(toml::Value::as_str)
                            }
                            _ => None,
                        };
                        out.push(ResolvedTomlLeaf { label: leaf_label, value, store_ref_value });
                        Ok(())
                    }
                    None if field.optional && parent.get(name.as_ref()).is_none() => Ok(()),
                    None => Err(format!("`#[secret]` field `{leaf_label}` is missing or not a string")),
                }
            }
            Some((SecretPathSegment::Field(name), rest)) => {
                let table = node.as_table().ok_or_else(|| format!("expected a table at `{rendered}`"))?;
                let next_rendered = if rendered.is_empty() { name.to_string() } else { format!("{rendered}.{name}") };
                match table.get(name.as_ref()) {
                    Some(child) => walk(child, field, rest, next_rendered, out),
                    None if field.optional => Ok(()),
                    None => Err(format!("missing `{next_rendered}`")),
                }
            }
            Some((SecretPathSegment::ArrayEach, rest)) => {
                let arr = node.as_array().ok_or_else(|| format!("expected an array at `{rendered}`"))?;
                for (idx, item) in arr.iter().enumerate() {
                    walk(item, field, rest, format!("{rendered}[{idx}]"), out)?;
                }
                Ok(())
            }
            None => Ok(()),
        }
    }
    let mut out = Vec::new();
    walk(root, field, &field.path, String::new(), &mut out)?;
    Ok(out)
}
```

- [ ] **Step 3: Rewrite the two consumers to use the collector**

Replace the flat lookups in `run_adapter_typed_checks` (`config.rs:1295-1333`) and `typed_secret_checks` (`config.rs:1339-1412`):

- `run_adapter_typed_checks`: for each `field in C::secret_fields()`, for each leaf in `collect_secret_leaves(raw_value, &field)?`, build entries. For `KeyInDefault`, use `default_store_id`; for `KeyInNamedStore`, use `leaf.store_ref_value` (error if `None`); push `TypedSecretEntry::new(store_id, leaf.label, leaf.value)`. `StoreRef` still produces no entry.
- `typed_secret_checks`: for each `field`, for each leaf, apply the existing empty-string / `[stores.secrets]`-declared / store-ref-in-ids checks, but keyed on `leaf.label`/`leaf.value`. For `StoreRef`, the leaf value must be in `[stores.secrets].ids` (as today).

Note the collector takes `&toml::Value` (the whole raw config) — `run_adapter_typed_checks`/`typed_secret_checks` currently start from `raw_table = ctx.raw_config.as_table()`; pass `&ctx.raw_config` to the collector instead (it does the `as_table` internally).

- [ ] **Step 4: Run (passes) + full CLI tests**

Run: `cargo test -p edgezero-cli --lib config 2>&1 | tail -25`
Expected: PASS — new nested tests + all pre-existing config tests (top-level fixtures still length-1 paths).

- [ ] **Step 5: Lint + commit**

Run: `cargo clippy -p edgezero-cli --all-targets --all-features -- -D warnings 2>&1 | tail -15`

```bash
git add crates/edgezero-cli/src/config.rs
git commit -m "feat(cli): path-aware secret reflection in config validate/push/diff"
```

---

## Task 7: End-to-end nested-secret extractor test + `KeyInNamedStore` fixture + docs

Prove the whole chain with a real `#[derive(AppConfig)]` config that has a 2-level nested secret and a nested `KeyInNamedStore` (sibling-in-parent), resolved through an `InMemorySecretStore` via the `AppConfig<C>` extractor. Then document the feature.

**Files:**
- Modify: `crates/edgezero-core/src/extractor.rs` (E2E test in `#[cfg(test)] mod tests`) — or a new `crates/edgezero-macros/tests/` integration test if a real `#[derive(AppConfig)]` is easier there (the derive lives in macros; a genuine nested derived struct needs `edgezero_core` as an external crate, so prefer `crates/edgezero-macros/tests/nested_secrets_e2e.rs`).
- Modify: `docs/guide/configuration.md`

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces: acceptance evidence; docs.

- [ ] **Step 1: Write the failing E2E test**

Create `crates/edgezero-macros/tests/nested_secrets_e2e.rs`. Define a real nested config with `#[derive(AppConfig, Deserialize, Validate)]`, including one `KeyInDefault` nested leaf and one `KeyInNamedStore` nested leaf whose `store_ref` sibling lives in the same inner struct. Build a `serde_json::Value` blob holding key NAMES, run `secret_walk` (via the public `AppConfig<C>` extraction path or by calling the crate's extraction entry point), and assert the resolved values.

Prefer driving it through the same public surface app-demo's `config_flow.rs` uses (`InMemorySecretStore::new([...])`, build a `BlobEnvelope`, extract via the `AppConfig<C>` extractor with the store registry in `ctx`). Model on `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs:210-231`. Assert:
  - nested `KeyInDefault` leaf resolves from the default store;
  - nested `KeyInNamedStore` leaf resolves from the named store identified by its sibling;
  - a nested config with an array of secrets resolves each element.

```rust
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct DataDome {
    #[secret]
    server_side_key: String,
}
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct Vaulted {
    #[secret(store_ref = "vault")]
    token: String,
    #[secret(store_ref)]
    vault: String,
}
#[derive(serde::Deserialize, validator::Validate, edgezero_core::AppConfig)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[app_config(nested)]
    #[validate(nested)]
    datadome: DataDome,
    #[app_config(nested)]
    #[validate(nested)]
    vaulted: Vaulted,
}
// ... build data = { "datadome": { "server_side_key": "dd_key" },
//                    "vaulted": { "token": "tok_key", "vault": "named" } }
// ... default store: default/dd_key -> "DD"; named store "named": named/tok_key -> "TOK"
// ... run extraction; assert cfg.datadome.server_side_key == "DD" and cfg.vaulted.token == "TOK".
```

- [ ] **Step 2: Run (fails, then passes)**

Run: `cargo test -p edgezero-macros --test nested_secrets_e2e 2>&1 | tail -25`
Expected: FAIL first (fixture/wiring), then PASS once assertions match resolved values. (If any Task 1–6 gap surfaces here, fix in the owning task's file and re-run.)

- [ ] **Step 3: Docs**

Append to `docs/guide/configuration.md` a "Nested and array secrets" section documenting: the `#[app_config(nested)]` opt-in (mirrors `#[validate(nested)]`; the nested type must itself derive `AppConfig`), `#[secret]` on `Option<String>` (absent → skipped at runtime), object nesting and `Vec<_>` arrays (`partners[*].api_key`), the `store_ref` sibling scoping rule (resolved within the innermost containing object), and the dotted-path error format (`integrations.datadome.server_side_key`, `partners[3].api_key`). Include a worked `Settings`/`Integrations`/`Partner` example.

- [ ] **Step 4: Full workspace verification (all CI gates)**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
cargo run -q -p edgezero-cli --bin check_no_nested_app_config --features nested-app-config-check -- examples/app-demo crates/edgezero-cli/src/templates
(cd examples/app-demo && cargo test)
```
Expected: all green; app-demo top-level `#[secret]` still resolves; the guard prints `OK`.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-macros/tests/nested_secrets_e2e.rs docs/guide/configuration.md
git commit -m "test(secrets): end-to-end nested + named-store resolution; docs: nested/array secrets"
```

---

## Acceptance criteria (spec §5)

1. `cargo fmt` / clippy clean across `edgezero-core`, `edgezero-macros`, all adapters, `edgezero-cli`.
2. New unit + UI + integration tests (Tasks 1–7) pass; the six pre-existing `validate_excluding_secrets_*` / `app_config_secret_walk_*` tests still pass.
3. `app-demo` still builds and serves on all four adapters; its top-level `#[secret] api_token` (`KeyInDefault`) and `vault` (`StoreRef`) resolve identically (length-1 paths).
4. `edgezero-cli` `config validate/push/diff` operate correctly over a config with nested + array secrets.
5. The **Nested AppConfig audit** CI step passes and now permits `#[app_config(nested)]` fields.
6. Rustdoc + `docs/guide/configuration.md` updates merged.

## Self-review notes (mapping to spec §4 + §8 blockers)

- §4.2 metadata: owned `SecretField { kind, path: Vec<SecretPathSegment>, optional }` + `dotted_path()` → Task 1. **[B, BLOCKER] owned segments** (not `&'static`) — done. **[B, BLOCKER] `optional` flag** — done.
- §4.3 derive: `#[app_config(nested)]` opt-in, recursion, `Option<String>`, path guards; **[B, HIGH] register `app_config` attr** (Task 4 Step 1); **[B, HIGH] nested-only `rename_all`** (Task 4 Step 7) → Task 4. **B-3 forced to `fn secret_fields()`** — Task 1.
- §4.4 runtime walk: Field/ArrayEach navigator, optional skip, sibling-in-parent, dotted `[n]` errors → Task 2.
- §4.5 CLI: path-aware `run_adapter_typed_checks`/`typed_secret_checks`; `build_config_envelope` unchanged (serializes verbatim); Spin collision keys on value (survives reshape, prints dotted label) → Tasks 1 + 6. **[B, HIGH] owned `TypedSecretEntry.field_name`** → Task 1 Step 8.
- §4.6 back-compat: top-level configs behave identically (length-1 paths); all in-tree consumers flip in the same branch → Task 1.
- **[B, IMPORTANT] nested `validate_excluding_secrets`** (not a flat remove) → Task 3.
- **[B, BLOCKER] inverted CI guard** → Task 5.
- **[B, HIGH] array scope decided: arrays-now** (`ArrayEach` implemented throughout) → Tasks 1–7.
- §4.7 tests: derive UI (nested/optional/rename/non-AppConfig), runtime (nested/named-store/optional/missing), E2E 2-level → Tasks 4, 2, 7. `KeyInNamedStore` needs a purpose-built fixture (app-demo has none) → Task 7.

## Review round 2 — fixes folded in

- **Optional `None` = JSON `null` (blocker):** `resolve_leaf` and the object-descent arm now skip an optional leaf/subtree that is missing *or* `null` (serde emits `None` as `null`; `#[secret]` bans `skip_serializing_if`). Added `secret_walk_skips_null_optional_leaf` (Task 2).
- **`TypedSecretEntry::new` back-compat (blocker):** constructor takes `field_name: impl Into<String>` so the 7 existing `&str`-literal Spin test call sites compile unchanged (Task 1 Step 8).
- **Malformed `#[app_config(...)]` (high):** derive helper is `nested_optin(field) -> syn::Result<bool>` (hard error on unknown option, propagated with `?`); added `app_config_unknown_option.rs` UI fixture. CI-guard helper stays lenient by design (documented: runs only over already-compiling code) (Tasks 4, 5).
- **Array validation pruning untested (high):** added `validate_excluding_secrets_prunes_array_secret_leaf_keeps_siblings` exercising the `ValidationErrorsKind::List` branch (Task 3).
- **`#[secret(store_ref)]` + `Option<String>` (high):** rejected at compile time (a store id is structural); added `secret_store_ref_optional.rs` UI fixture. Optional allowed only on `KeyInDefault`/`KeyInNamedStore` (Task 4 Step 6).
- **Nested-child marker (medium):** emit an explicit `AppConfigRoot` bound assertion per nested child (not just the implicit `AppConfigMeta` call), matching spec §4.3/B-2 (Task 4 Step 5).

## Review round 3 — fixes folded in

- **serde `rename` on nested parent fields:** the spec forbids `#[serde(rename)]` *anywhere* on a secret path. The plan now runs the existing `enforce_no_disallowed_serde_attrs` (bans rename/flatten/skip*) on every `#[app_config(nested)]` field too — a nested parent field with `#[serde(rename)]` would desync its `Field(field_name)` segment. Added `nested_field_serde_rename.rs` UI fixture (Task 4 Step 4).
- **Named-store coverage earlier + broader:** added a nested `KeyInNamedStore` sibling-in-parent runtime test (`secret_walk_resolves_nested_named_store_via_sibling_in_parent`) and a missing-sibling error test to Task 2, plus a nested `KeyInNamedStore` CLI validate test to Task 6 — no longer only end-to-end in Task 7.
- **Array pruning all-secret success:** added `validate_excluding_secrets_prunes_array_all_secret_failures_to_ok`, proving an array branch whose every element's only failure is the secret leaf collapses to `Ok(())` (the `items.retain(..)`/`items.is_empty()` path), complementing the sibling-survives test (Task 3).
- **Task 6 CLI tests made concrete:** replaced the pseudo-code with real tests driven through the public `run_config_validate_typed::<C>` entry point using the existing `setup_project`/`args_for` harness (`config.rs:1662/1671`) and real nested `#[derive(AppConfig)]` fixtures — with concrete TOML, `partners[1].api_key` indexed-label assertion, missing-leaf assertion, and the nested `KeyInNamedStore` case (Task 6 Step 1).
- **Removed the obsolete pruning sketch:** Task 3 Step 3 now shows a single `prune_secret_leaf` (the peek-next-segment form); the earlier draft referencing the undefined `list_children_mut` is deleted.
