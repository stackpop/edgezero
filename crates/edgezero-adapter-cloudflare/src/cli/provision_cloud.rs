use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

use edgezero_adapter::registry::{AdapterDeployedState, ProvisionOutcome, ProvisionStores};

use super::provision_local::{
    check_kv_namespaces_writeback_shape, existing_real_namespace_id, read_namespace_id,
    upsert_kv_namespace,
};
use super::WRANGLER_INSTALL_HINT;

/// Cloud-mode `provision` arm: shells out to `wrangler kv namespace
/// create <binding>` for every declared KV / config store that isn't
/// already provisioned, then writes the returned id back into
/// `wrangler.toml` via [`upsert_kv_namespace`]. Secret stores are
/// runtime-managed via `wrangler secret put` — the Cloud arm reports
/// each declared secret but performs no side effect.
pub(super) fn provision(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    //: KV ids and config ids both back to Cloudflare KV
    // namespaces. Secrets are runtime-managed via
    // `wrangler secret put` — provision is a no-op for them.
    let Some(rel) = adapter_manifest_path else {
        return Err(
            "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for provision"
                .to_owned(),
        );
    };
    let wrangler_path = manifest_root.join(rel);

    let mut out = Vec::new();
    // Track logical -> namespace_id for freshly-created namespaces
    // so the CLI's writeback can persist them under
    // `[adapters.cloudflare.deployed].kv_namespaces.<logical>`.
    // Keyed by LOGICAL id so teammates' env overlays (which
    // change the platform binding name) still resolve the same
    // mapping on their side. Only populated in the non-dry-run
    // create branch below -- dry-runs and idempotency skips
    // contribute nothing (no real wrangler invocation, no id to
    // record).
    let mut created_kv_ns: BTreeMap<String, String> = BTreeMap::new();
    for store in stores.kv.iter().chain(stores.config.iter()) {
        let logical = &store.logical;
        // The Cloudflare KV binding name is what the runtime
        // calls `env.kv(...)` with -- it's resolved at request
        // time from `EDGEZERO__STORES__<KIND>__<LOGICAL>__NAME`
        // (default = logical id). Provision must write the
        // resolved PLATFORM name into wrangler.toml, otherwise
        // the runtime will look up a binding the CLI never
        // created.
        let binding = &store.platform;
        // Idempotency check BEFORE shelling out: if a
        // [[kv_namespaces]] entry with `binding = <platform>`
        // is already present and has a real namespace id, skip.
        // Without this guard a re-run of provision would invoke
        // `wrangler kv namespace create` again and orphan the
        // previously-created namespace -- wasting account quota.
        // A placeholder id (anything that isn't a 32-char
        // lowercase hex string, like the
        // `local-dev-placeholder` the scaffold wrangler.toml
        // writes) is treated as "not yet provisioned" so the
        // entry gets rewritten with the real id.
        //
        // We deliberately do NOT cross-check the stored id
        // against Cloudflare's API (e.g. by calling `wrangler
        // kv namespace list` to confirm the id still exists).
        // Verifying every entry on every provision run would
        // add a network round-trip per id and require parsing
        // yet another wrangler subcommand output. The skip
        // line names the existing id explicitly so the operator
        // can verify it themselves and, if the Cloudflare-side
        // namespace was deleted out-of-band, remove the stale
        // entry by hand before re-running provision.
        let existing = existing_real_namespace_id(&wrangler_path, binding)?;
        if let Some(existing_id) = existing {
            out.push(format!(
                "binding `{binding}` (logical id `{logical}`) already provisioned (id={existing_id} in {}); skipping. To force a fresh namespace: delete the [[kv_namespaces]] entry for binding `{binding}` AND run `wrangler kv namespace delete --namespace-id={existing_id}` (the old remote namespace lingers otherwise), then re-run provision.",
                wrangler_path.display()
            ));
            continue;
        }
        // Pre-flight the writeback shape BEFORE shelling
        // `wrangler kv namespace create`. `read_namespace_id`
        // tolerates both `[[kv_namespaces]]` (array-of-tables)
        // and `kv_namespaces = [{ binding = "...", id = "..." }]`
        // (inline-array) forms, but `upsert_kv_namespace` only
        // writes back through the array-of-tables shape. Without
        // this guard, an inline-array manifest passes the
        // "already provisioned?" probe (because no id is
        // present), the remote `create` succeeds, and then the
        // upsert errors out — leaving the freshly-created
        // namespace orphaned on Cloudflare with no local
        // writeback to track it.
        //
        // Refuse early so the operator fixes the manifest shape
        // BEFORE any account-side mutation.
        check_kv_namespaces_writeback_shape(&wrangler_path)?;
        if dry_run {
            out.push(format!(
                "would run `wrangler kv namespace create {binding}` and append [[kv_namespaces]] binding = \"{binding}\" to {} (logical id `{logical}`)",
                wrangler_path.display()
            ));
            continue;
        }
        let namespace_id = create_kv_namespace(binding)?;
        upsert_kv_namespace(&wrangler_path, binding, &namespace_id)?;
        out.push(format!(
            "created KV namespace `{binding}` (logical id `{logical}`, namespace id={namespace_id}); written to {}",
            wrangler_path.display()
        ));
        // Record under the LOGICAL id, not the platform binding.
        // Teammates' `provision --local` re-resolves logical ->
        // platform via THEIR env overlay and reads the namespace
        // id back via the same logical key -- keying by
        // `binding` (platform) would break that lookup when
        // the overlays diverge.
        created_kv_ns.insert(logical.clone(), namespace_id);
    }
    for store in stores.secrets {
        let logical = &store.logical;
        let platform = &store.platform;
        out.push(format!(
            "cloudflare secret `{platform}` (logical id `{logical}`) is runtime-managed via `wrangler secret put`; nothing to provision"
        ));
    }
    if out.is_empty() {
        out.push("cloudflare has no declared stores to provision".to_owned());
    }
    // dry_run branch above `continue`s BEFORE reaching
    // `create_kv_namespace`, so `created_kv_ns` stays empty for
    // dry-runs -- `deployed` collapses to `None` and the CLI
    // writeback is a no-op. An idempotent skip (binding already
    // present with a real id) similarly doesn't repopulate the
    // map, since the existing id is already recorded in the
    // operator's `[adapters.cloudflare.deployed]` block from a
    // prior run.
    let created_deployed = if created_kv_ns.is_empty() {
        None
    } else {
        let mut state = AdapterDeployedState::default();
        state
            .sub_tables
            .insert("kv_namespaces".to_owned(), created_kv_ns);
        Some(state)
    };
    Ok(ProvisionOutcome {
        status_lines: out,
        deployed: created_deployed,
    })
}

