//! Vendor CLI subprocess wrappers for the Spin adapter: `build`,
//! `deploy`, `serve`, plus the manifest / artifact discovery and the
//! `synthesise_*_toml` baselines emitted by the CLI's `provision`
//! bootstrap.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use edgezero_adapter::cli_support::{
    self, find_manifest_upwards, find_workspace_root, path_distance, read_package_name,
};
use edgezero_adapter::env_file::reject_symlinked_target;
use edgezero_adapter::registry::AdapterExecContext;
use walkdir::WalkDir;

const TARGET_TRIPLE: &str = "wasm32-wasip2";

/// # Errors
/// Returns an error if the Spin CLI build command fails.
#[inline]
pub fn build(extra_args: &[String], ctx: &AdapterExecContext<'_>) -> Result<PathBuf, String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_spin_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    // `Cargo.toml` lives at the declared crate root, which is NOT
    // necessarily the manifest's parent -- a nested declared manifest
    // like `crates/server/config/spin.toml` would otherwise resolve
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
    let underscored = format!("{}.wasm", crate_name.replace('-', "_"));
    let pkg_dir = workspace_root.join("pkg");
    fs::create_dir_all(&pkg_dir)
        .map_err(|err| format!("failed to create {}: {err}", pkg_dir.display()))?;
    let dest = pkg_dir.join(&underscored);
    fs::copy(&artifact, &dest)
        .map_err(|err| format!("failed to copy artifact to {}: {err}", dest.display()))?;

    // Refresh the module path that `spin up` / `spin deploy` actually read
    // -- the component's DECLARED `source` in spin.toml -- so a custom
    // target dir OR an operator-edited `source` never leaves a stale module.
    // Falls back to the CONVENTIONAL workspace target path when the manifest
    // can't be parsed or declares no matching source.
    refresh_declared_source(&manifest, &workspace_root, &underscored, &artifact)?;
    refresh_conventional_source(&workspace_root, &underscored, &artifact)?;

    Ok(dest)
}

/// True when both paths refer to the SAME existing file (resolving `.`,
/// `..`, and symlinks). CRUCIAL guard against a self-copy: `fs::copy`
/// TRUNCATES the file to 0 bytes when source and destination are the same
/// inode -- which is exactly the default case, where the declared `source`
/// (`../../target/<triple>/release/<crate>.wasm`, a relative path with `..`)
/// resolves to the very artifact cargo just built.
fn is_same_existing_file(first: &Path, second: &Path) -> bool {
    match (fs::canonicalize(first), fs::canonicalize(second)) {
        (Ok(first_real), Ok(second_real)) => first_real == second_real,
        (Err(_), _) | (_, Err(_)) => false,
    }
}

