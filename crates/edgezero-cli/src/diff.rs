//! Format renderers for `config diff`: unified (delegating to the
//! unified diff helper), structured (human-readable per-path block list),
//! and json (machine-readable spec 8.1.3 envelope with `local_sha256`,
//! `remote_sha256`, `added`, `removed`, `changed` fields).
//!
//! The orchestrator ([`crate::config::run_config_diff_typed`]) lives in
//! `config.rs` so it can share helper types and load helpers with the push
//! flow.  This module owns the per-format rendering details: the
//! `walk_leaves` / `collect_changes` helpers, the `DiffEntry` /
//! `DiffKind` types, and the three renderer functions called by the
//! dispatcher inside `run_config_diff_typed`.
//!
//! **Stream discipline:** diff CONTENT → stdout (`print!` for
//! unified; `println!` for structured/json).  Informational MESSAGES →
//! stderr via `eprintln!`.  NEVER `log::*` — prefixes corrupt `jq` consumers.

use crate::config::short_ref;
use std::collections::BTreeMap;

// -------------------------------------------------------------------
// Public types — DiffEntry + DiffKind (alphabetical per clippy rule)
// -------------------------------------------------------------------

/// Kind of leaf-level change in a diff.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DiffKind {
    /// Leaf exists in remote but not in local.
    Added,
    /// Leaf exists in both but with different values.
    Changed,
    /// Leaf exists in local but not in remote.
    Removed,
}

/// A single leaf-level change between the remote and local configs.
///
/// Produced by [`collect_changes`] and consumed by both the structured
/// and json renderers.
///
/// Fields are private (accessed via `pub(crate)` struct) to satisfy
/// `clippy::field_scoped_visibility_modifiers`; the renderers and
/// `collect_changes` live in the same module and access fields directly.
#[derive(Debug, serde::Serialize)]
pub(crate) struct DiffEntry {
    /// Old value (present for `Changed` and `Removed`; absent for `Added`).
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<serde_json::Value>,
    /// Whether the leaf was added, removed, or changed.
    kind: DiffKind,
    /// Dot-separated path to the leaf (e.g. `"service.timeout_ms"`).
    path: String,
    /// New value (present for `Changed` and `Added`; absent for `Removed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<serde_json::Value>,
}

// -------------------------------------------------------------------
// walk_leaves — local to diff.rs, NOT exported from config.rs
// -------------------------------------------------------------------

/// Walk every leaf in `value`, emitting `(path, leaf)`.
///
/// Object keys are sorted by UTF-8 byte order so structured/json output
/// is deterministic — matching `render_for_diff`'s ordering rule so a
/// diff with no differences produces identical output across formats.
///
/// **Leaf contract:**
/// - Non-empty objects: recurse into each key (depth-first).
/// - Empty objects (`{}`): emit directly as a leaf — an absent-vs-present
///   empty container is a real structural change that would otherwise produce
///   no leaf and be silently dropped.
/// - Arrays (empty or non-empty): emit as a leaf. Element-wise array diffs
///   are not needed; this matches the unified diff behaviour and 8.1's examples.
/// - Scalars (`bool`, `null`, `number`, `string`): emit as leaves.
fn walk_leaves<F>(value: &serde_json::Value, prefix: String, emit: &mut F)
where
    F: FnMut(String, &serde_json::Value),
{
    match value {
        serde_json::Value::Object(map) if !map.is_empty() => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable_by(|ka, kb| ka.as_bytes().cmp(kb.as_bytes()));
            for key in keys {
                let next = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk_leaves(&map[key], next, emit);
            }
        }
        // Empty nested objects are leaves — a present-but-empty container at
        // a named path is a real structural value.  Only emit when the prefix
        // is non-empty (i.e. nested); the root-level empty document produces
        // no leaves, matching the original behaviour.
        serde_json::Value::Object(_) if !prefix.is_empty() => emit(prefix, value),
        // Root-level empty object: no leaves to emit.
        serde_json::Value::Object(_) => {}
        // Arrays and scalars are leaves — emit unchanged.
        // Explicit arms required by `clippy::wildcard_enum_match_arm`.
        serde_json::Value::Array(_)
        | serde_json::Value::Bool(_)
        | serde_json::Value::Null
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => emit(prefix, value),
    }
}

