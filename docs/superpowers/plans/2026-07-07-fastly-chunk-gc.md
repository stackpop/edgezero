# Fastly Chunk GC — Implementation Plan (as built)

**Authoritative contract:** `docs/superpowers/specs/2026-07-07-fastly-chunk-gc.md`.
This plan is the task decomposition that produced the shipped design and is kept
in sync with it. Tasks are checked because the work is complete; the value here
is the rationale and the map from task → code/tests.

**Goal:** Reclaim the chunk entries a Fastly config store leaks when an oversized
app-config envelope is re-pushed — **without ever deleting a chunk a live
pointer still references, and without any unsafe automatic cloud deletion.**

**Architecture (two very different halves):**

- **Local** (`fastly.toml`) prunes eagerly, inside the same `DocumentMut`
  rewrite. Safe because it is one file Viceroy reads at startup — no propagation
  window, no POPs.
- **Cloud** (remote config store) is eventually consistent, records no
  pointer-supersession time, and offers no compare-and-swap. There is **no safe
  automatic delete**. So a cloud `config push` reclaims **nothing**, and
  reclamation is a separate, **operator-invoked** `config gc` that derives the
  live set from the store and deletes only unreferenced chunks the operator has
  explicitly asserted (via `--older-than`) are old enough.

**Tech Stack:** Rust 2024, `serde_json`, `toml_edit`, `chrono` (already a
dependency), existing Fastly CLI shell-out helpers, handlebars-rendered
fake-`fastly` test shims, `cargo test -p edgezero-adapter-fastly --features cli`.

---

## Design history (why cloud reclamation is operator-invoked)

Three *automatic* cloud designs were built and each demolished in review. They
are recorded here and in the spec so a fourth is not attempted.

1. **Eager delete** after the commit, guarded by a control-plane read-back —
   **unsafe.** The read-back sees only the control plane; POPs may still serve
   the previous pointer, which references the chunks being deleted. Breaks reads
   on *every* re-push.
2. **Metadata sidecar** (record the superseded generation; reclaim it later) —
   **unsound.** No CAS ⇒ a failed write / failed read-back / concurrent push
   permanently loses a generation; and the record overflows the 8 000-char entry
   limit at ~71 generations.
3. **Store-derived clock** `superseded_at(G) = created_at(successor(G))` —
   **unsound.** Chunk creation is not a pointer transition. Counterexample:
   chunked A → direct B → direct C; during C, A has no chunk "successor" and ages
   from its own creation, so it is deleted though B superseded it seconds ago.

The fact needed to delete safely — *the pointer that referenced this chunk
stopped being served everywhere, ≥ the propagation window ago* — is not
recorded by Fastly and cannot be safely synthesised. It **is** known to the
operator. So `config gc --older-than <dur>` takes that assertion.

Verified against the live Fastly API (read-only): `config-store-entry list
--json` returns `item_key` + `created_at` + `item_value`; the root entry's
`updated_at` is **not** bumped by `update --upsert` (a root reading `updated_at
= 2026-07-07` pointed at chunks created `2026-07-13`), which is why `updated_at`
cannot be used as a supersession clock.

---

## File map

| File | What it holds |
| --- | --- |
| `crates/edgezero-adapter-fastly/src/chunked_config.rs` | `prior_chunk_keys` (validated, canonical, prefix-scoped), `validate_pointer_chunks` (chunk list must be internally consistent), `gc_classify_root` (fail-closed GC classifier), `gc_reject_root_like_chunk` (never delete a root), `redact_json_err`, `chunk_key_parts` / `chunk_key_generation` (canonical-only, index must fit `usize`), unit tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (push) | `reject_reserved_root_keys`, `reject_duplicate_root_keys`, `expand_root`; `write_fastly_local_config_store` (local eager prune); a cloud push writes only |
| `crates/edgezero-adapter-fastly/src/cli.rs` (config gc) | `gc_config_entries` → `gc_fastly_config_store` (rejects a zero window at the destructive boundary) → `plan_gc_reclamation`; `list_config_store_entries` (bare-array-only, non-empty fields, no duplicate keys), `chunk_key_generation_any`, `parse_rfc3339_secs`, `unix_now_secs`, `delete_config_store_entry` (`--key --auto-yes`, never `--all`; every failure is a failure); `redact_describe_response` / `redact_stderr` |
| `crates/edgezero-adapter/src/registry.rs` | `Adapter::gc_config_entries` trait method (default `Err`) |
| `crates/edgezero-cli/src/{args,config}.rs` | `ConfigGcArgs`, `run_config_gc` |
| `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs` + `examples/app-demo/.../app-demo-cli/src/main.rs` | wire the `Gc` subcommand |

