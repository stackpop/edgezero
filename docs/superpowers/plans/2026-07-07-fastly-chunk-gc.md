# Fastly Chunk GC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reclaim obsolete Fastly chunked config-store entries after a successful re-push without changing push success semantics.

**Architecture:** Add a root-scoped prior-pointer inspector in `chunked_config.rs`, then wire cloud and local Fastly push paths in `cli.rs` to snapshot prior chunk keys before writing and sweep them only after the new root entry is committed. Cloud GC is best-effort and warning-only; local GC happens inside the same `DocumentMut` rewrite used for `fastly.toml`.

**Tech Stack:** Rust 2021, `serde_json`, `toml_edit`, existing Fastly CLI shell-out helpers, existing fake-`fastly` test shims, `cargo test -p edgezero-adapter-fastly --features cli`.

---

## File Structure

- Modify `crates/edgezero-adapter-fastly/src/chunked_config.rs`
  - Add `prior_chunk_keys(root_key, raw) -> Result<Vec<String>, String>` next to `FastlyChunkPointer`.
  - Add focused unit tests in the existing `#[cfg(test)]` module.

- Modify `crates/edgezero-adapter-fastly/src/cli.rs`
  - Import `prior_chunk_keys` and `CHUNK_KEY_INFIX`.
  - Add small private GC metadata helpers (`FastlyConfigGcPlan`, `new_key_set_for_root`, `orphan_chunk_keys`) near the config-push helpers. Roots are sourced from logical `entries`/threaded `roots`, never inferred from physical keys.
  - Extend `push_config_entries` with offline dry-run intent, prior root read, post-commit cloud sweep (only after the whole flattened commit returns `Ok`), and warning status lines.
  - Add `delete_config_store_entry(store_id, key)`.
  - Extend `push_config_entries_local` dry-run with best-effort offline orphan counts (degrade to `unknown`, never newly fail).
  - Extend `write_fastly_local_config_store` with a `roots: &[&str]` param; prune prior root chunks in the same in-memory rewrite before the (non-atomic) `fs::write` and return warning lines from suspicious prior pointers.
  - Add/adjust tests in the existing `cli.rs` test module.

Optional (defence-in-depth, spec "Files touched"): reject a `--key`
containing `.__edgezero_chunks.` at push time in `edgezero-cli`
(`config.rs:297` / `args.rs:311`), since such a key collides with the
generated chunk namespace. Not required for GC correctness (roots are
threaded, not inferred); skip if it widens scope.

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

Add before `push_entries_with_committer`. Note there is **no**
`root_keys_from_physical_entries` inference helper: `--key` is a
free-form `Option<String>` (`args.rs:311`), so a logical root key may
itself contain `.__edgezero_chunks.`. Root keys are always sourced from
the caller's **logical** `entries` (cloud) or the threaded `roots` param
(local) — never string-matched out of the flattened physical entries.

```rust
#[derive(Debug)]
struct FastlyConfigGcPlan {
    root_key: String,
    prior_keys: Result<Vec<String>, String>,
    new_keys: std::collections::HashSet<String>,
}

fn new_key_set_for_root(root_key: &str, entries: &[(String, String)]) -> std::collections::HashSet<String> {
    entries
        .iter()
        .filter_map(|(key, _)| {
            if key == root_key || key.starts_with(&format!("{root_key}{CHUNK_KEY_INFIX}")) {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect()
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
```

Keep helpers private. If `clippy` complains about line length or type imports, pull `HashSet` into the top-level imports instead.

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