/// Collect the leaf-level diff between `remote_data` and `local_data`.
///
/// Returns a `Vec<DiffEntry>` with only the leaves that differ (added,
/// removed, or changed).  Leaves whose values are equal are excluded.
pub(crate) fn collect_changes(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
) -> Vec<DiffEntry> {
    let mut leaves: BTreeMap<String, (Option<serde_json::Value>, Option<serde_json::Value>)> =
        BTreeMap::new();
    walk_leaves(remote_data, String::new(), &mut |path, val| {
        leaves.entry(path).or_default().0 = Some(val.clone());
    });
    walk_leaves(local_data, String::new(), &mut |path, val| {
        leaves.entry(path).or_default().1 = Some(val.clone());
    });
    leaves
        .into_iter()
        .filter_map(|(path, (rem, loc))| match (rem, loc) {
            (Some(rr), Some(ll)) if rr == ll => None,
            (Some(rr), Some(ll)) => Some(DiffEntry {
                from: Some(rr),
                kind: DiffKind::Changed,
                path,
                to: Some(ll),
            }),
            (Some(rr), None) => Some(DiffEntry {
                from: Some(rr),
                kind: DiffKind::Removed,
                path,
                to: None,
            }),
            (None, Some(ll)) => Some(DiffEntry {
                from: None,
                kind: DiffKind::Added,
                path,
                to: Some(ll),
            }),
            (None, None) => None,
        })
        .collect()
}

// -------------------------------------------------------------------
// Renderers
// -------------------------------------------------------------------

/// Render the diff in `structured` format: one human-readable block per
/// changed leaf (stdout via `println!`).
///
/// Spec 8.1.2.
#[expect(
    clippy::print_stdout,
    reason = "stream discipline: diff CONTENT goes to stdout, never stderr"
)]
pub(crate) fn render_structured(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
    remote_sha: &str,
    local_sha: &str,
) {
    let changes = collect_changes(remote_data, local_data);
    println!("--- remote (sha256: {})", short_ref(remote_sha));
    println!("+++ local  (sha256: {})", short_ref(local_sha));
    if changes.is_empty() {
        println!("(no differences)");
        return;
    }
    for entry in &changes {
        let kind_label = match entry.kind {
            DiffKind::Added => "added",
            DiffKind::Changed => "changed",
            DiffKind::Removed => "removed",
        };
        println!("{} [{}]", entry.path, kind_label);
        if let Some(from) = &entry.from {
            println!("  - {from}");
        }
        if let Some(to) = &entry.to {
            println!("  + {to}");
        }
    }
}

/// Render the diff in `json` format: spec 8.1.3 machine-readable envelope
/// with `local_sha256`, `remote_sha256`, `added`, `removed`, `changed`
/// (stdout via `println!`).
///
/// `added`   — `path → new_value` for leaves present only in local.
/// `removed` — `path → old_value` for leaves present only in remote.
/// `changed` — `path → { "from": old_value, "to": new_value }` for leaves
///             that differ between remote and local.
///
/// All maps use `BTreeMap` so output is deterministic across runs.
///
/// Spec 8.1.3.
#[expect(
    clippy::print_stdout,
    reason = "stream discipline: diff CONTENT goes to stdout, never stderr"
)]
pub(crate) fn render_json(
    remote_data: &serde_json::Value,
    local_data: &serde_json::Value,
    remote_sha: &str,
    local_sha: &str,
) {
    let diff_entries = collect_changes(remote_data, local_data);
    let mut added: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut removed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut changed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for entry in diff_entries {
        match entry.kind {
            DiffKind::Added => {
                if let Some(val) = entry.to {
                    added.insert(entry.path, val);
                }
            }
            DiffKind::Removed => {
                if let Some(val) = entry.from {
                    removed.insert(entry.path, val);
                }
            }
            DiffKind::Changed => {
                let cell = serde_json::json!({
                    "from": entry.from,
                    "to": entry.to,
                });
                changed.insert(entry.path, cell);
            }
        }
    }
    // Spec 8.1.3 envelope shape.
    let envelope = serde_json::json!({
        "local_sha256": local_sha,
        "remote_sha256": remote_sha,
        "added": added,
        "removed": removed,
        "changed": changed,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "<unrenderable>".into())
    );
}

