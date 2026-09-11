use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use edgezero_adapter::registry::{ReadConfigEntry, ResolvedStoreId};

use crate::chunked_config::{
    chunk_key_generation, gc_classify_root, prepare_fastly_config_entries, prior_chunk_keys,
    resolve_fastly_config_value_typed, value_announces_our_kind, value_is_future_format,
};

use super::provision_local::write_fastly_local_config_store;
use super::{classify_resolved_read, expand_root, reject_generated_key_collisions};

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
    // Reject reserved / duplicate keys before any expansion or I/O.
    super::reject_reserved_root_keys(entries)?;
    super::reject_duplicate_root_keys(entries)?;
    // Expand each logical root once: flatten for the write, keep the exact
    // per-root keep-set for GC (no prefix scan of the flattened set).
    let mut physical_entries: Vec<(String, String)> = Vec::new();
    let mut gc_roots: Vec<(String, HashSet<String>)> = Vec::with_capacity(entries.len());
    for (key, body) in entries {
        let (expanded, new_keys, _new_root) = expand_root(key, body)?;
        physical_entries.extend(expanded);
        gc_roots.push((key.clone(), new_keys));
    }
    if dry_run {
        // Per the chunk-GC dry-run contract (spec 2026-07-07 §"Error
        // semantics"), a dry-run MUST NOT newly fail: it previews the edit and
        // DEGRADES the orphan count (absent prior -> 0; unreadable / malformed
        // / non-string prior -> "unknown") rather than rejecting. The REAL
        // push still fails fatally at the writer on malformed / not-yet-
        // provisioned state; only this preview count degrades. Read-only.
        let counts = local_orphan_counts_for_dry_run(&fastly_path, name, entries);
        let mut out = Vec::with_capacity(entries.len().saturating_mul(2).saturating_add(1));
        out.push(format!(
            "would edit `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`) with entries:",
            fastly_path.display(),
        ));
        for (idx, (key, body)) in entries.iter().enumerate() {
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
            match counts.get(idx).map(|(_, count)| count) {
                Some(Ok(n)) => out.push(format!(
                    "  would delete {n} orphan chunks from the previous generation of `{key}`"
                )),
                Some(Err(reason)) => out.push(format!(
                    "  would delete an unknown number of orphan chunks from the previous generation of `{key}` (unknown: {reason})"
                )),
                None => {}
            }
        }
        return Ok(out);
    }
    let warnings =
        write_fastly_local_config_store(&fastly_path, name, &physical_entries, &gc_roots)?;
    let mut out = vec![format!(
        "wrote {} physical entries ({} logical) to `[local_server.config_stores.{name}.contents]` in {} (logical id `{logical}`); restart `fastly compute serve` to pick up changes",
        physical_entries.len(),
        entries.len(),
        fastly_path.display()
    )];
    out.extend(warnings);
    Ok(out)
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
    // A prior-state read failure must never BLOCK the command: the diff just
    // cannot be computed, so it degrades to `Unsupported` ("cannot diff").
    // Erroring here would newly fail a dry-run that reads nothing today.
    let raw = match fs::read_to_string(&fastly_path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(ReadConfigEntry::MissingStore);
        }
        Err(_err) => {
            return Ok(ReadConfigEntry::Unsupported(
                "local fastly.toml could not be read; cannot diff the prior value",
            ));
        }
    };
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return Ok(ReadConfigEntry::Unsupported(
            "local fastly.toml is not valid TOML; cannot diff the prior value",
        ));
    };
    // Descend `[local_server.config_stores.<name>.contents]` level by level.
    // At each level an ABSENT key means the store isn't seeded yet
    // (MissingStore), but a key that is PRESENT yet not a table is malformed
    // store state — distinct outcomes. `descend` returns Ok(None) for absent
    // (-> MissingStore) and Err(Unsupported) for present-but-not-a-table.
    let descend = |parent: &'_ toml_edit::Item,
                   child: &str|
     -> Result<Option<toml_edit::Item>, ReadConfigEntry> {
        match parent.get(child) {
            None => Ok(None),
            Some(item) if item.is_table_like() => Ok(Some(item.clone())),
            Some(_) => Err(ReadConfigEntry::Unsupported(
                "a local config-store parent table is not a table; cannot diff the prior value",
            )),
        }
    };
    let root_item = toml_edit::Item::Table(doc.as_table().clone());
    let contents_item = (|| {
        let Some(local_server) = descend(&root_item, "local_server")? else {
            return Ok(None);
        };
        let Some(config_stores) = descend(&local_server, "config_stores")? else {
            return Ok(None);
        };
        let Some(store_tbl) = descend(&config_stores, name)? else {
            return Ok(None);
        };
        descend(&store_tbl, "contents")
    })();
    let contents = match contents_item {
        Ok(Some(item)) => item,
        Ok(None) => return Ok(ReadConfigEntry::MissingStore),
        Err(unsupported) => return Ok(unsupported),
    };
    let Some(contents_tbl) = contents.as_table_like() else {
        return Ok(ReadConfigEntry::Unsupported(
            "local config-store `contents` is not a table; cannot diff the prior value",
        ));
    };
    // The contents table is `key = "value"` pairs.
    match contents_tbl.get(key) {
        Some(item) => {
            let Some(value) = item.as_str() else {
                return Ok(ReadConfigEntry::Unsupported(
                    "the local prior value is not a string; cannot diff the prior value",
                ));
            };
            // Resolve chunk pointers using the same toml contents table.
            let resolved = resolve_fastly_config_value_typed(key, value.to_owned(), |chunk_key| {
                match contents_tbl.get(chunk_key) {
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
            });
            // Same taxonomy as the cloud read: a valid envelope is `Present`; a
            // non-envelope or corrupt/incomplete value is `Corrupt`; an
            // unknown/future kind is a hard error. There is no infrastructure
            // fetch here, so `fetch_failed` is always false.
            classify_resolved_read(resolved, value, false)
        }
        None => Ok(ReadConfigEntry::MissingKey),
    }
}