/// Shell out to `wrangler kv namespace create <binding>`, capture
/// stdout, and parse the resulting namespace id. The CLI's
/// `provision` command resolves this against the user's
/// `wrangler.toml` and writes the `[[kv_namespaces]]` entry.
///
/// # Errors
/// Returns an error if `wrangler` isn't on `PATH`, the child fails
/// to spawn, the exit status is non-zero, or stdout doesn't
/// include a parseable `id = "..."` line.
fn create_kv_namespace(binding: &str) -> Result<String, String> {
    let output = Command::new("wrangler")
        .args(["kv", "namespace", "create", binding])
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
            "`wrangler kv namespace create {binding}` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    extract_namespace_id(&stdout).ok_or_else(|| {
        format!(
            "wrangler created `{binding}` but stdout did not include a parseable `id = \"...\"` line -- wrangler may have changed its output format; pin a known-compatible wrangler version or file an issue. Raw stdout:\n{stdout}"
        )
    })
}

/// Pull the namespace id out of `wrangler kv namespace create`
/// stdout. Wrangler 3+ prints (something like):
///
/// ```text
/// 🌀 Creating namespace with title "..."
/// ✨ Success!
/// Add the following to your configuration file in your kv_namespaces array:
/// [[kv_namespaces]]
/// binding = "my-kv"
/// id = "abc123..."
/// ```
///
/// We tolerate leading whitespace + surrounding decoration. To
/// avoid grabbing a stray informational line like
/// `id = "<workspace_id>"` printed somewhere else in wrangler
/// output (or a hypothetical future `id = ...` line that names a
/// non-KV resource), we anchor to the `[[kv_namespaces]]` table
/// header AND require the value to be 32-char lowercase hex
/// (Cloudflare's actual namespace-id shape). The scan walks
/// lines top-down: when we see `[[kv_namespaces]]` we set a
/// scope flag; the next `id = "<32-char-hex>"` line within that
/// scope is the result. A new top-level header resets the scope.
fn extract_namespace_id(stdout: &str) -> Option<String> {
    let mut in_kv_namespaces = false;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed == "[[kv_namespaces]]" {
            in_kv_namespaces = true;
            continue;
        }
        // Any other table header ends the scope so we don't reach
        // forward into a sibling block.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_kv_namespaces = false;
            continue;
        }
        if !in_kv_namespaces {
            continue;
        }
        let Some(after_id_kw) = trimmed.strip_prefix("id") else {
            continue;
        };
        let Some(after_eq) = after_id_kw.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(quoted) = after_eq.trim_start().strip_prefix('"') else {
            continue;
        };
        let Some((id, _)) = quoted.split_once('"') else {
            continue;
        };
        if is_real_namespace_id(id) {
            return Some(id.to_owned());
        }
    }
    None
}

