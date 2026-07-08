# Fastly Chunk GC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reclaim obsolete Fastly chunked config-store entries after a successful re-push without changing push success semantics.

**Architecture:** Add a root-scoped prior-pointer inspector in `chunked_config.rs`, then wire cloud and local Fastly push paths in `cli.rs` to snapshot prior chunk keys before writing and sweep them only after the new root entry is committed. Cloud GC is best-effort and warning-only; local GC happens inside the same `DocumentMut` rewrite used for `fastly.toml`.

**Tech Stack:** Rust 2021, `serde_json`, `toml_edit`, existing Fastly CLI shell-out helpers, existing fake-`fastly` test shims, `cargo test -p edgezero-adapter-fastly --features cli`.

**Precondition (gate):** Cloud GC is **single-writer, best-effort** — it is safe only when pushes to a given config store are serialized. The post-commit read-back guard narrows but does not close the concurrent-push window (Fastly has no compare-and-delete). Do NOT start this plan unless the team accepts that concurrent cloud pushes are unsupported for GC purposes. If strict concurrent-push safety is required, stop and redesign around an offline, lease-guarded `config gc` (the deferred non-goal) instead. See the spec's "Concurrency model".

---

## File Structure

- Modify `crates/edgezero-adapter-fastly/src/chunked_config.rs`
  - Add `prior_chunk_keys(root_key, raw) -> Result<Vec<String>, String>` next to `FastlyChunkPointer`.
  - Add focused unit tests in the existing `#[cfg(test)]` module.

- Modify `crates/edgezero-adapter-fastly/src/cli.rs`
  - Import `prior_chunk_keys` and `CHUNK_KEY_INFIX`.
  - Add small private GC helpers (`FastlyConfigGcPlan`, `expand_root`, `orphan_chunk_keys`, `reject_reserved_root_keys`) near the config-push helpers. Per-root keep-sets come from `expand_root` (that root's own expansion), never prefix-scanned from the flattened physical set.
  - Extend `push_config_entries` with offline dry-run intent, prior root read, post-commit cloud sweep (only after the whole flattened commit returns `Ok`), and warning status lines.
  - Add `delete_config_store_entry(store_id, key)`.
  - Extend `push_config_entries_local` dry-run with best-effort offline orphan counts (degrade to `unknown`, never newly fail).
  - Extend `write_fastly_local_config_store` with a `gc_roots: &[(String, HashSet<String>)]` param (exact per-root keep-sets; empty ⇒ no GC); prune prior root chunks in the same in-memory rewrite before the (non-atomic) `fs::write` and return warning lines from suspicious prior pointers.
  - Add/adjust tests in the existing `cli.rs` test module.

**Mandatory** reserved-key rejection: both `push_config_entries` and
`push_config_entries_local` call `reject_reserved_root_keys(entries)?`
immediately after the empty-entries check. A logical key containing
`.__edgezero_chunks.` is rejected with an error (it collides with the
generated chunk namespace). This lives at the Fastly adapter boundary —
not in generic `edgezero-cli` — because the infix is a Fastly concept.
It is a hard error, not a warning: there is no valid reason for such a
key, and allowing it would let a push write into another key's chunk
namespace.

---

### Task 1: Add Root-Scoped Prior Pointer Inspection

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/chunked_config.rs:47`
- Test: `crates/edgezero-adapter-fastly/src/chunked_config.rs:309`

- [ ] **Step 1: Add failing tests for `prior_chunk_keys`**

Add tests near the existing chunked-config unit tests:

```rust
#[test]
fn prior_chunk_keys_returns_valid_v1_pointer_keys() {
    let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
    let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
    let (_, pointer_json) = entries.last().unwrap();

    let keys = prior_chunk_keys("app_config", pointer_json).expect("valid pointer");

    let expected: Vec<String> = entries[..entries.len().saturating_sub(1)]
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    assert_eq!(keys, expected);
}

#[test]
fn prior_chunk_keys_returns_empty_for_direct_envelope() {
    let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT);
    assert_eq!(prior_chunk_keys("app_config", &envelope).unwrap(), Vec::<String>::new());
}

