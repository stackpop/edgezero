# Blob App Config — Design Spec

**Date:** 2026-06-16
**Status:** v1 — Plan-ready (twenty-three reviewer passes complete; reviewer cleared for plan authoring at round 24)
**Author:** Aram Grigoryan
**Related branches:** `feature/extensible-cli` (current baseline)

## v1 changelog

Initial draft after twenty-four review rounds — one author
self-review, twenty-three reviewer passes against the current
branch's code. Round 24 cleared the spec for plan authoring;
only doc-hygiene items remained (stale commit counts + the
Status header).

**Self-review (round 1):** canonical-form rules (§4.2), Spin
Cloud size/exposure trade-off (§9.4 + Q12), orphan stance
(§10.5), `AppConfig<C>::from_store` cross-store form (§6.2.1),
`__KEY` precedence table (§5.2), per-request caching draft (§6.4

- Q5), TOCTOU note (§8.1.4), concurrent-push restriction
  (§10.6), rollback workflow (§10.7), grep-based acceptance gate
  (§10.2.1).

**Reviewer pass (round 2):** secret-field model first sketched
with strip + extractor-time resolution (§3.3); `AppConfig<C>`
renamed away from the existing `edgezero_core::extractor::Config`
(§6.0); Axum local file shape is a key→envelope map, not the
envelope itself (§9.1); blob model is TYPED-ONLY — the
bundled `edgezero` binary's `config push` / `config diff`
become stub-pointer subcommands that exit with a typed-CLI
pointer (round-8 fix); generated downstream CLIs with `C`
are the real entry point (§3.2.1); `Adapter`
trait gains `read_config_entry` (§9.0); scope and phasing
reconciled — one PR, five commits (round-23 collapse from
the earlier seven-commit plan), atomic landing, no
minimum-cut split (§13); errors use the existing
`ConfigStoreError::{Internal, InvalidKey, Unavailable}`
variants with an explicit mapping table to `EdgeError` (§6.3,
§4.3); missing-key behaviour originally picked
`EdgeError::Internal` (round-18 M-2 reversed this to
`EdgeError::ConfigOutOfDate` — see Q3); extractor module
path corrected to
`edgezero_core::extractor::AppConfig` (§6.1); "legacy" phrasing
dropped from the push flag table (§8.2).

**Reviewer pass (round 3):**

- **Secret model collapsed to marker-only** (§3.3 rewrite). The
  earlier strip + extractor-time resolution design didn't match
  app-demo's existing semantics (the `api_token` field VALUE is
  a secret-store KEY NAME, not the token bytes — non-sensitive
  metadata that belongs in the blob). The blob now carries both
  `#[secret]` and `#[secret(store_ref)]` field values verbatim;
  handlers do explicit secret-store lookups; the attributes are
  validate-time markers only. §3.3.1's three implementation
  options for extractor-time secret resolution collapse to "none
  of the above". §10.2 updated to reflect that app-demo's
  struct is unchanged.
- **`AppConfig<C>` trait bounds spelled out** (§6.2 signature):
  `DeserializeOwned + Validate + Send + 'static`. `AppConfigMeta`
  is NOT a required bound on the extractor (handlers that want
  the metadata can declare it themselves).
- **Runtime validation declared explicit** (§6.2.2). The
  extractor calls `Validate::validate(&cfg)` on every extract;
  failures map to `EdgeError::ConfigOutOfDate`.
- **Adapter read-back trait signature mirrors the writer**
  (§9.0). Takes the same `manifest_root` /
  `adapter_manifest_path` / `component_selector` /
  `ResolvedStoreId` / `AdapterPushContext` the writer takes, so
  Spin can reach `runtime_config_path`, Cloudflare can resolve
  the wrangler binding, etc. Split into
  `read_config_entry` / `read_config_entry_local` to mirror the
  two writers and avoid cross-mode comparisons in
  skip-on-equal.
- **Per-request cache dropped from v1** (§6.4 rewrite, Q5
  resolved). The earlier `OnceLock`-in-`Extensions` design
  didn't fit `FromRequest`'s immutable extension access. v1
  ships without caching; follow-up can revisit once the
  required `FromRequest` API change is its own discussion.
- **`--cleanup-legacy` dropped** (§10.5 rewrite). Adding a
  cleanup flag would require extending the `Adapter` trait
  with list/delete methods that exist nowhere else in the
  spec. Manual cleanup recipes per adapter live in the
  migration guide instead.
- **CLI examples consistently use `<app-cli>` / `app-demo-cli`**
  (§3.2.1, §5.4, §7, §8). The bundled `edgezero` binary is
  explicitly referenced only where the doc says "this command
  is removed from it".
- **`ConfigOutOfDate` gets a concrete contract** (§6.3.1):
  extension to the `EdgeError` response body to add a stable
  `kind` field, `Retry-After: 60` header on the variant, and
  `serde_path_to_error` dep for the `field_path` payload (with
  rationale for the dep).
- **Stale `Config<C>` references swept** (acceptance-gate
  grep script, Q3, the `MaybeConfig<C>` reference in §6.3 →
  `MaybeAppConfig<C>` in Q3).
- **Test plan extended** (§12.5–§12.10): secret retention,
  secret lookup, missing-secret error, runtime validation,
  env-var key override end-to-end, raw-binary removal,
  downstream CLI wiring, Spin Cloud size cap.

**Reviewer pass (round 4) — secret model flipped to
framework-resolved:**

- **§3.3 rewritten to Model A (framework-resolved).** The
  earlier round-3 design landed at "marker-only": handlers
  resolve secrets explicitly with `secret_store.require_str
(&cfg.api_token)`. Round-4 reviewer + project owner
  confirmed the preferred model is "framework-resolved": the
  extractor walks `C::SECRET_FIELDS` after reading the blob
  and BEFORE deserialising into `C`, swapping each
  `#[secret]` field's value from operator-supplied key NAME
  → resolved secret VALUE from the secret store. Handlers
  receive a complete `cfg` with secrets populated. Adds a
  third attribute form `#[secret(store_ref = "field_name")]`
  that picks which secret store via another field's value.
- **§3.2 + §6.1 propagated** to match Model A — secret
  values stay out of the blob; the runtime extractor
  resolves them; handler-side `cfg.api_token` is the
  resolved token, not a key name.
- **§3.3.6 (new) — failure modes.** Secret-store-id
  unknown, secret-key-not-found, and secret-store-unreachable
  map to `EdgeError::ConfigOutOfDate` (first two) and
  `EdgeError::ServiceUnavailable` (third) so dashboards
  distinguish "deploy is incomplete" from "backend is
  flaky".
- **§9.1 Axum reader uses `ConfigStore::get` returning
  `Option<String>`.** Previous draft made the extractor parse
  the Axum file-map shape directly, bypassing the
  provider-neutral trait contract. Fixed: the file holds
  per-key envelope JSON STRINGS, the `AxumConfigStore::get`
  impl returns the matching string, and the extractor sees
  the same `Option<String>` every adapter exposes.
- **§3.3.2 secret existence claim dropped.** The earlier
  draft claimed `<app-cli> config validate --strict` could
  verify the named secret exists; the typed validator
  actually only checks `[stores.secrets]` is declared and
  that store_ref ids point at declared stores. Validate-time
  secret-value probing would need an adapter trait that
  doesn't exist; pushed entirely to request time.
- **§10.2.1 grep gate tightened.** Added Pattern 2
  (`config_store_default`/`config_store(` two-step
  binding-then-`.get(`) and Pattern 3
  (`require_str(&cfg.<field>)` legacy secret resolution
  under Model B/round-3 that Model A retires). Added
  positive check that at least one in-tree handler imports
  `AppConfig` from `edgezero_core::extractor`.
- **§3.2.2 (new) — concrete CLI command-enum split.** The
  bundled binary's `ConfigCmd` becomes validate-only;
  generated CLIs use a new `TypedConfigCmd` enum with
  `Push` + `Diff` + `Validate`. Exported `ConfigPushArgs` /
  `ConfigValidateArgs` / new `ConfigDiffArgs` stay shared.
- **`from_store` bounds match `named`** —
  `DeserializeOwned + AppConfigMeta + Validate + Send +
'static`. `AppConfigMeta` promoted from optional (round-3)
  to required because the secret walk consults
  `SECRET_FIELDS`.
- **§3.2 "one blob per id" reframed** as "one ACTIVE typed
  app-config blob key per environment, within a config
  store id" — reconciles with §5 and §9.1's multi-environment
  blob support.
- **§12.5 missing-secret test expects `ConfigOutOfDate`**
  (was `Unavailable`). Calls out that the extractor wraps
  `SecretError::NotFound` BEFORE it bubbles to the standard
  `Internal` mapping in `secret_store.rs:131`.
- **§12.2 skip-on-equal test says "no WRITE shell-out"** —
  the read still shells out; that's how skip-on-equal works.
- **§12.1 numeric hash test narrowed** to same-type-value
  determinism (e.g. `1.5` vs `1.5000000` vs `15e-1` all
  parse to the same `f64`). Removed the "1500 vs 1500.0"
  example that contradicted §4.2's type-identity rule.
- **§10.2 app-demo handler migration** spells out the
  secret-resolution drop: every existing `secret_store
(&cfg.vault)?.require_str(&cfg.api_token)` call disappears
  under Model A.

**Reviewer pass (round 5) — macro / contract precision:**

- **§3.3.1.1–§3.3.1.4 (new) — concrete macro / metadata
  API.** Extended `SecretKind` with a third variant
  `KeyInNamedStore { store_ref_field: &'static str }` so the
  derived `SECRET_FIELDS` can name which sibling field
  carries the store id. Added the new
  `#[secret(store_ref = "field")]` attribute form. Spelled
  out serde rename policy (per-field `#[serde(rename)]` on
  `#[secret*]` fields is a compile_error in addition to the
  existing container-rename rejection). Spelled out
  nested-path policy: v1 only supports top-level
  `#[secret*]` annotations. Added a derive-time validation
  table (the eight compile-time checks the macro performs).
- **§3.3.8 (new) — push vs runtime validation
  semantics.** Documented the rule: push validates
  everything EXCEPT `#[secret]` / `#[secret(store_ref =
"...")]` fields; runtime validates everything. Push
  consults `SECRET_FIELDS` to skip per-field validators on
  secret-bearing fields. Sketched the
  `validate_excluding_secrets` wrapper. Acknowledged the
  twin-types alternative and rejected it for surface-area
  reasons.
- **§3.3.6 rewritten with full `SecretError` mapping
  table.** Documented all four `SecretError` variants
  (`NotFound`, `Validation`, `Internal`, `Unavailable`) and
  the `require_str`-specific invalid-UTF-8 path. Each maps
  to a documented `EdgeError` variant via the
  `map_secret_error` wrapper.
- **§4.3 ordering fixed.** Read-side validation now
  documents the precise sequence: sha verify → secret walk
  → `serde_json::from_value` → `Validate::validate` → yield
  `C`. Earlier draft had the deserialise BEFORE the secret
  walk, contradicting §3.3.3.
- **§10.2 app-demo migration updated for Model A.** The
  "struct UNCHANGED" claim revised to "minor change" —
  field shapes stay but the documented semantic of each
  `#[secret]` field changes from "key name" to "resolved
  value at runtime". `handlers.rs` line refs corrected to
  the actual call site at `handlers.rs:287` (not 159/166
  from an earlier read). Each scaffold template that
  changes is now enumerated explicitly per file
  (`core/src/lib.rs.hbs`, `core/src/handlers.rs.hbs`,
  `core/src/config.rs.hbs`, `app/name.toml.hbs`,
  `cli/src/main.rs.hbs`, `root/edgezero.toml.hbs`,
  `root/README.md.hbs`, and the per-adapter template trees
  for the generated `wrangler.toml` / `fastly.toml` /
  `spin.toml` / `axum.toml` snippets).
- **§10.2.1 grep gate scope widened** to include the four
  adapter template trees (`crates/edgezero-adapter-{axum,
cloudflare, fastly, spin}/src/templates`), so a
  generated project that still uses the legacy pattern in
  its adapter-side snippets fails CI.
- **§6.3.1 response-body shape declared precisely.** The
  outer `{ "error": { ... } }` envelope stays;
  `kind: String` is added inside `error` (every variant
  gets a stable string); `field_path: String` is added
  only on `config_out_of_date` (and any future
  field-anchored variant). Status code for
  `ConfigOutOfDate` is 503 with `Retry-After: 60`. Hard
  cutoff — clients that parse the body shape need to
  update.
- **§8.3 + §9.0 — Spin Cloud read-back marked
  UNSUPPORTED in v1.** Added a new
  `ReadConfigEntry::Unsupported(&'static str)` variant
  because the Spin CLI's `cloud key-value` subcommand set
  (Spin 3.6.x) exposes `set`/`delete`/`list` but no `get`.
  `config diff --adapter spin` against Cloud targets
  errors with an actionable message;
  `config push --adapter spin` against Cloud skips the
  skip-on-equal CHECK (no remote sha to compare against)
  but the §8.2 consent gate still runs — `--yes` or an
  accepted TTY prompt is required before the write.
  Follow-up to wire the Fermyon Cloud HTTP API or
  petition upstream is noted but out of v1 scope.
- **§12.5 validate test restated to structural-only.**
  Three sub-tests: `[stores.secrets]` declared,
  `#[secret(store_ref = "field")]` sibling exists at
  compile time, named store id is declared. No
  "secret value exists" probe; that test was removed.
- **§10.2.2 (new) — concrete scaffold template content.**
  For each of `core/src/config.rs.hbs`,
  `core/src/handlers.rs.hbs`, `app/name.toml.hbs`,
  `cli/src/main.rs.hbs`, `root/edgezero.toml.hbs`,
  `root/README.md.hbs`, and the four per-adapter template
  trees, the spec now shows the exact handlebars block
  that replaces the legacy wording. Covers the new
  `#[secret(store_ref = "vault")]` example in the
  generated struct, the `Diff(ConfigDiffArgs)` enum
  variant the downstream CLI gains, the
  `run_config_diff_typed::<C>` dispatch, and the
  "framework resolves secrets at extract time" comment
  shape. Implementer / reviewer use this section as the
  acceptance target for the scaffold migration; the
  existing `generated_project_builds --ignored` end-to-end
  test verifies the rendered output still compiles.

**Reviewer pass (round 6) — runtime carrier + contract
precision:**

- **§5.2.1 (new) — concrete carrier for `__KEY` runtime
  resolution.** Replaced today's per-id `ConfigStoreHandle`
  registry entry with a `ConfigStoreBinding { handle,
default_key }`. Adapters resolve
  `EDGEZERO__STORES__CONFIG__<ID>__KEY` at registry-build
  time (alongside their existing `__NAME` reads) and pack
  the resolved key into the binding. The extractor reads
  through `req.config_store_default_binding()`. Compared
  the alternatives (env in request extensions, generated
  resolver via `app!`) and pinned why the registry
  extension is the right call. `ConfigStoreHandle`-only
  paths (hand-managed `ConfigStore::get`) keep working —
  the helper unwraps `binding.handle`.
- **§3.3.3 extractor sketch rewritten** to match the exact
  `SecretField { name, kind }` / `SecretKind::{KeyInDefault,
StoreRef, KeyInNamedStore { store_ref_field }}` API from
  §3.3.1.1. Replaced `field.path` / `field.store_ref` with
  `field.name` / pattern-match on `field.kind`. The
  "nested-path handling" claim was removed — §3.3.1.2's
  top-level-only constraint is now consistent throughout.
- **§3.3.2 + §8.1 + §3.3.8 reconciled.** Push, diff, and
  validate all route through `validate_excluding_secrets`.
  The earlier "no change to push beyond the strip-revert"
  and "load + type-validate same as today" lines were
  internally inconsistent with §3.3.8; both rewritten to
  point at the shared wrapper.
- **§10.2.1 CI gate fixes.** The comment-skip filter
  `grep -vE '^\s*//'` was silently inert because
  `grep -rEn` prefixes lines with `path:line:`; replaced
  with a `:\s*//` anchor that survives the prefix. Pattern 2
  was overbroad — `\b(config_store_default|config_store\()`
  caught legitimate hand-managed reads (per §10.3); scoped
  to two known typed-handler files
  (`examples/app-demo/crates/app-demo-core/src/handlers.rs`,
  `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`)
  instead of the whole tree.
- **§3.2.1 raw-push rationale rewritten under Model A.**
  Earlier draft said "raw push would leak `#[secret]`
  values" — Model A doesn't leak values (the blob carries
  key names, not values). The new rationale is precise: raw
  push lacks a `C` for canonicalisation, validation routing
  through `validate_excluding_secrets`, and the envelope
  contract — those are the load-bearing reasons.
- **§8.3 + §9.0 Spin Cloud push UX nailed down.** The
  "writes unconditionally" stance now has three branches —
  `--yes` (silent unconditional write), TTY without
  `--yes` (prompt "cannot read remote on Spin Cloud; write
  anyway? [y/N]"), non-TTY without `--yes` (error pointing
  at `--yes` flag). Preserves §8.2's non-TTY explicit
  consent rule while accommodating the missing diff.
- **§3.3.6 `map_secret_error` sketch matches actual
  `SecretError` shapes.** Variants are
  `NotFound { name }` (struct-like), `Unavailable` (unit),
  `Validation(String)` (tuple), `Internal(#[from]
anyhow::Error)` (tuple). The earlier sketch
  pattern-matched on shapes that don't exist.
- **§6.3.1 `Retry-After: 60` extended to
  `ServiceUnavailable`** in addition to `ConfigOutOfDate`.
  Both are retryable on the same timescale; the single
  header rule serves both. Other variants explicitly do
  NOT set the header.
- **§4.1 primary blob example fixed.**
  `"api_token": "secret-store:default"` → `"demo_api_token"`
  matches Model A's "blob carries the secret-store key
  name verbatim" semantic that §3.3.4 documents.
- **`method_not_allowb ed` typo** in §6.3.1's stable-kind
  string list fixed to `method_not_allowed`.

**Reviewer pass (round 7) — implementability + contract
tightening:**

- **§5.2.1 raw `Config` extractor surface pinned.** Spelled
  out exactly what `Config::default()`, `Config::named()`,
  `Config::registry()` return under the new
  `StoreRegistry<ConfigStoreBinding>`. `default()` /
  `named()` keep returning `ConfigStoreHandle` (unwrap
  `binding.handle` internally) so hand-managed
  `bound.get(...)` callers compile unchanged. `registry()`
  exposes the binding-shaped registry — breaking surface,
  called out as hard cutoff per §1. Added two new
  accessors: `default_binding()` / `named_binding(id)` for
  callers that want the resolved `__KEY`. `BoundConfigStore`
  type alias stays as `ConfigStoreHandle` (not redefined to
  `ConfigStoreBinding`). Tests in §12.14 (`StoreRegistry`
  ref accessors) and §12.15 (raw `Config` binding
  accessors) cover both unwrap and binding-accessor paths.
- **§3.3.8 `validate_excluding_secrets` sketch made
  implementable.** `validator::ValidationErrors` in 0.20
  does not expose `.remove(field)`; it exposes
  `.errors_mut() -> &mut HashMap<&'static str,
ValidationErrorsKind>`. Rewrote the sketch to call
  `errors_mut().remove(field.name)` against that map.
  Documented that struct-level `#[validate]` rules live
  under `__all__` and are intentionally left alone (not
  field-scoped). Also documented the loader split
  required to support push: new
  `deserialize_app_config_with_options` (deserialise-only,
  no `.validate()`) for push + diff; existing
  `load_app_config_with_options` (deserialise + validate)
  for runtime + bundled `config validate`. Push calls
  deserialize-only then `validate_excluding_secrets`;
  existing call sites of `load_app_config*` keep their
  signature.
- **§5.2.1 `__KEY` env lookup centralised in
  `EnvConfig::store_key`.** The earlier sketch hand-coded
  `env.var(...)` per adapter. Adapters now call
  `env.store_key("config", id)`, mirroring the existing
  `env.store_name("config", id)` helper at
  `crates/edgezero-core/src/env_config.rs:133`. One helper
  with identical blank/whitespace/control-char filter
  rules; all four adapters share the behaviour. Tests
  live alongside `env_config`'s existing `store_name`
  tests.
- **§9.1 Axum local file shape reconciled.** The first
  example showed envelope OBJECTS as map values; the
  normative text said each value is an escaped JSON
  STRING. Picked the string form (matches
  `ConfigStore::get -> Option<String>`'s contract across
  all four adapters) and rewrote the §9.1 example to
  show escaped JSON strings only. The object form was
  removed.
- **§12.9 generated-CLI `validate --strict` rephrased.**
  Earlier line said it runs "the secret-store probes from
  §3.3.2", but §3.3.2 explicitly forbids probing live
  secret values. Reworded to: runs the typed validator
  path plus STRUCTURAL secret-metadata checks (named
  store ids declared, store-ref field values resolve to
  declared ids). Live-secret failure modes stay in
  §12.5's missing-secret-at-extract case.
- **§12.6 runtime validation assertion shape fixed.**
  Earlier draft asserted a flat `{ status, message,
kind: "config_out_of_date" }` body. Updated to the
  nested envelope §6.3.1 actually documents:
  `{ "error": { "status": 503, "kind": ..., "message":
..., "field_path": ... } }`. Added explicit assertion
  on the `field_path` value naming the offending field.
- **§6.3.1 `Retry-After: 60` scope made explicit.** The
  earlier line "on BOTH `ConfigOutOfDate` AND
  `ServiceUnavailable`" was correct but unscoped. Added
  a contract paragraph: the rule applies because
  `ServiceUnavailable` today only comes from config /
  secret-store backpressure (both retryable on ~60s);
  any future use of `ServiceUnavailable` must inherit
  the timescale constraint or introduce a new variant.
  Enforced by review, not by the type system; the
  variant's doc-comment captures the rule.

\*\*Reviewer pass (round 8) — implementability + arg surface

- enforceability:\*\*

* **§3.3.2 + §3.3.8 reconciled around `config validate`.**
  Round 7 left a contradiction: §3.3.2 said push, diff, AND
  generated `config validate` route through
  `validate_excluding_secrets`, but the §3.3.8 loader split
  said generated `config validate` keeps the existing
  deserialise+validate path. Aligned: all three call
  `deserialize_app_config_with_options` +
  `validate_excluding_secrets` + the §3.3.2 structural
  secret-metadata checks. Only the runtime extractor takes
  the deserialise + full `.validate()` path (the secret
  swap has already happened by then). The bundled
  `edgezero config validate` (no `C`) stays on
  `load_app_config_raw`.
* **§3.3.1.2 nested-`AppConfig` ban made enforceable.**
  Round 7 said "top-level only" but the derive macro only
  sees its own struct — a nested type that ALSO derives
  `AppConfig` carries its own `SECRET_FIELDS` that the
  outer extractor silently ignores. Added an
  `AppConfigRoot` sealed marker trait (emitted by the
  derive) + a §10.2.1 Pattern 4 CI grep gate that
  detects an `AppConfig`-rooted type being used as a
  field type in another `AppConfig`-rooted struct.
  Recursive-metadata path recorded as Q13 in §11 for v2.
* **§3.2.1 / §3.2.2 raw-binary UX aligned.** Earlier draft
  said the bundled binary's `edgezero config push` exits
  via clap's default "unrecognized subcommand" (§3.2.2),
  but §3.2.1 promised a pointer message and the §12.8 test
  expected the pointer. Aligned by keeping `Push` / `Diff`
  as STUB-POINTER variants in `ConfigCmd` so `--help`
  still lists them (operators see what to look for), with
  the match arms printing the pointer at the typed CLI
  and exiting code 2. §12.8 updated to assert the
  pointer text and `code 2`.
* **§3.2.2 CLI arg surface filled in.** Earlier draft said
  `ConfigPushArgs` is "unchanged" but the rest of the spec
  required `--key`, `--yes`, `--no-diff`, `--runtime-config`
  for Spin, `--dry-run`. Wrote out the full clap field
  list for `ConfigPushArgs`, `ConfigDiffArgs` (added
  `--local`, `--runtime-config`, `--format`), and
  `ConfigValidateArgs` (`--strict` on BOTH raw + typed —
  see round-9 H-1 reconciliation).
  Added an invariants paragraph naming where each flag is
  load-bearing.
* **§5.2.1 `StoreRegistry` ref accessors specified.**
  `default_binding()` / `named_binding(id)` return
  `Option<&ConfigStoreBinding>` but `StoreRegistry<H>`
  exposes only owned-clone accessors today. Added
  `default_ref` / `named_ref` (generic on `H`) to the
  registry. Also annotated `ConfigStoreBinding: Clone +
Debug` (required by `StoreRegistry<H: Clone>` bound +
  the existing `Debug` derive).
* **§9.2 Cloudflare wrangler command paths pinned.** Spec
  used `wrangler kv get` / `wrangler kv put` (deprecated
  three-segment form); current code uses the four-segment
  `wrangler kv key get` for read-back and the bulk-put
  form `wrangler kv bulk put --namespace-id=<id> --remote`
  for writes (matching the namespace-id-not-binding-name
  rationale at
  `crates/edgezero-adapter-cloudflare/src/cli.rs:289`).
  §8.3 + §9.2 updated to pin both paths.
* **§3.3.2 adapter typed checks include `KeyInNamedStore`.**
  Today `run_adapter_typed_checks` only forwards
  `KeyInDefault` field values to
  `Adapter::validate_typed_secrets` (so Spin's flat-
  namespace check sees default-store keys). Under Model A
  a `KeyInNamedStore` key targeted at the Spin store
  carries the same constraint. The walk now visits
  `KeyInNamedStore` entries and resolves the sibling
  store-ref field; trait grows a third `&str /* store_id
*/` parameter that non-Spin adapters ignore.
* **§10.2.2 scaffold `core/src/handlers.rs.hbs`
  unused-import fixed.** Round 6 added the sample handler
  as a commented block AND said to add a live
  `use edgezero_core::extractor::AppConfig;` at the top —
  but the live import on dead handler code would fail
  `-D warnings` clippy. The import is now ALSO commented
  in the same block as the sample, uncommenting together
  when the operator activates the sample. §10.2.1's
  positive grep scope narrowed to `examples/app-demo/`
  so the commented template import doesn't accidentally
  satisfy the gate; the §10.2.1 positive check also runs
  through `not_comment` to ignore stray commented matches
  in app-demo.

**Reviewer pass (round 9) — CLI surface alignment + gate
implementability + test coverage:**

