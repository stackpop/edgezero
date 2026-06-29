# Plan: Move Spin Config Store onto KV

**Status:** v12 — REVISION after tenth reviewer pass. Ready for
execution. **Reviewer green-lighted start.**

**Goal:** Back `SpinConfigStore` with the Spin KV API (`spin_sdk::key_value`)
instead of Spin variables (`spin_sdk::variables`). Bring Spin's config
surface into structural parity with Cloudflare (KV-backed) and Fastly
(Config Store-backed), so `config push` writes through a real per-store
backend on all three cloud adapters.

## v12 changelog

Round-10 reviewer gave the verdict "yes, we can start" and
flagged 1 Low + 1 Nit. Both fixed:

- **L1 (Stage 4/5 should explicitly REPLACE stale Spin-variable
  tests)** — fixed. The current tests assert translated keys
  - `[variables]` + `[component.<id>.variables]` writes at
    `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs:257`
    and `crates/edgezero-adapter-spin/src/cli.rs:1846`. The plan
    implied replacement via Task 4.6 (dry-run shape) and
    Task 5.1 (drop variables writes) but didn't say so explicitly.
    Added Task 4.7 and Task 5.5 to spell out the test rewrite:
    delete the translated-key / two-table assertions; add seed
    URL / JSON-body / no-POST-on-dry-run / status-code coverage.
