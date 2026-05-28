use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use ctor::ctor;
use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name, run_native_cli,
};
use edgezero_adapter::registry::{register_adapter, Adapter, AdapterAction, ProvisionStores};
use edgezero_adapter::scaffold::{
    register_adapter_blueprint, AdapterBlueprint, AdapterFileSpec, CommandTemplates,
    DependencySpec, LoggingDefaults, ManifestSpec, ReadmeInfo, TemplateRegistration,
};
use walkdir::WalkDir;

static CLOUDFLARE_ADAPTER: CloudflareCliAdapter = CloudflareCliAdapter;

static CLOUDFLARE_BLUEPRINT: AdapterBlueprint = AdapterBlueprint {
    id: "cloudflare",
    display_name: "Cloudflare Workers",
    crate_suffix: "adapter-cloudflare",
    dependency_crate: "edgezero-adapter-cloudflare",
    dependency_repo_path: "crates/edgezero-adapter-cloudflare",
    template_registrations: CLOUDFLARE_TEMPLATE_REGISTRATIONS,
    files: CLOUDFLARE_FILE_SPECS,
    extra_dirs: &["src", ".cargo"],
    dependencies: CLOUDFLARE_DEPENDENCIES,
    manifest: ManifestSpec {
        manifest_filename: "wrangler.toml",
        build_target: "wasm32-unknown-unknown",
        build_profile: "release",
        build_features: &["cloudflare"],
    },
    commands: CommandTemplates {
        build: "wrangler build --cwd {crate_dir}",
        deploy: "wrangler deploy --cwd {crate_dir}",
        serve: "wrangler dev --cwd {crate_dir}",
    },
    logging: LoggingDefaults {
        endpoint: None,
        level: "info",
        echo_stdout: None,
    },
    readme: ReadmeInfo {
        description: "{display} entrypoint.",
        dev_heading: "{display} (local)",
        dev_steps: &["`edgezero serve --adapter cloudflare`"],
    },
    run_module: "edgezero_adapter_cloudflare",
};

static CLOUDFLARE_DEPENDENCIES: &[DependencySpec] = &[
    DependencySpec {
        key: "dep_edgezero_core_cloudflare",
        repo_crate: "crates/edgezero-core",
        fallback: "edgezero-core = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-core\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_cloudflare",
        repo_crate: "crates/edgezero-adapter-cloudflare",
        fallback:
            "edgezero-adapter-cloudflare = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-cloudflare\", default-features = false }",
        features: &[],
    },
    DependencySpec {
        key: "dep_edgezero_adapter_cloudflare_wasm",
        repo_crate: "crates/edgezero-adapter-cloudflare",
        fallback:
            "edgezero-adapter-cloudflare = { git = \"https://git@github.com/stackpop/edgezero.git\", package = \"edgezero-adapter-cloudflare\", default-features = false, features = [\"cloudflare\"] }",
        features: &["cloudflare"],
    },
];

static CLOUDFLARE_FILE_SPECS: &[AdapterFileSpec] = &[
    AdapterFileSpec {
        template: "cf_Cargo_toml",
        output: "Cargo.toml",
    },
    AdapterFileSpec {
        template: "cf_src_lib_rs",
        output: "src/lib.rs",
    },
    AdapterFileSpec {
        template: "cf_src_main_rs",
        output: "src/main.rs",
    },
    AdapterFileSpec {
        template: "cf_cargo_config_toml",
        output: ".cargo/config.toml",
    },
    AdapterFileSpec {
        template: "cf_wrangler_toml",
        output: "wrangler.toml",
    },
];