- [ ] **Step 4: Run local tests to verify failures**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_local -- --nocapture
cargo test -p edgezero-adapter-fastly --features cli second_oversized_push_converges_runtime_on_new_envelope -- --nocapture
```

Expected: failures showing old chunks remain and dry-run lacks GC count.

- [ ] **Step 5: Implement local in-memory pruning**

Change `write_fastly_local_config_store` to (a) accept the **logical
root keys** explicitly via a new `roots: &[&str]` param and (b) return
warning lines, while preserving its error behavior:

```rust
fn write_fastly_local_config_store(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
    roots: &[&str],
) -> Result<Vec<String>, String>
```

Update ALL existing call sites to pass `roots`. The adapter caller
(`push_config_entries_local`) passes the logical entry keys
(`&entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()`).
Setup-only writes in the unit-test module pass the store id they seed
(e.g. `&[TEST_CONFIG_ID]`), or `&[]` where the test does not exercise
GC. Existing direct callers that only `.expect("write")` can ignore the
returned `Vec<String>`.

Inside the function:

1. Snapshot prior state for the **threaded roots** before mutation
   (no inference — the roots are given):

```rust
let mut plans = Vec::with_capacity(roots.len());
for root_key in roots {
    let prior_keys = contents_tbl
        .get(root_key)
        .and_then(toml_edit::Item::as_str)
        .map_or_else(|| Ok(Vec::new()), |raw| prior_chunk_keys(root_key, raw));
    plans.push(FastlyConfigGcPlan {
        root_key: (*root_key).to_owned(),
        prior_keys,
        new_keys: new_key_set_for_root(root_key, entries),
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
let roots: Vec<&str> = entries.iter().map(|(key, _)| key.as_str()).collect();
let warnings = write_fastly_local_config_store(&fastly_path, name, &physical_entries, &roots)?;
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
    // Missing file, missing/absent contents table, or a root with no prior
    //   pointer / a direct prior value => Ok(0) per root.
    // Unreadable file, malformed TOML, contents-not-a-table, or a root
    //   value that is not a string => Err("could not read prior state").
    // prior_chunk_keys(root, old_raw) returning Err (suspicious pointer)
    //   => Err(<that message>).
    // Use prior_chunk_keys(root, old_raw), new_key_set_for_root, and orphan_chunk_keys.
}
```

This helper MUST NOT fail the dry-run: it returns a per-root `Result`
that the caller renders as a count or an `unknown (...)` line. The
current dry-run reads no file at all, so introducing this read must not
turn a previously-succeeding dry-run into an error. Use exact
`toml_edit` table access, not string scanning. Keep this helper small;
if it gets large, split only into `local_contents_table` and count
computation.

- [ ] **Step 7: Wire local dry-run count output**

In `push_config_entries_local` dry-run, after each direct/chunked line add:

```rust
out.push(format!(
    "  would delete {count} orphan chunks from the previous generation of `{key}`"
));
```

For an `Err`, emit:

```rust
out.push(format!(
    "  would delete unknown orphan chunks from the previous generation of `{key}` ({err})"
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

- Prior chunks are deleted and new/root keys are not.
- Shrink-to-direct deletes all prior chunks.
- No prior value produces zero deletes.
- Identical re-push produces zero deletes.
- Delete failure returns `Ok` with a warning line naming the chunk.
- Prior read failure returns `Ok` with a warning and no deletes.
- Suspicious pointer version returns `Ok` with a warning and no deletes.
- Delete operations occur after root update.
- Dry-run stays offline and emits no-count GC intent.

Use `path_mutation_guard()` for tests that prepend a fake `fastly`.

- [ ] **Step 3: Run cloud tests to verify failures**

Run:

```bash
cargo test -p edgezero-adapter-fastly --features cli push_config_entries_ -- --nocapture
```

Expected: new GC tests fail because no deletes or warning lines exist yet.

- [ ] **Step 4: Add `delete_config_store_entry`**

Add near `create_config_store_entry`:

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
for (root_key, _) in entries {
    let prior_keys = match fetch_remote_config_store_entry(&resolved_id, root_key) {
        Ok(Some(raw)) => prior_chunk_keys(root_key, &raw),
        Ok(None) => Ok(Vec::new()),
        Err(err) => {
            Err(format!(
                "failed to read prior root `{root_key}` for chunk GC: {err}; skipping GC for this root"
            ))
        }
    };
    gc_plans.push(FastlyConfigGcPlan {
        root_key: root_key.clone(),
        prior_keys,
        new_keys: new_key_set_for_root(root_key, &physical_entries),
    });
}
```

If `prior_chunk_keys` returns `Err`, push a warning and keep the `Err` in the plan so deletes are skipped.

- [ ] **Step 6: Sweep after successful commit**

Keep the existing `push_entries_with_committer` call. Only after it returns `Ok`, iterate plans:

```rust
for plan in &gc_plans {
    match orphan_chunk_keys(plan) {
        Ok(keys) => {
            for key in keys {
                if let Err(err) = delete_config_store_entry(&resolved_id, &key) {
                    warnings.push(format!(
                        "warning: failed to delete orphan chunk `{key}` for `{}`: {err}",
                        plan.root_key
                    ));
                }
            }
        }
        Err(err) => warnings.push(format!(
            "warning: {err}"
        )),
    }
}
```

Return the existing success line plus warning lines.

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
- Source root keys from the caller's logical `entries` (cloud) or the threaded `roots: &[&str]` param (local). Never infer roots by string-matching `CHUNK_KEY_INFIX` on physical keys — `--key` is free-form and may itself contain the infix.
- `prior_chunk_keys` parses `serde_json::Value` first: non-pointer-kind values are silent `Ok([])`; a value that IS pointer-kind but malformed/invalid must `Err` (warn), not be silently skipped.
- Do not make cloud dry-run resolve the store id or call `fastly`.
- Do not change `edgezero_adapter::registry::Adapter`; raw prior pointer reads are Fastly-specific.
- Do not use prefix scans of the remote store. The only remote reads are per-root `describe` calls.
- Use exact `toml_edit::Table` key operations for local dotted chunk keys.
- If a push commit fails, do not sweep anything. The old root pointer may still be live.
- If GC fails, return `Ok` with warning status lines. The push success criterion remains new entries committed.
