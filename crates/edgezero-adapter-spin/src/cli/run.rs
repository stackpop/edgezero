//! Vendor CLI subprocess wrappers for the Spin adapter: `build`,
//! `deploy`, `serve`, plus the manifest / artifact discovery and the
//! `synthesise_*_toml` baselines emitted by the CLI's `provision`
//! bootstrap.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use edgezero_adapter::cli_support::{
    find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use walkdir::WalkDir;

const TARGET_TRIPLE: &str = "wasm32-wasip2";

/// # Errors
/// Returns an error if the Spin CLI build command fails.
#[inline]
pub fn build(extra_args: &[String]) -> Result<PathBuf, String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;
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
/// Returns an error if the Spin CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["deploy"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin deploy failed with status {status}"));
    }

    Ok(())
}

fn find_spin_manifest(start: &Path) -> Result<PathBuf, String> {
    if let Some(found) = find_manifest_upwards(start, "spin.toml") {
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
            path.file_name().is_some_and(|n| n == "spin.toml")
                && path
                    .parent()
                    .is_some_and(|dir| dir.join("Cargo.toml").exists())
        })
        .collect();

    if candidates.is_empty() {
        return Err("could not locate spin.toml".to_owned());
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
        "compiled artifact not found (looked in {} and workspace target)",
        manifest_dir.display()
    ))
}