/// Heuristic: is `id` a real Cloudflare KV namespace id (32-char
/// lowercase hex), as opposed to a scaffold placeholder like
/// `local-dev-placeholder`? Cloudflare's API consistently returns
/// 32-char lowercase hex, so we use that as a tight cheap signal.
///
/// Additionally rejects hex-shape sentinels that LOOK like real
/// ids but are obviously hand-typed placeholders: anything with
/// fewer than 6 distinct hex characters (catches all-zeros,
/// all-`a`, `deadbeefdeadbeefdeadbeefdeadbeef`, etc.). A real id
/// generated by Cloudflare's API has effectively uniform random
/// hex distribution: expected distinct chars over 32 draws from
/// 16 symbols is ~14, and the dominant term P(=5 distinct) is on
/// the order of 10^-13 -- so false rejections of real ids are
/// astronomically unlikely.
pub(super) fn is_real_namespace_id(id: &str) -> bool {
    if id.len() != 32 {
        return false;
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return false;
    }
    // Distinct-byte count via a BTreeSet: 32 inserts is trivial,
    // and the set form avoids the arithmetic-side-effect /
    // silent-as / indexing-panic shapes the project's clippy
    // profile rejects.
    let distinct: BTreeSet<u8> = id.bytes().collect();
    distinct.len() >= 6
}

/// Look up the namespace id wrangler.toml has bound to `binding`,
/// rejecting placeholder ids (anything that isn't a 32-char
/// lowercase hex Cloudflare API id).
///
/// Accepts both `[[kv_namespaces]]` (array-of-tables, what
/// `provision` writes and wrangler's own post-create hint prints)
/// and the inline-array form. Returns Err with a "did you run
/// provision?" hint if the binding is absent OR holds a placeholder
/// like `local-dev-placeholder` — without this check `push` would
/// shell out to `wrangler kv bulk put --namespace-id=<placeholder>`,
/// which fails at wrangler with a less actionable error.
pub(super) fn find_namespace_id(wrangler_path: &Path, binding: &str) -> Result<String, String> {
    // read_namespace_id returns Ok(None) for both
    // missing-file AND binding-not-present; for `find_namespace_id`
    // the user wants a "did you run provision?" hint in both cases,
    // so collapse them into the same error message.
    let raw = read_namespace_id(wrangler_path, binding)?.ok_or_else(|| {
        format!(
            "{}: no [[kv_namespaces]] entry with binding = {binding:?} (did you run `edgezero provision --adapter cloudflare`?)",
            wrangler_path.display()
        )
    })?;
    if is_real_namespace_id(&raw) {
        Ok(raw)
    } else {
        Err(format!(
            "{}: binding {binding:?} has id {raw:?}, which doesn't look like a real Cloudflare KV namespace id (expected 32-char lowercase hex). This is usually a scaffold placeholder -- run `edgezero provision --adapter cloudflare` to create a real namespace and overwrite the entry.",
            wrangler_path.display()
        ))
    }
}