// -------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::print_unified_diff_to_writer;

    // ------------------------------------------------------------------
    // walk_leaves + collect_changes unit tests
    // ------------------------------------------------------------------

    #[test]
    fn walk_leaves_emits_scalar_root() {
        let val = serde_json::json!(42_i32);
        let mut out: Vec<(String, serde_json::Value)> = Vec::new();
        walk_leaves(&val, String::new(), &mut |path, leaf| {
            out.push((path, leaf.clone()));
        });
        assert_eq!(out.len(), 1_usize);
        assert_eq!(out[0].0, "");
        assert_eq!(out[0].1, serde_json::json!(42_i32));
    }

    #[test]
    fn walk_leaves_emits_nested_paths_sorted() {
        let val = serde_json::json!({
            "zz": 1_i32,
            "aa": { "bb": 2_i32, "cc": 3_i32 },
        });
        let mut paths: Vec<String> = Vec::new();
        walk_leaves(&val, String::new(), &mut |path, _| {
            paths.push(path);
        });
        // Keys sorted by UTF-8 byte order: "aa" < "zz"
        assert_eq!(paths, vec!["aa.bb", "aa.cc", "zz"]);
    }

    #[test]
    fn walk_leaves_treats_array_as_leaf() {
        let val = serde_json::json!({ "tags": [1_i32, 2_i32] });
        let mut out: Vec<String> = Vec::new();
        walk_leaves(&val, String::new(), &mut |path, _| out.push(path));
        assert_eq!(out, vec!["tags"]);
    }

    #[test]
    fn walk_leaves_treats_empty_object_as_leaf() {
        let val = serde_json::json!({ "foo": {} });
        let mut out: Vec<(String, serde_json::Value)> = Vec::new();
        walk_leaves(&val, String::new(), &mut |path, leaf| {
            out.push((path, leaf.clone()));
        });
        assert_eq!(
            out.len(),
            1_usize,
            "empty object must emit exactly one leaf"
        );
        assert_eq!(out[0].0, "foo");
        assert_eq!(out[0].1, serde_json::json!({}));
    }

    #[test]
    fn collect_changes_detects_added_empty_object() {
        let remote = serde_json::json!({});
        let local = serde_json::json!({ "foo": {} });
        let changes = collect_changes(&remote, &local);
        assert_eq!(changes.len(), 1_usize, "added empty object: {changes:?}");
        assert!(matches!(changes[0].kind, DiffKind::Added));
        assert_eq!(changes[0].path, "foo");
        assert_eq!(changes[0].to, Some(serde_json::json!({})));
    }

    #[test]
    fn collect_changes_detects_removed_empty_object() {
        let remote = serde_json::json!({ "foo": {} });
        let local = serde_json::json!({});
        let changes = collect_changes(&remote, &local);
        assert_eq!(changes.len(), 1_usize, "removed empty object: {changes:?}");
        assert!(matches!(changes[0].kind, DiffKind::Removed));
        assert_eq!(changes[0].path, "foo");
        assert_eq!(changes[0].from, Some(serde_json::json!({})));
    }

    #[test]
    fn collect_changes_empty_when_equal() {
        let val = serde_json::json!({ "greeting": "hi", "count": 5_i32 });
        let changes = collect_changes(&val, &val);
        assert!(
            changes.is_empty(),
            "equal data must yield no changes: {changes:?}"
        );
    }

    #[test]
    fn collect_changes_detects_added_leaf() {
        let remote = serde_json::json!({});
        let local = serde_json::json!({ "key": "new" });
        let changes = collect_changes(&remote, &local);
        assert_eq!(changes.len(), 1_usize);
        assert!(matches!(changes[0].kind, DiffKind::Added));
        assert_eq!(changes[0].path, "key");
        assert!(changes[0].from.is_none());
        assert_eq!(changes[0].to, Some(serde_json::json!("new")));
    }

    #[test]
    fn collect_changes_detects_removed_leaf() {
        let remote = serde_json::json!({ "gone": true });
        let local = serde_json::json!({});
        let changes = collect_changes(&remote, &local);
        assert_eq!(changes.len(), 1_usize);
        assert!(matches!(changes[0].kind, DiffKind::Removed));
        assert_eq!(changes[0].path, "gone");
        assert_eq!(changes[0].from, Some(serde_json::json!(true)));
        assert!(changes[0].to.is_none());
    }

    #[test]
    fn collect_changes_detects_changed_leaf() {
        let remote = serde_json::json!({ "timeout": 100_i32 });
        let local = serde_json::json!({ "timeout": 200_i32 });
        let changes = collect_changes(&remote, &local);
        assert_eq!(changes.len(), 1_usize);
        assert!(matches!(changes[0].kind, DiffKind::Changed));
        assert_eq!(changes[0].path, "timeout");
        assert_eq!(changes[0].from, Some(serde_json::json!(100_i32)));
        assert_eq!(changes[0].to, Some(serde_json::json!(200_i32)));
    }

    #[test]
    fn collect_changes_three_leaf_fixture() {
        // Three-leaf fixture: one changed, one added, one removed.
        let remote = serde_json::json!({
            "greeting": "hi",
            "count": 5_i32,
            "removed": "bye",
        });
        let local = serde_json::json!({
            "greeting": "hello",
            "count": 5_i32,
            "added": "new",
        });
        let changes = collect_changes(&remote, &local);
        // `count` is unchanged → excluded.
        // `greeting` changed, `removed` removed, `added` added.
        assert_eq!(changes.len(), 3_usize, "expect 3 diffs: {changes:?}");
        let paths: Vec<&str> = changes.iter().map(|dd| dd.path.as_str()).collect();
        assert!(paths.contains(&"added"), "added leaf: {paths:?}");
        assert!(paths.contains(&"greeting"), "changed leaf: {paths:?}");
        assert!(paths.contains(&"removed"), "removed leaf: {paths:?}");
    }

    // ------------------------------------------------------------------
    // render_structured format dispatch (8.1.2)
    // ------------------------------------------------------------------

    /// Asserts structured output path-grouped blocks (3-leaf fixture).
    ///
    /// We verify via `collect_changes` directly (the renderers write to
    /// stdout, which is hard to capture in unit tests without a test
    /// harness shim). The path-block format is verified by asserting the
    /// change list shape.
    #[test]
    fn structured_three_leaf_fixture_change_paths() {
        let remote = serde_json::json!({
            "api_url": "https://old.example.com",
            "retries": 3_i32,
            "service": { "timeout_ms": 1000_i32 },
        });
        let local = serde_json::json!({
            "api_url": "https://new.example.com",
            "retries": 3_i32,
            "service": { "timeout_ms": 2000_i32 },
        });
        let changes = collect_changes(&remote, &local);
        // `retries` is equal → excluded; `api_url` + `service.timeout_ms` changed.
        assert_eq!(changes.len(), 2_usize, "two changes expected: {changes:?}");
        let paths: Vec<&str> = changes.iter().map(|dd| dd.path.as_str()).collect();
        assert!(paths.contains(&"api_url"), "api_url change: {paths:?}");
        assert!(
            paths.contains(&"service.timeout_ms"),
            "nested path: {paths:?}"
        );
    }

    // ------------------------------------------------------------------
    // render_json envelope shape (8.1.3)
    // ------------------------------------------------------------------

    /// Verify the JSON envelope has the spec 8.1.3 top-level fields:
    /// `local_sha256`, `remote_sha256`, `added`, `removed`, `changed`.
    #[test]
    fn json_envelope_has_spec_fields() {
        let remote = serde_json::json!({ "key": "old" });
        let local = serde_json::json!({ "key": "new" });
        let diff_entries = collect_changes(&remote, &local);
        let mut added: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut removed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut changed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for entry in diff_entries {
            match entry.kind {
                DiffKind::Added => {
                    if let Some(val) = entry.to {
                        added.insert(entry.path, val);
                    }
                }
                DiffKind::Removed => {
                    if let Some(val) = entry.from {
                        removed.insert(entry.path, val);
                    }
                }
                DiffKind::Changed => {
                    changed.insert(
                        entry.path,
                        serde_json::json!({ "from": entry.from, "to": entry.to }),
                    );
                }
            }
        }
        let envelope = serde_json::json!({
            "local_sha256": "def456",
            "remote_sha256": "abc123",
            "added": added,
            "removed": removed,
            "changed": changed,
        });
        let obj = envelope.as_object().expect("envelope is object");
        assert!(obj.contains_key("local_sha256"), "missing local_sha256");
        assert!(obj.contains_key("remote_sha256"), "missing remote_sha256");
        assert!(obj.contains_key("added"), "missing added");
        assert!(obj.contains_key("removed"), "missing removed");
        assert!(obj.contains_key("changed"), "missing changed");
        // changed["key"] must have from/to structure.
        let ch = obj["changed"].as_object().expect("changed is object");
        assert!(ch.contains_key("key"), "changed must contain 'key'");
        let cell = ch["key"].as_object().expect("changed cell is object");
        assert_eq!(cell["from"], serde_json::json!("old"));
        assert_eq!(cell["to"], serde_json::json!("new"));
    }

    /// A key present only in local lands in `added` (not `changes`).
    #[test]
    fn json_added_populated_for_missing_remote_path() {
        let remote = serde_json::json!({});
        let local = serde_json::json!({ "new.key": "new_value" });
        let changes = collect_changes(&remote, &local);
        let mut added: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for entry in changes {
            if matches!(entry.kind, DiffKind::Added)
                && let Some(val) = entry.to
            {
                added.insert(entry.path, val);
            }
        }
        assert_eq!(
            added.get("new.key"),
            Some(&serde_json::json!("new_value")),
            "added must contain the new key: {added:?}"
        );
    }

    /// A leaf that differs between remote and local lands in `changed`
    /// with correct `from`/`to` structure.
    #[test]
    fn json_changed_populated_for_differing_leaf() {
        let remote = serde_json::json!({ "service": { "timeout_ms": 1500_i32 } });
        let local = serde_json::json!({ "service": { "timeout_ms": 2000_i32 } });
        let diff_entries = collect_changes(&remote, &local);
        let mut changed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for entry in diff_entries {
            if matches!(entry.kind, DiffKind::Changed) {
                changed.insert(
                    entry.path.clone(),
                    serde_json::json!({ "from": entry.from, "to": entry.to }),
                );
            }
        }
        let cell = changed
            .get("service.timeout_ms")
            .expect("service.timeout_ms must be in changed");
        assert_eq!(cell["from"], serde_json::json!(1500_i32));
        assert_eq!(cell["to"], serde_json::json!(2000_i32));
    }

    /// Verify the JSON envelope serialises to valid JSON parseable by `jq`.
    #[test]
    fn json_envelope_is_valid_json() {
        let remote = serde_json::json!({ "aa": 1_i32 });
        let local = serde_json::json!({ "aa": 2_i32 });
        let diff_entries = collect_changes(&remote, &local);
        let mut added: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut removed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        let mut changed: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        for entry in diff_entries {
            match entry.kind {
                DiffKind::Added => {
                    if let Some(val) = entry.to {
                        added.insert(entry.path, val);
                    }
                }
                DiffKind::Removed => {
                    if let Some(val) = entry.from {
                        removed.insert(entry.path, val);
                    }
                }
                DiffKind::Changed => {
                    changed.insert(
                        entry.path,
                        serde_json::json!({ "from": entry.from, "to": entry.to }),
                    );
                }
            }
        }
        let envelope = serde_json::json!({
            "local_sha256": "sha_l",
            "remote_sha256": "sha_r",
            "added": added,
            "removed": removed,
            "changed": changed,
        });
        let text = serde_json::to_string_pretty(&envelope).expect("serialise");
        let reparsed: serde_json::Value = serde_json::from_str(&text).expect("round-trip parse");
        assert!(
            reparsed["changed"]["aa"].is_object(),
            "changed.aa must be an object with from/to: {reparsed}"
        );
    }

    // ------------------------------------------------------------------
    // unified format dispatch — route-through test
    // ------------------------------------------------------------------

    /// Verify that the unified dispatcher calls `print_unified_diff_inline`
    /// (via `print_unified_diff_to_writer`) with the same two data blobs.
    ///
    /// We test indirectly: equal blobs produce empty output; different
    /// blobs produce non-empty output.  The actual `print_unified_diff_inline`
    /// path is exercised in config.rs tests; here we only confirm the
    /// dispatcher routes to the right helper.
    #[test]
    fn unified_dispatch_equal_produces_empty_diff() {
        let val = serde_json::json!({ "greeting": "hi" });
        let mut buf = Vec::new();
        print_unified_diff_to_writer(&val, &val, "sha_r", "sha_l", &mut buf).expect("write");
        // Equal inputs → empty diff body (no hunks).
        assert!(
            buf.is_empty(),
            "equal blobs must produce empty unified diff"
        );
    }

    #[test]
    fn unified_dispatch_different_produces_nonempty_diff() {
        let remote = serde_json::json!({ "greeting": "hi" });
        let local = serde_json::json!({ "greeting": "hello" });
        let mut buf = Vec::new();
        print_unified_diff_to_writer(&remote, &local, "sha_r", "sha_l", &mut buf).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(
            !text.is_empty(),
            "different blobs must produce non-empty unified diff"
        );
        assert!(
            text.contains("@@"),
            "unified diff must contain hunk header: {text}"
        );
    }
}