- **§3.2.2 CLI flag surface aligned with the shipped CLI
  at `crates/edgezero-cli/src/args.rs:179`.** Round 8 had
  written `config: Option<PathBuf>` (rendering as
  `--config`) and dropped `--store` / `--no-env`, but the
  rest of the spec (and the existing PR #269 baseline)
  uses `--app-config`, `--store`, `--no-env`. The hard
  cutoff keeps the canonical names: `--app-config`,
  `--manifest` (default `edgezero.toml`), `--store`,
  `--no-env`, `--local`, `--runtime-config`, `--dry-run`
  — plus the new blob-model flags `--key`, `--yes`,
  `--no-diff`, `--format`, `--strict`. The flag-surface
  invariants block now enumerates every flag with its
  load-bearing section, and §12.11 parser-tests the
  whole surface.
- **§3.3.1.2 `AppConfigRoot` marker is PUBLIC and
  UNSEALED.** Round 8's "sealed module" wording would
  prevent the derive macro (expanding in downstream user
  crates) from emitting the impl. The marker is public,
  open, has no methods, and exists solely as a
  discoverable signal for the CI gate.
- **§10.2.1 Pattern 4 grep rewritten for multi-line
  derives.** The earlier one-line regex only matched
  `derive(...) ... struct X` on the same line; Rust
  idiomatically separates them (see
  `crates/edgezero-cli/src/templates/core/src/config.rs.hbs:18-20`,
  which sometimes interposes `#[serde(...)]` between
  derive and struct). Pattern 4 now uses awk to walk a
  5-line window forward from each
  `derive(.*AppConfig)` line and capture the next
  `(pub )?struct <Ident>`. §12.17 fixtures this with a
  multi-line derive that the awk window must correctly
  capture.
- **§9.2 Cloudflare local push uses `--binding`, not
  `--namespace-id`.** Round 8 had the local form using
  `--namespace-id=<id> --local`, but the shipped
  behaviour at
  `crates/edgezero-adapter-cloudflare/src/cli.rs:399`
  intentionally addresses by BINDING for local so the
  scaffold's `local-dev-placeholder` namespace ids
  still work pre-`provision`. Remote keeps the
  namespace-id form (per `cli.rs:289`'s rationale).
  §12.13 tests both modes.
- **§3.3.2 `--strict` semantics aligned.** Round 8 had
  `--strict` annotated "generated-CLI only" in the args
  block, but the shipped bundled raw `config validate`
  at `crates/edgezero-cli/src/config.rs:503` already
  forwards `--strict` into shared manifest-level
  checks. Hard cutoff: both bundled (raw) and
  generated (typed) `config validate --strict` honor
  the flag; the difference is the ADDED typed walk on
  the generated side, NOT a flag-honored / flag-ignored
  split. §12.9 + §12.11 updated to assert this.
- **§3.3.2 KeyInNamedStore adapter-validation
  invariant restated.** Round 8 added the trait
  extension; round 9 adds a one-line invariant under
  the §3.3.2 block stating that ALL three secret-key
  variants (`KeyInDefault`, `KeyInNamedStore`, and
  the value of `StoreRef`'s sibling) flow through
  `Adapter::validate_typed_secrets`. Tests at §12.16
  cover the Spin-collision path and assert the mock
  adapter sees both variants.
- **§3.2.1 raw-binary UX wording reconciled once
  more.** The doc still occasionally said "raw
  `edgezero config push` is removed", which would
  contradict §3.2.1's stub-pointer story. Updated the
  v1 changelog's round-2 retroactive note and the
  scaffold-README guidance to say the subcommands are
  stub-pointer subcommands (Push/Diff stubs print the
  pointer at the typed CLI and exit 2; Validate does
  the real raw work).
- **Test plan extended (§12.11-§12.17).** Six new
  test subsections covering the riskiest new surfaces:
  CLI parser tests for the canonical flag surface
  (§12.11); `--store` routing in push/diff (§12.12);
  Cloudflare local-vs-remote binding mode (§12.13);
  `StoreRegistry::default_ref` / `named_ref` (§12.14);
  raw `Config` binding accessors (§12.15); named-store
  secret adapter validation (§12.16); nested-`AppConfig`
  CI gate fixture with multi-line derive (§12.17).

**Reviewer pass (round 10) — portability + invariant
sharpening + contract precision:**

- **§10.2.1 Pattern 4 awk rewritten for POSIX
  portability.** Round 9 used `match(str, regex, m)`'s
  3-arg form, which is `gawk`-only — macOS / BusyBox /
  nawk silently fail and the pipeline's trailing
  `|| true` swallows the failure, so `roots` becomes
  empty and the nested-AppConfig ban is never enforced.
  Rewrote with POSIX `match()` / `RSTART` / `RLENGTH`
  (no array capture) + `substr` + `sub` to slice the
  identifier. Added a hard "empty roots = fail loud"
  check so a stripped awk doesn't silently pass the
  gate.
- **§4.2 canonicalisation string rule is verbatim
  UTF-8.** Earlier draft normalised strings to NFC at
  canonicalisation time. Round 10 audit found two
  fatal consequences: (a) push mutates operator input
  (secret key names lose their original encoding),
  (b) NFC and NFD blobs share a SHA, so skip-on-equal
  silently skips a real change. Switched to verbatim
  UTF-8 — the SHA identifies the EXACT persisted
  bytes. Operators wanting Unicode-invariance add a
  `#[validate(custom = "nfc_only")]` rule themselves;
  the framework does not silently normalise. §12.1
  determinism test updated to assert NFC vs NFD
  produce DIFFERENT shas.
- **§4.2 serde shape constraints added.** Verbatim
  canonicalisation requires the JSON output to match
  the Rust field tree exactly — `#[serde(skip_serializing)]`
  / `#[serde(skip_serializing_if = ...)]` would omit
  fields the canonical-form rule says must be `null`,
  and `#[serde(flatten)]` would break the
  field-identifier-sort rule and `serde_path_to_error`
  paths. The `#[derive(AppConfig)]` macro now rejects
  all three on every field (not just `#[secret]`
  ones), with `trybuild` compile-fail fixtures in
  §12.1.
- **§6.3.1 `Retry-After: 60` narrowed to
  `ConfigOutOfDate` only.** Round 9 extended the
  header to `ServiceUnavailable` too; round 10 audit
  of in-tree `ServiceUnavailable` producers found
  three non-retryable cases (KV size limit, missing
  named KV store, missing default secret store) that
  reuse the variant. Sending the header on all 503s
  would mislead clients into a tight retry loop on
  manifest-gap failures. Header now fires on
  `ConfigOutOfDate` only; `ServiceUnavailable` carries
  no `Retry-After`. Audit ticket recorded for a v2
  split into `ServiceUnavailable` (generic) +
  `ServiceUnavailableRetryable` (carries the header).
- **§3.2.2 CLI args — `--exit-code` added to
  `ConfigDiffArgs`.** Q10 chose `--exit-code` as v1
  CI-mode behaviour, but the round-9 args block
  omitted the flag. Now wired with `exit-code: false`
  default, behaviour: 0 == no diff, 1 == diff
  present, 2 == error. §12.11 parser test extended,
  §12.11 behaviour test added (3-branch).
- **§9.4 Spin Cloud `--dry-run` defined.** Pre-round-10
  the spec said push `--dry-run` is "diff only, no
  write" but Spin Cloud read-back is unsupported, so
  the diff is impossible. Added a fourth row to the
  Spin Cloud three-branch table: `--dry-run` exits
  non-zero with an actionable message ("no remote
  read-back; re-run with --local for the on-disk
  SQLite write or drop --dry-run for unconditional
  --yes push"). Refuses rather than printing a
  half-truth.
- **§3.2.2 stub Push/Diff are UNIT variants.** Round 9
  had `Push(ConfigPushArgs)` and `Diff(ConfigDiffArgs)`
  as stub-pointer variants — but those carry
  `--adapter` (required = true), so `edgezero config
push` would fail clap's required-arg check BEFORE
  the stub body ran, hiding the pointer message.
  Switched both to UNIT variants (`Push,` / `Diff,`)
  so clap accepts the bare subcommand and the match
  arm prints the pointer. The README + scaffold
  template references updated to match.
- **§12.16 Spin-secret named-store test rewritten
  against the actual `cli.rs:363` contract.** Round 9
  test invoked `config validate --strict --adapter
spin`, but `ConfigValidateArgs` has no `--adapter`
  field — `validate` runs adapter-typed checks across
  every adapter declared in `[adapters.*]`. The test
  also asserted a `[variables]` collision, but Spin's
  real check is "lowercased secret value is a valid
  Spin variable name AND no two collide in the flat
  variable namespace". Test cases rewritten to:
  (a) `KeyInNamedStore` value with an invalid Spin
  name (dash); (b) `KeyInDefault`-vs-`KeyInNamedStore`
  collision on the lowercased Spin variable name;
  (c) non-Spin adapter exempt. All three test the
  documented `crates/edgezero-adapter-spin/src/cli.rs:363`
  behaviour.
- **§3.3.2 `validate_typed_secrets` pinned to a named
  struct `TypedSecretEntry`.** Round 9 carried two
  parallel framings ("grows a second parameter" /
  "extend the existing tuple to a 3-tuple") that would
  invite adapter implementation drift. The trait now
  takes `&[TypedSecretEntry<'_>]` with named fields
  `store_id`, `field_name`, `key_value`. The struct
  is `#[non_exhaustive]` so future v2 additions are
  source-compatible.

**Reviewer pass (round 11) — cross-crate constructibility +
runtime accuracy + parser fidelity:**

- **§3.3.2 `TypedSecretEntry::new` constructor added.**
  Round 10 marked the struct `#[non_exhaustive]` but
  `edgezero-cli` is the construction site (not the
  defining crate `edgezero-adapter`), and
  `#[non_exhaustive]` blocks struct-literal
  construction from external crates. Added an inherent
  `pub fn new(store_id, field_name, key_value)`
  constructor so `edgezero-cli`'s
  `run_adapter_typed_checks` can assemble the slice.
  `#[non_exhaustive]` stays for v2 source-compat.
- **§3.3.3 extractor uses `req.secret_store_default()`,
  not a hardcoded id.** Round 10 sketch read
  `DEFAULT_SECRET_STORE_ID.to_owned()` for
  `KeyInDefault`, but the manifest's default secret
  store id is configurable (or inferred from a sole id
  via `StoreDeclaration::default_id()` at
  `crates/edgezero-core/src/manifest.rs:486`). Any
  project whose default id isn't literally `"default"`
  would fail. Rewrote the walk to dispatch via
  `RequestContext::secret_store_default()` at
  `crates/edgezero-core/src/context.rs:172` for
  `KeyInDefault` and via `secret_store(&id)` for
  `KeyInNamedStore`; the BOUND store carries its own
  `store_name()` for the error message.
- **§10.2.1 Pattern 4 switched to a syn-based Rust
  helper.** Round 10 used POSIX awk windowing, but two
  failure modes survived: (a) multi-line `#[derive(...)]`
  where `AppConfig` lands on a continuation line, and
  (b) generic-wrapped field types
  (`Option<ChildConfig>`, `Vec<...>`, `Box<...>`, tuple
  fields, array fields). Both require real AST
  awareness. The gate now invokes
  `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
  (a syn-based binary, ~120 LOC, gated behind a default-
  OFF `nested-app-config-check` feature so `syn`/`walkdir`
  stay out of the default build) that collects every
  `AppConfig`-rooted struct via token-level parsing and
  walks each AppConfig struct's `syn::Type` recursively
  to detect any nested reference. §12.17 extended to
  cover all six generic-wrap shapes plus the multi-line
  derive case plus the malformed-input case (exit 2).
- **§6.3.1 `EdgeError::config_out_of_date` split into
  two constructors.** Round 10 had two contradictory
  signatures: secret walk called
  `config_out_of_date(msg, field_path)` (round 10
  sketch); §6.3.1 declared the constructor takes a
  `serde_path_to_error::Error`. Aligned: two
  constructors,
  `config_out_of_date(message, field_path)` for
  explicit-pair callers (secret walk + validator
  path) and `config_out_of_date_from_serde(err)` for
  the deserialise path. Extractor sketch updated to
  use the serde constructor; secret-walk + validator
  paths use the explicit-pair form.
- **§3.2.2 + Q10 — `--exit-code` does not mask
  errors.** Round 10 §12.11 test asserted that
  without `--exit-code`, a remote-read network
  failure exits 0 (same as no-change AND diff
  cases). That would silently break CI gates. The
  flag now ONLY changes the "diff present" success
  branch: with `--exit-code`, exit 1 when the diff is
  non-empty; without, exit 0 when the diff is
  non-empty. In BOTH modes, real errors ALWAYS exit
  ≥2. §12.11 split into "success branches" and
  "error branches"; the latter is asserted
  non-zero-regardless. Q10's wording aligned.
- **§3.3.1.4 derive-time validation table unified.**
  Round 10 H-2 added the `#[serde(skip_*)]` /
  `#[serde(flatten)]` ban in §4.2 prose only; the
  derive-time table in §3.3.1.4 didn't list those
  rules. Same table claimed container `rename_all` is
  always a compile error, but the macro at
  `crates/edgezero-macros/src/app_config.rs:55` only
  rejects it when the struct has at least one secret
  field. Reconciled: §3.3.1.4 is now the
  AUTHORITATIVE single-source-of-truth list, with
  every rule annotated by scope (per-field /
  container) + introduction round + macro source
  reference. The "non-secret fields can rename
  freely" qualifier moved into a "Notes on apparent
  contradictions resolved here" block under the
  table.
- **§4.2 NFC guidance qualified non-secret-only +
  Q14 added.** Round 10 told operators to use
  `#[validate(custom = "nfc_only")]` for
  normalisation invariance — but for secret-bearing
  fields, push skips the validator (per §3.3.8) and
  runtime validates the resolved secret VALUE, not
  the operator-typed key NAME. The guidance is now
  qualified to NON-SECRET fields only; secret-key-
  name validation is an out-of-band concern in v1,
  tracked as new Q14 for v2 follow-up.

**Reviewer pass (round 12) — runnability + acceptance
gating + UX precision:**

- **§10.2.1 Pattern 4 helper RELOCATED + DEPS
  WIRED.** Round 11 said the helper lived at
  `scripts/check_no_nested_app_config.rs`, but cargo
  doesn't auto-discover bins under `scripts/`, and
  `edgezero-cli/Cargo.toml:11` only registers the
  `edgezero` bin. Moved the helper to
  `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
  (cargo auto-discovers `src/bin/*.rs`). Added a
  default-OFF `nested-app-config-check` feature in
  `edgezero-cli/Cargo.toml` gating optional
  `syn`/`walkdir` deps, with an explicit `[[bin]]`
  entry declaring `required-features =
["nested-app-config-check"]`. CI invokes via
  `cargo run --features nested-app-config-check
--bin check_no_nested_app_config -- ...`. The
  feature keeps `syn` + `walkdir` out of the default
  build entirely.
- **§3.2.2 + §12.8 stub-pointer surfaces reconciled
  for unit-variant clap.** Round 11 made bundled
  `Push` / `Diff` UNIT variants (no Args struct) so
  clap accepts `edgezero config push` bare, but then
  `edgezero config push --adapter axum` fails clap
  parse with "unexpected argument" BEFORE the
  match arm runs. The test plan invoked it with
  `--adapter axum` and expected the pointer text —
  contradiction. Reconciled: the same pointer string
  is wired into clap's `after_help` (via
  `#[command(after_help)]` / `Command::after_help`)
  for BOTH stub subcommands, so the with-flags case
  shows the pointer in clap's error output AND the
  bare case shows it in the match arm. §12.8 split
  into "bare invocation — match-arm path" and
  "with-flags invocation — clap after_help path"
  tests, asserting byte-for-byte identical pointer
  text across the two surfaces.
- **§3.2.2 `ConfigPushArgs::dry_run` doc revised.**
  Round 11's doc-comment said "Exit 0 always", but
  §9.4's Spin Cloud `--dry-run` branch exits non-zero
  (read-back unsupported). Reworded to "Exit 0 on a
  clean dry-run; non-zero on any blocking error
  (manifest / local-TOML parse failure, remote-read
  failure, OR the Spin Cloud unsupported case)". The
  contract is "no write, show the comparison"; if the
  comparison can't be produced honestly, refuse
  rather than print a half-truth.
- **§3.3.1.4 + §4.2 macro-side enforcement scope
  spelled out + `skip_serializing` fixture added.**
  Round 10 H-2 added per-field skip/flatten bans in
  §4.2 prose, but the existing macro at
  `crates/edgezero-macros/src/app_config.rs:140` only
  calls `enforce_no_disallowed_serde_attrs` after
  identifying a secret field. The spec now explicitly
  says the macro adds a whole-struct pass over EVERY
  field (secret or not) for the skip/flatten
  universals. The `trybuild` compile-fail fixture
  list was missing the bare `#[serde(skip_serializing)]`
  case — added.
- **§4.2 canonicaliser version/hash placeholders
  policy formalised + §13.1 acceptance gate added.**
  Round 11 left `serde_canonical_json = "=X.Y.Z"`
  and `5d4a0e7f…fixed-hex-value…b9` as placeholders.
  Spec now explicitly documents that the implementing
  PR (per §13) replaces both with concrete values, and
  adds a new §13.1 CI gate
  (`scripts/check_no_placeholder_pins.sh`) that greps
  for `…`, `fixed-hex-value`, and `X.Y.Z` markers in
  the pin test + workspace Cargo.toml and fails the
  build if any survive. The acceptance gate makes the
  "version: 1 means this byte format" contract
  testable by ensuring the byte format is actually
  pinned to a concrete value before merge.
- **§8.1 diff synopsis extended.** Round 11 listed
  only `manifest`, `app-config`, `store`, `key`,
  `no-env`, `format`. Added `--local`,
  `--runtime-config`, `--exit-code` to match the
  `ConfigDiffArgs` declaration in §3.2.2 (which already
  carried all three after round 10/11). Synopsis now
  matches the args struct 1:1.
- **§8.3 + §3.2.2 Spin Cloud branch count corrected
  from three to four.** Round 11 added the `--dry-run`
  row to the Spin Cloud UX table (making it four rows)
  but the surrounding prose, the args doc-comments, and
  the flag-surface invariants still said
  "three-branch". Updated to "four-branch" across the
  three call sites; also clarified that the "successful
  Cloud write" output applies only to the `--yes`
  branch and the TTY-with-accepted-prompt branch
  (the `--dry-run` and non-TTY-without-`--yes`
  branches exit non-zero before any write).

**Reviewer pass (round 13) — runnability + portability +
gate accuracy:**

- **§10.2.1 Pattern 4 scope narrowed to `.rs` only +
  rendered-template fixture pass added.** Round 12
  said the helper walked `.rs` AND `.rs.hbs`, but
  `.rs.hbs` files contain unrendered Handlebars
  (`{{NameUpperCamel}}` at
  `crates/edgezero-cli/src/templates/core/src/config.rs.hbs:20`,
  `{{#each}}` blocks at
  `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:18`,
  etc.) which `syn::parse_file` rejects with a syntax
  error before any nesting check runs. Scoped the
  helper to `.rs` only; added a paired integration
  test at
  `crates/edgezero-cli/tests/scaffold_render.rs` that
  renders every template with deterministic fixture
  values and runs the same helper on the rendered
  output. The integration test runs under
  `cargo test --workspace`; the shell gate runs
  separately. §3.3.1.2 updated to point at the
  syn helper (no more awk reference).
- **§10.2.1 shell gate `set -e` interaction
  fixed.** Round 12 wrote
  `nested_helper_output=$(...); status=$?` — but
  under `set -euo pipefail`, a non-zero command
  substitution exits the script immediately, so
  status 1 (violations) and status 2 (syntax
  errors) would never reach the case statement.
  Switched to
  `if cmd; then status=0; else status=$?; fi`,
  which suppresses the `set -e` trip and lets the
  script inspect the actual exit code.
- **§3.3.1.2 stale awk reference replaced.** The
  field-path-resolution section still described the
  rejected awk-windowing approach. Updated to
  describe the syn-based helper at
  `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
  and the rendered-template fixture pass; removed
  the "5-line window from `derive(.*AppConfig)`"
  wording entirely.
- **§5.2 store-id charset tightened.** The current
  manifest validator at
  `crates/edgezero-core/src/manifest.rs:880` allows
  `[A-Za-z0-9_-]` in store ids, but
  `EDGEZERO__STORES__CONFIG__FEATURE-FLAGS__KEY=...`
  is not a valid POSIX shell environment variable
  name (hyphen breaks `export`). Operators with
  hyphenated ids silently lose the `__NAME` /
  `__KEY` overrides. Hard-cutoff per §1: the blob
  model narrows the allowed charset to
  `[A-Za-z0-9_]+` and the manifest validator
  rejects hyphens with an actionable error message.
  §12.18 adds the manifest-validation test case.
- **§13.1 canonicaliser gate accepts the
  hand-rolled option.** Round 12's gate hard-coded
  a workspace-dependency check, but §4.2 explicitly
  allows EITHER a pinned external crate OR an
  in-tree hand-rolled walker. The in-tree option
  would have failed the gate despite being
  allowed. Updated the gate to accept either a
  workspace dependency entry OR an in-tree module
  at `crates/edgezero-core/src/canonical_form.rs`
  (or `.../canonical_form/mod.rs`). §4.2 acceptance
  criteria reworded to name the in-tree path
  explicitly.
- **§10.2.1 positive AppConfig migration gate
  matches usage shapes, not import lines.** Round
  12's gate greppe d for the literal line
  `use edgezero_core::extractor::AppConfig` — but
  the current app-demo handlers at
  `examples/app-demo/crates/app-demo-core/src/handlers.rs:8`
  use a grouped extractor import
  (`{Headers, Json, Kv, Path, Query, Secrets,
ValidatedPath}`). A correct migration that adds
  `AppConfig` to that group would silently fail the
  gate. Switched the grep to match the usage shape
  `AppConfig[<(]` (extractor instantiation in a
  handler signature or pattern destructure), which
  is robust to both grouped and single-import
  forms.

**Reviewer pass (round 14) — behaviour-affecting fixes
before plan authoring:**

- **§3.2.2 stub Push/Diff use a CATCH-ALL args
  struct, not after_help.** Round 13 wired the
  §3.2.1 pointer text into clap's `after_help` to
  cover the with-flags case (`edgezero config push
--adapter axum`). The round-14 reviewer ran a Clap
  4.6.x probe and confirmed `after_help` renders on
  `--help` output but NOT on unexpected-flag /
  missing-required-arg parse errors. So the
  with-flags case would have shown clap's generic
  error WITHOUT the pointer. Replaced with a
  catch-all `ConfigCmdStubArgs` carrying
  `trailing_var_arg = true` +
  `allow_hyphen_values = true` — clap dispatches
  every flag/arg to the match arm, the stub body
  prints the pointer, and `after_help` continues to
  cover the explicit `--help` path. §12.8 split into
  three surfaces: bare-invocation, with-flags
  (catch-all path), and `--help` (after_help path);
  all three carry byte-for-byte identical pointer
  text.
- **§3.3.8 CLI loader vs runtime extractor paths
  documented as DISTINCT.** Earlier drafts said the
  runtime extractor was the "only consumer of the
  full deserialise+validate `load_app_config*` shape"
  — but the runtime extractor reads JSON from the
  blob (post-SHA-verify, post-secret-walk), not TOML
  from disk, so it never touches `load_app_config*`
  at all. Split the §3.3.8 loader section into a
  three-row table (CLI typed paths, bundled raw
  validate, runtime extractor) naming each
  caller's source + path. The runtime extractor's
  row uses `serde_path_to_error::Deserializer` over
  the blob's JSON `data` field plus
  `Validate::validate(&cfg)`, NOT
  `load_app_config*`. The new
  `deserialize_app_config*` entry points are
  CLI-side only.
- **§9.0 `ReadConfigEntry::Unsupported` narrowed to
  skip-on-equal short-circuit.** Earlier doc said
  the variant short-circuits push to "unconditional
  write", but that contradicts §8.2's non-TTY-
  without-`--yes` consent gate. Reworded: the
  skip-on-equal CHECK short-circuits (no remote sha
  to compare against), but the consent policy still
  runs — for Spin Cloud that means `--yes` or an
  accepted TTY prompt. The §1 changelog reference
  and §9.0 doc-comment both updated.
- **§10.2.1 `[[bin]]` registration text de-
  contradicted.** Round 13 said the helper "self-
  registers via Cargo's `src/bin/` convention with
  no manual `[[bin]]` entry needed" and ALSO
  required `[[bin]] required-features = [...]` in
  the very next paragraph. Removed the auto-
  discovery claim — the explicit `[[bin]]` entry is
  NECESSARY (without it `required-features` can't
  be attached, and `cargo build` without the feature
  would fail to compile the helper). Replaced with
  a single coherent block explaining why the
  explicit entry is required.
- **§8.2 `--no-diff --yes` semantics narrowed.**
  Earlier wording called this "equivalent to the
  pre-rewrite blind push". Reworded: `--no-diff
--yes` suppresses the diff RENDER and the prompt
  ONLY; read-back and skip-on-equal STILL RUN, so a
  no-op push exits early without writing. Exception:
  on `ReadConfigEntry::Unsupported` (Spin Cloud) the
  skip-on-equal can't run because there's no remote
  sha, so the write proceeds unconditionally under
  the consent gate.

**Reviewer pass (round 15) — pre-plan blockers:**

- **§9.4 + Q12 + §12.10 Spin Cloud cap aligned with
  shipped writer.** Earlier drafts proposed a 200 KiB
  cap with a 199 KiB success test, but the writer at
  `crates/edgezero-adapter-spin/src/cli/push_cloud.rs:46`
  already enforces `MAX_ARGV_BYTES_PER_INVOCATION = 96
  - 1024`per`<KEY>=<VALUE>`pair. Under the blob
model the effective cap is`MAX_ARGV_BYTES_PER_INVOCATION - <KEY>.len() - 1`bytes (~95 KiB for the default`app_config`key).
The 200 KiB spec wouldn't have matched reality and
the 199 KiB test would have failed. v1 inherits
the existing cap; the implementing PR only updates
the error message to name blob-model workarounds.
Q12 wording and §12.10 boundary tests rewritten to
use`MAX_ARGV_BYTES_PER_INVOCATION` symbolically.
- **§4.2 non-finite floats rejected at load time.**
  `serde_json::to_value(f64::NAN)` and `f64::INFINITY`
  serialise to JSON `null`, which would collide with
  real `Option::None` / explicit `null` values in the
  canonical SHA — false skip-on-equal matches across
  fundamentally different configs. TOML's float
  grammar accepts `nan` / `inf` / `-inf`, and the env
  overlay's `parse::<f64>()` at
  `crates/edgezero-core/src/app_config.rs:298`
  accepts them too. The loader now walks the parsed
  `toml::Value` tree after env overlay and calls
  `f64::is_finite()` on every float leaf; the overlay
  parser adds an `is_finite()` check immediately
  after `parse::<f64>()`. Both paths raise
  `AppConfigError::Validation` naming the offending
  dotted path. §12.1 adds three test cases (TOML
  literal, env overlay, finite-OK control).
- **§3.2.2 stub catch-all `trailing` arg marked
  `hide = true`.** A round-15 Clap probe confirmed
  that without the hide flag, `edgezero config push
--help` renders `Usage: ... [TRAILING]...` plus an
  `Arguments: [TRAILING]...` section — exposing the
  implementation-detail sink. `hide = true` removes
  both surfaces without disabling parsing.
  §12.8's `--help` test now asserts BOTH that the
  pointer text appears AND that `[TRAILING]` does
  NOT appear anywhere in the help output.

**Reviewer pass (round 16) — pre-plan polish:**

- **§3.2.2 + §10.2.2 stub variant prose called
  "unit variants" but the declaration is
  `Push(ConfigCmdStubArgs)` (a tuple variant
  carrying the hidden catch-all). Reviewer caught
  the contradiction.** Rewrote the §3.2.2
  implementation-note comment AND the §10.2.2
  scaffold-migration bullet to call the stubs
  TUPLE variants whose tuple element is the
  hidden catch-all `ConfigCmdStubArgs` — NOT
  `ConfigPushArgs` / `ConfigDiffArgs` (those are
  the typed-CLI Args structs). Also added
  `#[command(after_help = STUB_POINTER_AFTER_HELP)]`
  directly on each variant in the sketch (the help-
  path test at §12.8 depends on the attribute being
  per-variant; the round-15 sketch implied it but
  didn't show it). Historical round-10/11
  changelog entries still describe their then-
  current "UNIT variant" form to keep the
  evolution trail readable.
- **§4.2 + §12.1 non-finite-float error shape made
  concrete.** Round 15 said the loader returns
  `AppConfigError::Validation` "with a message
  naming the dotted path", but
  `AppConfigError::Validation` at
  `crates/edgezero-core/src/app_config.rs:125`
  wraps `Box<ValidationErrors>` — not a plain
  message. Constructing a fake `ValidationErrors`
  with an owned dotted-path key would mix the
  validator-rule error renderer into a non-
  validator path. Added a NEW variant
  `AppConfigError::InvalidValue { path,
field_path, message }`; both load paths (TOML
  walk + env-overlay parse) raise it. §12.1's
  three test families now pattern-match on
  `InvalidValue` and assert `field_path` + a
  message substring — no `ValidationErrors`
  construction.
- **§12.18 added; stale `§12.x` placeholders
  replaced.** Three references to `§12.x`
  (round-9 raw-`Config` binding tests, round-13
  manifest-charset tests, round-13 store-id-charset
  prose) pointed at unnumbered slots. Resolved to
  concrete sections — §12.14 / §12.15 for the
  raw `Config` paths, and a new §12.18 for
  manifest-validation tightening (hyphen rejected,
  underscore-only succeeds, `__` stays rejected).

**Reviewer pass (round 17) — final pre-plan polish:**

- **§3.2.2 + §12.8 parent `Command::Config` carries
  `#[command(after_help = STUB_POINTER_AFTER_HELP)]`.**
  A round-17 Clap probe confirmed that child
  `after_help` on `Push` / `Diff` does NOT render on
  `edgezero config --help` — clap renders child
  `after_help` only when that specific child's
  `--help` is invoked. Without the parent-level
  attribute, the parent help screen lists `push` /
  `diff` as available subcommands with no hint
  they're stubs. Added the parent-level
  `after_help` in §3.2.2's enum sketch; §12.8 now
  asserts the pointer on four surfaces (`config
push --help`, `config diff --help`, `config
--help`, and explicitly ABSENT from `config
validate --help`).
- **§3.2.1 + §3.2.2 ONE canonical pointer
  constant.** §3.2.1's example showed
  `Run <your-app>-cli config push --adapter axum`
  while the §3.2.2 `STUB_POINTER_AFTER_HELP`
  constant said `Run <your-app>-cli config push
(or ... diff)`. §12.8 asserts byte-for-byte
  equality, so the two would have failed the
  assertion. §3.2.1's reproduction now references
  the constant explicitly and matches it
  verbatim; the example is annotated as "do NOT
  hand-edit without updating the constant".
- **§4.3 read-side deserialize uses
  `serde_path_to_error`.** Earlier draft step 5
  said `serde_json::from_value(data)` directly,
  but §3.3.3 and §6.3.1 both require
  `serde_path_to_error::Deserializer` so
  `EdgeError::ConfigOutOfDate.field_path` is
  populated. Step 5 now wraps
  `Value::into_deserializer()` with
  `serde_path_to_error::deserialize` and maps the
  error via `config_out_of_date_from_serde` per
  the round-11 two-constructor split. Without
  this, §12.6's `field_path` assertion would fail.
- **Q6 + §12.10 Fastly cap stated as ONE number
  (64 KiB).** Q6 earlier called it ~8 KiB
  (historical edge-dictionary cap, not Config
  Store), while §12.10 said "well beyond" the
  Spin cap. The current Fastly writer uses
  `--stdin` (PR #269 F4) so there's no argv
  ceiling; the only cap is the platform's
  Config Store entry value limit of 64 KiB. Q6
  now carries a per-adapter table (Axum / CF /
  Fastly / Spin Cloud / Spin local) with the
  concrete numbers; §12.10 cross-references
  Fastly's 64 KiB and notes that §12.3 covers
  the Fastly-specific size-cap test (separate
  from the Spin-shaped guard).
- **§10.2.2 scaffold gate wording matches the
  script.** §10.2.1's CI script greps for the
  USAGE shape `AppConfig[<(]` (round-13
  change), but the §10.2.2 scaffold-section
  prose still said the gate "greps for the
  `use edgezero_core::extractor::AppConfig`
  literal". Replaced the stale wording with
  the usage-shape description; the gate is
  robust to grouped imports.
- **§12.11 no-flag invocation scoped to
  `ConfigValidateArgs`.** Push and Diff Args
  require `--adapter`, so a no-flag parser
  roundtrip would fail clap before any
  defaults could be observed. The default-
  values assertion now runs against Validate
  (which has no required flags) — the
  defaults on `--manifest` are shared across
  all three Args structs per §3.2.2, so the
  Validate-only assertion covers the
  contract.
- **Author field filled in.** Header field
  `Author:` no longer says `(TBD)`.

**Reviewer pass (round 18) — adapter-cap corrections +
contract reversals:**

- **Fastly cap corrected from 64 KiB → 8 000
  characters.** Reviewer cross-checked against the
  official Fastly Config Store item docs:
  `item_value` is capped at 8 000 characters, not
  64 KiB. The Q6 per-adapter cap table now states
  8 000 characters with a "round-18 reviewer caught
  this against the official docs" note; §12.10's
  "Fastly supports sizes well beyond Spin cap" claim
  is replaced with the actual relationship (a
  Spin-boundary blob ~95 KiB DOES exceed Fastly's
  cap). Added §12.3 Fastly-specific size-cap test
  asserting the boundary (8 000 OK, 8 001 errors via
  the writer-side guard). Fastly Q6 (a) gains a
  pre-platform guard sketch in the Fastly writer
  that errors before the platform call.
- **"Split across `[stores.config]` ids" workaround
  reworded as "restructure into separate typed
  structs".** The framework does NOT auto-split one
  `C` across stores —
  `run_config_push_typed::<C>` writes ONE blob per
  store, and §3.2 explicitly forbids multi-blob
  merge. Operators with too-large configs need to
  RESTRUCTURE their typed `C` into multiple separate
  types (one per logical surface, e.g.
  `BillingConfig` / `FeatureConfig`) and wire each
  through its own `[stores.config]` id with its own
  `AppConfig<...>` extractor. Updated the wording in
  Q6 (Fastly), Q12 (Spin Cloud), and §9.4 (Spin
  Cloud follow-up paths) accordingly.
- **Parent `Command::Config` `after_help` via the
  TUPLE variant, not a struct variant.** Round-17's
  sketch converted `Config(ConfigCmd)` to a struct
  variant `Config { cmd: ConfigCmd }`, which would
  force match-arm churn across every existing
  `Command::Config(...)` site. A round-18 Clap probe
  confirmed `#[command(subcommand, after_help = ...)]`
  on the TUPLE variant works identically — clap
  renders `after_help` on `edgezero config --help`
  and the `subcommand` attribute keeps the inner
  `ConfigCmd` dispatch intact. Sketch reverted to
  the tuple form.
- **Q3 missing-blob behaviour reversed from
  `Internal` to `ConfigOutOfDate`.** §6.3's
  ConfigOutOfDate rationale defines the class as "a
  re-run of `<app-cli> config push` fixes it" — and
  a missing blob is exactly that case. Mapping to
  `Internal` would page oncall (500-class signal)
  when the actionable response is "push the
  config", which `ConfigOutOfDate` (503 with
  `Retry-After: 60`) already encodes. The §3.3.3
  extractor sketch's `EdgeError::internal("missing
typed app-config blob")` call site changes to
  `EdgeError::config_out_of_date(...)` with an
  actionable message naming the key + the
  `<app-cli> config push` remediation. (c)
  (`MaybeAppConfig<C>` for endpoints with a
  sensible default) is still tracked as a
  follow-up. The §1 round-2 changelog reference
  updated to record the round-18 reversal.

**Reviewer pass (round 19) — §6.3 catch-up to the Q3
reversal:**

- **§6.3 missing-blob narrative + mapping table
  brought into line with §3.3.3 + Q3 (d).** The
  round-18 reversal updated the extractor sketch
  (§3.3.3) and the Q3 default to
  `EdgeError::ConfigOutOfDate`, but the §6.3
  narrative and the `ConfigStoreError → EdgeError`
  mapping table at §6.3 still showed the old
  "Key missing → `Internal`" rule. The round-19
  reviewer caught this divergence. Fixed:

  - The §6.3 "Key missing from the store" bullet
    now reads
    `EdgeError::ConfigOutOfDate` (HTTP 503,
    `Retry-After: 60`, "run `<app-cli> config
push`" message) and explicitly references Q3
    (d) and the §3.3.3 extractor sketch.
  - The mapping table replaces the stale
    `Internal (none) → Internal | 500` row with a
    "missing key (`Ok(None)` from `get`)" row
    mapping to `ConfigOutOfDate | 503`, with a
    Notes column clarifying that this isn't a
    `ConfigStoreError` variant — it's caught at
    the extractor's `ok_or_else` after
    `ConfigStore::get` returns `Ok(None)`. The
    note explicitly cites round-18 M-2 so a
    future reader sees the reversal.

**Reviewer pass (round 20) — remaining missing-blob
echo + Spin Cloud CLI surface accuracy:**

- **§5.2.1 extractor-consumption sketch caught up
  with the §6.3 / Q3 (d) rule.** Round 18 / 19
  updated §3.3.3 and §6.3 to map missing blobs to
  `ConfigOutOfDate`, but the SECONDARY sketch in
  §5.2.1 (illustrating
  `RequestContext::config_store_default_binding`
  consumption) still mapped `Ok(None)` to
  `EdgeError::internal("no blob at key ...")`. An
  implementer following that sketch would have
  produced a 500 where the spec wanted a 503.
  Sketch now uses
  `EdgeError::config_out_of_date(...)` with the
  same actionable message as §3.3.3.
- **§12.1 missing-key test made concrete.** Was
  "errors on missing key (per Q3 default)". Now
  pattern-matches on
  `EdgeError::ConfigOutOfDate { message,
field_path }`, asserts the message contains the
  literal `key \`<resolved-key>\``and the`run \`<app-cli> config push\``remediation, AND
renders the`Response`to assert HTTP 503 +`Retry-After: 60` header. Implementers can no
  longer pick the wrong variant.
- **§8.3 Spin Cloud read-back wording aligned with
  the Fermyon Cloud command reference.** Earlier
  text said `spin cloud key-value list` "enumerates
  keys" — per the reference, `list` enumerates
  STORES, not keys. Reworded to enumerate the full
  CLI surface (`create` / `delete` / `list` /
  `rename` / `set`) and clarify that `set` is the
  only per-key operation. The "no `get`" conclusion
  is unchanged.
- **§10.x Spin Cloud cleanup recipe rewritten.**
  Earlier draft had `spin cloud key-value delete
--app <APP> --label <LABEL> <KEY>` per leaf —
  that subcommand shape doesn't exist; `delete`
  removes a whole STORE. Replaced with three real
  options: (1) leave orphan leaves (the runtime
  only reads the `app_config` blob anyway),
  (2) `delete` + `create` the store and re-push
  (only safe if the store is config-dedicated),
  (3) Fermyon Cloud dashboard / HTTP API (out of
  v1 scope). Round-20 reviewer cross-checked
  against the Fermyon command reference.

**Reviewer pass (round 21) — canonicaliser pinned to
in-tree walker:**

- **Q1 resolved to (b): in-tree hand-rolled
  walker.** Round-21 reviewer probed
  `serde_canonical_json` v1.0.0 (the previously
  surveyed (a) candidate) and found it errors with
  `Floating point numbers are forbidden` on any
  finite float. §4.2's numeric-value rules
  explicitly require finite-`f64` support via
  `ryu`, so (a) cannot implement the spec unchanged.
  Q1 marked **resolved to (b)** and §4.2's
  "Stability across implementations" block
  rewritten to name the in-tree walker
  `crates/edgezero-core/src/canonical_form.rs` as
  the v1 default (no external crate name + version
  to pin). The walker depends only on `serde_json`
  - `ryu` + `sha2` — all in-tree already.
- **§13.1 acceptance gate simplified.** With Q1
  resolved, the gate no longer needs to accept
  EITHER an external dep OR an in-tree module —
  the in-tree module is the only valid form.
  Rewrote the gate to:
  1. Refuse the SHA hex placeholder
     (`…` / `fixed-hex-value`).
  2. Require the in-tree module file at the
     documented path.
  3. Fail loud if a future PR tries to revive
     `serde_canonical_json` in workspace
     `Cargo.toml` (defensive — the gate names
     the Q1 (b) resolution in the error message).
     The old "exact `=X.Y.Z` pin OR in-tree module"
     branching is gone; an external dep version pin
     no longer applies because there is no external
     dep.

**Reviewer pass (round 22) — migration scope +
implementability + phasing for plan-readiness:**

- **§10.3 manifest-schema migration scope corrected.**
  Earlier wording said the manifest schema was
  unaffected — but §5.2's hyphen-rejection rule IS a
  hard-cutoff manifest-schema change. §10.3 now
  explicitly carves out "manifest schema DOES change in
  one narrow way" and names the renamed sites
  operators must touch (`edgezero.toml` `[stores.*]`
  ids + matching `[adapters.<name>.*]` bindings).
  §10.2 gains a `grep -RnE` recipe for the
  implementing PR + operators to audit hyphenated
  store ids across in-tree manifests, scaffold
  templates, smoke scripts, and docs.
- **§3.3.3 extractor sketch deserialize call fixed.**
  Earlier sketch used
  `serde_path_to_error::Deserializer::new(data, &mut
track)` directly on a `serde_json::Value` — that's
  the wrong shape (the `Deserializer::new` constructor
  takes a `Deserializer`, not a `Value`). The §4.3
  prose already said the correct form
  (`Value::into_deserializer()` +
  `serde_path_to_error::deserialize`). Sketch
  rewritten to match: `use serde::de::IntoDeserializer
as _;` + `serde_path_to_error::deserialize(data.into_deserializer())`,
  matching what an implementer would actually write.
- **Q9 audit log resolved to (c) — out of v1 scope.**
  Earlier default was (a) "structured JSON line on
  push stdout", but §8.2 push flow, §12.2 tests, and
  §13 commit phasing carried no contract for the JSON
  shape, output order, skip-on-equal behaviour, or
  test assertions. Two options to resolve: pin the
  full contract end-to-end OR drop. Picking drop: the
  deploy-pipeline layer already has the data, the
  sha is on push stdout per §8.2's success line, and
  specifying a full audit-log contract is a separate
  work stream. (a) and (b) tracked as follow-ups for
  when a real audit requirement lands.
- \*\*§13 commit phasing regrouped for plan-readiness
  - bisect-friendliness.** Round-22 reviewer flagged
    that the earlier phasing split `--key`, `diff`, and
    read-back across separate later commits, making
    intermediate slices non-bisectable. Phasing
    rewritten into 7 commits where each one is annotated
    as **complete slice\*\* (builds + tests pass on that
    commit alone). Grouping changes: `ConfigStoreBinding`
  - `EnvConfig::store_key` + manifest-charset tightening
    land WITH the extractor's binding consumer (Commit B);
    `--key` push flag lands WITH `config push` rewrite
    (Commit E); inline-diff prompt lands with push
    (Commit E, reusing the read trait from Commit D).
    The standalone "`__KEY` + `config push --key`"
    commit from the round-21 phasing is gone — its two
    halves now live where they're load-bearing.
- **§8.4 dangling reference fixed.** Two callers
  pointed at a §8.4 that doesn't exist (the diff
  format docs live in §8.1.1 / §8.1.2 / §8.1.3).
  Both call sites updated to the real subsection
  numbers.

**Reviewer pass (round 23) — phasing honesty + response-
body test coverage:**

- **§13 phasing honors §10's atomic-cutoff rule.**
  Round 22 had marked every commit as a "complete
  slice", but §10.1's no-platform-bridge rule + §10.2's
  same-commit app-demo migration rule make
  intermediate cutover commits inherently
  non-bisectable: a tree with the new runtime but the
  OLD writer (or vice versa) can't run end-to-end, and
  the §10.2.1 grep gate would fail in any commit that
  moves the extractor without migrating app-demo
  handlers in the same commit. Round-23 reviewer
  flagged this directly.

  Phasing rewritten into five commits with explicit
  annotations:
  - **Complete slice** — bisect-safe.
  - **Cutover slice** — THE commit that flips the
    blob model live (new reader + new writer +
    app-demo migration + grep gate all land
    together).
  - **Pre-cutover infrastructure** — adds types /
    traits with no in-tree caller exercising them
    yet; behaviour unchanged from `main`.
  - **Post-cutover additive** — depends on the
    cutover commit; bisect-safe within the post-
    cutover range.

  Concrete: Commit A (canonical form), Commit B
  (bundle of pre-cutover infrastructure: binding +
  charset + `EnvConfig::store_key` + read trait —
  none called yet), **Commit C (THE cutover —
  extractor + push rewrite + app-demo migration +
  templates + grep gate in one commit)**, Commit D
  (`config diff` post-cutover), Commit E (docs).
  Removed the round-22 split of "read trait" and
  "config push" as separate commits — both fold
  into Commit C because each one alone leaves an
  unbisectable intermediate.

- **§12.6.1 added: `kind` strings + `field_path`
  presence + `Retry-After` policy tests across ALL
  `EdgeError` variants.** Round-22 test plan only
  covered the `ConfigOutOfDate` body shape, but
  §6.3.1 added the `kind: String` field to every
  variant's response. New §12.6.1 covers:
  - One fixture per variant asserts
    `body.error.kind` matches the documented
    string (table of eight variants).
  - `field_path` is asserted ABSENT in every
    non-`ConfigOutOfDate` body (per §6.3.1's
    "ONLY on `config_out_of_date`" rule).
  - `Retry-After: 60` asserted PRESENT on
    `ConfigOutOfDate` and ABSENT on
    `ServiceUnavailable` (per the §6.3.1 / round-10
    H-3 narrowing — audit found
    `ServiceUnavailable` is reused for several
    non-retryable cases, so the header would
    mislead clients).
  - HTTP status codes asserted per the documented
    variant → status mapping.

**Reviewer pass (round 24) — doc hygiene; spec cleared
for plan authoring:**

- **Stale commit counts in §13 + §1 changelog
  reconciled.** Round-23 collapsed the phasing from
  seven commits into five, but three call sites still
  said "ALL six" / "seven commits" / "Stages A-C / D-F":
  - §13 "land Stages A-C now, do D-F later" rewritten
    to "land Commits A-B now, do C-E later" (matches
    the five-commit shape).
  - §13 "PR cannot land without ALL six" → "ALL five
    (Commits A–E)" with the explicit lettering.
  - §1 round-2 changelog reference "one PR, seven
    commits" updated to "five commits (round-23
    collapse from the earlier seven-commit plan)" so
    readers walking the v1 changelog don't trip over
    the stale number.
- **Status header updated.** Was "v1 — Draft, pending
  first review"; the spec has had 23 reviewer passes
  and round 24 cleared it for plan authoring. Header
  now reads "v1 — Plan-ready (twenty-three reviewer
  passes complete; reviewer cleared for plan
  authoring at round 24)".

**Reviewer pass (round 26) — phasing carve-out for the
plan author:**

- **§10.2.2 `cli/src/main.rs.hbs` template phasing
  noted.** Plan author discovered that running the
  `generated_project_builds` test against the cutover
  commit (§13 Commit C) would fail if the template's
  `TypedConfigCmd` enum already declared `Diff` —
  because `ConfigDiffArgs` + `run_config_diff_typed`
  don't exist until §13 Commit D. The full target
  template shape is unchanged; the spec just adds an
  "Implementation phasing note" explaining that
  Commit C ships the template's `TypedConfigCmd` enum
  with `Push` + `Validate` only, and Commit D adds the
  `Diff` variant + dispatch in the same commit that
  ships the `Diff` arg struct + entry point. Net
  effect on the generated project is the same: a
  user-generated project from the merged PR has all
  three commands. Bisecting between Commit C and
  Commit D just sees the `Diff` arm appear when
  Commit D lands.

Stance from initial discussion: **no backward compatibility, no
migration aid.** Apps are responsible for their own schema
evolution (`#[serde(default)]`, struct versioning, etc.); the
platform does not bridge pre-blob and post-blob state. Operators
coordinate downtime / blue-green per their existing deploy
process. §10 is written under this assumption.

## 1. Goal

Replace the current key-by-key typed-config storage with a single-key
JSON blob.

The runtime reads ONE entry per request, deserialises it into the
app's typed struct, and exposes the struct through an `AppConfig<C>`
extractor. Pushing the config is one atomic write per environment.
Comparing local vs. remote state is a structural JSON diff. Drift
detection rides on an embedded SHA-256.

This is a **hard-cutoff redesign** of the typed app-config storage
layer. **There is no backward compatibility:** no dual-shape
parsing, no compat shims, no "if you see a flat leaf, treat it as
v0; if you see an envelope, treat it as v1" fallback. The runtime
recognises the blob envelope and nothing else.

Every in-tree consumer migrates as part of the work. That
explicitly includes:

- The reference **`app-demo`** (`examples/app-demo/`) — its
  `app-demo.toml`, its `AppDemoConfig` struct's serde attrs, its
  handler code that reads config, its `app-demo-cli config push`
  tests, AND its smoke scripts.
- The scaffold templates that generate new projects
  (`crates/edgezero-cli/src/templates/**`).
- All four adapters' integration tests + per-adapter contract
  tests.
- The CI workflow's `generated_project_builds` check.
- Every doc-block code example that mentions the typed config
  read pattern.

Concrete file list for the app-demo migration is in §10.2.

## 2. Motivation

The current model:

1. Reads the typed `<name>.toml` into a struct.
2. Flattens the struct into `(dotted_key, string_value)` pairs.
3. Writes each pair as a separate entry in the per-id config store.
4. The runtime reads keys individually via `ctx.config_store_default()?.get("foo.bar")`.

Pain points the blob model fixes:

- **Atomicity.** A multi-chunk push (Fastly, Spin Cloud) can land
  partially under failure — PR #269 round 3 already added a
  partial-failure diagnostic, but the underlying non-atomicity stays.
  A single-blob write either succeeds whole or fails whole; the next
  push retries the whole state.
- **Argv / size limits.** Per-leaf push has tripped argv-size caps
  (PR #269 F4) and forced `--stdin` plumbing. A single blob ends up
  smaller as a tarred JSON than as N separate `--key=…` argv tokens.
- **Read amplification.** Today's `Config` requires the handler to
  know the dotted keys to fetch. The struct deserialise happens
  out-of-band (or not at all). A blob gives the handler the typed
  struct directly — one lookup, one parse, one struct.
- **Diff-ability.** Per-leaf state has no natural "configuration
  version" to compare against. A blob has a sha; two shas are equal
  or not. The structural diff is well-defined on a single object.
- **Per-environment swap.** Swapping a whole environment (dev →
  staging) currently means writing every flattened leaf again under
  a different store name. With a blob, swapping is point the runtime
  at a different KEY inside the same store.

## 3. Scope

### 3.1 In scope

- New blob shape for the typed app-config, with embedded SHA-256.
- New `AppConfig<C>` extractor returning the typed struct, with a
  `named(key)` form for explicit per-request key override.
- New `<app-cli> config diff --adapter <name>` command (where
  `<app-cli>` is the downstream typed CLI generated by
  `edgezero new`, e.g. `app-demo-cli`). The push flow runs the
  diff inline by default.
- Runtime per-environment KEY override via a new env var.
- `<app-cli> config push --adapter <name>` rewrite: serialise the
  whole struct, compute sha, write under one key. Skip the write
  when the remote sha already matches.
- Per-adapter read-back implementation for the four adapters
  (axum / cloudflare / fastly / spin) so push and diff can both
  ask the store "what's currently there?".
- Per-adapter writer rewrite for the blob shape.
- App-demo migration to the new model.
- Migration guide doc + manifest-migration error pointers updated.

### 3.2 Out of scope

- **Schema evolution.** What happens when the operator pushes a
  blob whose shape doesn't match the runtime's typed struct after
  a code change. Today the deserialise fails loudly; we keep that.
  A future spec can add schema-version negotiation.
- **In-process caching.** `AppConfig<C>` re-fetches per request for v1
  (matches what `ctx.config_store_default()` already does). A
  follow-up can introduce a TTL'd cache layer once the blob model
  is settled.
- **Secret values inside the blob.** The blob never carries
  secret material. `#[secret]` field VALUES are
  operator-supplied secret-store KEY NAMES (non-sensitive
  metadata); `#[secret(store_ref)]` field VALUES are
  `[stores.secrets]` IDs (also non-sensitive). Both stay in
  the blob verbatim. The runtime `AppConfig<C>` extractor
  **resolves** each `#[secret]` field's value from the secret
  store BEFORE handing the typed struct to the handler — so
  `cfg.api_token` in handler code is the actual token, not
  the key name. The full model is in §3.3.
- **Auxiliary blobs per id.** The model is **one ACTIVE typed
  app-config blob key per environment, within a config store
  id**. The runtime reads exactly one key per id per request
  (resolved through the `__KEY` env override per §5.2). Storing
  multiple environment blobs as sibling keys under the same id
  (e.g. `app_config` + `app_config_staging` co-located in one
  KV namespace, with the runtime picking via `__KEY`) IS
  supported and called out in §5 and §9.1. What's out of scope
  is the SAME runtime reading multiple blobs from the same id
  in one request — there's no "merge two blobs into `cfg`"
  composition.
- **Push retry / resume.** Single-blob writes are atomic per
  shellout. Multi-chunk recovery from PR #269 round 3 stops being
  relevant for the typed config path.

### 3.2.1 Raw CLI path

Today the project ships TWO `config push` paths:
`run_config_push` (raw — flattens TOML, no type, no secret
strip) and `run_config_push_typed::<C>` (typed — uses the
project's `C` to validate, strips `#[secret]` fields,
flattens).

The blob model is **typed-only**:

- `run_config_push_typed::<C>` is rewritten to write the blob
  envelope per §4 + §3.3.
- `run_config_push` (the raw path on the bundled `edgezero`
  binary) is REMOVED. Same for `run_config_diff` — there's no
  raw diff.

Rationale (rewritten under Model A — the round-3 review pushed
back on the earlier "raw push would leak secret values"
framing, which doesn't hold because Model A keeps secret
KEY NAMES in the blob as non-sensitive metadata, NOT secret
values):

- **No `C` to canonicalise against.** The blob's sha is
  computed over the canonical form of `data` (§4.2), and the
  canonical form's "type identity" rule depends on the typed
  Rust struct (a TOML `1500` parsed as `i64` hashes
  differently than as `f64`). The raw path has no `C` to
  resolve type ambiguity, so two raw pushes of the same TOML
  could produce different shas across builds — breaking
  skip-on-equal and drift detection.
- **No validation envelope.** Push routes through
  `validate_excluding_secrets` (§3.3.8). The raw path has no
  `Validate` impl to call. Skipping validation entirely
  would land malformed config that the runtime extractor
  then 503's on at first request — the exact "deploy is
  incomplete" surface §6.3 tries to AVOID through push-time
  catches.
- **No envelope contract.** The blob envelope's `sha256` /
  `version` / `generated_at` fields are derived from the
  serialised `data`. The raw push could compute them only by
  treating `<name>.toml` as opaque JSON — which loses the
  typed-struct guarantees other layers depend on.
- **The secret-key-name argument is real but secondary.**
  The raw path can't distinguish `#[secret]` fields from
  plain ones, so it can't run the right validator-skip
  policy (§3.3.8). Even though the values themselves
  aren't secret material, the wrong validation discipline
  would either accept too much or reject too much at push
  time.

Net: the bundled binary loses `config push` / `config diff`
because they need `C`. `config validate` stays (raw flow
validates the manifest + syntactic shape without needing
`C`).

Downstream effects:

- The bundled `edgezero` binary no longer exposes `edgezero
config push` or `edgezero config diff`. The subcommands are
  removed AND replaced with stub subcommands whose entire
  body is a clap-level error message pointing at the typed
  path on the generated CLI:

  The exact text is the single `STUB_POINTER_AFTER_HELP`
  constant defined in §3.2.2 (one definition, shared
  across the match-arm `eprintln!`, the per-variant
  `after_help` on `Push` / `Diff`, AND the parent-level
  `after_help` on `Command::Config`). Reproduced here for
  reference:

  ```text
  $ edgezero config push --adapter axum
  This command requires a typed app-config struct (`C`) and runs
  from your generated downstream CLI, not the bundled `edgezero`
  binary. Run `<your-app>-cli config push` (or `... diff`) instead.
  See `<your-app>-cli config push --help`.
  ```

  §12.8 asserts the text appears byte-for-byte across all
  three invocation surfaces (bare, with-flags, `--help`)
  AND on the parent `edgezero config --help`. The
  examples in this doc reference `STUB_POINTER_AFTER_HELP`
  to keep them in sync; do NOT hand-edit the rendered
  example above without updating the constant in §3.2.2.

  Concretely the bundled `ConfigCmd` enum keeps `Push` and
  `Diff` variants (clap doesn't see them as removed), but the
  match arm prints the pointer message and exits with code 2.
  See §3.2.2 for the enum shape — the bundled `ConfigCmd`
  variants list explicitly carries `Push` and `Diff` as
  stub-pointer arms even though only `Validate` does real work.

- Generated downstream CLIs (`my-app-cli config push`, `my-app-
cli config diff`) are the only entry points. The scaffold
  templates already wire these via `run_config_push_typed::<C>`
  / `run_config_validate_typed::<C>`; the diff command's typed
  variant gets wired the same way.
- `config validate` STAYS on both paths. The raw flow can still
  validate the manifest + the syntactic shape of `<name>.toml`
  (key syntax, etc.) without needing `C`. It's the push / diff
  paths that require `C`.

#### 3.2.2 Concrete command-enum split

Today `crates/edgezero-cli/src/args.rs` exposes a shared
`Command::Config(ConfigCmd)` enum with `ConfigCmd::Push(...)`

- `ConfigCmd::Validate(...)` (line 41) used by BOTH the
  bundled `edgezero` binary AND generated downstream CLIs (the
  template's `main.rs.hbs:56` mirrors it). The blob model splits
  this:

* **`ConfigCmd` (bundled binary) carries `Validate` plus
  stub-pointer `Push` and `Diff` variants.** Clap parses
  them so `edgezero config push --help` lists the
  subcommands (otherwise operators get the confusing
  "unrecognized subcommand" message and don't know where to
  look). The match arms for `Push` / `Diff` print the
  pointer message documented in §3.2.1 and exit with code 2. `Validate` does the real work via the raw flow.
* **`TypedConfigCmd` (new, exported) gains `Push`, `Diff`,
  `Validate`.** The arg structs (`ConfigPushArgs`,
  `ConfigValidateArgs`, new `ConfigDiffArgs`) STAY exported
  from `edgezero_cli::args` so downstream CLIs reuse them
  verbatim. The new enum is what generated CLIs put on their
  own `Command::Config(...)` arm.
* **Generated CLI templates use `TypedConfigCmd`.** The
  scaffold `main.rs.hbs` `match Command::Config(...)` arm
  expands to handle `Push` (via `run_config_push_typed::<C>`),
  `Diff` (via `run_config_diff_typed::<C>`), and `Validate`
  (via `run_config_validate_typed::<C>`).

Exported surface (full flag list, hard-cutoff — supersedes
every earlier "unchanged" or partial sketch in this doc).
The flag names match the existing canonical CLI surface at
`crates/edgezero-cli/src/args.rs:179` (already shipped in
PR #269), with the new blob-model flags `--key`, `--yes`,
`--no-diff` added. There is no `--config` / `--app-config`
mismatch and no missing `--store` / `--no-env`:

```rust
// edgezero_cli::args

#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigPushArgs {
    /// Target adapter name (axum / cloudflare / fastly / spin).
    #[arg(long, required = true)]
    pub adapter: String,

    /// Path to the typed app-config file
    /// (default: `<app_name>.toml` next to the manifest).
    #[arg(long)]
    pub app_config: Option<PathBuf>,

    /// Path to the manifest. Default: `edgezero.toml`.
    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,

    /// Logical config store id to push to. Defaults to
    /// `[stores.config].default` (or the only declared id when
    /// `[stores.config].ids` has length 1). Already shipped
    /// at `args.rs:222`.
    #[arg(long)]
    pub store: Option<String>,

    /// Override the default key — §5.4. Default is the
    /// logical store id (e.g. `app_config`).
    #[arg(long)]
    pub key: Option<String>,

    /// Skip the `<APP_NAME>__…__<KEY>` env-var overlay when
    /// loading the typed app-config. Default: overlay ON, so
    /// push sees the same resolved values the runtime sees.
    /// Already shipped at `args.rs:208`.
    #[arg(long)]
    pub no_env: bool,

    /// Push to the adapter's LOCAL-emulator state instead of
    /// the live platform. Already shipped at `args.rs:200`;
    /// behavior per adapter documented in the field's
    /// existing doc-comment.
    #[arg(long)]
    pub local: bool,

    /// Spin runtime-config file (currently Spin-only) — see
    /// §9.4 + `args.rs:217`.
    #[arg(long)]
    pub runtime_config: Option<PathBuf>,

    /// Skip the inline diff prompt and write unconditionally.
    /// §8.2 non-TTY consent rule + §8.3 Spin Cloud
    /// four-branch UX use this flag.
    #[arg(long, short)]
    pub yes: bool,

    /// Skip the inline diff render — used by automation that
    /// already rendered the diff via `config diff` and only
    /// wants the write side-effect.
    #[arg(long)]
    pub no_diff: bool,

    /// Dry-run: render the diff, do NOT write. Exit 0 on
    /// a clean dry-run (manifest + local TOML parse OK,
    /// remote read OK, diff rendered). Exit NON-ZERO on
    /// any error encountered along the way — manifest /
    /// local-TOML parse failure, remote-read failure, OR
    /// the "remote read-back unsupported" case (Spin Cloud
    /// per §9.4: `--dry-run` exits non-zero with an
    /// actionable message since the diff is structurally
    /// impossible). The flag's contract is "no write,
    /// show the comparison"; if the comparison can't be
    /// produced honestly, the command refuses rather
    /// than printing a half-truth. Already shipped at
    /// `args.rs:188` (the round-12 update narrows the
    /// "always exit 0" wording to "exit 0 on a clean
    /// dry-run, non-zero on any blocking error").
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigDiffArgs {
    #[arg(long, required = true)]
    pub adapter: String,

    #[arg(long)]
    pub app_config: Option<PathBuf>,

    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,

    /// Logical config store id (see ConfigPushArgs::store).
    #[arg(long)]
    pub store: Option<String>,

    /// Override the default key — §5.4.
    #[arg(long)]
    pub key: Option<String>,

    /// Skip the env overlay during load (see
    /// ConfigPushArgs::no_env).
    #[arg(long)]
    pub no_env: bool,

    /// Diff against the LOCAL `.edgezero/local-config-<id>.json`
    /// state instead of the remote — used in offline / CI
    /// pre-push runs. Default is remote per §8.1. See Q7.
    #[arg(long)]
    pub local: bool,

    /// Spin-only runtime config (§9.4).
    #[arg(long)]
    pub runtime_config: Option<PathBuf>,

    /// Output format — `unified` (default), `structured`, `json`.
    /// See §8.1.1 / §8.1.2 / §8.1.3 for the format
    /// specifications.
    #[arg(long, default_value = "unified")]
    pub format: String,

    /// CI-mode "diff-present" exit-code semantics.
    /// With `--exit-code`, the command exits 0 when
    /// local == remote and 1 when there's a diff to
    /// show. WITHOUT `--exit-code`, both outcomes exit 0
    /// and only the printed diff distinguishes them. In
    /// EITHER mode, real errors (manifest load failure,
    /// remote-read network failure, schema mismatch,
    /// etc.) ALWAYS exit non-zero (≥2) — `--exit-code`
    /// only changes the "diff present" success branch;
    /// it does not mask errors. See Q10.
    #[arg(long)]
    pub exit_code: bool,
}

#[derive(clap::Args, Debug)]
#[non_exhaustive]
pub struct ConfigValidateArgs {
    #[arg(long)]
    pub app_config: Option<PathBuf>,

    #[arg(long, default_value = "edgezero.toml")]
    pub manifest: PathBuf,

    /// Skip the env overlay during load.
    #[arg(long)]
    pub no_env: bool,

    /// Strict mode — see §3.3.2 (structural secret-metadata
    /// checks). Honored by BOTH bundled raw `config validate`
    /// (manifest-level checks only — no `C` to walk) AND
    /// the generated-CLI `config validate` (full typed
    /// validation + structural secret checks). Today's raw
    /// implementation at `crates/edgezero-cli/src/config.rs:503`
    /// already forwards `--strict` into shared checks; the
    /// blob model preserves that.
    #[arg(long)]
    pub strict: bool,
}

// Bundled binary's enum. Push/Diff stay as STUB-POINTER
// TUPLE variants carrying a hidden catch-all args struct
// (NOT `ConfigPushArgs` / `ConfigDiffArgs`, and NOT unit
// variants). The two important properties are:
//
//   - The catch-all absorbs whatever clap sees on the
//     command line — including `--adapter axum`, `--key
//     staging`, positional tokens, `--`-trailing args —
//     so clap dispatches to the match arm rather than
//     erroring on unrecognised flags. The match arm
//     then prints the §3.2.1 pointer and exits 2.
//   - `#[command(after_help = "...")]` on each tuple
//     variant carries the SAME pointer string for the
//     explicit `--help` path (clap renders `after_help`
//     on `--help` output but NOT on parse errors per
//     the round-14 Clap 4.6.x probe — so the catch-all
//     is what saves the with-flags case; `after_help`
//     just covers the help-text rendering).
//
// Earlier draft framing called these "UNIT variants" —
// round-16 reviewer flagged the contradiction with the
// `Push(ConfigCmdStubArgs)` declaration below. The
// correct framing is: stub-pointer TUPLE variants whose
// tuple element is the hidden catch-all (NOT the real
// `ConfigPushArgs` / `ConfigDiffArgs` from the typed
// generator).
#[derive(clap::Args, Debug)]
pub struct ConfigCmdStubArgs {
    /// Sink for any flags / args the operator passed. The
    /// stub does not inspect this; it exists so clap
    /// dispatches to the match arm instead of erroring.
    ///
    /// `hide = true` keeps the catch-all out of
    /// `--help` output. Without it, a round-15 Clap
    /// probe confirmed `push --help` renders
    /// `Usage: ... [TRAILING]...` plus an
    /// `Arguments: [TRAILING]...` section, which leaks
    /// the implementation detail. `hide` does NOT
    /// disable parsing — operator-supplied flags still
    /// go through the catch-all and dispatch to the
    /// match arm. §12.8 asserts both: that `--help`
    /// shows the pointer text but does NOT show
    /// `[TRAILING]` anywhere, and that
    /// `config push --adapter axum` still dispatches
    /// to the match arm.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        hide = true
    )]
    pub trailing: Vec<String>,
}

// SINGLE source of truth for the pointer string. All
// four call sites (the match-arm `eprintln!`, both
// per-variant `after_help` attributes, AND the parent-
// level `after_help` on `Command::Config`) reference
// this constant directly so §12.8's byte-for-byte
// equality assertion holds across every surface,
// including `edgezero config --help`.
const STUB_POINTER_AFTER_HELP: &str = "\
This command requires a typed app-config struct (`C`) \
and runs from your generated downstream CLI, not the \
bundled `edgezero` binary. Run `<your-app>-cli config \
push` (or `... diff`) instead. See \
`<your-app>-cli config push --help`.";

pub enum ConfigCmd {
    /// Stub: prints the §3.2.1 pointer at the typed CLI,
    /// exits with code 2. The hidden catch-all
    /// `ConfigCmdStubArgs` ensures the match arm runs
    /// regardless of what flags clap sees.
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Push(ConfigCmdStubArgs),
    /// Stub: same shape as `Push`.
    #[command(after_help = STUB_POINTER_AFTER_HELP)]
    Diff(ConfigCmdStubArgs),
    Validate(ConfigValidateArgs),
}

// The PARENT command also carries the pointer via
// `after_help`. A round-17 Clap probe confirmed that
// `after_help` attached to child variants does NOT
// render on `edgezero config --help` — clap renders the
// child `after_help` only when that specific child's
// `--help` is invoked. To make `edgezero config --help`
// also surface the pointer, attach `after_help` AND
// `subcommand` together on the existing TUPLE variant
// declaration of `Config(ConfigCmd)` in the top-level
// `Command` enum:
//
//   #[derive(clap::Subcommand)]
//   pub enum Command {
//       // ... other subcommands ...
//       #[command(subcommand, after_help = STUB_POINTER_AFTER_HELP)]
//       Config(ConfigCmd),
//   }
//
// Round-18 M-1 caught an earlier sketch that converted
// `Config(ConfigCmd)` into a struct variant
// `Config { cmd: ConfigCmd }` — that shape change
// would force match-arm churn across every existing
// `Command::Config(...)` site. A round-18 Clap probe
// confirmed `#[command(subcommand, after_help = ...)]`
// on the TUPLE variant works the same: clap renders
// `after_help` on `edgezero config --help`, and the
// `subcommand` attribute keeps the inner `ConfigCmd`
// dispatch intact. Keep the tuple form.

// Implementation note: the catch-all carries
// `trailing_var_arg = true` and `allow_hyphen_values =
// true`, which together tell clap to put every remaining
// argv token (including `--adapter` and its value) into
// the `trailing` Vec without trying to match them against
// declared options. This is the same idiom clap's docs
// recommend for "pass-through" CLIs.
//
// Three invocation surfaces, all covered:
//
//   1. Bare `edgezero config push` — match arm runs;
//      stub prints the pointer; exit 2.
//   2. `edgezero config push --adapter axum` — match arm
//      ALSO runs (catch-all absorbs the flag); stub
//      prints the same pointer; exit 2.
//   3. `edgezero config push --help` — clap renders its
//      own help output with the `after_help` pointer
//      text appended (the stub's `#[command(after_help =
//      "...")]` covers the explicit `--help` path that
//      bypasses the match arm).
//
// All three surfaces show the same pointer text. §12.8
// tests all three.

// Generated-CLI enum (Push + Diff + Validate):
pub enum TypedConfigCmd {
    Push(ConfigPushArgs),
    Diff(ConfigDiffArgs),
    Validate(ConfigValidateArgs),
}
```

This lets the bundled binary keep `config validate` (it works
on the raw flow) without dragging in Push/Diff that can't work
without `C`. Downstream CLIs reuse the same `Args` structs the
bundled binary historically defined; only the ENUM the parser
matches on differs.

Flag-surface invariants the rest of the spec depends on:

- `--app-config` exists on Push / Diff / Validate (the
  typed config file path — name MATCHES the existing
  shipped flag, NOT `--config`).
- `--store` exists on Push and Diff (the logical config
  store id from `[stores.config].ids`; without it, the
  multi-store config model cannot route to non-default
  stores).
- `--no-env` exists on Push / Diff / Validate (the env
  overlay toggle from `AppConfigLoadOptions`).
- `--key` exists on Push and Diff (§5.4 push-side override
  example uses it; §9.x diff command examples use it).
- `--yes` exists on Push (§8.2 + §8.3's Spin Cloud
  four-branch UX).
- `--no-diff` exists on Push (§8.1 push-with-diff workflow).
- `--local` exists on Push (per `args.rs:200`'s shipped
  doc-comment — per-adapter local-emulator targeting) AND
  on Diff (§9.0 read trait picks remote vs local read-back
  by this flag).
- `--runtime-config` exists on Push and Diff (Spin Cloud
  read-back per §9.4 needs the path to the runtime config
  file that has `KEY_VALUE_STORE_TOKEN`).
- `--format` exists on Diff (unified / structured / json
  per §8.1.1 / §8.1.2 / §8.1.3).
- `--exit-code` exists on Diff (CI mode — see Q10).
- `--dry-run` exists on Push (preview, no write).
- `--strict` exists on Validate (raw + typed both honor it
  — see §3.3.2 / M-1 paragraph above).

### 3.3 Secret-field model — framework-resolved

The blob never carries secret values. The `<app-cli> config
push` writer persists the operator's TOML literally (which
holds non-sensitive METADATA — a secret-store key NAME or a
store ID), and the runtime `AppConfig<C>` extractor RESOLVES
each `#[secret]` field's value from the secret store before
deserialising into `C`. Handlers receive a complete typed
config with secret values populated, not raw key names.

#### 3.3.1 What the attributes mean

The `#[derive(AppConfig)]` macro (already emits `SECRET_FIELDS`
metadata in `crates/edgezero-macros/src/app_config.rs`)
recognises three attribute forms. All three appear on `String`
fields in the typed struct; their VALUES at the operator's
TOML are non-sensitive metadata, distinct from the runtime
VALUES the handler sees through `cfg.<field>`:

| Attribute                                      | TOML value            | Runtime value                          |
| ---------------------------------------------- | --------------------- | -------------------------------------- |
| `#[secret] field: String`                      | secret-store KEY NAME | resolved secret VALUE                  |
| `#[secret(store_ref = "vault")] field: String` | secret-store KEY NAME | resolved secret VALUE from `cfg.vault` |
| `#[secret(store_ref)] vault: String`           | `[stores.secrets].id` | same id (unchanged)                    |

In app-demo today:

- `#[secret] pub api_token: String` with TOML value
  `"demo_api_token"` → at runtime, `cfg.api_token` is the
  resolved secret value (the actual token bytes), looked up in
  the default secret store under key `"demo_api_token"`.
- `#[secret(store_ref)] pub vault: String = "default"` → at
  runtime, `cfg.vault == "default"` (the store id, unchanged).
  This field is the store-id POINTER; the extractor never
  swaps it.

The two-form distinction matters because `#[secret]` fields
need a STORE to resolve from. By default, that's the default
secret store. If the operator wants a specific one, they use
`#[secret(store_ref = "vault")]` and the extractor reads
`cfg.vault` (the value of the store-ref field) to pick the
store.

#### 3.3.1.1 Exact macro / metadata API

Today the macro (`crates/edgezero-macros/src/app_config.rs`)
accepts `#[secret]` and `#[secret(store_ref)]` only and emits:

```rust
// edgezero_core::app_config (current)
pub struct SecretField {
    pub name: &'static str,
    pub kind: SecretKind,
}
pub enum SecretKind {
    KeyInDefault,
    StoreRef,
}
```

Model A introduces a third attribute form
`#[secret(store_ref = "vault")]` and needs the metadata to
carry a pointer to the store-ref field. The new shape:

```rust
// edgezero_core::app_config (new)
pub struct SecretField {
    /// Rust field identifier verbatim — the macro rejects
    /// containers with `#[serde(rename_all)]` so the
    /// serde-emitted JSON key matches this name 1:1. (Per-
    /// field `#[serde(rename = "...")]` is also rejected on
    /// `#[secret]` fields; see "Derive-time validation"
    /// below.)
    pub name: &'static str,
    pub kind: SecretKind,
}

pub enum SecretKind {
    /// `#[secret] field: String` — resolve from the default
    /// secret store. Field value at rest is the key NAME;
    /// at runtime it's the resolved value.
    KeyInDefault,

    /// `#[secret(store_ref)] field: String` — pointer to a
    /// `[stores.secrets].id`. Carried through to the runtime
    /// `cfg` field UNCHANGED (the value IS the store id).
    /// The extractor does NOT swap this field.
    StoreRef,

    /// `#[secret(store_ref = "vault")] field: String` —
    /// resolve from the secret store named by `cfg.vault`
    /// (which must itself carry `SecretKind::StoreRef`).
    /// Field value at rest is the key NAME; at runtime it's
    /// the resolved value.
    KeyInNamedStore { store_ref_field: &'static str },
}
```

The single addition is `SecretKind::KeyInNamedStore` carrying
the source-Rust-identifier of the sibling `StoreRef` field.

#### 3.3.1.2 Field path resolution

The metadata's `name` is a flat Rust field identifier — the
macro emits `"api_token"`, not `"feature.api_token"`. Nested
typed structs ARE supported by the deserialise step, but
`#[secret]` annotations on a nested struct's field would
require nested-path metadata that v1 does not implement.

**v1 stance: `#[secret]` annotations are only valid on
TOP-LEVEL `String` fields of the `#[derive(AppConfig)]`
struct.** Two enforcement layers, in order:

1. **Macro-time, intra-struct** (already enforced at
   `crates/edgezero-macros/src/app_config.rs:140` via
   `enforce_scalar_string_type`): `#[secret]` on a
   non-`String` field — including a nested struct field
   like `feature: FeatureConfig` — is a compile error.
   This covers the "operator annotates a nested field
   directly on the parent struct" case.
2. **The nested-derive case is NOT auto-enforceable.** A
   nested type `FeatureConfig` can independently derive
   `AppConfig` and carry its own `SECRET_FIELDS`. When the
   parent `AppDemoConfig` embeds `FeatureConfig`, the
   parent's `SECRET_FIELDS` only lists the parent's
   top-level `#[secret]` fields — `FeatureConfig`'s
   secrets are silently treated as plain blob values by
   the extractor's swap walk. Detecting this at the
   parent derive requires either a negative trait impl
   (unstable) or whole-program reflection (out of scope).

   **v1 rule, enforced by review + CI:** a type that
   derives `AppConfig` MUST NOT be used as a field type
   in another `AppConfig`-derived struct. The §10.2.1 CI
   gate adds a fourth pattern (see below) that asserts
   no struct field whose type is a known `AppConfig`
   root is embedded in another `AppConfig` root.
   Concretely:

   - The derive emits `impl AppConfigRoot for C {}`
     against a PUBLIC, UNSEALED marker trait
     `edgezero_core::app_config::AppConfigRoot`. The
     trait is NOT sealed because the derive macro
     expands in downstream user crates — a sealed trait
     could only be implemented from inside
     `edgezero-core`, which would break downstream
     `#[derive(AppConfig)]`. The marker is "open" and
     exists solely as a discoverable signal for the
     CI gate and for human readers; it carries no
     methods and is intentionally not bound by other
     traits in the framework, so its open-ness has zero
     runtime semantic effect.
   - The CI gate uses a syn-based Rust helper to walk
     the AST of every `.rs` file in the in-tree scope
     (an awk-windowing approach was tried in an earlier
     draft but couldn't handle multi-line derives or
     generic-wrapped field types — `Option<C>`,
     `Vec<C>`, `Box<C>`, tuple fields, array fields —
     and the round-13 reviewer correctly insisted on
     real AST awareness). The helper lives at
     `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
     behind a default-OFF `nested-app-config-check`
     feature; templates (`.rs.hbs`) are NOT parsed as
     raw Rust (they contain unrendered Handlebars) but
     are covered by a separate
     rendered-template fixture pass at
     `crates/edgezero-cli/tests/scaffold_render.rs`.
     See §10.2.1 Pattern 4 + the
     "rendered-template coverage" block below it for
     the exact implementation.
   - The macro additionally emits a doc-comment on the
     `AppConfigRoot` impl pointing at this constraint
     and the §10.2.1 gate command, so a future
     contributor adding a nested derive sees the
     constraint at impl site.

Implementation note: the extractor's secret walk indexes the
blob's `data` JSON object at `data[field.name]`. A top-level
key only. No path traversal needed in v1. The recursive-
metadata path is recorded as a v2 follow-up in §11 (Q13).

#### 3.3.1.3 Serde rename policy

The macro already rejects `#[serde(rename_all = ...)]` on
structs with `#[secret]` fields (the round-3 reviewer's note
on `app_config.rs:221`); extended for Model A to also reject
per-field `#[serde(rename = "...")]` on `#[secret]` /
`#[secret(store_ref)]` / `#[secret(store_ref = "...")]`
fields specifically.

Rationale: `SecretField::name` is the Rust identifier (e.g.
`api_token`). The blob's JSON `data` field is the
serde-serialised form of `C`. If the operator wrote
`#[serde(rename = "apiToken")] #[secret] api_token: String`,
the blob would carry `data.apiToken = "demo_api_token"` but
the extractor would look up `data.api_token` (the Rust
identifier) and find nothing. Two ways to fix:

- (a) Make the metadata carry the serde-renamed name too. The
  macro would inspect `#[serde(rename)]` attributes.
- (b) Reject renames on secret fields and force the Rust
  identifier to match the JSON key.

Picking (b) is consistent with the round-3 container-rename
rejection and keeps the metadata small. The error message
points at the macro span:

```text
error: `#[secret*]` field `api_token` cannot also carry
       `#[serde(rename = "apiToken")]`: SECRET_FIELDS uses
       the Rust identifier `api_token` to index the blob
       JSON, so a rename would desync the resolution lookup.
       Drop the rename, or drop the secret annotation.
```

Non-secret fields can rename freely under the
SECRET-field rename rule alone — but see §4.2's serde
shape constraints below: `#[serde(skip_serializing)]`,
`#[serde(skip_serializing_if = "...")]`, and
`#[serde(flatten)]` are rejected on EVERY field of an
`AppConfig`-derived struct (secret OR non-secret),
because the canonical-form rule needs the serde JSON
output to match the Rust field tree. The exact policy
is consolidated in the table below.

#### 3.3.1.4 Derive-time validation — authoritative table

This is the SINGLE-SOURCE-OF-TRUTH list of macro-time
checks. Every rule named elsewhere in the spec must
appear here; if it's not in this table, the macro does
NOT enforce it.

| Check                                                                         | Scope                            | Action                        | When introduced                                              |
| ----------------------------------------------------------------------------- | -------------------------------- | ----------------------------- | ------------------------------------------------------------ |
| `#[secret]` arguments are `[]` or `(store_ref)`                               | per `#[secret]` site             | compile_error if other        | existing                                                     |
| `#[secret(...)]` only on `String` fields                                      | per `#[secret]` site             | compile_error if other type   | existing                                                     |
| `#[serde(rename_all)]` on container, IF the struct has any secret field       | container                        | compile_error                 | existing (per `crates/edgezero-macros/src/app_config.rs:55`) |
| `#[secret(store_ref = "field")]` accepted                                     | per `#[secret]` site             | parsed into `KeyInNamedStore` | **new (v1)**                                                 |
| `#[secret(store_ref = "field")]` names a sibling field                        | per `#[secret]` site             | compile_error if not found    | **new (v1)**                                                 |
| Named sibling has `#[secret(store_ref)]` annotation                           | per `#[secret]` site             | compile_error if not          | **new (v1)**                                                 |
| Named sibling is `String`                                                     | per `#[secret]` site             | compile_error if not          | **new (v1)**                                                 |
| `#[serde(rename)]` on any `#[secret*]` field                                  | per field                        | compile_error                 | **new (v1)**                                                 |
| `#[secret*]` annotations on nested struct fields                              | per field                        | compile_error                 | **new (v1)**                                                 |
| `#[serde(skip_serializing)]` on ANY field of an `AppConfig` struct            | per field (secret OR non-secret) | compile_error                 | **new (v1, round-10 H-2)**                                   |
| `#[serde(skip_serializing_if = "...")]` on ANY field of an `AppConfig` struct | per field                        | compile_error                 | **new (v1, round-10 H-2)**                                   |
| `#[serde(flatten)]` on ANY field of an `AppConfig` struct                     | per field                        | compile_error                 | **new (v1, round-10 H-2)**                                   |

Notes on apparent contradictions resolved here:

- **Container `#[serde(rename_all)]`** is rejected ONLY
  when the struct has at least one `#[secret*]` field
  (the existing macro at
  `crates/edgezero-macros/src/app_config.rs:55`). A
  struct with no secret fields can use
  `#[serde(rename_all)]` freely — the canonical-form
  rule keys off the serde JSON output regardless of the
  Rust identifier, so renaming doesn't break the SHA.
- **Per-field `#[serde(rename = "...")]`** is rejected
  on secret-bearing fields ONLY (the rename-on-secret
  rule above). Non-secret fields can rename freely —
  same justification as the container case.
- **`#[serde(skip*)]` and `#[serde(flatten)]`** are
  rejected on EVERY field, secret or not, because they
  break the JSON-shape-matches-Rust-field-tree invariant
  the canonical form (§4.2) depends on for the
  `None → null` rule and stable key ordering. This is
  the round-10 H-2 finding; earlier drafts had this
  rule in §4.2 only, which the round-11 reviewer
  correctly flagged as a contradiction.

Combined the cross-field check is: `#[secret(store_ref =
"vault")] api_token: String` ⟹ the same struct must declare
`#[secret(store_ref)] vault: String` at the top level. The
macro inspects sibling fields in the same `proc_macro2::TokenStream`
pass; one struct can refer to its own fields' attributes.

#### 3.3.2 What this means for the writer

The current blanket-strip in
`crates/edgezero-cli/src/config.rs::flatten_typed_app_config`
(line 391) is **reverted**. Both `#[secret]` and
`#[secret(store_ref)]` fields are kept verbatim in the blob,
along with every other field. Push has no special handling for
either attribute — the blob carries operator-supplied metadata
(key names + store ids), never resolved secret values.

The attributes still carry meaning for **`<app-cli> config
validate`**:

- `#[secret] field: String` declares that `field`'s value
  names a secret-store key. The typed validator checks that
  `[stores.secrets]` is declared in the manifest (today's
  behaviour in
  `crates/edgezero-cli/src/config.rs::validate_typed_secrets`).
  Whether the named secret EXISTS in the store is NOT
  probed at validate time — that's a request-time concern
  (§6.3) because most adapters don't expose a "secret
  exists" query, and the ones that do (Fastly) would require
  authenticated cloud calls just to run validate.
- `#[secret(store_ref)] field: String` declares that
  `field`'s value names a `[stores.secrets].id`. The typed
  validator rejects if the named id is not declared.
- `#[secret(store_ref = "name")] field: String` declares
  that `field`'s value names a secret-store key AND that the
  store should be picked by reading `cfg.name`. The typed
  validator verifies the named field exists, is a String,
  and itself carries `#[secret(store_ref)]`.

**Adapter-side typed-secret checks include
`KeyInNamedStore` keys.** Today
`run_adapter_typed_checks` in
`crates/edgezero-cli/src/config.rs:611` only forwards
`KeyInDefault` field VALUES to each adapter's
`validate_typed_secrets` (so Spin's flat-namespace
collision check sees the default-store keys), and the
trait doc at
`crates/edgezero-adapter/src/registry.rs:379` matches.
Under Model A's third attribute form, a
`KeyInNamedStore` field's value ALSO becomes a Spin
variable name when the named store is the Spin one — the
same flat-namespace constraint applies. The check
extends as follows:

- The walk visits both `KeyInDefault` and
  `KeyInNamedStore` entries. For each
  `KeyInNamedStore { store_ref_field }`, the walker
  resolves the sibling `store_ref_field`'s value (the
  named store id) and groups the key under THAT id.
  `KeyInDefault` keys stay grouped under the default
  store id.
- `Adapter::validate_typed_secrets`'s parameter
  changes to a slice of a NEW NAMED STRUCT
  `TypedSecretEntry` (one shape, no tuple drift across
  adapter impls):

  ```rust
  // crates/edgezero-adapter/src/registry.rs (new public type)

  /// Per-secret-key entry passed to
  /// `Adapter::validate_typed_secrets`. Replaces the
  /// earlier `Vec<(&str, &str)>` tuple.
  ///
  /// `#[non_exhaustive]` so future v2 additions
  /// (e.g. a `kind: SecretKind` field) are source-
  /// compatible. Construction from external crates
  /// (notably `edgezero-cli`, which assembles the
  /// slice in `run_adapter_typed_checks`) goes through
  /// the inherent `new(...)` constructor below —
  /// `#[non_exhaustive]` blocks struct-literal
  /// construction from outside the defining crate, and
  /// `edgezero-cli` is the construction site.
  #[non_exhaustive]
  pub struct TypedSecretEntry<'a> {
      /// Logical secret-store id this key targets.
      /// `KeyInDefault` entries carry the default store
      /// id (resolved from `[stores.secrets].default` or
      /// the sole-id auto-default per
      /// `StoreDeclaration::default_id()`);
      /// `KeyInNamedStore` entries carry the resolved
      /// value of the sibling `StoreRef` field.
      pub store_id: &'a str,
      /// The Rust struct field name (e.g. `api_token`).
      pub field_name: &'a str,
      /// The blob value (i.e. the secret-store KEY NAME).
      pub key_value: &'a str,
  }

  impl<'a> TypedSecretEntry<'a> {
      /// Construct a `TypedSecretEntry`. This is the only
      /// way external crates (e.g. `edgezero-cli`) can
      /// build instances, since the struct is
      /// `#[non_exhaustive]`. Future v2 fields default
      /// to documented zero-values; this constructor's
      /// signature MAY grow new parameters at the v2
      /// trait revision.
      #[must_use]
      #[inline]
      pub fn new(
          store_id: &'a str,
          field_name: &'a str,
          key_value: &'a str,
      ) -> Self {
          Self { store_id, field_name, key_value }
      }
  }

  // Trait signature (renamed param for clarity).
  fn validate_typed_secrets(
      &self,
      entries: &[TypedSecretEntry<'_>],
  ) -> Result<(), String>;
  ```

  Why a named struct, not a 3-tuple: tuples in trait
  signatures invite adapter-implementation drift (the
  reviewer caught the doc carrying both "second
  parameter" and "3-tuple" framings — that's the kind
  of thing tuples encourage). The named struct fixes
  the wire so all four adapters read the same field
  names. `#[non_exhaustive]` + `new(...)` keep the door
  open for v2 additions without a breaking trait rev,
  while letting `edgezero-cli` build the slice from
  outside the `edgezero-adapter` crate.

- Trait doc + implementor docs updated to say "called
  per typed-secret key including `KeyInNamedStore`
  resolutions". Tests added under §12.5 to cover the
  named-store collision case.

Migration path for adapters: existing
`validate_typed_secrets` callers pass `Vec<(&str,
&str)>`; they now pass
`&[TypedSecretEntry<'_>]` (per the sketch above).
Spin's existing collision check at
`crates/edgezero-adapter-spin/src/cli.rs:363` reads
`entry.field_name` + `entry.key_value`; non-Spin
adapters' impls become no-ops that ignore the slice.
One-touch trait extension; no logical behaviour change
for the three non-Spin adapters.

**Invariant:** ALL three of `SecretKind::KeyInDefault`,
`SecretKind::KeyInNamedStore`, and (where applicable)
the value of the `SecretKind::StoreRef` sibling field
flow through `Adapter::validate_typed_secrets`. Future
code that introduces a fourth `SecretKind` variant
that names a secret store key MUST also be added to
the walk — the trait doc-comment captures the rule and
§12.5's named-store collision test makes the gap
visible if it's ever overlooked.

**Push, diff, and validate ALL route through
`validate_excluding_secrets`** (sketched in §3.3.8). The
unified routing is the only consistent answer: pushing
`<name>.toml` whose secret-bearing key name violates a
`#[validate]` rule should never succeed at push but block at
runtime (or vice-versa). All three command paths build the
typed `C` from the file, then call the wrapper:

```rust
// crates/edgezero-cli/src/config.rs (sketch)
fn build_and_validate<C>(path: &Path) -> Result<C, String>
where
    C: DeserializeOwned + AppConfigMeta + Validate,
{
    let cfg: C = load_typed_config(path)?;
    validate_excluding_secrets(&cfg)?; // §3.3.8
    Ok(cfg)
}

pub fn run_config_push_typed<C>(args: &ConfigPushArgs) -> Result<(), String> { ... }
pub fn run_config_diff_typed<C>(args: &ConfigDiffArgs) -> Result<(), String> { ... }
pub fn run_config_validate_typed<C>(args: &ConfigValidateArgs) -> Result<(), String> { ... }
// all three call build_and_validate(...) at the top
```

The current `run_config_push_typed` at
`crates/edgezero-cli/src/config.rs:204` calls
`Validate::validate(&cfg)` directly; that call site moves to
`validate_excluding_secrets`. The new
`run_config_diff_typed` uses the same wrapper.
`run_config_validate_typed` (today at
`crates/edgezero-cli/src/config.rs:122`) also routes
through the wrapper so `--strict` and the bare validate
agree about which fields are checked.

#### 3.3.3 What this means for the extractor

The `AppConfig<C>` extractor walks `C::SECRET_FIELDS` after
reading + sha-verifying the blob, but BEFORE deserialising
into `C`. The walk operates on the raw `serde_json::Value`:

```rust
async fn extract<C>(req: &RequestContext) -> Result<C, EdgeError>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    // 1. Fetch the envelope JSON string from the adapter via ConfigStore::get.
    //    Missing blob maps to ConfigOutOfDate (Q3 (d) per round-18 M-2) —
    //    re-running `<app-cli> config push` resolves the case, which is
    //    exactly what ConfigOutOfDate means.
    let raw = config_store.get(&resolved_key).await?
        .ok_or_else(|| EdgeError::config_out_of_date(
            format!("missing typed app-config blob at key `{resolved_key}` — run `<app-cli> config push` for this deploy"),
            String::new(),
        ))?;
    let envelope: BlobEnvelope = serde_json::from_str(&raw)?;
    envelope.verify_sha()?;
    let mut data: serde_json::Value = envelope.into_data();

    // 2. Walk SECRET_FIELDS. For each KeyInDefault / KeyInNamedStore
    //    entry, look up the named secret store, fetch the value, swap
    //    it into `data[field.name]`. StoreRef entries are untouched.
    let data_obj = data.as_object_mut()
        .ok_or_else(|| EdgeError::internal("blob `data` is not a JSON object"))?;
    for field in C::SECRET_FIELDS {
        let key_name = data_obj.get(field.name)
            .and_then(|v| v.as_str())
            .ok_or_else(|| EdgeError::config_out_of_date(
                format!("missing or non-string value at `{}`", field.name),
                field.name.to_owned(),
            ))?
            .to_owned();
        // For KeyInDefault, resolve to a BOUND default store via
        // RequestContext rather than a hardcoded id string. The
        // manifest's `[stores.secrets].default` (or
        // `StoreDeclaration::default_id()`'s sole-id auto-default
        // at `crates/edgezero-core/src/manifest.rs:486`) defines
        // what "default" means; the registry exposes that as
        // `secret_store_default()` at
        // `crates/edgezero-core/src/context.rs:172`. Hardcoding
        // a string `"default"` would break any project whose
        // configured default id isn't literally that name.
        let (bound, resolved_store_id) = match field.kind {
            SecretKind::KeyInDefault => {
                let bound = req.secret_store_default().ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!(
                            "secret field `{}` has kind KeyInDefault but \
                             no default secret store is registered (check \
                             [stores.secrets].default or declare a single id)",
                            field.name,
                        ),
                        field.name.to_owned(),
                    )
                })?;
                let id = bound.store_name().to_owned();
                (bound, id)
            }
            SecretKind::StoreRef => continue, // not a secret-value field
            SecretKind::KeyInNamedStore { store_ref_field } => {
                let store_id_str = data_obj.get(store_ref_field)
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| EdgeError::config_out_of_date(
                        format!("missing store_ref `{store_ref_field}` for secret field `{}`", field.name),
                        field.name.to_owned(),
                    ))?
                    .to_owned();
                let bound = req.secret_store(&store_id_str).ok_or_else(|| {
                    EdgeError::config_out_of_date(
                        format!("blob declared store_ref `{store_id_str}` but [stores.secrets] has no such id"),
                        field.name.to_owned(),
                    )
                })?;
                (bound, store_id_str)
            }
        };
        let secret = bound.require_str(&key_name).await
            .map_err(|err| map_secret_error(err, field.name, &resolved_store_id, &key_name))?;
        data_obj.insert(field.name.to_owned(), serde_json::Value::String(secret));
    }

    // 3. Deserialise (through serde_path_to_error so we get
    //    the field-path) + validate.
    use serde::de::IntoDeserializer as _;
    let cfg: C = serde_path_to_error::deserialize(data.into_deserializer())
        .map_err(EdgeError::config_out_of_date_from_serde)?;
    cfg.validate().map_err(|err| {
        EdgeError::config_out_of_date(
            err.to_string(),
            first_violating_field(&err).unwrap_or_default(),
        )
    })?;
    Ok(cfg)
}
```

The sketch uses the exact metadata API from §3.3.1.1:
`field.name` (no `field.path`), and pattern-matches
`SecretKind::{KeyInDefault, StoreRef, KeyInNamedStore { store_ref_field }}`
explicitly. `data[field.name]` indexes a TOP-LEVEL field of
the blob's JSON object — per §3.3.1.2's "top-level only"
constraint. No nested-path traversal exists in v1.

The walk operates BEFORE `serde_json::from_value` so that:

- The serde deserialise sees the FINAL shape and either
  accepts or rejects in one step. No partial deserialise
  followed by patching.
- The validator (§6.2.2) runs AFTER secrets are populated,
  so a validator rule like `#[validate(length(min=1))]` on a
  secret-bearing field checks the RESOLVED value, not the
  key name.
- `StoreRef` fields stay untouched — their value IS the
  store id, which `cfg.<store_ref_field>` reads unchanged.

#### 3.3.4 What this means for handlers

Handlers receive a complete typed config and use the secret
values directly:

```rust
#[action]
async fn handler(
    AppConfig(cfg): AppConfig<AppDemoConfig>,
) -> Result<Response, EdgeError> {
    // cfg.api_token is the RESOLVED token (not "demo_api_token").
    upstream::call_api(&cfg.api_token).await?;
    // cfg.vault is still "default" (the store-ref field, unchanged).
    Ok(Response::ok())
}
```

This is the user-facing change vs. app-demo's current
handlers, which call
`ctx.secret_store_default()?.require_str(&cfg.api_token)`
explicitly. The migration drops every such explicit
secret-store lookup; the framework owns the resolution.

§10.2 enumerates each handler call site that changes.

#### 3.3.5 What this means for the blob shape

A typical `data` field at rest:

```json
{
  "greeting": "hello from blob",
  "api_token": "demo_api_token",
  "vault": "default",
  "feature": { "new_checkout": false },
  "service": { "timeout_ms": 1500 }
}
```

`api_token` holds the secret-store key NAME (`"demo_api_token"`),
exactly what the operator typed in `<name>.toml`. The actual
secret value lives in the secret store under that key. The
runtime `cfg.api_token` (what the handler sees) is the
RESOLVED value, populated by the extractor's walk.

#### 3.3.6 What this means for failure modes

Current `SecretError`
(`crates/edgezero-core/src/secret_store.rs:113`) has four
variants: `NotFound`, `Validation`, `Internal`, `Unavailable`.
`require_str` (line 269) maps invalid-UTF-8 bytes from the
store to `SecretError::Internal`. The extractor wraps each
into an `EdgeError` variant based on what action the operator
can take. The mapping below is comprehensive — every
`SecretError` variant has a documented landing.

| Extractor failure                                  | `SecretError`                                | `EdgeError`          | Why                                                                                                                                                                                                                                                                     |
| -------------------------------------------------- | -------------------------------------------- | -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Secret-store ID unknown (no `[stores.secrets].id`) | none — caught before secret call             | `ConfigOutOfDate`    | Manifest declares the wrong store name OR the manifest wasn't redeployed. Re-push fixes it.                                                                                                                                                                             |
| Secret key not found in the named store            | `SecretError::NotFound`                      | `ConfigOutOfDate`    | Operator forgot to provision the secret value. Re-run secret provisioning.                                                                                                                                                                                              |
| Store rejected the key shape (length, charset)     | `SecretError::Validation { .. }`             | `ConfigOutOfDate`    | The blob's key name is invalid for the named store (e.g. Fastly Secret Store keys are constrained). Operator either renames the secret-store key or fixes the `<name>.toml` value.                                                                                      |
| Secret value is bytes, not UTF-8                   | `SecretError::Internal` (from `require_str`) | `Internal`           | The store CONTAINS the key but the bytes aren't a `String`. Data quality at rest; not deploy-related. Operator audits the secret-store entry directly.                                                                                                                  |
| Secret store unreachable (transient network)       | `SecretError::Unavailable`                   | `ServiceUnavailable` | A flaky backend is not an out-of-date config — retry is the right response. Surfaces as HTTP 503; per §6.3.1 the `Retry-After: 60` header is NOT set on `ServiceUnavailable` in v1 (audit of existing producers showed too many non-retryable cases reuse the variant). |
| Any other `SecretError::Internal`                  | `SecretError::Internal`                      | `Internal`           | Unexpected store-side failure (an adapter bug, a wire-format change). Pages oncall; not actionable by deploy.                                                                                                                                                           |

The boundary between `ConfigOutOfDate` and `Internal` is:
**does re-running `<app-cli> config push` (or its sibling
secret-provisioning step) fix the situation?** If yes →
`ConfigOutOfDate`. If no → `Internal`. The invalid-UTF-8 case
falls on the wrong side of that boundary (re-push doesn't fix
non-string bytes in the store), so it stays on `Internal`.

The extractor's wrapper around the secret-store call
materialises this mapping ONCE near the call site:

```rust
// Matches the actual SecretError shape at
// crates/edgezero-core/src/secret_store.rs:113:
//   Internal(#[from] anyhow::Error)
//   NotFound { name: String }     // struct-like
//   Unavailable                   // unit
//   Validation(String)            // tuple
fn map_secret_error(err: SecretError, field_path: &str, store_id: &str, key: &str) -> EdgeError {
    match err {
        SecretError::NotFound { name } => EdgeError::ConfigOutOfDate {
            message: format!("secret `{name}` in store `{store_id}` not found"),
            field_path: field_path.to_owned(),
        },
        SecretError::Validation(msg) => EdgeError::ConfigOutOfDate {
            message: format!("secret `{key}` in store `{store_id}` rejected: {msg}"),
            field_path: field_path.to_owned(),
        },
        SecretError::Unavailable => EdgeError::service_unavailable(format!(
            "secret store `{store_id}` unreachable"
        )),
        SecretError::Internal(source) => EdgeError::internal(anyhow::anyhow!(
            "secret `{key}` in store `{store_id}` produced unexpected store error: {source}"
        )),
    }
}
```

**`Retry-After: 60` is NOT set on `ServiceUnavailable` in
v1.** An earlier draft extended the header from
`ConfigOutOfDate` to `ServiceUnavailable`; round-10 audit
of existing producers (KV size limits at
`crates/edgezero-core/src/key_value_store.rs:708`, missing
named KV store at
`examples/app-demo/.../handlers.rs:185`, missing default
secret store at `.../handlers.rs:285`) found that the
variant is reused for several non-retryable failure modes
where the header would mislead clients into a tight retry
loop. §6.3.1 documents the narrowed rule in detail:
header on `ConfigOutOfDate` ONLY; future v2 work may
split `ServiceUnavailable` so each producer site picks
the right variant.

#### 3.3.7 Sha-canonicalisation interaction

The blob's `data` field includes the secret-key NAMES. Two
pushes of the same struct produce the same canonical form
→ the same sha. The skip-on-equal path is correct: pushes
that don't change non-secret-name config skip even when the
actual secret VALUES rotated in the secret store. The blob's
contract is "what config keys + store-IDs the runtime needs",
not "what's currently in the secret store at those keys".
Secret rotation operates orthogonally; the runtime picks up
the new value on the next extractor call.

#### 3.3.8 Push-time vs runtime validation

Model A's value-swap introduces a subtlety: at PUSH time,
`cfg.api_token` is the secret-store KEY NAME
(`"demo_api_token"`); at RUNTIME, it's the resolved secret
VALUE (the actual token bytes). A `#[validate(...)]` rule on
`api_token` would run against TWO different value spaces for
the same `C`.

Concrete example. Today's typed push at
`crates/edgezero-cli/src/config.rs:204` calls
`Validate::validate(&cfg)` immediately after loading
`<name>.toml`. If `AppDemoConfig::api_token` carried
`#[validate(length(min = 32))]`, that constraint would:

- Pass at push time if the operator's key name is ≥ 32 chars.
- Fail at push time if the key name is < 32 chars.
- At runtime, validate the RESOLVED secret value against the
  same rule (which is what the operator probably meant).

The push and runtime rules disagree about what they're
validating. Picking one without the other is wrong:

- **Skip secret validators at push** but run them at runtime →
  push doesn't catch a typo in the key name length, but
  runtime validates the secret material once it's resolved.
- **Run secret validators at push** but skip at runtime →
  push validates the key name (probably not what the operator
  intended) and runtime sees an unvalidated secret.

**v1 stance: push validates everything EXCEPT secret-bearing
fields; runtime validates everything including secret-bearing
fields.** The push path consults `C::SECRET_FIELDS` to
identify which field names to skip:

- `SecretKind::KeyInDefault` — SKIP at push.
- `SecretKind::KeyInNamedStore { .. }` — SKIP at push.
- `SecretKind::StoreRef` — VALIDATE at push (the value is a
  store id, not a secret; validating its shape — e.g.
  `length(min=1)` — at push time is fine).

The push uses a wrapper over `Validate::validate` that takes
`SECRET_FIELDS` and removes per-field errors from a
`validator::ValidationErrors` after the fact:

```rust
fn validate_excluding_secrets<C: Validate + AppConfigMeta>(
    cfg: &C,
) -> Result<(), validator::ValidationErrors> {
    let result = cfg.validate();
    let Err(mut errors) = result else { return Ok(()); };
    // validator 0.20 exposes `errors_mut() -> &mut HashMap<&'static str, ValidationErrorsKind>`
    // (no public `remove` method on ValidationErrors itself).
    let bag = errors.errors_mut();
    for field in C::SECRET_FIELDS {
        if matches!(field.kind, SecretKind::StoreRef) {
            continue; // validate-at-push for store-ref fields
        }
        bag.remove(field.name);
    }
    if bag.is_empty() {
        return Ok(());
    }
    Err(errors)
}
```

The struct-level `#[validate]` rules (when the derive
annotates the type as a whole) appear in `errors_mut()`
under the special `__all__` key in validator 0.20; the
filter intentionally leaves those alone — they're not
field-scoped, so the secret-field skip doesn't apply.

The runtime path (§6.2.2) calls plain `Validate::validate` —
the swap has already happened, so the resolved values are
what the validator sees.

**Loader split required.** Today's
`load_app_config_with_options` at
`crates/edgezero-core/src/app_config.rs:191` deserialises
THEN immediately validates inline. That single entry point
can't support push: push needs to deserialise the typed `C`
WITHOUT running `.validate()`, then run
`validate_excluding_secrets(&cfg)` instead. So the loader
splits:

```rust
// edgezero_core::app_config

// NEW: deserialise-only, no .validate() call.
pub fn deserialize_app_config<C>(path: &Path, app_name: &str) -> Result<C, AppConfigError>
where C: DeserializeOwned + AppConfigMeta { /* deserialise body of load_app_config_with_options */ }

pub fn deserialize_app_config_with_options<C>(
    path: &Path, app_name: &str, opts: &AppConfigLoadOptions,
) -> Result<C, AppConfigError>
where C: DeserializeOwned + AppConfigMeta { /* ... */ }

// UNCHANGED behaviour: deserialise + validate (runtime + bundled `config validate`).
pub fn load_app_config<C>(path: &Path, app_name: &str) -> Result<C, AppConfigError>
where C: DeserializeOwned + Validate + AppConfigMeta {
    let cfg: C = deserialize_app_config(path, app_name)?;
    cfg.validate().map_err(/* AppConfigError::Validation */)?;
    Ok(cfg)
}
```

**Two distinct loader paths — DO NOT conflate them.**
The CLI commands and the runtime extractor read from
entirely different sources and so go through entirely
different loaders. The split below makes that explicit
(round-14 I-2 reviewer caught earlier drafts saying the
runtime extractor was the "only consumer of
`load_app_config*`" — that was wrong; the runtime
extractor doesn't touch TOML on disk at all):

| Caller                                                          | Source                                            | Path                                                                                                                                                                                                                                                                                                                    |
| --------------------------------------------------------------- | ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config push` / `config diff` / generated CLI `config validate` | `<name>.toml` on disk                             | `deserialize_app_config_with_options(path, app_name, opts)` → `validate_excluding_secrets(&cfg)` → §3.3.2 structural checks                                                                                                                                                                                             |
| Bundled raw `edgezero config validate`                          | `<name>.toml` on disk                             | `load_app_config_raw(path, app_name)` (TOML round-trip, no `C`, no validators)                                                                                                                                                                                                                                          |
| Runtime extractor (`AppConfig<C>`)                              | Envelope JSON STRING from `ConfigStore::get(key)` | envelope parse → SHA verify → secret walk (per §3.3.3) → `serde_path_to_error::Deserializer` over the JSON `data` field → `Validate::validate(&cfg)`. The runtime path does NOT load TOML and does NOT apply the env overlay; the env overlay is a CLI-side notion that the operator's push run resolved into the blob. |

The CLI paths all share a `build_and_validate<C>` helper
in `crates/edgezero-cli/src/config.rs` (sketch in §3.3.2).
The runtime extractor's path is sketched in §3.3.3 and
uses the §6.3.1 `EdgeError::config_out_of_date_from_serde`
constructor for serde failures (preserving the
`field_path` from `serde_path_to_error`).

The split adds two new TOML-loader entry points
(`deserialize_app_config` /
`deserialize_app_config_with_options`); the existing
`load_app_config*` entry points keep their signature and
behaviour. Today's call sites of `load_app_config*` that
DO load TOML (e.g. internal tests, the
`load_app_config_with_options` test at
`crates/edgezero-core/src/app_config.rs:764`) keep
compiling unchanged.

**Why this rule, not twin types.** A `<C>InputBlob` /
`<C>Runtime` split would let push and runtime each have their
own type with their own validators. That's cleaner
type-theoretically but expensive in surface area: every
struct doubles, every test fixture doubles, every derive
macro emits two types. The skip-at-push filter is a 20-LOC
wrapper that lets operators keep one struct.

**Operator guidance**, called out in the migration guide
(§10):

- For `#[secret]` / `#[secret(store_ref = "...")]` fields, a
  `#[validate]` rule applies to the RESOLVED secret value, NOT
  the key name. Pick rules that make sense for the secret
  itself (e.g. `length(min = 32)` for an API token).
- For `#[secret(store_ref)]` fields, the `#[validate]` rule
  applies to the store id (the value is the same at push and
  runtime). Pick rules that make sense for an id (e.g.
  `regex(r"^[a-z][a-z0-9_]*$")`).

## 4. Storage model

### 4.1 Blob shape

```json
{
  "data": {
    "greeting": "hello from blob",
    "api_token": "demo_api_token",
    "feature": { "new_checkout": false },
    "service": { "timeout_ms": 1500 },
    "vault": "default"
  },
  "sha256": "1f3a…b9",
  "version": 1,
  "generated_at": "2026-06-16T18:42:31Z"
}
```

- `data` — the typed struct serialised to JSON (camelCase /
  snake_case follows the app's serde attrs). Nested tables stay
  nested; dotted strings stay strings. This is the SHAPE the
  runtime deserialises into the app's `AppConfig` type.
- `sha256` — hex-encoded SHA-256 of the canonical-form `data`
  field (see 4.2). Embedded for one-write atomicity. The sha is
  verifiable: a reader can recompute and compare. Drift detection
  rides on this field.
- `version` — schema version of the envelope itself (NOT the
  app's config). `1` for this design. A future envelope bump
  (e.g. adding `signature`) sets `2`. The runtime rejects
  unknown values with a clear error.
- `generated_at` — RFC3339 UTC timestamp of when `config push`
  wrote the blob. Informational only; not part of the sha.

### 4.2 Canonical form for the SHA

SHA-256 is computed over the canonical form of the `data` field
(not the envelope wrapper). The canonical form is fully specified
below so any compliant implementation produces the same bytes.

**Serialisation rules:**

- **JSON, no insignificant whitespace.** No spaces between
  tokens, no trailing newline. (Whitespace inside strings is
  preserved as-is — that's significant.)
- **Object keys sorted by UTF-8 byte order**, lexicographically.
  Stable across pushes regardless of insertion order in the
  source TOML or the source struct's field order.
- **String values: VERBATIM UTF-8 bytes from the serde
  output.** No Unicode normalisation; no NFC fold; no
  whitespace trim. The bytes the canonicaliser hashes are
  the SAME bytes that go into the stored blob. This is
  load-bearing: the SHA identifies the EXACT persisted
  data, so a reader that recomputes the SHA over the
  stored blob's `data` field always agrees with the
  push-side value byte-for-byte.

  **Why not NFC.** An earlier draft normalised to NFC at
  push and re-NFC'd at read. Two consequences killed it:

  1. **Stored bytes drift from operator input.** Push
     would mutate operator-typed strings — including
     secret key names — into a normalised form. A secret
     key `"naïve"` (NFD `ñä...`) typed by the
     operator becomes the NFC `"naïve"` in storage; the
     operator's secret-store entry, if keyed by NFD, no
     longer matches.
  2. **NFC and NFD blobs share a SHA.** If two blobs
     differ only in NFC-vs-NFD encoding of the same
     visible string, their canonical SHAs are equal under
     the NFC rule, so `config push` skip-on-equal SKIPS a
     real change (the persisted bytes differ; the SHA
     says they don't).

  Operators who want Unicode-normalisation invariance on
  a NON-SECRET field can add a
  `#[validate(custom = "nfc_only")]` rule (the validator
  runs at both push and runtime, per §3.3.8 — both paths
  see the same NFC violation, same error message).

  **This guidance does NOT apply to secret-bearing
  fields** (`#[secret]`, `#[secret(store_ref = "...")]`).
  Per §3.3.8, push routes through
  `validate_excluding_secrets`, which SKIPS per-field
  validators on secret-bearing fields (the push-side
  value is a key NAME, not the secret); at runtime the
  validator runs against the RESOLVED secret VALUE, not
  the key name. So an `nfc_only` rule on a `#[secret]`
  field would validate the wrong string at the wrong
  time:

  - **At push**: skipped entirely.
  - **At runtime**: applied to the resolved secret
    bytes, not the operator-typed key name.

  Operators who need to enforce NFC on secret key names
  do it OUT OF BAND — e.g. a `<name>.toml`-shape linter
  in their deploy pipeline that normalises before push.
  v1 has no built-in hook for "validate the key NAME of
  a secret field"; that's tracked as a v2 follow-up
  (Q14 in §11). The framework does NOT silently
  normalise.

- **Numeric values:**
  - Integers (`i64` / `u64`): rendered as `<digits>`. No `+`
    sign, no leading zeros (except for `0` itself).
  - Floats (`f64`): rendered via `ryu`'s shortest
    round-trippable form. `1.5` stays `1.5`, not `1.5000000000000002`.
  - **Non-finite floats (`NaN`, `+inf`, `-inf`) are REJECTED**
    at load time, before serialisation. TOML accepts them
    (per `toml-rs`'s float grammar), and `serde_json`
    serialises them as `null` rather than erroring — which
    would silently collide with real `null` / `Option::None`
    values and produce false skip-on-equal matches across
    fundamentally different configs.

        **Error variant — new `AppConfigError::InvalidValue`.**
        The existing `AppConfigError::Validation` variant at
        `crates/edgezero-core/src/app_config.rs:125` wraps
        `Box<ValidationErrors>` — that's for
        `Validate::validate()` rule failures (range / length /
        regex / custom). Non-finite floats are a load-time
        structural problem, not a validator-rule failure;
        constructing a fake `ValidationErrors` with an owned
        dotted-path key would be awkward AND would let
        `validator`-shaped error rendering bleed into a
        non-`validator` error path. The implementing PR adds:

        ```rust
        #[error("invalid value at {field_path} in {}: {message}", path.display())]
        InvalidValue {
            path: PathBuf,
            /// Dotted path of the offending leaf, e.g.
            /// `"service.ratio"`. Joined from the env-overlay
            /// segment stack (`["service", "ratio"]`) so the
            /// same path format works for both load paths.
            field_path: String,
            /// Human-readable reason, e.g. `"non-finite f64
            /// value `NaN` is not representable in canonical
            /// form"`.
            message: String,
        },
        ```

        Tests assert the variant + the `field_path` + a
        substring of `message` (`"NaN"`, `"inf"`, or
        `"-inf"`); no `ValidationErrors` construction is
        needed.

        The check runs in two places to cover both load
        paths:
        1. **TOML deserialise.** The loader
           (`load_app_config_raw_with_options` at
           `crates/edgezero-core/src/app_config.rs:242`) walks
           the parsed `toml::Value` tree after env overlay and
           calls `f64::is_finite()` on every float leaf. The
           first non-finite hit produces
           `AppConfigError::InvalidValue { path, field_path,

    message }`.
    2. **Env overlay coercion.** The overlay's float
       parser at
       `crates/edgezero-core/src/app_config.rs:298`calls
      `parse::<f64>()`which accepts`nan`/`inf`/
      `-inf`; the implementing PR adds an
       `is_finite()`check IMMEDIATELY after the parse,
       erroring with the same`InvalidValue`variant.
       Without this, an env var
      `<APP>**FEATURE**RATIO=nan`would silently flow
       through the overlay AND through`serde_json::to_value`       into a`null` in the canonical form.

        The rejection happens BEFORE `serde_json::to_value`
        runs, so the canonicaliser never sees a non-finite
        float. §12.1 adds tests for both paths (TOML
        literal, env overlay) per the round-15 B-2 finding +
        round-16 I-2 concretisation.

  - **Type identity matters.** If the source TOML's `1500` is
    typed as `i64` in `AppConfig`, the canonical form is `1500`.
    If typed as `f64`, it's `1500.0`. The runtime's struct
    decides — push and read MUST use the same struct.

- **Booleans:** `true` / `false` lowercase.
- **`null`:** the literal `null`. Empty `Option<T>` fields
  are written as `null` in canonical form (NOT omitted),
  so two structs that differ only by `Some` vs `None`
  produce different shas. THIS IS A CONSTRAINT on the
  app's serde shape, NOT a layer we apply post-hoc — see
  the "Serde shape constraints" callout below for the
  exact rules and the macro-time enforcement.
- **Empty containers:** `{}` for empty objects, `[]` for empty
  arrays. NEITHER is interchangeable with `null` — a field with
  value `{}` is distinct from a field with value `null`.
- **UTF-8 bytes of the resulting string** are fed straight into
  `Sha256`. The hex output is lowercase, no `0x` prefix.

**Serde shape constraints (load-bearing for canonicalisation).**
The canonicaliser walks the serde-serialised JSON output of
`C` directly — it does NOT post-process the output to coerce
`None` into `null` or to round-trip-normalise omitted fields.
If `serde_json::to_value(&cfg)` returns a JSON object with the
field missing entirely (because `#[serde(skip_serializing_if =
"Option::is_none")]`), the canonical form ALSO has it missing
— which means `Some(x)` vs `None` would produce the same
canonical bytes whenever `Option::is_none` skips the field,
defeating the §4.2 rule that they hash differently. To keep
the rule honest, the `#[derive(AppConfig)]` macro enforces:

- **`#[serde(skip_serializing)]` and
  `#[serde(skip_serializing_if = "...")]` are REJECTED on
  every field of an `AppConfig`-derived struct** (NOT just
  `#[secret]` fields — round-10 H-2 finding extended this
  beyond the existing per-field-rename ban at
  `crates/edgezero-macros/src/app_config.rs:228`). If you
  want an optional field, use `Option<T>` and let it
  serialise as `null`.
- **`#[serde(flatten)]` is REJECTED on every field of an
  `AppConfig`-derived struct.** Flatten makes the JSON
  shape diverge from the Rust field tree, which breaks
  the canonicaliser's "sort keys by UTF-8 byte order of
  the field identifier" rule (the field identifier isn't
  the key any more) and complicates `serde_path_to_error`
  paths in `EdgeError::ConfigOutOfDate`. Operators who
  need a flat shape define it explicitly.
- **`#[serde(rename_all)]` is already rejected by the
  existing per-secret-field policy at
  `crates/edgezero-macros/src/app_config.rs:62`.** This
  doesn't change.

**Macro-side enforcement scope (round-12 M-4 finding).**
The existing macro at
`crates/edgezero-macros/src/app_config.rs:140` only
calls `enforce_no_disallowed_serde_attrs` from inside
`scan_field` — i.e. AFTER a field has already been
identified as carrying `#[secret*]`. Under the round-10
H-2 rule, the three skip/flatten attributes are banned
on EVERY field, secret or not. The macro is updated to
walk ALL fields of the struct (a separate pass before
the per-secret scan) and invoke
`enforce_no_disallowed_serde_attrs` on each. The
existing per-secret call site stays for the
secret-specific rules (per-field `#[serde(rename)]`,
secret-on-non-String, etc.); the new whole-struct pass
handles the skip/flatten universals.

The macro rejections fire at compile time with a clear
error message naming the disallowed attribute and the
field. §12.1 adds compile-fail (`trybuild` UI) fixtures
for EACH of the three banned attributes:

- A struct with `#[serde(skip_serializing)]` on a
  non-secret field.
- A struct with `#[serde(skip_serializing_if =
"Option::is_none")]` on a non-secret field.
- A struct with `#[serde(flatten)]` on a non-secret
  field.

All three fail to compile with the documented error.
(Earlier draft listed only `skip_serializing_if` and
`flatten` — round-12 M-4 added the
`skip_serializing` fixture for full coverage of the
authoritative table in §3.3.1.4.)

**Helper signature:**

```rust
fn canonical_data_sha256(data: &serde_json::Value) -> String {
    let canonical = canonical_form::to_string(data); // rules above
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}
```

**Stability across implementations:**

- **The v1 canonicaliser is a hand-rolled ~150-LOC
  walker over `serde_json::Value` shipped in
  `crates/edgezero-core/src/canonical_form.rs` (or
  `.../canonical_form/mod.rs`).** No external
  canonicaliser crate. Round-21 reviewer ran a probe
  against `serde_canonical_json` v1.0.0 and found it
  REJECTS finite floats (`Floating point numbers are
forbidden`); the §4.2 rules explicitly support
  finite `f64` via `ryu` (see "Numeric values" above),
  so the external crate cannot implement this spec
  unchanged. Earlier drafts left the external-vs-
  hand-rolled choice open to the implementing PR;
  round-21 pins it to hand-rolled because the
  candidate external crate is incompatible AND
  because the hand-rolled walker is short enough
  (~150 LOC) that the "zero external dep, full
  control of the float-rendering rule" trade-off wins
  on its own.

  Concrete acceptance criteria for the implementing PR:

  1. The hand-rolled module
     `crates/edgezero-core/src/canonical_form.rs` (or
     `.../canonical_form/mod.rs`) exists and exposes
     `canonical_data_sha256(&serde_json::Value) ->
String` matching the §4.2 rules. It depends only
     on `serde_json` (already in-tree), `ryu` (for
     finite-float rendering — round-trippable
     shortest form), and `sha2`.
  2. `canonical_form_pins.rs` test asserts a real
     64-character hex SHA against the fixture below;
     the `5d4a0e7f…fixed-hex-value…b9` placeholder is
     replaced with the actual computed hex.
  3. The test file's doc-comment identifies the
     module as the in-tree v1 canonicaliser (no
     external crate name to record).

  Patch-level changes to `ryu` go through code review
  (not Dependabot auto-merge) because they change the
  hash space. The walker itself is in-tree, so its
  behaviour is governed by the repo's code-review
  process directly.

  Earlier draft surveyed `serde_canonical_json` as a
  candidate external crate. That candidate is
  excluded by §4.2's finite-float rule (the crate's
  `to_string` errors on any finite float). A future
  external crate that matches §4.2's float rendering
  could replace the hand-rolled walker behind the
  same `canonical_data_sha256` signature; until then
  the in-tree walker is the v1 contract.

- A `canonical_form_pins.rs` test in `edgezero-core` hashes a
  fixed `serde_json::Value` fixture and asserts the hex output
  byte-for-byte. The fixture is FROZEN at the implementing
  PR's chosen version; the hex value below is a
  PLACEHOLDER that the implementing PR replaces with the
  real computed hex:

  ```rust
  #[test]
  fn canonical_form_pin_v1() {
      let data = json!({
          "greeting": "héllo",        // verbatim bytes; NFC vs NFD encodings hash differently per §4.2
          "feature": { "new_checkout": true },
          "service": { "timeout_ms": 1500 },
          "ratio": 1.5,
          "missing": null,
          "empty": {}
      });
      assert_eq!(
          canonical_data_sha256(&data),
          // IMPLEMENTING PR replaces this with the real 64-char hex.
          // The placeholder MUST not survive merge — a CI
          // gate (§13 acceptance step) greps for `…` /
          // `fixed-hex-value` in this file and fails the build
          // if found.
          "5d4a0e7f…fixed-hex-value…b9",
      );
  }
  ```

  If a canonicaliser bump changes the hex output, this test
  fails and the bump turns into a forcing function: either roll
  back, OR bump the envelope `version` field (§4.1) to signal
  to all readers that the canonical-form rule changed.

- The envelope `version` field is co-versioned with the
  canonical-form rules. `version: 1` MEANS the rules above
  PLUS the implementing PR's pinned canonicaliser version.
  `version: 2` would mean a future, documented rewrite. A
  reader built for v1 errors on v2 envelopes pointing at
  the migration guide.

### 4.3 Read-side validation

When the runtime reads the blob:

1. Deserialise the envelope into `BlobEnvelope { data, sha256,
version, .. }`. Unknown envelope versions error with a
   pointer at the migration guide.
2. Recompute `canonical_data_sha256(&data)`.
3. If it doesn't match the stored `sha256`, return
   `ConfigStoreError::internal("blob sha mismatch: stored {hex}
!= computed {hex}")` (the existing `internal(...)`
   constructor at `config_store.rs:180`; see §6.3 for the
   reasoning behind keeping this on `Internal` instead of
   adding a new variant) and DO NOT proceed. The runtime
   gives up on this request rather than silently honouring a
   tampered blob. The extractor maps
   `ConfigStoreError::Internal` to `EdgeError::Internal` per
   existing convention.
4. **Secret walk (per §3.3.3).** Iterate over
   `C::SECRET_FIELDS`. For each `KeyInDefault` or
   `KeyInNamedStore` field, look up `data[field.name]` in the
   appropriate secret store and REPLACE the JSON value with
   the resolved secret value. `StoreRef` fields are
   untouched. The walk runs against the
   `serde_json::Value` form of `data` — secrets land before
   `serde_json::from_value` ever sees the modified shape.
5. Deserialise the modified `data` into `C` via
   `serde_path_to_error::deserialize` wrapping
   `serde_json::Value::into_deserializer()` (NOT
   `serde_json::from_value` directly — that path discards
   the field-path information that
   `EdgeError::ConfigOutOfDate.field_path` requires per
   §6.3.1). The wrapper accumulates the JSON path of any
   deserialise failure (e.g. `"feature.new_checkout"`); the
   extractor maps the resulting
   `serde_path_to_error::Error<serde_json::Error>` to
   `EdgeError::config_out_of_date_from_serde(err)` per
   §6.3.1's two-constructor split, which populates
   `field_path` from `err.path()`. Without this wrapper, a
   schema mismatch surfaces as a generic
   `EdgeError::ConfigOutOfDate { message, field_path: "" }`
   and §12.6's field_path assertion fails.
6. Run `Validate::validate(&cfg)` per §6.2.2 — full validation
   including secret-bearing fields, now that they hold
   resolved values.
7. Yield `C` to the handler.

This ordering matters: the sha check runs BEFORE the secret
walk (so we don't waste secret-store calls on a tampered
blob), and the secret walk runs BEFORE `serde_json::from_value`
(so the deserialise sees one consistent shape, never a
partial-deserialise plus patch).

The sha check is cheap (one hash over the canonical form) and
catches manual KV-store edits that bypassed `config push`, plus
truncation / partial-write corruption that the per-leaf model
couldn't even detect.

## 5. Per-environment KEY override

### 5.1 The default key

`config push` writes under a default key derived from the
manifest's `[stores.config].default` (or the only declared id when
`ids` has length 1). The default is the logical id itself —
matches what the current per-leaf flow does for STORE selection.

So with `[stores.config] ids = ["app_config"]`, the blob lands
under key `app_config` inside the store.

### 5.2 Runtime override

A new env var picks the active KEY inside the store:

```
EDGEZERO__STORES__CONFIG__<ID>__KEY
```

Same shape as the existing `__NAME` override, just `__KEY` instead.
`<ID>` is the upper-cased logical id from `[stores.config].ids`.

**Store-id charset constraint (round-13 M-2 finding).**
The current manifest validator at
`crates/edgezero-core/src/manifest.rs:880` accepts
`[A-Za-z0-9_-]` for store ids — but a hyphen makes the
upper-cased `<ID>` invalid as a POSIX shell environment
variable name (`export EDGEZERO__STORES__CONFIG__FEATURE-FLAGS__KEY=...`
fails in `bash` / `zsh` / `sh`). Operators using
`feature-flags` as a store id today silently lose access
to the `__NAME` / `__KEY` overrides.

The blob model TIGHTENS the manifest validator: store ids
in `[stores.kv]`, `[stores.config]`, and `[stores.secrets]`
MUST match `[A-Za-z0-9_]+` (alphanumeric + underscore;
hyphen is NOT allowed). The validator change is a hard
cutoff per §1 — operators with hyphenated store ids
rename them (`feature-flags` → `feature_flags`) before
the blob model merges. The existing manifest error
message at `manifest.rs:894` is updated to point at the
exportability constraint:

```text
`[stores.<kind>].ids` entry `feature-flags` contains a
hyphen, which is not a valid POSIX shell environment
variable character. The `EDGEZERO__STORES__<KIND>__<ID>__NAME`
and `__KEY` overrides would not be exportable. Rename it
to use only ASCII alphanumeric + `_` (e.g. `feature_flags`).
```

`__` (double underscore) remains reserved as the env-var
separator (existing rule at `manifest.rs:891`); the new
charset rule just narrows the allowed character set
within each segment from `[A-Za-z0-9_-]` to
`[A-Za-z0-9_]`. §12.18 covers this with three fixtures
(hyphen rejected, underscore-only succeeds, `__`
remains rejected).

Examples:

- `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging` —
  the runtime reads the `app_config_staging` blob instead of
  `app_config` from the same KV namespace / config store / Spin KV
  label.
- Unset: defaults to the logical id (`app_config`).

**Precedence table** (highest wins):

| Source                                                           | Effect                                       |
| ---------------------------------------------------------------- | -------------------------------------------- |
| `AppConfig::<C>::named(req, "k")` / `from_store(..., Some("k"))` | Explicit per-call key — wins over everything |
| `EDGEZERO__STORES__CONFIG__<ID>__KEY=<val>` (non-empty)          | Per-id runtime override                      |
| `[stores.config].default` mapped to a key                        | Manifest default for the logical id          |
| (none)                                                           | Use the logical id literal as the key        |

**Whitespace / empty handling.** Matches the existing `__NAME`
fall-back rule: if the env value is empty, whitespace-only, or
contains control characters, it's treated as unset and the
runtime falls back to the next precedence level. This prevents a
blank `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=` export from
silently making the runtime look up the `""` key (which doesn't
exist) and surfacing as `EdgeError::Internal`.

**Non-interaction with `__NAME`.** `__NAME` picks the platform
STORE; `__KEY` picks the entry within it. They compose
orthogonally:

```
EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_kv_ns        # which KV namespace
EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_canary  # which key inside it
```

→ runtime reads key `app_config_canary` from the KV namespace
bound as `prod_kv_ns`.

This lets one platform store host every environment's blob (dev,
staging, prod) under different keys, and the env var picks one at
runtime. No code change, no manifest edit — just the env var.

### 5.2.1 How the runtime carries the resolved key

The runtime extractor needs the resolved key at request time,
but today's request context only holds `ConfigRegistry`
handles + a default id
(`crates/edgezero-core/src/context.rs:38`,
`crates/edgezero-core/src/store_registry.rs:106`). Adapters
read `EnvConfig` when they BUILD the registry handles
(`crates/edgezero-adapter-fastly/src/request.rs:385`,
`crates/edgezero-adapter-cloudflare/src/request.rs:362`,
`crates/edgezero-adapter-spin/src/request.rs:279`), then drop
it. The extractor has no surface to ask "what KEY should I
use for this id?".

**Solution: pair each registry entry with the resolved key.**

Replace today's per-id `ConfigStoreHandle` registry value
with a `ConfigStoreBinding` carrying both the handle AND the
key the adapter pre-resolved at request-context-construction
time:

```rust
// edgezero_core::store_registry (extended)

#[derive(Clone, Debug)]
pub struct ConfigStoreBinding {
    /// Handle the extractor calls `get(...)` on.
    pub handle: ConfigStoreHandle,
    /// The key the extractor should look up by DEFAULT.
    /// Computed by the adapter as `env.store_key("config", id)`
    /// (see §5.2.1's `EnvConfig` extension below) — equivalent to
    /// reading `EDGEZERO__STORES__CONFIG__<ID_UPPER>__KEY`, applying
    /// the §5.2 blank/control filter, and falling back to the
    /// logical id.
    pub default_key: String,
}

pub type ConfigRegistry = StoreRegistry<ConfigStoreBinding>;
```

`ConfigStoreBinding: Clone + Debug` is required — the
`StoreRegistry<H>` impl bounds `H: Clone` at
`crates/edgezero-core/src/store_registry.rs:106`, and the
binding is stored, cloned through `default()` / `named()`,
and printed via the existing `Debug` derive on
`StoreRegistry`. The derive is on the struct above.

**`StoreRegistry` ref accessors.** Today `StoreRegistry<H>`
exposes only owned-clone accessors at
`crates/edgezero-core/src/store_registry.rs:119`:

```rust
pub fn default(&self) -> Option<H> { self.by_id.get(&self.default_id).cloned() }
pub fn named(&self, id: &str) -> Option<H> { /* cloned */ }
```

The new `default_binding()` / `named_binding(id)` accessors
on `Config` (per §5.2.1 below) and the new
`config_store_default_binding()` / `config_store_binding(id)`
on `RequestContext` want `Option<&ConfigStoreBinding>` so
callers can read `binding.default_key` without paying the
clone cost on every extract. Two options:

- **(chosen) Add `default_ref` / `named_ref`** on
  `StoreRegistry<H>` that return `Option<&H>`. One-line
  helpers that delegate to `BTreeMap::get`. The owned
  accessors keep working (Kv / Secrets / hand-managed
  Config callers unchanged); the new ref accessors are
  what `Config::default_binding()` and
  `RequestContext::config_store_*_binding()` forward to.
- (rejected) Have the binding accessors return owned
  `ConfigStoreBinding`. Clones a `String` (the
  `default_key`) on every request — small, but
  unnecessary since the binding lives for the request's
  lifetime in the registry-in-extensions.

Adding the ref accessors:

```rust
// crates/edgezero-core/src/store_registry.rs (one new pair)

impl<H: Clone> StoreRegistry<H> {
    #[must_use]
    #[inline]
    pub fn default_ref(&self) -> Option<&H> {
        self.by_id.get(&self.default_id)
    }

    #[must_use]
    #[inline]
    pub fn named_ref(&self, id: &str) -> Option<&H> {
        self.by_id.get(id)
    }
}
```

The new accessors are general on `H` (not specific to
`ConfigStoreBinding`), so future `KvRegistry` /
`SecretRegistry` use can pick them up if needed — costs
nothing today.

**Env-var lookup is centralised, not hand-coded per adapter.**
`EnvConfig` already centralises store-name env parsing with the
empty/whitespace/control-character filter at
`crates/edgezero-core/src/env_config.rs:113` (`store_name`). Add
a parallel `store_key` helper so all four adapters use one code
path with one set of rules:

```rust
// edgezero_core::env_config (new helper)

/// Key for a logical store — `EDGEZERO__STORES__<KIND>__<ID>__KEY` —
/// falling back to `id` itself when unset, blank, whitespace-only,
/// or containing control characters. Mirrors `store_name`'s
/// filter exactly (`is_blank_or_control` at env_config.rs:151).
/// `kind` is `"kv"` / `"config"` / `"secrets"` (though `"config"`
/// is the only kind that uses `default_key` today).
#[must_use]
#[inline]
pub fn store_key(&self, kind: &str, id: &str) -> String {
    self.get(&["stores", kind, id, "key"])
        .filter(|value| !is_blank_or_control(value))
        .map_or_else(|| id.to_owned(), str::to_owned)
}
```

Each adapter's `build_config_registry` calls
`env.store_key("config", id)` for every declared id while it's
already calling `env.store_name("config", id)` — one extra
line per id, no behaviour duplicated. Tests for the
blank/whitespace/control fallback rules live alongside
`env_config`'s existing `store_name` tests; the adapter sites
just pass-through.

The KV and secret registries are NOT changed — they have no
analogous per-id "active key" concept; their `__NAME` env
override targets the platform store name, which the adapter
already wires when building handles. Only `ConfigRegistry`
needs the binding pair.

**Adapter responsibilities.** Each adapter's
`build_config_registry` (or equivalent) calls
`env.store_key("config", id)` (see the helper above) for
every declared id while it's already calling
`env.store_name("config", id)`, and packages
`(handle, default_key)` into the binding. The four
adapters update at the lines referenced above; the
env-var lookup is one line per id inside the existing
loop, and the blank/control fallback is centralised in
`EnvConfig` so all four adapters share identical
behaviour.

**Extractor consumption.** The extractor reads through the
registry:

```rust
async fn extract<C>(req: &RequestContext, override_key: Option<&str>) -> Result<C, EdgeError>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    let binding = req
        .config_store_default_binding()
        .ok_or_else(|| EdgeError::internal("no default config store registered"))?;
    let key = override_key.unwrap_or(&binding.default_key);
    // Missing blob maps to ConfigOutOfDate per §6.3 / Q3 (d) — a re-run
    // of `<app-cli> config push` is the actionable response, not a 500.
    let raw = binding.handle.get(key).await? // ConfigStore::get
        .ok_or_else(|| EdgeError::config_out_of_date(
            format!("missing typed app-config blob at key `{key}` — run `<app-cli> config push` for this deploy"),
            String::new(),
        ))?;
    // ... envelope parse, sha verify, secret walk, deserialise + validate per §4.3
}
```

`RequestContext` grows two helpers mirroring the existing
`config_store_default()` / `config_store(id)`:

```rust
impl RequestContext {
    pub fn config_store_default_binding(&self) -> Option<&ConfigStoreBinding>;
    pub fn config_store_binding(&self, id: &str) -> Option<&ConfigStoreBinding>;
}
```

The existing `config_store_default()` / `config_store(id)`
that return `ConfigStoreHandle` stay (hand-managed
`ConfigStore::get(...)` callers keep working). They just
unwrap to `binding.handle` internally.

**Raw `Config` extractor surface.** The raw multi-store
extractor at `crates/edgezero-core/src/extractor.rs:597` —
`Config(ConfigRegistry)` with `default() / named() /
registry()` accessors — currently returns
`BoundConfigStore` (= `ConfigStoreHandle`) directly. Under
the binding change:

| Method                      | Before (today)                      | After (this spec)                                                                                                            |
| --------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | --- | ---------------------------------------------------------- |
| `Config::default()`         | `Option<ConfigStoreHandle>`         | `Option<ConfigStoreHandle>` — unchanged. Internally `self.0.default().map(                                                   | b   | b.handle)`. Hand-managed `bound.get(...)` works unchanged. |
| `Config::named(id)`         | `Option<ConfigStoreHandle>`         | `Option<ConfigStoreHandle>` — same unwrap.                                                                                   |
| `Config::registry()`        | `&StoreRegistry<ConfigStoreHandle>` | `&StoreRegistry<ConfigStoreBinding>` — **breaking** for any caller that destructured the registry value. Hard cutoff per §1. |
| `Config::default_binding()` | (didn't exist)                      | `Option<&ConfigStoreBinding>` — new accessor for callers that want the resolved `__KEY`.                                     |
| `Config::named_binding(id)` | (didn't exist)                      | `Option<&ConfigStoreBinding>` — new accessor (named variant).                                                                |

`BoundConfigStore` stays as the `ConfigStoreHandle` alias —
NOT redefined to `ConfigStoreBinding`. That keeps the
"bound" word meaning "a handle pre-paired with a platform
identity" everywhere in the codebase and stops the binding
pair from leaking into call sites that only want the
handle.

The `registry()` change is the only breaking surface for
existing handler code. An audit of in-tree callers
(`grep -r '\.registry()' crates/`) shows the only consumer
is the framework's own extractor plumbing — no hand-managed
caller iterates the registry value type directly today, so
the blast radius is contained. Tests in §12.15 (raw `Config`
binding accessors) cover both the unwrap path
(`Config::default().get(key)` still works) and the new
accessor path (`Config::default_binding().default_key`
matches the env override).

**Why this carrier and not the alternatives.**

The reviewer pointed at three options: env config in request
extensions, extend `StoreRegistry`, or generate a key
resolver via the `app!` macro. Trade-offs:

| Option                              | Cost                                                                  | Why rejected                                                                                                                                           |
| ----------------------------------- | --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `EnvConfig` in request extensions   | Hot data on every request; lookup cost per extract                    | Re-reads env on every request. Adapters already do this once at startup; duplicating per request wastes work.                                          |
| **Extend `StoreRegistry`** (chosen) | One new field on the registry value; one env lookup per id at startup | Caches the resolution; extractor reads from registry like every other store lookup.                                                                    |
| `app!` macro emits a const map      | Compile-time const                                                    | Env can't be compile-time. The macro would need to emit a `LazyLock<HashMap>` populated at first request, plus the macro grows runtime-config surface. |

Extending `StoreRegistry` keeps the change localised to the
adapters' registry-build code + the registry type itself,
and the extractor's request-time path stays one indirection.

### 5.3 Why not per-environment stores

Considered: have each environment use a different `__NAME`
override pointing at a different platform store (a different KV
namespace, config store, Spin label).

Rejected because:

- Provisioning gets harder: each env needs its own provisioned
  store, and `provision` would need to know which env to target.
- The blob shape stays the same across envs; the only thing that
  changes is "which blob". A different KEY in the same store is
  the minimal version of that.
- Operators can still combine the two if they want fully isolated
  stores: set both `__NAME` and `__KEY`.

### 5.4 Push-side override

`config push` writes to the default key by default. A `--key
<key>` flag overrides:

```
app-demo-cli config push --adapter cloudflare --key app_config_staging
```

So an operator running CI can push the `staging` blob to the same
store the prod runtime reads from, then point the staging
deployment at `__KEY=app_config_staging`. One store, multiple
co-existing environment blobs.

## 6. Extractor surface

### 6.0 Name and relationship to the existing `Config` extractor

`edgezero_core::extractor::Config` ALREADY exists today as the
raw multi-store registry accessor (a tuple struct around
`ConfigRegistry`) — handlers reach for it to do hand-managed
`store.get("...")` calls. The blob model needs a DIFFERENT
extractor: typed, generic over `C`, returning the deserialised
struct.

Reusing the name `Config<C>` would silently change the meaning
of every existing handler that imports `Config`. To avoid the
collision, the blob model introduces a new extractor:

```rust
edgezero_core::extractor::AppConfig<C>
```

Naming rationale:

- `AppConfig` matches the project's existing naming convention
  (`AppDemoConfig`, `<NameUpperCamel>Config`).
- It's clearly distinct from `Config` (the raw store registry),
  so handler code reads unambiguously.
- `AppConfig<C>` reads as "the typed application config of type
  `C`", matching how the rest of the framework talks about it.

The existing raw `Config` extractor STAYS for hand-managed
entries (the `ConfigStore::get` use cases §6.5 calls out). It
is not renamed, removed, or replaced; we just don't reuse the
name.

### 6.1 Default-key form

```rust
use edgezero_core::extractor::AppConfig;

#[action]
async fn handler(AppConfig(cfg): AppConfig<MyAppConfig>) -> Result<Response, EdgeError> {
    log::info!("greeting = {}", cfg.greeting);
    Ok(Response::new(...))
}
```

`AppConfig<C>` reads the env-resolved default key from the
default config store (resolves the same way
`ctx.config_store_default()` does today), parses the envelope,
verifies the sha, **walks `C::SECRET_FIELDS` and replaces each
secret field's key-name value with the resolved secret VALUE
from the secret store** (§3.3.3), then deserialises the
modified `data` into `C` and runs `Validate::validate(&cfg)`
(§6.2.2) before yielding `C`. Handler code that destructures
`AppConfig(cfg)` gets a complete struct with secrets
populated.

### 6.2 Explicit-key form

```rust
#[action]
async fn handler(req: RequestContext) -> Result<Response, EdgeError> {
    let staging: MyAppConfig = AppConfig::<MyAppConfig>::named(&req, "app_config_staging").await?;
    // …
}
```

`AppConfig::<C>::named(req, key)` reads the specified key from
the **default** config store. Useful for:

- Admin endpoints that read another environment's config for
  comparison.
- A/B routing that splits traffic between two blob keys.
- The diff command's runtime read-back (eats its own dogfood).

**Signature.** `named` is a static async method returning the
inner type directly, NOT wrapped in `AppConfig<C>`. The newtype
wrapper exists only to drive the `FromRequest` extraction; once
the caller explicitly names a key, they get the plain typed
value:

```rust
impl<C> AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    async fn named(req: &RequestContext, key: &str) -> Result<C, EdgeError>;
}
```

**Trait bounds rationale:**

- `DeserializeOwned` — serde deserialise from the blob's `data`
  field (post-secret-resolution).
- `AppConfigMeta` — provides `SECRET_FIELDS`, the metadata the
  extractor walks to look up + replace `#[secret]` values
  during the resolution step (§3.3.3). The macro emits this
  trait impl alongside the struct; under Model A it's now a
  required bound (round-3 said optional; round-4 promoted it
  because the extractor relies on it).
- `Validate` (from `validator`) — the extractor calls
  `cfg.validate()` after deserialising. Matches what `config
validate --strict` runs at push time, so env-overlay drift
  (a manifest-side value silently overriding a typed field
  past its declared bounds) becomes a runtime error rather
  than a silent acceptance. See §6.3.1 for the decision.
- `Send + 'static` — standard `FromRequest` requirements.

### 6.2.2 Runtime validation

Once the extractor deserialises `data` into `C`, it calls
`Validate::validate(&cfg)`. Validation failures map to:

- `EdgeError::ConfigOutOfDate` — the validator's report names
  exactly which field violated which constraint. Same surface
  as the deserialise-failure case (§6.3): operator action is
  "re-push the typed config; the deployed `<name>.toml` is
  out of bounds for the deployed code".
- A `log::error!` line with the full validator report so
  dashboards see the violation, not just the 5xx.

Rationale for validating at every extract: the runtime trusts
the BLOB's authenticity (sha verified) but NOT its
correctness. A push that bypassed `config validate --strict`
(operator ran `--no-validate` or hit a `config push` from an
older CLI version that didn't validate) could land an
out-of-bounds value. The runtime check catches it on the
first request rather than letting the handler use the bad
value.

Performance: `Validate::validate` is constant-time per field
for the `validator` derive macro's common rules
(`range`, `length`, `regex`). The cost is paid once per
request — same as deserialisation. If a future profiling pass
shows it dominates, the extractor can opt into "validate
once per blob sha" caching; out of scope for v1.

### 6.2.1 Cross-store form

Apps with multiple `[stores.config]` ids — e.g. one for the typed
app config and another for ops-managed runtime flags — need to
target a specific store, not the default. PR #269 shipped
multi-id config stores; the extractor exposes them via:

```rust
let ops: OpsFlags = AppConfig::<OpsFlags>::from_store(&req, "ops_flags", None).await?;
let ops_named: OpsFlags =
    AppConfig::<OpsFlags>::from_store(&req, "ops_flags", Some("ops_flags_canary")).await?;
```

```rust
impl<C> AppConfig<C>
where
    C: DeserializeOwned + AppConfigMeta + Validate + Send + 'static,
{
    async fn from_store(
        req: &RequestContext,
        store_id: &str,        // logical id from [stores.config].ids
        key: Option<&str>,     // None = env-resolved default key for that id
    ) -> Result<C, EdgeError>;
}
```

Bounds match `named` (§6.2) and the extractor itself.
`AppConfigMeta` is required here because the cross-store form
also resolves secrets (§3.3.3) and the secret walk needs
`C::SECRET_FIELDS`.

The store_id resolves against the env-overlaid `[stores.config]`
ids the same way `ctx.config_store(id)` does today. The optional
key argument lets cross-store reads stay aware of the per-store
`__KEY` env override.

The default-store sugar (`AppConfig<C>` extractor +
`AppConfig::named`) is shorthand for `from_store(req, default_id,
key)`.

### 6.3 Errors

The extractor surfaces:

- `EdgeError::ServiceUnavailable` — store unreachable, network
  errors. Maps to HTTP 503.
- `EdgeError::ConfigOutOfDate { message, field_path }` —
  typed-struct deserialise failure: the blob is present and
  the envelope parses, but `data` doesn't fit the runtime's
  `C` type. Almost always means "the code shipped before the
  matching `<app-cli> config push`". See the contract below.
- `EdgeError::Internal` — sha mismatch (drift / corruption),
  envelope parse failure (envelope `version` unrecognised or
  shape unexpected). Maps to HTTP 500. These are genuinely
  unexpected and should page the operator.
- **Key missing from the store** (i.e.
  `ConfigStore::get(key)` returned `Ok(None)`) maps to
  `EdgeError::ConfigOutOfDate` (Q3 (d) per round-18
  M-2, restated here for §6.3 hard-cutoff). HTTP 503
  with `Retry-After: 60`. Message: `missing typed
app-config blob at key \`<key>\` — run \`<app-cli>
  config push\` for this deploy`. Rationale: a missing
typed-app-config blob is operationally
indistinguishable from "the operator didn't run
`config push`yet" — which is exactly the`ConfigOutOfDate`class ("re-run config push fixes
it"). Mapping to`Internal`would page oncall on a
push-fixable condition; mapping to`NotFound`(404)
would imply the URL is wrong, which it isn't. A
future`MaybeAppConfig<C>`→`Option<C>`extractor
(Q3 (c)) could remap this for endpoints that want
explicit defaults; v1 ships with`ConfigOutOfDate`
  and no opt-out.

**Implementation note — `ConfigStoreError` to `EdgeError`
mapping.** `ConfigStoreError` (`config_store.rs:165`) has only
three variants today: `Internal`, `InvalidKey`, `Unavailable`.
The extractor maps:

| ConfigStoreError                      | EdgeError            | HTTP | Notes                                                                                                                                                                                                                                       |
| ------------------------------------- | -------------------- | ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Unavailable`                         | `ServiceUnavailable` | 503  | Transient backend issue.                                                                                                                                                                                                                    |
| `Internal` (sha mismatch)             | `Internal`           | 500  | Drift or corruption — the stored sha doesn't match canonical recompute.                                                                                                                                                                     |
| `Internal` (envelope parse failure)   | `Internal`           | 500  | Envelope `version` unrecognised or shape unexpected.                                                                                                                                                                                        |
| `InvalidKey`                          | `BadRequest`         | 400  | Adapter rejected the key shape.                                                                                                                                                                                                             |
| _missing key_ (`Ok(None)` from `get`) | `ConfigOutOfDate`    | 503  | NOT a `ConfigStoreError` variant — caught at the extractor's `ok_or_else` after `ConfigStore::get` returns `Ok(None)`. Round-18 M-2 reversal: was `Internal` in earlier drafts; the new mapping matches Q3 (d) + §3.3.3's extractor sketch. |

Plus the new `ConfigOutOfDate` variant `EdgeError` gains as part
of this work; no new variant on `ConfigStoreError` is needed.

**Why `ConfigOutOfDate` is separate from `Internal`:** the
operator's response is different. `Internal` means "investigate
what's wrong with our store / blob"; `ConfigOutOfDate` means
"re-run `<app-cli> config push` for the deployed code
revision". A single generic 500 conflates both and trains
operators to ignore the class of error that's most actionable.

### 6.3.1 `ConfigOutOfDate` concrete contract

The current `EdgeError` response shape carries `status` +
`message` only (`crates/edgezero-core/src/error.rs:159`). The
blob model needs more structure so dashboards can route
`ConfigOutOfDate` to a different oncall than generic 503s.
Two specific extensions:

**1. Response-body shape.** Today's `EdgeError::IntoResponse`
at `crates/edgezero-core/src/error.rs:159` writes:

```json
{ "error": { "status": 500, "message": "…" } }
```

The blob model extends this to:

```json
{
  "error": {
    "status": 503,
    "kind": "config_out_of_date",
    "message": "missing field `new_checkout`",
    "field_path": "feature"
  }
}
```

Concretely:

- The outer `error` envelope (the body's top-level
  `{ "error": { ... } }` wrapper) STAYS — handler-facing
  clients that already match on `body.error.status` keep
  working.
- A `kind: String` field is added INSIDE `error`. Every
  variant gets a stable string:
  `"bad_request"`, `"internal"`, `"method_not_allowed"`,
  `"not_found"`, `"not_implemented"`, `"service_unavailable"`,
  `"validation"`, and the new `"config_out_of_date"`. The
  enum-discriminant-to-string mapping is fixed at impl time;
  clients can pattern-match on `kind` instead of inferring
  from `status`.
- A `field_path: String` field is added INSIDE `error`,
  ONLY on `config_out_of_date` (and any future
  field-anchored variant). Other variants omit it. Clients
  that don't care can ignore.
- Status code: 503 for `ConfigOutOfDate` (not 500). The
  semantic is "service can't honor this request because the
  deployed config + code are out of sync; retry after
  redeploy".

The change touches `error.rs:159`'s `IntoResponse` impl and
the serde-serialised body shape. This IS a breaking change
to the response body — operators who parse the body shape on
the client side need to update. Per §1's hard-cutoff stance,
no compat shim.

**2. Send `Retry-After: 60`** on the new `ConfigOutOfDate`
variant ONLY. Earlier drafts extended the header to all
`ServiceUnavailable` responses; an audit of in-tree
`ServiceUnavailable` producers shows that the variant is
ALREADY used for several non-config-related conditions
where a tight client retry would be actively harmful or
just noisy:

| Producer                                                     | Condition                             | Retry-After:60 helpful?                                                                    |
| ------------------------------------------------------------ | ------------------------------------- | ------------------------------------------------------------------------------------------ |
| `crates/edgezero-core/src/error.rs:119`                      | config-store unavailability           | yes — a rolling re-push converges in <60s                                                  |
| `crates/edgezero-core/src/key_value_store.rs:708`            | KV size / count limit exceeded        | NO — the request keeps failing until the operator drops data; client retry just wastes RPS |
| `examples/app-demo/crates/app-demo-core/src/handlers.rs:185` | missing NAMED kv store (manifest gap) | NO — the binding is missing; retry won't fix                                               |
| `examples/app-demo/crates/app-demo-core/src/handlers.rs:285` | missing default secret store          | NO — same: manifest gap, retry won't fix                                                   |

Adding `Retry-After: 60` to every `ServiceUnavailable`
would lie to clients in three of four cases. The blob
model takes the narrower stance:

- **`ConfigOutOfDate`** — header sent. The new variant is
  shaped specifically for "deployed config + code are out
  of sync; redeploy converges in <60s", which is exactly
  what the header tells clients.
- **`ServiceUnavailable`** — header NOT sent in v1. The
  variant covers too many distinct conditions today to
  unconditionally promise a 60-second retry. Audit
  ticket: a future v2 PR may split `ServiceUnavailable`
  into `ServiceUnavailable` (current generic) plus a new
  `ServiceUnavailableRetryable` (carries the header) so
  each producer site picks the right variant; out of
  scope here.

Added via
`Response::headers_mut().insert("retry-after",
HeaderValue::from_static("60"))` inside the
`IntoResponse` impl branch for `ConfigOutOfDate` only.
Other variants (`ServiceUnavailable`, `Internal`,
`BadRequest`, etc.) do NOT set the header.

**3. `field_path` for the variant's payload** comes from
`serde_path_to_error::Track::path()` (or equivalent), wrapped
around the blob's `data` field's deserialiser. Plain serde
errors only report position-in-input, not field-path; the
`serde_path_to_error` crate adds the JSON path
(`feature.new_checkout`) at de-cost negligible compared to
the deserialise itself. The dep is small (~500 LOC, no
transitive deps) and locked-in for the variant.

**Variant declaration (sketch).** Two constructors —
one for the serde-deserialise path (which has rich
field-path data from `serde_path_to_error`), one for
the secret-walk and validator paths (which already
have an explicit `(message, field_path)` pair):

```rust
pub enum EdgeError {
    // ...existing variants...
    ConfigOutOfDate {
        message: String,      // e.g. "missing field `new_checkout`"
        field_path: String,   // e.g. "feature"
    },
}

impl EdgeError {
    /// Construct from an explicit (message, field_path) pair.
    /// Used by the secret walk (§3.3.3) and the validator path
    /// (§6.2.2). `field_path` SHOULD be a dotted path naming
    /// the offending field (e.g. `"feature.new_checkout"`);
    /// pass `String::new()` when no specific field is anchored
    /// (the response simply omits the `field_path` key).
    pub fn config_out_of_date(
        message: impl Into<String>,
        field_path: impl Into<String>,
    ) -> Self {
        Self::ConfigOutOfDate {
            message: message.into(),
            field_path: field_path.into(),
        }
    }

    /// Construct from a `serde_path_to_error` error returned
    /// by the deserialise wrapper around the blob's `data`
    /// field. The field-path is extracted via
    /// `Error::path().to_string()`. Used by the deserialise
    /// step in §3.3.3 / §6.2.2.
    pub fn config_out_of_date_from_serde(
        serde_err: serde_path_to_error::Error<serde_json::Error>,
    ) -> Self {
        Self::ConfigOutOfDate {
            message: serde_err.inner().to_string(),
            field_path: serde_err.path().to_string(),
        }
    }
}
```

Caller sites:

- Secret walk (§3.3.3) and validator path (§6.2.2):
  `EdgeError::config_out_of_date(msg, field_path)`.
- Blob deserialise (§3.3.3, §6.2.2):
  `EdgeError::config_out_of_date_from_serde(err)`.

The validator path (§6.2.2) wraps a
`validator::ValidationErrors`:

```rust
EdgeError::config_out_of_date(
    validation_err.to_string(),         // validator's rendered report
    extract_first_field(&validation_err),
)
```

The `extract_first_field` helper picks the first
violating-field name from the `validator::ValidationErrors`
tree; if multiple fields violate at once, the response
surfaces one (the first by alphabetical order) and the full
list goes to the log line.

**Why `serde_path_to_error` is OK as a new dep.** Worth
mentioning because PR #269 fought several "do we add this
dep" rounds. Verdict: yes, this one is small enough, single
purpose, and the only realistic way to give operators a
useful field-path hint. Without it, every `ConfigOutOfDate`
response says "missing field" with no anchor — operators have
to grep the typed struct manually.

### 6.4 Per-request caching

**v1 stance: no caching. Every `AppConfig<C>` extractor
invocation reads from the store.**

Earlier drafts proposed a per-request `OnceLock` cache hung off
the request's `Extensions` map. That doesn't fit the current
`FromRequest` API: extractors receive `&RequestContext`, and
`Extensions` access through that surface is immutable
(`crates/edgezero-core/src/context.rs:149`). Adding the cache
would require either (a) interior mutability on the cache
(another `Mutex`/`RwLock` to maintain) or (b) preseeded
mutable state at dispatch time before the request reaches the
handler. Both are non-trivial design surface that doesn't
justify v1's risk budget.

Performance implications:

- Most handlers extract `AppConfig<C>` exactly once. The cache
  would benefit only the (rare) handler that extracts the same
  key twice or extracts the default key AND a named override
  of the same key in the same request.
- The per-adapter `ConfigStore` impls already cache at the
  adapter level for some backends (Spin's KV is in-process;
  Cloudflare's KV has region-local caches). The marginal cost
  of a second extractor call is bounded by the slowest
  adapter's repeat-read.
- Validation (§6.2.2) runs on every extractor call, cache or
  no cache. The validation cost is comparable to the
  deserialisation cost; caching the deserialised value still
  pays the validation cost.

**Follow-up path:** if profiling on real workloads shows the
repeat-read cost matters, the cache can be added in a
follow-up that ALSO solves the `FromRequest` extension-mut
problem (e.g., add an `&mut Extensions` accessor to
`RequestContext` for extractors, or introduce a per-request
arena). That's design surface worth its own discussion;
deferring is the right call for v1.

Cross-request caching (TTL, invalidation on push) is out of
scope per §3.2.

### 6.5 What happens to the existing `ConfigStore` trait

`ConfigStore::get(key)` stays — it's still useful for ad-hoc
lookups against the same backends. But the `AppConfig<C>` extractor
is the ONLY supported way to read the typed app-config.
Hand-written `ctx.config_store_default()?.get("greeting")` against
the typed app-config's store will now find a JSON blob under
"greeting"-ish keys, not raw strings, and fail to deserialise into
`String`. The migration guide says "use `AppConfig<C>` for the
typed app-config; reserve the raw `Config` / `ConfigStore` for
hand-managed entries".

## 7. SHA-256

### 7.1 Where it's computed

- **Push side** (`<app-cli> config push`): compute the sha BEFORE
  the write. The blob envelope carries the sha as a field.
- **Read side** (`AppConfig<C>` extractor + diff command): recompute
  the sha after reading and compare with the stored field. Mismatch
  is a hard error.
- **Skip-on-equal push**: read the current remote blob FIRST,
  pluck its `sha256` field, compare with the about-to-be-written
  local sha. Equal → log "no change, skipping" and exit 0. Not
  equal → push.

### 7.2 What it covers

The sha covers the `data` field's canonical form (see §4.2). It
intentionally does NOT cover:

- `generated_at` — would change every push even when the data
  doesn't.
- The envelope `version` — pinned by the runtime version.
- The sha field itself (chicken-and-egg).

This means two pushes of the same data, written ten minutes
apart, produce blobs with identical `sha256` fields but different
`generated_at`. The skip-on-equal path catches this — no write
happens.

### 7.3 What it does NOT promise

The sha is a **drift / integrity** signal, not a signature. It
proves "this `data` matches that `sha256`" — nothing about WHO
wrote it. An attacker with write access to the store can compute
a new sha for arbitrary content and the runtime accepts it.

A future envelope `version: 2` could add a `signature` field over
the same canonical form, validated against a public key the
runtime bakes in. Out of scope for v1.

## 8. Diff command + push integration

### 8.1 `<app-cli> config diff`

```
<app-cli> config diff --adapter <name>
                     [--manifest <path>]
                     [--app-config <path>]
                     [--store <id>]
                     [--key <key>]
                     [--no-env]
                     [--local]
                     [--runtime-config <path>]
                     [--format unified|structured|json]
                     [--exit-code]
```

(`<app-cli>` is the typed downstream CLI, e.g. `app-demo-cli`.
Per §3.2.1 the bundled `edgezero` binary's `config push` /
`config diff` are stub-pointer subcommands that exit non-
zero with a pointer at the typed downstream CLI.

Synopsis flags map 1:1 to `ConfigDiffArgs` fields in
§3.2.2; the order above mirrors the declaration order
there for easy cross-reference.)

Behaviour:

1. Load `<name>.toml` into the typed `C` (the project's
   `<NameConfig>`), then run `validate_excluding_secrets`
   per §3.3.8. Same validation routing as `config push` and
   `config validate` — secret-bearing fields are skipped at
   push/diff/validate time and re-checked at runtime extract
   against the resolved values.
2. Serialise to the canonical `data` form and compute the local sha.
3. Read the remote blob from the configured `[stores.config]` id
   - adapter. Extract `sha256` and `data` from the envelope.
4. If local sha == remote sha: print
   `# no changes (sha256 matches: 1f3a…b9)` and exit 0.
5. Else: compute a structural diff between the two `data` objects
   and print per `--format`:

#### 8.1.1 `unified` format (default)

```
--- remote (sha256: a472…)
+++ local  (sha256: 1f3a…)

feature.new_checkout
- false
+ true

service.timeout_ms
- 1500
+ 2000

vault (added)
+ "default"

nested.subtree (added)
+ { "alpha": 1, "beta": 2 }

obsolete.feature (removed)
- "old-value"
```

Dotted paths in alphabetical order; one block per change.
**Subtree handling:** when a whole subtree is added or removed in
one push (the parent key transitions from absent → present or
vice versa), the diff prints ONE block at the parent path with
the full subtree value pretty-printed (compact JSON, 2-space
indent if multi-line). When the parent EXISTS on both sides but
leaves underneath change, each changed leaf gets its own block —
no rolled-up "5 leaves changed under `nested`" summary, because
the dotted-path form is the operator's actionable surface.

The goal is "readable in a git terminal".

#### 8.1.2 `structured` format

YAML-shaped tree showing additions / removals / value changes at
each subtree. Useful for nested objects with many changed leaves
under one parent.

#### 8.1.3 `json` format

```json
{
  "local_sha256": "1f3a…",
  "remote_sha256": "a472…",
  "added": { "vault": "default" },
  "removed": {},
  "changed": {
    "feature.new_checkout": { "from": false, "to": true },
    "service.timeout_ms": { "from": 1500, "to": 2000 }
  }
}
```

Machine-readable. Useful for downstream CI that wants to gate
deploys on the change set.

#### 8.1.4 TOCTOU note

`config push` runs the diff against a moment-in-time read of the
remote, then writes. Between the read and the write, another
operator's push can change the remote state. The skip-on-equal
check happens right before the write (re-fetched then), but the
diff the operator approved was against the EARLIER snapshot.

Implications:

- **Correctness:** none. Each adapter's KV layer is
  last-writer-wins; our write is atomic per shellout.
- **UX:** the operator may see "no changes vs. remote" in the
  diff but the final push still writes (because by the time we
  hash-checked, the remote was something else). The status
  output names the actually-observed remote sha so the operator
  isn't lied to.
- **Coordination:** §10 ("Migration / operational notes")
  spells out the "config push isn't safe to run concurrently"
  stance.

### 8.2 Diff inside `config push`

```
<app-cli> config push --adapter <name>           # diff inline, prompts on changes
<app-cli> config push --adapter <name> -y        # skip prompt (-y alias)
<app-cli> config push --adapter <name> --yes     # skip prompt
<app-cli> config push --adapter <name> --no-diff # scripted: skip the diff render (still prompts unless --yes)
<app-cli> config push --adapter <name> --dry-run # diff only, no write
```

Default flow:

1. Same setup as `config diff`.
2. Print the unified diff (skipped if `--no-diff`).
3. If `--yes` / `-y` is set, proceed to write.
4. Else (TTY detected via `io::IsTerminal` on stdin): prompt
   `Apply changes? [y/N] `. `y` proceeds; `n` exits with
   non-zero. (Default response on bare `<enter>` is `N`.)
5. Else (non-TTY without `--yes`): exit non-zero with a clear
   "set `--yes` for non-interactive runs" hint.
6. Run the write. On success, print the new sha + `generated_at`.

**Flag interactions:**

- `--no-diff --yes`: skip the diff RENDER and the
  prompt. Read-back + skip-on-equal STILL RUN — if the
  remote sha matches the local sha, the push exits early
  with the "no changes" message and no write happens
  (saving an unnecessary store write + the deploy-side
  cache-invalidation it would trigger). This is NOT the
  pre-rewrite blind push; the only thing suppressed is
  the human-facing render + the consent gate. Round-14
  M-2 caught earlier drafts calling this "equivalent to
  the pre-rewrite blind push" — that wording understated
  the skip-on-equal contract that round-12 + round-13
  pinned as load-bearing. Exception: when the adapter
  read returns `ReadConfigEntry::Unsupported` (Spin
  Cloud per §9.4), there's no remote sha to compare
  against, so skip-on-equal can't run; the write
  proceeds under the consent gate (`--yes` satisfied
  here) but writes UNCONDITIONALLY.
- `--no-diff` without `--yes`: still prompts (the prompt
  asks before writing — the render is just suppressed).
  Read-back + skip-on-equal still run; the prompt is
  only shown if there are changes. Useful when the
  operator already inspected the diff via `config
diff`.
- `--dry-run --yes`: `--dry-run` wins. The diff renders,
  no write happens. `--yes` is a no-op in this
  combination.

If the diff shows no changes (local sha == remote sha), step 3
skips the prompt and the write entirely — same skip-on-equal path
as `config push --dry-run`.

### 8.3 Per-adapter read-back

Read-back is required by both `config diff` and skip-on-equal
push. Per adapter:

- **Axum**: parse the per-id local file
  (`manifest_root/.edgezero/local-config-<id>.json`) as a
  key-to-envelope-string map (per §9.1), look up the requested
  key, return the envelope JSON string. Missing file →
  `MissingStore`. Missing key in present file → `MissingKey`.
- **Cloudflare**: `wrangler kv key get --binding <BINDING>
<KEY> --remote` for read-back. (Note: as of Wrangler 4.x
  the subcommand path is `wrangler kv key get` /
  `wrangler kv key put` / `wrangler kv key list` — the
  earlier short forms `wrangler kv get` etc. are deprecated.
  Implementation pins the four-segment form.) With
  `--local`, reads from `.wrangler/state` instead. The push
  side keeps the existing `wrangler kv bulk put
--namespace-id=<id> --remote` form at
  `crates/edgezero-adapter-cloudflare/src/cli.rs:289` —
  the binding-name-based `kv key get` for read-back is OK
  because read-back is one key at a time, but the push
  side intentionally uses `kv bulk put` with the
  pre-resolved namespace id (per `cli.rs:289`'s comment
  about wrangler v4's silent local fallback).
- **Fastly**: `fastly config-store-entry describe --store-id=<id>
--key=<key> --json`. Returns the value as a string field; parse
  to JSON.
- **Spin (local)**: read directly from
  `<spin.toml dir>/.spin/sqlite_key_value.db` via the vendored
  `spin_key_value` schema. We already write through this schema;
  reading is symmetric.
- **Spin (Fermyon Cloud) — DIFF / SKIP-ON-EQUAL UNSUPPORTED IN
  v1.** The Spin CLI's `spin cloud key-value` subcommand set
  (per the Fermyon Cloud command reference) exposes
  `create` / `delete` / `list` / `rename` / `set` — there
  is NO `get` that returns a per-key value. Per the same
  reference, `list` enumerates STORES (not keys), and
  `delete` deletes a whole STORE (not individual keys);
  `set` is the only per-key operation shown.
  Implementing read-back via the Fermyon Cloud HTTP API
  directly would require wiring HTTP+auth into the spin
  adapter (a separate work stream — out of scope here).

  v1 behaviour: `Adapter::read_config_entry` on
  `--adapter spin` against a Cloud-targeting deploy returns
  `ReadConfigEntry::Unsupported`. The diff command surfaces:

  ```text
  config diff for spin against Fermyon Cloud is unsupported in v1
  (Spin CLI 3.x exposes no `spin cloud key-value get`). Push
  unconditionally with `<app-cli> config push --adapter spin --yes`
  or run `--local` for the on-disk SQLite read.
  ```

  And `config push --adapter spin` against Cloud has a
  four-branch UX that reconciles §8.2's "prompt unless
  `--yes`" rule with the no-diff-to-show situation
  (round-12 L-2 fix: the table grew from three branches
  to four when `--dry-run` was added; the prose count
  is updated accordingly):

  | Caller environment      | Behaviour                                                                                                                                                                                                                                                                                                                                                                                   |
  | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | `--dry-run`             | Exit non-zero with: `config push --dry-run --adapter spin against Spin Cloud is unsupported (no remote read-back; re-run with --local for the on-disk SQLite write or drop --dry-run to write unconditionally with --yes)`. The flag's contract is "show the diff", but Spin Cloud has no remote read to compute the diff against; the framework refuses rather than printing a half-truth. |
  | `--yes` (or `-y`) set   | Write unconditionally. No prompt, no diff render. Output names the skip-on-equal absence.                                                                                                                                                                                                                                                                                                   |
  | TTY without `--yes`     | Prompt with: `cannot read remote on Spin Cloud (no get subcommand); write anyway? [y/N]`. `y` proceeds. `n` exits non-zero.                                                                                                                                                                                                                                                                 |
  | Non-TTY without `--yes` | Exit non-zero with: `Spin Cloud read-back unsupported; pass --yes for non-interactive runs (the push writes unconditionally)`.                                                                                                                                                                                                                                                              |

  This preserves §8.2's invariant that non-TTY pushes
  require explicit confirmation, while letting interactive
  operators answer the prompt directly. The "write anyway"
  prompt makes the missing-read explicit rather than
  silently skipping the diff phase.

  Output on a successful Cloud write — i.e. the
  `--yes` branch and the TTY-with-accepted-prompt branch
  (the `--dry-run` and non-TTY-without-`--yes` branches
  exit non-zero before any write):
  `pushed N entries to Spin Cloud (skip-on-equal unavailable;
remote sha not readable)`.

  This requires a new `ReadConfigEntry::Unsupported` variant
  on the enum at §9.0; missing this would force adapters to
  fake `MissingStore`, which is incorrect (the STORE is
  present; we just can't query it). The diff command branches
  on `Unsupported` separately to produce the actionable
  error above instead of "no remote at this key".

  Follow-up (not v1): wire the Fermyon Cloud HTTP API for
  read-back, OR petition upstream to add `spin cloud
key-value get`. Either unblocks Spin Cloud diff +
  skip-on-equal in a future PR.

## 9. Per-adapter implementation

### 9.0 Adapter trait surface

Today the `Adapter` trait
(`crates/edgezero-adapter/src/registry.rs:181`) has WRITE-only
config methods: `push_config_entries` (line 277) and
`push_config_entries_local` (line 315). The blob model needs
read-back for `config diff` and `config push`'s skip-on-equal
path, so the trait gains explicit read methods alongside the
existing writers.

The read method's signature **mirrors `push_config_entries`** so
each adapter has the same context to do the symmetric operation:

```rust
pub trait Adapter: Sync + Send {
    // ... existing methods (provision, push_config_entries,
    //     push_config_entries_local, ...) ...

    /// Read the raw blob bytes for the typed app-config entry at
    /// (store, key) from the adapter's remote backend. Used by
    /// `config diff` and `config push`'s skip-on-equal check.
    ///
    /// Matches `push_config_entries` (registry.rs:277) in
    /// signature shape so the symmetric read/write paths share
    /// context: same manifest_root, same adapter_manifest_path,
    /// same component_selector (Spin), same ResolvedStoreId, and
    /// the same AdapterPushContext (which carries
    /// runtime_config_path, deploy mode hints, etc.).
    fn read_config_entry(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String>;

    /// Local-emulator counterpart of `read_config_entry`. Mirrors
    /// `push_config_entries_local` (registry.rs:315). Axum's
    /// read = read_config_entry (axum is local-only).
    fn read_config_entry_local(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        component_selector: Option<&str>,
        store: &ResolvedStoreId,
        key: &str,
        push_ctx: &AdapterPushContext<'_>,
    ) -> Result<ReadConfigEntry, String>;
}

pub enum ReadConfigEntry {
    /// The store and key both exist; bytes are the raw blob
    /// envelope JSON. Caller parses + verifies.
    Present(Vec<u8>),
    /// The store exists but no entry sits at this key. Diff
    /// treats this as "every leaf is added".
    MissingKey,
    /// The store itself is absent (e.g. wrangler.toml has no
    /// matching binding, fastly.toml has no setup table, axum's
    /// local-config-<id>.json doesn't exist yet). Diff treats
    /// this the same as `MissingKey` plus an extra hint that
    /// the operator hasn't run `provision` yet.
    MissingStore,
    /// The adapter cannot query the backend for this entry —
    /// e.g. Spin Cloud's CLI exposes `set`/`delete`/`list` but
    /// no `get`. Diff surfaces an "unsupported on this
    /// adapter" error pointing the operator at the workaround
    /// (push unconditionally with `--yes`, or use `--local`).
    ///
    /// **Skip-on-equal only.** `config push` short-circuits
    /// the skip-on-equal CHECK on this variant (there's no
    /// remote sha to compare against), but the §8.2 consent
    /// policy still applies — the push does NOT silently
    /// write. For Spin Cloud, that means the operator still
    /// needs `--yes` (or an accepted TTY prompt) before any
    /// write happens. Round-14 I-3 caught earlier drafts
    /// describing this as "short-circuits to unconditional
    /// write", which contradicted §3.2.2's non-TTY-without-
    /// `--yes` rule. Fixed: `Unsupported` skips the diff
    /// comparison; the write side keeps the same consent
    /// gate every other adapter has.
    Unsupported(&'static str),
}
```

**Why mirror the writer signature.** The reviewer's earlier
finding ("the proposed read_config_entry takes store_id and
local only … current writers receive resolved logical/platform
store ids, component selector, adapter manifest path, runtime
config path, and push context") is correct. Spin's read-back
needs `push_ctx.runtime_config_path` to know which
`runtime-config.toml` declares the KV label; Cloudflare's needs
the manifest path to find the wrangler binding; Fastly's needs
the setup table inside fastly.toml. Mirroring the writer's
context keeps the read implementation parallel and lets each
adapter reuse its existing helper functions for resolution.

**Read-vs-local split.** Two methods (`read_config_entry` /
`read_config_entry_local`) mirror the two writers. The
`config diff` command picks one based on the operator's
`--local` flag (per §8.3 Q7's resolved default: explicit).
`config push`'s skip-on-equal check uses the SAME variant as
the about-to-execute write — push-remote reads remote;
push-local reads local. Avoids the cross-mode comparison
that would tell the operator "no changes vs. remote" right
before a local-mode write that lands different bytes.

Missing-remote semantics:

- `Present(bytes)`: diff parses the envelope and produces a
  per-leaf diff vs. local.
- `MissingKey`: diff prints "no remote at key
  `<key>`; all <N> leaves added" and the unified output shows
  every local leaf with a `+`.
- `MissingStore`: same as `MissingKey` but the header line
  ALSO says "store has no matching backend yet — run `edgezero
provision --adapter <name>` first if this is the live remote".

`config push` short-circuits the skip-on-equal CHECK
whenever the read returns anything but `Present` (no remote
sha to compare against). The CONSENT side keeps running:
the §8.2 prompt / `--yes` rule applies in all four cases
(`Present`, `MissingKey`, `MissingStore`, `Unsupported`),
so no variant turns a push into a silent unconditional
write. Per-variant summary:

- `Present` — skip-on-equal compares shas; consent decides
  the write.
- `MissingKey` / `MissingStore` — no remote sha; render
  diff as "all leaves added"; consent decides the write.
- `Unsupported` — no remote sha AND no diff possible
  (per §8.3 Spin Cloud); consent decides the write
  exactly as it would for `Present`. The §8.3 prompt
  text adapts to mention the missing read.

The four adapters implement `read_config_entry` per §9.1-9.4
below.

### 9.1 Axum

The other adapters back onto KV-shaped stores where multiple
keys live in one platform store. Axum has no such platform
layer — it's a single local file per logical id. For the
per-environment `__KEY` override (§5.2) to work the same way on
Axum, the file shape can NOT be the envelope itself; it has to
be a MAP from key to envelope so multiple environment blobs
coexist in one file.

**File shape — `manifest_root/.edgezero/local-config-<id>.json`.**
A flat `{ key: envelope_json_string }` map. Each value is the
envelope serialised to JSON and stored as an ESCAPED STRING (not
a nested JSON object), so the file deserialises into
`BTreeMap<String, String>` and Axum's `ConfigStore::get(key)`
returns the string directly without any envelope-aware
parsing inside the adapter.

```json
{
  "app_config": "{\"data\":{...},\"sha256\":\"1f3a…\",\"version\":1,\"generated_at\":\"2026-06-16T18:42:31Z\"}",
  "app_config_staging": "{\"data\":{...},\"sha256\":\"a472…\",\"version\":1,\"generated_at\":\"2026-06-16T18:51:02Z\"}"
}
```

- Push: read the file (creating an empty `{}` if missing), insert
  / overwrite the entry at the requested key (default
  `app_config`, override via `--key`), write atomically (write
  to `<file>.tmp`, fsync, rename). The entry value is the
  envelope serialised then `serde_json::to_string`-ed and
  inserted as a string value.
- Runtime: the extractor stays adapter-neutral and only calls
  `ConfigStore::get(key) -> Option<String>` per
  `crates/edgezero-core/src/config_store.rs:214`. Axum's
  `ConfigStore` impl wraps the file map:

  ```rust
  impl ConfigStore for AxumConfigStore {
      async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
          let map: BTreeMap<String, String> = self.map.read().await.clone();
          Ok(map.get(key).cloned())
      }
  }
  ```

  The string form keeps the per-adapter `ConfigStore::get ->
Option<String>` contract intact across all four adapters —
  storing nested JSON objects would force the Axum store to
  re-serialise on every `get`, AND would require the extractor
  to know Axum's file shape. The string form sidesteps both.

- The existing per-id file path (`local-config-<id>.json`)
  stays — it's per-id, not per-key. Per-key resolution happens
  INSIDE the file. This means `[stores.config].ids = ["a",
"b"]` produces TWO files (`local-config-a.json`,
  `local-config-b.json`), each of which can hold multiple
  environment blobs as map entries.
- File layout: project-root discovery (PR #269 F7) already
  handles the cwd / project-root mismatch.
- Read-back for `config diff` and skip-on-equal: parse the
  file, look up the key, return the envelope STRING (the
  same value `ConfigStore::get` returns at request time). The
  caller (the diff command) does the envelope parse +
  sha-extract once on that string.
  Missing file → `MissingStore`. Missing key in a present
  file → `MissingKey`.

### 9.2 Cloudflare

- Push: serialise envelope → JSON string → `wrangler kv
bulk put <tempfile.json> --namespace-id=<id> --remote`
  (the same form
  `crates/edgezero-adapter-cloudflare/src/cli.rs:289`
  already uses for per-leaf writes; the blob is now ONE
  entry in the bulk-put JSON instead of N). Resolving
  `--namespace-id` from `wrangler.toml` via the existing
  `find_namespace_id` helper avoids wrangler v4's silent
  local-storage fallback when the command is invoked
  binding-name-only.
- Read-back (for diff / skip-on-equal):
  `wrangler kv key get --binding <BINDING> <KEY> --remote`
  (Wrangler 4.x's four-segment subcommand path; the older
  three-segment `wrangler kv get` is deprecated).
- Runtime: `CloudflareConfigStore::get(key)` returns the JSON
  string; the `AppConfig<C>` extractor parses + verifies.
- `--local` push: `wrangler kv bulk put <tempfile.json>
--binding <BINDING> --local` — lands in
  `.wrangler/state`. Local push deliberately addresses by
  BINDING name (not by namespace id) per
  `crates/edgezero-adapter-cloudflare/src/cli.rs:399`:
  the scaffold ships with `local-dev-placeholder`
  namespace ids in `wrangler.toml`, so an operator who
  hasn't run `edgezero provision` yet should still be
  able to seed `.wrangler/state` from the manifest.
  Resolving the placeholder to a numeric id would
  block the local workflow.
- Remote push (above) uses `--namespace-id=<id> --remote`
  because wrangler v4's `kv bulk put` falls back silently
  to LOCAL storage when given only `--binding` without an
  explicit `--remote` resolved against a real namespace
  — see the comment at `cli.rs:289` for the full
  rationale.

### 9.3 Fastly

- Push: serialise envelope → JSON string →
  `fastly config-store-entry update --store-id=<id> --key=<key>
--upsert --stdin` (PR #269 round 2 F4 already wired `--upsert
--stdin`; we just write one key now).
- Runtime: `FastlyConfigStore::get(key)` returns the JSON value;
  extractor parses.
- Local server: `[local_server.config_stores.<id>]` now has ONE
  entry — the blob key. PR #269 F6 already moved local-server
  seeding to `config push --local`; this just writes one `key =
"<json blob>"` line.

### 9.4 Spin

- Push (local): SQLite-direct write into `.spin/sqlite_key_value.db`
  via the vendored `spin_key_value` schema. One INSERT statement,
  one (store, key) tuple, one VALUE (the JSON blob).
- Push (Cloud): `spin cloud key-value set --app <APP> --label
<LABEL> <KEY>=<JSON>`. Single shellout, no chunking needed (the
  chunking was for many `key=value` pairs; we have one).

  **Argv exposure trade-off.** Unlike Fastly's
  `config-store-entry update --stdin` (PR #269 round 1 F4), Spin
  Cloud's CLI has no documented stdin / file-input form for
  `cloud key-value set` as of Spin 3.6.x. That means the entire
  blob lands on argv, with two consequences:

  - The whole config appears in `ps`/`/proc/<pid>/cmdline` of
    the operator's shell during the push.
  - The blob size is bounded by the host's `ARG_MAX` (~256 KiB
    on Linux, smaller on macOS) AND by the Spin writer's
    pre-existing per-pair cap at
    `crates/edgezero-adapter-spin/src/cli/push_cloud.rs:46`:
    `MAX_ARGV_BYTES_PER_INVOCATION = 96 * 1024` (98 304 bytes).
    The per-pair check is `pair.len() >=
MAX_ARGV_BYTES_PER_INVOCATION` where `pair` is
    `<KEY>=<VALUE>` — under the blob model the key is the
    logical id (e.g. `app_config`, 10 bytes) plus `=` plus
    the entire envelope JSON string, so the EFFECTIVE blob
    cap is `MAX_ARGV_BYTES_PER_INVOCATION - <KEY>.len() - 1`
    bytes.

  Both are regressions of an issue we already fixed for Fastly.
  The blob model AMPLIFIES it because the per-leaf push had
  per-pair size; the blob model concentrates everything into
  one shell-out.

  **v1 stance:** accept the trade-off, document the cap, and
  align the spec to the writer's PRE-EXISTING 96 KiB
  per-invocation cap. The implementing PR does NOT raise
  the cap as part of this work (raising it would require a
  separate ARG_MAX audit + cross-platform testing); the
  blob model just inherits whatever the writer already
  enforces. Concretely, `config push --adapter spin` to
  Cloud errors with an actionable message when the local
  blob (after envelope wrapping) exceeds
  `MAX_ARGV_BYTES_PER_INVOCATION - <KEY>.len() - 1` bytes —
  i.e. ~95 KiB for the default `app_config` key. The check
  reuses the existing per-pair guard at `push_cloud.rs:90`;
  the implementing PR updates its error message to point at
  the blob-model workarounds (split across
  `[stores.config]` ids; use `--local`). The check is
  Spin-Cloud-specific; other adapters don't trigger it.

  An earlier draft of this spec proposed a 200 KiB cap
  ("~80% of `ARG_MAX`"), but that contradicted the
  existing writer cap and would have required raising
  it. The round-15 reviewer correctly flagged the
  conflict; the spec is now aligned with the shipped
  enforcement.

  **Follow-up paths** (not v1):
  - File an upstream Spin issue requesting `spin cloud
key-value set --stdin` or `--from-file`.
  - When/if it lands, switch the writer over and remove the
    size cap.
  - Until then, operators with larger configs can either
    RESTRUCTURE their typed `C` into multiple separate
    types (one per logical surface — e.g.
    `BillingConfig`, `FeatureConfig`) and wire each
    through its own `[stores.config]` id with its own
    `AppConfig<...>` extractor, OR use a non-Cloud Spin
    deploy. Round-18 H-1 reframed this from "split
    across ids" (which the framework does NOT
    auto-do — `run_config_push_typed::<C>` writes one
    blob per store, NOT a chunked merge) to
    "restructure into separate typed structs". §3.2
    keeps the "no multi-blob merge" stance — splitting
    is operator-side schema work.

- Runtime: `SpinConfigStore::get(key)` returns the JSON value;
  extractor parses.

## 10. Migration

Hard cutoff. No compat shims, no dual-shape parsing. **The
runtime does not parse pre-blob flat-leaf state at all** — it
deserialises the envelope wrapper and errors if the read returns
a string that doesn't fit `BlobEnvelope`'s shape. This is by
design. Hard cutoff means no "two paths to support forever"
maintenance tax.

### 10.1 Top-level migration sequence

1. The first commit that lands this work updates every in-tree
   consumer to the blob model. No PR splits this into "runtime
   first, app-demo later" — the two are atomic.
2. The runtime `AppConfig<C>` extractor replaces direct
   `ctx.config_store_default()?.get(key)` calls against the typed
   config's store. Downstream apps with custom `ConfigStore`
   consumers see a deprecation note in `CHANGELOG.md`; the
   compile-time surface (the `ConfigStore` trait itself) doesn't
   change.
3. Pre-blob blobs (the flattened per-leaf state) become
   unreadable. The runtime errors loudly on the first request
   pointing at the migration guide. We do NOT silently treat
   per-leaf state as "the app_config blob" — the deserialise into
   the envelope wrapper fails fast.
4. Migration guide:
   `docs/guide/manifest-store-migration.md` gets a new section
   covering the blob shape, the `__KEY` override, and the manual
   one-time `<app-cli> config push --adapter <name>` re-run that
   converts every adapter's state from per-leaf to blob.

### 10.2 App-demo migration (concrete file list)

The app-demo reference (`examples/app-demo/`) must land on the
blob model in the SAME commit that ships the runtime change.
There is no transitional "old app-demo on flat-leaf, new runtime
on blob" state. The reviewer should expect every one of these
files to change:

- **Typed-config struct — minor change.**
  `examples/app-demo/crates/app-demo-core/src/config.rs`
  declarations stay shape-compatible (Rust field
  identifiers, types, and serde non-rename attrs don't
  change), but the FIELD SEMANTICS shift under Model A:
  `api_token: String` is now a `#[secret]` field whose
  runtime value (`cfg.api_token`) is the **resolved secret
  value**, not the operator's TOML literal. The
  `#[derive(AppConfig)]` macro is updated per §3.3.1.1 to
  emit the new `SECRET_FIELDS` metadata; the existing
  `#[secret] api_token: String` keeps the same annotation.
  No struct-shape edits, but the documented semantic of each
  `#[secret]` field changes; comments on the struct should
  call out "this field's RUNTIME value is the resolved
  secret; the TOML value is the secret-store key name".
- **Source-of-truth TOML.**
  `examples/app-demo/app-demo.toml` — `api_token =
"demo_api_token"` stays as-is (still a secret-store key
  name AT REST). Comments updated to reflect "the loader
  serialises this whole file to a single JSON blob written
  under key `app_config`; at runtime, the framework swaps
  `#[secret]` field values for resolved secrets before the
  handler sees them" instead of the current per-leaf
  wording.
- **Handlers — TWO classes of changes.**
  `examples/app-demo/crates/app-demo-core/src/handlers.rs`:
  1. **Typed-config reads switch to the extractor.** Any
     handler that today reads
     `ctx.config_store_default()?.get("...")` (single-line
     OR two-step `let store = ...; store.get(...)`) for
     typed-config leaves switches to the `AppConfig<C>`
     extractor (per §6.1).
  2. **Secret resolution drops out of handler code.** The
     current secret-store lookup at `handlers.rs:287`
     (`ctx.secret_store(&cfg.vault)?.require_str
(&cfg.api_token)`, plus any sibling call sites) is
     REMOVED. The handler uses `cfg.api_token` DIRECTLY —
     the framework's secret walk (§3.3.3, §4.3) populated
     it at extract time.
  3. **Handlers that read raw store keys for hand-managed
     entries** (NOT typed app-config) stay on the
     `ConfigStore` trait. They're unaffected.

  The grep gate (§10.2.1) Patterns 1-3 catch each removal
  category; the positive `AppConfig` import check confirms
  the new extractor wired in.

- **Typed-push tests.**
  `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs`
  (and any sibling test files) — assertions move from "the
  config store contains key X with value Y" to "the config
  store contains key `app_config` whose JSON envelope's
  `data.X` equals Y". The sha-mismatch path gets a dedicated
  assertion. The secret-key retention assertion needs an
  update: today's strip-check asserts `api_token` is ABSENT
  from the push; the new test asserts it's PRESENT with the
  expected secret-key NAME (`"demo_api_token"`), confirming
  the writer keeps the operator's literal value (the secret
  VALUE is resolved only at runtime extract per §3.3.3).
- **Per-adapter smoke harness.**
  `scripts/smoke_test_config.sh` — the per-adapter seed step
  changes from "push N flat leaves" to "push the blob". The
  per-key HTTP checks against the running app stay the same
  (they're testing handler-side reads, which the blob model
  preserves on the wire). The Spin local seed already runs
  `app-demo-cli config push --adapter spin --local --no-env`
  (PR #269 F5); that command's writer is what changes shape.
- **Per-adapter manifests.**
  `examples/app-demo/crates/app-demo-adapter-{fastly,
cloudflare, spin}/{fastly,wrangler,spin}.toml` — local-server
  / per-adapter store declarations that hold a snapshot of the
  typed config (Fastly's `[local_server.config_stores]`,
  Spin's `runtime-config.toml`) re-shape to hold the single
  blob key + JSON value. PR #269 F6 already centralised the
  config-store local-server writer via `config push --local`;
  that writer changes shape.
- **Scaffold templates — every relevant file changes.**
  `crates/edgezero-cli/src/templates/` ships handlebars
  templates that `edgezero new` renders. Each one this
  redesign affects:
  - **`core/src/lib.rs.hbs`** — if `app!` macro examples or
    handler skeletons reference the per-leaf
    `ctx.config_store_default()?.get(...)` pattern, swap
    for `AppConfig<C>` extractor usage.
  - **`core/src/handlers.rs.hbs`** — the sample handler
    drops any `secret_store.require_str(&cfg.<field>)`
    call (was a footgun in earlier scaffolds);
    `cfg.<secret_field>` is used DIRECTLY since the
    extractor resolved it. Imports add
    `use edgezero_core::extractor::AppConfig;`.
  - **`core/src/config.rs.hbs`** — generated
    `<NameConfig>` struct field comments call out that
    `#[secret]` field RUNTIME values are resolved
    secrets, not key names. Macro emits the new
    `SECRET_FIELDS` shape (§3.3.1.1) automatically — no
    handlebars change for the metadata itself.
  - **`app/name.toml.hbs`** — comments in the generated
    `<name>.toml` describe "this whole file becomes a
    single JSON blob; `#[secret]` values here are key
    NAMES in the secret store (the runtime resolves them
    on the fly)".
  - **`cli/src/main.rs.hbs`** — the generated `<name>-cli`
    main wires the new `TypedConfigCmd` enum per §3.2.2.
    The full target shape is `Push` →
    `run_config_push_typed::<C>` + `Diff` →
    `run_config_diff_typed::<C>` (new) + `Validate` →
    `run_config_validate_typed::<C>`. **Implementation
    phasing note (round-26 M-1):** §13's atomic cutover
    commits the runtime extractor + writers + app-demo
    - scaffold templates together, but `ConfigDiffArgs`
    - `run_config_diff_typed` are a post-cutover
      additive (§13 Commit D). To keep the cutover
      commit's scaffold output compilable, the cutover
      commit ships the template's `TypedConfigCmd` enum
      declaring `Push(ConfigPushArgs)` +
      `Validate(ConfigValidateArgs)` ONLY; the same
      commit that ships `ConfigDiffArgs` +
      `run_config_diff_typed` (Commit D) adds the
      `Diff(ConfigDiffArgs)` variant + dispatch arm to
      this template. Net effect: a generated project from
      Commit C has `config push` + `config validate`; a
      generated project from Commit D adds `config diff`
      purely additively. The bundled `edgezero` binary's
      `main.rs` (NOT a template; lives at
      `crates/edgezero-cli/src/main.rs`) keeps `Push` and
      `Diff` as STUB-POINTER TUPLE-variant arms whose
      tuple element is the hidden catch-all
      `ConfigCmdStubArgs` (per §3.2.2) — the match arms
      print the typed-CLI pointer and exit 2; the
      catch-all absorbs whatever flags clap saw, and
      `after_help` attached to each variant covers the
      explicit `--help` path. NOT `ConfigPushArgs` /
      `ConfigDiffArgs` (those are the typed-CLI Args
      structs). Note that the bundled binary's stub
      variants stay STABLE across Commit C and Commit D
      — only the SCAFFOLD template's `TypedConfigCmd`
      enum grows the new variant in Commit D.
  - **`root/edgezero.toml.hbs`** — comments around the
    generated `[stores.config]` block call out that the
    config store now carries a single key (default
    `app_config`) and that per-environment overrides
    use the `__KEY` env var (§5.2).
  - **`root/README.md.hbs`** — the "Configuration"
    section's `<name>-cli config push` example shows the
    inline diff prompt (§8.2). Any reference to
    `edgezero config push` against the bundled binary is
    replaced with `<name>-cli config push` (the bundled
    stub still prints the pointer if invoked, but the
    README no longer documents it as a usable path).
  - **Adapter-specific template files** under
    `crates/edgezero-adapter-*/src/templates/` — if the
    generated `wrangler.toml` / `fastly.toml` / `spin.toml`
    /`axum.toml` snippets contain per-leaf config-store
    seed comments, rewrite for the blob model.

  The generated-project compile gate
  (`cargo test -p edgezero-cli --test
generated_project_builds -- --ignored`) confirms the
  rendered output still compiles under the new templates.
  The grep gate (§10.2.1) ALSO scopes
  `crates/edgezero-cli/src/templates` and
  `crates/edgezero-adapter-*/src/templates`, so a
  generated project that still uses the legacy pattern
  fails CI the same way an unmigrated app-demo handler
  would.

The PR's commit message lists every file in this section so the
reviewer can audit completeness.

**Store-id charset audit (round-22 I-1).** Per §10.3,
manifest schema does change in ONE narrow way: `[stores.*]`
ids drop hyphens. Implementer runs this one-liner to
verify no in-tree manifest, scaffold template, smoke
script, or doc fixture uses a hyphenated store id, and
to find every site that needs renaming:

```sh
# Look for `ids = [..., "x-y", ...]` and `default = "x-y"`
# style entries with hyphens. False positives are easy to
# audit by hand.
grep -RnE '(ids = \[[^\]]*"-?[a-z0-9]+-[a-z0-9]+"|default = "-?[a-z0-9]+-[a-z0-9]+")' \
  examples/ crates/edgezero-cli/src/templates \
  crates/edgezero-adapter-*/src/templates docs/ \
  || echo "OK: no hyphenated store ids found"
```

App-demo, the four adapter templates, and the migration-
guide examples should already use underscore-only ids
(`app_config`, `feature_flags`, `sessions`, …) per the
existing convention. The grep is a belt-and-suspenders
check against drift.

### 10.2.1 Acceptance gate — `verify_no_legacy_typed_reads`

A new `scripts/check_no_legacy_typed_reads.sh` runs in CI on
every push. It greps the example tree and scaffold templates
for FOUR patterns that the redesign removes (three legacy
read forms plus the v1 nested-`AppConfig` ban from
§3.3.1.2), plus does a positive check that the new entry
point is present:

```sh
#!/usr/bin/env bash
# Fails if any in-tree consumer still reads the typed app-config
# via the per-leaf ConfigStore::get path, OR if any in-tree
# handler still resolves secret fields by hand (the framework
# resolves them under Model A; see §3.3.3).
set -euo pipefail

SCOPES=(
  examples/app-demo
  crates/edgezero-cli/src/templates
  crates/edgezero-adapter-axum/src/templates
  crates/edgezero-adapter-cloudflare/src/templates
  crates/edgezero-adapter-fastly/src/templates
  crates/edgezero-adapter-spin/src/templates
)

violations=""

# Comment-skip helper. grep -rEn prefixes every line with
# `path:line:` so a naive `grep -vE '^\s*//'` filter never
# matches commented lines (the prefix breaks the
# beginning-of-line anchor). Use a colon-then-whitespace
# anchor that survives the prefix.
not_comment() { grep -vE ':\s*//'; }

# Pattern 1: single-line `config_store_default()?.get(` — the
# canonical legacy form on app-demo + the scaffold templates.
violations+=$(
  grep -rEn 'config_store_default\(\)\?\.get\(' "${SCOPES[@]}" 2>/dev/null \
    | not_comment || true
)

# Pattern 2 (handler-side typed reads, two-step form). Targets
# ONLY the known typed-config handler files so legitimate
# hand-managed reads in OTHER files (per §10.3) don't false-
# positive. The round-3 review caught that a broad
# `config_store_default|config_store(` regex sweeps up valid
# hand-managed call sites; this narrower scope keeps the gate
# specific.
TYPED_HANDLER_FILES=(
  examples/app-demo/crates/app-demo-core/src/handlers.rs
  crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs
)
for f in "${TYPED_HANDLER_FILES[@]}"; do
  [ -f "$f" ] || continue
  # Any reference to config_store_default / config_store(...)
  # in a TYPED-HANDLER file means the migration left a legacy
  # read behind — hand-managed reads belong in NON-handler
  # files (§10.3).
  violations+=$(
    grep -En '\b(config_store_default|config_store\()' "$f" 2>/dev/null \
      | not_comment | sed "s|^|$f:|" || true
  )
done

# Pattern 3: handler-side secret resolution. Under Model A the
# extractor populates secret values; any handler still calling
# `secret_store.require_str(&cfg.<field>)` against an app-config
# secret field is using the legacy explicit-lookup model.
violations+=$(
  grep -rEn 'require_str\(&cfg\.' "${SCOPES[@]}" 2>/dev/null \
    | not_comment || true
)

# Pattern 4: nested `AppConfig`-derived types inside another
# `AppConfig` struct. v1 bans this (§3.3.1.2): a type that
# derives `AppConfig` MUST NOT be used as a field type in
# another `AppConfig`-derived struct (directly OR wrapped in
# `Option<T>` / `Vec<T>` / `Box<T>` / tuples / arrays).
#
# Shell-grep approaches can't reliably detect either of the
# two relevant patterns:
#
#   - Multi-line `#[derive(...)]` where `AppConfig` lands on
#     a continuation line:
#         #[derive(
#             Debug,
#             edgezero_core::AppConfig,
#         )]
#     A line-anchored regex misses this (the `derive` token
#     is on a different line from `AppConfig`).
#
#   - Generic-wrapped uses: `field: Option<ChildConfig>`,
#     `field: Vec<ChildConfig>`, `field: Box<ChildConfig>`,
#     `(ChildConfig, OtherTy)` tuple fields, `[ChildConfig;
#     N]` array fields. A `: <Ident>` regex misses all of
#     these.
#
# The CI gate calls a small Rust helper that runs `syn`'s
# AST walk over every `.rs` file in `SCOPES`,
# collects every struct that derives `AppConfig` (handling
# multi-line derives via real token parsing), then walks
# every other `AppConfig` struct's fields and reports any
# field-type that mentions one of the collected idents at
# any depth of nesting (`syn::Type::visit`-style recursion).
#
# IMPORTANT: the helper does NOT parse `.rs.hbs` files as
# raw Rust. Templates contain unrendered Handlebars (e.g.
# `pub struct {{NameUpperCamel}}Config {` at
# `crates/edgezero-cli/src/templates/core/src/config.rs.hbs:20`,
# the `{{#each ...}}` block at
# `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs:18`,
# the `{{name}}` interpolation at
# `crates/edgezero-adapter-axum/src/templates/src/main.rs.hbs:4`)
# which `syn::parse_file` rejects with a syntax error. The
# helper's scope is `.rs` only; the gate covers templates
# through a SEPARATE rendered-template fixture pass
# described below.
#
# The helper lives at
# `crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
# (NOT `scripts/` — a top-level `scripts/` file would force a
# `[[bin]] path = "..."` entry pointing OUTSIDE the package's
# `src/` tree, which Cargo allows but is fragile against
# workspace path changes). Living inside `edgezero-cli`
# keeps the helper inside an existing workspace member, with
# `syn` / `walkdir` visible only to this bin.
#
# Cargo registration:
#
#   [features]
#   nested-app-config-check = ["dep:syn", "dep:walkdir"]
#
#   [[bin]]
#   name = "check_no_nested_app_config"
#   path = "src/bin/check_no_nested_app_config.rs"
#   required-features = ["nested-app-config-check"]
#
# The `[[bin]]` entry is REQUIRED even though
# `src/bin/*.rs` is normally auto-discovered: without it,
# `required-features` cannot be attached, and `cargo
# build` without the feature would try to compile the
# helper (and fail with missing `syn` / `walkdir`). The
# explicit `[[bin]]` lets Cargo skip the helper when the
# feature is off and pull in the optional deps only when
# CI enables it.
#
# CI invokes:
#
#   cargo run -q --bin check_no_nested_app_config \
#     --features nested-app-config-check \
#     -- "${SCOPES[@]}"
#
# The feature gate keeps `syn` + `walkdir` out of the
# default `edgezero-cli` build — they're only pulled in
# when CI explicitly enables `--features
# nested-app-config-check`. The bin's
# `required-features` declaration makes that explicit at
# Cargo level (running `cargo build` without the feature
# simply skips the bin).
#
# Behaviour: exits 0 on no violations; exits 1 with a list
# of `<file>:<line>:<field>` lines on violations; exits 2 on
# syntax errors in a `.rs` file (refuses to silently pass on
# unparseable input). The helper is ~120 LOC built on
# `syn` (for AST parsing) + `walkdir` (for file traversal).
#
# IMPORTANT (shell gotcha): under `set -euo pipefail` at the
# top of the script, a nonzero command substitution would
# exit the script immediately, so `cmd; status=$?` does NOT
# work for capturing nonzero exit codes. The idiom below
# uses an `if`/`else` block, which suppresses the `set -e`
# trip and lets the script inspect the status.
if nested_helper_output=$(
     cargo run -q --bin check_no_nested_app_config \
       --features nested-app-config-check \
       -- "${SCOPES[@]}" 2>&1
   ); then
  nested_helper_status=0
else
  nested_helper_status=$?
fi
case "$nested_helper_status" in
  0) ;;
  1)
    violations+="\nPattern 4 — nested \`AppConfig\` rooted type used as a field:\n${nested_helper_output}"
    ;;
  *)
    # Exit 2 (syntax error) or any unexpected status — fail loud.
    echo "ERROR: check_no_nested_app_config exited with status ${nested_helper_status}:" >&2
    echo "${nested_helper_output}" >&2
    exit 1
    ;;
esac

# Why a Rust helper, not awk: an earlier draft attempted POSIX
# awk windowing. Both the multi-line-derive case and the
# generic-wrapped-field case escape that approach (see the
# comment block at the top of this Pattern). A real AST walk is
# the only way to honor the spec's "any depth of nesting"
# clause. The helper is small, has no new runtime deps beyond
# `syn` (already in-tree), and runs in <2s on the full SCOPES
# tree.

# Pattern 4 cont'd: rendered-template coverage. The syn-based
# helper above intentionally skips `.rs.hbs` files (unrendered
# Handlebars is not valid Rust). To cover templates, the spec's
# acceptance test (§10.2.2 #1, "generated output compiles
# cleanly") is paired with an integration test in
# `crates/edgezero-cli/tests/scaffold_render.rs` that:
#
#   1. Renders every `*.rs.hbs` template under
#      `crates/edgezero-cli/src/templates/` and every
#      `crates/edgezero-adapter-*/src/templates/` using the
#      deterministic fixture context `{name = "test-scaffold",
#      NameUpperCamel = "TestScaffold", EnvPrefix = "TEST_SCAFFOLD"}`
#      (matches the `generated_project_builds --ignored` fixture).
#   2. Writes the rendered output into a temp directory.
#   3. Invokes the same `check_no_nested_app_config` binary on
#      that temp directory.
#   4. Asserts the helper exits 0 (no nested-`AppConfig` patterns
#      in any rendered template).
#
# This catches the "scaffolded template silently introduces a
# nested AppConfig" case that the `.rs`-only CI grep can't see,
# without forcing a Handlebars parser into the helper. The
# integration test lives in CI as part of `cargo test
# --workspace`, so it runs alongside the §10.2.1 shell gate.

# Positive check: at least one handler file USES the AppConfig
# extractor at the type level. Confirms the migration actually
# wired the new extractor; a repo with no `AppConfig<...>` use
# means the migration didn't happen, even if the negative grep
# passed for unrelated reasons.
#
# We check USAGE shapes (`AppConfig<`, `AppConfig(`), NOT the
# import line. The app-demo handlers at
# `examples/app-demo/crates/app-demo-core/src/handlers.rs:8`
# use a GROUPED extractor import
# (`use edgezero_core::extractor::{Headers, Json, Kv, Path,
# Query, Secrets, ValidatedPath};`), and a future migration
# would just add `AppConfig` to that group — a grep for the
# exact line `use edgezero_core::extractor::AppConfig` would
# false-fail on a correct migration. The usage-shape grep is
# robust to either import style (grouped OR single-import).
#
# Scope ONLY to `examples/app-demo/` — scaffold templates
# carry the import + usage in a COMMENTED block (§10.2.2's
# scaffold rules) to avoid `-D warnings` on the generated
# project, so they would satisfy a tree-wide grep regardless
# of whether the real migration landed. `not_comment` further
# filters any stray commented lines in app-demo.
if ! grep -rEn 'AppConfig[<(]' examples/app-demo 2>/dev/null \
    | not_comment | grep -q .; then
  echo "ERROR: no in-tree consumer in examples/app-demo USES AppConfig" >&2
  echo "(searched for an \`AppConfig<...>\` or \`AppConfig(...)\` token)." >&2
  echo "The migration to the blob model wires AppConfig<C>; app-demo handlers" >&2
  echo "should be using it." >&2
  exit 1
fi

if [ -n "${violations// /}" ]; then
  echo "ERROR: legacy typed-config reads or secret resolutions found:" >&2
  echo "$violations" >&2
  echo >&2
  echo "Switch each typed-config read to the AppConfig<C> extractor" >&2
  echo "(see §6 of the spec). Drop handler-side require_str(&cfg.<field>)" >&2
  echo "calls; the framework resolves #[secret] fields (see §3.3.4)." >&2
  exit 1
fi
echo "OK: no legacy typed-config reads or secret resolutions in scoped trees."
```

The gate has two false-positive fixes baked in (both from
round-6 review):

- **Comment-skip uses `:\s*//` anchor**, not `^\s*//`. `grep
-rEn` prefixes every line with `path:line:`, so an `^`
  anchor can never see whitespace+`//` — the old filter was
  silently inert. The colon-then-whitespace pattern survives
  the prefix.
- **Pattern 2 is scoped to KNOWN typed-handler files**, not
  the whole tree. Targeting only
  `examples/app-demo/crates/app-demo-core/src/handlers.rs`
  and `crates/edgezero-cli/src/templates/core/src/handlers.rs.hbs`
  keeps legitimate hand-managed reads (per §10.3) from
  failing CI when they coexist with typed reads in
  unrelated modules.

The scope excludes `crates/edgezero-adapter-*/src/` (raw
`ConfigStore::get` IS still valid for hand-managed entries —
see §10.3) and the spec doc itself (which by definition
mentions the legacy pattern).

**Why three patterns and a positive check.** The round-4
reviewer caught that the single-line `config_store_default()?
.get(` grep misses app-demo's two-step `let store =
ctx.config_store_default()?; ...; store.get(...)` pattern.
Pattern 2 catches it (scoped to typed-handler files) and
Pattern 3 (`require_str(&cfg.<field>)`,
the secret-resolution call site that Model A retires) closes
that hole. The positive check catches the "everyone migrated
to use NOTHING from the store layer" failure mode where the
PR accidentally removed all typed-config reads without
introducing the new extractor.

CI wires this as the LAST step in the test workflow, so it
runs after `cargo test` proves the structural change compiles

- tests pass. The pre-PR-#269 review process taught us that
  "did you migrate everything?" is a question the reviewer
  keeps re-asking; this gate answers it deterministically.

### 10.2.2 Scaffold template changes — concrete content

The §10.2 bullet list enumerates WHICH files change. The
table below pins down WHAT each one's generated output looks
like under the blob model. Implementer + reviewer use this as
the acceptance target for the scaffold migration.

#### `core/src/config.rs.hbs`

The current template (`config.rs.hbs:21-54`) has commented-out
`#[secret]` and `#[secret(store_ref)]` examples whose
explanatory text says the handler resolves via
`ctx.secret_store_default()?.require_str(&cfg.api_token)`.
That call site no longer exists under Model A — the framework
resolves at extract time.

Replace the secret-block comment (lines 32-54) with:

```rust
// `#[secret]` — uncomment when the project declares
// `[stores.secrets]` in `edgezero.toml` (with at least a
// `default` id). At rest in `{{name}}.toml`, the value is
// the *key name* in the default secret store. At RUNTIME,
// the framework swaps it for the resolved secret value
// before the handler sees `cfg.api_token`. The handler reads
// the secret directly:
//
//     async fn handler(AppConfig(cfg): AppConfig<{{NameUpperCamel}}Config>) {
//         use_token(&cfg.api_token); // already the resolved secret
//     }
//
// `config validate --strict` rejects the field unless
// `[stores.secrets]` is declared; this is opt-in to keep the
// scaffold's `serve` path runnable out of the box.
//
// #[secret]
// pub api_token: String,
//
// `#[secret(store_ref)]` — uncomment when the project
// declares more than one secret store id under
// `[stores.secrets].ids`. The value is the logical id of
// the secret store; carried through to `cfg.vault` at
// runtime UNCHANGED (no framework swap — this field is a
// store pointer, not a secret key).
//
// #[secret(store_ref)]
// pub vault: String,
//
// `#[secret(store_ref = "vault")]` — uncomment when the
// project has multiple secret stores AND you want this
// specific secret to come from the store named by
// `cfg.vault`. The framework reads `cfg.vault` (which must
// itself carry `#[secret(store_ref)]`), opens that secret
// store, and looks up the secret-key NAME under it.
//
// #[secret(store_ref = "vault")]
// pub multi_store_api_token: String,
```

#### `core/src/handlers.rs.hbs`

The current handler skeleton has no secret-store reads
(it's about routing/echo/proxy), so under Model A no
handler line changes mechanically. ADD a commented-out
sample handler that DEMONSTRATES the new ergonomics — the
scaffold's documentation surface is where operators learn
the model. Insert at the bottom of the routes section:

```rust
// `cfg.api_token` is the RESOLVED secret value (not the
// key name). The framework's secret walk populated it at
// extract time; the handler uses it directly. No
// `secret_store.require_str(...)` call — the framework
// owns the lookup.
//
// #[action]
// pub async fn upstream() -> Result<Response, EdgeError> {
//     // Uncomment when AppConfig has `#[secret] api_token`.
//     // let AppConfig(cfg) = ...; // via extractor injection
//     // make_upstream_call(&cfg.api_token).await?;
//     Ok(text("upstream sample"))
// }
```

This stays commented so the default scaffold still builds
without `[stores.secrets]`.

**Import handling.** Since the sample handler is commented
out, the `use edgezero_core::extractor::AppConfig;` import
also stays COMMENTED in the same block as the sample —
otherwise `-D warnings` clippy fails on an unused import in
the generated project. Both lines uncomment together when
the operator activates the sample:

```rust
// // Uncomment together with the `upstream` handler above:
// use edgezero_core::extractor::AppConfig;
```

The §10.2.1 CI gate's positive check greps for the
USAGE shape `AppConfig[<(]` (extractor instantiation
in a handler signature or pattern destructure — see
the script in §10.2.1 for the exact regex). It is
robust to grouped imports
(`{Headers, Json, Kv, AppConfig, ...}`) and single
imports alike, and is scoped to `examples/app-demo/`
— app-demo's handlers actively instantiate the
extractor, so the usage-shape match fires there. The
scaffold templates are not in scope of the positive
check; their commented import + commented sample
handler don't satisfy the usage match, but they also
don't need to (the gate runs against the in-tree
example, not the template). Earlier draft wording
said the gate greps for the `use
edgezero_core::extractor::AppConfig` literal —
round-9 already switched to usage-shape matching; the
round-17 reviewer caught the stale text here.

#### `app/name.toml.hbs`

The current template (`name.toml.hbs:21-29`) says the
runtime resolves via
`ctx.secret_store_default()?.require_str(&cfg.api_token)`.
Under Model A the framework resolves at extract time, so
the handler never calls `require_str`. Replace lines 21-29
with:

```toml
# When you uncomment `#[secret] api_token` in the AppConfig
# struct (see `crates/{{proj_core}}/src/config.rs`), the
# matching key here is the *key name* in the default secret
# store -- NOT the secret bytes. The framework's secret walk
# resolves it at extractor time; in handler code,
# `cfg.api_token` is already the resolved secret value.
# Uncomment alongside the corresponding `[stores.secrets]`
# block in `edgezero.toml`.
#
# api_token = "demo_api_token"
```

#### `cli/src/main.rs.hbs`

The current template (`main.rs.hbs:55-63`) defines
`{{NameUpperCamel}}ConfigCmd` with `Push` + `Validate`
variants. Under Model A this grows a `Diff` variant per
§3.2.2. The match arm gains a typed-diff dispatch.

**Imports** (line 14-17) — add `ConfigDiffArgs`:

```rust
use edgezero_cli::args::{
    AuthArgs, BuildArgs, ConfigDiffArgs, ConfigPushArgs,
    ConfigValidateArgs, DeployArgs, NewArgs, ProvisionArgs,
    ServeArgs,
};
```

**Enum** (line 55-63) — add `Diff(ConfigDiffArgs)`:

```rust
#[derive(Subcommand, Debug)]
enum {{NameUpperCamel}}ConfigCmd {
    /// Render the diff between `{{name}}.toml` and the
    /// remote-or-local blob. Skip-on-equal short-circuits
    /// the push path when the local blob matches what's
    /// already in the store. See spec §8.
    Diff(ConfigDiffArgs),
    /// Push `{{name}}.toml` (as a single JSON blob with
    /// embedded SHA-256) to the adapter's config store.
    /// Shows the inline diff by default; `--yes` skips
    /// the prompt; `--dry-run` renders the diff without
    /// writing.
    Push(ConfigPushArgs),
    /// Validate `edgezero.toml` and `{{name}}.toml` against
    /// the typed `{{NameUpperCamel}}Config` contract.
    Validate(ConfigValidateArgs),
}
```

**Doc comment** (line 48-54) — drop the "secret-stripped"
phrasing (no strip under Model A) AND mention the diff:

```rust
/// Mirrors the bundled binary's `Validate`-only `ConfigCmd`
/// but adds `Push` and `Diff` parameterised over
/// `{{NameUpperCamel}}Config` — the downstream project owns
/// the struct, so it can enforce the typed deserialise,
/// `validator` rules, and `#[secret]` / `#[secret(store_ref)]`
/// / `#[secret(store_ref = "...")]` checks the bundled
/// binary's validate-only path can't run.
```

**Match arm** (line 69-82) — add the diff branch:

```rust
let result = match Args::parse().cmd {
    Cmd::Auth(args) => edgezero_cli::run_auth(&args),
    Cmd::Build(args) => edgezero_cli::run_build(&args),
    Cmd::Config({{NameUpperCamel}}ConfigCmd::Diff(args)) => {
        edgezero_cli::run_config_diff_typed::<{{NameUpperCamel}}Config>(&args)
    }
    Cmd::Config({{NameUpperCamel}}ConfigCmd::Push(args)) => {
        edgezero_cli::run_config_push_typed::<{{NameUpperCamel}}Config>(&args)
    }
    Cmd::Config({{NameUpperCamel}}ConfigCmd::Validate(args)) => {
        edgezero_cli::run_config_validate_typed::<{{NameUpperCamel}}Config>(&args)
    }
    Cmd::Deploy(args) => edgezero_cli::run_deploy(&args),
    Cmd::New(args) => edgezero_cli::run_new(&args),
    Cmd::Provision(args) => edgezero_cli::run_provision(&args),
    Cmd::Serve(args) => edgezero_cli::run_serve(&args),
};
```

#### `root/edgezero.toml.hbs`

If the generated `[stores.config]` block carries comments
about per-leaf push semantics, replace with:

```toml
# Single-blob model: `<app-cli> config push` writes the
# typed `{{name}}.toml` as ONE JSON blob (with embedded
# SHA-256) under key `{{default_config_key}}` in this
# store. Per-environment overrides use the
# `EDGEZERO__STORES__CONFIG__{{ID_UPPER}}__KEY` env var
# at runtime (see spec §5).
```

#### `root/README.md.hbs`

The "Configuration" section's `<name>-cli config push`
example shows the inline diff prompt. Replace any
`edgezero config push` reference (the bundled binary's
`config push` is a stub-pointer subcommand per §3.2.1 —
the typed CLI is the real entry point) with
`<name>-cli config push`.

#### Per-adapter templates

The four `crates/edgezero-adapter-*/src/templates/` trees
hold per-platform manifests. Anywhere they declare local-
server config-store seeding (Fastly's
`[local_server.config_stores]` block, Spin's
`runtime-config.toml`), update the comments to say "this
holds a single JSON blob, not flattened leaves". The
generated MANIFEST shape itself doesn't change (per-adapter
config-store declaration is still per-id, not per-key); only
the comment + the seed shape change.

No content edits required in the adapter-specific Rust
templates (`src/main.rs.hbs`, `src/lib.rs.hbs`) — those wire
the runtime entry point, not config-store reads.

#### Acceptance gate

For every `.hbs` file listed above, the implementer asserts:

1. The generated output (after handlebars render with
   `app-demo` as `{{name}}`) compiles cleanly.
2. The grep gate (§10.2.1) finds no legacy patterns in
   the rendered output.
3. `cargo test -p edgezero-cli --test
generated_project_builds -- --ignored` (the existing
   end-to-end render-and-build test) passes.

### 10.3 What does NOT migrate (and what DOES at the manifest level)

- Hand-managed `ConfigStore` entries (the `ConfigStore` trait
  itself stays — it's still useful for ad-hoc raw KV reads
  that aren't the typed app-config).
- The KV store (`[stores.kv]`). KV is application data, not the
  typed app-config; its shape STRUCTURE doesn't change —
  but the store-id charset does, see below.
- The secret store (`[stores.secrets]`). Secrets stay
  out-of-blob per §3.2 — but the store-id charset
  changes, see below.

**Manifest schema DOES change in one narrow way:**
per §5.2, store ids in `[stores.kv]`, `[stores.config]`,
and `[stores.secrets]` tighten from `[A-Za-z0-9_-]` to
`[A-Za-z0-9_]+` (hyphens rejected — they break POSIX
shell `export` of the
`EDGEZERO__STORES__<KIND>__<ID>__KEY` overrides). This
is a hard-cutoff manifest-schema change per §1: operators
with hyphenated store ids (`feature-flags`,
`session-cache`, …) MUST rename them in `edgezero.toml`
AND in any matching `[adapters.<name>.*]` binding /
namespace declarations BEFORE the blob model merges.
§12.18 covers the validator rejection; the migration
guide carries a sed/grep recipe in §10.2's bullet list
so the operator can find every reference in one pass.
Round-22 reviewer correctly flagged that earlier draft
wording ("manifest schema unaffected") understated this
change; the wording is now narrow and accurate.

### 10.4 No platform-side bridge

This is a hard cutoff with **no platform support for a
transition window**:

- The runtime does not read pre-blob (flat-leaf) state under
  any circumstance. There is no "if the value isn't a valid
  envelope, fall back to per-leaf reads" path.
- The CLI does not write pre-blob and post-blob shapes
  simultaneously. `config push` writes the blob and nothing
  else.
- The app is responsible for its OWN schema-evolution story.
  Adding a non-`Option` field to `AppConfig` and pushing the
  old `<name>.toml` deserialises into a struct missing that
  field — that's a serde failure, not a platform failure.
  Use `#[serde(default)]`, struct versioning, or
  `Option<NewField>` per your taste.
- Operators are responsible for the deploy-vs-push ordering.
  Two reasonable patterns:
  - **Drain-and-flip.** Stop traffic, push, deploy, restore.
    Simplest. Brief downtime.
  - **Blue-green.** Push to staging key, deploy new code
    pointing at staging key via `__KEY`, swap routing, then
    repoint at the default key. Zero downtime if your
    infrastructure supports the routing flip.

### 10.5 Stale per-leaf orphans

Migrating leaves the pre-rewrite per-leaf state SITTING in the
cloud KV / Fastly Config Store / Spin label. The new code
ignores them, but they:

- Count against platform quotas (Cloudflare KV per-namespace
  caps, Fastly Config Store entry counts).
- Show up in `wrangler kv list` / `fastly config-store-entry
list` and confuse operators auditing state.
- Leak data: a previously-pushed feature flag value lingers
  even after the app version that read it shipped.

**v1 stance: orphans stay; cleanup is manual.**

This is consistent with the "no migration aid" stance from §10.
Adding a `config push --cleanup-legacy` flag would require
extending the `Adapter` trait with list/delete methods that
exist neither today nor anywhere else in the spec — read-back
(§9.0) is the only new read surface, and adding list+delete
just to support a one-time cleanup is disproportionate.

The migration guide instead carries per-adapter cleanup
recipes the operator runs once (manually, after verifying the
blob is correct):

- **Cloudflare:** `wrangler kv key list --binding <BINDING>
--remote` to see what's there, then `wrangler kv key delete
--binding <BINDING> --remote <KEY>` per leaf. Easy to
  script.
- **Fastly:** `fastly config-store-entry list --store-id=<id>
--json | jq` to identify the per-leaf entries, then
  `fastly config-store-entry delete --store-id=<id>
--key=<key>` per leaf.
- **Spin (local):** `sqlite3 .spin/sqlite_key_value.db
"DELETE FROM spin_key_value WHERE store = '<label>' AND key
!= 'app_config'"` (substitute the actual default key from
  §5).
- **Spin (Cloud): no per-key cleanup through the current
  Spin Cloud CLI.** Per the Fermyon Cloud command
  reference, `spin cloud key-value` exposes
  `create` / `delete` / `list` / `rename` / `set` —
  `list` enumerates STORES, `delete` removes a STORE,
  and `set` is the only per-key write; there is no
  per-key list or delete. Three options for the
  operator post-migration:

  1. Leave the pre-migration leaf keys in place
     (they're orphan bytes the runtime no longer
     reads). The blob-model runtime only reads the
     `app_config` blob key, so orphan leaves cause
     no incorrect behaviour — just storage waste.
  2. Recreate the store from scratch: `spin cloud
key-value delete <STORE>` followed by
     `spin cloud key-value create <STORE>`, then
     re-run `<app-cli> config push` to seed the
     blob. Loses any other data the store held;
     only safe if the store is dedicated to this
     app's config.
  3. Use the Fermyon Cloud dashboard / HTTP API
     (if/when documented) to enumerate + delete
     individual keys. Out of scope for v1; tracked
     as a follow-up.

  Earlier draft suggested
  `spin cloud key-value delete --app <APP> --label
<LABEL> <KEY>` per leaf — round-20 reviewer
  cross-checked against the Fermyon command
  reference and found that form does NOT exist. The
  recipe above reflects the actual CLI surface.

The migration guide includes a one-liner that lists the
expected per-leaf key shapes (it's the result of
`flatten_typed_app_config` against the pre-rewrite
`<name>.toml`), so the operator has a reference of what to
look for. Cleanup is one shell session per environment, not a
recurring concern.

**Why not the flag.** Even the opt-in variant requires the
adapter trait to know how to LIST and DELETE entries, which
the spec deliberately doesn't introduce. Manual cleanup keeps
the trait surface minimal (read for diff, write for push) and
trades one shell session for a smaller maintained surface.

### 10.6 Concurrent pushes

`config push` is **not safe under concurrent operators against
the same store + key**. Two operators racing:

- Both compute their own local sha.
- Both read the (same) remote sha.
- Both decide their local differs from remote.
- Both write. Last-writer-wins.

The loser's intended change is silently dropped. Per-adapter,
the platform's KV layer is the source of truth on ordering —
none of them offer compare-and-swap that EdgeZero could use to
make this safer.

**v1 stance:** document the restriction. Operators coordinate
via their existing deploy pipeline (one push per pipeline
stage; mutually-exclusive locks at the pipeline level if
multi-pipeline). Operators running `config push` from a laptop
do so KNOWING no one else is pushing.

Future work: add a `--cas <expected-sha>` flag that includes
the expected previous sha in the push, and have the writer
abort if the actual remote sha doesn't match. Out of scope for
v1; needs adapter-level conditional-write support that today's
KV CLIs don't uniformly expose.

### 10.7 Rollback

There is no platform-built rollback. The operator's recovery
path:

1. Revert the offending `<name>.toml` change in git.
2. Re-run `<app-cli> config push --adapter <name>`.
3. Push lands the previous content; runtime reads it on the next
   request.

Recommended habit: keep `<name>.toml` under version control
(scaffold already does this — `.gitignore` does NOT include it),
and tag each successful push in the deploy pipeline so reverting
is one git operation.

## 11. Open questions

These are decisions I've punted to the reviewer. Each has a
default I'd ship if the reviewer doesn't redirect — flagged with
**default:**.

### Q1. SHA-canonicalisation dependency — resolved

The canonical form needs stable key ordering for JSON serialisation.

- (a) Pull in `serde_canonical_json`.
- (b) Write our own canonicaliser (~150 LOC, no new
  external dep beyond what's in-tree).
- **Resolved: (b).** Round-21 reviewer probed
  `serde_canonical_json` v1.0.0 and found it rejects
  finite floats (`Floating point numbers are
forbidden`). §4.2 explicitly requires finite-float
  support via `ryu` (per the "Numeric values" rule),
  so (a) cannot implement this spec unchanged. The
  hand-rolled walker at
  `crates/edgezero-core/src/canonical_form.rs` is the
  v1 default — see §4.2's "Stability across
  implementations" block for the concrete acceptance
  criteria. Earlier drafts of the spec left this
  question open; round-21 pinned it to (b) because
  (a) is incompatible AND the in-tree walker is
  short enough (~150 LOC) that no external dep is
  cheaper to maintain.

### Q2. Diff format

`config diff`'s default `--format`:

- (a) `unified` — git-style, terminal-friendly.
- (b) `structured` — YAML tree, nested-aware.
- (c) `json` — machine-readable.
- **default:** (a) `unified`. Most calls are interactive; the
  other two are available via `--format`.

### Q3. Missing-blob behaviour in `AppConfig<C>`

What happens when the store has no entry at the requested key?

- (a) Return `EdgeError::Internal` so the operator notices.
- (b) Return `EdgeError::ServiceUnavailable`.
- (c) Add a sibling extractor `MaybeAppConfig<C>` returning
  `Option<C>` for endpoints that have a sensible default.
- (d) Return `EdgeError::ConfigOutOfDate` — matches the
  "re-run `<app-cli> config push` fixes it" rule
  spelled out for `ConfigOutOfDate` at §6.3.
- **default:** (d) for v1 (round-18 M-2 reversal of the
  earlier (a) pick). The §6.3 rationale defines
  `ConfigOutOfDate` as "the situation a re-run of
  `<app-cli> config push` resolves" — and a missing
  blob is squarely in that bucket: the deployed code
  expects a blob, the store doesn't have one, an
  operator push fixes it. Mapping this to `Internal`
  would page oncall (a 500-class signal) when the
  actionable response is "push the config", which
  `ConfigOutOfDate` (503 with `Retry-After: 60`)
  already encodes. The §3.3.3 extractor's
  `ok_or_else(|| EdgeError::internal("missing typed
app-config blob"))` call site changes to
  `EdgeError::config_out_of_date("missing typed
app-config blob at key `<key>`— run`<app-cli>
  config push`", String::new())` accordingly. (c) is
  still tracked as a follow-up for endpoints that
  want explicit `Option<C>` semantics.

### Q4. Envelope `version` storage

- (a) Inside the envelope (as drafted): `{ "data": {…},
"sha256": "…", "version": 1 }`. The runtime reads the version,
  validates the schema, then unwraps `data`.
- (b) Separate KEY: `app_config__v1`. Per-version blobs.
  Cleaner if we ever need multi-version coexistence; messier
  for the one-key invariant.
- **default:** (a). Multi-version coexistence is out of scope
  per §3.2.

### Q5. In-process caching — resolved

**Resolved: (a) no cache in v1.** §6.4 documents the
rationale. The earlier per-request `OnceLock` proposal didn't
fit `FromRequest`'s immutable extension access; the design
surface to fix that is bigger than the optimization it would
unlock. Re-evaluate after profiling on real workloads.

### Q6. Adapter-side blob size caps

Per-platform value caps as of v1:

| Adapter    | Platform value cap                                     | Source / notes                                                                                                                                                                                                                                                                                                                                                                                                                      |
| ---------- | ------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Axum       | none (local file)                                      | The Axum store is `BTreeMap<String, String>` serialised to a JSON file; cap is filesystem-bound.                                                                                                                                                                                                                                                                                                                                    |
| Cloudflare | 25 MiB                                                 | KV per-value limit (`developers.cloudflare.com/kv/platform/limits`).                                                                                                                                                                                                                                                                                                                                                                |
| Fastly     | **8 000 characters** (~8 KB UTF-8)                     | Fastly Config Store `item_value` limit per Fastly's published platform docs. The writer uses `fastly config-store-entry update --upsert --stdin` at the Fastly adapter (PR #269 F4) so there's no argv ceiling, but the platform itself rejects values over 8 000 characters. **Earlier drafts cited 64 KiB — that was a documentation error; the round-18 reviewer caught it against the official Fastly Config Store item docs.** |
| Spin Cloud | `MAX_ARGV_BYTES_PER_INVOCATION` (`96 * 1024`) per pair | Writer cap at `crates/edgezero-adapter-spin/src/cli/push_cloud.rs:46`; under the blob model the effective limit is the cap minus `<KEY>=` (per §9.4).                                                                                                                                                                                                                                                                               |
| Spin local | sqlite filesystem-bound                                | `<spin.toml dir>/.spin/sqlite_key_value.db`; cap is filesystem-bound.                                                                                                                                                                                                                                                                                                                                                               |

For Fastly specifically:

- (a) Accept the platform cap. Document it. Operators
  with >8 000-character configs need to restructure
  their typed `C` into separate types per logical
  surface (e.g. `BillingConfig` / `FeatureConfig`)
  and wire each through its own
  `[stores.config]` id with its own
  `AppConfig<BillingConfig>` / `AppConfig<FeatureConfig>`
  extractor. The blob model intentionally does NOT
  auto-split one `C` across ids (see §3.2's "no
  multi-blob merge" stance and the round-18 H-1
  reframing); each id holds ONE typed struct. The
  restructure is operator-side schema work — the
  framework provides the multi-id machinery, not the
  split itself.
- (b) Add a `--chunked` flag that splits a too-big blob
  across N sibling keys (`app_config__1`,
  `app_config__2`, …).
- **default:** (a). Chunking re-introduces the
  partial-write hazard the blob model exists to avoid.
  8 000 characters is tight for a single struct but
  workable for app-demo-shaped workloads; operators
  with substantially larger configs either restructure
  per (a) or pick Cloudflare / Spin / Axum.

The Fastly writer adds a pre-platform-call guard that
checks `envelope.len() > 8_000` and errors with an
actionable message naming the per (a) restructure
guidance before incurring the platform error. The
guard mirrors the Spin Cloud cap check (§9.4) but uses
the Fastly-specific limit.

### Q7. Diff against `--local` vs remote

When the operator runs `config diff --adapter cloudflare`, do we
diff against the LOCAL Miniflare state (`.wrangler/state`) or the
REMOTE Cloudflare KV?

- (a) Always remote. Local diff is a separate `--local` flag.
- (b) Mirror `config push`: if a previous `push --local` ran,
  compare against local; else remote.
- **default:** (a). Explicit is better than mirroring a
  potentially-stale flag.

### Q8. Where the `AppConfig<C>` extractor lives

- (a) `edgezero-core::extractor::AppConfig` (sits next to the
  renamed multi-store registry extractor).
- (b) New module `edgezero-core::app_config_extractor` for
  symmetry with `edgezero-core::app_config`.
- **default:** (a). The existing extractors all live in
  `extractor`; the new one stays alongside.

### Q9. Push-side audit log — resolved out of v1 scope

Should `config push` log a structured audit line on every
successful write (operator user / host, local sha, remote sha,
timestamp, store id, key, adapter)?

- (a) Yes, written to the push command's stdout as a structured
  JSON line before the human-readable summary.
- (b) Yes, written to an opt-in `.edgezero/push-audit.log` per
  project.
- (c) No, leave to the deploy-pipeline layer.
- **Resolved: (c) for v1.** Earlier draft defaulted to (a),
  but the round-22 reviewer correctly observed that §8.2
  (push flow), §12.2 (push/diff tests), and §13 (commit
  phasing) carried NO contract for the JSON shape, output
  order, skip-on-equal behaviour, or test assertions. Two
  options to resolve: (i) fully specify the audit contract
  AND wire it into §8.2/§12.2/§13, OR (ii) drop the audit
  log from v1. Picking (ii) — the deploy-pipeline layer
  already has all the data (it knows when push ran, what
  the diff was, what the sha was — the sha is on push
  stdout already per §8.2's success line "pushed N entries
  to … (sha=<hex>)"). Specifying a full audit-log contract
  is a separate work stream; lumping it into v1 widens
  scope without clear value. (a) and (b) are tracked as
  follow-ups; when a real audit requirement lands, write
  it up as its own spec with the contract pinned.

### Q10. `config diff` exit-code semantics for CI

- (a) Exit 0 on success (no-change OR diff present), exit
  ≥2 on error. Success branches don't distinguish in the
  exit code; the printed stdout does.
- (b) Exit 0 on no-change, exit 1 on diff present, exit
  ≥2 on error (matches `git diff --quiet` / `diff`
  conventions). Operators in CI gate deploys on this.
- (c) Add `--exit-code` flag opting into (b)'s
  success-branch split; default is (a). In both modes,
  errors ALWAYS exit non-zero (≥2) — the flag never
  masks an error.
- **default:** (c). Matches `git diff` precedent (default
  permissive; explicit `--exit-code` for CI gating). Avoids
  breaking shell scripts that pipe diff into a pager. The
  errors-always-non-zero rule keeps the flag from creating
  silent CI failures when the diff command can't even
  complete the comparison.

### Q11. `config diff --from <sha256>` for rollback workflows

Should the diff command support diffing against a hex sha
provided on the CLI instead of the live remote?

- (a) Yes, with `--from <hex-sha>`. Useful when an operator
  wants to confirm what a known-good past state looked like
  vs. local before rolling back.
- (b) No. Recovery happens via "revert <name>.toml in git, then
  diff vs remote as usual".
- **default:** (b) for v1. The (a) story requires either
  uploading historical blobs to the cloud store (the platform
  doesn't keep them) OR a local on-disk archive (out of scope
  per §10.7). Document (a) as a follow-up.

### Q12. Spin Cloud size cap when the blob exceeds `ARG_MAX`

See §9.4 for context.

- (a) Hard-error in `config push` when the local blob exceeds
  the writer's pre-existing per-pair cap
  (`MAX_ARGV_BYTES_PER_INVOCATION = 96 * 1024` at
  `crates/edgezero-adapter-spin/src/cli/push_cloud.rs:46`,
  i.e. ~95 KiB after envelope wrapping + the `<KEY>=`
  prefix), with an actionable message naming the Q6 (a)
  remediation (restructure `C` into separate types across
  multiple `[stores.config]` ids; do NOT auto-chunk).
- (b) Soft-warn but proceed. Operator gets an `E2BIG` from
  Spin's CLI if it actually overflows.
- (c) Chunk the blob across N sibling keys (recreates the
  partial-write hazard we're trying to avoid).
- **default:** (a). Predictable failure with a clear remediation.
  (b) buries the failure on the CLI's argv-parse layer where the
  error message is opaque; (c) reintroduces the per-leaf hazard.

### Q13. Recursive secret metadata

See §3.3.1.2 for context.

- (a) v1 ban — `AppConfigRoot` marker + §10.2.1 CI gate +
  doc comment. No code-level recursion. The "embed an
  `AppConfig` struct in another" failure mode is caught
  by grep at PR time, not by `cargo check`.
- (b) Emit recursive `SECRET_FIELDS` from the derive — the
  parent's metadata array is the union of its own fields
  plus each `AppConfigRoot` field's metadata, with the
  field name prefixed (`"feature.api_token"`). The
  extractor's secret walk learns to traverse dotted
  paths. Significant API change: `SecretField::name`
  becomes a `&'static [&'static str]` or a dotted-string
  contract.
- (c) Negative trait impl detection (`assert_not_impl!`
  via `static_assertions`). Currently requires `nightly`
  and is brittle (impls in upstream crates would break
  the build).
- **default:** (a) for v1. Document (b) as the v2 successor.
  Operators with deeply-nested configs get clear failure
  at PR review; the framework doesn't ship code that
  silently loses secret semantics. Push to (b) when the
  first real operator hits the wall; until then the v1
  ban + CI gate keeps complexity proportional.

### Q14. Secret key-name validation hook

See §4.2 (NFC qualification) for context. `#[validate]`
rules on `#[secret*]` fields don't fire on the key name
at push (skipped per §3.3.8) and at runtime they fire
on the resolved secret VALUE, not the key name. There
is no in-framework way to enforce shape rules (NFC,
length, charset) on the operator-typed KEY NAME.

- (a) v1 ban — out-of-band only. Document the gap;
  operators add a TOML linter in their deploy
  pipeline. Zero framework code.
- (b) Add a new attribute `#[secret_key(validate(...))]`
  whose rules apply ONLY to the key NAME at push (skipped
  at runtime, where the field holds the value). Doubles
  the secret-attribute surface area for a rarely-needed
  invariant.
- (c) Have `validate_excluding_secrets` honour
  `#[validate]` on secret fields after all, but apply
  it to the KEY NAME at push. Conflates the
  "validators describe the value" contract; surprising
  for operators whose runtime validators expect to see
  the secret bytes.
- **default:** (a) for v1. Defer (b) until a real
  operator hits the wall (the secret-keys-must-be-NFC
  use case is rare; the symptoms — a secret-store
  miss at runtime with a clear `NotFound` error —
  surface fast enough that a TOML linter is enough).
  (c) is wrong on principle.

## 12. Test plan

### 12.1 Unit

- `canonical_data_sha256` is deterministic across:
  - Key-insertion order (insert {b, a} vs {a, b} → same sha).
  - Whitespace in the source TOML.
  - **Equivalent typed values** — sources that round-trip to
    the SAME typed Rust value hash to the same sha. Example:
    a `f64` field with TOML source `1.5` vs `1.5000000` vs
    `15e-1` all parse to the same `f64` and hash identically.
    A `String` field's NFC-vs-NFD encoding produces
    DIFFERENT shas per §4.2's verbatim-UTF-8 rule (the
    persisted bytes differ; the SHA reflects that). This
    test does NOT mix types:
    per §4.2 type-identity rule, `1500` (i64) and `1500.0`
    (f64) hash DIFFERENTLY because the typed Rust value
    differs even though the source text "feels" the same;
    that's the type-identity rule, not a determinism
    failure.
- `AppConfig<C>` extractor:
  - Returns the deserialised struct for a valid blob.
  - Errors on sha mismatch (manually-edited blob).
  - **Missing key (per Q3 (d) / §6.3 — round-19/20).**
    Fixture: `ConfigStore::get` returns `Ok(None)`.
    Assert the extractor produces
    `EdgeError::ConfigOutOfDate { message, field_path }`
    where `message` contains the literal `key
\`<resolved-key>\``AND the literal`run
    \`<app-cli> config push\``. Render the
resulting `Response`; assert HTTP status 503 and
the `Retry-After: 60`header is present. NOT`EdgeError::Internal` (the round-18 reversal that
    round-19 then propagated through §6.3).
  - `named(key)` reads a different key from the same store.
- `BlobEnvelope` deserialise:
  - Rejects unknown envelope `version`.
  - Tolerates additional ignored fields (forward-compat for
    `signature`, etc.).
- **Non-finite floats rejected (round-15 B-2 / round-16
  I-2).** Each fixture asserts the loader returns
  `Err(AppConfigError::InvalidValue { path, field_path,
message })` (the variant added per §4.2; NOT the
  existing `Validation` variant which wraps
  `validator::ValidationErrors`). Tests pattern-match on
  the variant and assert:
  - `field_path == "service.ratio"`, and
  - `message.contains("NaN")` (or `"inf"`, or `"-inf"`).

  Three fixture families:
  1. TOML literal `service.ratio = nan` (repeated for
     `inf`, `-inf`). Run
     `load_app_config_raw_with_options` against the
     fixture; assert `Err(InvalidValue { ... })` with
     the dotted path + value substring per above. The
     canonicaliser MUST NOT run on this input (assert
     via a counter in a test-only canonicaliser hook,
     or via a panic-on-call mock).
  2. Env overlay: TOML has `service.ratio = 1.5`, env
     var `<APP_NAME>__SERVICE__RATIO=nan` (and `inf`,
     `-inf`). Run with env overlay ON; assert
     `Err(InvalidValue { field_path: "service.ratio",
message: <contains "NaN">, ... })` from the
     overlay's `parse::<f64>()` + `is_finite()` check.
     With env overlay OFF (`--no-env`), the TOML value
     flows through unchanged and the load succeeds
     (overlay is what pulled the bad value in).
  3. NEGATIVE case: TOML `service.ratio = 1.5` AND
     env `<APP_NAME>__SERVICE__RATIO=2.5` BOTH
     succeed (the rule rejects non-finite values, not
     any specific float magnitude). Asserts the load
     returns `Ok(..)` with `cfg.service.ratio == 2.5`.

### 12.2 Push / diff

- `config push` writes the expected envelope shape per adapter.
- Skip-on-equal: a push with no change exits 0 and performs
  no WRITE shell-out. The READ shell-out (the read-back per
  §9.0) still runs — that's how we know to skip. The test
  asserts on which shell invocation paths fire (use
  `Command::env_clear()` + a fake adapter PATH so the test
  observes the actual `Command::new(...)` calls).
- `config diff` against an unchanged remote prints
  "no changes".
- `config diff --format json` emits the documented schema.
- `config diff` against a missing remote (no prior push) treats
  it as "every leaf added".

### 12.3 Per-adapter end-to-end

For each of axum / cloudflare / fastly / spin:

- `config push` writes a known struct.
- The runtime extractor reads it back and observes the same
  field values.
- A second `config push` with one field changed is observed in
  the runtime after a re-read.
- The smoke scripts (`scripts/smoke_test_config.sh`) seed via
  the new blob path and assert the runtime returns the seeded
  values.

**Fastly-specific size-cap test (round-18 B-1).** Q6's
Fastly cap of 8 000 characters is enforced by a writer-
side guard (per Q6's last paragraph); the test
exercises both sides of the boundary:

- A blob whose envelope JSON is exactly 8 000
  characters pushes successfully (no false positive on
  the boundary). The runtime reads it back and
  validates.
- A blob whose envelope JSON is 8 001 characters
  errors via the Fastly-writer guard before the
  platform call. Assert exit non-zero and the message
  names the 8 000-character cap AND the Q6 (a)
  remediation (restructure into separate `C` types per
  `[stores.config]` id; do NOT auto-chunk).
- The guard does NOT fire on the OTHER three adapters
  at the same blob size — only the Fastly writer
  enforces this cap.

### 12.4 Migration

- Loading a pre-blob (flattened) state errors with the
  documented "migration required" message.
- The migration guide's documented `<app-cli> config push`
  flow brings the store to a working blob state in one
  command.

### 12.5 Secret-field model (round-3 additions)

The framework-resolved `#[secret]` model (§3.3) needs explicit
coverage so a future refactor doesn't quietly reintroduce the
strip or revert the resolution direction:

- **Blob carries key name, not secret value.** Fixture:
  app-demo's TOML with `api_token = "demo_api_token"`,
  `vault = "default"`. Assert: pushed blob's
  `data.api_token == "demo_api_token"` (the key NAME) AND
  `data.vault == "default"` (the store id). The blob NEVER
  contains the resolved secret value bytes.
- **Extractor resolves secrets to values.** Spin up the test
  runtime with a mock secret store that returns
  `"the-actual-token"` for key `demo_api_token` in store
  `"default"`. Extract `AppConfig<AppDemoConfig>` from a
  handler. Assert `cfg.api_token == "the-actual-token"` (the
  resolved VALUE) AND `cfg.vault == "default"` (the
  store-ref field, unchanged). This is the user-facing
  Model A contract — the framework swaps key NAME for
  resolved VALUE before the handler sees `cfg`.
- **Missing secret at extract time.** Same fixture, mock
  secret store configured to `NotFound` on lookup. Assert
  the extractor produces `EdgeError::ConfigOutOfDate` (per
  §3.3.6) — NOT `Unavailable` (that's reserved for
  store-backend network errors) and NOT `Internal` (which
  today's `SecretError::NotFound → EdgeError::Internal`
  mapping in `crates/edgezero-core/src/secret_store.rs:131`
  produces). The blob model's missing-secret behaviour
  requires a DELIBERATE deviation from the current mapping:
  the extractor wraps `SecretError::NotFound` into
  `ConfigOutOfDate` BEFORE it bubbles, so the dashboards
  signal "deploy is incomplete" rather than "we 500'd".
  Calls out: the `secret_store.rs` mapping itself doesn't
  change for raw handler-side callers (still maps to
  `Internal`); the extractor's `?` operator catches it and
  re-wraps. Test asserts the resulting `EdgeError` variant
  AND the `Retry-After: 60` header per §6.3.1.
- **Missing secret-store id at extract time.** Fixture:
  manifest declares no `[stores.secrets].ids = ["missing"]`,
  but a `#[secret(store_ref = "vault")]` field's value is
  `"missing"`. Assert extractor surfaces
  `EdgeError::ConfigOutOfDate` with the actionable message
  "blob declared store_ref `missing` but [stores.secrets]
  has no such id".
- **Secret-store unreachable at extract time.** Same
  fixture, mock store configured to error with a network
  failure (NOT NotFound). Assert extractor surfaces
  `EdgeError::ServiceUnavailable` (per §3.3.6 — a flaky
  backend is not an out-of-date config).
- **`<app-cli> config validate --strict` — structural
  checks ONLY.** Three sub-tests, each asserting on a
  structural property the validator can verify without
  hitting any secret store:

  1. **`[stores.secrets]` declared.** Fixture: typed
     struct has at least one `#[secret]` or
     `#[secret(store_ref*)]` field; manifest omits
     `[stores.secrets]`. Assert validate errors with a
     message naming `[stores.secrets]`.
  2. **`#[secret(store_ref = "field")]` sibling exists
     and has `#[secret(store_ref)]`.** Fixture: typed
     struct has `#[secret(store_ref = "vault")] api_token`
     but no `vault` field, OR a `vault` field WITHOUT
     `#[secret(store_ref)]`. Assert validate errors at
     macro-compile time (per §3.3.1.4). This is a
     compile-time test (`trybuild` UI test), not a runtime
     assertion.
  3. **Named store id is declared.** Fixture: struct's
     `#[secret(store_ref)] vault: String = "missing"`,
     manifest's `[stores.secrets].ids = ["default"]` (no
     `"missing"`). Assert validate errors with a message
     naming the missing id.

  Validate does NOT probe whether the secret VALUE exists
  in any store (per §3.3.2). The test plan deliberately
  does not include a case that requires a live secret-store
  call — those failure modes are §12.5's
  missing-secret-at-extract test, which uses a mock store
  at REQUEST time.

### 12.6 Runtime validation (§6.2.2)

- **Validate runs on every extract.** Fixture: a blob that
  serdes cleanly but violates a `#[validate(range(min=1,
max=10000))]` rule. Extract. Assert
  `EdgeError::ConfigOutOfDate` with `field_path` naming the
  offending field.
- **Validate-OK is the happy path.** Fixture: a blob in
  bounds. Extract. Assert the typed struct comes back with
  no error.
- **Validator violation report format.** Assert the
  `ConfigOutOfDate` response body matches the nested
  envelope documented in §6.3.1 exactly:
  `{ "error": { "status": 503, "kind": "config_out_of_date",
"message": "<…>", "field_path": "<dot.path>" } }`. Assert
  the `field_path` value names the offending field (e.g.
  `"feature.new_checkout"` for the §12.6 violation
  fixture). Assert the response carries the
  `Retry-After: 60` header.

#### 12.6.1 `kind` strings on every `EdgeError` variant + header policy (round-23 M-1)

§6.3.1 adds a stable `kind: String` field to the response
body for EVERY `EdgeError` variant (`bad_request`,
`internal`, `method_not_allowed`, `not_found`,
`not_implemented`, `service_unavailable`, `validation`,
and the new `config_out_of_date`). The earlier test plan
only asserted the `config_out_of_date` body; this section
covers the other six and the cross-cutting
header / field-presence rules so a future refactor can't
silently drop or rename a `kind` string.

- **Stable `kind` strings.** One fixture per variant
  that triggers it (e.g. a handler that returns
  `Err(EdgeError::not_found("…"))` for `not_found`).
  Render the `Response`. Parse the body as JSON.
  Assert `body.error.kind` equals the exact string
  documented in §6.3.1. Variant → expected string:

  | Variant              | Expected `kind` string  |
  | -------------------- | ----------------------- |
  | `BadRequest`         | `"bad_request"`         |
  | `Internal`           | `"internal"`            |
  | `MethodNotAllowed`   | `"method_not_allowed"`  |
  | `NotFound`           | `"not_found"`           |
  | `NotImplemented`     | `"not_implemented"`     |
  | `ServiceUnavailable` | `"service_unavailable"` |
  | `Validation`         | `"validation"`          |
  | `ConfigOutOfDate`    | `"config_out_of_date"`  |

- **`field_path` is OMITTED outside `ConfigOutOfDate`.**
  For each non-`ConfigOutOfDate` variant fixture above,
  assert `body.error.get("field_path")` is `None` (the
  field is absent from the JSON, not present with an
  empty string). Per §6.3.1's "ONLY on
  `config_out_of_date`" rule.

- **`Retry-After: 60` is PRESENT on `ConfigOutOfDate`
  ONLY.** Fixture per variant: render the response,
  assert `response.headers().get("retry-after")`:

  - `ConfigOutOfDate`: `Some("60")`.
  - `ServiceUnavailable`: `None`. Round-10 H-3 audit
    found the variant is reused for several
    non-retryable cases (KV size limit, missing
    named KV store, missing default secret store);
    sending the header would mislead clients into a
    tight retry loop. The §6.3.1 narrowing pins
    `ServiceUnavailable` to no `Retry-After`.
  - All other variants: `None`.

- **Status codes match the documented variant → HTTP
  mapping.** Fixture per variant: assert
  `response.status()`:

  | Variant              | HTTP status |
  | -------------------- | ----------- |
  | `BadRequest`         | 400         |
  | `Internal`           | 500         |
  | `MethodNotAllowed`   | 405         |
  | `NotFound`           | 404         |
  | `NotImplemented`     | 501         |
  | `ServiceUnavailable` | 503         |
  | `Validation`         | 422         |
  | `ConfigOutOfDate`    | 503         |

  (The two 503-class variants are deliberately
  distinguished by the `kind` string + the
  `Retry-After` header, not the status code.)

### 12.7 Env-var key override (§5.2)

For each adapter:

- Set `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging`.
- Push two distinct blobs: one to `app_config`, one to
  `app_config_staging`.
- Assert the runtime extractor returns the staging blob's
  values (not the default blob's).
- Unset the env var; re-extract; assert default blob's
  values come back.
- Empty / whitespace env var: assert it falls back to the
  logical id (per §5.2 whitespace rule).

### 12.8 Raw-binary removal (§3.2.1)

Three invocation surfaces (per §3.2.2's catch-all stub
design); the first two go through the match arm and the
third goes through clap's help renderer. ALL THREE must
carry the §3.2.1 pointer text byte-for-byte.

- **Bare invocation — match arm.**
  `edgezero config push` (no flags) exits with code 2 and
  the §3.2.1 stub-pointer text (contains "requires a
  typed app-config struct" and "your generated
  downstream CLI"). Same for `edgezero config diff`. The
  text comes from the match arm body.
- **With-flags invocation — match arm (catch-all).**
  `edgezero config push --adapter axum --key staging
-- positional` exits with code 2 and the SAME
  §3.2.1 pointer text. The catch-all
  `ConfigCmdStubArgs.trailing` field absorbs every flag
  / positional / `--`-trailing token, so clap dispatches
  to the match arm rather than producing an "unexpected
  argument" error. Same for `edgezero config diff
<anything>`. Round-13 had this path going through
  `after_help`, but a round-14 probe confirmed Clap
  4.6.x does NOT render `after_help` on parse errors;
  the catch-all bypasses that limitation. Assert the
  pointer phrases appear in stdout verbatim — they
  match the bare-invocation text byte-for-byte.
- **`--help` invocation — clap renderer (after_help).**
  Four help surfaces, each must include the pointer
  text byte-for-byte identical to
  `STUB_POINTER_AFTER_HELP`:
  1. `edgezero config push --help` — child
     `after_help` on `Push(ConfigCmdStubArgs)`.
  2. `edgezero config diff --help` — child
     `after_help` on `Diff(ConfigCmdStubArgs)`.
  3. `edgezero config --help` — PARENT `after_help`
     on `Command::Config`. A round-17 Clap probe
     confirmed child `after_help` does NOT bubble up
     to the parent help screen; the parent variant
     declaration MUST carry its own `after_help`.
     §3.2.2's parent-enum sketch wires this.
  4. `edgezero config validate --help` — should NOT
     show the pointer (Validate is the only non-stub
     variant on the bundled binary). Assert the
     pointer text is ABSENT from this surface to
     guard against accidentally attaching
     `after_help` to all three children.

  Additionally assert that the catch-all `[TRAILING]`
  argument does NOT appear anywhere in surfaces 1-3:
  `hide = true` on `ConfigCmdStubArgs.trailing`
  (§3.2.2) suppresses both the Usage-line
  `[TRAILING]...` placeholder and the `Arguments:`
  section. The presence of `[TRAILING]` in `--help`
  output would leak the internal sink and confuse
  operators; the assertion guards against accidental
  removal of `hide = true`.

- **`edgezero config validate` still works.** Raw +
  typed both preserved per §3.2.1.

### 12.9 Downstream CLI wiring

Generated downstream CLIs (`my-app-cli` scaffold output):

- `my-app-cli config push --adapter axum --dry-run` renders
  the diff and exits 0 without writing.
- `my-app-cli config diff --adapter axum --format json`
  emits the documented JSON schema.
- `my-app-cli config validate --strict` runs the typed
  validator path plus the STRUCTURAL secret-metadata
  checks from §3.3.2 (every named-store id from
  `#[secret(store_ref = "...")]` is declared in
  `[stores.secrets]`; every store-ref field's value
  resolves to a declared id). It does NOT probe live
  secret values — that's the request-time path tested in
  §12.5's missing-secret-at-extract case.
- `edgezero config validate --strict` (bundled, raw)
  runs the manifest-level structural checks the current
  implementation already runs at
  `crates/edgezero-cli/src/config.rs:503` (capability-
  aware completeness, handler paths). The bundled CLI
  has no `C` so the SECRET_FIELDS walk is skipped, but
  `--strict` is NOT silently ignored — it's the same
  flag, doing the manifest-level half of the work. The
  difference between bundled and generated is the
  ADDED typed walk on the generated side, not a
  flag-honored / flag-ignored split.

### 12.10 Spin Cloud blob size cap (§9.4, Q12)

The cap is the writer's pre-existing
`MAX_ARGV_BYTES_PER_INVOCATION = 96 * 1024` at
`crates/edgezero-adapter-spin/src/cli/push_cloud.rs:46`.
Under the blob model, the effective limit on the envelope
JSON is
`MAX_ARGV_BYTES_PER_INVOCATION - <KEY>.len() - 1` bytes —
for the default `app_config` key (10 chars), that's
98 304 - 10 - 1 = 98 293 bytes (~95 KiB).

- A blob whose `<KEY>=<envelope JSON>` form is exactly
  `MAX_ARGV_BYTES_PER_INVOCATION` bytes (or larger) pushed
  to Spin Cloud exits non-zero with the documented
  "exceeds the 96 KiB safe-argv-per-invocation cap"
  message (extended in the implementing PR to name the
  blob-model remediation: restructure `C` into separate
  typed structs across multiple `[stores.config]` ids
  per Q6's (a) guidance, or use `--local`).
- A blob exactly one byte UNDER the cap (i.e. the
  `<KEY>=<JSON>` pair is
  `MAX_ARGV_BYTES_PER_INVOCATION - 1` bytes) pushes
  successfully (no false positive on the boundary).
- The Spin-Cloud-specific cap does NOT fire on the other
  three adapters at the SAME blob size — Cloudflare's KV
  value limit is 25 MiB, Axum has no transport-side cap.
  Fastly is the EXCEPTION: a Spin-boundary blob
  (~95 KiB) DOES hit Fastly's tighter 8 000-character
  Config Store limit (per Q6, round-18 B-1
  correction). Fastly's separate platform cap is
  covered in §12.3's Fastly-specific size test: a blob
  at ≤ 8 000 characters pushes successfully; a blob at
  > 8 000 characters errors via the
  > Fastly-writer-side pre-platform guard (per Q6) with
  > the restructure-into-multiple-`[stores.config]`-ids
  > remediation message. The §12.10 Spin-Cloud-cap test
  > does NOT exercise the Fastly path — that's §12.3's
  > job.

### 12.11 CLI parser tests for the canonical flag surface (§3.2.2)

`crates/edgezero-cli/src/args.rs:325-419` already has a
parser-test pattern. Extend it with the blob-model flags
so a future renaming or default-value change can't
silently break the spec's invariants:

- **`ConfigPushArgs` parses all canonical flags.**
  Fixture: `--adapter axum --app-config alt.toml --manifest
custom.toml --store other --key staging --no-env --local
--runtime-config rt.toml --yes --no-diff --dry-run`.
  Assert every field is populated to the supplied value;
  short `-y` resolves to `yes = true`.
- **`ConfigDiffArgs` parses all canonical flags.** Same
  exercise on `--format json`, `--local`, `--store`,
  `--key`, `--runtime-config`, `--exit-code`. Assert
  the `exit_code` field is `true` after parse.
- **`config diff --exit-code` behaviour — success
  branches.** Mock the remote read so local == remote:
  assert exit 0 (no diff). Same fixture with a diff
  present: assert exit 1. Without `--exit-code`, BOTH
  cases exit 0 and only the printed stdout
  distinguishes them.
- **`config diff` error branches are non-zero
  REGARDLESS of `--exit-code`.** Three fixtures, each
  run with and without `--exit-code`:

  1. Remote read returns a network error → exit ≥ 2.
  2. Manifest fails to load (missing
     `edgezero.toml`) → exit ≥ 2.
  3. Local TOML deserialise fails (a typed-schema
     mismatch) → exit ≥ 2.

  Per §3.2.2's `exit_code` doc-comment, the flag only
  changes the "diff present" success branch; real
  errors NEVER exit 0 in either mode.

- **`ConfigValidateArgs` parses `--strict`.** Assert
  `strict: true` after parse.
- **Default values match `default_manifest_path()`** —
  parser-roundtrip a no-flag invocation against
  `ConfigValidateArgs` (which has no required flags),
  assert `manifest == default_manifest_path()` (per
  `args.rs:144`). NOT against `ConfigPushArgs` or
  `ConfigDiffArgs` — those require `--adapter`, so a
  no-flag invocation against them would fail clap parse
  before any defaults could be observed. The defaults
  on `--manifest` are shared across all three Args
  structs (§3.2.2), so the validate-only assertion
  covers the contract.
- **Removed flags don't sneak back.** Parse fails (clap
  error) on `--config <path>` (an earlier-spec name
  that was never shipped) and on any other non-listed
  flag. This catches accidental renames during refactor.

### 12.12 `--store` routing in push / diff (§3.2.2 invariants)

- **Default store routing.** Manifest declares
  `[stores.config].ids = ["app_config"]`,
  `default = "app_config"`. `config push --adapter axum`
  with no `--store` flag writes to logical id
  `app_config`. Assert the blob lands at the
  `app_config` key in the Axum file map.
- **Explicit `--store` overrides default.** Manifest
  declares `ids = ["app_config", "experiments"]`,
  `default = "app_config"`. `config push --adapter axum
--store experiments` writes to logical id
  `experiments`. Assert the blob lands at the
  `experiments` key, NOT `app_config`.
- **Unknown `--store` errors clearly.** `--store
missing_id` against the same manifest errors with
  "logical id `missing_id` not declared in
  [stores.config]" and exits non-zero.
- **`--store` interacts with `--key`.** `--store
experiments --key experiments_staging` writes to the
  `experiments` store under the `experiments_staging`
  key — the two flags are independent.

### 12.13 Cloudflare local vs remote binding mode (§9.2)

- **Remote push uses `--namespace-id`.** Mocked wrangler
  invocation; assert the spawned command line includes
  `--namespace-id=<id>` AND `--remote`, NOT `--binding`.
- **Local push uses `--binding`, NOT `--namespace-id`.**
  Mocked wrangler invocation against a manifest whose
  namespace id is `local-dev-placeholder` (the scaffold
  default); assert the command line includes
  `--binding <BINDING>` AND `--local`. Crucially the
  test should PASS even when no `edgezero provision`
  step has resolved the placeholder id — that's the
  whole point of the binding-mode choice
  (`cli.rs:399`'s rationale).
- **Read-back uses `wrangler kv key get`.** Mocked
  invocation; assert the four-segment subcommand path
  is used (not the deprecated three-segment `wrangler
kv get`).

### 12.14 `StoreRegistry` ref accessors (§5.2.1 M-1)

- **`default_ref` returns `Some(&ConfigStoreBinding)`
  for a one-id registry.** Construct a registry with
  `app_config → binding`. Assert `default_ref()`
  borrows the binding without cloning (verified by
  pointer equality on the inner `default_key` string
  across two consecutive calls).
- **`named_ref` returns `Some(&H)` for declared ids,
  `None` for unknown.** Assert both.
- **Owned accessors still work.** `default().handle`
  vs `default_ref().handle` resolve to the same handle.
  Owned-clone callers (Kv / Secrets) keep building.

### 12.15 Raw `Config` extractor binding accessors (§5.2.1 H-1)

- **`Config::default()` returns the unwrapped handle.**
  Construct a `ConfigRegistry<ConfigStoreBinding>` with
  one id, extract `Config`, call `default()`, assert it
  returns `BoundConfigStore` (= `ConfigStoreHandle`) —
  NOT a binding. Hand-managed `bound.get(...)` call
  works against it.
- **`Config::default_binding()` returns the binding.**
  Same setup, call `default_binding()`, assert it
  returns `Some(&ConfigStoreBinding)` and that
  `binding.default_key` matches the registry's
  pre-resolved key.
- **`Config::registry()` exposes the binding-shaped
  registry.** Assert
  `registry().default_ref().unwrap().default_key`
  matches the supplied key.

### 12.16 Named-store secret adapter validation (§3.3.2 M-2 / round-9)

`ConfigValidateArgs` has no `--adapter` field; `config
validate` runs adapter-typed checks across EVERY adapter
declared in the manifest's `[adapters]` table (per
`crates/edgezero-cli/src/config.rs:611`'s loop). The
manifest's `[adapters.spin]` table is what activates the
Spin path; tests below set `[adapters.spin]` and rely on
the loop to invoke `SpinAdapter::validate_typed_secrets`.

Spin's `validate_typed_secrets` at
`crates/edgezero-adapter-spin/src/cli.rs:363` enforces
TWO rules, NOT a `[variables]` collision:

1. The lowercased secret value is a valid Spin variable
   name (no dashes, not digit-first, no `.` translation).
2. No two lowercased secret values collide in the flat
   Spin variable namespace.

Tests align with that contract:

- **`KeyInNamedStore` value with an invalid Spin name.**
  Fixture: `#[secret(store_ref = "vault")] api_token:
String` with `cfg.vault = "default"`,
  `cfg.api_token = "demo-token"` (a dash makes it
  invalid as a Spin variable name). Manifest declares
  `[adapters.spin]`. Run `config validate --strict`.
  Assert the Spin adapter rejects with a message
  naming `demo-token` AND the Spin variable-name
  rule violation (per `cli.rs:382`'s
  `spin_key_rule_violation`).
- **Collision between a `KeyInDefault` and a
  `KeyInNamedStore` value at the lowercased Spin
  variable name.** Fixture: two secret fields
  `#[secret] one: String = "Demo_Token"` and
  `#[secret(store_ref = "vault")] two: String =
"demo_token"`. Both lowercase to `demo_token`.
  Manifest declares `[adapters.spin]`. Assert
  validate rejects with a collision message naming
  BOTH field names AND the colliding lowercased
  variable.
- **Non-Spin adapter is exempt.** Same collision
  fixture against a manifest with only `[adapters.axum]`
  declared (no `[adapters.spin]`). Validate passes
  (Axum has no flat-namespace constraint).
- **`Adapter::validate_typed_secrets` receives all
  three secret-key variants.** Mock adapter that
  records every call; fixture with one `KeyInDefault`,
  one `KeyInNamedStore`, one `StoreRef` field. Assert
  the mock saw entries for both
  `KeyInDefault` and `KeyInNamedStore` (the `StoreRef`
  value is the store id, which IS the resolved
  `store_id` parameter for `KeyInNamedStore`'s entry).

### 12.17 Nested-`AppConfig` ban fixture (§3.3.1.2 H-2)

The §10.2.1 gate's Pattern 4 calls
`crates/edgezero-cli/src/bin/check_no_nested_app_config.rs`
(a syn-based helper, gated behind the
`nested-app-config-check` feature). The tests exercise it
end-to-end through the helper binary built with the
feature enabled:

- **Direct nesting fires.** Fixture: two `.rs` files —
  `child.rs` derives `AppConfig` on `ChildConfig`,
  `parent.rs` derives `AppConfig` on `ParentConfig`
  with `child: ChildConfig`. Both use the idiomatic
  multi-line `#[derive(...)]` + `struct` layout. Run
  the helper. Assert exit 1 and the violation message
  names BOTH `ChildConfig` and `parent.rs:<line>`.
- **`Option<ChildConfig>` fires.** Same fixture but
  the parent field is `child: Option<ChildConfig>`.
  Assert exit 1 and the message still names
  `ChildConfig`.
- **`Vec<ChildConfig>` fires.** Same fixture, field
  `children: Vec<ChildConfig>`. Assert exit 1.
- **`Box<ChildConfig>` fires.** Same fixture, field
  `child: Box<ChildConfig>`. Assert exit 1.
- **Tuple field `(ChildConfig, OtherTy)` fires.**
  Assert exit 1 and the message names `ChildConfig`.
- **`[ChildConfig; N]` array fires.** Assert exit 1.
- **Multi-line `#[derive]` with `AppConfig` on a
  continuation line fires.** Fixture's `child.rs`
  spreads its derive across lines, e.g.:
  ```rust
  #[derive(
      Debug,
      edgezero_core::AppConfig,
  )]
  pub struct ChildConfig { ... }
  ```
  The parent uses `child: ChildConfig`. Assert exit 1
  — the syn-based root collector handles multi-line
  derives correctly (a one-line regex would NOT
  catch this).
- **Path-qualified derive `edgezero_core::AppConfig`
  is recognised.** Same as above but the parent uses
  `child: ChildConfig`. Assert the helper recognises
  BOTH the bare `AppConfig` derive AND the
  `edgezero_core::AppConfig` path-qualified form.
- **Legitimate non-`AppConfig` field is exempt.**
  Fixture: parent uses `child: SomeOtherStruct`,
  where `SomeOtherStruct` does NOT derive
  `AppConfig`. Assert exit 0.
- **Syntax error in a fixture file exits 2.** Fixture
  contains a malformed `.rs` file. Assert exit 2 —
  the helper refuses to pass silently on input it
  can't parse.

### 12.18 Manifest validation tightening (§5.2 round-13 M-2)

The blob model narrows the manifest's store-id charset
from `[A-Za-z0-9_-]` (round-9 baseline at
`crates/edgezero-core/src/manifest.rs:890`) to
`[A-Za-z0-9_]+` so the `EDGEZERO__STORES__<KIND>__<ID>__KEY`
override is exportable from a POSIX shell. Tests assert:

- **Hyphen in a store id is rejected.** Manifest
  fixture: `[stores.config]
ids = ["feature-flags"]
default = "feature-flags"`. Run manifest validation;
  assert `Err(ValidationError { code: "store_id_format",
message: <names "feature-flags" + the env-export
constraint>, ... })`. Same for `[stores.kv]` and
  `[stores.secrets]` (all three kinds share the
  validator).
- **Underscore-only ids still pass.** Manifest fixture
  with `ids = ["feature_flags"]`; validation succeeds.
- **`__` (double underscore) still rejected.** The
  pre-existing rule at `manifest.rs:891`. Fixture
  with `ids = ["feature__flags"]` errors.

## 13. Implementation phasing

**One PR, atomic landing.** The blob model is a hard cutoff (§10):
the runtime stops reading the per-leaf shape AT THE SAME COMMIT
that the writer stops writing it AND the in-tree consumers move
over. There is no "land Commits A-B now, do C-E later" path —
without `config push --key`, the `__KEY` runtime override is
useless; without the diff command, the push UX regresses below
what we ship today; without the read-back trait, push can't
skip-on-equal. They're load-bearing parts of the same model
change.

The PR organises commits by ConcernArea for review readability,
but ships them together. **The cutover from per-leaf reads/writes
to the blob model lands in ONE commit.** Round-22 attempted to
mark every commit as a "complete slice"; the round-23 reviewer
correctly observed that this contradicts §10.1's no-platform-bridge
rule + §10.2's atomic-app-demo-migration rule. An intermediate
commit with the new runtime extractor but the OLD per-leaf writer
(or vice-versa) cannot run end-to-end — there's no blob in the
store yet for the new reader, OR the new writer's output is
unreachable by the old reader. Pretending those mid-cutover
commits are complete slices misleads bisecting reviewers.

Each commit is annotated as one of:

- **Complete slice** — builds + tests pass on this commit
  alone. Bisect-safe.
- **Cutover slice** — THE commit that flips the blob model
  live. End-to-end-testable on its own (the new reader + new
  writer + app-demo migration land together here).
- **Pre-cutover infrastructure** — adds new types/traits but
  no in-tree caller exercises them yet. Builds + tests pass
  (the new code carries its own unit tests), but the runtime
  behaviour is unchanged from `main`. Bisect-safe.
- **Post-cutover additive** — builds on top of the cutover
  commit; landing it alone (without the cutover) is
  impossible because it depends on the new types. Bisect-safe
  once the cutover commit is in the bisect range.

The PR cannot land without ALL five (Commits A–E); the
annotations tell reviewers and `git bisect` users what to
expect.

1. **Commit A — Envelope + sha + canonical form
   (complete slice — pre-cutover infrastructure).**
   Pure `edgezero-core` work. Adds the `BlobEnvelope`
   type, `canonical_data_sha256` (the in-tree walker
   per §4.2 Q1 (b)), the pin test (§4.2), and the
   unit tests in §12.1. No runtime behaviour change;
   the new types aren't wired anywhere yet.

2. **Commit B — Binding + manifest charset +
   `EnvConfig::store_key` + read trait
   (pre-cutover infrastructure).** §5.2 + §5.2.1 +
   §9.0 + §12.18. Bundles four pieces that share the
   "add new structure, no in-tree caller exercises
   it yet" property:

   - Tightens `[stores.*]` ids to `[A-Za-z0-9_]+`
     (§5.2 / §12.18); manifest validator gains the
     new error code. Existing in-tree manifests
     already use underscore-only ids (§10.2's grep
     recipe verifies), so no in-tree breakage.
   - Introduces `ConfigStoreBinding { handle,
default_key }` on `ConfigRegistry`. `Config::default()`
     keeps returning `ConfigStoreHandle` (unwraps
     `binding.handle`) so existing hand-managed
     callers compile unchanged; the new
     `default_binding()` accessor is added but no
     in-tree caller invokes it yet.
   - Adds `EnvConfig::store_key("config", id)`
     centralised lookup; the four adapters'
     `build_config_registry` calls it to pack the
     binding. Builds clean; the field is unused at
     runtime because the extractor doesn't read it
     yet.
   - Adds `Adapter::read_config_entry` trait + per-
     adapter impls (including
     `ReadConfigEntry::Unsupported` for Spin Cloud).
     No in-tree caller invokes the trait yet.

   Each piece could ship alone; bundling them keeps
   the cutover commit (C) focused on actual
   behaviour change.

3. **Commit C — THE ATOMIC CUTOVER (cutover slice).**
   §3.3 + §8.2 + §10.2 + §10.2.1 + §10.2.2. This is
   the load-bearing commit; everything it touches has
   to land together per §10's hard-cutoff rule:

   - `AppConfig<C>` extractor + secret resolution
     wired into all four adapters' `ConfigStore::get`
     read path. Missing-blob maps to
     `EdgeError::ConfigOutOfDate` per Q3 (d).
   - `config push` rewrite — single-blob writers per
     adapter (Axum file map / Cloudflare bulk-put /
     Fastly `--upsert --stdin` / Spin direct-write).
     `--key` override flag wired here (§5.4 + §3.2.2)
     so push + the Commit B `__KEY` runtime path are
     end-to-end testable in this single commit.
   - `config push` inline-diff prompt + `--no-diff` /
     `--yes` / `--dry-run` flags (§8.2). Uses the
     read trait from Commit B.
   - App-demo migrates (`examples/app-demo/`): the
     handler files, the source TOML, the per-adapter
     manifests. Per §10.2's "the same commit"
     requirement.
   - Scaffold templates migrate
     (`crates/edgezero-cli/src/templates/`,
     `crates/edgezero-adapter-*/src/templates/`). Per
     §10.2.2.
   - §10.2.1 grep gate enabled
     (`scripts/check_no_legacy_typed_reads.sh` runs
     in CI). It passes because app-demo + scaffold
     templates are migrated in this same commit.

   Why this is the cutover and not splittable: §10.1
   forbids the "new runtime, old writer" intermediate
   state. The runtime extractor expects an envelope;
   the existing per-leaf writer doesn't produce one;
   so any commit that lands ONE half leaves
   intermediate trees that can't run app-demo (no
   blob for the new reader / the new reader rejects
   the leaf shape). The grep gate adds another
   constraint: it would fail in any commit that
   moves the extractor without migrating app-demo
   handlers. Landing both halves together is the only
   path that satisfies §10.

   Acceptance for Commit C alone: `cargo test
--workspace` passes; `scripts/smoke_test_config.sh`
   seeds a blob via `app-demo-cli config push --adapter
axum` and the runtime returns the seeded values;
   §10.2.1 gate passes.

4. **Commit D — `config diff` command (post-cutover
   additive).** §8.1. Adds the diff subcommand
   reusing the read trait + the diff renderer from
   Commit C's push flow. `--format unified |
structured | json`, `--exit-code` semantics per
   Q10.

5. **Commit E — Migration guide, smoke scripts,
   README updates (post-cutover additive).** §10
   migration guide, `scripts/smoke_test_config.sh`
   updates, scaffold READMEs. Documentation /
   pipeline polish; landing it alone is impossible
   (it references Commit C's CLI surface) but it's
   bisect-safe in the post-C range.

Removed from the round-22 phasing: separate Commits D
("read trait alone") and E ("config push" alone). Both
are now folded into Commit C — the read trait gets its
caller in Commit C, and config push can't ship without
the runtime extractor accepting the blob it writes.
Both halves still get full review (the diff is
inspectable; the PR description points at the
adapter/extractor pair), but they ship as one commit so
the bisect line is honest.

Reviewer can read commits in order to follow the
design from "types" → "infrastructure" → "cutover" →
"diff" → "docs". The PR cannot land without ALL five;
the reviewer's job is to confirm completeness, not to
approve a subset.

### 13.1 Acceptance gate — no placeholder values in shipped code

The spec leaves one value intentionally unfilled because it
depends on the implementing PR's actual hashing:

- The 64-character SHA hex in `canonical_form_pin_v1`
  (§4.2).

(Round-21 Q1 resolution: the canonicaliser is the in-tree
hand-rolled walker at
`crates/edgezero-core/src/canonical_form.rs`; there is
no external crate name + version pin to enforce.)

A CI gate `scripts/check_no_placeholder_pins.sh` runs on
every PR. It greps the canonicaliser pin test file for
placeholder markers AND confirms the in-tree walker
module exists. The gate is short:

```sh
#!/usr/bin/env bash
set -euo pipefail

FILE="crates/edgezero-core/tests/canonical_form_pins.rs"
if [ ! -f "$FILE" ]; then
  echo "ERROR: $FILE missing — implementing PR didn't write the pin test." >&2
  exit 1
fi

if grep -qE '(…|fixed-hex-value)' "$FILE"; then
  echo "ERROR: $FILE still contains placeholder markers" >&2
  echo "('…' or 'fixed-hex-value'). Replace with the real computed hex." >&2
  exit 1
fi

# Canonicaliser is in-tree (Q1 (b), §4.2). Confirm the
# module exists at one of the documented paths. The path
# is fixed so the gate can find it; the implementer does
# NOT choose between an external crate and the walker —
# §4.2's Stability section pins the in-tree walker as
# the v1 default.
if [ ! -f crates/edgezero-core/src/canonical_form.rs ] \
   && [ ! -f crates/edgezero-core/src/canonical_form/mod.rs ]; then
  echo "ERROR: in-tree canonicaliser module missing. §4.2 requires" >&2
  echo "crates/edgezero-core/src/canonical_form.rs (or .../mod.rs)." >&2
  exit 1
fi

# Defensive: an `serde_canonical_json` line in workspace
# Cargo.toml almost certainly means someone tried to
# revive the rejected (a) candidate. Q1's resolution
# explicitly excludes it (probe at v1.0.0 rejected
# finite floats). Fail loud rather than silently
# accepting a hybrid.
if grep -qE '^serde_canonical_json' Cargo.toml 2>/dev/null; then
  echo "ERROR: serde_canonical_json appears in workspace Cargo.toml." >&2
  echo "Q1 (b) resolution excludes it — the crate rejects finite floats." >&2
  echo "Remove the entry; the in-tree walker is the v1 canonicaliser." >&2
  exit 1
fi

echo "OK: no placeholder pins remain; canonicaliser is in-tree."
```

Wired into `.github/workflows/test.yml` as a step before
`cargo test`. The gate's purpose is to prevent the spec's
intentional placeholder (the SHA hex) from surviving into
shipped code AND to fail loud if a future PR tries to
revive the rejected `serde_canonical_json` candidate. A
real CI failure beats a silent merge that makes the §4.2
byte-format contract un-testable.