---

### Task 1 — Pointer inspection + canonical key validation ✅

`crates/edgezero-adapter-fastly/src/chunked_config.rs`.

- [x] `prior_chunk_keys(root_key, raw) -> Result<Vec<String>, String>`:
  `serde_json::Value`-first (so a pointer-kind value with missing fields WARNS,
  not silently `Ok([])`); `Ok(vec![])` for direct/absent/non-pointer; `Err` for
  pointer-kind-but-invalid. Every referenced key must pass `chunk_key_generation`
  (canonical) or it is a hard error.
- [x] `chunk_key_generation(root_key, key) -> Option<String>`: recognises **only**
  `<root>.__edgezero_chunks.<64-char-lowercase-sha>.<canonical-decimal-index>` and
  returns the SHA. Rejects short/uppercase/non-hex SHAs, leading-zero and
  non-numeric indices, empty roots. This is the destructive-delete gate, so it
  matches exactly what `prepare_fastly_config_entries` emits and nothing else.
- [x] Tests: valid/direct/garbage/wrong-kind/bad-version/foreign-prefix for
  `prior_chunk_keys`; `chunk_key_generation_requires_canonical_shape`.

### Task 2 — Push-time key hygiene + payload redaction ✅

- [x] `reject_reserved_root_keys` — a logical key containing `.__edgezero_chunks.`
  is a hard error at the adapter boundary (it would collide with the chunk
  namespace). Called by both push paths before any I/O.
- [x] `reject_duplicate_root_keys` — a batch naming the same logical root twice is
  a hard error: GC builds one plan per entry against the same prior, so a
  duplicate would reclaim the chunks the last tuple just installed.
- [x] `redact_describe_response` / `redact_stderr` — every `describe`/`list`
  diagnostic reports only a response's size + top-level field-name shape (or
  redacted stderr), never a stored value. App config may hold credentials and CLI
  status lines are logged verbatim. Sentinel-secret tests cover the read path and
  the `config gc` list path.

### Task 3 — Local eager prune ✅

- [x] `write_fastly_local_config_store(path, platform_name, entries, gc_roots)`:
  takes exact per-root keep-sets (`gc_roots`), snapshots each root's prior value
  before the upsert, and prunes `prior_chunk_keys − new_keys` in the **same**
  in-memory rewrite before the single `fs::write`. A suspicious prior pointer
  warns and prunes nothing. Sibling roots are untouched (canonical, root-scoped
  keys).
- [x] `push_config_entries_local` threads per-root expansion, rejects reserved +
  duplicate keys, and reports best-effort dry-run orphan counts (degrading to
  `unknown` on malformed prior state, never newly failing the dry-run).
- [x] Tests: shrink-to-direct prune, suspicious-pointer skip, sibling-chunk
  preservation, dry-run counts (exact / identical-repush-zero / non-table-unknown
  / suspicious-unknown).

### Task 4 — Cloud push reclaims nothing ✅

- [x] `push_config_entries` rejects reserved + duplicate keys, expands per root,
  resolves the store id, and commits (chunks first, pointer last). **No delete,
  no read-back, no metadata.** A carried NOTE comment explains why (eventual
  consistency + no CAS). Cloud storage accretes orphans exactly as before — not a
  regression.

### Task 5 — `config gc` (operator-invoked reclamation) ✅

`Adapter::gc_config_entries` (trait, default `Err`) → `gc_fastly_config_store`
→ `plan_gc_reclamation` (which owns every safety guard).