/// Lexically resolve `.` / `..` in `path` WITHOUT touching the filesystem
/// (so it works for a not-yet-created destination). Symlinks are NOT
/// followed -- that's intentional: the caller separately refuses a symlinked
/// final component so a link can't redirect the write outside the tree.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// Refresh the DECLARED `source` path of THIS crate's component in
/// `spin.toml` with the freshly built `artifact`, so `spin up` / `spin
/// deploy` (which read `source`) never serve a stale module after a
/// custom-target build. A local file `source` is resolved relative to the
/// manifest's directory; registry (`{ url = ... }`) sources are skipped.
/// Best-effort on parse failure.
///
/// Safety / correctness:
/// - Never self-copies (which would TRUNCATE the artifact to 0) -- the
///   default `source` resolves to the artifact itself.
/// - Refuses a `source` that resolves OUTSIDE `workspace_root`, or whose
///   final component is a symlink, so an operator-controlled `source` can't
///   overwrite files elsewhere on disk.
/// - In a single-component manifest the sole component is refreshed
///   regardless of its `source` basename (an operator may rename it); a
///   multi-component manifest matches by basename so siblings aren't
///   clobbered.
fn refresh_declared_source(
    manifest: &Path,
    workspace_root: &Path,
    underscored_wasm: &str,
    artifact: &Path,
) -> Result<(), String> {
    let manifest_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let Ok(raw) = fs::read_to_string(manifest) else {
        return Ok(());
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&raw) else {
        return Ok(());
    };
    let Some(components) = doc.get("component").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let single_component = components.len() == 1;
    let root = fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    for component in components.values() {
        let Some(source) = component.get("source").and_then(toml::Value::as_str) else {
            continue;
        };
        // A single-component manifest's sole component is this crate's, whatever
        // its source basename; only a MULTI-component manifest needs the basename
        // match to avoid clobbering a sibling component.
        let basename_matches =
            Path::new(source).file_name().and_then(|name| name.to_str()) == Some(underscored_wasm);
        if !single_component && !basename_matches {
            continue;
        }
        let src_path = manifest_dir.join(source);
        // Self-copy guard: the default source IS the artifact -- copying it
        // onto itself truncates it to 0 bytes.
        if is_same_existing_file(&src_path, artifact) {
            continue;
        }
        // Containment: never write through `..` / an absolute source / a
        // symlink to a location outside the build tree.
        let normalized = lexically_normalize(&src_path);
        if !normalized.starts_with(&root) {
            return Err(format!(
                "spin component source `{source}` resolves to {} -- outside the project tree {}; \
                 refusing to write there",
                normalized.display(),
                root.display()
            ));
        }
        if src_path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(format!(
                "spin component source {} is a symlink; refusing to overwrite through it",
                src_path.display()
            ));
        }
        if let Some(parent) = src_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        fs::copy(artifact, &src_path).map_err(|err| {
            format!(
                "failed to refresh spin component source {}: {err}",
                src_path.display()
            )
        })?;
    }
    Ok(())
}

/// Copy the freshly-located `artifact` to the CONVENTIONAL workspace target
/// path the synthesised `spin.toml` `source` references
/// (`<workspace>/target/<triple>/release/<crate>.wasm`), so `spin up` /
/// `spin deploy` never read a stale module after a custom-target build. A
/// no-op when the artifact is already at that path (the default case).
fn refresh_conventional_source(
    workspace_root: &Path,
    underscored_wasm: &str,
    artifact: &Path,
) -> Result<(), String> {
    let source_path = workspace_root
        .join("target")
        .join(TARGET_TRIPLE)
        .join("release")
        .join(underscored_wasm);
    // Self-copy guard: on the default (non-custom-target) build the artifact
    // IS this path; copying it onto itself would truncate it to 0 bytes.
    if is_same_existing_file(artifact, &source_path) {
        return Ok(());
    }
    if let Some(parent) = source_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::copy(artifact, &source_path).map_err(|err| {
        format!(
            "failed to refresh spin.toml source path {}: {err}",
            source_path.display()
        )
    })?;
    Ok(())
}

/// # Errors
/// Returns an error if the Spin CLI deploy command fails.
#[inline]
pub fn deploy(extra_args: &[String], ctx: &AdapterExecContext<'_>) -> Result<(), String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_spin_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let mut command = Command::new("spin");
    command
        // `spin cloud deploy`, NOT bare `spin deploy`: on Spin 3+/4 a bare
        // `spin deploy` errors ("needs to be told which deployment plugin to
        // use") -- it now requires a `SPIN_DEPLOY_PLUGIN` / `spin <plugin>
        // deploy`. Fermyon Cloud is EdgeZero's Spin deploy target, and
        // `spin cloud deploy` is its first-class command.
        .args(["cloud", "deploy"])
        // Pass the DECLARED manifest explicitly (`-f/--from`). `current_dir`
        // alone makes `spin` load whatever `spin.toml` sits in that dir --
        // wrong when the declared manifest has a non-standard filename
        // (e.g. `spin.prod.toml`).
        .arg("--from")
        .arg(&manifest)
        .args(extra_args)
        .current_dir(manifest_dir);
    for (key, value) in ctx.env() {
        command.env(key, value);
    }
    let status = command
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
            if candidate.exists() {
                Ok(candidate)
            } else {
                Err(format!(
                    "compiled artifact `{release_name}` not found in the requested target directory {} (a custom target dir was set via --target-dir, CARGO_TARGET_DIR, or .cargo/config.toml); refusing to fall back to a conventional target path to avoid packaging a stale artifact",
                    candidate.display()
                ))
            }
        }
        cli_support::CargoTargetDir::Conventional => {
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
                "compiled artifact not found (looked in {} and workspace target)",
                crate_dir.display()
            ))
        }
    }
}