static CLOUDFLARE_TEMPLATE_REGISTRATIONS: &[TemplateRegistration] = &[
    TemplateRegistration {
        name: "cf_Cargo_toml",
        contents: include_str!("templates/Cargo.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_lib_rs",
        contents: include_str!("templates/src/lib.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_src_main_rs",
        contents: include_str!("templates/src/main.rs.hbs"),
    },
    TemplateRegistration {
        name: "cf_cargo_config_toml",
        contents: include_str!("templates/.cargo/config.toml.hbs"),
    },
    TemplateRegistration {
        name: "cf_wrangler_toml",
        contents: include_str!("templates/wrangler.toml.hbs"),
    },
];

const TARGET_TRIPLE: &str = "wasm32-unknown-unknown";

const WRANGLER_INSTALL_HINT: &str =
    "install the Cloudflare CLI (`npm install -g wrangler`) and try again";

struct CloudflareCliAdapter;

#[expect(
    clippy::missing_trait_methods,
    reason = "cloudflare has no validate_app_config_keys / validate_adapter_manifest / validate_typed_secrets requirements; the trait defaults already model that"
)]
impl Adapter for CloudflareCliAdapter {
    fn execute(&self, action: AdapterAction, args: &[String]) -> Result<(), String> {
        match action {
            // `wrangler` is the native sign-in surface for Cloudflare
            // Workers. EdgeZero stores no credentials — this is a thin
            // shell-out.
            AdapterAction::AuthLogin => {
                run_native_cli("wrangler", &["login"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::AuthLogout => {
                run_native_cli("wrangler", &["logout"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::AuthStatus => {
                run_native_cli("wrangler", &["whoami"], WRANGLER_INSTALL_HINT)
            }
            AdapterAction::Build => build(args).map(|artifact| {
                log::info!(
                    "[edgezero] Cloudflare build artifact -> {}",
                    artifact.display()
                );
            }),
            AdapterAction::Deploy => deploy(args),
            AdapterAction::Serve => serve(args),
            other => Err(format!("cloudflare adapter does not support {other:?}")),
        }
    }

    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn provision(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        stores: &ProvisionStores<'_>,
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
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
        for id in stores.kv.iter().chain(stores.config.iter()) {
            if dry_run {
                out.push(format!(
                    "would run `wrangler kv namespace create {id}` and append [[kv_namespaces]] binding = \"{id}\" to {}",
                    wrangler_path.display()
                ));
                continue;
            }
            let namespace_id = create_kv_namespace(id)?;
            append_kv_namespace(&wrangler_path, id, &namespace_id)?;
            out.push(format!(
                "created KV namespace `{id}` (id={namespace_id}); appended to {}",
                wrangler_path.display()
            ));
        }
        for id in stores.secrets {
            out.push(format!(
                "cloudflare secret `{id}` is runtime-managed via `wrangler secret put`; nothing to provision"
            ));
        }
        if out.is_empty() {
            out.push("cloudflare has no declared stores to provision".to_owned());
        }
        Ok(out)
    }

    fn push_config_entries(
        &self,
        manifest_root: &Path,
        adapter_manifest_path: Option<&str>,
        _component_selector: Option<&str>,
        store_id: &str,
        entries: &[(String, String)],
        dry_run: bool,
    ) -> Result<Vec<String>, String> {
        //: read namespace id from wrangler.toml (matched by
        // `binding = <store_id>`), then `wrangler kv bulk put
        // <tempfile.json> --namespace-id=<id>`. Keys in dotted
        // form — the CLI already flattened them.
        let Some(rel) = adapter_manifest_path else {
            return Err(
                "[adapters.cloudflare.adapter].manifest must point at wrangler.toml for config push"
                    .to_owned(),
            );
        };
        let wrangler_path = manifest_root.join(rel);
        let namespace_id = find_namespace_id(&wrangler_path, store_id)?;
        if entries.is_empty() {
            return Ok(vec![format!(
                "no config entries to push to KV namespace `{store_id}` (id={namespace_id})"
            )]);
        }
        if dry_run {
            return Ok(vec![format!(
                "would run `wrangler kv bulk put <tempfile.json> --namespace-id={namespace_id}` with {} entries for binding `{store_id}`",
                entries.len()
            )]);
        }
        let payload = bulk_payload(entries)?;
        let temp = tempfile::Builder::new()
            .prefix("edgezero-cf-push-")
            .suffix(".json")
            .tempfile()
            .map_err(|err| {
                format!("failed to create temp file for wrangler bulk payload: {err}")
            })?;
        fs::write(temp.path(), payload.as_bytes())
            .map_err(|err| format!("failed to write {}: {err}", temp.path().display()))?;
        let temp_arg = temp
            .path()
            .to_str()
            .ok_or_else(|| format!("temp file path {} is not UTF-8", temp.path().display()))?;
        let namespace_arg = format!("--namespace-id={namespace_id}");
        let output = Command::new("wrangler")
            .args(["kv", "bulk", "put", temp_arg, namespace_arg.as_str()])
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
                "`wrangler kv bulk put` exited with status {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(vec![format!(
            "pushed {} entries to KV namespace `{store_id}` (id={namespace_id})",
            entries.len()
        )])
    }

    fn single_store_kinds(&self) -> &'static [&'static str] {
        //: cloudflare is Multi for KV (KV namespaces) and
        // Config (KV namespaces), Single for Secrets (Worker
        // Secrets is a single flat bag).
        &["secrets"]
    }
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
            "wrangler created `{binding}` but stdout did not include a parseable `id = \"...\"` line; raw output:\n{stdout}"
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
/// We tolerate leading whitespace + surrounding decoration; the
/// only contract is a line containing `id` `=` `"<value>"`.
fn extract_namespace_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim();
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
        if !id.is_empty() {
            return Some(id.to_owned());
        }
    }
    None
}

/// Append a `[[kv_namespaces]]` block to the user's `wrangler.toml`
/// (creating the array if absent). Existing entries are preserved;
/// if a binding with the same name is already present this is a
/// no-op (idempotent across re-runs).
fn append_kv_namespace(path: &Path, binding: &str, id: &str) -> Result<(), String> {
    use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table, Value};

    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;

