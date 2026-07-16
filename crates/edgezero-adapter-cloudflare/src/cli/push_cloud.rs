use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use edgezero_adapter::registry::{ReadConfigEntry, ResolvedStoreId};

use super::WRANGLER_INSTALL_HINT;
use super::provision_cloud::find_namespace_id;

/// Push `entries` to the remote KV namespace bound to `store` (looked
/// up in `wrangler.toml`) via `wrangler kv bulk put <tempfile.json>
/// --namespace-id=<id> --remote`. **--remote** is mandatory — wrangler
/// v4 defaults to LOCAL storage otherwise.
///
/// Dry-run reports the intended invocation + per-entry preview without
/// resolving the namespace id strictly (operators can preview the
/// keyset BEFORE running provision). Real runs err loudly on unresolved
/// bindings.
pub(super) fn write_entries(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    // Read namespace id from wrangler.toml (matched by
    // `binding = <platform>`), then `wrangler kv bulk put
    // <tempfile.json> --namespace-id=<id> --remote`. The
    // CLI hands this writer one logical (root_key, envelope_json)
    // entry; the bulk-put still works because it's one upsert
    // per entry, and the one-entry case is degenerate.
    //
    // **--remote** is mandatory for the prod-push path:
    // wrangler v4 defaults KV bulk-put to LOCAL storage when
    // the command supports both — meaning a v4 user running
    // `wrangler kv bulk put` without `--remote` would silently
    // populate Miniflare state under `.wrangler/state` and
    // report success while leaving the live Cloudflare
    // namespace empty. Explicit `--remote` removes the
    // ambiguity.
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config push"
                .to_owned(),
        );
    };
    let wrangler_path = manifest_root.join(rel);
    let binding = store.platform.as_str();
    let logical = store.logical.as_str();
    // Dry-run is lenient about a missing/unresolved binding so
    // operators can preview the keyset BEFORE running provision.
    // Real runs still err loudly so we don't silently push to
    // a non-existent namespace.
    if dry_run {
        let header = find_namespace_id(&wrangler_path, binding).map_or_else(
            |_| format!(
                "would run `wrangler kv bulk put <tempfile.json> --namespace-id=<unresolved> --remote` with {} entries for binding `{binding}` (logical id `{logical}`, binding not yet provisioned -- run `edgezero provision --adapter cloudflare` to resolve the namespace id)",
                entries.len()
            ),
            |ns_id| format!(
                "would run `wrangler kv bulk put <tempfile.json> --namespace-id={ns_id} --remote` with {} entries for binding `{binding}` (logical id `{logical}`)",
                entries.len()
            ),
        );
        let mut out = vec![header];
        for (key, _) in entries {
            out.push(format!("  would create entry `{key}`"));
        }
        return Ok(out);
    }
    let namespace_id = find_namespace_id(&wrangler_path, binding)?;
    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to KV namespace `{binding}` (logical id `{logical}`, id={namespace_id})"
        )]);
    }
    let payload = bulk_payload(entries)?;
    let temp = tempfile::Builder::new()
        .prefix("edgezero-cf-push-")
        .suffix(".json")
        .tempfile()
        .map_err(|err| format!("failed to create temp file for wrangler bulk payload: {err}"))?;
    fs::write(temp.path(), payload.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", temp.path().display()))?;
    let temp_arg = temp
        .path()
        .to_str()
        .ok_or_else(|| format!("temp file path {} is not UTF-8", temp.path().display()))?;
    let namespace_arg = format!("--namespace-id={namespace_id}");
    // Run from the wrangler.toml's directory so wrangler picks
    // up its `account_id` / `--env` resolution + persistence
    // settings the same way `wrangler dev` / `wrangler deploy`
    // do for this project.
    let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
    let output = Command::new("wrangler")
        .current_dir(project_dir)
        .args([
            "kv",
            "bulk",
            "put",
            temp_arg,
            namespace_arg.as_str(),
            "--remote",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
            } else {
                format!("failed to spawn `wrangler`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`wrangler kv bulk put --remote` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(vec![format!(
        "pushed {} entries to KV namespace `{binding}` (logical id `{logical}`, id={namespace_id})",
        entries.len()
    )])
}