/// Navigate to `[local_server.config_stores.<name>.contents]` for the
/// dry-run counter. `Ok(None)` when any level is absent (no prior state);
/// `Err` when a level is present but the wrong type — prior state the real
/// writer would reject, so the count must degrade to "unknown", not 0.
fn local_contents_table<'doc>(
    doc: &'doc toml_edit::DocumentMut,
    platform_name: &str,
) -> Result<Option<&'doc toml_edit::Table>, String> {
    let malformed = || "could not read prior state".to_owned();
    let Some(server_item) = doc.get("local_server") else {
        return Ok(None);
    };
    let Some(server) = server_item.as_table() else {
        return Err(malformed());
    };
    let Some(stores_item) = server.get("config_stores") else {
        return Ok(None);
    };
    let Some(stores) = stores_item.as_table() else {
        return Err(malformed());
    };
    let Some(store_item) = stores.get(platform_name) else {
        return Ok(None);
    };
    let Some(store) = store_item.as_table() else {
        return Err(malformed());
    };
    let Some(contents_item) = store.get("contents") else {
        return Ok(None);
    };
    contents_item
        .as_table()
        .map_or_else(|| Err(malformed()), |table| Ok(Some(table)))
}

/// Is `key` a plain, prunable chunk PAYLOAD in `contents`? `false` for a value
/// that must be KEPT: a runtime-readable root, a value claiming our
/// `edgezero_kind` namespace or written by a newer format, or a NESTED root (a
/// key with a canonical chunk beneath it). Only a raw leaf payload prunes.
///
/// The single source of truth shared by the real prune
/// (`write_fastly_local_config_store`) and the dry-run count, so the previewed
/// number can never drift from what `--yes` actually removes.
pub(super) fn is_prunable_leaf(contents: &toml_edit::Table, key: &str) -> bool {
    let value_protected = contents
        .get(key)
        .and_then(toml_edit::Item::as_str)
        .is_some_and(|text| {
            value_announces_our_kind(text)
                || value_is_future_format(text)
                || gc_classify_root(key, text).is_ok()
        });
    let has_nested = contents
        .iter()
        .any(|(other, _)| other != key && chunk_key_generation(key, other).is_some());
    !(value_protected || has_nested)
}

/// [`reject_generated_key_collisions`] against a local `contents` table.
pub(super) fn reject_local_generated_key_collisions(
    contents_tbl: &toml_edit::Table,
    entries: &[(String, String)],
) -> Result<(), String> {
    let sibling_keys: HashSet<String> = contents_tbl
        .iter()
        .map(|(existing_key, _)| existing_key.to_owned())
        .collect();
    reject_generated_key_collisions(entries, &sibling_keys, |chunk_key| {
        Ok(contents_tbl
            .get(chunk_key)
            .and_then(toml_edit::Item::as_str)
            .map(str::to_owned))
    })
}