- **Nit (reworded "backward-compatible" around run_app return)** —
  fixed. The migration is hard-cutoff; "backward-compatible"
  wording suggested legacy Spin-variable support was being
  preserved (it isn't). Reworded throughout to
  "source-compatible with the generated scaffold handler
  signature" — narrower, accurate.

## v11 changelog

Round-9 reviewer flagged 1 Medium + 1 Low against v10. Both real
and fixed:

- **M1 (unused `IntoResponse` import after run_app signature change)**
  — fixed. Today `crates/edgezero-adapter-spin/src/lib.rs` imports
  `spin_sdk::http::{IntoResponse, Request as SpinRequest, Response
as SpinResponse}` because `run_app` returns
  `impl spin_sdk::http::IntoResponse`. After Task 3.5 changes the
  return to `SpinFullResponse`, `IntoResponse` is no longer
  referenced and the wasm-clippy `-D warnings` gate would fail on
  `unused_imports`. Added an explicit substep to Task 3.5: drop
  `IntoResponse` from the import line. Documented in the Scope
  section under `src/lib.rs` too.
- **L1 (Stage 8 smoke test not executable as written)** — fixed.
  `spin up` is foreground/long-running; the v10 step list
  couldn't be pasted into a script. Stage 8 now provides a real
  shell snippet that backgrounds `spin up`, polls
  `127.0.0.1:3000` with `curl --silent --fail` until ready (5s
  timeout, fails the test cleanly), runs `config push --local`,
  asserts the curl, and cleans up the spin process in a `trap`
  so a failed assertion never leaves an orphan listener on
  port 3000.

## v10 changelog

Round-8 reviewer flagged 1 High against v9. Real and fixed:

- **H1 (seed branch Result type mismatch)** — fixed. In v9,
  `handle_seed_request_spin` returned bare `SpinFullResponse` but
  `run_app_with_seeder`'s seed branch was returning that value
  while the fall-through `run_app::<A>(req).await` returns
  `anyhow::Result<SpinFullResponse>`. Mismatched arm types in
  the `if/else` would not compile.

  **Resolution**: change `handle_seed_request_spin` to return
  `anyhow::Result<SpinFullResponse>` so both arms produce the
  same type. As a side benefit this drops the `.expect("static-
shaped seed response")` from v9's D10 example, which was a
  latent panic in a request handler. Internal failures
  (`into_core_request`, `from_core_response`) now propagate via
  `?` and surface as runtime errors instead of panics. Updated
  in D10, Scope (lib.rs), and Task 3.5.

## v9 changelog

Round-7 reviewer flagged 2 High + 1 Medium against v8. All three
are real and fixed:

- **H1 (`#[non_exhaustive]` + struct-literal across crates)** —
  settled in [D8 update](#d8-push-context-schema). Rust rejects
  struct-literal construction of a `#[non_exhaustive]` type from
  outside its defining crate. Added a builder API:
  `AdapterPushContext::new()` (returns the default), plus
  `with_seed_url` / `with_seed_token` / `with_local` chained
  setters. The CLI's `dispatch_push` builds via the builder
  pattern, never the struct literal. `#[non_exhaustive]` stays so
  future field additions don't break out-of-tree adapter
  implementers (who only RECEIVE it via the trait method anyway).
- **H2 (`run_app_with_seeder` return-type mismatch with `run_app`)** —
  settled. Today `run_app` returns
  `anyhow::Result<impl IntoResponse>`; the opaque return type
  can't be implicitly converted to a concrete `SpinFullResponse`,
  so `run_app_with_seeder`'s fallthrough `run_app::<A>(req).await`
  wouldn't compile. **Resolution: change `run_app` to return
  `anyhow::Result<SpinFullResponse>`** (the concrete type already
  publicly aliased in `lib.rs`). This is **source-compatible with
  the generated scaffold handler signature** (NOT a legacy-Spin-
  variable carve-out — this migration is still hard-cutoff). The
  existing template handler signature
  `async fn handle(req: Request) -> anyhow::Result<impl IntoResponse>`
  keeps compiling because `SpinFullResponse: IntoResponse`, so the
  scaffold doesn't need re-running. Both `run_app` and
  `run_app_with_seeder` now return the same concrete type, and
  the fallthrough is a direct return.
  Documented in D9 + Scope + Task 3.5.
- **M1 (D12 401 message omits short-token case)** — settled in
  [D12 update](#d12-blocking-http-client). The 401 arm's message
  now spells out all four fail-closed reasons (unset / blank /
  whitespace-only / shorter than 16 bytes) so an operator who
  set a 4-character placeholder doesn't waste time debugging the
  wrong side.

## v8 changelog

Round-7 reviewer flagged 1 High + 1 Medium + 1 Low against v7.
Triage:

- **H1 (D1 `label` field unused)** — **already fixed in v7 on
  disk.** The reviewer was reading a stale snapshot. Line 329 of
  the v7 file matches `SpinConfigBackend::Spin { label, store }`
  and the error messages include `store \`{label}\`:`. No change
  in v8.
- **M1 (Stage 3.5 stale)** — **already fixed in v7 on disk.**
  Same stale-snapshot issue. Task 3.5 in v7 spells out
  `anyhow::Result<SpinFullResponse>`, the template body swap, and
  "unset / blank / shorter than 16 bytes" fail-closed behavior.
  No change in v8.
- **L1 (D10 prose test list out-of-sync with Task 3.2)** —
  **real.** Fixed in v8. D10's narrative list expanded to match
  Task 3.2's full row set, grouped by surface (auth /
  request-shape / store-resolution / write). Added a
  "keep-in-sync" note so the two lists can't drift again.

## v7 changelog

Round-6 reviewer flagged 1 High + 3 Medium against v6. All addressed:

- **H1 (Stage 8 smoke test would 401 itself)** — fixed. `test-token`
  is 10 bytes and falls below v6's 16-byte floor, so the smoke test
  would hit the fail-closed 401 path before any real KV write
  happens. Replaced with `test-token-1234567890` (21 bytes) in both
  the `spin up` env and the `app-demo-cli config push` env.
- **M1 (Stage 3 doesn't pin the 16-byte rule with a test)** —
  fixed. Added explicit test rows to Task 3.2 covering
  short-server-token paths: token unset → 401; token blank /
  whitespace-only → 401; token 15 bytes → 401 (just under the
  floor); token 16 bytes (offered correct on the wire) → 204 (just
  at the floor). Task 3.5 explicitly references the floor check
  when resolving `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN`.
- **M2 (`run_app_with_seeder` return shape mismatch with template)** —
  fixed. Spec'd as `anyhow::Result<SpinFullResponse>` to mirror
  the existing `run_app` shape and the scaffold template handler.
  Operators can switch from `run_app::<App>(req).await` to
  `run_app_with_seeder::<App>(req).await` with no signature change
  on the `#[http_service]` handler.
- **M/L (`label` unused in `SpinConfigBackend::Spin`)** — fixed.
  D1's `get` impl now uses `&self.label` in the unavailable error
  messages so the field is read (no `-D warnings` dead-code
  failure) AND so error logs name which platform store fired the
  error — useful when the operator has multiple config stores.

## v6 changelog

Round-5 reviewer flagged 2 Medium + 2 Low + 1 Medium/Low against v5.
All addressed:

- **M1 (Stage 1 acceptance vs Task 2.5)** — fixed. The Stage 1
  acceptance line previously said `config_store_contract_tests!`
  must pass on host + wasm32-wasip2. Task 2.5 (v4 fix) correctly
  scoped wasm KV out. Stage 1 now matches: "host-side
  `config_store_contract_tests!` against the `InMemory` backend;
  real KV write/read coverage lives in the Stage 8 `spin up` smoke
  test".
- **M2 (token min-length still open)** — settled. **Q2 closed YES:
  enforce a 16-byte minimum token at handler startup.** Below 16
  bytes (or unset/blank/whitespace-only) → fail-closed; every
  request to the seed route returns 401. Cheap to implement,
  prevents the worst accidental misconfiguration. D9 status table
  updated to spell this out. Removed from open questions.
- **M/L (Cargo.toml scope checklist stale)** — fixed. The scope
  line previously listed only `reqwest`; updated to mirror D11's
  full set: `reqwest` (optional under `cli`), and non-optional
  `serde` / `serde_json` / `subtle`.
- **L1 (Task 4.4 stale status list)** — fixed. The "Surface 401 /
  403 / 404 / 422" wording is replaced with "surface every D9
  status (400 / 401 / 403 / 404 / 405 / 415 / 422)" matching D12.
- **L2 (test backend uses `from_utf8_lossy`)** — fixed. The
  `InMemory` config-store backend now uses strict UTF-8 (matches
  production behavior). Added a doc comment + a "non-utf8 value
  → unavailable" test to the contract-test fixture so the
  divergence couldn't reappear.

## v5 changelog

Round-4 reviewer flagged 1 High + 4 Medium + 1 Low against v4. All
addressed:

- **H1 (stale `build_config_registry` snippet)** — settled in
  [Scope: edgezero-adapter-spin](#cratesedgezero-adapter-spin-the-heavy-crate)
  and [Stage 2 Task 2.4](#stage-2--runtime-backend-swap--registry-rewrite).
  Updated to async/error-propagating signature: returns
  `anyhow::Result<Option<ConfigRegistry>>`, awaits
  `SpinConfigStore::open(...).await?` per id. The
  `dispatch_with_registries` snippet shows
  `build_config_registry(config_meta, env).await?`.
- **M1 (`PushContext` naming collision)** — settled. The trait-level
  type is now **`AdapterPushContext`**; the CLI's internal
  `PushContext` (config.rs:42) keeps its name. Updated everywhere
  the new type is mentioned (D8, D12, Scope, Stages).
- **M2 (dispatch_push signature gap)** — settled in
  [D8 update](#d8-push-context-schema). `load_push_context` now
  resolves the `AdapterPushContext` upstream (it already takes
  `&ConfigPushArgs` and reads `env` for store resolution; adding
  the seed_url/token/local resolution there is natural). The
  resolved `AdapterPushContext` is stashed in the CLI's
  internal `PushContext` and `dispatch_push` reads it from there —
  no signature change required on `dispatch_push` itself.
- **M3 (stale D9 wording about `subtle` gating)** — fixed. D9's
  "gated under the spin feature" line removed; cross-reference to
  D11 ("non-optional dep") added.
- **M4 (in-memory store key shape)** — settled in
  [D1](#d1-backend-spin-kv-via-spin_sdkkey_valuestore) and
  [Scope](#cratesedgezero-adapter-spin-the-heavy-crate). The
  `InMemory` test backend is keyed plain `String → Bytes`. Removed
  the conflicting "(label, key)" mention in the Scope section and
  Task 2.2. The contract-test macro exercises one store at a time,
  so plain `key → bytes` is enough. The handler-side
  `InMemorySeedWriter` (D10) is the only place that needs to
  distinguish stores — that one stays keyed `(label, key)` because
  it serves multi-store seed requests.
- **L1 (version labels stale)** — fixed throughout: Stage 1 task
  text now says "Move this plan into specs"; the open-questions
  header is "(round 5)"; the settled-section header keeps "round 2"
  as the historical pointer for when those decisions were taken.

## v4 changelog

Round-3 reviewer flagged 4 High + 2 Medium + 1 Low against v3. All
addressed:

- **H1 (SpinConfigStore won't host-compile)** — settled in
  [D1 update](#d1-backend-spin-kv-via-spin_sdkkey_valuestore).
  Restored the cfg-gated backend enum pattern (matching the existing
  shape in `config_store.rs`). Wasm variant holds the opened
  `key_value::Store`; `InMemory` test variant holds a `BTreeMap`.
  Construction is async on wasm, sync in tests. The trait `get`
  dispatches on the variant.
- **H2 (`subtle` can't be wasm-only if core is host-tested)** —
  settled in [D11 update](#d11-dependency-gating). Move `subtle`
  out of the `spin` feature into a non-optional dependency. It's
  tiny and compiles on both host and wasm; the host tests can
  reach `subtle::ConstantTimeEq` without enabling `spin`.
- **H3 (JSON deps missing from scope)** — settled in
  [D11 update](#d11-dependency-gating). Add `serde` + `serde_json`
  as non-optional dependencies on `edgezero-adapter-spin`. Both
  are already workspace deps; both compile on host AND wasm. CLI
  POST body, seed handler core parser, and the migration story
  all need them.
- **H4 (`--local` could fall back to manifest prod URL)** —
  settled in [D3 update](#d3-config-push---local-for-spin) and
  [D8 update](#d8-push-context-schema). `--local` short-circuits
  the manifest fallback completely. New `PushContext::local: bool`
  field. Resolution chain when `local = true`: `--seed-url` CLI
  flag → `EDGEZERO__ADAPTERS__SPIN__LOCAL_SEED_URL` env → builtin
  default `http://127.0.0.1:3000/__edgezero/config/seed`. NEVER
  reads the manifest's prod `seed_url`.
- **M1 (Stage 2.5 overclaims wasm contract)** — settled. CI's spin
  wasm matrix runs `wasmtime run`, which doesn't host Spin KV.
  Task 2.5 now: host-side `config_store_contract_tests!` against
  the `InMemory` backend. Real KV write/read coverage moves to the
  end-to-end smoke test in Stage 8 that requires `spin up`.
- **M2 (CLI error mapping incomplete)** — settled in
  [D12 update](#d12-blocking-http-client). The CLI match now
  covers every intentional status: 400, 401, 403, 404, 405, 415, 422. Each gets a specific message.
- **L1 (`cargo tree | grep '^reqwest'` may miss prefixed entries)**
  — settled in [Stage 8 update](#stage-8--verify-gate). Replace
  with `cargo tree -i reqwest -p edgezero-adapter-spin --features
spin --target wasm32-wasip2` which errors when `reqwest` is not
  in the tree at all (the desired outcome). Pair check uses the
  same form for `subtle` (which MUST resolve).

## v3 changelog

Round-2 reviewer flagged 4 High + 2 Medium + 1 Low against v2. All
addressed:

- **H1 (sync trait vs async reqwest)** — settled in
  [D12](#d12-blocking-http-client). Use `reqwest::blocking::Client`
  so the existing sync `Adapter::push_config_entries*` trait shape
  is preserved. Workspace `reqwest` gets the `blocking` + `json`
  features added. No runtime needs to be threaded through the
  dispatcher.
- **H2 (`subtle` gated to wrong feature)** — settled. The token
  comparison runs in the wasm **seed handler**, not in the host
  CLI. Move `subtle` from `cli` to the `spin` feature in
  `edgezero-adapter-spin/Cargo.toml`. D9 updated to reflect.
- **H3 (store validation vs env-remapped platform names)** —
  settled in [D9 update](#d9-seed-handler-security). The seed
  handler validates the body's `store` field against the set of
  env-resolved **platform** labels (computed from
  `A::stores().config` × `EnvConfig::store_name("config", id)`),
  not the logical ids. Operators can run with
  `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod-config` and
  push a body `{"store": "prod-config", ...}` — the validation
  passes because that's the correct platform label.
- **H4 (host-testable seed signature)** — settled in
  [D10 update](#d10-testable-seed-writer). Split the handler into
  two layers: a host-compilable `handle_seed_request_core` that
  takes `edgezero_core::http::Request` / returns
  `edgezero_core::http::Response`, and a thin wasm wrapper that
  translates Spin types ↔ core types and lives under the wasm
  cfg gate. Unit tests target the core layer.
- **M1 (open-on-every-get)** — settled in [D1 update](#d1-backend-spin-kv-via-spin_sdkkey_valuestore).
  `SpinConfigStore` holds the opened `key_value::Store` handle.
  Construction is async, so `build_config_registry` becomes async
  too (called from `dispatch_with_registries`, already async).
  Missing `key_value_stores` declaration surfaces at registry
  build time, not on first config read.
- **M2 (manifest `seed_url` is open but assumed)** — settled.
  `[adapters.spin.commands].seed_url` IS a supported source.
  Moved from open questions to settled. Resolution order codified
  in D8.
- **L1 (`cargo tree | grep reqwest` exit-code semantics)** —
  fixed in Stage 8: use `! cargo tree … | grep -q reqwest` so
  the step fails ONLY when reqwest leaks into the wasm tree.

## v2 changelog

Reviewer flagged 4 High + 3 Medium + 1 Low against v1. All addressed:

- **H1 (per-id config registry)** — added Stage 2 Task 2.4: rewrite
  `build_config_registry` in `request.rs` to open one
  `spin_sdk::key_value::Store` per declared id using
  `env.store_name("config", id)` — mirroring the existing
  `build_kv_registry`. The old "one shared handle cloned for every id"
  shape goes away with Single→Multi.
- **H2 (seed URL/token transport schema)** — settled in new
  [D8](#d8-push-context-schema). Adds `PushContext` to the
  `push_config_entries*` trait signature, threads adapter command
  metadata through `dispatch_push`, and gives `ConfigPushArgs` two
  new CLI args (`--seed-url`, `--seed-token`) plus env fallbacks.
- **H3 (config-key validation)** — settled in
  [D1.5](#d15-validator-relaxation). `validate_app_config_keys`
  becomes a no-op for spin (KV accepts arbitrary key bytes). Existing
  uppercase / dash / start-char tests are deleted; new tests pin
  "any UTF-8 key passes".
- **H4 (seed handler security spec)** — settled in
  [D9](#d9-seed-handler-security). POST-only, fail-closed on missing
  or blank token, explicit status code table, and scaffolding is
  opt-in (`run_app_with_seeder` is what the scaffold uses; existing
  `run_app` is unchanged so downstream apps can opt out).
- **M1 (scaffold spin.toml key_value_stores)** — Stage 5 Task 5.4
  added: generator `spin.toml.hbs` declares
  `key_value_stores = ["app_config"]` by default. `provision`
  remains the safe path for already-scaffolded projects.
- **M2 (testable seed handler)** — settled in
  [D10](#d10-testable-seed-writer). Introduces `trait SeedWriter` so
  unit tests inject a fake; production uses a `SpinKvSeedWriter`
  that calls the hostcall.
- **M3 (HTTP client gating)** — settled in
  [D11](#d11-http-client-feature-gating). `reqwest` becomes a
  `cli`-feature-only dep on `edgezero-adapter-spin` (native-only);
  confirmed not pulled into the wasm target. Plan lists the exact
  Cargo.toml edits.
- **L1 (legacy flag)** — settled. **No `--legacy-spin-variables`
  flag.** Hard-cutoff matches the rest of the rewrite's posture.
  Removed from open questions.

Three remaining open questions for round 2 — see [Open questions](#open-questions-round-2).

## Why

Today `SpinConfigStore` wraps `spin_sdk::variables`. That has four
practical costs:

1. **No dynamic config.** Spin variables are baked into `spin.toml`
   at build time and override-able only via `SPIN_VARIABLE_<UPPER>`
   env vars or `spin up --env`. Pushing a new value mid-run requires
   a redeploy.
2. **Shared namespace with secrets.** `SpinSecretStore::get_bytes`
   ALSO reads `spin_sdk::variables`, so config keys and `#[secret]`
   values share the same flat namespace. We carry an explicit
   collision-check in `validate_typed_secrets` to compensate
   (`cli.rs:425-449`).
3. **Single-capable.** Spin is forced into the `single_store_kinds`
   spec axis for config (one flat variable namespace per app) while
   Cloudflare and Fastly are Multi. Operators can't have e.g.
   `app_config` + `tenant_overrides` as two separate Spin stores.
4. **No platform parity.** `config push --adapter spin` edits
   `spin.toml`; the other two cloud adapters shell out to a
   platform-native bulk-write CLI (`fastly config-store-entry create`
   / `wrangler kv bulk put`). The mental model split is real.

KV-backed config fixes all four.

## Design decisions

### D1. Backend: Spin KV via `spin_sdk::key_value::Store`

Runtime change in `crates/edgezero-adapter-spin/src/config_store.rs`:

**v4**: keep the existing **cfg-gated backend enum** pattern from
today's `config_store.rs` so the file compiles on host (for tests)
without dragging in `spin_sdk` types. The wasm variant holds the
opened `key_value::Store`; the `InMemory` test variant holds a
`BTreeMap<String, Bytes>` (was `HashMap<String, String>` in the
variables-backed impl). Construction is async on wasm, sync in
tests; the trait method dispatches on the variant.

```rust
pub struct SpinConfigStore {
    inner: SpinConfigBackend,
}

enum SpinConfigBackend {
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    Spin {
        label: String,
        store: spin_sdk::key_value::Store,   // opened ONCE at dispatch
    },
    #[cfg(test)]
    InMemory(BTreeMap<String, bytes::Bytes>),
    /// Never constructed; keeps the enum inhabited outside production Spin and tests.
    #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
    _Uninhabited(std::convert::Infallible),
}

impl SpinConfigStore {
    /// Open the platform store once. Called from
    /// `build_config_registry` during dispatch setup. Wasm-only;
    /// tests use `from_entries`.
    #[cfg(all(feature = "spin", target_arch = "wasm32"))]
    pub async fn open(label: String) -> Result<Self, ConfigStoreError> {
        let store = spin_sdk::key_value::Store::open(&label).await
            .map_err(|err| ConfigStoreError::unavailable(format!("open `{label}`: {err}")))?;
        Ok(Self { inner: SpinConfigBackend::Spin { label, store } })
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = (String, bytes::Bytes)>) -> Self {
        Self { inner: SpinConfigBackend::InMemory(entries.into_iter().collect()) }
    }
}

#[async_trait(?Send)]
impl ConfigStore for SpinConfigStore {
    async fn get(&self, key: &str) -> Result<Option<String>, ConfigStoreError> {
        match &self.inner {
            #[cfg(all(feature = "spin", target_arch = "wasm32"))]
            SpinConfigBackend::Spin { label, store } => {
                // v7 (round-6 M/L): use `label` in error wording so
                // (a) the field isn't dead-code under -D warnings,
                // (b) the operator running multi-store sees which
                //     platform store fired the failure.
                match store.get(key).await {
                    Ok(Some(bytes)) => String::from_utf8(bytes).map(Some).map_err(|err| {
                        ConfigStoreError::unavailable(format!(
                            "store `{label}`: non-utf8 value for `{key}`: {err}"
                        ))
                    }),
                    Ok(None) => Ok(None),
                    Err(err) => Err(ConfigStoreError::unavailable(format!(
                        "store `{label}`: {err}"
                    ))),
                }
            }
            #[cfg(test)]
            SpinConfigBackend::InMemory(map) => match map.get(key) {
                Some(bytes) => String::from_utf8(bytes.to_vec()).map(Some).map_err(|err| {
                    // v6 fix (L2): strict UTF-8 to match the wasm
                    // backend's behaviour. `from_utf8_lossy` would
                    // hide a divergence between test and prod.
                    ConfigStoreError::unavailable(format!("non-utf8 value for `{key}`: {err}"))
                }),
                None => Ok(None),
            },
            #[cfg(not(any(all(feature = "spin", target_arch = "wasm32"), test)))]
            SpinConfigBackend::_Uninhabited(never) => match *never {},
        }
    }
}
```

Drops the `.→__` translation (KV accepts arbitrary key bytes).

### D1.5. Validator relaxation

Reviewer (H3): the existing `validate_app_config_keys` enforces Spin
variable syntax (lowercase, `^[a-z][a-z0-9_]*$` after `.→__`). With
KV-backed config, none of that applies — KV stores accept arbitrary
key bytes.

Concrete change in `crates/edgezero-adapter-spin/src/cli.rs`:

- `validate_app_config_keys`: collapses to `Ok(())`. The function stays
  in place (trait shape) but no longer rejects anything.
- `translate_key_for_spin`: deleted. Callers (push, validator) read
  keys verbatim.
- `is_valid_spin_key` / `spin_key_rule_violation`: stay — still used
  by `validate_typed_secrets` for `#[secret]` value validation
  (secrets still live in variables; see D7).
- Tests deleted (Stage 6 Task 6.1):
  - `validate_app_config_keys_*` tests covering uppercase rejection,
    dash rejection, leading-digit rejection, etc.
- Tests added (Stage 6 Task 6.2):
  - `validate_app_config_keys_accepts_any_utf8` (covers `Greeting`,
    `feature-flag`, `1numeric_start`, `with.dots`, `with spaces`).

### D2. Push: HTTP POST to a seeding handler

Spin has no `spin kv put` CLI subcommand and no bulk-write hostcall
reachable from outside the wasm runtime. Two options ruled out:

- **Write Spin's SQLite KV file directly** — Spin doesn't guarantee
  schema stability across versions. Brittle.
- **Wait for upstream `spin kv` CLI** — months of latency at best.

So: the adapter ships a small **seeding handler** that
`app-demo-cli config push --adapter spin` HTTP-POSTs.

### D3. `config push --local` for Spin

With D2, `--local` and the default push both HTTP-POST to the
seeding handler, but the URL resolution chains are **strictly
disjoint** — `--local` never falls back to the manifest's prod URL.
This protects an operator who forgets to start `spin up` locally
from accidentally pushing to production.

**Without `--local`** (prod push), `seed_url` resolves in order:

1. `--seed-url` CLI arg.
2. `EDGEZERO__ADAPTERS__SPIN__SEED_URL` env.
3. `[adapters.spin.commands].seed_url` in `edgezero.toml`.

Errors with a clear message if none are set.

**With `--local`** (local push), `seed_url` resolves in order:

1. `--seed-url` CLI arg (explicit operator override always wins).
2. `EDGEZERO__ADAPTERS__SPIN__LOCAL_SEED_URL` env (separate from
   the prod env var — operators who set both don't accidentally
   leak prod URL into local pushes).
3. Builtin default `http://127.0.0.1:3000/__edgezero/config/seed`.

The manifest's `[adapters.spin.commands].seed_url` is **never read**
when `--local` is set. The dispatcher needs to know about
`args.local` before building `AdapterPushContext` — see D8.

### D4. Provision: declare the KV store in `spin.toml`

`provision --adapter spin` already edits `spin.toml`. Extension: for
each declared `[stores.config].id`, append the env-resolved platform
name to the component's `key_value_stores = [...]` list. Idempotent
on existing entries. Same pattern as the existing KV provision flow.

### D5. Capability: Spin becomes Multi for config

Drop `"config"` from `Spin::single_store_kinds` (currently
`&["config", "secrets"]` → `&["secrets"]`). Strict validation no
longer rejects `[stores.config].ids.len() > 1` for spin.

### D6. Collision check goes away

`validate_typed_secrets` currently builds a Spin variable name set of
`{flattened config keys} ∪ {#[secret] values}` and errors on
duplicates. With config off the variables namespace, the
intersection is empty by construction. Delete the check + spec/doc
text that explains it.

### D7. Secrets stay on variables (unchanged)

`SpinSecretStore` continues to use `spin_sdk::variables`. The
single-flat-namespace constraint applies only to secrets now.
`#[secret]` values still get the lowercase-only translation; the
runtime check stays.

### D8. Push context schema

Reviewer (H2): the v1 plan said "no CLI-side changes" but then
required the Spin adapter to read seed URL/token from somewhere the
trait signature doesn't expose. Fixed by introducing
`AdapterPushContext` (v5: renamed from v4's `PushContext` to avoid
collision with the CLI's internal `PushContext` struct at
[config.rs:42]).

Changes to `crates/edgezero-adapter/src/registry.rs`:

```rust
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct AdapterPushContext<'a> {
    /// Already-resolved seed URL. Caller (CLI dispatch) follows the
    /// resolution chain for prod or local per D3 and produces the
    /// final string here. `None` means "no URL was set anywhere
    /// in the resolution chain" -- the adapter errors loudly.
    pub seed_url: Option<&'a str>,
    /// Already-resolved seed token.
    pub seed_token: Option<&'a str>,
    /// `true` when the operator passed `--local`. Adapters that
    /// have a separate local-emulator path use this to pick the
    /// right writeback target; adapters where local == default
    /// can ignore it.
    pub local: bool,
}

impl<'a> AdapterPushContext<'a> {
    /// Construct a default context: no seed URL / token, prod (not
    /// local). v9 (round-7 H1): Rust rejects struct-literal
    /// construction of `#[non_exhaustive]` types from outside the
    /// defining crate, so the CLI MUST build via this constructor
    /// and the `with_*` setters below.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_seed_url(mut self, url: &'a str) -> Self {
        self.seed_url = Some(url);
        self
    }

    #[must_use]
    pub fn with_seed_token(mut self, token: &'a str) -> Self {
        self.seed_token = Some(token);
        self
    }

    #[must_use]
    pub fn with_local(mut self, local: bool) -> Self {
        self.local = local;
        self
    }
}

fn push_config_entries(
    &self,
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    component_selector: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    push_ctx: &AdapterPushContext<'_>, // NEW
    dry_run: bool,
) -> Result<Vec<String>, String> { ... }
```

`AdapterPushContext` is non-exhaustive so we can grow it later
without breaking downstream adapters that RECEIVE it via the
trait method. The CLI (which CONSTRUCTS it) is in-tree and uses
the builder API, so the `#[non_exhaustive]` constraint is
honoured at the source-code level. Same shape on
`push_config_entries_local`.

Changes to `crates/edgezero-cli/src/args.rs`:

```rust
pub struct ConfigPushArgs {
    /* … existing fields … */
    /// Seed URL for adapters that push via HTTP (currently spin).
    /// Resolution: this flag → `EDGEZERO__ADAPTERS__<NAME>__SEED_URL`
    /// → `[adapters.<name>.commands].seed_url`.
    #[arg(long)]
    pub seed_url: Option<String>,
    /// Seed token for adapters that push via HTTP (currently spin).
    /// Resolution: this flag → `EDGEZERO__ADAPTERS__<NAME>__SEED_TOKEN`.
    /// Never read from `edgezero.toml` (don't put secrets in the
    /// manifest).
    #[arg(long)]
    pub seed_token: Option<String>,
}
```

Manifest schema: `ManifestAdapterCommands` (currently lives in
`crates/edgezero-core/src/manifest.rs`) gains an optional
`seed_url: Option<String>` field. Already covered by `#[non_exhaustive]`,
so additive.

Changes to `crates/edgezero-cli/src/config.rs`:

The CLI's internal `PushContext` struct (config.rs:42) gains a
field carrying the resolved adapter context:

```rust
struct PushContext {
    // … existing fields …
    /// Resolved by `load_push_context` from CLI args + env +
    /// manifest per D3's prod/local chains. Stashed here so
    /// `dispatch_push` can pass it through to the trait method
    /// without re-reading args / env. Owned strings (not
    /// borrows) so the lifetime story stays simple.
    adapter_push_ctx: ResolvedAdapterPushContext,
}

struct ResolvedAdapterPushContext {
    seed_url: Option<String>,
    seed_token: Option<String>,
    local: bool,
}
```

`load_push_context(args: &ConfigPushArgs)` (which already takes
`&ConfigPushArgs` and reads `env` for store resolution) gains the
resolution logic per D3's disjoint chains:

```rust
fn load_push_context(args: &ConfigPushArgs) -> Result<PushContext, String> {
    // … existing manifest + store resolution …

    let env = EnvConfig::from_env();
    let name = &args.adapter;

    let seed_url = args.seed_url.clone().or_else(|| {
        if args.local {
            // D3 local chain: env → builtin default. Manifest NEVER consulted.
            env.get(&["adapters", name, "local_seed_url"])
                .map(str::to_owned)
                .or_else(|| Some("http://127.0.0.1:3000/__edgezero/config/seed".to_owned()))
        } else {
            // D3 prod chain: env → manifest.
            env.get(&["adapters", name, "seed_url"]).map(str::to_owned)
                .or_else(|| manifest.adapters.get(name)
                    .and_then(|cfg| cfg.adapter.commands.seed_url.clone()))
        }
    });

    let seed_token = args.seed_token.clone()
        .or_else(|| env.get(&["adapters", name, "seed_token"]).map(str::to_owned));
    // Manifest never consulted for tokens, even on the prod chain.

    Ok(PushContext {
        // … existing fields …
        adapter_push_ctx: ResolvedAdapterPushContext {
            seed_url, seed_token, local: args.local,
        },
    })
}
```

`dispatch_push` (unchanged signature) just borrows from the
already-resolved context when building the `AdapterPushContext`
to hand the trait method:

```rust
fn dispatch_push(ctx: &PushContext, entries: &[(String, String)],
                 dry_run: bool, local: bool) -> Result<(), String> {
    let r = &ctx.adapter_push_ctx;
    // v9 (round-7 H1): build via the builder, NOT a struct literal —
    // AdapterPushContext is #[non_exhaustive] and external crates
    // can't use struct-literal construction.
    let mut push_ctx = AdapterPushContext::new().with_local(r.local);
    if let Some(url) = r.seed_url.as_deref() {
        push_ctx = push_ctx.with_seed_url(url);
    }
    if let Some(token) = r.seed_token.as_deref() {
        push_ctx = push_ctx.with_seed_token(token);
    }
    let lines = if local {
        ctx.adapter.push_config_entries_local(/* … */, &push_ctx, dry_run)?
    } else {
        ctx.adapter.push_config_entries(/* … */, &push_ctx, dry_run)?
    };
    // … existing logging …
}
```

For non-Spin adapters this is constructed but unused — costs nothing.

This change is **breaking** for any out-of-tree adapter that
implements `Adapter::push_config_entries*` (no in-tree adapter
outside the four ships today). Document in the next release notes.

### D9. Seed handler security

Reviewer (H4): pin the security contract before code.

**Route**: `/__edgezero/config/seed`. Single fixed path, not
configurable per app — keeps every Spin deploy's seeding surface
predictable for ops scripts.

**Method**: POST only. GET/PUT/DELETE/HEAD/OPTIONS/PATCH → 405.

**Headers**:

- `x-edgezero-seed: <token>` — REQUIRED. Compared constant-time
  against `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN`.
- `content-type: application/json` — REQUIRED. Anything else → 415.

**Body shape** (validated against this schema):

```json
{
  "store": "app_config",
  "entries": [
    { "key": "greeting", "value": "hello" },
    { "key": "service.timeout_ms", "value": "1500" }
  ]
}
```

The `store` field is the **platform label** (what `Store::open(name)`
needs), not the logical id. The handler builds the set of accepted
labels from `A::stores().config` × `EnvConfig::store_name("config", id)`
— so an operator running with
`EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod-config` pushes
`{"store": "prod-config", …}` and the validation passes. A body
mentioning the logical id `"app_config"` in that environment is
correctly rejected (404).

The CLI does the resolution before POSTing — `dispatch_push` already
resolves the platform label via `env.store_name("config", id)`, so
the body the CLI emits matches what the handler expects.

**Status code table**:

| Code | Condition                                                                                                                                        |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| 204  | Success. Body empty.                                                                                                                             |
| 400  | Malformed JSON, missing `store`, missing/empty `entries`, or any `key`/`value` not a string.                                                     |
| 401  | `x-edgezero-seed` header missing, or `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` env unset/blank/whitespace-only/shorter than 16 bytes (fail-closed). |
| 403  | `x-edgezero-seed` header present but does not match the env token.                                                                               |
| 404  | `store` does not match any env-resolved platform label for a declared `[stores.config].id`.                                                      |
| 405  | Non-POST method.                                                                                                                                 |
| 415  | `content-type` not `application/json`.                                                                                                           |
| 422  | KV store open / set hostcall returned an error mid-write (partial-write — see body for the failed key).                                          |

**Fail-closed contract**: if `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN`
is unset, blank, whitespace-only, OR **shorter than 16 bytes**
(v6 — round-5 Q2 settled), EVERY request to the seed route returns
401 — even with no `x-edgezero-seed` header. We never default a
token, never accept "no token = no auth", and never accept a
short-enough token to brute-force in a reasonable time. An operator
who forgot to set the token, or set a 4-character placeholder, gets
a clean error rather than an open writeable endpoint.

**Why 16 bytes**: at 8 bits/byte that's 128 bits of token surface.
Even a single-shot guess against a constant-time compare has
~2^-128 odds; rate-limiting from the Spin runtime kills any
practical brute-force. Below 16 bytes the operator is almost
certainly using a placeholder ("dev", "test123") that doesn't
belong in production OR local.

**Token comparison**: `subtle::ConstantTimeEq` (workspace dep,
non-optional on the spin adapter per [D11](#d11-dependency-gating)
— v4's "gated under `spin` feature" was wrong; the host
unit tests for `handle_seed_request_core` need to reach this type
without enabling `--features spin`). Prevents timing-oracle
leakage of the token prefix.

**Logging**: log auth failures at `warn` level with the source IP
(via `spin-client-addr` header) but NEVER the offered token.

**Opt-in vs always-scaffolded**: scaffold-side OPT-IN — the
generator emits `run_app_with_seeder` for new projects, but
`run_app` (no seeding route) stays available for projects that
explicitly opt out by switching the entrypoint. Existing
deployments keep `run_app` and aren't affected.

### D10. Testable seed writer

Reviewer (M2): the v1 plan called for unit tests on the seed handler
but `spin_sdk::key_value` is wasm-runtime-bound. Solution: trait +
fake.

**v3**: split the handler into two layers so tests compile on the
host without dragging in `spin_sdk` types. The core layer is
host-compilable; the wasm wrapper translates Spin types to/from
`edgezero_core::http::{Request, Response}`.

`crates/edgezero-adapter-spin/src/seed.rs`:

```rust
// ---- Core layer (host-compilable) ---------------------------------

#[async_trait(?Send)]
pub(crate) trait SeedWriter {
    async fn write(&self, store: &str, key: &str, value: &str) -> Result<(), SeedError>;
}

/// Host-compilable seed handler core. Takes a core HTTP `Request`
/// (body already buffered into `Body::Once`) and returns a core HTTP
/// `Response`. Parsing, auth, status-code routing, and the writer
/// dispatch all live here. NO spin_sdk references.
pub(crate) async fn handle_seed_request_core<W: SeedWriter>(
    req: &edgezero_core::http::Request,
    writer: &W,
    valid_token: Option<&str>,        // None → fail-closed (401)
    known_platform_labels: &[String], // env-resolved labels per H3
) -> edgezero_core::http::Response { ... }

#[cfg(test)]
pub(crate) struct InMemorySeedWriter {
    pub(crate) entries: Mutex<BTreeMap<(String, String), String>>,  // (label, key) → value
}

// ---- Wasm wrapper (spin-runtime only) -----------------------------

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub(crate) struct SpinKvSeedWriter;

#[cfg(all(feature = "spin", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl SeedWriter for SpinKvSeedWriter {
    async fn write(&self, store: &str, key: &str, value: &str) -> Result<(), SeedError> {
        let kv = spin_sdk::key_value::Store::open(store).await?;
        kv.set(key, value.as_bytes()).await?;
        Ok(())
    }
}

/// Thin wasm wrapper: Spin `Request` → core `Request` → core handler
/// → core `Response` → Spin `Response`. Lives where the existing
/// `into_core_request` / `from_core_response` helpers do.
///
/// v10 (round-8 H1): returns `anyhow::Result<SpinFullResponse>` so
/// it matches `run_app`'s shape (allows `?` at the call site in
/// `run_app_with_seeder` instead of a `.expect()` panic).
#[cfg(all(feature = "spin", target_arch = "wasm32"))]
pub(crate) async fn handle_seed_request_spin(
    req: spin_sdk::http::Request,
    writer: &SpinKvSeedWriter,
    valid_token: Option<&str>,
    known_platform_labels: &[String],
) -> anyhow::Result<SpinFullResponse> {
    let core_req = crate::request::into_core_request(req).await?;
    let core_resp = handle_seed_request_core(&core_req, writer,
        valid_token, known_platform_labels).await;
    Ok(crate::response::from_core_response(core_resp).await?)
}
```

Host-compilable unit tests (live in `seed.rs`'s `#[cfg(test)] mod
tests`). The full row set lives in Task 3.2 — keep this list in
sync if either side moves:

- **Auth surface (v6 16-byte floor + fail-closed)**:
  - Token unset (env missing) → 401.
  - Token blank (`""`) → 401.
  - Token whitespace-only (`"   "`) → 401.
  - Token 15 bytes (just under the floor) → 401, even when the
    client offers the matching token on the wire.
  - Token exactly 16 bytes + matching wire token → 204
    (just-at-the-floor sentinel).
  - Token 16 bytes + missing `x-edgezero-seed` → 401.
  - Token 16 bytes + wrong `x-edgezero-seed` → 403.
- **Request-shape surface**:
  - Non-POST method → 405.
  - `content-type` not `application/json` → 415.
  - Malformed JSON → 400.
  - Missing `store` / `entries` / non-string values → 400.
- **Store-resolution surface**:
  - Unknown store (no env-resolved label matches) → 404.
- **Write surface**:
  - `SeedWriter::write` errors mid-stream → 422 (body names the
    failed key).
  - Happy path → 204 + `InMemorySeedWriter` recorded all entries.

### D11. Dependency gating

Three new deps. Different gates for different reasons:

| Dep                    | Gate                           | Why                                                                                                                                                                                                                                |
| ---------------------- | ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `reqwest`              | `cli` feature (host-only)      | Pulls `tokio` + TLS — would explode the wasm bundle and fail to compile on `wasm32-wasip2`. Only the host CLI uses it.                                                                                                             |
| `subtle`               | **non-optional** (host + wasm) | Used by the seed handler core (wasm) AND by its host-compilable unit tests (D10). Reviewer H2: can't be `spin`-gated when host tests reach `ConstantTimeEq` without `--features spin`. Tiny dep; compiles cleanly on both targets. |
| `serde` + `serde_json` | **non-optional** (host + wasm) | Reviewer H3: seed core parses JSON (wasm), CLI builds JSON body (host), `--features cli` body type derives `Serialize` / `Deserialize`. Both already workspace deps; both compile on host AND wasm.                                |

Concrete `Cargo.toml` change on `crates/edgezero-adapter-spin`:

```toml
[features]
spin = [
    "dep:spin-sdk",
]
cli = [
    "dep:edgezero-adapter",
    "edgezero-adapter/cli",
    "dep:ctor",
    "dep:reqwest",         # NEW (host HTTP push)
    "dep:toml",
    "dep:toml_edit",
    "dep:walkdir",
]

[dependencies]
# … existing entries …
reqwest     = { workspace = true, optional = true }
serde       = { workspace = true }   # NEW; non-optional
serde_json  = { workspace = true }   # NEW; non-optional
subtle      = { workspace = true }   # NEW; non-optional
```

**Why subtle is not optional**: gating it under `spin` would hide
it from the host build, but the host unit tests for
`handle_seed_request_core` (D10) need to construct `subtle::Choice`
and friends. Making it non-optional is the simplest correct
answer; the dep is ~5 KB compiled.

**Why serde/serde_json are not optional**: similarly, the core
seed handler runs JSON parsing on both wasm (production) and host
(tests). The Cargo features model can't express "available in
wasm under `spin` AND in host under `cfg(test)`" cleanly — making
it always-on does the right thing.

Verification step (added to Stage 8 gate): use `cargo tree -i`
which errors when the dep is not in the tree at all (per L1). Two
checks:

```sh
# reqwest MUST NOT be in the wasm tree.
# `cargo tree -i <pkg>` exits non-zero when <pkg> isn't a dep --
# which is the success case here. Invert with `!`:
! cargo tree -i reqwest -p edgezero-adapter-spin \
    --features spin --target wasm32-wasip2 2>/dev/null

# subtle / serde_json MUST be in the wasm tree.
# `cargo tree -i <pkg>` succeeds when the dep IS present:
cargo tree -i subtle -p edgezero-adapter-spin \
    --features spin --target wasm32-wasip2
cargo tree -i serde_json -p edgezero-adapter-spin \
    --features spin --target wasm32-wasip2
```

### D12. Blocking HTTP client

Reviewer (H1): the existing `Adapter::push_config_entries*` trait
methods are SYNCHRONOUS. `reqwest::Client::post` is async. Two
options:

- **(a) `reqwest::blocking`** — keeps the sync trait shape. Needs
  `blocking` + `json` features on the workspace `reqwest`.
- **(b) Async trait + runtime in dispatcher** — clean but bigger
  blast radius (every adapter impl signature changes; CLI gets a
  tokio dep).

**Resolution: (a).** Workspace `Cargo.toml` change:

```toml
reqwest = { version = "0.13", default-features = false,
            features = ["rustls", "blocking", "json"] }
```

Spin's `push_config_entries`:

```rust
let client = reqwest::blocking::Client::new();
let response = client
    .post(&seed_url)
    .header("x-edgezero-seed", token)
    .json(&body)        // serde-derived; `json` feature
    .send()
    .map_err(|err| match err.is_connect() {
        true  => format!("seed POST to {seed_url} failed: connection refused. Is the Spin app running?"),
        false => format!("seed POST to {seed_url} failed: {err}"),
    })?;
// Map every status the handler intentionally emits (D9 status table).
match response.status().as_u16() {
    204 => Ok(vec![format!(
        "pushed {} entries to seed handler at {seed_url}",
        entries.len()
    )]),
    400 => Err(format!(
        "seed handler rejected (400 Bad Request): {}. Check CLI version / store id.",
        response.text().unwrap_or_default()
    )),
    401 => Err(format!(
        "seed handler rejected (401 Unauthorized). Fail-closed reasons (D9): \
         server-side `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` is unset, blank, \
         whitespace-only, or shorter than 16 bytes; OR your client-side \
         `--seed-token` / `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` is missing. \
         Check the server's env first -- a 4-character placeholder triggers \
         this even when the wire token matches."
    )),
    403 => Err(format!(
        "seed handler rejected (403 Forbidden): x-edgezero-seed mismatch. \
         Check that the token on the client matches the server's \
         EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN"
    )),
    404 => Err(format!(
        "seed handler rejected (404 Not Found): store `{}` is not a recognised platform label. \
         Check `[stores.config].ids` and any EDGEZERO__STORES__CONFIG__<ID>__NAME overrides",
        store.platform
    )),
    405 => Err(format!(
        "seed handler rejected (405 Method Not Allowed). \
         This usually means a transparent proxy rewrote the POST -- check intermediaries"
    )),
    415 => Err(format!(
        "seed handler rejected (415 Unsupported Media Type). \
         Internal: the CLI should always set content-type: application/json"
    )),
    422 => Err(format!(
        "seed handler rejected (422 Unprocessable): KV write failed mid-stream: {}",
        response.text().unwrap_or_default()
    )),
    other => Err(format!(
        "seed handler returned unexpected status {other}: {}",
        response.text().unwrap_or_default()
    )),
}
```

The blocking client is fine for a CLI binary; it spins up its own
single-thread tokio runtime under the hood. No external runtime
needed.

## Migration story (hard-cutoff)

Existing Spin deployments break on upgrade. No legacy flag.

- Apps that read config via `ctx.config_store_default()` keep working
  unchanged after a `config push --adapter spin` against the new
  backend.
- Apps that read config via `spin_sdk::variables::get(...)` directly
  break. They must either (a) move to the EdgeZero abstraction, or
  (b) keep their values in `[variables]` and stop using EdgeZero's
  config store for those keys.
- Existing `spin.toml` files that declare config keys in
  `[variables]` need a one-time migration: the values move from
  `[variables].<key>` (and `[component.<id>.variables].<key>`) to
  the KV store via `config push --adapter spin`. After confirming
  the values land in KV, the operator manually removes the
  now-orphaned `[variables].<key>` entries.

Migration guide section title: "Spin: variables → KV for config
(2026-Q3)".

## Scope (files touched)

### crates/edgezero-adapter-spin (the heavy crate)

- `src/config_store.rs` — rewrite `SpinConfigStore` per
  [D1](#d1-backend-spin-kv-via-spin_sdkkey_valuestore). Cfg-gated
  backend enum: wasm variant holds the opened
  `key_value::Store`; the `InMemory` test variant is keyed
  plain `String → bytes::Bytes` (one store at a time — that's all
  the contract-test macro exercises). Drop `translate_key`.
- `src/request.rs` — rewrite `build_config_registry` as **async**
  per H1 (v5: returns `anyhow::Result` so registry-build errors
  propagate up the dispatcher):
  ```rust
  async fn build_config_registry(
      meta: Option<StoreMetadata>,
      env: &EnvConfig,
  ) -> anyhow::Result<Option<ConfigRegistry>> {
      let Some(meta) = meta else { return Ok(None); };
      let mut by_id = BTreeMap::new();
      for id in meta.ids {
          let label = env.store_name("config", id);  // per-id env resolution
          let store = SpinConfigStore::open(label).await
              .map_err(|err| anyhow::anyhow!(
                  "open config store for id `{id}`: {err}"
              ))?;
          by_id.insert((*id).to_owned(),
              ConfigStoreHandle::new(Arc::new(store)));
      }
      Ok(StoreRegistry::from_parts(by_id, meta.default.to_owned()))
  }
  ```
  And in `dispatch_with_registries`:
  ```rust
  let config_registry = build_config_registry(config_meta, env).await?;
  ```
  Mirrors `build_kv_registry`'s existing async + Result shape.
- `src/cli.rs` —
  - `push_config_entries`: HTTP POST against `seed_url` (resolved
    from `AdapterPushContext` via D8). Body is the D9 schema.
    Uses `reqwest` (D11/D12). Surfaces every status code from D9
    with clear messages (D12).
  - `push_config_entries_local`: defaults `seed_url` to
    `http://127.0.0.1:3000/__edgezero/config/seed` if
    `AdapterPushContext` didn't supply one. Otherwise identical.
  - `provision`: emit `key_value_stores = [...]` entries per D4.
    Drop the `[variables]` / `[component.<id>.variables]`
    config-declaration writes (the migration guide tells operators
    to remove existing ones).
  - `validate_app_config_keys`: no-op per D1.5. Delete
    `translate_key_for_spin`.
  - `validate_typed_secrets`: delete the collision-check block per
    D6. Keep the secret-name format check.
  - `single_store_kinds`: returns `&["secrets"]`.
- `src/seed.rs` — NEW. `SeedWriter` trait + `SpinKvSeedWriter` +
  `handle_seed_request`. ~200 LoC + tests.
- `src/lib.rs` — `pub mod seed;`. Plus two functions sharing
  the same concrete return type (v9 round-7 H2 fix — `run_app`'s
  old `impl IntoResponse` opaque return type made the fall-through
  uninvocable from `run_app_with_seeder`). **v11 round-9 M1**:
  drop `IntoResponse` from the
  `use spin_sdk::http::{IntoResponse, Request as SpinRequest,
Response as SpinResponse}` import line — once `run_app` returns
  `SpinFullResponse`, `IntoResponse` is no longer referenced and
  the wasm-clippy gate would fail on `unused_imports`.

  ```rust
  pub async fn run_app<A: Hooks>(req: SpinRequest)
      -> anyhow::Result<SpinFullResponse> { /* existing body */ }

  pub async fn run_app_with_seeder<A: Hooks>(req: SpinRequest)
      -> anyhow::Result<SpinFullResponse> {
      // Route /__edgezero/config/seed to the seed handler, else
      // fall through to run_app::<A>. v10 (round-8 H1):
      // handle_seed_request_spin now also returns
      // anyhow::Result<SpinFullResponse>, so both arms are
      // type-compatible.
      if req.uri().path() == "/__edgezero/config/seed" {
          handle_seed_request_spin(req, &SpinKvSeedWriter, …).await
      } else {
          run_app::<A>(req).await
      }
  }
  ```

  Changing `run_app` from `impl IntoResponse` → `SpinFullResponse`
  is **source-compatible with the generated scaffold handler
  signature** (NOT a Spin-variable backwards-compat carve-out —
  this migration stays hard-cutoff). `SpinFullResponse: IntoResponse`,
  so the existing
  `async fn handle(req: Request) -> anyhow::Result<impl IntoResponse>`
  template signature keeps accepting the value through type
  coercion — no need to regenerate already-scaffolded projects.
  Token resolved from
  `EnvConfig::get(&["adapters", "spin", "seed_token"])`; if unset
  / blank / shorter than 16 bytes (D9), every request hitting the
  seed route returns 401 (fail-closed).

- `src/templates/src/lib.rs.hbs` — scaffold uses
  `run_app_with_seeder` per
  [D9 opt-in scaffolding](#d9-seed-handler-security).
- `src/templates/spin.toml.hbs` — add
  `key_value_stores = ["app_config"]` to the default
  `[component.*]` block per M1. Scaffolded projects work with
  `config push --adapter spin --local` out of the box.
- `Cargo.toml` — per D11: `reqwest` optional under `cli` feature
  (host HTTP push); `serde`, `serde_json`, `subtle` non-optional
  (used by both the wasm seed handler core and its host-compilable
  unit tests, so feature-gating would break the test layer).

### crates/edgezero-adapter (the trait)

- `src/registry.rs` — `AdapterPushContext` struct + threaded
  through `push_config_entries` / `push_config_entries_local`
  per D8.

### crates/edgezero-core

- `src/manifest.rs` — `ManifestAdapterCommands::seed_url:
Option<String>` per D8 (additive; `#[non_exhaustive]` already in
  place).

### crates/edgezero-cli

- `src/args.rs` — `ConfigPushArgs::seed_url` / `seed_token` per D8.
- `src/config.rs` — per D8: `load_push_context` resolves the
  `ResolvedAdapterPushContext` (owned `String`s) and stashes it
  on the CLI's `PushContext`. `dispatch_push` constructs the
  borrowing `AdapterPushContext<'_>` from it and hands that to
  the trait method. Update the `push_args` test fixture.

### examples/app-demo

- `crates/app-demo-adapter-spin/src/lib.rs` — switch
  `run_app` → `run_app_with_seeder`.
- `crates/app-demo-adapter-spin/spin.toml` — add `app_config` to
  `key_value_stores = [...]`. Remove `[variables].greeting` /
  `feature__new_checkout` / `service__timeout_ms` (now in KV).
- `edgezero.toml` — `[adapters.spin.commands].seed_url =
"http://127.0.0.1:3000/__edgezero/config/seed"` so contributors
  don't need to set the env var locally.

### Workspace

- `Cargo.toml` — three changes:
  - `reqwest`: add `blocking` + `json` features to the existing
    workspace declaration so the CLI's sync push (D12) works:
    `reqwest = { version = "0.13", default-features = false,
features = ["rustls", "blocking", "json"] }`.
  - `subtle`: NEW workspace dep for constant-time token
    comparison: `subtle = "2"` (non-optional per D11; used by
    both the wasm seed handler core and its host tests).
  - `serde` / `serde_json`: already workspace deps; just declared
    as non-optional on `edgezero-adapter-spin` per D11.

### docs

- `guide/adapters/spin.md` — rewrite config-store section:
  KV-backed, no `.→__` translation, no collision check. New
  seed-handler section explaining the security model + token
  rotation guidance.
- `guide/manifest-store-migration.md` — new section "Spin:
  variables → KV for config".
- `guide/cli-walkthrough.md` — update the Spin row in the
  `config push` section. Add a `config push --adapter spin --local`
  example that mirrors the Fastly one.
- `guide/cli-reference.md` — document `--seed-url` /
  `--seed-token` on `config push`.

## Stages

### Stage 1 — Spec promotion + tracking issue

- [ ] Move this plan into
      `docs/superpowers/specs/2026-06-01-spin-kv-config.md`.
- [ ] Open a tracking issue with the acceptance criteria
      (matches Task 2.5 + Stage 8 — wasm KV hostcalls aren't
      reachable under the CI wasm matrix's `wasmtime run`, so
      real KV coverage lives in the `spin up` smoke test): - host-side `config_store_contract_tests!` passes against
      the `InMemory` backend; - the wasm32-wasip2 contract test compiles + runs (no live
      KV hostcalls — those are runtime-bound); - collision check gone; - provision writes the right `key_value_stores`; - seed handler hits all status codes from D9's table; - `app-demo` works end-to-end under `spin up` with real
      KV writes via `config push --adapter spin --local`.

### Stage 2 — Runtime backend swap + registry rewrite

- [ ] **Task 2.1**: Rewrite `SpinConfigStore` per D1.
- [ ] **Task 2.2** (M4 fix): `InMemory` test backend is keyed
      plain `String → bytes::Bytes`. (One store per
      `config_store_contract_tests!` invocation — no need to track
      labels at this layer. The multi-store seed-handler test
      fixture `InMemorySeedWriter` IS the place that tracks
      `(label, key)`; see D10.) **v6**: `get` uses strict
      `String::from_utf8` (NOT `from_utf8_lossy`) to match the
      wasm backend's error path. New contract-test case
      `non_utf8_value_returns_unavailable` documents the
      behaviour and prevents future divergence.
- [ ] **Task 2.3**: Delete `translate_key_for_spin` and its callers
      inside `config_store.rs`.
- [ ] **Task 2.4** (H1 + M1): Rewrite `build_config_registry` in
      `request.rs` as **async**. Per declared id, await
      `SpinConfigStore::open(env.store_name("config", id))` so the
      `key_value::Store` handle is opened ONCE at dispatch setup
      and cached in `SpinConfigStore`. Thread `&env` to
      `dispatch_with_registries`'s config branch. Missing
      `key_value_stores = [...]` surfaces as a registry-build
      error, not a first-read error.
- [ ] **Task 2.5** (M1 update): `config_store_contract_tests!`
      against the `InMemory` backend on the **host** target. Real
      KV write/read coverage CANNOT live in the wasm contract test
      — CI runs that via plain `wasmtime run`, which does not host
      Spin's KV hostcalls. Real coverage moves to the Stage 8
      end-to-end smoke test (which requires `spin up`).

### Stage 3 — Seed handler + testable writer

- [ ] **Task 3.1** (D10 split): `crates/edgezero-adapter-spin/src/seed.rs`.
      Build the host-compilable core: `SeedWriter` trait,
      `InMemorySeedWriter`, `handle_seed_request_core(req: &Request,
…) -> Response` using `edgezero_core::http` types only. NO
      `spin_sdk` references in the core layer.
- [ ] **Task 3.2**: Host unit tests against `InMemorySeedWriter`
      covering every row of the D9 status code table PLUS the
      v6 short-token fail-closed cases (M1 fix). Required test
      rows: - Token unset (env var missing) → 401. - Token blank ("") → 401. - Token whitespace-only (" ") → 401. - Token 15 bytes (one under the floor) → 401, EVEN when
      the client offers the matching token on the wire. - Token exactly 16 bytes + matching wire token → 204. - Token 16 bytes + missing wire header → 401. - Token 16 bytes + wrong wire token → 403. - Non-POST method → 405. - `content-type` not `application/json` → 415. - Malformed JSON → 400. - Missing `store` / `entries` / non-string values → 400. - Unknown store (no env-resolved label matches) → 404. - `SeedWriter::write` errors mid-stream → 422. - Happy path → 204 + `InMemorySeedWriter` recorded all
      entries.
- [ ] **Task 3.3** (H3): Token comparison uses
      `subtle::ConstantTimeEq`. The `known_platform_labels` arg is
      computed by the caller (the wasm wrapper / lib.rs) from
      `A::stores().config` × `env.store_name("config", id)`.
- [ ] **Task 3.4** (D10 wrapper, wasm-gated): Thin
      `rust
pub(crate) async fn handle_seed_request_spin(
    req: spin_sdk::http::Request,
    writer: &SpinKvSeedWriter,
    valid_token: Option<&str>,
    known_platform_labels: &[String],
) -> anyhow::Result<SpinFullResponse>
`
      that translates Spin `Request` → `edgezero_core::http::Request`
      via `into_core_request` (uses `?`), calls the core handler,
      translates back via `from_core_response` (uses `?`). v10
      (round-8 H1): returns `anyhow::Result<SpinFullResponse>` so
      `run_app_with_seeder`'s seed branch is type-compatible with
      the fall-through `run_app::<A>` branch. NO `.expect()` panic
      in the request path.
- [ ] **Task 3.5** (M2 + v9 round-7 H2 + v10 round-8 H1 + v11
      round-9 M1): 1. Change `run_app`'s signature from
      `anyhow::Result<impl IntoResponse>` to
      `anyhow::Result<SpinFullResponse>` (concrete type already
      publicly aliased). **Source-compatible with the generated
      scaffold handler signature** (NOT a Spin-variable
      carve-out — this migration stays hard-cutoff):
      `SpinFullResponse: IntoResponse`, so the template
      `async fn handle(...) -> anyhow::Result<impl IntoResponse>`
      keeps compiling without re-scaffolding.
      1a. Drop `IntoResponse` from the
      `use spin_sdk::http::{...}` import in `src/lib.rs` — once
      `run_app` no longer returns `impl IntoResponse`, the
      import is unused and the wasm-clippy `-D warnings` gate
      fails on `unused_imports`. 2. Add `run_app_with_seeder` with the SAME return shape:
      `rust
   pub async fn run_app_with_seeder<A: Hooks>(req: SpinRequest)
       -> anyhow::Result<SpinFullResponse>
   `
      Routes `/__edgezero/config/seed` to
      `handle_seed_request_spin(req, &SpinKvSeedWriter, …).await`
      (returns `anyhow::Result<SpinFullResponse>` per Task 3.4)
      and falls through to `run_app::<A>(req).await`. Both
      arms produce `anyhow::Result<SpinFullResponse>` so the
      `if/else` typechecks and either result propagates via
      the outer `?` at the handler call site. 3. Scaffold template handler stays
      `async fn handle(req: Request) -> anyhow::Result<impl IntoResponse>`
      with the body swapped from
      `edgezero_adapter_spin::run_app::<App>(req).await` to
      `edgezero_adapter_spin::run_app_with_seeder::<App>(req).await`. 4. Token resolved from `EnvConfig::get(&["adapters", "spin",
   "seed_token"])`; if unset / blank / shorter than 16 bytes
      (D9), every request hitting the seed route returns 401
      (fail-closed).

### Stage 4 — CLI push rewrite

- [ ] **Task 4.1** (D8): Add `AdapterPushContext` to the trait
      (renamed from v4's `PushContext` to avoid colliding with
      the CLI's internal `PushContext`). Update all four existing
      impls to take it (no-ops for fastly/cloudflare/axum; spin
      reads from it).
- [ ] **Task 4.2**: Add `seed_url` / `seed_token` to
      `ConfigPushArgs`. Update the `push_args` test fixture and the
      `app-demo-cli/tests/config_flow.rs` helper.
- [ ] **Task 4.3**: Rewrite `load_push_context` to resolve the
      `ResolvedAdapterPushContext` (D3's disjoint prod/local
      chains per D8). `dispatch_push` converts to the
      borrow-shaped `AdapterPushContext<'_>` at call time.
- [ ] **Task 4.4** (D12): Implement spin `push_config_entries` via
      `reqwest::blocking::Client::post`. The CLI must resolve the
      body's `store` field to the **platform label** (via
      `env.store_name("config", id)`), per H3. JSON body per D9.
      Surface every status from D9's table — 400 / 401 / 403 /
      404 / 405 / 415 / 422 — per D12's match block. Handle
      connection-refused with a specific hint ("is the spin app
      running?").
- [ ] **Task 4.5**: Implement spin `push_config_entries_local`.
      Defaults `seed_url` to local. Otherwise delegates to the
      Task 4.4 impl.
- [ ] **Task 4.6**: `--dry-run` prints the planned URL + entries
      without POSTing. Tests for the dry-run shape.
- [ ] **Task 4.7** (v12 round-10 L1): **Delete and replace stale
      Spin-variable push tests.** Today's push tests in
      `examples/app-demo/crates/app-demo-cli/tests/config_flow.rs`
      (around line 257) and `crates/edgezero-adapter-spin/src/cli.rs`
      (around line 1846) assert: - dotted-key → underscore translation - `[variables].<key>` writes - `[component.<component>.variables].<key>` writes
      Under KV-backed push these assertions are wrong (variables
      table is no longer touched). Delete them; add coverage for
      the new contract: - Push body contains the resolved platform-label `store`
      (with and without `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=…`
      override). - Push body's `entries` array is the flattened typed
      `AppDemoConfig` minus `#[secret]` / `#[secret(store_ref)]`
      (mirrors the existing config-flow assertions, just on the
      body shape instead of the manifest edit). - `--dry-run` produces NO POST (verify via a mock seed
      endpoint that records hits). - Each D9 status code surfaces as the matching D12 error
      string (covers 400 / 401 / 403 / 404 / 405 / 415 / 422 + happy 204).

### Stage 5 — Provision + scaffold + manifest updates

- [ ] **Task 5.1**: Drop `[variables]` /
      `[component.<id>.variables]` config-key writes from spin's
      `provision`.
- [ ] **Task 5.2**: For each `[stores.config].id`, append the
      platform name to the component's `key_value_stores = [...]`.
      Idempotent. New `provision_writes_config_kv_store_entry`
      test.
- [ ] **Task 5.3**: `single_store_kinds` returns `&["secrets"]`.
- [ ] **Task 5.4** (M1): Generator `spin.toml.hbs` declares
      `key_value_stores = ["app_config"]` by default. Add a test
      in `generated_project_builds.rs` that checks the rendered
      spin.toml contains the entry.
- [ ] **Task 5.5** (v12 round-10 L1): **Delete stale
      provision-side variable-write assertions** that pair with
      the Stage 4.7 deletions. Concrete sites in
      `crates/edgezero-adapter-spin/src/cli.rs` (around line 1846)
      currently assert the provision step emits `[variables]` /
      `[component.<id>.variables]` blocks for declared config
      ids. Under D4 those writes are gone. Replace with assertions
      that: - For each `[stores.config].id`, the platform label appears
      in the component's `key_value_stores = [...]` (Task 5.2's
      change). - `[variables]` / `[component.<id>.variables]` are NOT
      touched for config ids (regression guard so a future
      change doesn't silently revive the old path). - Existing `[variables]` entries for `#[secret]` fields
      (Task 6.2 keeps these) are preserved.

### Stage 6 — Validator changes

- [ ] **Task 6.1** (H3): Delete uppercase/dash/leading-digit tests
      on `validate_app_config_keys`. Replace with
      `validate_app_config_keys_accepts_any_utf8`.
- [ ] **Task 6.2**: Delete `validate_typed_secrets`'s
      collision-check block per D6. Keep the secret-name format
      check (it still validates `#[secret]` values against Spin
      variable rules).
- [ ] **Task 6.3**: Update strict-completeness tests:
      `[stores.config].ids.len() > 1` now PASSES for spin.

### Stage 7 — Docs + app-demo migration

- [ ] **Task 7.1**: Rewrite `docs/guide/adapters/spin.md` config
      section. Add seed-handler section with the D9 security table.
- [ ] **Task 7.2**: Add the migration section to
      `docs/guide/manifest-store-migration.md`.
- [ ] **Task 7.3**: Update `docs/guide/cli-walkthrough.md` Spin row + add `--adapter spin --local` example.
- [ ] **Task 7.4**: Update `docs/guide/cli-reference.md` for
      `--seed-url` / `--seed-token`.
- [ ] **Task 7.5**: app-demo migration in ONE commit (per
      resolved Q5): switch entrypoint to `run_app_with_seeder`,
      update `spin.toml`, set `seed_url` in `edgezero.toml`.

### Stage 8 — Verify gate

- [ ] Full gate: cargo fmt, host clippy --workspace, workspace
      tests, all three adapter wasm-clippy gates, docs
      lint/format/build.
- [ ] Spin wasm contract test under wasmtime (wasm32-wasip2).
- [ ] **Wasm dep gating checks** (D11, fixed per L1 — use
      `cargo tree -i` which errors when the dep is absent).
      ``sh
  # reqwest MUST NOT leak into the wasm tree. `cargo tree -i`
  # errors when reqwest isn't a dep; invert with `!`:
  ! cargo tree -i reqwest -p edgezero-adapter-spin \
   --features spin --target wasm32-wasip2 2>/dev/null
  # subtle / serde_json MUST be in the wasm tree.
  cargo tree -i subtle -p edgezero-adapter-spin \
   --features spin --target wasm32-wasip2
  cargo tree -i serde_json -p edgezero-adapter-spin \
   --features spin --target wasm32-wasip2
  ``
- [ ] **End-to-end smoke test** in `examples/app-demo` (v11
      round-9 L1: shell-form, backgrounded, port-wait + trap
      cleanup so the test can actually be run in CI / pasted
      into a shell).

      ```sh
      #!/usr/bin/env bash
      set -euo pipefail

      readonly TOKEN="test-token-1234567890"
      readonly PORT=3000
      readonly URL="http://127.0.0.1:${PORT}"
      export EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN="$TOKEN"

      cd examples/app-demo

      # 1. Build the wasm so `spin up` has something to serve.
      (cd crates/app-demo-adapter-spin && \
        cargo build --target wasm32-wasip2 --release \
          -p app-demo-adapter-spin)

      # 2. Background `spin up` and arrange to kill it on exit.
      (cd crates/app-demo-adapter-spin && spin up --listen "127.0.0.1:${PORT}") \
          &> /tmp/edgezero-spin-smoke.log &
      readonly SPIN_PID=$!
      trap 'kill $SPIN_PID 2>/dev/null || true; wait $SPIN_PID 2>/dev/null || true' \
          EXIT INT TERM

      # 3. Wait up to 10s for the listener (Spin warm-up + KV
      #    backend init). 20 × 0.5s = 10s. Fail clean on timeout.
      for _ in $(seq 1 20); do
          if curl --silent --fail --max-time 1 "${URL}/" \
                  > /dev/null 2>&1; then
              break
          fi
          sleep 0.5
      done
      if ! curl --silent --fail --max-time 1 "${URL}/" \
              > /dev/null 2>&1; then
          echo "spin up did not bind ${URL} within 10s" >&2
          tail -n 100 /tmp/edgezero-spin-smoke.log >&2
          exit 1
      fi

      # 4. Push config to the LOCAL endpoint. The token env var
      #    is inherited from the parent shell (line 5).
      cargo run -p app-demo-cli --quiet -- \
          config push --adapter spin --local

      # 5. Assert the pushed value flows through to the handler.
      readonly GOT="$(curl --silent --fail "${URL}/config/greeting")"
      readonly WANT="hello from app-demo"
      if [[ "$GOT" != "$WANT" ]]; then
          echo "smoke test FAILED: got=${GOT@Q} want=${WANT@Q}" >&2
          exit 1
      fi
      echo "smoke test PASSED: GET /config/greeting → ${GOT@Q}"
      # trap kills SPIN_PID on exit.
      ```

      The token value (`test-token-1234567890`, 21 bytes) clears
      the v6 16-byte floor on BOTH sides (server `spin up`
      inherits the var; CLI `config push` inherits the var).
      The `trap` ensures no orphan `spin up` lingers on port 3000
      if the assertion fails — important for re-runnability.

## Open questions

None outstanding. All round-2/3/5 questions are settled. See the
"Settled" section below for the historical decisions.

## Settled

- **Q1 (round 2) → YES**: `[adapters.spin.commands].seed_url` IS a
  valid source (third in the resolution order after CLI flag and
  env). `seed_token` stays env/CLI only — never manifest.
- **Q2 (round 5) → YES, 16-byte floor**: The seed handler rejects
  tokens shorter than 16 bytes at startup with a fail-closed 401
  on every request. See D9 "Fail-closed contract" for rationale.
- **Q3 (round 2) → ONE COMMIT**: Stage 7.5 ships
  `run_app_with_seeder` switch + `spin.toml` KV declaration +
  `edgezero.toml` seed_url together for atomic reversibility.

## Estimated scope (v4)

- **Code**: 14 files modified, 1 new (`seed.rs`), ~820 LoC impl
  - ~430 LoC tests. (Up from v3 — D1's cfg-gated backend enum,
    the H4 disjoint local resolution chain in `dispatch_push`, and
    the extra D12 status-code arms add ~70 LoC; H2/H3 non-optional
    dep moves are zero-LoC on the runtime side.)
- **Docs**: 4 files modified, ~100 LoC prose.
- **Migration**: hard-cutoff (resolved per L1).
- **Time**: 2 focused days assuming no surprises in the spin
  hostcall surface.

## Risks (v2 additions)

- **`PushContext` is a breaking trait change for any out-of-tree
  adapter**. Document in release notes; no in-tree adapter outside
  the four ships today.
- **`reqwest` adds ~3 MB to the host CLI binary**. Acceptable for
  a dev tool; flag if it ever becomes a problem.
- **Token enforcement in CI**: the end-to-end smoke test needs the
  `EDGEZERO__ADAPTERS__SPIN__SEED_TOKEN` env var to flow into both
  `spin up` and `app-demo-cli`. Test harness sets it once.