/// # Errors
/// Returns an error if the Spin CLI up command fails.
#[inline]
pub fn serve(extra_args: &[String]) -> Result<(), String> {
    let manifest =
        find_spin_manifest(env::current_dir().map_err(|err| err.to_string())?.as_path())?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let status = Command::new("spin")
        .args(["up"])
        .args(extra_args)
        .current_dir(manifest_dir)
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

/// Header-only baseline for `runtime-config.toml`. Task 25's
/// local arm appends `[key_value_store.<name>]` blocks on top of
/// this baseline; there is nothing to synthesise structurally at
/// bootstrap time — the header line pins the schema version so
/// later appenders know they are editing an EdgeZero-owned file.
pub(crate) fn synthesise_runtime_config_toml() -> String {
    String::from("# edgezero-provision: v1\n")
}

/// Synthesised baseline `spin.toml` for scaffold-time and clean-clone
/// bootstrap (single source — the Spin blueprint has no scaffold
/// `.hbs` template for `spin.toml`, so scaffold and clean-clone
/// produce byte-identical output; see the "Generated Adapter
/// manifests" note in the spec).
///
/// Built via `toml_edit::DocumentMut` (NOT raw `format!`) so any
/// legal `[app].name` or `[adapters.spin.adapter].component`
/// selector — including values with TOML-significant characters
/// like `"`, `\`, or newlines — is escaped correctly.
///
/// Component-id resolution: prefer the explicit
/// `[adapters.spin.adapter].component` selector when set. When
/// unset, fall back to the scaffold-convention crate name
/// `<app_name>-adapter-spin` (matching what
/// `SPIN_BLUEPRINT.crate_suffix` produces at `edgezero new` time)
/// so the resolved `[component.<id>].source` path names the
/// wasm artifact Cargo actually builds. The pre-2026-07 fallback
/// to bare `app_name` produced e.g. `demo_app.wasm` while Cargo
/// produced `demo_app_adapter_spin.wasm`, so a clean clone's
/// regenerated `spin.toml` pointed at a non-existent artifact.
///
/// The wasm source path uses the UNDERSCORED component id because
/// Rust's Cargo output filenames convert hyphens to underscores
/// (`my-crate` → `my_crate.wasm`).
pub(crate) fn synthesise_spin_toml(app_name: &str, component: Option<&str>) -> String {
    use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

    let derived_component;
    let component_id: &str = if let Some(cid) = component {
        cid
    } else {
        derived_component = if app_name.is_empty() {
            "app-adapter-spin".to_owned()
        } else {
            format!("{app_name}-adapter-spin")
        };
        &derived_component
    };
    let component_under = component_id.replace('-', "_");

    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix("# edgezero-provision: v1\n");
    // `Table::insert` returns the previous value (if any). We build
    // a fresh document from `DocumentMut::new()`, so nothing to
    // displace -- discarding the returned Option is intentional.
    // Using `insert` rather than `doc["..."] = ...` sidesteps
    // `clippy::indexing_slicing` (the index form panics if the key
    // is missing; `insert` doesn't).
    doc.insert("spin_manifest_version", value(2));

    // [application] — name matches the component id (which is the
    // adapter crate name) so the emitted application name lines up
    // with the Cargo package that produces the wasm artifact.
    let mut application = Table::new();
    application.insert("name", value(component_id));
    application.insert("version", value("0.1.0"));
    doc.insert("application", Item::Table(application));

    // [[trigger.http]] — array-of-tables so toml_edit emits the
    // `[[...]]` double-bracket syntax. The `trigger` parent table
    // is marked implicit so the emitter skips a bare `[trigger]`
    // header (`[[trigger.http]]` already declares the container).
    let mut http_trigger = Table::new();
    http_trigger.insert("route", value("/..."));
    http_trigger.insert("component", value(component_id));
    let mut http_aot = ArrayOfTables::new();
    http_aot.push(http_trigger);
    let mut trigger = Table::new();
    trigger.set_implicit(true);
    trigger.insert("http", Item::ArrayOfTables(http_aot));
    doc.insert("trigger", Item::Table(trigger));

    // [component.<id>] — insert the sub-table typed so a pathological
    // component id can't inject unescaped section-header syntax; the
    // parent `component` table is implicit so the emitter renders
    // only `[component.<id>]` (no bare `[component]` header).
    let mut comp = Table::new();
    comp.insert(
        "source",
        value(format!(
            "../../target/wasm32-wasip2/release/{component_under}.wasm"
        )),
    );
    // Spin defaults outbound HTTP to deny-all; the operator-facing
    // scaffold historically shipped `["https://*:*"]` so the first
    // `spin up` doesn't silently refuse outbound calls. Match that
    // default here so scaffold and clean-clone produce the same file.
    let mut allowed_hosts = Array::new();
    allowed_hosts.push("https://*:*");
    comp.insert("allowed_outbound_hosts", value(allowed_hosts));
    comp.insert("key_value_stores", value(Array::new()));

    // [component.<id>.build] — `spin build` reads this table; without
    // it the operator has to `cargo build --target wasm32-wasip2 ...`
    // manually before every `spin up`. Match the scaffold default.
    let mut build_table = Table::new();
    build_table.insert(
        "command",
        value("cargo build --target wasm32-wasip2 --release"),
    );
    let mut watch = Array::new();
    watch.push("src/**/*.rs");
    watch.push("Cargo.toml");
    build_table.insert("watch", value(watch));
    comp.insert("build", Item::Table(build_table));

    let mut component_section = Table::new();
    component_section.set_implicit(true);
    component_section.insert(component_id, Item::Table(comp));
    doc.insert("component", Item::Table(component_section));

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const TEST_COMPONENT_ID: &str = "demo";

    #[test]
    fn finds_closest_manifest_when_multiple_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        let first = root.join("crates/first");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("Cargo.toml"), "[package]\nname=\"first\"").unwrap();
        fs::write(first.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let second = root.join("examples/second");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\nname=\"second\"").unwrap();
        fs::write(second.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let found = find_spin_manifest(&second).unwrap();
        assert_eq!(found, second.join("spin.toml"));
    }

    #[test]
    fn finds_manifest_in_current_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("spin.toml"), "spin_manifest_version = 2").unwrap();

        let manifest = find_spin_manifest(root).expect("should find manifest");
        assert_eq!(manifest, root.join("spin.toml"));
    }

    #[test]
    fn locate_artifact_considers_workspace_target() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(manifest_dir.join("target/wasm32-wasip2/release")).unwrap();
        let artifact = workspace.join("target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, TEST_COMPONENT_ID).unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn locate_artifact_converts_hyphens_to_underscores() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("crates/my-cool-crate");
        fs::create_dir_all(&manifest_dir).unwrap();

        // Cargo emits underscored filenames for hyphenated crate names.
        let artifact = workspace.join("target/wasm32-wasip2/release/my_cool_crate.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(workspace, &manifest_dir, "my-cool-crate").unwrap();
        assert_eq!(located, artifact);
    }

    // ---------- synthesise_spin_toml / synthesise_runtime_config_toml ----------

    #[test]
    fn synthesises_spin_toml_derives_crate_name_when_component_unset() {
        // 2026-07 fix: unset component now derives `<app>-adapter-spin`
        // (scaffold-convention crate name), not bare `<app>`, so the
        // resolved wasm source path matches the artifact Cargo emits
        // (`<app>_adapter_spin.wasm`). Prior behaviour named the
        // component `demo` and pointed at `/release/demo.wasm`, which
        // Cargo never produced under a `demo-adapter-spin` package.
        let out = synthesise_spin_toml("demo", None);
        assert!(out.starts_with("# edgezero-provision: v1"));
        assert!(out.contains("spin_manifest_version = 2"));
        assert!(out.contains(r#"name = "demo-adapter-spin""#));
        assert!(out.contains(r#"component = "demo-adapter-spin""#));
        // toml_edit emits the section header as a bare key
        // (`demo-adapter-spin`) — no quoting needed for TOML bare
        // keys that only contain `A-Z`, `a-z`, `0-9`, `-`, `_`.
        assert!(out.contains("[component.demo-adapter-spin]"));
        assert!(out.contains("/release/demo_adapter_spin.wasm"));
    }

    #[test]
    fn synthesises_spin_toml_honors_component_selector() {
        let out = synthesise_spin_toml("demo", Some("worker"));
        assert!(out.contains(r#"component = "worker""#));
        assert!(out.contains("[component.worker]"));
        // wasm path matches the component id, not the app name:
        assert!(out.contains("/release/worker.wasm"));
    }

    #[test]
    fn synthesises_spin_toml_includes_allowed_outbound_hosts_and_build_block() {
        // Scaffold parity: `allowed_outbound_hosts` is a
        // deny-by-default guard in Spin, and `[component.<id>.build]`
        // is what `spin build` reads. Both were previously written by
        // the scaffold `spin.toml.hbs` template; folding them into the
        // synth keeps `edgezero new` and clean-clone `provision --local`
        // byte-identical.
        let out = synthesise_spin_toml("demo", None);
        assert!(
            out.contains(r#"allowed_outbound_hosts = ["https://*:*"]"#),
            "synth must ship the scaffold's outbound-host allow-list: {out}"
        );
        assert!(
            out.contains(r#"command = "cargo build --target wasm32-wasip2 --release""#),
            "synth must include the [component.<id>.build] command: {out}"
        );
    }

    #[test]
    fn synthesises_runtime_config_toml_is_header_only() {
        let out = synthesise_runtime_config_toml();
        assert_eq!(out, "# edgezero-provision: v1\n");
    }

    #[test]
    fn synthesise_spin_toml_escapes_pathological_app_names() {
        // Post-2026-07 synth derives the component id (and hence
        // `[application].name`) from `<app_name>-adapter-spin` when
        // no explicit component override is set. Assert the derived
        // suffix survives escaping for pathological `app_name`s.
        for name in [
            r#"has"quote"#,
            r"has\backslash",
            "has\nnewline",
            "has = equals",
        ] {
            let out = synthesise_spin_toml(name, None);
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            let expected = format!("{name}-adapter-spin");
            assert_eq!(
                doc["application"]["name"].as_str(),
                Some(expected.as_str()),
                "app name round-trip failed for {name:?}: {out}"
            );
        }
    }

    #[test]
    fn synthesise_spin_toml_escapes_pathological_component_id() {
        // Component id flows into BOTH the trigger's `component =`
        // value AND the `[component.<id>]` table key — both must
        // round-trip cleanly.
        for cid in [r#"has"quote"#, r"has\backslash", "has\nnewline"] {
            let out = synthesise_spin_toml("demo", Some(cid));
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            // trigger[0].component == cid
            let trigger_http = doc["trigger"]["http"]
                .as_array_of_tables()
                .expect("trigger.http must be ArrayOfTables");
            assert_eq!(trigger_http.len(), 1);
            assert_eq!(
                trigger_http.get(0).unwrap()["component"].as_str(),
                Some(cid),
                "trigger.component round-trip failed for {cid:?}: {out}"
            );
            // [component.<cid>] exists and has a `source` key
            let comp = doc["component"]
                .as_table()
                .expect("component must be a table");
            assert!(
                comp.contains_key(cid),
                "component table missing key {cid:?}: {out}"
            );
        }
    }
}