/// Best-effort per-root orphan count for `config push --local --dry-run`.
/// Never fails the dry-run: on a missing file / no prior pointer / direct prior
/// value it reports `Ok(0)`; on unreadable or malformed prior state it reports
/// `Err(reason)` which the caller renders as an "unknown" line.
fn local_orphan_counts_for_dry_run(
    path: &Path,
    platform_name: &str,
    entries: &[(String, String)],
) -> Vec<(String, Result<usize, String>)> {
    use toml_edit::DocumentMut;

    // Parse the current file once (best-effort). Absent file => no prior.
    let parsed: Result<Option<DocumentMut>, String> = match fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map(Some)
            .map_err(|_err| "could not read prior state".to_owned()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(_) => Err("could not read prior state".to_owned()),
    };

    entries
        .iter()
        .map(|(root_key, body)| {
            let new_keys = match expand_root(root_key, body) {
                Ok((_, keys, _)) => keys,
                Err(err) => return (root_key.clone(), Err(err)),
            };
            let count = match &parsed {
                Err(reason) => Err(reason.clone()),
                Ok(None) => Ok(0),
                Ok(Some(doc)) => match local_contents_table(doc, platform_name) {
                    Err(reason) => Err(reason),
                    Ok(None) => Ok(0),
                    Ok(Some(contents)) => match contents.get(root_key) {
                        None => Ok(0), // no prior value for this root
                        Some(item) => match item.as_str() {
                            None => Err("could not read prior state".to_owned()),
                            Some(raw) => match prior_chunk_keys(root_key, raw) {
                                Ok(prior) => Ok(prior
                                    .iter()
                                    .filter(|key| !new_keys.contains(*key))
                                    // Count only what the real prune would remove:
                                    // it must still be PRESENT and a prunable leaf
                                    // by the SAME predicate the prune uses.
                                    .filter(|key| {
                                        contents.get(key.as_str()).is_some()
                                            && is_prunable_leaf(contents, key)
                                    })
                                    .count()),
                                Err(_) => Err("suspicious prior pointer".to_owned()),
                            },
                        },
                    },
                },
            };
            (root_key.clone(), count)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::FastlyCliAdapter;
    use super::super::provision_local::write_fastly_local_config_store;
    use super::*;
    use crate::chunked_config::CHUNK_KEY_INFIX;
    #[cfg(unix)]
    use crate::cli::test_support::chunk_keys_of;
    use edgezero_adapter::registry::{Adapter as _, AdapterPushContext, ResolvedStoreId};
    use tempfile::tempdir;

    // Shared fixture names.
    const TEST_CONFIG_ID: &str = "app_config";

    /// Write a `fastly.toml` at `path` carrying `name = "demo"` plus the
    /// provisioned `[local_server.config_stores.<platform>]` block (with
    /// `format` + an empty `contents` table) that `provision --local`
    /// creates. `config push --local` only upserts into that existing
    /// table -- it refuses to fabricate the block -- so tests that push
    /// must seed it first.
    fn seed_provisioned(path: &Path, platform: &str) {
        fs::write(
            path,
            format!(
                "name = \"demo\"\n\n\
                 [local_server.config_stores.{platform}]\n\
                 format = \"inline-toml\"\n\n\
                 [local_server.config_stores.{platform}.contents]\n"
            ),
        )
        .expect("seed provisioned fastly.toml");
    }

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
        seed_provisioned(&path, TEST_CONFIG_ID);

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
            &[],
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
        seed_provisioned(&path, TEST_CONFIG_ID);

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

    /// Push-after-provision: `config push --local` writes config into
    /// `[local_server.config_stores.*]`; it must leave the operator's
    /// hand-edited `[[local_server.secret_stores.<id>]]` entry (real
    /// `env` mapping) untouched.
    #[test]
    fn push_after_provision_preserves_secret_store_entry() {
        use edgezero_adapter::registry::{ProvisionMode, TypedSecretEntry};
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fastly.toml");
        // Baseline + the provisioned config-store block the later
        // `config push --local` upserts into (absolute dotted header, so
        // it appends cleanly regardless of the baseline's trailing table).
        let baseline = format!(
            "{}\n[local_server.config_stores.{TEST_CONFIG_ID}]\nformat = \"inline-toml\"\n[local_server.config_stores.{TEST_CONFIG_ID}.contents]\n",
            super::super::run::synthesise_fastly_toml("demo", None),
        );
        fs::write(&path, baseline).expect("write baseline");
        // 1. Provision writes the `[[local_server.secret_stores.*]]`
        //    entry (env defaults to the upper-cased key).
        FastlyCliAdapter
            .provision_typed(
                dir.path(),
                Some("fastly.toml"),
                None,
                &[TypedSecretEntry::new("default", "field", "api_token")],
                ProvisionMode::Local,
                false,
            )
            .expect("provision_typed writes the secret entry");
        // 2. Operator customises the env mapping on that entry.
        let provisioned = fs::read_to_string(&path).expect("provision wrote fastly.toml");
        assert!(
            provisioned.contains("[[local_server.secret_stores.default]]"),
            "provision must write the secret_store entry: {provisioned}"
        );
        fs::write(
            &path,
            provisioned.replace("env = \"API_TOKEN\"", "env = \"REAL_ENV_MAPPING\""),
        )
        .expect("operator edit");

        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "world"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[("greeting".to_owned(), envelope)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("push succeeds");

        let after = fs::read_to_string(&path).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("re-parse");
        let arr = doc["local_server"]["secret_stores"]["default"]
            .as_array_of_tables()
            .expect("secret_stores.default preserved");
        assert_eq!(
            arr.len(),
            1,
            "operator's secret entry still present: {after}"
        );
        let row = arr.get(0).expect("row");
        assert_eq!(
            row.get("key").and_then(|item| item.as_str()),
            Some("api_token")
        );
        assert_eq!(
            row.get("env").and_then(|item| item.as_str()),
            Some("REAL_ENV_MAPPING"),
            "config push must not disturb the operator's secret_store env mapping: {after}"
        );
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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);
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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);
        let original = fs::read_to_string(&fastly_toml).expect("read seed");

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

        // File must be untouched — including by the structural probe.
        let after = fs::read_to_string(&fastly_toml).expect("read back");
        assert_eq!(after, original, "dry-run must not edit fastly.toml");

        // Output must describe chunking intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would set") && combined.contains("chunked"),
            "must report chunked intent: {combined}"
        );
    }

    #[test]
    fn push_config_entries_local_dry_run_continues_over_unprovisioned_store() {
        // Per the chunk-GC dry-run contract (spec 2026-07-07 §"Error
        // semantics"), a dry-run MUST NOT fail on absent / unprovisioned prior
        // state: it PREVIEWS the edit and degrades the orphan count (absent
        // prior -> 0). The REAL push still refuses at the writer -- that path
        // is unchanged.
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let original = "name = \"demo\"\n";
        fs::write(&fastly_toml, original).expect("write");

        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[("greeting".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must NOT fail on an unprovisioned store");
        assert!(
            out.iter().any(|line| line.contains("would set `greeting`")),
            "dry-run previews the edit: {out:?}"
        );
        // Read-only: the probe must leave the file byte-identical.
        let after = fs::read_to_string(&fastly_toml).expect("read back");
        assert_eq!(after, original, "dry-run must not edit fastly.toml");
    }

    #[test]
    fn push_config_entries_local_dry_run_degrades_over_malformed_prior() {
        // Malformed prior TOML -> the orphan count degrades to "unknown" and
        // the dry-run CONTINUES (does not error, does not leak the raw value).
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let original = "this is not = = valid toml with SECRET_abc in it\n";
        fs::write(&fastly_toml, original).expect("write");

        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[("greeting".to_owned(), "{\"envelope\":\"A\"}".to_owned())],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must degrade over malformed prior, not fail");
        let joined = out.join("\n");
        assert!(
            joined.contains("unknown number of orphan chunks") && !joined.contains("SECRET_abc"),
            "count degrades to unknown without leaking the raw value: {joined}"
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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), json_str.clone())],
            &[],
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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &physical, &[])
            .expect("write");

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
    /// spec 12.7). Within the SAME root key, GC on re-push prunes the
    /// prior generation: after envelope B's push, envelope A's chunks —
    /// now unreferenced by the `app_config` pointer — are removed from
    /// the contents table. A read after push B follows the active
    /// pointer and reconstructs envelope B, not A.
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
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // First push: envelope A. Records the chunk-key set so we can
        // confirm they are pruned by the second push's GC.
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
        // content-addressed chunk keys must shift to B's sha; GC then
        // prunes the old A-chunks. Build envelope B with a distinct
        // payload key so its SHA differs from A's even at the same
        // total length.
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
        // from A's. GC on re-push prunes the now-unreferenced A-chunks.
        let new_b_chunks: Vec<&String> = chunks_b
            .iter()
            .filter(|key| !chunks_a.contains(*key))
            .collect();
        assert!(
            !new_b_chunks.is_empty(),
            "push B must have added at least one new content-addressed chunk: A-set={chunks_a:?} B-set={chunks_b:?}"
        );
        // Old A-chunks are pruned: GC deletes the prior generation the
        // old pointer referenced once B's pointer supersedes it.
        for chunk_key in &chunks_a {
            assert!(
                !chunks_b.contains(chunk_key),
                "old A-chunk `{chunk_key}` must be pruned from the local table after push B; B-set={chunks_b:?}"
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

    /// a corrupt/invalid prior value must NOT abort the
    /// local read, or the CLI push aborts on the diff read before the writer's
    /// fail-soft ("overwrite, warn, prune nothing") can repair the state.
    /// `config push` is how an operator recovers, so the read reports `Corrupt`
    /// ("cannot diff; will overwrite") and lets the write proceed.
    #[test]
    fn read_config_entry_local_degrades_corrupt_prior_to_corrupt() {
        use crate::chunked_config::{CHUNK_KEY_INFIX, POINTER_KIND};
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // A pointer-KIND value that is invalid (missing the chunks it needs).
        // The resolver would error on this; the local read must NOT propagate
        // that as `Err`.
        let broken_pointer = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"cfg{CHUNK_KEY_INFIX}{sha}.0","len":10,"sha256":"x"}}],"data_sha256":"","envelope_len":10,"envelope_sha256":"{sha}"}}"#,
            sha = "a".repeat(64),
        );
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[("cfg".to_owned(), broken_pointer)],
            &[],
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
            .expect("a corrupt local prior must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Corrupt(_)),
            "a corrupt prior value must degrade to Corrupt so the push can overwrite it"
        );
    }

    /// A `contents` that is not a table (a scalar or array) is malformed store
    /// state. It must degrade to `Unsupported`, not fall through to `MissingKey`
    /// (which would render an inaccurate "all values added" diff).
    #[test]
    fn read_config_entry_local_non_table_contents_is_unsupported() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        fs::write(
            &fastly_toml,
            format!("[local_server.config_stores.{TEST_CONFIG_ID}]\ncontents = 42\n"),
        )
        .expect("seed");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("a non-table contents must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "a non-table `contents` must degrade to Unsupported, not MissingKey"
        );
    }

    /// A malformed PARENT table (`local_server` etc. as a scalar) must degrade to
    /// Unsupported, not collapse to `MissingStore`'s "all values added" diff.
    #[test]
    fn read_config_entry_local_non_table_parent_is_unsupported() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // `local_server` is a scalar, not a table.
        fs::write(&fastly_toml, "local_server = 42\n").expect("seed");

        let result = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("a non-table parent must NOT abort the read");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "a non-table parent must degrade to Unsupported, not MissingStore"
        );
    }

    // ---------- local chunk GC ----------

    /// Config shrinks from chunked back under the 8 000-char limit: the
    /// new value is a direct envelope, so GC prunes every prior chunk.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_prunes_prior_chunks_when_value_shrinks_to_direct() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

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

        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "root holds the direct envelope"
        );
        assert!(
            !contents
                .iter()
                .any(|(key, _)| key.contains(CHUNK_KEY_INFIX)),
            "prior chunks must be pruned: {after}"
        );
    }

    /// The local prune must NOT delete a prior chunk key whose VALUE is a
    /// runtime-readable root (a valid direct envelope). A small envelope padded
    /// with trailing whitespace chunks so that chunk 0 is itself a whole,
    /// verifying envelope; deleting it would drop live config.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_a_valid_envelope() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // A padded envelope: chunk 0 is the whole envelope plus trailing spaces
        // (still a valid, verifying envelope on its own).
        let envelope = BlobEnvelope::new(json!({ "k": "v" }), "2026-06-22T00:00:00Z".into());
        let mut padded = serde_json::to_string(&envelope).unwrap();
        padded.push_str(&" ".repeat(8_200));
        let entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &padded).expect("expand");
        let chunk0_key = entries[0].0.clone();
        // Confirm the fixture: chunk 0's value verifies as an envelope.
        let parsed: BlobEnvelope = serde_json::from_str(&entries[0].1).expect("chunk0 parses");
        parsed.verify().expect("chunk0 verifies");

        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), padded)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("first push");

        // Re-push a direct value: the prior generation's chunks become orphans.
        let direct = make_test_envelope(100);
        let expected_deletions = entries.len().saturating_sub(2); // chunks minus the protected chunk0

        // DRY-RUN first: its count must MATCH what the real prune deletes, i.e.
        // it must exclude the protected root-like chunk0.
        let dry = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run");
        assert!(
            dry.join("\n")
                .contains(&format!("would delete {expected_deletions} orphan chunks")),
            "dry-run must count only the prunable orphans (excluding the protected root): {dry:?}"
        );

        let warnings = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
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

        assert!(
            contents.contains_key(&chunk0_key),
            "a chunk key holding a valid envelope is a runtime-readable root and must be kept: \
             {after}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("runtime-readable root")),
            "the operator must be warned that the key was kept: {warnings:?}"
        );
    }

    /// The local prune must NOT delete a prior chunk key whose value was written
    /// by a NEWER format -- a v2 direct envelope (bumped version) stored under a
    /// chunk-shaped key. Cloud GC fails closed on it; local prune must be
    /// symmetric, or an older CLI destroys config a newer writer produced.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_a_future_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        // A v2 direct envelope (version bumped) parked at a chunk key.
        let mut v2_value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&BlobEnvelope::new(
                json!({ "k": "v" }),
                "2026-01-01T00:00:00Z".to_owned(),
            ))
            .unwrap(),
        )
        .unwrap();
        v2_value["version"] = json!(2_u32);
        let v2 = v2_value.to_string();

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let victim = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        contents.insert(&victim, toml_edit::value(v2));
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        let direct = make_test_envelope(100);
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        assert!(
            after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
                .as_table()
                .expect("contents")
                .contains_key(&victim),
            "a v2 (future-format) envelope must be KEPT, not pruned: {after}"
        );
    }

    /// The local prune must NOT delete a prior chunk key whose value claims our
    /// `edgezero_kind` namespace with an UNKNOWN/future kind. The cloud GC path
    /// fails closed on such a value; local replacement must be symmetric, or it
    /// would destroy a newer-format entry an older CLI cannot understand.
    #[test]
    fn push_config_entries_local_keeps_a_chunk_key_holding_an_unknown_kind() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // Seed a real chunked generation, then overwrite ONE chunk value with a
        // future-format value that claims our namespace but is not a v1 pointer.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let victim = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        contents.insert(
            &victim,
            toml_edit::value(r#"{"edgezero_kind":"fastly_config_chunks_v2","new":true}"#),
        );
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Re-push a direct value: every prior chunk becomes an orphan.
        let direct = make_test_envelope(100);
        let warnings = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let present = after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table()
            .expect("contents")
            .contains_key(&victim);
        assert!(
            present,
            "an unknown/future-kind value must be KEPT (symmetric with cloud GC fail-closed): {after}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("kept") && warning.contains("edgezero_kind")),
            "the operator must be warned the namespace-claiming key was kept: {warnings:?}"
        );
    }

    /// SYMMETRY with cloud GC: a local prune must NOT delete a truncated pointer
    /// at a chunk-shaped key that HAS a canonical chunk nested beneath it — it is
    /// a (broken) nested root, and removing it would orphan the nested chunks.
    #[test]
    fn push_config_entries_local_keeps_a_malformed_nested_root_holder() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // Seed a chunked generation, then turn ONE chunk key into a nested root:
        // give it a truncated (unclassifiable) value AND a canonical chunk nested
        // beneath it.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let holder = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a chunk key");
        // Truncated pointer at the holder (unclassifiable, announces no kind).
        contents.insert(&holder, toml_edit::value(r#"{"chunks":[{"key":"#));
        // A canonical chunk nested BENEATH the holder.
        let nested_chunk = format!("{holder}{CHUNK_KEY_INFIX}{}.0", "b".repeat(64));
        contents.insert(&nested_chunk, toml_edit::value("nested-payload"));
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Re-push a direct value: every prior chunk becomes an orphan, including
        // the holder (which the OLD pointer referenced).
        let direct = make_test_envelope(100);
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("re-push");

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let after_doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let after_contents = after_doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table()
            .expect("contents");
        assert!(
            after_contents.contains_key(&holder),
            "a nested-root holder with chunks beneath it must be KEPT, not pruned: {after}"
        );
    }

    /// A logical key containing the reserved chunk infix is rejected
    /// before any file I/O (it would collide with the chunk namespace).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_rejects_reserved_key() {
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let bad_key = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");

        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(bad_key.clone(), "{}".to_owned())],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("reserved key must be rejected");
        assert!(err.contains(&bad_key), "error names the key: {err}");
        assert!(
            !fastly_toml.exists(),
            "rejection must happen before any write"
        );
    }

    /// A suspicious prior pointer (pointer-kind but invalid) makes GC
    /// warn and delete nothing — pre-seeded chunk keys must survive.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_warns_on_suspicious_prior_pointer_and_keeps_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // Seed the root with a pointer-kind-but-invalid value AND a real
        // chunk-like key so "no deletes" is non-vacuous.
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
            "\"app_config.__edgezero_chunks.deadbeef.0\" = \"seeded-chunk-payload\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must still succeed");

        let combined = out.join("\n");
        assert!(
            combined.contains("skipping chunk GC"),
            "must warn about the suspicious prior pointer: {combined}"
        );

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");
        assert!(
            contents
                .get("app_config.__edgezero_chunks.deadbeef.0")
                .is_some(),
            "pre-seeded chunk key must survive a suspicious-pointer skip: {after}"
        );
        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "new value still written"
        );
    }

    /// TOCTOU guard: if the locked reread finds the root now holds a NEWER format
    /// (installed between the pre-push check and the lock), the writer must REFUSE
    /// to overwrite it -- an older writer must never clobber a newer format.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_to_overwrite_a_future_prior() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // The root now holds a v2 direct envelope from a newer writer.
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"data\\\":{},\\\"sha256\\\":\\\"x\\\",\\\"generated_at\\\":\\\"t\\\",\\\"version\\\":2}\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a future prior must abort the push under the lock");
        assert!(
            err.contains("newer format") && err.to_lowercase().contains("refusing"),
            "must refuse to clobber a newer format: {err}"
        );
        // The v2 value must survive untouched.
        let after = fs::read_to_string(&fastly_toml).expect("read");
        assert!(
            after.contains("\\\"version\\\":2"),
            "the newer-format value must be left intact: {after}"
        );
    }

    /// The locked downgrade guard must catch a future INNER envelope hidden behind
    /// a VALID v1 pointer -- only knowable after reconstruction against the locked
    /// contents. The raw pointer looks like healthy v1.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_a_future_inner_prior() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // A large v1 envelope, chunked, with its inner version bumped to 2. Seed
        // the pointer + chunks as the prior local state.
        let v1 = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let mut v2_value: serde_json::Value = serde_json::from_str(&v1).expect("parse");
        v2_value["version"] = serde_json::json!(2_u32);
        let v2 = v2_value.to_string();
        let seed_entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &v2).expect("chunk");
        write_fastly_local_config_store(&fastly_toml, TEST_CONFIG_ID, &seed_entries, &[])
            .expect("seed the prior v2-inner generation");

        let direct = make_test_envelope(100);
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a future INNER prior must abort the push under the lock");
        assert!(
            err.contains("newer format") && err.to_lowercase().contains("refusing"),
            "must refuse to clobber a future inner envelope: {err}"
        );
    }

    /// A generated chunk key must never clobber an existing root-like sibling.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_refuses_clobbering_a_root_like_chunk_sibling() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // The chunk keys the push will generate for this body.
        let body = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let generated = prepare_fastly_config_entries(TEST_CONFIG_ID, &body).expect("chunk");
        let chunk_key = generated
            .iter()
            .map(|(key, _)| key.clone())
            .find(|key| key.contains(CHUNK_KEY_INFIX))
            .expect("a generated chunk key");

        // Pre-seed that EXACT key with a root-like value (a valid direct envelope).
        let root_like = make_test_envelope(100);
        write_fastly_local_config_store(
            &fastly_toml,
            TEST_CONFIG_ID,
            &[(chunk_key.clone(), root_like)],
            &[],
        )
        .expect("seed a root-like value at a chunk-shaped key");

        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), body)],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("a generated chunk key clobbering a root-like sibling must abort");
        assert!(
            err.contains("refusing to push") && err.contains(&chunk_key),
            "must refuse and name the colliding chunk key: {err}"
        );
    }

    /// The dry-run count must EXCLUDE a prior chunk that is already absent from
    /// the file: the real prune's `remove()` is a no-op there, so counting it
    /// would over-report the number of deletions.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_excludes_already_missing_prior_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // Seed a multi-chunk generation.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed");

        // Manually delete ONE chunk entry: a prior chunk that is already gone.
        let mut doc: toml_edit::DocumentMut = fs::read_to_string(&fastly_toml)
            .expect("read")
            .parse()
            .expect("parse");
        let contents = doc["local_server"]["config_stores"][TEST_CONFIG_ID]["contents"]
            .as_table_mut()
            .expect("contents");
        let chunk_keys: Vec<String> = contents
            .iter()
            .map(|(key, _)| key.to_owned())
            .filter(|key| key.contains(CHUNK_KEY_INFIX))
            .collect();
        assert!(chunk_keys.len() >= 2, "seed must have chunked");
        contents.remove(&chunk_keys[0]);
        let present_after = chunk_keys.len().saturating_sub(1);
        fs::write(&fastly_toml, doc.to_string()).expect("write");

        // Dry-run a shrink-to-direct re-push: every remaining chunk is an orphan.
        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run");

        let reported = out
            .join("\n")
            .split("would delete ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse::<usize>().ok())
            .expect("a numeric orphan count");
        assert_eq!(
            reported, present_after,
            "the already-absent chunk must not be counted (reported {reported}, present {present_after})"
        );
    }

    /// Dry-run reports the orphan count and writes nothing.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_reports_orphan_count() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        let envelope_a = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_a)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("seed push");
        let before = fs::read_to_string(&fastly_toml).expect("read");

        let envelope_b = {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ "alt": "y".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:02Z".to_owned()))
                .expect("envelope B")
        };
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope_b)],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run");

        let combined = out.join("\n");
        assert!(
            combined.contains("would delete") && combined.contains("orphan chunks"),
            "dry-run must report orphan count: {combined}"
        );
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            before,
            "dry-run must not edit fastly.toml"
        );
    }

    /// PARITY: the dry-run's reported orphan count equals the number of chunk
    /// keys the real (non-dry-run) push actually deletes, on ONE fixture. A
    /// divergence would make the dry-run a misleading preview of the delete.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_count_matches_real_deletions() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;

        fn count_chunk_keys(toml_src: &str) -> usize {
            let doc: toml_edit::DocumentMut = toml_src.parse().expect("parse");
            doc.get("local_server")
                .and_then(|ls| ls.get("config_stores"))
                .and_then(|cs| cs.get(TEST_CONFIG_ID))
                .and_then(|st| st.get("contents"))
                .and_then(toml_edit::Item::as_table)
                .map_or(0, |table| {
                    table
                        .iter()
                        .filter(|(key, _)| key.contains(CHUNK_KEY_INFIX))
                        .count()
                })
        }
        fn parse_would_delete_count(text: &str) -> Option<usize> {
            let marker = "would delete ";
            let idx = text.find(marker)?;
            text.get(idx.saturating_add(marker.len())..)?
                .split_whitespace()
                .next()?
                .parse::<usize>()
                .ok()
        }

        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        // Seed a multi-chunk generation, then measure how many chunk keys exist.
        let chunked = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(5_000));
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
            .expect("seed push");
        let seeded = fs::read_to_string(&fastly_toml).expect("read");
        let prior_chunk_count = count_chunk_keys(&seeded);
        assert!(prior_chunk_count >= 2, "seed must have chunked: {seeded}");

        // Dry-run a shrink-to-direct re-push: capture the reported orphan count.
        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run");
        let reported = parse_would_delete_count(&out.join("\n"))
            .expect("dry-run must report a numeric orphan count");
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            seeded,
            "dry-run must not edit fastly.toml"
        );

        // Real re-push: count the chunk keys actually removed.
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                false,
            )
            .expect("real push");
        let after = fs::read_to_string(&fastly_toml).expect("read");
        let actually_deleted = prior_chunk_count.saturating_sub(count_chunk_keys(&after));

        assert_eq!(
            reported, actually_deleted,
            "dry-run count {reported} must equal real deletions {actually_deleted}"
        );
        assert_eq!(
            reported, prior_chunk_count,
            "a shrink-to-direct re-push orphans every prior chunk"
        );
    }

    /// Real (non-dry-run) push over a MALFORMED prior pointer WARNS and deletes
    /// nothing: its chunk list is untrustworthy, so no key is removed and the
    /// root is simply overwritten with the new value. This is the real-push
    /// counterpart to the dry-run "unknown" degradation on the same prior state.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_real_push_over_malformed_prior_warns_and_deletes_nothing() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        // A pointer-kind prior value missing its required fields — malformed, so
        // `prior_chunk_keys` returns Err (warn, delete nothing).
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("real push must not fail on a malformed prior");

        assert!(
            out.iter().any(|line| line.contains("skipping chunk GC")),
            "must warn about the malformed prior pointer: {out:?}"
        );

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");
        assert_eq!(
            contents
                .get(TEST_CONFIG_ID)
                .and_then(toml_edit::Item::as_str),
            Some(direct.as_str()),
            "root is overwritten with the new direct envelope: {after}"
        );
    }

    /// Dry-run of an identical re-push reports zero orphans (new keys
    /// equal prior keys — regression for expanding `new_keys`).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_identical_repush_counts_zero() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope.clone())],
                &AdapterPushContext::new(),
                false,
            )
            .expect("seed push");

        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), envelope)],
                &AdapterPushContext::new(),
                true, // dry_run, same bytes
            )
            .expect("dry-run");

        assert!(
            out.join("\n").contains("would delete 0 orphan chunks"),
            "identical re-push must count 0 orphans: {out:?}"
        );
    }

    /// Dry-run over a suspicious prior pointer reports an unknown count
    /// and does not fail.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_dry_run_suspicious_prior_pointer_unknown() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        let seed = concat!(
            "name = \"demo\"\n\n",
            "[local_server.config_stores.app_config]\n",
            "format = \"inline-toml\"\n\n",
            "[local_server.config_stores.app_config.contents]\n",
            "app_config = \"{\\\"edgezero_kind\\\":\\\"fastly_config_chunks\\\",\\\"version\\\":1}\"\n",
        );
        fs::write(&fastly_toml, seed).expect("seed");

        let direct = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let out = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(TEST_CONFIG_ID.to_owned(), direct)],
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not fail on suspicious pointer");

        assert!(
            out.join("\n").contains("unknown: suspicious prior pointer"),
            "dry-run must degrade to unknown: {out:?}"
        );
    }

    /// A duplicate root key in one batch is rejected before any I/O.
    /// Otherwise the earlier tuple's GC plan would reclaim the chunks the
    /// LAST tuple just installed, leaving the final pointer dangling.
    /// Regression: prior B, batch `[(root, A), (root, B)]` — the root must
    /// still resolve afterwards.
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_rejects_duplicate_root_keys() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        let make = |tag: &str| {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
                .expect("envelope")
        };
        let envelope_a = make("aaa");
        let envelope_b = make("bbb");

        // Prior generation B is live.
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
            .expect("seed push");
        let before = fs::read_to_string(&fastly_toml).expect("read");

        // Duplicate-root batch must be rejected outright.
        let err = FastlyCliAdapter
            .push_config_entries_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[
                    (TEST_CONFIG_ID.to_owned(), envelope_a),
                    (TEST_CONFIG_ID.to_owned(), envelope_b.clone()),
                ],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("duplicate root keys must be rejected");
        assert!(
            err.contains("more than once"),
            "error explains the duplicate: {err}"
        );
        assert_eq!(
            fs::read_to_string(&fastly_toml).expect("read"),
            before,
            "rejection must happen before any write"
        );

        // The live root still resolves to B (nothing was reclaimed).
        let read = FastlyCliAdapter
            .read_config_entry_local(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("root must still resolve");
        let ReadConfigEntry::Present(value) = read else {
            panic!("expected Present");
        };
        assert_eq!(value, envelope_b, "root still reconstructs envelope B");
    }

    /// GC of a chunked root must not touch a chunked SIBLING's chunks —
    /// the prefix `app_config.__edgezero_chunks.` must not match
    /// `app_config_staging.__edgezero_chunks.` (shared string prefix).
    #[cfg(unix)]
    #[test]
    fn push_config_entries_local_gc_preserves_sibling_chunks() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");
        let fastly_toml = dir.path().join("fastly.toml");
        seed_provisioned(&fastly_toml, TEST_CONFIG_ID);

        let make = |tag: &str| {
            use edgezero_core::blob_envelope::BlobEnvelope;
            use serde_json::json;
            let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
                .expect("envelope")
        };
        let push = |key: &str, body: String| {
            FastlyCliAdapter
                .push_config_entries_local(
                    dir.path(),
                    Some("fastly.toml"),
                    None,
                    &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                    &[(key.to_owned(), body)],
                    &AdapterPushContext::new(),
                    false,
                )
                .expect("push");
        };

        // app_config gen X, then a chunked sibling, then app_config gen Z.
        push("app_config", make("x1"));
        push("app_config_staging", make("staging"));
        let staging_chunks = chunk_keys_of("app_config_staging", &make("staging"));
        push("app_config", make("z2")); // GCs app_config's gen-X chunks

        let after = fs::read_to_string(&fastly_toml).expect("read");
        let doc: toml_edit::DocumentMut = after.parse().expect("parse");
        let contents = doc
            .get("local_server")
            .and_then(|ls| ls.get("config_stores"))
            .and_then(|cs| cs.get(TEST_CONFIG_ID))
            .and_then(|st| st.get("contents"))
            .and_then(toml_edit::Item::as_table)
            .expect("contents");
        for key in &staging_chunks {
            assert!(
                contents.get(key).is_some(),
                "sibling chunk `{key}` must survive app_config GC: {after}"
            );
        }
    }
}
