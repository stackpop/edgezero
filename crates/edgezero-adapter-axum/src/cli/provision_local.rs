use std::fs;
use std::path::Path;

use edgezero_adapter::env_file::{append_lines_dedup_with_header, EDGEZERO_PROVISION_HEADER};
use edgezero_adapter::registry::{ProvisionOutcome, ProvisionStores};

/// Local-mode `provision` arm.
///
/// Axum's baseline `axum.toml` is written by
/// `Adapter::synthesise_baseline_manifest` (see `cli/mod.rs`); the
/// merge path here doesn't touch the manifest because Axum has no
/// per-machine identifiers to weave in on re-provision. Once
/// synthesised, operator edits (custom host / port / `crate_dir`)
/// survive re-runs byte-identical.
///
/// The only thing this fn writes is the `.edgezero/.env` file the
/// runtime reads at boot: `__NAME` lines seed the
/// store->platform-name map for every declared kind (KV / CONFIG /
/// SECRETS), and commented `__KEY` placeholders for CONFIG stores
/// let the operator uncomment them to switch to a staging blob
/// without hand-remembering the full env-var name.
///
/// The `.edgezero/` directory anchors at `manifest_root`.
///
/// Dedup — including commented/uncommented cross-form dedup — is
/// delegated to [`append_lines_dedup`] so operator overrides survive
/// re-runs.
pub(super) fn provision(
    manifest_root: &Path,
    stores: &ProvisionStores<'_>,
    dry_run: bool,
) -> Result<ProvisionOutcome, String> {
    let dot_edgezero = manifest_root.join(".edgezero");
    if !dry_run {
        fs::create_dir_all(&dot_edgezero)
            .map_err(|err| format!("create {}: {err}", dot_edgezero.display()))?;
    }
    let env_path = dot_edgezero.join(".env");
    let env_lines = build_axum_env_lines(stores);
    append_lines_dedup_with_header(
        &env_path,
        Some(EDGEZERO_PROVISION_HEADER),
        &env_lines,
        dry_run,
    )
    .map_err(|err| format!("write {}: {err}", env_path.display()))?;
    let status_lines = vec![format!(
        "axum: ensured {} + appended {} lines to {}",
        dot_edgezero.display(),
        env_lines.len(),
        env_path.display()
    )];
    Ok(ProvisionOutcome::from_status_lines(status_lines))
}