// `create_kv_namespace` is exercised indirectly via the
// `cloudflare_cloud_provision_returns_created_namespace_ids` test
// (which installs a fake `wrangler` shim on PATH and asserts
// against the parsed namespace id).
#[cfg(test)]
mod tests {
    use super::super::CloudflareCliAdapter;
    use super::*;
    use edgezero_adapter::registry::{
        Adapter as _, ProvisionMode, ProvisionStores, ResolvedStoreId,
    };
    #[cfg(unix)]
    use std::env;
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::Mutex;
    use tempfile::tempdir;

    const TEST_KV_ID: &str = "sessions";
    const TEST_KV_ID_ALT: &str = "cache";
    const TEST_CONFIG_ID: &str = "app_config";
    const TEST_SECRET_ID: &str = "default";

    #[cfg(unix)]
    struct PathPrepend {
        original: Option<OsString>,
    }

    #[cfg(unix)]
    impl PathPrepend {
        fn new(extra: &Path) -> Self {
            let original = env::var_os("PATH");
            let new = match &original {
                Some(prev) => {
                    let mut accum = OsString::from(extra);
                    accum.push(":");
                    accum.push(prev);
                    accum
                }
                None => OsString::from(extra),
            };
            env::set_var("PATH", new);
            Self { original }
        }
    }