- [x] `list_config_store_entries` — one `config-store-entry list --json`, parsed
  **fail-closed**: the payload must be a BARE ARRAY (an `{"items":[...]}` envelope
  may carry pagination we don't follow), and every entry must carry a NON-EMPTY
  string `item_key`/`item_value`/`created_at`, else abort. A skipped, defaulted,
  or empty field would hide a root and get its live chunks deleted.
- [x] Live set — classify non-chunk entries as roots via `gc_classify_root`, which
  accepts ONLY a valid direct envelope or a valid v1 pointer; union of referenced
  canonical keys = live. Deliberately NOT the push-path `prior_chunk_keys`, which
  answers `Ok([])` for a non-pointer value — on a destructive path an empty or
  truncated pointer must fail, not read as "references nothing" and orphan its own
  live chunks.
- [x] **Completeness guard** — every live-referenced key must appear in the
  listing, else fail closed (guards against a paginated/incomplete list, where an
  unseen root could reference a chunk we'd wrongly delete).
- [x] **Live-generation-age guard (best-effort defence in depth — NOT an
  independent safety proof)** — per root, the newest `created_at` among its *live*
  chunks approximates when its current config went live, a real lower bound on
  when that root's orphans were superseded *when the live value is chunked*. It
  catches the design-3 counterexample. It is blind when the live value is direct
  (no signal) or when a re-push reuses a content-address (no new chunk). So it is
  applied only as an ADDITIONAL restriction on top of the chunk's own age — both
  must clear `--older-than`, more restrictive wins — and the operator's
  `--older-than` assertion remains the only sound basis for any delete.
- [x] Delete each candidate with `--key --auto-yes` (never `--all`).
  **Continue-then-fail**: attempt every delete, then return a **non-zero** error
  naming any that failed, so automation detects partial failure.
- [x] `plan_gc_reclamation` extracted from `gc_fastly_config_store` so the guards
  read as one unit (no `#[expect(too_many_lines)]`).
- [x] Tests: never-delete-live, reclaim-aged-orphan, **protect recently-superseded
  gen with old chunks**, **retain young orphan under a long-stable root** (both
  ages), fail-closed on truncated-root-pointer / empty-root-value /
  enveloped-(paginated)-listing / unclassifiable-root / malformed-listing-entry /
  unreadable-timestamp, non-canonical-not-deleted (fails closed), delete argv
  shape, narrow absent-key match (store/auth/500 must surface), delete-failure
  non-zero exit, list-path redaction.
- [x] CLI gating tests: `--yes` requires `--older-than`; `--yes --older-than 0`
  rejected; dry-run allows a missing threshold. Scaffold test asserts a generated
  project wires `Gc(ConfigGcArgs)` → `edgezero_cli::run_config_gc` (verified live
  by mutating the template).

### Task 6 — CLI wiring ✅

`crates/edgezero-cli`.

- [x] `ConfigGcArgs { adapter, manifest, no_env, older_than: Option<String>,
  store, yes }`. `run_config_gc`: resolve adapter + store from `edgezero.toml`
  only (gc inspects the store, not the typed app-config).
- [x] `--older-than` is **REQUIRED for `--yes`** (a destructive run must not guess
  the operator's assertion); a dry-run without it previews **every** orphan and
  age (threshold 0) so the operator can choose one. Dry-run by default; `--yes`
  deletes.
- [x] The generated CLI template **and** app-demo wire the untyped `Gc`
  subcommand → `edgezero_cli::run_config_gc`.
- [x] Tests: `config_gc_yes_requires_explicit_older_than`,
  `config_gc_dry_run_allows_missing_older_than`.

### Task 7 — Verification ✅

- [x] `cargo test --workspace --all-targets` (20 suites), `cargo test -p
  edgezero-adapter-fastly --features cli` (132), the CLI gc gating tests.
- [x] `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` (no per-site `#[expect]` for length).
- [x] `cargo check --workspace --all-targets --features "fastly cloudflare spin"`;
  `cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin`;
  app-demo CLI builds.
- [x] **`cargo clippy -p edgezero-adapter-fastly --target wasm32-wasip1 --features
  fastly --all-targets -- -D warnings`.** Round 6 caught a real blocker here that
  the host-target gates cannot see: a helper `#[cfg(any(feature = "cli", test))]`
  but used only from `cli.rs` is DEAD in a test build without `cli`. Any new
  `cli`-only helper needs a unit test in its own module, or a `cli`-only gate.
  Run this gate before claiming the suite is green.

---

## Notes for implementers / reviewers

- The cloud path is **operator-gated, not automatic** — review it as a new
  design, not a diff. The contentious surface is `plan_gc_reclamation`'s
  live-generation-age clock and the `--older-than` + no-concurrent-push operator
  contract (Fastly has no CAS, so `config gc` must not run alongside a push to
  the same store).
- Every destructive decision is **fail-closed**: unreadable listing, missing
  field, unclassifiable root, unreadable timestamp, non-canonical key, or an
  incomplete listing all abort with nothing deleted.
- Local pruning and the safety plumbing (redaction, canonical validation,
  reserved/duplicate rejection) are independent of the cloud saga and have passed
  every review; the PR can be split to land them without `config gc` if desired.