    // Accept both representations for the idempotency check so a
    // re-run silently skips even if the user happens to use the
    // inline-array form. We only force array-of-tables on insert.
    let already_present = match doc.get("kv_namespaces") {
        Some(Item::ArrayOfTables(arr)) => arr
            .iter()
            .any(|table| table.get("binding").and_then(Item::as_str) == Some(binding)),
        Some(Item::Value(Value::Array(arr))) => arr.iter().any(|item| {
            item.as_inline_table()
                .and_then(|table| table.get("binding"))
                .and_then(Value::as_str)
                == Some(binding)
        }),
        Some(_) | None => false,
    };
    if already_present {
        return Ok(());
    }

    let entry = doc
        .entry("kv_namespaces")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let arr_of_tables = entry.as_array_of_tables_mut().ok_or_else(|| {
        format!(
            "{}: `kv_namespaces` exists but is not an array-of-tables (`[[kv_namespaces]]`); convert it manually before re-running provision",
            path.display()
        )
    })?;

    let mut new_table = Table::new();
    new_table.insert("binding", value(binding));
    new_table.insert("id", value(id));
    arr_of_tables.push(new_table);

    fs::write(path, doc.to_string())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(())
}

/// Render the entries as the `[{"key": "...", "value": "..."}, …]`
/// JSON wrangler expects for `kv bulk put`. Keys arrive pre-flattened
/// from the CLI (dotted form,); cloudflare passes them through.
fn bulk_payload(entries: &[(String, String)]) -> Result<String, String> {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|(key, value)| serde_json::json!({ "key": key, "value": value }))
        .collect();
    serde_json::to_string(&payload)
        .map_err(|err| format!("failed to serialize wrangler bulk payload: {err}"))
}

/// # Errors
/// Returns an error if the Cloudflare wrangler build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let cargo_manifest = manifest_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "--target",
            TARGET_TRIPLE,
            "--manifest-path",
            cargo_manifest
                .to_str()
                .ok_or("invalid Cargo manifest path")?,
        ])
        .args(extra_args)
        .status()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }

    let workspace_root = find_workspace_root(manifest_dir);
    let artifact = locate_artifact(&workspace_root, manifest_dir, &crate_name)?;
    let pkg_dir = workspace_root.join("pkg");
    fs::create_dir_all(&pkg_dir)
        .map_err(|err| format!("failed to create {}: {err}", pkg_dir.display()))?;
    let dest = pkg_dir.join(format!("{}.wasm", crate_name.replace('-', "_")));
    fs::copy(&artifact, &dest)
        .map_err(|err| format!("failed to copy artifact to {}: {err}", dest.display()))?;

    Ok(dest)
}

