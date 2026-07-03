use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use edgezero_adapter::registry::{ReadConfigEntry, ResolvedStoreId};

use crate::chunked_config::{prepare_fastly_config_entries, resolve_fastly_config_value};

use super::provision_local::write_fastly_local_config_store;

/// Local-emulator `push_config_entries_local`: edit
/// `[local_server.config_stores.<platform>.contents]` in `fastly.toml`.
/// Viceroy reads it on startup, so a subsequent `fastly compute serve`
/// exposes the new values to the wasm component. No shell-out to the
/// production Fastly CLI -- the operator may not be authenticated and
/// wouldn't want a local push to touch production anyway.
pub(super) fn write_entries(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.fastly.adapter].manifest must point at fastly.toml for config push --local"
                .to_owned(),
        );
    };
    let fastly_path = manifest_root.join(rel);
    let logical = store.logical.as_str();
    let name = store.platform.as_str();
    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to `[local_server.config_stores.{name}]` in {} (logical id `{logical}`)",
            fastly_path.display()
        )]);
    }
    // Expand logical entries into physical entries (chunks + pointer).
    let mut physical_entries: Vec<(String, String)> = Vec::new();
    for (key, body) in entries {
        let expanded = prepare_fastly_config_entries(key, body)?;
        physical_entries.extend(expanded);
    }
    if dry_run {
        let mut out = Vec::with_capacity(entries.len().saturating_add(1));
        out.push(format!(
            "would edit `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`) with entries:",
            fastly_path.display(),
        ));
        for (key, body) in entries {
            let expanded = prepare_fastly_config_entries(key, body)
                .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
            if expanded.len() == 1 {
                out.push(format!(
                    "  would set `{key}` as direct entry ({}B)",
                    body.len()
                ));
            } else {
                let chunk_count = expanded.len().saturating_sub(1);
                out.push(format!(
                    "  would set `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                    body.len()
                ));
            }
        }
        return Ok(out);
    }
    write_fastly_local_config_store(&fastly_path, name, &physical_entries)?;
    Ok(vec![format!(
        "wrote {} physical entries ({} logical) to `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`); restart `fastly compute serve` to pick up changes",
        physical_entries.len(),
        entries.len(),
        fastly_path.display()
    )])
}

/// Local-emulator `read_config_entry_local`: read from
/// `[local_server.config_stores.<platform_name>.contents]` in fastly.toml
/// — the same section `push_config_entries_local` writes.
pub(super) fn read_entry(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
) -> Result<ReadConfigEntry, String> {
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.fastly.adapter].manifest must point at fastly.toml for config diff --local"
                .to_owned(),
        );
    };
    let fastly_path = manifest_root.join(rel);
    let name = store.platform.as_str();
    let raw = match fs::read_to_string(&fastly_path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(ReadConfigEntry::MissingStore),
        Err(err) => {
            return Err(format!("failed to read {}: {err}", fastly_path.display()));
        }
    };
    let doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", fastly_path.display()))?;
    // Probe `[local_server.config_stores.<name>]` — if absent, the store
    // has not been seeded locally yet.
    let Some(contents) = doc
        .get("local_server")
        .and_then(|ls| ls.get("config_stores"))
        .and_then(|cs| cs.get(name))
        .and_then(|store_tbl| store_tbl.get("contents"))
    else {
        return Ok(ReadConfigEntry::MissingStore);
    };
    // The contents table is `key = "value"` pairs.
    match contents.get(key) {
        Some(item) => {
            let value = item.as_str().ok_or_else(|| {
                format!(
                    "`[local_server.config_stores.{name}.contents].{key}` in {} is not a string",
                    fastly_path.display()
                )
            })?;
            // Resolve chunk pointers using the same toml contents table.
            let resolved = resolve_fastly_config_value(key, value.to_owned(), |chunk_key| {
                match contents.get(chunk_key) {
                    Some(chunk_item) => {
                        let chunk_val = chunk_item.as_str().ok_or_else(|| {
                            format!(
                                "chunk key `{chunk_key}` in {} is not a string",
                                fastly_path.display()
                            )
                        })?;
                        Ok(Some(chunk_val.to_owned()))
                    }
                    None => Ok(None),
                }
            })?;
            Ok(ReadConfigEntry::Present(resolved))
        }
        None => Ok(ReadConfigEntry::MissingKey),
    }
}

#[cfg(test)]
mod tests {
    use super::super::provision_local::write_fastly_local_config_store;
    use super::super::FastlyCliAdapter;
    use super::*;
    use edgezero_adapter::registry::{Adapter as _, AdapterPushContext, ResolvedStoreId};
    use tempfile::tempdir;

    // Shared fixture names.
    const TEST_CONFIG_ID: &str = "app_config";