#[test]
fn prior_chunk_keys_returns_empty_for_unrelated_json() {
    assert_eq!(
        prior_chunk_keys("app_config", r#"{"hello":"world"}"#).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn prior_chunk_keys_returns_empty_for_wrong_kind() {
    let raw = r#"{"edgezero_kind":"other","version":1,"chunks":[],"data_sha256":"","envelope_len":0,"envelope_sha256":""}"#;
    assert_eq!(prior_chunk_keys("app_config", raw).unwrap(), Vec::<String>::new());
}

#[test]
fn prior_chunk_keys_rejects_unsupported_pointer_version() {
    let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
    let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
    let (_, pointer_json) = entries.last().unwrap();
    let mut pointer: FastlyChunkPointer = serde_json::from_str(pointer_json).unwrap();
    pointer.version = 2;
    let raw = serde_json::to_string(&pointer).unwrap();

    let err = prior_chunk_keys("app_config", &raw).expect_err("version 2 should warn");
    assert!(err.contains("unsupported version"), "{err}");
}

#[test]
fn prior_chunk_keys_rejects_foreign_chunk_prefix() {
    let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
    let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
    let (_, pointer_json) = entries.last().unwrap();
    let mut pointer: FastlyChunkPointer = serde_json::from_str(pointer_json).unwrap();
    pointer.chunks[0].key = pointer.chunks[0].key.replacen("app_config", "app_config_staging", 1);
    let raw = serde_json::to_string(&pointer).unwrap();

    let err = prior_chunk_keys("app_config", &raw).expect_err("foreign chunk should warn");
    assert!(err.contains("outside"), "{err}");
}

// Regression for the Value-first rule: a value that IS pointer-kind but
// is missing required fields (`chunks`, `data_sha256`, …) must WARN, not
// be silently dropped by a failed struct deserialize.
#[test]
fn prior_chunk_keys_warns_on_pointer_kind_with_missing_fields() {
    let raw = r#"{"edgezero_kind":"fastly_config_chunks","version":2}"#;
    let err = prior_chunk_keys("app_config", raw)
        .expect_err("pointer-kind but malformed must warn, not Ok([])");
    assert!(!err.is_empty(), "{err}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli prior_chunk_keys
```

Expected: fail because `prior_chunk_keys` is not defined.

- [ ] **Step 3: Implement `prior_chunk_keys`**

Add near the other public helpers:

Parse as `serde_json::Value` **first** so a pointer-kind value with
missing/invalid fields reaches the warning path (spec: "Rules — parse as
`serde_json::Value` first"). Deserializing straight into
`FastlyChunkPointer` would make `{"edgezero_kind":"...","version":2}`
(no `chunks`) fail parsing and be silently returned as `Ok([])`.

```rust
#[cfg(any(feature = "cli", test))]
pub(crate) fn prior_chunk_keys(root_key: &str, raw: &str) -> Result<Vec<String>, String> {
    // 1. Parse loosely. Not-JSON, or not our pointer kind => silent.
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };
    if value.get("edgezero_kind").and_then(serde_json::Value::as_str) != Some(POINTER_KIND) {
        // Direct BlobEnvelope, unrelated JSON, or first push.
        return Ok(Vec::new());
    }

    // 2. It IS pointer-kind: from here every failure WARNS (Err), never silent.
    let pointer: FastlyChunkPointer = serde_json::from_value(value).map_err(|err| {
        format!("prior chunk pointer at `{root_key}` is malformed: {err}; skipping chunk GC")
    })?;
    if pointer.version != 1 {
        return Err(format!(
            "prior chunk pointer at `{root_key}` has unsupported version {}; skipping chunk GC",
            pointer.version
        ));
    }

    // 3. Prefix-scope every referenced key to this root's own namespace.
    let prefix = format!("{root_key}{CHUNK_KEY_INFIX}");
    let mut keys = Vec::with_capacity(pointer.chunks.len());
    for chunk_ref in pointer.chunks {
        if !chunk_ref.key.starts_with(&prefix) {
            return Err(format!(
                "prior chunk pointer at `{root_key}` references chunk key `{}` outside expected prefix `{prefix}`; skipping chunk GC",
                chunk_ref.key
            ));
        }
        keys.push(chunk_ref.key);
    }
    Ok(keys)
}
```

- [ ] **Step 4: Run scoped tests**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli prior_chunk_keys
```

Expected: all `prior_chunk_keys` tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/chunked_config.rs
git commit -m "test(fastly): cover prior chunk key extraction"
```

---

### Task 2: Add Shared GC Metadata Helpers in Fastly CLI

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:8`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:1004`
- Test: compile through later tasks

- [ ] **Step 1: Update imports**

Change the Fastly chunked config import:

```rust
use crate::chunked_config::{
    prior_chunk_keys, prepare_fastly_config_entries, resolve_fastly_config_value, CHUNK_KEY_INFIX,
};
```

- [ ] **Step 2: Add small metadata structs/helpers near config-push helpers**

Add before `push_entries_with_committer`. There is **no** prefix-scan
helper: `--key` is free-form (`args.rs:311`), so `new_keys` must NOT be
derived by string-matching flattened physical entries (that reintroduces
inference). Instead, expand each logical root **on its own** and take
that root's exact keys.

```rust
use std::collections::HashSet;

#[derive(Debug)]
struct FastlyConfigGcPlan {
    root_key: String,
    prior_keys: Result<Vec<String>, String>,
    new_keys: HashSet<String>,
    /// Exactly the value this push wrote at `root_key` (the root pointer
    /// for a chunked push, or the direct envelope). Cloud uses it as a
    /// read-back concurrency guard before deleting; local ignores it.
    new_root_value: String,
}

/// Expand ONE logical (root_key, body) into its physical entries, the
/// exact keep-set for that root, and the value written at the root key.
/// No cross-root prefix scanning.
fn expand_root(root_key: &str, body: &str) -> Result<(Vec<(String, String)>, HashSet<String>, String), String> {
    let expanded = prepare_fastly_config_entries(root_key, body)?;
    let new_keys: HashSet<String> = expanded.iter().map(|(k, _)| k.clone()).collect();
    // prepare_* always emits the root entry LAST (root pointer or direct value).
    let new_root_value = expanded.last().map(|(_, v)| v.clone()).unwrap_or_default();
    Ok((expanded, new_keys, new_root_value))
}

fn orphan_chunk_keys(plan: &FastlyConfigGcPlan) -> Result<Vec<String>, String> {
    match &plan.prior_keys {
        Ok(prior) => Ok(prior
            .iter()
            .filter(|key| !plan.new_keys.contains(*key))
            .cloned()
            .collect()),
        Err(err) => Err(err.clone()),
    }
}

/// Enforced at the adapter boundary because `--key` is free-form: a key
/// containing the reserved chunk infix would collide with chunk storage.
fn reject_reserved_root_keys(entries: &[(String, String)]) -> Result<(), String> {
    for (key, _) in entries {
        if key.contains(CHUNK_KEY_INFIX) {
            return Err(format!(
                "config key `{key}` contains the reserved infix `{CHUNK_KEY_INFIX}`, which collides with Fastly chunk storage; choose a different --key"
            ));
        }
    }
    Ok(())
}
```

Both `push_config_entries` and `push_config_entries_local` MUST call
`reject_reserved_root_keys(entries)?` immediately after the empty-entries
check, before any expansion or I/O. Keep helpers private.

- [ ] **Step 3: Run compile check**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli --no-run
```

Expected: compile succeeds.

- [ ] **Step 4: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "refactor(fastly): add chunk gc planning helpers"
```

---

### Task 3: Implement Local Fastly TOML GC

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:421`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:926`
- Test: `crates/edgezero-adapter-fastly/src/cli.rs:2991`
- Test: `crates/edgezero-adapter-fastly/src/cli.rs:3296`

- [ ] **Step 1: Rewrite the second oversized local push test to fail**

In `second_oversized_push_converges_runtime_on_new_envelope`, invert the old-chunk assertion:

```rust
for chunk_key in &chunks_a {
    assert!(
        !chunks_b.contains(chunk_key),
        "old A-chunk `{chunk_key}` must be pruned after push B; B-set={chunks_b:?}"
    );
}
```

Update stale comments that say old chunks remain or no GC exists.

- [ ] **Step 2: Add shrink-to-direct local test**

Add a test near the local push tests:

```rust
#[cfg(unix)]
#[test]
fn push_config_entries_local_prunes_prior_chunks_when_value_shrinks_to_direct() {
    use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
    let dir = tempdir().expect("tempdir");
    let fastly_toml = dir.path().join("fastly.toml");
    fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

    let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
    FastlyCliAdapter
        .push_config_entries_local(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            &[(TEST_CONFIG_ID.to_owned(), chunked)],
            &AdapterPushContext::new(),
            false,
        )
        .expect("first push");

    let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
    FastlyCliAdapter
        .push_config_entries_local(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
            &AdapterPushContext::new(),
            false,
        )
        .expect("second push");

    let after = fs::read_to_string(&fastly_toml).expect("read");
    let doc: toml_edit::DocumentMut = after.parse().expect("parse");
    let contents = doc
        .get("local_server")
        .and_then(|ls| ls.get("config_stores"))
        .and_then(|cs| cs.get(TEST_CONFIG_ID))
        .and_then(|st| st.get("contents"))
        .and_then(toml_edit::Item::as_table)
        .expect("contents");

    assert_eq!(contents.get(TEST_CONFIG_ID).and_then(toml_edit::Item::as_str), Some(direct.as_str()));
    assert!(
        !contents.iter().any(|(key, _)| key.contains(CHUNK_KEY_INFIX)),
        "prior chunks must be removed: {after}"
    );
}
```

- [ ] **Step 3: Extend local dry-run test to expect orphan count**

Seed a prior chunked push before dry-run, keep `original`, then assert:

```rust
assert!(combined.contains("would delete"));
assert!(combined.contains("orphan chunks"));
```

Also keep `assert_eq!(after, original)`.

Add a second dry-run test for the identical-bytes re-push (regression
for MEDIUM-A / the expand-`new_keys` rule): seed a chunked config, then
dry-run the SAME bytes and assert the count is `0`:

```rust
assert!(combined.contains("would delete 0 orphan chunks"));
```

Add a third dry-run test for a suspicious prior pointer: seed the root
with a pointer-kind-but-invalid value (e.g. `version: 2`), dry-run a new
config, and assert the degraded line plus no write:

```rust
assert!(combined.contains("unknown: suspicious prior pointer"));
// and the file is unchanged
```

- [ ] **Step 3c: Add local suspicious-pointer real-push test (spec §"local")**

Seed `fastly.toml` so the root holds a pointer-kind-but-invalid value
(e.g. `{"edgezero_kind":"fastly_config_chunks","version":2}`), then run a
REAL (non-dry-run) local push of a new config. Assert:
- the returned status `Vec<String>` contains a suspicious-pointer warning,
- no chunk keys were removed as a side effect (the writer must not delete
  when `prior_chunk_keys` returns `Err`),
- the new value is written to the root key.

- [ ] **Step 3d: Add reserved-key rejection test (local)**

Assert `push_config_entries_local` with a logical key containing
`.__edgezero_chunks.` returns `Err` before touching `fastly.toml` (file
unchanged / not created).

- [ ] **Step 4: Run local tests to verify failures**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_local -- --nocapture
cargo test -p edgezero-adapter-fastly --features cli second_oversized_push_converges_runtime_on_new_envelope -- --nocapture
```

Expected: failures showing old chunks remain and dry-run lacks GC count.

- [ ] **Step 5: Implement local in-memory pruning**

Change `write_fastly_local_config_store` to (a) accept the exact per-root
keep-sets via a new `gc_roots: &[(String, HashSet<String>)]` param
(each `(root_key, new_keys)` from `expand_root` — NOT prefix-scanned) and
(b) return warning lines, while preserving its error behavior:

```rust
fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
    gc_roots: &[(String, std::collections::HashSet<String>)],
) -> Result<Vec<String>, String>
```

Update ALL existing call sites to pass `gc_roots`. There are **10** (line
numbers approximate — grep `write_fastly_local_config_store` to confirm):

| Site | Kind | `gc_roots` to pass |
| --- | --- | --- |
| `cli.rs:483` | prod (`push_config_entries_local`) | `&gc_roots` built via `expand_root` (see caller snippet below) |
| `cli.rs:1838` | test — minimal-file block | `&[]` (setup-only; no prior generation to prune) |
| `cli.rs:1867`, `:1873` | test — replaces-block-on-re-push | `&[]` (both writes; asserts value replacement, not GC) |
| `cli.rs:1902` | test — preserves-unrelated-blocks | `&[]` |
| `cli.rs:1931` | test — creates-file-when-missing | `&[]` |
| `cli.rs:2366` | test | `&[]` unless it asserts GC |
| `cli.rs:3243` | test | `&[]` unless it asserts GC |
| `cli.rs:3275` | test — local chunked roundtrip | `&[]` (read-back roundtrip, not GC) |

Setup-only writes pass `&[]` (empty ⇒ the writer skips the snapshot/prune
loop entirely, so behaviour is unchanged for those tests). Only tests
that assert pruning build a real `gc_roots`. Existing direct callers that
only `.expect("write")` can ignore the returned `Vec<String>` (return
type changes from `Result<(), _>` to `Result<Vec<String>, _>`, which is
source-compatible with `.expect(...)`).

Inside the function:

1. Snapshot prior state using the **given exact keep-sets** before
   mutation (no inference, no prefix scan):

```rust
let mut plans = Vec::with_capacity(gc_roots.len());
for (root_key, new_keys) in gc_roots {
    let prior_keys = contents_tbl
        .get(root_key)
        .and_then(toml_edit::Item::as_str)
        .map_or_else(|| Ok(Vec::new()), |raw| prior_chunk_keys(root_key, raw));
    plans.push(FastlyConfigGcPlan {
        root_key: root_key.clone(),
        prior_keys,
        new_keys: new_keys.clone(),
        new_root_value: String::new(), // unused locally (no remote concurrency)
    });
}
```

2. Keep the existing upsert loop.

3. After the upsert loop, remove orphans and collect warnings:

```rust
let mut warnings = Vec::new();
for plan in &plans {
    match orphan_chunk_keys(plan) {
        Ok(orphan_keys) => {
            for key in orphan_keys {
                contents_tbl.remove(&key);
            }
        }
        Err(err) => warnings.push(format!("warning: {err}")),
    }
}
```

Return `Ok(warnings)` after `fs::write`. Suspicious prior pointers must not fail the local write and must not produce partial deletes.

In `push_config_entries_local`, capture the warning vector and append it after the existing success line:

```rust
// Expand each logical root once: flatten for the write, keep exact per-root
// keep-sets for GC (no prefix scan of the flattened set).
let mut physical_entries: Vec<(String, String)> = Vec::new();
let mut gc_roots: Vec<(String, std::collections::HashSet<String>)> = Vec::with_capacity(entries.len());
for (root_key, body) in entries {
    let (expanded, new_keys, _new_root) = expand_root(root_key, body)?;
    physical_entries.extend(expanded);
    gc_roots.push((root_key.clone(), new_keys));
}
let warnings = write_fastly_local_config_store(&fastly_path, name, &physical_entries, &gc_roots)?;
let mut out = vec![format!(
    "wrote {} physical entries ({} logical) to `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`); restart `fastly compute serve` to pick up changes",
    physical_entries.len(),
    entries.len(),
    fastly_path.display()
)];
out.extend(warnings);
Ok(out)
```

- [ ] **Step 6: Add local dry-run count helper**

Add a private helper that reads `fastly.toml` and returns counts per root:

```rust
fn local_orphan_counts_for_dry_run(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)], // logical entries; roots are their keys
) -> Vec<(String, Result<usize, String>)> {
    // Roots are the logical entry keys (NOT inferred from physical keys).
    // For each logical (root_key, body):
    //   new_keys = keys of prepare_fastly_config_entries(root_key, body) ∪ {root_key}
    //     — MUST expand here; using the logical key alone would over-count an
    //       identical-bytes re-push (new chunk keys would be missing from
    //       new_keys, so every prior chunk would look like an orphan).
    //   old_raw  = contents table value at root_key (if present)
    //   count    = orphan_chunk_keys({prior_chunk_keys(root, old_raw), new_keys}).len()
    // Missing file, missing/absent contents table, or a root with no prior
    //   pointer / a direct prior value => Ok(0) per root.
    // Unreadable file, malformed TOML, contents-not-a-table, or a root
    //   value that is not a string => Err("could not read prior state").
    // prior_chunk_keys(root, old_raw) returning Err (suspicious pointer)
    //   => Err("suspicious prior pointer").
}
```

This helper MUST NOT fail the dry-run: it returns a per-root `Result`
that the caller renders as a count or an `unknown (...)` line. The
current dry-run reads no file at all, so introducing this read must not
turn a previously-succeeding dry-run into an error. Note it expands each
logical entry via `prepare_fastly_config_entries` to derive the true
`new_keys` — the same expansion the real push performs — so the count
matches what the push would actually delete (0 for an identical
re-push). Use exact `toml_edit` table access, not string scanning. Keep
this helper small; if it gets large, split only into
`local_contents_table` and count computation.

- [ ] **Step 7: Wire local dry-run count output**

In `push_config_entries_local` dry-run, after each direct/chunked line add:

```rust
out.push(format!(
    "  would delete {count} orphan chunks from the previous generation of `{key}`"
));
```

For an `Err`, emit (wording matches the spec's local dry-run bullet —
`{reason}` is `could not read prior state` or `suspicious prior pointer`):

```rust
out.push(format!(
    "  would delete an unknown number of orphan chunks from the previous generation of `{key}` (unknown: {reason})"
));
```

- [ ] **Step 8: Run local scoped tests**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_local -- --nocapture
cargo test -p edgezero-adapter-fastly --features cli second_oversized_push_converges_runtime_on_new_envelope -- --nocapture
cargo test -p edgezero-adapter-fastly --features cli read_config_entry_local -- --nocapture
```

Expected: local push/read tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "feat(fastly): prune local config chunks on re-push"
```

---

### Task 4: Implement Cloud Fastly GC

**Files:**
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:349`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:671`
- Modify: `crates/edgezero-adapter-fastly/src/cli.rs:1073`
- Test: `crates/edgezero-adapter-fastly/src/cli.rs:2440`
- Test: `crates/edgezero-adapter-fastly/src/cli.rs:2760`

- [ ] **Step 1: Add or replace command-aware fake Fastly helper**

Add a Unix-only helper in the test module that:

- Responds to `config-store list --json`.
- Responds to `config-store-entry describe --key=<root>` from a key-response map.
- For the concurrency-guard test, can serve the root `describe`
  **sequentially** — first the prior pointer (pre-commit read), then a
  different value (post-commit read-back) — e.g. via an invocation
  counter file so successive `describe <root>` calls return different
  bodies.
- Accepts `config-store-entry update --upsert --stdin` and logs `update <key>`.
- Accepts `config-store-entry delete --key=<key> --auto-yes` and logs `delete <key>`.
- Can be configured so specific delete keys fail with stderr.

Prefer one operation per log line, for example:

```text
list
describe app_config
update app_config.__edgezero_chunks.<sha>.0
update app_config
delete app_config.__edgezero_chunks.<oldsha>.0
```

- [ ] **Step 2: Add failing cloud tests**

Add tests for:

- Prior chunks are deleted and new/root keys are not. (The fake serves the
  root `describe` twice: the prior pointer pre-commit, then the
  newly-written root value on read-back, so the concurrency guard passes
  and deletes proceed.)
- Shrink-to-direct deletes all prior chunks.
- No prior value produces zero deletes.
- Identical re-push produces zero deletes.
- Delete failure returns `Ok` with a warning line naming the chunk.
- Prior read failure returns `Ok` with a warning and no deletes.
- Suspicious pointer version returns `Ok` with a warning and no deletes.
- Delete operations occur after root update.
- Concurrency guard: when the post-commit root re-read differs from what
  this push wrote (fake `fastly` returns a different root value on the
  second `describe`), GC is skipped with a "root changed" warning and
  **no** deletes are issued.
- Reserved key: `--key` containing `.__edgezero_chunks.` is rejected
  before any `fastly` invocation (no `list`/`describe`/`update`/`delete`).
- Dry-run stays offline and emits no-count GC intent.

Note the concurrency test needs the fake `fastly` to serve the root
`describe` TWICE with different values: the prior pointer (pre-commit
read) and then a mismatching value (post-commit read-back).

Use `path_mutation_guard()` for tests that prepend a fake `fastly`.

- [ ] **Step 3: Run cloud tests to verify failures**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_ -- --nocapture
```

Expected: new GC tests fail because no deletes or warning lines exist yet.

- [ ] **Step 4: Add `delete_config_store_entry`**

Add near `create_config_store_entry`. NOTE: only ever pass `--key`.
The delete subcommand also accepts `-a/--all`, which wipes **every**
entry in the store — never construct that flag here.

```rust
fn delete_config_store_entry(store_id: &str, key: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "delete",
            store_arg.as_str(),
            key_arg.as_str(),
            "--auto-yes",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not found") || lower.contains("does not exist") || lower.contains("404") {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry delete --store-id={store_id} --key={key} --auto-yes` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}
```

- [ ] **Step 5: Build GC plans before cloud commit**

In `push_config_entries`, after `resolved_id`:

```rust
let mut gc_plans = Vec::with_capacity(entries.len());
let mut warnings = Vec::new();
for (root_key, body) in entries {
    // Exact per-root keep-set + the value we will write (no prefix scan).
    let (_, new_keys, new_root_value) = expand_root(root_key, body)?;
    let prior_keys = match fetch_remote_config_store_entry(&resolved_id, root_key) {
        Ok(Some(raw)) => prior_chunk_keys(root_key, &raw),
        Ok(None) => Ok(Vec::new()),
        Err(err) => Err(format!(
            "failed to read prior root `{root_key}` for chunk GC: {err}; skipping GC for this root"
        )),
    };
    gc_plans.push(FastlyConfigGcPlan {
        root_key: root_key.clone(),
        prior_keys,
        new_keys,
        new_root_value,
    });
}
```

If `prior_chunk_keys` returns `Err`, keep the `Err` in the plan so
deletes are skipped (the sweep in Step 6 turns it into a warning).

- [ ] **Step 6: Sweep after successful commit**

Keep the existing `push_entries_with_committer` call. Only after it
returns `Ok`, iterate plans. Before deleting a root's orphans, **re-read
the raw root and confirm it still equals exactly what this push wrote**
(`plan.new_root_value`). This guards against a concurrent push that
overwrote the root between our commit and our sweep — e.g. reverting it
to the prior pointer, which would make the "orphans" live again:

```rust
for plan in &gc_plans {
    let keys = match orphan_chunk_keys(plan) {
        Ok(keys) if !keys.is_empty() => keys,
        Ok(_) => continue,                                  // nothing to reclaim
        Err(err) => { warnings.push(format!("warning: {err}")); continue }
    };
    // Concurrency guard: only sweep if the root still holds our write.
    match fetch_remote_config_store_entry(&resolved_id, &plan.root_key) {
        Ok(Some(current)) if current == plan.new_root_value => {
            for key in keys {
                if let Err(err) = delete_config_store_entry(&resolved_id, &key) {
                    warnings.push(format!(
                        "note: could not reclaim orphan chunk `{key}` for `{}` ({err}); it is inert and will be removed by a future `config gc`",
                        plan.root_key
                    ));
                }
            }
        }
        Ok(_) => warnings.push(format!(
            "note: skipped chunk GC for `{}`: root changed since this push wrote it (concurrent push?); orphans left for a future `config gc`",
            plan.root_key
        )),
        Err(err) => warnings.push(format!(
            "note: skipped chunk GC for `{}`: could not re-read root before sweep ({err}); orphans left for a future `config gc`",
            plan.root_key
        )),
    }
}
```

This narrows but does not fully close the window (a writer could still
intervene between the read-back and an individual delete) — Fastly has no
compare-and-delete. That residual is acceptable under invariant 4:
GC is best-effort and the guard eliminates the realistic revert-to-prior
corruption. Return the existing success line plus warning lines.

- [ ] **Step 7: Keep cloud dry-run offline**

In the `dry_run` branch, after each direct/chunked line add:

```rust
out.push(format!(
    "  would delete orphaned prior-generation chunks of `{key}` (count determined at push time)"
));
```

Do not move `resolve_remote_config_store_id` above the dry-run branch.

- [ ] **Step 8: Run cloud scoped tests**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_ -- --nocapture
```

Expected: cloud push tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "feat(fastly): delete prior remote config chunks after push"
```

---

### Task 5: Integration Cleanup and Verification

**Files:**
- Modify as needed: `crates/edgezero-adapter-fastly/src/cli.rs`
- Modify as needed: `crates/edgezero-adapter-fastly/src/chunked_config.rs`

- [ ] **Step 1: Run Fastly adapter tests**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli
```

Expected: all Fastly adapter CLI-feature tests pass.

- [ ] **Step 2: Run full workspace tests required after code changes**

Run:

```bash
cargo test --workspace --all-targets
```

Expected: all workspace tests pass.

- [ ] **Step 3: Run formatting check**

Run:

```bash
cargo fmt --all -- --check
```

Expected: no formatting diffs.

- [ ] **Step 4: Run clippy**

Run:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: no warnings.

- [ ] **Step 5: Run feature compile check**

Run:

```bash
cargo check --workspace --all-targets --features "fastly cloudflare spin"
```

Expected: compile succeeds.

- [ ] **Step 6: Run Spin target check**

Run:

```bash
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
```

Expected: compile succeeds.

- [ ] **Step 7: Commit final cleanup**

```bash
git add crates/edgezero-adapter-fastly/src/chunked_config.rs crates/edgezero-adapter-fastly/src/cli.rs
git commit -m "test(fastly): verify chunk gc behavior"
```

---

## Notes for Implementers

- Do not delete root keys. Only delete keys returned by `prior_chunk_keys(root_key, raw)` after subtracting the root's new keep-set.
- Source root keys and keep-sets from the caller's logical `entries` via `expand_root` (each root expanded on its own); pass exact per-root keep-sets to the local writer as `gc_roots`. Never infer roots or keep-sets by string-matching `CHUNK_KEY_INFIX` on the flattened physical set — `--key` is free-form and may itself contain the infix (and is separately rejected via `reject_reserved_root_keys`).
- `prior_chunk_keys` parses `serde_json::Value` first: non-pointer-kind values are silent `Ok([])`; a value that IS pointer-kind but malformed/invalid must `Err` (warn), not be silently skipped.
- Do not make cloud dry-run resolve the store id or call `fastly`.
- Do not change `edgezero_adapter::registry::Adapter`; raw prior pointer reads are Fastly-specific.
- Do not use prefix scans of the remote store. The only remote reads are per-root `describe` calls.
- Use exact `toml_edit::Table` key operations for local dotted chunk keys.
- If a push commit fails, do not sweep anything. The old root pointer may still be live.
- If GC fails, return `Ok` with warning status lines. The push success criterion remains new entries committed.
- Cloud GC assumes a **single writer** per config store (serialized pushes). The post-commit read-back guard removes the common revert-before-read-back race but NOT the read-back-then-revert interleaving — Fastly has no compare-and-delete. This is best-effort by design (spec "Concurrency model"); do not represent cloud GC as strictly concurrent-safe.
- Cloud GC costs: one pre-push `describe` per root, plus one post-commit read-back `describe` per root that has orphans, plus one `fastly` `delete` process per orphan (sequential). A 50-chunk prior generation ≈ 50 sequential deletes. Fastly has no bulk delete-by-key (`--all` is store-wide and unusable); do not add parallel spawns in v1 without measuring.
- Line numbers in this plan are approximate anchors against `main`; `grep` the named function/test before editing, since prior edits shift offsets.