/// # Errors
/// Returns an error if the Cloudflare wrangler deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["deploy", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler deploy failed with status {status}"));
    }

    Ok(())
}

/// Look up the namespace id wrangler.toml has bound to `binding`.
/// Accepts both `[[kv_namespaces]]` (array-of-tables, what
/// `provision` writes and wrangler's own post-create hint prints)
/// and the inline-array form. Returns Err with a "did you run
/// provision?" hint if the binding is absent — the most common
/// cause of this error is forgetting to provision first.
fn find_namespace_id(wrangler_path: &Path, binding: &str) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Value};

    let raw = fs::read_to_string(wrangler_path).map_err(|err| {
        format!(
            "failed to read {}: {err} (did you run `edgezero provision --adapter cloudflare`?)",
            wrangler_path.display()
        )
    })?;
    let doc: DocumentMut = raw
        .parse()
        .map_err(|err| format!("failed to parse {}: {err}", wrangler_path.display()))?;
    let id = match doc.get("kv_namespaces") {
        Some(Item::ArrayOfTables(arr)) => arr.iter().find_map(|table| {
            if table.get("binding").and_then(Item::as_str) == Some(binding) {
                table.get("id").and_then(Item::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(Item::Value(Value::Array(arr))) => arr.iter().find_map(|item| {
            let table = item.as_inline_table()?;
            if table.get("binding").and_then(Value::as_str) == Some(binding) {
                table.get("id").and_then(Value::as_str).map(str::to_owned)
            } else {
                None
            }
        }),
        Some(_) | None => None,
    };
    id.ok_or_else(|| {
        format!(
            "{}: no [[kv_namespaces]] entry with binding = {binding:?} (did you run `edgezero provision --adapter cloudflare`?)",
            wrangler_path.display()
        )
    })
}

fn find_wrangler_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "wrangler.toml") {
        return Ok(found);
    }

    let root = find_workspace_root(start);
    let mut candidates: Vec<PathBuf> = WalkDir::new(&root)
        .follow_links(true)
        .max_depth(8)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_path_buf())
        .filter(|path| {
            path.file_name().is_some_and(|n| n == "wrangler.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate wrangler.toml".to_owned());
    }

    candidates.sort_by_key(|path| {
        let parent = path.parent().unwrap_or(Path::new(""));
        path_distance(start, parent)
    });

    Ok(candidates.remove(0))
}

fn locate_artifact(
    workspace_root: &Path,
    manifest_dir: &Path,
    crate_name: &str,
) -> Result<PathBuf, String> {
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    if let Some(custom) = env::var_os("CARGO_TARGET_DIR") {
        let candidate = PathBuf::from(custom)
            .join(TARGET_TRIPLE)
            .join("release")
            .join(&release_name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let manifest_target = manifest_dir
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if manifest_target.exists() {
        return Ok(manifest_target);
    }

    let workspace_target = workspace_root
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(&release_name);
    if workspace_target.exists() {
        return Ok(workspace_target);
    }

    Err(format!(
        "compiled artifact not found for {crate_name} (looked in manifest and workspace target directories)"
    ))
}

#[inline]
pub fn register() {
    register_adapter(&CLOUDFLARE_ADAPTER);
    register_adapter_blueprint(&CLOUDFLARE_BLUEPRINT);
}

#[ctor(unsafe)]
fn register_ctor() {
    register();
}

/// # Errors
/// Returns an error if the Cloudflare wrangler dev command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_wrangler_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "wrangler manifest has no parent directory".to_owned())?;
    let config = manifest
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?;

    let status = Command::new("wrangler")
        .args(["dev", "--config", config])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler dev failed with status {status}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------- extract_namespace_id ----------

    #[test]
    fn extract_namespace_id_parses_wrangler_3_output() {
        // wrangler decorates these lines with unicode glyphs in real
        // output; we drop them from the fixture to keep the source
        // file ASCII-only (clippy::non_ascii_literal). The parser
        // only cares about the literal `id = "..."` line.
        let stdout = r#"Creating namespace with title "my-kv"
Success!
Add the following to your configuration file in your kv_namespaces array:
[[kv_namespaces]]
binding = "my-kv"
id = "abc123def456"
"#;
        assert_eq!(
            extract_namespace_id(stdout).as_deref(),
            Some("abc123def456")
        );
    }

    #[test]
    fn extract_namespace_id_tolerates_extra_whitespace() {
        let stdout = "   id   =   \"xyz789\"   \n";
        assert_eq!(extract_namespace_id(stdout).as_deref(), Some("xyz789"));
    }

    #[test]
    fn extract_namespace_id_returns_none_on_missing_id_line() {
        assert!(extract_namespace_id("nothing to see here").is_none());
        assert!(extract_namespace_id("").is_none());
        assert!(
            extract_namespace_id("id = \"\"").is_none(),
            "empty value not a real id"
        );
    }

    #[test]
    fn extract_namespace_id_ignores_unrelated_lines_starting_with_id() {
        // A line like `identifier = "..."` shouldn't match — we
        // strip exactly the prefix `id` then require `=`.
        assert!(extract_namespace_id("identifier = \"x\"").is_none());
    }

    // ---------- append_kv_namespace ----------

    fn write_wrangler(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("wrangler.toml");
        fs::write(&path, contents).expect("write wrangler.toml");
        path
    }

    #[test]
    fn append_kv_namespace_adds_block_to_minimal_file() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(dir.path(), "name = \"my-worker\"\n");
        append_kv_namespace(&path, "sessions", "abc123").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("[[kv_namespaces]]"),
            "added array entry: {after}"
        );
        assert!(
            after.contains("binding = \"sessions\""),
            "binding present: {after}"
        );
        assert!(after.contains("id = \"abc123\""), "id present: {after}");
        assert!(
            after.contains("name = \"my-worker\""),
            "preserved original keys: {after}"
        );
    }

    #[test]
    fn append_kv_namespace_appends_to_existing_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"cache\"\nid = \"old\"\n",
        );
        append_kv_namespace(&path, "sessions", "abc123").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("binding = \"cache\""),
            "existing entry kept: {after}"
        );
        assert!(
            after.contains("binding = \"sessions\""),
            "new entry added: {after}"
        );
        assert_eq!(
            after.matches("[[kv_namespaces]]").count(),
            2,
            "two entries: {after}"
        );
    }

    #[test]
    fn append_kv_namespace_is_idempotent_on_duplicate_binding() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "[[kv_namespaces]]\nbinding = \"sessions\"\nid = \"existing\"\n",
        );
        append_kv_namespace(&path, "sessions", "new-id").expect("idempotent append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("id = \"existing\""),
            "did not overwrite existing id: {after}"
        );
        assert_eq!(
            after.matches("binding = \"sessions\"").count(),
            1,
            "no duplicate binding: {after}"
        );
    }

    #[test]
    fn append_kv_namespace_preserves_top_comments() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "# managed by hand -- please keep this line\nname = \"my-worker\"\n",
        );
        append_kv_namespace(&path, "sessions", "abc123").expect("append");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("# managed by hand"),
            "preserved comment: {after}"
        );
    }

    // ---------- provision (dry-run + error path) ----------

    #[test]
    fn provision_dry_run_does_not_invoke_wrangler() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let kv_ids = vec!["sessions".to_owned(), "cache".to_owned()];
        let config_ids = vec!["app_config".to_owned()];
        let secret_ids = vec!["default".to_owned()];
        let stores = ProvisionStores {
            config: &config_ids,
            kv: &kv_ids,
            secrets: &secret_ids,
        };
        let out = CloudflareCliAdapter
            .provision(dir.path(), Some("wrangler.toml"), None, &stores, true)
            .expect("dry-run succeeds");
        // 2 KV + 1 config + 1 secret = 4 status lines.
        assert_eq!(out.len(), 4);
        assert!(out[0].contains("would run `wrangler kv namespace create sessions`"));
        assert!(out[1].contains("would run `wrangler kv namespace create cache`"));
        assert!(out[2].contains("would run `wrangler kv namespace create app_config`"));
        assert!(out[3].contains("runtime-managed via `wrangler secret put`"));
        // Manifest untouched.
        let after = fs::read_to_string(dir.path().join("wrangler.toml")).expect("read");
        assert_eq!(after, "name = \"demo\"\n", "dry-run mutated wrangler.toml");
    }

    #[test]
    fn provision_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let kv_ids = vec!["sessions".to_owned()];
        let stores = ProvisionStores {
            config: &[],
            kv: &kv_ids,
            secrets: &[],
        };
        let err = CloudflareCliAdapter
            .provision(dir.path(), None, None, &stores, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml"),
            "error names what's missing: {err}"
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
            .provision(dir.path(), Some("wrangler.toml"), None, &stores, false)
            .expect("no-store provision is fine");
        assert_eq!(out, vec!["cloudflare has no declared stores to provision"]);
    }

    // ---------- find_namespace_id ----------

    #[test]
    fn find_namespace_id_reads_array_of_tables() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"abc123\"\n",
        );
        let id = find_namespace_id(&path, "app_config").expect("found");
        assert_eq!(id, "abc123");
    }

    #[test]
    fn find_namespace_id_reads_inline_array() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\nkv_namespaces = [{ binding = \"app_config\", id = \"xyz789\" }]\n",
        );
        let id = find_namespace_id(&path, "app_config").expect("found");
        assert_eq!(id, "xyz789");
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"other\"\nid = \"abc\"\n",
        );
        let err = find_namespace_id(&path, "app_config").expect_err("missing must error");
        assert!(
            err.contains("app_config") && err.contains("provision"),
            "error names the binding and points at provision: {err}"
        );
    }

    #[test]
    fn find_namespace_id_errors_with_provision_hint_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let err =
            find_namespace_id(&path, "app_config").expect_err("missing wrangler.toml must error");
        assert!(
            err.contains("provision"),
            "error points at provision: {err}"
        );
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
        let original =
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"abc123\"\n";
        let path = write_wrangler(dir.path(), original);
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                "app_config",
                &entries,
                true,
            )
            .expect("dry-run succeeds");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("would run `wrangler kv bulk put")
                && out[0].contains("--namespace-id=abc123"),
            "dry-run line names namespace id: {out:?}"
        );
        let after = fs::read_to_string(&path).expect("read");
        assert_eq!(after, original, "dry-run must not mutate wrangler.toml");
    }

    #[test]
    fn push_errors_when_adapter_manifest_path_missing() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![("k".to_owned(), "v".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(dir.path(), None, None, "app_config", &entries, true)
            .expect_err("missing adapter manifest path must error");
        assert!(
            err.contains("wrangler.toml") && err.contains("config push"),
            "error explains the missing manifest pointer: {err}"
        );
    }

    #[test]
    fn push_errors_with_provision_hint_when_binding_absent() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(dir.path(), "name = \"demo\"\n");
        let entries = vec![("greeting".to_owned(), "hello".to_owned())];
        let err = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                "app_config",
                &entries,
                true,
            )
            .expect_err("missing binding must error");
        assert!(
            err.contains("provision") && err.contains("app_config"),
            "error points at provision: {err}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_after_resolving_namespace() {
        let dir = tempdir().expect("tempdir");
        write_wrangler(
            dir.path(),
            "name = \"demo\"\n[[kv_namespaces]]\nbinding = \"app_config\"\nid = \"abc123\"\n",
        );
        let out = CloudflareCliAdapter
            .push_config_entries(
                dir.path(),
                Some("wrangler.toml"),
                None,
                "app_config",
                &[],
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries") && out[0].contains("abc123"),
            "status line names empty + namespace id: {out:?}"
        );
    }
}