/// Push `entries` to Miniflare's local KV storage via `wrangler kv
/// bulk put <file> --binding <BINDING> --local`.
///
/// Local mode does NOT resolve a namespace id — the scaffold ships
/// with `local-dev-placeholder` ids, so operators who haven't run
/// `edgezero provision` yet can still seed `.wrangler/state` from the
/// manifest. Wrangler stores local entries keyed by binding, not
/// namespace id, so `wrangler dev --local` / `edgezero serve --adapter
/// cloudflare` reads them back through the same binding name.
pub(super) fn write_entries_local(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    // Local push: address the binding directly via
    // `wrangler kv bulk put <file> --binding <BINDING> --local`.
    // Crucially we do NOT resolve a namespace id here — the
    // scaffold ships with `local-dev-placeholder` ids, so an
    // operator that hasn't run `edgezero provision` yet should
    // still be able to seed `.wrangler/state` from the manifest
    // (matching wrangler's own local KV docs). Wrangler stores
    // local entries keyed by binding, not namespace id, so the
    // follow-up `wrangler dev --local` / `edgezero serve
    // --adapter cloudflare` reads them back through the same
    // binding name.
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config push --local"
                .to_owned(),
        );
    };
    let wrangler_path = manifest_root.join(rel);
    let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
    let binding = store.platform.as_str();
    let logical = store.logical.as_str();
    if dry_run {
        let mut out = vec![format!(
            "would run `wrangler kv bulk put <tempfile.json> --binding {binding} --local` with {} entries for binding `{binding}` (logical id `{logical}`)",
            entries.len()
        )];
        for (key, _) in entries {
            out.push(format!("  would create local entry `{key}`"));
        }
        return Ok(out);
    }
    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to local KV namespace `{binding}` (logical id `{logical}`)"
        )]);
    }
    let payload = bulk_payload(entries)?;
    let temp = tempfile::Builder::new()
        .prefix("edgezero-cf-push-local-")
        .suffix(".json")
        .tempfile()
        .map_err(|err| format!("failed to create temp file for wrangler bulk payload: {err}"))?;
    fs::write(temp.path(), payload.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", temp.path().display()))?;
    let temp_arg = temp
        .path()
        .to_str()
        .ok_or_else(|| format!("temp file path {} is not UTF-8", temp.path().display()))?;
    let output = Command::new("wrangler")
        .current_dir(project_dir)
        .args([
            "kv",
            "bulk",
            "put",
            temp_arg,
            "--binding",
            binding,
            "--local",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
            } else {
                format!("failed to spawn `wrangler`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`wrangler kv bulk put --binding {binding} --local` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(vec![format!(
        "pushed {} entries to local KV namespace bound as `{binding}` (logical id `{logical}`); `.wrangler/state` updated",
        entries.len()
    )])
}

/// Render the entries as the `[{"key": "...", "value": "..."}, …]`
/// JSON wrangler expects for `kv bulk put`. Under the blob model the
/// CLI hands this writer one logical `(root_key, envelope_json)` entry;
/// Cloudflare passes the value through unchanged (the envelope is an
/// opaque string from the platform's perspective).
fn bulk_payload(entries: &[(String, String)]) -> Result<String, String> {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|(key, value)| serde_json::json!({ "key": key, "value": value }))
        .collect();
    serde_json::to_string(&payload)
        .map_err(|err| format!("failed to serialize wrangler bulk payload: {err}"))
}

