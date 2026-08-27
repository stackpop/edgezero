use std::fs;
use std::path::{Path, PathBuf, absolute};
use std::process::Command;

use edgezero_adapter::cli_support::{
    self, find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use edgezero_adapter::registry::AdapterExecContext;
use walkdir::WalkDir;

use super::TARGET_TRIPLE;

/// # Errors
/// Returns an error if the Cloudflare wrangler build command fails.
pub(super) fn build(
    extra_args: &[String],
    ctx: &AdapterExecContext<'_>,
) -> Result<PathBuf, String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_wrangler_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    // `Cargo.toml` lives at the declared crate root, which is NOT
    // necessarily the manifest's parent -- a nested declared manifest
    // like `crates/server/config/wrangler.toml` would otherwise resolve
    // `crates/server/config/Cargo.toml`.
    let crate_dir = cli_support::adapter_crate_dir(ctx, &manifest)?;
    let cargo_manifest = crate_dir.join("Cargo.toml");
    let crate_name = read_package_name(&cargo_manifest)?;

    let mut command = Command::new("cargo");
    command
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
        // Anchor cargo at the crate root, not the process cwd. When the
        // CLI dispatches through an absolute `EDGEZERO_MANIFEST` from
        // outside the project, an unanchored `cargo` would discover the
        // wrong `.cargo/config.toml` and resolve relative args against the
        // caller's directory.
        .current_dir(&crate_dir);
    for (key, value) in ctx.env() {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with status {status}"));
    }

    let workspace_root = find_workspace_root(&crate_dir);
    let artifact = locate_artifact(&workspace_root, &crate_dir, &crate_name, extra_args, ctx)?;
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
pub(super) fn deploy(extra_args: &[String], ctx: &AdapterExecContext<'_>) -> Result<(), String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_wrangler_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    // Run wrangler from the CRATE ROOT, not the manifest dir. `wrangler
    // deploy`/`dev` run the `[build]` `worker-build` command in the CWD --
    // which is where `Cargo.toml` lives and where the `build/worker/shim.mjs`
    // it emits must land. For a NESTED manifest (e.g.
    // `crates/server/config/wrangler.toml`) the manifest dir is NOT the crate
    // root, so running there leaves worker-build unable to find `Cargo.toml`
    // and the shim in the wrong place. The synthesised `main` is written
    // relative to the manifest dir (see `wrangler_main_relpath`) so it still
    // resolves to the crate-root output.
    let crate_dir = cli_support::adapter_crate_dir(ctx, &manifest)?;
    let config = absolute(&manifest)
        .unwrap_or(manifest.clone())
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?
        .to_owned();

    let mut command = Command::new("wrangler");
    command
        .args(["deploy", "--config", config.as_str()])
        .args(extra_args)
        .current_dir(&crate_dir);
    for (key, value) in ctx.env() {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler deploy failed with status {status}"));
    }

    Ok(())
}

/// # Errors
/// Returns an error if the Cloudflare wrangler dev command fails.
pub(super) fn serve(extra_args: &[String], ctx: &AdapterExecContext<'_>) -> Result<(), String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_wrangler_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    // Run from the crate root, same rationale as `deploy` above.
    let crate_dir = cli_support::adapter_crate_dir(ctx, &manifest)?;
    let config = absolute(&manifest)
        .unwrap_or(manifest.clone())
        .to_str()
        .ok_or_else(|| "invalid wrangler config path".to_owned())?
        .to_owned();

    let mut command = Command::new("wrangler");
    command
        .args(["dev", "--config", config.as_str()])
        .args(extra_args)
        .current_dir(&crate_dir);
    for (key, value) in ctx.env() {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|err| format!("failed to run wrangler CLI: {err}"))?;
    if !status.success() {
        return Err(format!("wrangler dev failed with status {status}"));
    }

    Ok(())
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
    crate_dir: &Path,
    crate_name: &str,
    build_args: &[String],
    ctx: &AdapterExecContext<'_>,
) -> Result<PathBuf, String> {
    let release_name = format!("{}.wasm", crate_name.replace('-', "_"));

    // Resolve cargo's effective target dir the SAME way the build did
    // (`--target-dir` arg, then `CARGO_TARGET_DIR`, then a
    // `.cargo/config.toml` `[build] target-dir`). When an override is in
    // play, look ONLY there -- falling back to the conventional `target/`
    // paths could package a STALE artifact from an earlier default build.
    match cli_support::resolve_cargo_target_dir(crate_dir, build_args, ctx) {
        cli_support::CargoTargetDir::Explicit(dir) => {
            let candidate = dir.join(TARGET_TRIPLE).join("release").join(&release_name);
            return if candidate.exists() {
                Ok(candidate)
            } else {
                Err(format!(
                    "compiled artifact `{release_name}` not found in the requested target directory {} (a custom target dir was set via --target-dir, CARGO_TARGET_DIR, or .cargo/config.toml); refusing to fall back to a conventional target path to avoid packaging a stale artifact",
                    candidate.display()
                ))
            };
        }
        cli_support::CargoTargetDir::Conventional => {}
    }

    let manifest_target = crate_dir
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

/// Synthesised baseline `wrangler.toml` for scaffold-time and
/// clean-clone bootstrap (single source — the Cloudflare blueprint
/// has no scaffold `.hbs` template for `wrangler.toml`, so
/// `edgezero new` and clean-clone `provision --local` produce
/// byte-identical output; see the "Generated Adapter manifests"
/// note in the spec).
///
/// The `name` field spells the adapter crate's Cargo package name.
/// The caller in `cli/mod.rs` reads this from the `Cargo.toml`
/// adjacent to the adapter manifest (honouring the operator's
/// `[adapters.cloudflare.adapter].crate` rename) and falls back
/// to the scaffold convention `<app_name>-adapter-cloudflare`
/// only when no Cargo.toml is discoverable. `worker-build` reads
/// this field and expects it to match the Cargo package it builds.
///
/// Built via `toml_edit::DocumentMut` (NOT raw `format!`) so any
/// legal name — including values with TOML-significant characters
/// like `"`, `\`, or newlines — is escaped correctly.
/// The `main` value for a wrangler.toml at `manifest_rel` (relative to the
/// project root) whose crate root is `crate_rel`. Wrangler resolves `main`
/// against the config-file's directory, while `worker-build` emits
/// `build/worker/shim.mjs` at the CRATE ROOT (where `Cargo.toml` lives and
/// where wrangler runs it). So for a manifest nested BELOW the crate root the
/// value must climb back up with `../` before `build/worker/shim.mjs`.
pub(super) fn wrangler_main_relpath(manifest_rel: &Path, crate_rel: Option<&str>) -> String {
    const SHIM: &str = "build/worker/shim.mjs";
    let manifest_dir = manifest_rel.parent().unwrap_or_else(|| Path::new(""));
    let Some(crate_root) = crate_rel else {
        return SHIM.to_owned();
    };
    // How far the manifest dir sits BELOW the crate root.
    let Ok(suffix) = manifest_dir.strip_prefix(Path::new(crate_root)) else {
        // Manifest not under the declared crate (unusual layout) -- fall back
        // to the crate-root-adjacent default rather than guess.
        return SHIM.to_owned();
    };
    let depth = suffix.components().count();
    if depth == 0 {
        return SHIM.to_owned();
    }
    format!("{}{SHIM}", "../".repeat(depth))
}

pub(super) fn synthesise_wrangler_toml(crate_name: &str, main_rel: &str) -> String {
    use toml_edit::{DocumentMut, value};

    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix("# edgezero-provision: v1\n");
    // `Table::insert` returns the previous value (if any). We build a
    // fresh document from `DocumentMut::new()`, so nothing to displace
    // -- but the return is discarded intentionally. Using `insert`
    // instead of `doc["..."] = ...` sidesteps `clippy::indexing_slicing`
    // (the index form panics if the key is missing; `insert` doesn't).
    doc.insert("name", value(crate_name));
    // `main` is written RELATIVE TO THE MANIFEST DIR (wrangler resolves it
    // against the config file's location), pointing at the crate-root
    // `build/worker/shim.mjs` `worker-build` emits. For a manifest at the
    // crate root this is just `build/worker/shim.mjs`; for a NESTED manifest
    // it prefixes enough `../` to climb back to the crate root.
    doc.insert("main", value(main_rel));
    doc.insert("compatibility_date", value("2024-01-01"));

    // `[build] command = "worker-build --release"`: `main` points at
    // `build/worker/shim.mjs`, the wasm-bindgen glue that ONLY `worker-build`
    // produces (a plain `cargo build` emits just the raw wasm). Without this
    // command a fresh scaffold cannot serve or deploy -- `wrangler dev` /
    // `wrangler deploy` load `main` but nothing ever creates the shim.
    // `wrangler` runs this `[build].command` automatically before dev/deploy,
    // so `edgezero serve` / `edgezero deploy` (which shell out to wrangler
    // with `--config`) get the shim built for them.
    let mut build_table = toml_edit::Table::new();
    build_table.insert("command", value("worker-build --release"));
    doc.insert("build", toml_edit::Item::Table(build_table));

    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ---------- locate_artifact ----------

    #[test]
    fn locate_artifact_honors_target_dir_build_arg_over_stale_default() {
        // A `--target-dir` build arg redirects cargo. Discovery must look
        // ONLY there, even when a STALE artifact sits at the conventional
        // workspace target from an earlier default build.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let crate_dir = workspace.join("service");
        fs::create_dir_all(&crate_dir).unwrap();
        let stale = workspace
            .join("target")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "stale").unwrap();
        let fresh = crate_dir
            .join("custom")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        fs::write(&fresh, "fresh").unwrap();

        let build_args = ["--target-dir".to_owned(), "custom".to_owned()];
        let located = locate_artifact(
            workspace,
            &crate_dir,
            "demo",
            &build_args,
            &AdapterExecContext::new(),
        )
        .unwrap();
        assert_eq!(located, fresh, "must select the custom-target artifact");
    }

    #[test]
    fn locate_artifact_errors_when_explicit_target_dir_has_no_artifact() {
        // An explicit target dir with no artifact must error rather than
        // silently fall back to a stale conventional artifact.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let crate_dir = workspace.join("service");
        fs::create_dir_all(&crate_dir).unwrap();
        let stale = workspace
            .join("target")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "stale").unwrap();

        let build_args = ["--target-dir=custom".to_owned()];
        let err = locate_artifact(
            workspace,
            &crate_dir,
            "demo",
            &build_args,
            &AdapterExecContext::new(),
        )
        .expect_err("must not fall back to the stale conventional artifact");
        assert!(err.contains("stale"), "error explains the refusal: {err}");
    }

    #[test]
    fn locate_artifact_conventional_search_still_works() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let crate_dir = workspace.join("service");
        fs::create_dir_all(&crate_dir).unwrap();
        let artifact = workspace
            .join("target")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let located = locate_artifact(
            workspace,
            &crate_dir,
            "demo",
            &[],
            &AdapterExecContext::new(),
        )
        .unwrap();
        assert_eq!(located, artifact);
    }

    // ---------- wrangler_main_relpath ----------

    #[test]
    fn wrangler_main_relpath_climbs_to_the_crate_root() {
        // Manifest at the crate root -> plain path.
        assert_eq!(
            wrangler_main_relpath(Path::new("wrangler.toml"), None),
            "build/worker/shim.mjs"
        );
        assert_eq!(
            wrangler_main_relpath(Path::new("crates/cf/wrangler.toml"), Some("crates/cf")),
            "build/worker/shim.mjs"
        );
        // Nested one level below the crate root -> one `../`.
        assert_eq!(
            wrangler_main_relpath(
                Path::new("crates/cf/config/wrangler.toml"),
                Some("crates/cf")
            ),
            "../build/worker/shim.mjs"
        );
        // Nested two levels.
        assert_eq!(
            wrangler_main_relpath(Path::new("crates/cf/a/b/wrangler.toml"), Some("crates/cf")),
            "../../build/worker/shim.mjs"
        );
    }

    // ---------- synthesise_wrangler_toml ----------

    #[test]
    fn synthesises_wrangler_toml_matches_spec_baseline_exactly() {
        // Exact-content test: the synthesised wrangler.toml must equal the
        // Cloudflare baseline byte-for-byte. The `[build]` command is REQUIRED
        // (not an extra): `main` points at the `worker-build`-generated shim,
        // so without it a fresh scaffold cannot serve or deploy. A loose
        // `contains` check let the baseline drift; this pins it.
        let out = synthesise_wrangler_toml("demo-adapter-cloudflare", "build/worker/shim.mjs");
        let expected = "# edgezero-provision: v1\n\
             name = \"demo-adapter-cloudflare\"\n\
             main = \"build/worker/shim.mjs\"\n\
             compatibility_date = \"2024-01-01\"\n\
             \n\
             [build]\n\
             command = \"worker-build --release\"\n";
        assert_eq!(out, expected, "wrangler.toml baseline drifted");
    }

    #[test]
    fn synthesise_wrangler_toml_escapes_pathological_crate_names() {
        // Adapter crate names come from Cargo.toml `[package].name`
        // — Cargo restricts them to `[A-Za-z0-9_-]`, but the synth
        // must still be defensive against TOML-hostile inputs so
        // an operator that stashes something exotic into
        // `[adapters.<name>.adapter].crate` doesn't produce
        // invalid TOML.
        for name in [
            r#"has"quote"#,
            r"has\backslash",
            "has\nnewline",
            "has = equals",
        ] {
            let out = synthesise_wrangler_toml(name, "build/worker/shim.mjs");
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(doc["name"].as_str(), Some(name), "input: {name:?}");
        }
    }
}