/// Build the `.env` line set emitted by [`provision_local`].
///
/// - One `EDGEZERO__STORES__<KIND>__<LOGICAL_UPPER>__NAME=<platform>`
///   line per store, for every kind (KV, CONFIG, SECRETS).
/// - One commented `# EDGEZERO__STORES__CONFIG__<LOGICAL_UPPER>__KEY=<logical>_staging`
///   placeholder per CONFIG store, so the operator can uncomment to
///   switch blobs without remembering the exact env-var name.
///
/// Env-var KEY uses the LOGICAL id upper-cased so the runtime env
/// overlay finds it regardless of a teammate's per-store platform
/// override. Env-var VALUE uses the PLATFORM name so the runtime
/// resolves the same backend the rest of the toolchain (Cloudflare,
/// Fastly, Spin, and here the Axum local file store) points at.
fn build_axum_env_lines(stores: &ProvisionStores<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for (kind, kind_stores) in [
        ("KV", stores.kv),
        ("CONFIG", stores.config),
        ("SECRETS", stores.secrets),
    ] {
        for store in kind_stores {
            let logical_upper = store.logical.to_ascii_uppercase();
            let platform = &store.platform;
            lines.push(format!(
                "EDGEZERO__STORES__{kind}__{logical_upper}__NAME={platform}"
            ));
        }
    }
    for store in stores.config {
        let logical_upper = store.logical.to_ascii_uppercase();
        let logical = &store.logical;
        lines.push(format!(
            "# EDGEZERO__STORES__CONFIG__{logical_upper}__KEY={logical}_staging"
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::AxumCliAdapter;
    use edgezero_adapter::registry::{
        Adapter as _, ProvisionMode, ProvisionStores, ResolvedStoreId,
    };
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn axum_local_provision_creates_dot_edgezero_dir() {
        // Empty fixture — no `.edgezero/` yet, no stores declared.
        // Local provision must still create the directory so the
        // runtime always sees a well-known location for the `.env`
        // file it reads at boot.
        let dir = tempdir().unwrap();
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        assert!(
            dir.path().join(".edgezero").is_dir(),
            ".edgezero/ must exist after local provision"
        );
    }

    #[test]
    fn axum_local_provision_preserves_existing_axum_toml_content() {
        // Contract: when axum.toml already exists (operator has
        // edited host/port or other fields), provision's MERGE path
        // must NOT rewrite it. The synthesise_baseline_manifest hook
        // only writes when the file is missing (write_baseline_to_disk
        // skips existing files); the provision merge itself is a
        // no-op on axum.toml because Axum has no cloud identifiers
        // to weave in. Operator edits therefore survive re-runs
        // byte-identical.
        let dir = tempdir().unwrap();
        let axum_toml = dir.path().join("axum.toml");
        let operator_content =
            "# operator-edited\n[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"0.0.0.0\"\nport = 3000\n";
        fs::write(&axum_toml, operator_content).unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                Some("axum.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read_to_string(&axum_toml).unwrap();
        assert_eq!(
            after, operator_content,
            "existing axum.toml must be byte-for-byte unchanged after re-provision"
        );
    }

    #[test]
    fn axum_local_provision_writes_env_name_lines() {
        // For every declared store id (all kinds), a `__NAME` line
        // seeds the runtime store->platform-name map. CONFIG stores
        // also get a commented `__KEY` placeholder the operator can
        // uncomment to switch to a staging blob.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "kv __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default"),
            "secrets __NAME line present: {env}"
        );
        assert!(
            env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "commented __KEY placeholder present for CONFIG only: {env}"
        );
    }

    #[test]
    fn axum_local_provision_dedup_preserves_operator_env_overrides() {
        // Operator already uncommented + edited the __KEY override.
        // A re-provision must NOT re-add the commented placeholder,
        // and must NOT clobber the operator's live value.
        let dir = tempdir().unwrap();
        let dot_edgezero = dir.path().join(".edgezero");
        fs::create_dir_all(&dot_edgezero).unwrap();
        let env_path = dot_edgezero.join(".env");
        fs::write(
            &env_path,
            "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override\n",
        )
        .unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(&env_path).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=operator_override"),
            "operator override preserved: {env}"
        );
        assert!(
            !env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="),
            "commented placeholder must NOT be re-added: {env}"
        );
    }

    #[test]
    fn axum_local_provision_uses_platform_name_when_env_overlay_active() {
        // Simulates
        //   EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config
        // in effect at CLI time via ResolvedStoreId::new(logical,
        // platform). The emitted __NAME line's VALUE must be the
        // env-resolved platform (`prod_config`); the ENV-VAR KEY
        // must still use the LOGICAL id upper-cased (`APP_CONFIG`)
        // so the runtime env overlay finds it. Same discipline as
        // Cloudflare Task 19.
        let dir = tempdir().unwrap();
        let config_ids = vec![ResolvedStoreId::new("app_config", "prod_config")];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=prod_config"),
            "value uses PLATFORM, env-var key uses LOGICAL: {env}"
        );
        assert!(
            !env.contains("EDGEZERO__STORES__CONFIG__PROD_CONFIG__NAME="),
            "platform name must NOT leak into the env-var key: {env}"
        );
    }

    #[test]
    fn axum_local_provision_cloud_mode_is_a_no_op() {
        // Cloud mode: the pre-existing status-line-only arm stays in
        // charge; nothing gets written to disk, and `.edgezero/` must
        // NOT be auto-created. The load-bearing assertion here is
        // the negative one — the Local arm's file work must not leak
        // into Cloud mode.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        let outcome = AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Cloud,
                false,
            )
            .unwrap();
        assert!(
            !dir.path().join(".edgezero").exists(),
            "cloud mode must NOT auto-create .edgezero/"
        );
        assert!(
            !outcome.status_lines.is_empty(),
            "cloud arm still emits informational status lines"
        );
    }

    #[test]
    fn provision_local_creates_dot_edgezero_dir() {
        // Empty fixture: `.edgezero/` does not pre-exist and no stores
        // are declared. Local provision must still create the directory
        // so the runtime has a well-known location to read the `.env`
        // file from at boot.
        let dir = tempdir().unwrap();
        assert!(
            !dir.path().join(".edgezero").exists(),
            "sanity: .edgezero/ must NOT pre-exist"
        );
        let stores = ProvisionStores {
            config: &[],
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        assert!(
            dir.path().join(".edgezero").is_dir(),
            ".edgezero/ must exist as a directory after local provision"
        );
    }

    #[test]
    fn provision_local_preserves_existing_axum_toml() {
        // Renamed from `provision_local_does_not_touch_axum_toml`
        // (2026-07 refactor). Axum's manifest joined the provision-
        // generated set when `synthesise_baseline_manifest` was wired
        // up. Provision synthesises a baseline `axum.toml` only when
        // the file is missing (via `write_baseline_to_disk`); the
        // adapter's merge path is a no-op because Axum has no cloud
        // identifiers. Operator edits therefore survive re-runs
        // byte-identical -- lock this with a distinctive sentinel.
        let dir = tempdir().unwrap();
        let axum_toml = dir.path().join("axum.toml");
        let sentinel =
            b"# operator-edited\n[adapter]\ncrate = \"demo\"\ncrate_dir = \".\"\nhost = \"0.0.0.0\"\nport = 9090\n";
        fs::write(&axum_toml, sentinel).unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                Some("axum.toml"),
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read(&axum_toml).unwrap();
        assert_eq!(
            after,
            sentinel.to_vec(),
            "existing axum.toml must be byte-for-byte unchanged after re-provision"
        );
    }

    #[test]
    fn provision_local_writes_env_name_lines() {
        // Fixture: one store per kind. Local provision must:
        //   - write `.edgezero/.env` starting with the provenance
        //     header (Section 5 review fix — `# edgezero-provision: v1`);
        //   - emit one `__NAME` line per kind (KV / CONFIG / SECRETS);
        //   - emit a commented `__KEY` placeholder for CONFIG only.
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let kv_ids = ResolvedStoreId::from_logicals(&["sessions"]);
        let secret_ids = ResolvedStoreId::from_logicals(&["default"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env = fs::read_to_string(dir.path().join(".edgezero/.env")).unwrap();
        assert!(
            env.starts_with("# edgezero-provision: v1"),
            ".env must start with the provenance header: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__CONFIG__APP_CONFIG__NAME=app_config"),
            "config __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__KV__SESSIONS__NAME=sessions"),
            "kv __NAME line present: {env}"
        );
        assert!(
            env.contains("EDGEZERO__STORES__SECRETS__DEFAULT__NAME=default"),
            "secrets __NAME line present: {env}"
        );
        assert!(
            env.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "commented __KEY placeholder present for CONFIG only: {env}"
        );
    }

    #[test]
    fn re_provision_preserves_operator_env_edits() {
        // First provision writes the base `.edgezero/.env` (including
        // the commented `__KEY` placeholder). The operator uncomments
        // AND edits the line to point at their own override value.
        // Re-running provision must NOT re-add the commented form and
        // MUST leave the operator's uncommented line byte-identical
        // (Task 16c dedup semantics — key-normalised uncommented
        // form wins over any commented sibling).
        let dir = tempdir().unwrap();
        let config_ids = ResolvedStoreId::from_logicals(&["app_config"]);
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &[],
            secrets: &[],
        };
        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let env_path = dir.path().join(".edgezero/.env");
        let first = fs::read_to_string(&env_path).unwrap();
        assert!(
            first.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging"),
            "first-run must seed the commented placeholder: {first}"
        );

        // Operator uncomments AND edits the value.
        let operator_line = "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=my_local_override";
        let edited = first.replace(
            "# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging",
            operator_line,
        );
        fs::write(&env_path, &edited).unwrap();

        AxumCliAdapter
            .provision(
                dir.path(),
                None,
                None,
                &stores,
                None,
                ProvisionMode::Local,
                false,
            )
            .unwrap();
        let after = fs::read_to_string(&env_path).unwrap();
        let matching: Vec<&str> = after
            .lines()
            .filter(|line| *line == operator_line)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "operator's uncommented override line must survive byte-identical: {after}"
        );
        assert!(
            !after.contains("# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY="),
            "commented placeholder must NOT be re-added when uncommented form exists: {after}"
        );
    }
}