/// Read a single key from a Cloudflare KV namespace by shelling out to
/// `wrangler kv key get --binding <BINDING> <KEY> <locality>`.
///
/// `locality` is either `"--remote"` (live Cloudflare KV) or `"--local"`
/// (Miniflare `.wrangler/state`). The two read methods on the adapter call
/// this shared helper with the appropriate flag.
///
/// # Mapping to `ReadConfigEntry`
/// - Success (exit 0) → `Present(stdout)`.
/// - Exit non-zero, stderr contains "not found" / "does not exist" → `MissingKey`.
/// - Exit non-zero, stderr mentions "binding" → `MissingStore` (the KV
///   namespace binding itself doesn't exist in `wrangler.toml`).
/// - Any other non-zero exit → `Err`.
pub(super) fn read_wrangler_kv_key(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    store: &ResolvedStoreId,
    key: &str,
    locality: &str,
) -> Result<ReadConfigEntry, String> {
    let rel = adapter_manifest_path.ok_or_else(|| {
        "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config diff"
            .to_owned()
    })?;
    let wrangler_path = manifest_root.join(rel);
    let binding = store.platform.as_str();
    let project_dir = wrangler_path.parent().unwrap_or(manifest_root);
    let output = Command::new("wrangler")
        .args(["kv", "key", "get", "--binding", binding, key, locality])
        .current_dir(project_dir)
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`wrangler` not found on PATH; {WRANGLER_INSTALL_HINT}")
            } else {
                format!("failed to spawn `wrangler`: {err}")
            }
        })?;
    if output.status.success() {
        let body = String::from_utf8(output.stdout)
            .map_err(|err| format!("`wrangler kv key get` stdout is not UTF-8: {err}"))?;
        // Wrangler 4.x (verified 4.64.0) returns exit 0 + stdout
        // "Value not found" for a missing key instead of exit 1 +
        // stderr. Detect that shape and map to MissingKey -- a
        // missing key in the blob model is valid initial state
        // (first push hasn't run yet), not corrupt remote state.
        // Match the trimmed first line so trailing newlines or
        // future variants like "Value not found.\n" still match.
        let trimmed = body.trim();
        if trimmed.eq_ignore_ascii_case("value not found")
            || trimmed.eq_ignore_ascii_case("value not found.")
        {
            return Ok(ReadConfigEntry::MissingKey);
        }
        return Ok(ReadConfigEntry::Present(body));
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not found") || stderr.contains("does not exist") {
        return Ok(ReadConfigEntry::MissingKey);
    }
    if stderr.contains("binding") || stderr.contains("Binding") {
        return Ok(ReadConfigEntry::MissingStore);
    }
    Err(format!(
        "`wrangler kv key get --binding {binding} {key} {locality}` exited with status {}\nstderr: {}",
        output.status,
        stderr.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::super::CloudflareCliAdapter;
    #[cfg(unix)]
    use super::super::path_mutation_guard;
    use super::*;
    use edgezero_adapter::registry::{
        Adapter as _, AdapterPushContext, ReadConfigEntry, ResolvedStoreId,
    };
    use edgezero_core::test_env::PathPrepend;
    use std::path::PathBuf;
    use tempfile::tempdir;

    const TEST_CONFIG_ID: &str = "app_config";

    #[cfg(unix)]
    fn fake_wrangler_returning(
        stdout_body: &str,
        stderr_body: &str,
        exit_code: i32,
    ) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("wrangler");
        let stdout_file = dir.path().join("stdout_payload.txt");
        let stderr_file = dir.path().join("stderr_payload.txt");
        fs::write(&stdout_file, stdout_body).expect("write stdout payload");
        fs::write(&stderr_file, stderr_body).expect("write stderr payload");
        let script = format!(
            "#!/bin/sh\ncat '{stdout}'\ncat '{stderr}' >&2\nexit {code}\n",
            stdout = stdout_file.display(),
            stderr = stderr_file.display(),
            code = exit_code,
        );
        fs::write(&script_path, script).expect("write wrangler script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    #[cfg(unix)]
    fn fake_wrangler_argv_log(out_path: &Path) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("wrangler");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{out}'; done\nprintf 'val'\n",
            out = out_path.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    fn write_wrangler(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("wrangler.toml");
        fs::write(&path, contents).expect("write wrangler.toml");
        path
    }

    // ---------- bulk_payload ----------

    #[test]
    fn bulk_payload_emits_wrangler_array_of_key_value_objects() {
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("service.timeout_ms".to_owned(), "1500".to_owned()),
        ];
        let raw = bulk_payload(&entries).expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let array = parsed.as_array().expect("array");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["key"], "greeting");
        assert_eq!(array[0]["value"], "hello");
        assert_eq!(array[1]["key"], "service.timeout_ms");
        assert_eq!(array[1]["value"], "1500");
    }

    #[test]
    fn bulk_payload_with_no_entries_is_empty_array() {
        let raw = bulk_payload(&[]).expect("empty payload");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(parsed, serde_json::json!([]));
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_resolves_namespace_id_and_does_not_invoke_wrangler() {
        let dir = tempdir().expect("tempdir");
        let original = "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n";
        let path = write_wrangler(dir.path(), original);
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        // Header + per-entry preview, matching the fastly dry-run shape.
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would run `wrangler kv bulk put")
                && out[0].contains("--namespace-id=00112233445566778899aabbccddeeff"),
            "dry-run header names namespace id: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run lists `greeting`: {out:?}"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("`feature.new_checkout`")),
            "dry-run lists `feature.new_checkout`: {out:?}"
        );
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, original, "dry-run must not mutate wrangler.toml");
    }

    #[test]
    fn push_dry_run_is_lenient_when_binding_not_yet_provisioned() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run is lenient: pre-provision preview is allowed");
        assert!(
            out[0].contains("<unresolved>") && out[0].contains("provision"),
            "dry-run header explains the namespace is unresolved and points at provision: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run still lists the entries it would push: {out:?}"
        );
    }

    #[test]
    fn push_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("k".to_owned(), "v".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml") && err.contains("config push"),
            "error explains the missing manifest pointer: {err}"
        );
    }

    #[test]
    fn push_real_run_errors_with_provision_hint_when_binding_absent() {
        // dry-run is now lenient (see
        // `push_dry_run_is_lenient_when_binding_not_yet_provisioned`),
        // but a real run still must err so we don't silently push
        // to a non-existent namespace.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("missing binding must error on real run");
        assert!(
            err.contains("provision") && err.contains(TEST_CONFIG_ID),
            "error points at provision: {err}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_after_resolving_namespace() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries")
                && out[0].contains("00112233445566778899aabbccddeeff"),
            "status line names empty + namespace id: {out:?}"
        );
    }

    // ---------- read_config_entry / read_config_entry_local (fake wrangler) ----------

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_present_on_success() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("hello-cloudflare", "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("wrangler exit-0 must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, "hello-cloudflare");
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_not_found_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("", "Error: key not found", 1);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("not-found maps to MissingKey (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "not-found stderr => MissingKey"
        );
    }

    /// Wrangler 4.x (verified 4.64.0) returns exit 0 + stdout
    /// `"Value not found"` for a missing key instead of exit 1 +
    /// stderr. The previous read path treated every exit-0 stdout
    /// as a `Present` envelope, which made the next CLI step try
    /// to parse `"Value not found"` as a `BlobEnvelope` and abort.
    /// A missing key in the blob model is valid initial state --
    /// the first push hasn't run yet -- not corrupt remote state,
    /// so it must map to `MissingKey`.
    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_on_wrangler_4_value_not_found_stdout() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("Value not found\n", "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("Wrangler 4.x exit-0 'Value not found' must map to MissingKey");
        if let ReadConfigEntry::Present(body) = &result {
            panic!(
                "expected MissingKey on Wrangler 4.x 'Value not found' stdout; \
                 got Present({body:?})",
            );
        }
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "Wrangler 4.x stdout='Value not found' (exit 0) must classify as MissingKey",
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_on_binding_stderr() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let fake = fake_wrangler_returning("", "Error: binding APP_CONFIG is not defined", 1);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("binding-error maps to MissingStore (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "binding stderr => MissingStore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_local_uses_local_flag() {
        // Verify that read_config_entry_local passes `--local` (not `--remote`)
        // to wrangler. We capture argv via a fake wrangler and check the args.
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let argv_log = project_dir.path().join("argv.txt");
        let fake = fake_wrangler_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());
        let result = CloudflareCliAdapter
            .read_config_entry_local(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("local read succeeds");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "expected Present from local read"
        );
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            captured.contains("--local"),
            "read_local must pass --local to wrangler; got argv:\n{captured}"
        );
        assert!(
            !captured.contains("--remote"),
            "read_local must NOT pass --remote; got argv:\n{captured}"
        );
    }

    #[test]
    fn read_config_entry_requires_adapter_manifest_path() {
        let dir = tempdir().expect("tempdir");
        let result = CloudflareCliAdapter.read_config_entry(
            dir.path(),
            None,
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        match result {
            Err(err) => assert!(
                err.contains("[adapters.cloudflare.adapter].manifest"),
                "error names the missing field: {err}"
            ),
            Ok(_) => panic!("expected Err when adapter_manifest_path is None"),
        }
    }
}