    #[cfg(unix)]
    impl Drop for PathPrepend {
        fn drop(&mut self) {
            match self.original.take() {
                Some(prev) => env::set_var("PATH", prev),
                None => env::remove_var("PATH"),
            }
        }
    }

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
    fn path_mutation_guard() -> &'static Mutex<()> {
        use std::sync::OnceLock;
        static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
        GUARD.get_or_init(|| Mutex::new(()))
    }

    fn write_wrangler(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("wrangler.toml");
        fs::write(&path, contents).expect("write wrangler.toml");
        path
    }

    // ---------- extract_namespace_id ----------

    #[test]
    fn extract_namespace_id_parses_wrangler_3_output() {
        // wrangler decorates these lines with unicode glyphs in real
        // output; we drop them from the fixture to keep the source
        // file ASCII-only (clippy::non_ascii_literal). The parser
        // requires both the `[[kv_namespaces]]` anchor and a
        // 32-char-lowercase-hex id.
        let stdout = r#"Creating namespace with title "my-kv"
Success!
Add the following to your configuration file in your kv_namespaces array:
[[kv_namespaces]]
binding = "my-kv"
id = "00112233445566778899aabbccddeeff"
"#;
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn extract_namespace_id_tolerates_extra_whitespace() {
        let stdout = "[[kv_namespaces]]\n   id   =   \"00112233445566778899aabbccddeeff\"   \n";
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    #[test]
    fn extract_namespace_id_returns_none_on_missing_id_line() {
        assert!(extract_namespace_id("nothing to see here").is_none());
        assert!(extract_namespace_id("").is_none());
        assert!(
            extract_namespace_id("[[kv_namespaces]]\nid = \"\"").is_none(),
            "empty value not a real id"
        );
    }

    #[test]
    fn extract_namespace_id_ignores_unrelated_lines_starting_with_id() {
        // `identifier = "..."` doesn't match -- we strip exactly the
        // prefix `id` then require `=`. Also doesn't match because
        // there's no `[[kv_namespaces]]` anchor.
        assert!(extract_namespace_id("[[kv_namespaces]]\nidentifier = \"x\"").is_none());
    }

    #[test]
    fn extract_namespace_id_requires_kv_namespaces_anchor() {
        // A bare `id = "<32-char-hex>"` line that isn't preceded by
        // `[[kv_namespaces]]` must not match -- otherwise a future
        // wrangler info line like `id = "<workspace_id>"` printed
        // somewhere else in stdout would be picked up as the
        // namespace id and silently corrupt wrangler.toml on writeback.
        let unanchored = "id = \"00112233445566778899aabbccddeeff\"\n";
        assert!(extract_namespace_id(unanchored).is_none());

        // A different table header BEFORE the `id` line scopes us
        // out of the kv-namespaces context.
        let other_block = "[[d1_databases]]\nid = \"00112233445566778899aabbccddeeff\"\n";
        assert!(extract_namespace_id(other_block).is_none());
    }

    #[test]
    fn extract_namespace_id_rejects_non_real_id_inside_kv_namespaces_anchor() {
        // Even with the anchor, the value must look like a real
        // Cloudflare id (32-char lowercase hex with the diversity
        // floor). Shorter or non-hex values are skipped, not
        // returned -- forces the operator to investigate stdout
        // drift rather than silently writing a bogus id.
        let stdout = "[[kv_namespaces]]\nbinding = \"my-kv\"\nid = \"abc123\"\n";
        assert!(extract_namespace_id(stdout).is_none());
    }

    #[test]
    fn extract_namespace_id_returns_first_real_match_inside_kv_namespaces_anchor() {
        // Pin: top-down scan, first qualifying line inside the
        // `[[kv_namespaces]]` anchor wins. Real wrangler output has
        // exactly one. A hypothetical future format with multiple
        // qualifying lines would surface the earliest, but only
        // values that look like real Cloudflare ids count.
        let stdout = "[[kv_namespaces]]\n\
                      id = \"00112233445566778899aabbccddeeff\"\n\
                      id = \"ffeeddccbbaa99887766554433221100\"\n";
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
    }

    // ---------- is_real_namespace_id ----------

    #[test]
    fn is_real_namespace_id_accepts_32_char_lowercase_hex_with_sufficient_diversity() {
        // 16-distinct-char fixture: maximum diversity.
        assert!(is_real_namespace_id("00112233445566778899aabbccddeeff"));
        // Realistic randomish fixture: 14 distinct chars.
        assert!(is_real_namespace_id("4a8f3c2b9e1d5670adef2839c4b6e1f0"));
    }

    #[test]
    fn is_real_namespace_id_rejects_placeholder_or_short_id() {
        assert!(!is_real_namespace_id("local-dev-placeholder"));
        assert!(!is_real_namespace_id("abc123"));
        assert!(!is_real_namespace_id(""));
    }

    #[test]
    fn is_real_namespace_id_rejects_uppercase_or_non_hex() {
        // Uppercase rejected: Cloudflare's API returns lowercase.
        assert!(!is_real_namespace_id("00112233445566778899AABBCCDDEEFF"));
        // Non-hex digits rejected.
        assert!(!is_real_namespace_id("z0112233445566778899aabbccddeeff"));
    }

    #[test]
    fn is_real_namespace_id_rejects_hex_shape_sentinels() {
        // 32-char lowercase hex but obvious hand-typed placeholder:
        // distinct-hex-digit count is below the diversity floor.
        // Real Cloudflare ids have effectively uniform random hex,
        // so collisions with this guard are astronomical.
        assert!(
            !is_real_namespace_id("00000000000000000000000000000000"),
            "all-zeros rejected"
        );
        assert!(
            !is_real_namespace_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "all-a rejected"
        );
        assert!(
            !is_real_namespace_id("deadbeefdeadbeefdeadbeefdeadbeef"),
            "deadbeef rejected (only 5 distinct chars: d,e,a,b,f)"
        );
        // Boundary: a real-looking id with the diversity floor or
        // more must still pass.
        assert!(
            is_real_namespace_id("00112233445566778899aabbccddeeff"),
            "16-distinct-char fixture must still pass"
        );
        // Exactly 6 distinct chars (a,b,c,d,e,f): on the boundary,
        // must pass.
        assert!(
            is_real_namespace_id("aabbccddeeffaabbccddeeffaabbccdd"),
            "6-distinct-char fixture (boundary) passes"
        );
    }

    // ---------- provision (dry-run + error path) ----------

    #[test]
    fn provision_dry_run_does_not_invoke_wrangler() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let kv_ids: Vec<ResolvedStoreId> =
            ResolvedStoreId::from_logicals(&[TEST_KV_ID, TEST_KV_ID_ALT]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let secret_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_SECRET_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        // 2 KV + 1 config + 1 secret = 4 status lines.
        assert_eq!(out.status_lines.len(), 4);
        assert!(out.status_lines[0].contains("would run `wrangler kv namespace create sessions`"));
        assert!(out.status_lines[1].contains("would run `wrangler kv namespace create cache`"));
        assert!(out.status_lines[2].contains("would run `wrangler kv namespace create app_config`"));
        assert!(out.status_lines[3].contains("runtime-managed via `wrangler secret put`"));
        // Manifest untouched.
        let after = fs::read_to_string(dir.path().join("wrangler.toml")).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated wrangler.toml");
    }

    #[test]
    fn provision_dry_run_writes_resolved_platform_name_into_binding() {
        // Regression: provision used to receive only logical ids
        // and write them verbatim into wrangler.toml. With the
        // platform-name flow, an operator who sets
        // `EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config`
        // sees `prod_config` land as the binding name (matching what
        // the runtime resolves via `env.kv(...)`), with the logical
        // id still mentioned for human-facing wording.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let config_ids = vec![ResolvedStoreId::new(TEST_CONFIG_ID, "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("wrangler kv namespace create prod_config"),
            "dry-run uses platform name in the `wrangler` invocation: {out:?}"
        );
        assert!(
            out.status_lines[0].contains("binding = \"prod_config\""),
            "dry-run writes platform name as the binding: {out:?}"
        );
        assert!(
            out.status_lines[0].contains("logical id `app_config`"),
            "logical id is preserved for operator wording: {out:?}"
        );
    }

    #[test]
    fn provision_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = CloudflareCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml"),
            "error names what's missing: {err}"
        );
    }

    #[test]
    fn provision_dry_run_skips_bindings_already_provisioned_with_real_id() {
        let dir = tempdir().expect("tempdir");
        // 32-char lowercase hex id == real Cloudflare namespace id.
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("already provisioned")
                && out.status_lines[0].contains("00112233445566778899aabbccddeeff"),
            "skip line names the existing id: {out:?}"
        );
        let after = fs::read_to_string(&path).expect("read");
        assert!(
            after.contains("00112233445566778899aabbccddeeff"),
            "did not touch existing id: {after}"
        );
    }

    #[test]
    fn provision_dry_run_treats_placeholder_id_as_unprovisioned() {
        // A scaffolded wrangler.toml ships with placeholder ids the
        // user is expected to overwrite by running provision.
        // Dry-run should report the would-be create call, NOT the
        // already-provisioned skip.
        let dir = tempdir().expect("tempdir");
        write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"local-dev-placeholder\"\n",
        );
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.status_lines.len(), 1);
        assert!(
            out.status_lines[0].contains("would run `wrangler kv namespace create sessions`"),
            "placeholder id is treated as unprovisioned: {out:?}"
        );
    }

    #[test]
    fn provision_with_no_declared_stores_says_so() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("no-store provision is fine");
        assert_eq!(
            out.status_lines,
            vec!["cloudflare has no declared stores to provision"]
        );
        // No wrangler was invoked (no stores) => no id to record.
        assert!(
            out.deployed.is_none(),
            "no-store provision has nothing to write back: {:?}",
            out.deployed
        );
    }

    #[cfg(unix)]
    #[test]
    fn cloudflare_cloud_provision_returns_created_namespace_ids() {
        // Non-dry-run Cloud provision must populate
        // `deployed.sub_tables["kv_namespaces"]` keyed by LOGICAL id
        // (not the platform binding name). Task 16's CLI writeback
        // then lands them under `[adapters.cloudflare.deployed]`.
        //
        // Uses the same wrangler-fake shim pattern as the
        // read_config_entry tests: a shell script on PATH prints the
        // Wrangler-3 `[[kv_namespaces]] / id = "..."` block that
        // `extract_namespace_id` parses.
        let _lock = path_mutation_guard().lock().expect("guard");
        let project_dir = tempdir().expect("tempdir");
        write_wrangler(project_dir.path(), "name = \"demo\"\n");
        let stdout = "[[kv_namespaces]]\nbinding = \"ignored-by-parser\"\nid = \"00112233445566778899aabbccddeeff\"\n";
        let fake = fake_wrangler_returning(stdout, "", 0);
        let _path = PathPrepend::new(fake.path());

        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                project_dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .expect("cloud provision succeeds against fake wrangler");
        let deployed = out
            .deployed
            .expect("cloud provision with creates populates deployed");
        let kv = deployed
            .sub_tables
            .get("kv_namespaces")
            .expect("deployed carries kv_namespaces sub-table");
        // Key MUST be the LOGICAL id -- teammates' env overlays
        // change the platform binding, but the logical id is
        // env-overlay-independent.
        assert_eq!(
            kv.get(TEST_KV_ID).map(String::as_str),
            Some("00112233445566778899aabbccddeeff"),
            "kv_namespaces keyed by logical id `{TEST_KV_ID}`: {kv:?}"
        );
    }

    #[test]
    fn cloudflare_cloud_provision_dry_run_returns_none_deployed() {
        // Cloud dry-run means no real `wrangler kv namespace create`
        // invocation happened -- no real id to record. `deployed`
        // must be `None` so the CLI writeback is a no-op.
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let kv_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_KV_ID]);
        let config_ids: Vec<ResolvedStoreId> = ResolvedStoreId::from_logicals(&[TEST_CONFIG_ID]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &[],
        };
        let out = CloudflareCliAdapter
            .provision(
                dir.path(),
                Some("wrangler.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                true,
            )
            .expect("dry-run succeeds");
        assert!(
            out.deployed.is_none(),
            "dry-run must not populate deployed (no wrangler ran): {:?}",
            out.deployed
        );
    }

    // ---------- find_namespace_id ----------

    #[test]
    fn find_namespace_id_reads_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let id = find_namespace_id(&path, TEST_CONFIG_ID).expect("found");
        assert_eq!(id, "00112233445566778899aabbccddeeff");
    }

    #[test]
    fn find_namespace_id_reads_inline_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\nkv_namespaces = [{ binding = \"app_config\", id = \"ffeeddccbbaa99887766554433221100\" }]\n",
        );
        let id = find_namespace_id(&path, TEST_CONFIG_ID).expect("found");
        assert_eq!(id, "ffeeddccbbaa99887766554433221100");
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"other\"\nid = \"00112233445566778899aabbccddeeff\"\n",
        );
        let err = find_namespace_id(&path, TEST_CONFIG_ID).expect_err("missing must error");
        assert!(
            err.contains(TEST_CONFIG_ID) && err.contains("provision"),
            "error names the binding and points at provision: {err}"
        );
    }

    #[test]
    fn find_namespace_id_rejects_placeholder_id_with_provision_hint() {
        // A binding with `id = "local-dev-placeholder"` (or any
        // other non-32-char-hex value) is treated the same as
        // a missing binding: the operator needs to run provision
        // before the id is usable for `wrangler kv bulk put`.
        // Without this guard, push would shell out with the
        // placeholder as `--namespace-id=...` and fail at wrangler
        // with a less actionable error.
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"local-dev-placeholder\"\n",
        );
        let err =
            find_namespace_id(&path, TEST_CONFIG_ID).expect_err("placeholder id must be rejected");
        assert!(
            err.contains("local-dev-placeholder") && err.contains("provision"),
            "error names the placeholder and points at provision: {err}"
        );
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let err =
            find_namespace_id(&path, TEST_CONFIG_ID).expect_err("missing wrangler.toml must error");
        assert!(
            err.contains("provision"),
            "error points at provision: {err}"
        );
    }
}