/// # Errors
/// Returns an error if the Spin CLI up command fails.
#[inline]
/// The `KEY=VALUE` strings to hand `spin up --env` so the EDGEZERO__*
/// runtime-config overrides reach the GUEST's WASI env. A wasip2 component
/// reads its own sandboxed `std::env`, which Spin populates from `--env` /
/// `[component.<id>.environment]` -- NOT from the `spin` host process env --
/// so store `__NAME` / `__KEY` overrides must be forwarded explicitly. Only
/// the `EDGEZERO__` namespace is forwarded (secrets travel as Spin variables,
/// and unrelated host env stays out of the sandbox).
///
/// Merges TWO sources, with the PARENT shell winning:
/// - `overlay` -- the provisioned `.env` / manifest values `ctx.env()`
///   carries (adapter.rs deliberately keeps parent-exported keys OUT of this
///   overlay, since the host process inherits them directly);
/// - `parent` -- the process env `edgezero serve` was launched with. Without
///   this, a highest-precedence shell `EDGEZERO__...__KEY` reaches the spin
///   HOST (inherited) but silently never reaches the sandboxed guest, which
///   then falls back to the store's logical id.
fn guest_env_forwards(overlay: &[(String, String)], parent: &[(String, String)]) -> Vec<String> {
    let mut merged: BTreeMap<&str, &str> = BTreeMap::new();
    for (key, value) in overlay {
        if key.starts_with("EDGEZERO__") {
            merged.insert(key, value);
        }
    }
    // Parent shell overrides the provisioned overlay (last write wins).
    for (key, value) in parent {
        if key.starts_with("EDGEZERO__") {
            merged.insert(key, value);
        }
    }
    merged
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

pub fn serve(extra_args: &[String], ctx: &AdapterExecContext<'_>) -> Result<(), String> {
    let manifest = cli_support::declared_or_discovered_manifest(ctx, || {
        find_spin_manifest(cli_support::discovery_base(ctx)?.as_path())
    })?;
    let manifest_dir = manifest
        .parent()
        .ok_or_else(|| "spin manifest has no parent directory".to_owned())?;

    let mut command = Command::new("spin");
    command.args(["up"]);
    // Pass the DECLARED manifest explicitly (a non-standard filename like
    // `spin.prod.toml` is otherwise ignored -- `spin up` would load whatever
    // `spin.toml` is in the cwd, or fail if none exists there).
    command.arg("--from").arg(&manifest);
    // Match the manifest `commands.serve` shell path, which passes
    // `--runtime-config-file <dir>/runtime-config.toml`. Provision
    // writes each store's `[key_value_store.<name>]` block into that
    // file, so a `spin up` without it starts with none of the local
    // KV bindings the app expects. Only
    // when the file actually exists -- a fresh project without local
    // stores has none, and `spin up --runtime-config-file <missing>`
    // errors.
    let runtime_config = manifest_dir.join("runtime-config.toml");
    if runtime_config.exists() {
        // Reject a symlinked final component before handing it to `spin
        // up`, matching the write side (provision refuses to create it
        // through a symlink) -- one consistent final-path policy.
        reject_symlinked_target(&runtime_config)?;
        command.arg("--runtime-config-file").arg(&runtime_config);
    }
    // Forward the EDGEZERO__* runtime-config overlay INTO THE GUEST via
    // `spin up --env`. Setting them only on the `spin` host process (the
    // `command.env` loop below) does NOT reach a wasip2 component: the guest
    // reads its own sandboxed WASI env (`std::env`), which Spin populates
    // from `--env` / `[component.<id>.environment]`, never from the host's
    // inherited environment. Without this the store `__NAME` / `__KEY`
    // overrides provision writes into `.env` are invisible to the runtime and
    // every store silently falls back to its logical id.
    // Include the operator's PARENT shell env: an exported
    // `EDGEZERO__...__KEY` is the highest-precedence override and is kept out
    // of `ctx.env()` (adapter.rs "parent wins" -> the host inherits it
    // directly), so it must be merged in HERE or it never reaches the guest.
    let parent_env: Vec<(String, String)> = env::vars().collect();
    for pair in guest_env_forwards(ctx.env(), &parent_env) {
        command.arg("--env").arg(pair);
    }
    command.args(extra_args).current_dir(manifest_dir);
    for (key, value) in ctx.env() {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|err| format!("failed to run spin CLI: {err}"))?;
    if !status.success() {
        return Err(format!("spin up failed with status {status}"));
    }

    Ok(())
}

/// Header-only baseline for `runtime-config.toml`. the
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
/// legal `<crate_name>` or `[adapters.spin.adapter].component`
/// selector — including values with TOML-significant characters
/// like `"`, `\`, or newlines — is escaped correctly.
///
/// Two distinct identities feed this synth and are kept SEPARATE:
///
/// - `crate_name`: the Cargo `[package].name` the caller resolved
///   from the adapter crate's `Cargo.toml` (via
///   `cli_support::read_adapter_crate_name`). Drives
///   `[application].name` AND the wasm source basename
///   (`<crate_name_under>.wasm`) — Cargo names the wasm artifact
///   after the package name, regardless of what the operator
///   calls the Spin component.
///
/// - `component`: the Spin component id selector from
///   `[adapters.spin.adapter].component`. The operator's runtime
///   discriminator for a multi-component `spin.toml`. Drives
///   `[[trigger.http]].component` AND the `[component.<id>]`
///   table key. Defaults to `crate_name` when unset (single-
///   component projects).
///
/// A pre-2026-07-v3 shape derived the wasm basename from the
/// component id, which broke when the operator set
/// `[adapters.spin.adapter].component = "worker"` on a Cargo
/// package named `spin-server`: the synthesiser emitted
/// `source = ".../worker.wasm"` while Cargo produced
/// `spin_server.wasm`.
/// Compute the `../` prefix that walks from a manifest sitting
/// at `manifest_rel` (relative to the workspace root) back up to
/// the workspace root itself. The synthesised
/// `[component.<id>].source` joins this prefix with `target/...`
/// so the emitted wasm path reaches the workspace target dir
/// regardless of how deeply the operator nests `spin.toml`.
///
/// Pre-2026-07-13 the synthesiser hard-coded `../../target/...`
/// (correct for the scaffold convention
/// `crates/<crate>/spin.toml` — parent has 2 components), which
/// silently mispointed on nested layouts like
/// `crates/spin-server/config/spin.toml` (needs `../../../target/...`).
///
/// Empty `manifest_rel` (bare `spin.toml` at the workspace root)
/// yields an empty prefix, so the source becomes plain
/// `target/wasm32-wasip2/release/<crate>.wasm`.
pub(crate) fn workspace_relative_target_prefix(manifest_rel: &Path) -> String {
    use std::path::Component;
    let parent = manifest_rel.parent().unwrap_or_else(|| Path::new(""));
    let depth = parent
        .components()
        .filter(|comp| matches!(comp, Component::Normal(_)))
        .count();
    "../".repeat(depth)
}

pub(crate) fn synthesise_spin_toml(
    crate_name: &str,
    component: Option<&str>,
    manifest_rel: &Path,
    allowed_outbound_hosts: &[String],
) -> String {
    use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

    let component_id: &str = component.unwrap_or(crate_name);
    // Wasm source path underscores the CARGO CRATE name — NOT the
    // component id. Spin's component id is an operator selector;
    // the actual artifact Cargo builds is always
    // `<package.name>.wasm` (with hyphens converted to underscores
    // per Cargo's output convention).
    let crate_name_under = crate_name.replace('-', "_");

    let mut doc = DocumentMut::new();
    doc.decor_mut().set_prefix("# edgezero-provision: v1\n");
    // `Table::insert` returns the previous value (if any). We build
    // a fresh document from `DocumentMut::new()`, so nothing to
    // displace -- discarding the returned Option is intentional.
    // Using `insert` rather than `doc["..."] = ...` sidesteps
    // `clippy::indexing_slicing` (the index form panics if the key
    // is missing; `insert` doesn't).
    doc.insert("spin_manifest_version", value(2));

    // [application] — name IS the Cargo package name, so the
    // emitted application identity lines up with the Cargo
    // package that produces the wasm artifact regardless of how
    // the operator names the runtime component below.
    let mut application = Table::new();
    application.insert("name", value(crate_name));
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
    let target_prefix = workspace_relative_target_prefix(manifest_rel);
    comp.insert(
        "source",
        value(format!(
            "{target_prefix}target/wasm32-wasip2/release/{crate_name_under}.wasm"
        )),
    );
    // Spin defaults outbound HTTP to deny-all. That secure baseline is
    // the default here too: the key is emitted ONLY when the operator
    // opts in via `[adapters.spin.adapter].allowed_outbound_hosts`
    //. Both `edgezero new` and clean-clone
    // provision read the same manifest, so the emitted file stays
    // byte-identical across the two paths whether or not the knob is
    // set -- the scaffold-parity contract does not require a specific
    // value, only that both paths agree.
    if !allowed_outbound_hosts.is_empty() {
        let mut allowed_hosts = Array::new();
        for host in allowed_outbound_hosts {
            allowed_hosts.push(host.as_str());
        }
        comp.insert("allowed_outbound_hosts", value(allowed_hosts));
    }
    comp.insert("key_value_stores", value(Array::new()));

    // No `[component.<id>.build]` block: the spec's normative Spin
    // baseline (spec §"Spin (spin.toml)") stops at `source` +
    // `key_value_stores`, and EdgeZero drives builds through its own
    // `edgezero build --adapter spin` (which runs `cargo build`
    // directly), not through bare `spin build`. Emitting a build table
    // exceeded that baseline. Operators
    // who want `spin build` to work standalone add the block to their
    // gitignored spin.toml by hand; the merge path preserves it.

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
    /// The common case: no `[adapters.spin.adapter].allowed_outbound_hosts`
    /// declared, so synthesis stays at Spin's deny-all baseline.
    const NO_HOSTS: &[String] = &[];

    #[test]
    fn guest_env_forwards_only_edgezero_namespace() {
        // Only `EDGEZERO__*` is forwarded into the guest via `spin up --env`
        // (store overrides). Secrets (SPIN_VARIABLE_*) and unrelated host env
        // must NOT leak into the sandbox.
        let env = vec![
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "app_config_staging".to_owned(),
            ),
            (
                "EDGEZERO__STORES__KV__SESSIONS__NAME".to_owned(),
                "sessions".to_owned(),
            ),
            (
                "SPIN_VARIABLE_DEMO_API_TOKEN".to_owned(),
                "secret".to_owned(),
            ),
            ("HOME".to_owned(), "/home/dev".to_owned()),
        ];
        let forwards = guest_env_forwards(&env, &[]);
        assert_eq!(
            forwards,
            vec![
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config_staging".to_owned(),
                "EDGEZERO__STORES__KV__SESSIONS__NAME=sessions".to_owned(),
            ],
            "only EDGEZERO__* forwards; secrets and host env stay out of the guest"
        );
    }

    #[test]
    fn guest_env_forwards_lets_the_parent_shell_override_the_overlay() {
        // A highest-precedence `EDGEZERO__...__KEY` exported in the operator's
        // shell must reach the guest and WIN over the provisioned overlay --
        // otherwise a shell override silently falls back to the logical id.
        let overlay = vec![
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "from_overlay".to_owned(),
            ),
            (
                "EDGEZERO__STORES__KV__SESSIONS__NAME".to_owned(),
                "sessions".to_owned(),
            ),
        ];
        let parent = vec![
            (
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY".to_owned(),
                "from_shell".to_owned(),
            ),
            ("PATH".to_owned(), "/usr/bin".to_owned()),
        ];
        let forwards = guest_env_forwards(&overlay, &parent);
        assert_eq!(
            forwards,
            vec![
                "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=from_shell".to_owned(),
                "EDGEZERO__STORES__KV__SESSIONS__NAME=sessions".to_owned(),
            ],
            "parent-shell EDGEZERO__* overrides the overlay; non-EDGEZERO__ parent env stays out"
        );
    }

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

        let located = locate_artifact(
            workspace,
            &manifest_dir,
            TEST_COMPONENT_ID,
            &[],
            &AdapterExecContext::new(),
        )
        .unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn locate_artifact_honors_ctx_env_cargo_target_dir() {
        // The build runs with `CARGO_TARGET_DIR` applied from `ctx.env()`
        // (manifest `[environment]`), so artifact discovery must read the
        // SAME source -- not just the process env. A relative value is
        // resolved against the crate root, matching cargo's own behavior.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let manifest_dir = workspace.join("service");
        fs::create_dir_all(&manifest_dir).unwrap();
        let artifact = manifest_dir.join("custom-target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, "wasm").unwrap();

        let env = [("CARGO_TARGET_DIR".to_owned(), "custom-target".to_owned())];
        let ctx = AdapterExecContext::new().with_env(&env);
        let located = locate_artifact(workspace, &manifest_dir, "demo", &[], &ctx).unwrap();
        assert_eq!(located, artifact);
    }

    #[test]
    fn refresh_conventional_source_copies_custom_artifact_to_source_path() {
        // A custom-target build leaves the fresh artifact outside the
        // conventional target/ that spin.toml `source` reads. The refresh
        // must copy it there so serve/deploy don't use a stale module.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        // Stale conventional artifact.
        let source = workspace
            .join("target")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "stale").unwrap();
        // Fresh custom-target artifact.
        let fresh = workspace.join("custom/release/demo.wasm");
        fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        fs::write(&fresh, "fresh").unwrap();

        refresh_conventional_source(workspace, "demo.wasm", &fresh).unwrap();
        assert_eq!(
            fs::read_to_string(&source).unwrap(),
            "fresh",
            "the conventional source path must be refreshed with the fresh artifact"
        );
    }

    #[test]
    fn refresh_conventional_source_is_a_noop_when_already_at_source() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let source = workspace
            .join("target")
            .join(TARGET_TRIPLE)
            .join("release/demo.wasm");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "fresh").unwrap();
        // Passing the source path itself must not error (no self-copy).
        refresh_conventional_source(workspace, "demo.wasm", &source).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "fresh");
    }

    #[test]
    fn locate_artifact_honors_target_dir_build_arg_over_stale_default() {
        // A `--target-dir` build arg redirects cargo's output. Discovery
        // must look ONLY there, even when a STALE artifact sits at the
        // conventional workspace target from an earlier default build.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let crate_dir = workspace.join("service");
        fs::create_dir_all(&crate_dir).unwrap();
        // Stale default-target artifact that must NOT be selected.
        let stale = workspace.join("target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(&stale, "stale").unwrap();
        // Fresh custom-target artifact.
        let fresh = crate_dir.join("custom/wasm32-wasip2/release/demo.wasm");
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
        let stale = workspace.join("target/wasm32-wasip2/release/demo.wasm");
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
    fn locate_artifact_honors_cargo_config_target_dir() {
        // `.cargo/config.toml` `[build] target-dir` also redirects cargo.
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        let crate_dir = workspace.join("service");
        fs::create_dir_all(crate_dir.join(".cargo")).unwrap();
        fs::write(
            crate_dir.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"cfg-target\"\n",
        )
        .unwrap();
        let fresh = crate_dir.join("cfg-target/wasm32-wasip2/release/demo.wasm");
        fs::create_dir_all(fresh.parent().unwrap()).unwrap();
        fs::write(&fresh, "fresh").unwrap();

        let located = locate_artifact(
            workspace,
            &crate_dir,
            "demo",
            &[],
            &AdapterExecContext::new(),
        )
        .unwrap();
        assert_eq!(located, fresh);
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

        let located = locate_artifact(
            workspace,
            &manifest_dir,
            "my-cool-crate",
            &[],
            &AdapterExecContext::new(),
        )
        .unwrap();
        assert_eq!(located, artifact);
    }

    // ---------- synthesise_spin_toml / synthesise_runtime_config_toml ----------

    #[test]
    fn synthesises_spin_toml_uses_crate_name_when_component_unset() {
        // Caller resolves the crate name from the adapter-crate
        // Cargo.toml `[package].name` — the synth just takes the
        // resolved value and threads it into `[application].name`
        // + the component id + the underscored wasm path. Verifying
        // with the scaffold-convention name `demo-adapter-spin` so a
        // renamed-adapter regression is easy to spot.
        let out = synthesise_spin_toml(
            "demo-adapter-spin",
            None,
            Path::new("crates/demo-adapter-spin/spin.toml"),
            NO_HOSTS,
        );
        assert!(out.starts_with("# edgezero-provision: v1"));
        assert!(out.contains("spin_manifest_version = 2"));
        assert!(out.contains(r#"name = "demo-adapter-spin""#));
        assert!(out.contains(r#"component = "demo-adapter-spin""#));
        assert!(out.contains("[component.demo-adapter-spin]"));
        assert!(out.contains("/release/demo_adapter_spin.wasm"));
    }

    #[test]
    fn synthesises_spin_toml_uses_renamed_crate_name() {
        // Regression for the reviewer-flagged renamed-adapter bug:
        // when the operator sets `[adapters.spin.adapter].crate =
        // "crates/spin-server"`, the synth must emit the wasm
        // source path Cargo actually produces (`spin_server.wasm`),
        // not the scaffold-convention `demo_app_adapter_spin.wasm`.
        // The synth takes the crate name verbatim; the caller in
        // `cli/mod.rs` is responsible for resolving it from the
        // Cargo.toml — this test pins the synth half of the invariant.
        let out = synthesise_spin_toml(
            "spin-server",
            None,
            Path::new("crates/spin-server/spin.toml"),
            NO_HOSTS,
        );
        assert!(out.contains(r#"name = "spin-server""#));
        assert!(out.contains(r#"component = "spin-server""#));
        assert!(out.contains("[component.spin-server]"));
        assert!(
            out.contains("/release/spin_server.wasm"),
            "spin.toml source must underscore the renamed crate name: {out}"
        );
    }

    #[test]
    fn synthesises_spin_toml_honors_component_selector() {
        let out = synthesise_spin_toml(
            "demo-adapter-spin",
            Some("worker"),
            Path::new("crates/demo-adapter-spin/spin.toml"),
            NO_HOSTS,
        );
        // Component selector drives the trigger/section keys...
        assert!(out.contains(r#"component = "worker""#));
        assert!(out.contains("[component.worker]"));
        // ...but the wasm source basename ALWAYS follows the Cargo
        // crate name — Cargo produces `<package.name>.wasm`
        // regardless of the operator-chosen component id.
        assert!(
            out.contains("/release/demo_adapter_spin.wasm"),
            "wasm basename must underscore the Cargo crate name, not the component selector: {out}"
        );
        assert!(
            !out.contains("/release/worker.wasm"),
            "wasm path MUST NOT track the component selector (Cargo doesn't name artifacts after it): {out}"
        );
        // [application].name also stays tied to the crate name, not
        // the component selector — Spin's application identity is
        // the Cargo package, not the runtime dispatch label.
        assert!(out.contains(r#"name = "demo-adapter-spin""#));
    }

    #[test]
    fn synthesised_spin_toml_component_selector_does_not_leak_into_wasm_basename() {
        // Reviewer-flagged regression: with
        // `[package].name = "spin-server"` and
        // `[adapters.spin.adapter].component = "worker"`, the
        // previous synth emitted `source = ".../worker.wasm"`
        // while Cargo produced `spin_server.wasm`. The two knobs
        // must be independent.
        let out = synthesise_spin_toml(
            "spin-server",
            Some("worker"),
            Path::new("crates/spin-server/spin.toml"),
            NO_HOSTS,
        );
        assert!(
            out.contains(r#"name = "spin-server""#),
            "app.name = crate: {out}"
        );
        assert!(
            out.contains(r#"component = "worker""#),
            "trigger.component: {out}"
        );
        assert!(out.contains("[component.worker]"), "component table: {out}");
        assert!(
            out.contains("/release/spin_server.wasm"),
            "wasm basename must match the Cargo package (spin_server), not the component (worker): {out}"
        );
        assert!(
            !out.contains("worker.wasm"),
            "wasm path must NOT include the component id as a filename: {out}"
        );
    }

    #[test]
    fn synthesises_spin_toml_matches_spec_minimal_baseline() {
        // Exact-content test: with
        // no `allowed_outbound_hosts` declared, synthesis must equal
        // the spec's normative Spin baseline byte-for-byte -- NEITHER
        // an `allowed_outbound_hosts` key (Spin stays deny-all) NOR a
        // `[component.<id>.build]` table. The `crates/demo-adapter-spin`
        // path is 2-deep, so the wasm source prefix is `../../`.
        let out = synthesise_spin_toml(
            "demo-adapter-spin",
            None,
            Path::new("crates/demo-adapter-spin/spin.toml"),
            NO_HOSTS,
        );
        let expected = "# edgezero-provision: v1\n\
             spin_manifest_version = 2\n\n\
             [application]\n\
             name = \"demo-adapter-spin\"\n\
             version = \"0.1.0\"\n\n\
             [[trigger.http]]\n\
             route = \"/...\"\n\
             component = \"demo-adapter-spin\"\n\n\
             [component.demo-adapter-spin]\n\
             source = \"../../target/wasm32-wasip2/release/demo_adapter_spin.wasm\"\n\
             key_value_stores = []\n";
        assert_eq!(out, expected, "spin.toml baseline drifted from spec");
    }

    #[test]
    fn synthesises_spin_toml_emits_declared_outbound_hosts() {
        // Opt-in: the operator declares hosts, so synthesis emits the
        // `allowed_outbound_hosts` array verbatim. Both `edgezero new`
        // and clean-clone read the SAME manifest, so the emitted file
        // stays byte-identical across the two paths (5e19f4f's parity
        // requirement) regardless of the value.
        let hosts = vec![
            "https://*:*".to_owned(),
            "https://api.example.com".to_owned(),
        ];
        let out = synthesise_spin_toml(
            "demo-adapter-spin",
            None,
            Path::new("crates/demo-adapter-spin/spin.toml"),
            &hosts,
        );
        assert!(
            out.contains(r#"allowed_outbound_hosts = ["https://*:*", "https://api.example.com"]"#),
            "declared hosts must be emitted verbatim: {out}"
        );
    }

    #[test]
    fn synthesises_runtime_config_toml_is_header_only() {
        let out = synthesise_runtime_config_toml();
        assert_eq!(out, "# edgezero-provision: v1\n");
    }

    #[test]
    fn synthesise_spin_toml_escapes_pathological_crate_names() {
        // Cargo restricts `[package].name` to `[A-Za-z0-9_-]`, but
        // the synth must still be defensive against TOML-hostile
        // inputs so an exotic value in
        // `[adapters.spin.adapter].crate` doesn't produce invalid
        // TOML at either `[application].name` (root) or the
        // `[component.<id>]` header key.
        for name in [
            r#"has"quote"#,
            r"has\backslash",
            "has\nnewline",
            "has = equals",
        ] {
            let out = synthesise_spin_toml(name, None, Path::new("crates/x/spin.toml"), NO_HOSTS);
            let doc: toml_edit::DocumentMut = out.parse().unwrap();
            assert_eq!(
                doc["application"]["name"].as_str(),
                Some(name),
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
            let out = synthesise_spin_toml(
                "demo",
                Some(cid),
                Path::new("crates/demo/spin.toml"),
                NO_HOSTS,
            );
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