    /// Build a valid `BlobEnvelope` JSON string of approximately `target_len` bytes.
    fn make_test_envelope(target_len: usize) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let pad = "x".repeat(target_len.saturating_add(64));
        let data = json!({ "pad": pad });
        let raw =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
        if raw.len() >= target_len {
            let overhead = raw.len().saturating_sub(pad.len());
            let adjusted = "x".repeat(target_len.saturating_sub(overhead));
            let data2 = json!({ "pad": adjusted });
            serde_json::to_string(&BlobEnvelope::new(data2, "2026-06-22T00:00:00Z".into())).unwrap()
        } else {
            raw
        }
    }

    // ---------- read_config_entry_local ----------

    #[test]
    fn read_local_returns_missing_store_when_fastly_toml_absent() {
        let dir = tempdir().expect("tempdir");
        // No fastly.toml written — file missing.
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing file is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "absent fastly.toml => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_store_when_no_local_server_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // fastly.toml exists but has no [local_server.config_stores.*] block.
        fs::write(&path, "name = \"demo\"\n[setup.config_stores.app_config]\n").expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing local_server block is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "no local_server stanza => MissingStore"
        );
    }

    #[test]
    fn read_local_returns_missing_key_when_key_absent_from_contents() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Write a local_server block with a different key so the store exists
        // but the requested key is absent.
        fs::write(
            &path,
            format!(
                "name = \"demo\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}]\n\
                 format = \"inline-toml\"\n\
                 [local_server.config_stores.{TEST_CONFIG_ID}.contents]\n\
                 other_key = \"other_value\"\n"
            ),
        )
        .expect("write");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("missing key is not an error");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "key absent from contents => MissingKey"
        );
    }

    #[test]
    fn read_local_returns_present_when_key_exists_in_contents() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write initial toml");

        // Use a valid BlobEnvelope value — the resolver requires BlobEnvelope
        // or chunk-pointer JSON; raw strings are not accepted post-chunking.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        write_fastly_local_config_store(
            &path,
            TEST_CONFIG_ID,
            &[("greeting".to_owned(), envelope_json.clone())],
        )
        .expect("setup write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("key present");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present variant");
        };
        assert_eq!(value, envelope_json, "value matches what was written");
    }

    #[test]
    fn read_local_roundtrips_with_push_local() {
        // Write via push_config_entries_local, then read via
        // read_config_entry_local — the two must agree on the value.
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        fs::write(&path, "name = \"demo\"\n").expect("write");

        // push_config_entries_local passes the value through the chunk-pointer
        // helper which stores it verbatim when ≤ 8 000 chars. The reader then
        // resolves it through the same resolver that requires BlobEnvelope JSON.
        let envelope_json = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "roundtrip"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entries = vec![("greeting".to_owned(), envelope_json.clone())];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");
        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("read succeeds");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present after push+read roundtrip");
        };
        assert_eq!(value, envelope_json, "roundtrip value matches");
    }

    #[test]
    fn read_local_requires_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let result = FastlyCliAdapter.read_config_entry_local(
            dir.path(),
            None, // adapter_manifest_path missing
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("[adapters.fastly.adapter].manifest"),
                "error names the missing field: {err}"
            ),
            Ok(_) => panic!("expected Err when adapter_manifest_path is None"),
        }
    }

    // ---------- push_config_entries_local ----------

    /// Spec 12.7: pushing two blobs under different root keys
    /// (e.g. `app_config` + `app_config_staging`) must leave both
    /// keys readable from the local fastly.toml so the runtime
    /// `EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY` override can
    /// switch between them. Prior to the upsert fix the second
    /// push wholesale-replaced the per-store contents table.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_preserves_sibling_keys() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");
        let store = ResolvedStoreId::from_logical(TEST_CONFIG_ID);
        let ctx = AdapterPushContext::new();

        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &store,
                &[("app_config".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &ctx,
                false,
            )
            .expect("first push");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &store,
                &[(
                    "app_config_staging".to_owned(),
                    "{\"envelope\":\"B\"}".to_owned(),
                )],
                &ctx,
                false,
            )
            .expect("second push (sibling key)");

        let raw = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = raw.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents after sibling push");
        let app_config = contents
            .get("app_config")
            .and_then(toml_edit::Item::as_str)
            .expect("default key must survive sibling push");
        assert_eq!(
            app_config, "{\"envelope\":\"A\"}",
            "default key value: {raw}"
        );
        let staging = contents
            .get("app_config_staging")
            .and_then(toml_edit::Item::as_str)
            .expect("staging key must be present");
        assert_eq!(staging, "{\"envelope\":\"B\"}", "staging key value: {raw}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_writes_literal_dotted_chunk_keys() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("local push must succeed");

        let after = fs::read_to_string(&fastly_toml).expect("read back");
        // Chunk keys contain '.' and must appear as quoted string keys,
        // not as TOML nested tables (which would look like [table.sub]).
        assert!(
            after.contains(".__edgezero_chunks."),
            "chunk keys written to fastly.toml: {after}"
        );
        // Parse with toml_edit and confirm chunk keys are string-keyed entries.
        let doc: toml_edit::DocumentMut = after.parse().expect("must parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .expect("contents table must exist");
        // At least one chunk key must be present as a string value (not a table).
        let has_chunk_string = contents.as_table().is_some_and(|tbl| {
            tbl.iter()
                .any(|(key, val)| key.contains(".__edgezero_chunks.") && val.as_value().is_some())
        });
        assert!(
            has_chunk_string,
            "chunk keys must be literal string-valued entries, not nested tables: {after}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_reports_chunking_and_does_not_edit_fastly_toml() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let original = "name = \"demo\"\n";
        fs::write(&fastly_toml, original).expect("write");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("local dry-run must not error");

        // File must be untouched.
        let after = fs::read_to_string(&fastly_toml).expect("read back");
        assert_eq!(after, original, "dry-run must not edit fastly.toml");

        // Output must describe chunking intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would set") && combined.contains("chunked"),
            "must report chunked intent: {combined}"
        );
    }

    // ---------- local read integration tests ----------

    #[test]
    fn read_config_entry_local_resolves_direct_value() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = BlobEnvelope::new(json!({"x": 1_i32}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        // Write directly as a single entry (not via push_config_entries_local so we
        // control the exact TOML content).
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), json_str.clone())],
        )
        .expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("local read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[test]
    fn read_config_entry_local_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        // Write all physical entries (chunks + pointer) to the local store.
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &physical).expect("write");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    /// Spec 12.3 + 9.3: a second oversized push must converge the
    /// runtime on the NEW envelope — chunk keys are content-addressed
    /// by the full-envelope SHA, so push B writes a new chunk-set and
    /// installs a new root pointer.
    ///
    /// The local fastly.toml writer upserts per-key (so a sibling
    /// `--key app_config_staging` push leaves `app_config` intact per
    /// spec 12.7). Within the SAME root key, old chunks for envelope
    /// A remain in the contents table after envelope B's push — they're
    /// unreferenced (the root pointer at `app_config` now names B's
    /// chunks), matching the remote Fastly behaviour where the
    /// per-entry `update --upsert` shell-out has no atomic-delete
    /// pairing. The runtime-correctness property holds either way: a
    /// read after push B follows the active pointer and reconstructs
    /// envelope B, not A.
    #[cfg(unix)]
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "linear test scenario: push A, inspect, push B, inspect, read; splitting would obscure the chunk-set comparison"
    )]
    fn second_oversized_push_converges_runtime_on_new_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(&fastly_toml, "name = \"demo\"\n").expect("seed");

        // First push: envelope A. Records the chunk-key set so we can
        // confirm they survive the second push (no garbage collection
        // in v1 — spec 9.3 + Q6).
        let envelope_a = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_a.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push must succeed");

        let after_a = fs::read_to_string(&fastly_toml).expect("read");
        let doc_a: toml_edit::DocumentMut = after_a.parse().expect("parse");
        let contents_a = doc_a
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push A");
        let chunks_a: Vec<String> = contents_a
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_a.is_empty(),
            "push A must have produced chunk entries: {after_a}"
        );

        // Second push: a DIFFERENT oversized envelope B. The
        // content-addressed chunk keys must shift to B's sha; old
        // A-chunks may remain in the table (v1 doesn't GC). Build
        // envelope B with a distinct payload key so its SHA differs
        // from A's even at the same total length.
        let envelope_b = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ "alt": "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:01Z".to_owned()))
                .expect("envelope B serialises")
        };
        assert_ne!(envelope_a, envelope_b, "test fixtures must differ");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("second push must succeed");

        let after_b = fs::read_to_string(&fastly_toml).expect("read");
        let doc_b: toml_edit::DocumentMut = after_b.parse().expect("parse");
        let contents_b = doc_b
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents table after push B");
        let chunks_b: Vec<String> = contents_b
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(".__edgezero_chunks."))
            .collect();
        assert!(
            !chunks_b.is_empty(),
            "push B must have produced chunk entries: {after_b}"
        );

        // Chunk keys are content-addressed by envelope SHA, so the B
        // push installs a fresh chunk-set whose keys are all distinct
        // from A's. Under the upsert semantic the A-chunks remain in
        // the contents table (no GC in v1); B's chunks are simply added.
        let new_b_chunks: Vec<&String> = chunks_b
            .iter()
            .filter(|key| !chunks_a.contains(*key))
            .collect();
        assert!(
            !new_b_chunks.is_empty(),
            "push B must have added at least one new content-addressed chunk: A-set={chunks_a:?} B-set={chunks_b:?}"
        );
        // Old A-chunks remain in the table (orphan-but-present —
        // matches the remote Fastly write-only-upsert semantic).
        for chunk_key in &chunks_a {
            assert!(
                chunks_b.contains(chunk_key),
                "old A-chunk `{chunk_key}` must remain in the local table after push B (v1 has no GC); B-set={chunks_b:?}"
            );
        }

        // Runtime-correctness property: a fresh read after push B
        // reconstructs envelope B (NOT envelope A).
        let read = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("local read after push B");
        let ReadConfigEntry::Present(value) = read else {
            panic!("expected Present after push B");
        };
        assert_eq!(
            value, envelope_b,
            "read after second push must reconstruct envelope B, not A"
        );
        assert_ne!(
            value, envelope_a,
            "old envelope A's chunks must be inert -- read must NOT return A"
        );
    }
}
