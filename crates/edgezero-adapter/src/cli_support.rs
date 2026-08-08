#![allow(
    dead_code,
    reason = "helpers consumed conditionally via the `cli` feature in adapter crates"
)]

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::AdapterExecContext;

/// Directory an adapter's manifest discovery should start from.
///
/// When the CLI reached the registry fallback with a manifest loaded,
/// [`AdapterExecContext::cwd`] carries the project root; the adapter
/// must seed discovery there rather than from the process cwd, or a
/// `serve`/`build` invoked from elsewhere resolves the wrong project
///. With no context (empty ctx
/// / no manifest) it falls back to the process cwd, preserving the
/// pre-context behaviour.
///
/// # Errors
/// Propagates a failure to read the process cwd.
#[inline]
pub fn discovery_base(ctx: &AdapterExecContext<'_>) -> Result<PathBuf, String> {
    match ctx.cwd() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => env::current_dir().map_err(|err| err.to_string()),
    }
}

/// The manifest an adapter should operate on: the declared,
/// root-resolved path from [`AdapterExecContext::adapter_manifest`]
/// when the CLI provided one, otherwise the result of `discover`
/// (the adapter's workspace-scan fallback).
///
/// Using the declared path stops ambient discovery from selecting the
/// wrong manifest in a nested / multi-app layout or following a symlink
/// off the validated tree whenever a manifest was actually loaded.
///
/// # Errors
/// Propagates a discovery failure when no declared path is present.
#[inline]
pub fn declared_or_discovered_manifest<F>(
    ctx: &AdapterExecContext<'_>,
    discover: F,
) -> Result<PathBuf, String>
where
    F: FnOnce() -> Result<PathBuf, String>,
{
    match ctx.adapter_manifest() {
        Some(declared) => Ok(declared.to_path_buf()),
        None => discover(),
    }
}

/// The directory holding the adapter crate's `Cargo.toml`: the declared,
/// root-resolved [`AdapterExecContext::adapter_crate`] when the CLI
/// provided one, otherwise the platform manifest's parent directory.
///
/// The manifest's parent is only correct when the platform manifest sits
/// AT the crate root (the scaffold convention). A declared nested
/// manifest such as `crates/server/config/spin.toml` has its crate root
/// one level up, so assuming the parent would look for a `Cargo.toml`
/// that does not exist and fail the build.
///
/// # Errors
/// Returns an error when neither a declared crate root nor a manifest
/// parent is available.
#[inline]
pub fn adapter_crate_dir(
    ctx: &AdapterExecContext<'_>,
    adapter_manifest: &Path,
) -> Result<PathBuf, String> {
    if let Some(declared) = ctx.adapter_crate() {
        return Ok(declared.to_path_buf());
    }
    adapter_manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            format!(
                "adapter manifest `{}` has no parent directory and no `[adapters.<name>.adapter].crate` was declared",
                adapter_manifest.display()
            )
        })
}

/// Walks up the directory tree looking for `manifest_name` alongside a `Cargo.toml`.
#[inline]
#[must_use]
pub fn find_manifest_upwards(start: &Path, manifest_name: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(manifest_name);
        if candidate.exists() && dir.join("Cargo.toml").exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

/// Returns the workspace root for `dir` by walking upward and stopping at the
/// first `Cargo.toml` that contains a `[workspace]` table.  If no workspace
/// table is found, falls back to the highest ancestor containing a `Cargo.toml`,
/// and finally to `dir` itself.
#[inline]
#[must_use]
pub fn find_workspace_root(dir: &Path) -> PathBuf {
    let mut current: Option<&Path> = Some(dir);
    let mut candidate: Option<PathBuf> = None;

    while let Some(path) = current {
        let cargo = path.join("Cargo.toml");
        if cargo.exists() {
            candidate = Some(path.to_path_buf());
            if fs::read_to_string(&cargo).is_ok_and(|contents| contents.contains("[workspace]")) {
                break;
            }
        }
        current = path.parent();
    }

    candidate.unwrap_or_else(|| dir.to_path_buf())
}

/// The effective cargo target directory for build-artifact discovery.
pub enum CargoTargetDir {
    /// An explicit override was requested -- via a `--target-dir` build
    /// argument, `CARGO_TARGET_DIR`, or a `.cargo/config.toml`
    /// `[build] target-dir`. Artifact discovery must look ONLY here:
    /// falling back to the conventional `target/` directories could select
    /// a STALE artifact from an earlier default-target build after a
    /// successful custom-target one.
    Explicit(PathBuf),
    /// No override; use the conventional crate/workspace `target/` search.
    Conventional,
}

/// Resolve cargo's effective target directory for locating a build
/// artifact, mirroring cargo's own precedence: an explicit `--target-dir`
/// build argument, then a `--config build.target-dir=…` build argument,
/// then `CARGO_TARGET_DIR` / `CARGO_BUILD_TARGET_DIR` (the ctx-applied env
/// the build used, then the process env), then a `.cargo/config.toml`
/// `[build] target-dir` walking from the crate root up to the workspace
/// root. Relative values follow cargo: `--target-dir` /
/// `--config build.target-dir` / the env vars resolve against the build's
/// working directory (`crate_dir`); a config-file `target-dir` resolves
/// against the directory that contains the `.cargo` directory.
///
/// Returning [`CargoTargetDir::Explicit`] signals the caller must not fall
/// back to the conventional paths, which is what prevents a stale
/// default-target artifact from being packaged or deployed.
#[inline]
#[must_use]
pub fn resolve_cargo_target_dir(
    crate_dir: &Path,
    build_args: &[String],
    ctx: &AdapterExecContext<'_>,
) -> CargoTargetDir {
    // 1. `--target-dir <v>` / `--target-dir=<v>`, then a
    //    `--config build.target-dir="…"` / `--config <file>` override, in
    //    the build args. Cargo lets the LAST occurrence win.
    if let Some(dir) = target_dir_from_args(build_args) {
        return CargoTargetDir::Explicit(resolve_dir_against(crate_dir, &dir));
    }
    if let Some(dir) = config_target_dir_from_args(crate_dir, build_args) {
        return CargoTargetDir::Explicit(resolve_dir_against(crate_dir, &dir));
    }
    // 2. `CARGO_TARGET_DIR`, then its `[build] target-dir` env alias
    //    `CARGO_BUILD_TARGET_DIR`. The ctx-applied env the build ran with
    //    takes precedence over the process env, mirroring `command.env`.
    for key in ["CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR"] {
        if let Some(dir) = env_value(ctx, key).map(PathBuf::from) {
            return CargoTargetDir::Explicit(resolve_dir_against(crate_dir, &dir));
        }
    }
    // 3. `.cargo/config.toml` (or `.cargo/config`) `[build] target-dir`,
    //    walking EVERY ancestor from the crate dir to the filesystem root
    //    (cargo doesn't stop at the workspace), preferring `config.toml`
    //    over `config`, then `$CARGO_HOME`. The nearest declaration wins.
    let mut dir = Some(crate_dir);
    while let Some(current) = dir {
        for name in ["config.toml", "config"] {
            if let Some(target) = config_build_target_dir(&current.join(".cargo").join(name)) {
                return CargoTargetDir::Explicit(resolve_dir_against(current, &target));
            }
        }
        dir = current.parent();
    }
    if let Some(home) = cargo_home(ctx) {
        for name in ["config.toml", "config"] {
            if let Some(target) = config_build_target_dir(&home.join(name)) {
                return CargoTargetDir::Explicit(resolve_dir_against(&home, &target));
            }
        }
    }
    CargoTargetDir::Conventional
}

/// Read an env var the build actually ran with: the ctx-applied overlay
/// (mirroring `command.env`) takes precedence over the process env.
fn env_value(ctx: &AdapterExecContext<'_>, key: &str) -> Option<OsString> {
    ctx.env()
        .iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| OsString::from(value))
        .or_else(|| env::var_os(key))
}

/// `$CARGO_HOME` (or `~/.cargo`), where cargo keeps its global config.
/// Honours a ctx-provided `CARGO_HOME` / `HOME` (the env the build ran
/// with) before the process env, so the global-config lookup matches the
/// directory cargo actually used.
fn cargo_home(ctx: &AdapterExecContext<'_>) -> Option<PathBuf> {
    env_value(ctx, "CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env_value(ctx, "HOME").map(|home| PathBuf::from(home).join(".cargo")))
}

/// Extract the value of a `--target-dir` build argument (`--target-dir X`
/// or `--target-dir=X`). Cargo lets the LAST occurrence win.
fn target_dir_from_args(args: &[String]) -> Option<PathBuf> {
    let mut last = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--target-dir=") {
            last = Some(PathBuf::from(value));
            continue;
        }
        if arg == "--target-dir"
            && let Some(value) = iter.next()
        {
            last = Some(PathBuf::from(value));
        }
    }
    last
}

/// Extract a target-dir override from `--config` build args -- both the
/// inline `--config build.target-dir="…"` form AND the `--config <file>`
/// form (where the file's `[build] target-dir` is read). Cargo lets the
/// LAST `--config` win. An inline value is parsed as a TOML expression (so
/// `--config 'build.target-dir = "x"'` with spaces works exactly like
/// `--config build.target-dir="x"`) and returned relative for the caller to
/// resolve against the build's working directory; a config FILE's relative
/// `target-dir` is resolved by [`config_build_target_dir`] against cargo's
/// own base for that file.
fn config_target_dir_from_args(crate_dir: &Path, args: &[String]) -> Option<PathBuf> {
    let mut last: Option<PathBuf> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let raw = if arg == "--config" {
            iter.next().map(String::as_str)
        } else {
            arg.strip_prefix("--config=")
        };
        let Some(spec) = raw.map(str::trim) else {
            continue;
        };
        if is_inline_config_kv(spec) {
            // Inline `--config <toml>`: parse the whole payload as TOML
            // rather than string-matching a fixed `key=` prefix, so a
            // spaced `build.target-dir = "x"` and quoted keys are honoured,
            // not dropped. An inline `include` is followed too (cargo does).
            if let Some(target) = target_dir_from_inline(spec, crate_dir) {
                last = Some(target);
            }
            continue;
        }
        // `--config <file>`: read the file's `[build] target-dir` (following
        // `include`), already resolved against cargo's base for the file.
        let cfg_path = resolve_dir_against(crate_dir, Path::new(spec));
        if let Some(target) = config_build_target_dir(&cfg_path) {
            last = Some(target);
        }
    }
    last
}

/// Whether a `--config` payload is an inline TOML expression (as opposed to
/// a config FILE path). Cargo treats `--config <arg>` as inline TOML when it
/// parses as a non-empty TOML table; anything that doesn't (a bare path like
/// `cfg/config.toml`) is a file. Parsing -- rather than a hand-rolled key
/// scan -- accepts quoted dotted keys (`build."target-dir" = "x"`) that cargo
/// accepts too.
fn is_inline_config_kv(payload: &str) -> bool {
    toml::from_str::<toml::Table>(payload).is_ok_and(|table| !table.is_empty())
}

/// Extract a target-dir from an inline `--config` TOML payload: the direct
/// `build.target-dir` (returned RAW for the caller to resolve against the
/// build cwd), or, failing that, one reached via an inline `include`
/// (resolved against the build cwd `crate_dir`, LAST entry winning).
fn target_dir_from_inline(spec: &str, crate_dir: &Path) -> Option<PathBuf> {
    let doc: toml::Value = toml::from_str(spec).ok()?;
    if let Some(target) = doc
        .get("build")
        .and_then(|build| build.get("target-dir"))
        .and_then(toml::Value::as_str)
    {
        return Some(PathBuf::from(target));
    }
    collect_includes(&doc)?.iter().rev().find_map(|inc| {
        config_build_target_dir_at(&resolve_dir_against(crate_dir, Path::new(inc)), 0)
    })
}

/// The `include` directive as a list of paths: a single string or an array
/// of strings (cargo's two accepted forms). `None` when absent or malformed.
fn collect_includes(doc: &toml::Value) -> Option<Vec<String>> {
    match doc.get("include") {
        Some(toml::Value::String(one)) => Some(vec![one.clone()]),
        Some(toml::Value::Array(many)) => Some(
            many.iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

/// Navigate `[build] target-dir` out of a parsed TOML document string,
/// returning the RAW (unresolved) value. Shared by the inline `--config`
/// path and the config-file reader.
fn target_dir_from_toml_str(raw: &str) -> Option<PathBuf> {
    let doc: toml::Value = toml::from_str(raw).ok()?;
    doc.get("build")?
        .get("target-dir")?
        .as_str()
        .map(PathBuf::from)
}

/// Resolve a possibly-relative target dir against `base` (absolute values
/// pass through unchanged), matching how cargo resolves the corresponding
/// path.
fn resolve_dir_against(base: &Path, dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        base.join(dir)
    }
}

/// Depth bound on `include` chains, guarding against cyclic or pathological
/// config includes.
const MAX_CONFIG_INCLUDE_DEPTH: usize = 16;

/// Read `[build] target-dir` from a cargo config file, following `include`
/// directives, and RESOLVE a relative value the way cargo does: a
/// config-relative path is relative to the PARENT of the directory that
/// contains the config file (for `<root>/.cargo/config.toml` that is
/// `<root>`). The including file wins over its includes, and among includes
/// a LATER entry wins over an earlier one -- matching cargo's merge order.
/// Returns the resolved dir, or `None` if none is declared anywhere in the
/// chain.
fn config_build_target_dir(config_path: &Path) -> Option<PathBuf> {
    config_build_target_dir_at(config_path, 0)
}

fn config_build_target_dir_at(config_path: &Path, depth: usize) -> Option<PathBuf> {
    if depth > MAX_CONFIG_INCLUDE_DEPTH {
        return None;
    }
    let raw = fs::read_to_string(config_path).ok()?;
    let doc: toml::Value = toml::from_str(&raw).ok()?;

    // The including file's own `[build] target-dir` takes precedence.
    if let Some(target) = doc
        .get("build")
        .and_then(|build| build.get("target-dir"))
        .and_then(toml::Value::as_str)
    {
        // cargo base: the parent of the directory containing the config file.
        let base = config_path.parent().and_then(Path::parent);
        let dir = Path::new(target);
        return Some(match base {
            Some(parent) => resolve_dir_against(parent, dir),
            None => dir.to_path_buf(),
        });
    }

    // Otherwise follow `include` (a single path or an array), resolved
    // against the including file's own directory. Cargo merges later
    // includes ON TOP of earlier ones, so a LATER entry wins: iterate in
    // reverse and take the first (i.e. last-listed) that declares one.
    let include_dir = config_path.parent()?;
    collect_includes(&doc)?.iter().rev().find_map(|inc| {
        let inc_path = resolve_dir_against(include_dir, Path::new(inc));
        config_build_target_dir_at(&inc_path, depth.saturating_add(1))
    })
}

/// Calculates the path distance between two directories based on shared leading components.
#[inline]
#[must_use]
pub fn path_distance(left: &Path, right: &Path) -> usize {
    let left_components: Vec<_> = left.components().collect();
    let right_components: Vec<_> = right.components().collect();

    let common = left_components
        .iter()
        .zip(&right_components)
        .take_while(|&(lhs, rhs)| lhs == rhs)
        .count();

    left_components
        .len()
        .saturating_sub(common)
        .saturating_add(right_components.len().saturating_sub(common))
}

/// Spawn `program args…` inheriting parent stdio, returning a
/// human-readable error message.
///
/// Used by every adapter's auth dispatch (`wrangler login`,
/// `fastly profile create`, `spin cloud login`, …). The
/// `install_hint` is appended to the not-found message so the
/// adapter can point operators at the right install instructions
/// (`npm install -g wrangler`, the Fastly CLI download page, etc.).
///
/// # Errors
/// Returns an error string if the binary is missing from `PATH`,
/// the child fails to spawn, or it exits non-zero.
#[inline]
pub fn run_native_cli(program: &str, args: &[&str], install_hint: &str) -> Result<(), String> {
    let status = Command::new(program).args(args).status().map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            format!("`{program}` not found on PATH; {install_hint}")
        } else {
            format!("failed to spawn `{program}`: {err}")
        }
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {}` exited with status {status}",
            args.join(" ")
        ))
    }
}

/// Resolve the ADAPTER CRATE package name for the manifest being
/// synthesised. Walks from the manifest's parent directory upward
/// (toward `manifest_root`) looking for the nearest `Cargo.toml`,
/// then reads its `[package].name`. The spec allows the adapter
/// manifest to resolve anywhere inside the adapter crate — including
/// nested paths like `crates/server/config/axum.toml` — so a
/// parent-only lookup misses the crate's actual Cargo.toml when the
/// operator organises manifests under a sub-directory (spec
/// §"[adapters.<name>.adapter]").
///
/// Used by `Adapter::synthesise_baseline_manifest` impls to write
/// runtime-authoritative fields — Axum's `[adapter].crate`, the
/// Spin `[application].name` / wasm source path, Cloudflare's
/// `wrangler.toml` `name`, Fastly's `fastly.toml` `name`. The
/// synthesised value MUST match the Cargo package the adapter
/// actually builds; hardcoding a `<app>-adapter-<id>` convention
/// silently mispoints the wasm source path on any project that
/// renames the adapter crate.
///
/// Returns `None` when:
/// - `adapter_manifest_path` is `None` (no adapter manifest path
///   declared in `edgezero.toml`), OR
/// - no ancestor from the manifest's parent up to `manifest_root`
///   (inclusive) has a readable `Cargo.toml` with `[package].name`.
///
/// Callers fall back to a scaffold-convention crate name in that
/// case (e.g. `<app_name>-adapter-<id>`) so the synthesis is
/// still deterministic on a fresh scaffold.
/// Read the crate name from the DECLARED `.crate` directory's
/// `Cargo.toml`, resolved against `manifest_root`.
///
/// This is authoritative: unlike [`read_adapter_crate_name`], which
/// walks up from the platform manifest's parent and stops at the FIRST
/// `Cargo.toml` it finds, this reads exactly the operator-declared crate
/// root. A nested package that happens to sit between the platform
/// manifest and the intended crate can't be mis-selected.
///
/// Returns `Ok(None)` only when `.crate` is UNDECLARED, so callers fall
/// back to the ancestor search / scaffold convention. Once `.crate` IS
/// declared it is authoritative: an unreadable `Cargo.toml` at that path
/// is a hard error (`Err`), not a silent fallback -- otherwise a
/// mis-declared crate would quietly mispoint the synthesised
/// runtime-authoritative fields at whatever ancestor package happens to
/// sit above the manifest.
///
/// # Errors
/// Returns `Err` when `.crate` is declared but the `Cargo.toml` at that
/// path can't be read or parsed for a package name.
#[inline]
pub fn read_crate_name_at(
    manifest_root: &Path,
    adapter_crate_path: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(rel) = adapter_crate_path else {
        return Ok(None);
    };
    let cargo_toml = manifest_root.join(rel).join("Cargo.toml");
    read_package_name(&cargo_toml).map(Some).map_err(|err| {
        format!(
            "declared adapter crate `{rel}` has an unreadable Cargo.toml: {err}. Fix \
             `[adapters.<name>.adapter].crate` or the crate's Cargo.toml; provision will not \
             silently fall back to an ancestor package."
        )
    })
}

#[inline]
#[must_use]
pub fn read_adapter_crate_name(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
) -> Option<String> {
    let rel = adapter_manifest_path?;
    let manifest_abs = manifest_root.join(rel);
    let mut current = manifest_abs.parent()?;
    // Walk up until we either find a Cargo.toml or step above
    // `manifest_root`. The walk is inclusive at `manifest_root`
    // (a root-level manifest at `edgezero.toml`-sibling path still
    // gets to inspect the workspace root Cargo.toml if the adapter
    // is unrolled at that level). Bounded by `manifest_root` so we
    // never leak up into the user's home directory or workspace
    // parents when the adapter manifest lives at a shallow depth.
    let root_abs = manifest_root
        .canonicalize()
        .unwrap_or_else(|_| manifest_root.to_path_buf());
    loop {
        if let Ok(name) = read_package_name(&current.join("Cargo.toml")) {
            return Some(name);
        }
        // Stop after checking `manifest_root` itself.
        let current_abs = current
            .canonicalize()
            .unwrap_or_else(|_| current.to_path_buf());
        if current_abs == root_abs {
            return None;
        }
        current = current.parent()?;
    }
}

/// Walk from the manifest's parent up to `manifest_root` and
/// return the first ancestor directory that carries a
/// `Cargo.toml`. Mirrors [`read_adapter_crate_name`]'s traversal
/// but returns the DIRECTORY instead of the parsed package name —
/// callers use it to resolve fields like Axum's `[adapter].crate_dir`
/// (a relative path FROM the adapter manifest's parent TO the
/// crate root that carries `Cargo.toml`).
///
/// Returns `None` when the manifest path is unset OR no ancestor
/// up to `manifest_root` (inclusive) carries a `Cargo.toml`.
#[inline]
#[must_use]
pub fn read_adapter_crate_root(
    manifest_root: &Path,
    adapter_manifest_path: Option<&str>,
) -> Option<PathBuf> {
    let rel = adapter_manifest_path?;
    let manifest_abs = manifest_root.join(rel);
    let mut current = manifest_abs.parent()?;
    let root_abs = manifest_root
        .canonicalize()
        .unwrap_or_else(|_| manifest_root.to_path_buf());
    loop {
        if current.join("Cargo.toml").is_file() {
            return Some(current.to_path_buf());
        }
        let current_abs = current
            .canonicalize()
            .unwrap_or_else(|_| current.to_path_buf());
        if current_abs == root_abs {
            return None;
        }
        current = current.parent()?;
    }
}

/// Reads the crate name from a `Cargo.toml`, supporting both the inline and `[package]` forms.
///
/// # Errors
/// Returns an error if the manifest cannot be read or its `[package].name` field is missing.
#[inline]
pub fn read_package_name(manifest: &Path) -> Result<String, String> {
    let contents = fs::read_to_string(manifest)
        .map_err(|err| format!("failed to read {}: {err}", manifest.display()))?;
    let table: toml::Value = toml::from_str(&contents)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.display()))?;

    if let Some(name) = table
        .get("package")
        .and_then(|pkg| pkg.get("name"))
        .and_then(|value| value.as_str())
    {
        return Ok(name.to_owned());
    }

    if let Some(name) = table.get("name").and_then(|value| value.as_str()) {
        return Ok(name.to_owned());
    }

    Err(format!(
        "package.name or name missing from {}",
        manifest.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::AdapterExecContext;
    use tempfile::tempdir;

    #[test]
    fn resolve_target_dir_prefers_build_arg_over_env_and_config() {
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join(".cargo")).unwrap();
        fs::write(
            crate_dir.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"from-config\"\n",
        )
        .unwrap();
        let env = [("CARGO_TARGET_DIR".to_owned(), "from-env".to_owned())];
        let ctx = AdapterExecContext::new().with_env(&env);
        let args = ["--target-dir".to_owned(), "from-arg".to_owned()];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("from-arg")),
            CargoTargetDir::Conventional => panic!("expected explicit from the build arg"),
        }
    }

    #[test]
    fn resolve_target_dir_prefers_env_over_config() {
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join(".cargo")).unwrap();
        fs::write(
            crate_dir.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"from-config\"\n",
        )
        .unwrap();
        let env = [("CARGO_TARGET_DIR".to_owned(), "/abs-env".to_owned())];
        let ctx = AdapterExecContext::new().with_env(&env);
        match resolve_cargo_target_dir(crate_dir, &[], &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, PathBuf::from("/abs-env")),
            CargoTargetDir::Conventional => panic!("expected explicit from the env"),
        }
    }

    #[test]
    fn resolve_target_dir_reads_config_when_no_arg_or_env() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
        let crate_dir = workspace.join("crates/server");
        fs::create_dir_all(crate_dir.join(".cargo")).unwrap();
        fs::write(
            crate_dir.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"cfg-out\"\n",
        )
        .unwrap();
        let ctx = AdapterExecContext::new();
        match resolve_cargo_target_dir(&crate_dir, &[], &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("cfg-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from config.toml"),
        }
    }

    #[test]
    fn resolve_target_dir_is_conventional_without_any_override() {
        let dir = tempdir().unwrap();
        let ctx = AdapterExecContext::new();
        assert!(matches!(
            resolve_cargo_target_dir(dir.path(), &[], &ctx),
            CargoTargetDir::Conventional
        ));
    }

    #[test]
    fn resolve_target_dir_reads_config_build_target_dir_arg() {
        // `cargo build --config build.target-dir="custom"` redirects output
        // exactly like `--target-dir`; the value is TOML (quoted).
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        let ctx = AdapterExecContext::new();
        let args = [
            "--config".to_owned(),
            "build.target-dir=\"custom\"".to_owned(),
        ];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("custom")),
            CargoTargetDir::Conventional => panic!("expected explicit from --config"),
        }
    }

    #[test]
    fn resolve_target_dir_config_build_arg_last_occurrence_wins() {
        // Cargo lets the LAST `--config build.target-dir` win.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        let ctx = AdapterExecContext::new();
        let args = [
            "--config".to_owned(),
            "build.target-dir=\"first\"".to_owned(),
            "--config".to_owned(),
            "build.target-dir=\"second\"".to_owned(),
        ];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("second")),
            CargoTargetDir::Conventional => panic!("expected explicit"),
        }
    }

    #[test]
    fn resolve_target_dir_target_dir_arg_last_occurrence_wins() {
        let dir = tempdir().unwrap();
        let ctx = AdapterExecContext::new();
        let args = [
            "--target-dir".to_owned(),
            "first".to_owned(),
            "--target-dir=second".to_owned(),
        ];
        match resolve_cargo_target_dir(dir.path(), &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, dir.path().join("second")),
            CargoTargetDir::Conventional => panic!("expected explicit"),
        }
    }

    #[test]
    fn resolve_target_dir_reads_config_file_arg() {
        // `--config <file>` loads the file's `[build] target-dir`. cargo
        // resolves a config-relative path against the PARENT of the
        // directory containing the config file, so a file at
        // `<crate>/cfg/alt-config.toml` resolves `file-out` against
        // `<crate>` -- NOT against `<crate>/cfg`.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join("cfg")).unwrap();
        fs::write(
            crate_dir.join("cfg/alt-config.toml"),
            "[build]\ntarget-dir = \"file-out\"\n",
        )
        .unwrap();
        let ctx = AdapterExecContext::new();
        let args = ["--config".to_owned(), "cfg/alt-config.toml".to_owned()];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("file-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from --config <file>"),
        }
    }

    #[test]
    fn resolve_target_dir_reads_spaced_inline_config() {
        // cargo parses `--config <toml>` as a TOML expression, so a spaced
        // `build.target-dir = "x"` must be honoured exactly like the
        // unspaced `build.target-dir="x"` -- not dropped as a file path.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        let ctx = AdapterExecContext::new();
        let args = [
            "--config".to_owned(),
            "build.target-dir = \"spaced-out\"".to_owned(),
        ];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("spaced-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from spaced inline --config"),
        }
    }

    #[test]
    fn resolve_target_dir_follows_config_include() {
        // A `--config <file>` whose `[build] target-dir` lives in an
        // `include`d file must still be found. The base file is nested so
        // the parent-of-parent resolution lands on `<crate>`.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join("cfg")).unwrap();
        fs::write(
            crate_dir.join("cfg/base.toml"),
            "include = \"included.toml\"\n",
        )
        .unwrap();
        fs::write(
            crate_dir.join("cfg/included.toml"),
            "[build]\ntarget-dir = \"inc-out\"\n",
        )
        .unwrap();
        let ctx = AdapterExecContext::new();
        let args = ["--config".to_owned(), "cfg/base.toml".to_owned()];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("inc-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from included config"),
        }
    }

    #[test]
    fn resolve_target_dir_config_include_array_last_entry_wins() {
        // Cargo merges later `include` entries on top of earlier ones, so a
        // target-dir declared in the LAST include must win over an earlier
        // one.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join("cfg")).unwrap();
        fs::write(
            crate_dir.join("cfg/base.toml"),
            "include = [\"first.toml\", \"second.toml\"]\n",
        )
        .unwrap();
        fs::write(
            crate_dir.join("cfg/first.toml"),
            "[build]\ntarget-dir = \"first-out\"\n",
        )
        .unwrap();
        fs::write(
            crate_dir.join("cfg/second.toml"),
            "[build]\ntarget-dir = \"second-out\"\n",
        )
        .unwrap();
        let ctx = AdapterExecContext::new();
        let args = ["--config".to_owned(), "cfg/base.toml".to_owned()];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(
                path,
                crate_dir.join("second-out"),
                "the LAST include must win"
            ),
            CargoTargetDir::Conventional => panic!("expected explicit from include array"),
        }
    }

    #[test]
    fn resolve_target_dir_accepts_quoted_inline_key() {
        // Cargo accepts quoted dotted keys in inline `--config`; a hand-rolled
        // key scan rejected the quotes and mis-treated it as a file path.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        let ctx = AdapterExecContext::new();
        let args = [
            "--config".to_owned(),
            "build.\"target-dir\" = \"quoted-out\"".to_owned(),
        ];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("quoted-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from quoted inline key"),
        }
    }

    #[test]
    fn resolve_target_dir_follows_inline_config_include() {
        // An inline `--config 'include="..."'` must be followed too, not just
        // a `--config <file>` include.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path();
        fs::create_dir_all(crate_dir.join("cfg")).unwrap();
        fs::write(
            crate_dir.join("cfg/included.toml"),
            "[build]\ntarget-dir = \"inline-inc-out\"\n",
        )
        .unwrap();
        let ctx = AdapterExecContext::new();
        let args = [
            "--config".to_owned(),
            "include=\"cfg/included.toml\"".to_owned(),
        ];
        match resolve_cargo_target_dir(crate_dir, &args, &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, crate_dir.join("inline-inc-out")),
            CargoTargetDir::Conventional => panic!("expected explicit from inline include"),
        }
    }

    #[test]
    fn resolve_target_dir_honours_ctx_cargo_home_for_global_config() {
        // The global-config lookup must honour a ctx-provided CARGO_HOME (the
        // env the build actually ran with), not only the process env.
        let dir = tempdir().unwrap();
        let crate_dir = dir.path().join("proj");
        let cargo_home = dir.path().join("custom-cargo-home");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::create_dir_all(&cargo_home).unwrap();
        fs::write(
            cargo_home.join("config.toml"),
            "[build]\ntarget-dir = \"/abs-global-out\"\n",
        )
        .unwrap();
        let cargo_home_str = cargo_home.to_string_lossy().into_owned();
        let env = [("CARGO_HOME".to_owned(), cargo_home_str)];
        let ctx = AdapterExecContext::new().with_env(&env);
        match resolve_cargo_target_dir(&crate_dir, &[], &ctx) {
            CargoTargetDir::Explicit(path) => {
                assert_eq!(path, PathBuf::from("/abs-global-out"));
            }
            CargoTargetDir::Conventional => {
                panic!("expected explicit from ctx CARGO_HOME global config")
            }
        }
    }

    #[test]
    fn resolve_target_dir_reads_cargo_build_target_dir_env() {
        // `CARGO_BUILD_TARGET_DIR` is the `[build] target-dir` env alias.
        let dir = tempdir().unwrap();
        let env = [(
            "CARGO_BUILD_TARGET_DIR".to_owned(),
            "/abs-build-env".to_owned(),
        )];
        let ctx = AdapterExecContext::new().with_env(&env);
        match resolve_cargo_target_dir(dir.path(), &[], &ctx) {
            CargoTargetDir::Explicit(path) => assert_eq!(path, PathBuf::from("/abs-build-env")),
            CargoTargetDir::Conventional => panic!("expected explicit from CARGO_BUILD_TARGET_DIR"),
        }
    }

    #[test]
    fn declared_manifest_wins_over_ambient_discovery() {
        // When the context carries the declared adapter manifest, the
        // adapter must use it verbatim and NOT run its scan closure.
        let declared = Path::new("/proj/crates/server/spin.toml");
        let ctx = AdapterExecContext::new().with_adapter_manifest(declared);
        let got = declared_or_discovered_manifest(&ctx, || {
            panic!("discovery must not run when a manifest is declared")
        })
        .expect("declared path returned");
        assert_eq!(got, declared);
    }

    #[test]
    fn discovery_runs_only_when_no_manifest_declared() {
        let ctx = AdapterExecContext::new();
        let discovered = Path::new("/scanned/spin.toml").to_path_buf();
        let got =
            declared_or_discovered_manifest(&ctx, || Ok(discovered.clone())).expect("discovered");
        assert_eq!(got, discovered);
    }

    #[test]
    fn read_adapter_crate_name_returns_package_name_from_sibling_cargo_toml() {
        // The common case: `[adapters.axum.adapter].manifest =
        // "crates/server/axum.toml"` with a package name of
        // `server` at `crates/server/Cargo.toml`. The helper must
        // return `Some("server")` so the synthesiser emits
        // `crate = "server"` in the resulting axum.toml.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let crate_dir = root.join("crates/server");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let out = read_adapter_crate_name(root, Some("crates/server/axum.toml"));
        assert_eq!(out.as_deref(), Some("server"));
    }

    #[test]
    fn read_adapter_crate_name_walks_up_to_nested_manifest_crate_root() {
        // The spec allows the adapter manifest to sit anywhere
        // inside the adapter crate (spec §"[adapters.<name>.adapter]").
        // A common shape is `crates/server/config/axum.toml` — the
        // manifest's parent is `crates/server/config/`, which has NO
        // Cargo.toml. The helper must walk upward and find
        // `crates/server/Cargo.toml` with `[package].name = "server"`
        // so the synthesiser emits `crate = "server"` (not the
        // fallback `<app>-adapter-axum`).
        let dir = tempdir().unwrap();
        let root = dir.path();
        let crate_dir = root.join("crates/server");
        fs::create_dir_all(crate_dir.join("config")).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let out = read_adapter_crate_name(root, Some("crates/server/config/axum.toml"));
        assert_eq!(
            out.as_deref(),
            Some("server"),
            "helper must walk from the manifest parent up to the first Cargo.toml"
        );
    }

    #[test]
    fn read_adapter_crate_name_stops_at_manifest_root() {
        // Bound the walk at `manifest_root` — we must not leak up
        // into the user's home directory or workspace parents when
        // no Cargo.toml exists inside the project. The test seeds
        // NO Cargo.toml under the tempdir but WOULD find one further
        // up the real filesystem; the helper must return None
        // regardless.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/server/config")).unwrap();
        // No Cargo.toml anywhere under `root`.

        let out = read_adapter_crate_name(root, Some("crates/server/config/spin.toml"));
        assert!(
            out.is_none(),
            "helper must not walk above manifest_root: {out:?}"
        );
    }

    #[test]
    fn read_adapter_crate_name_returns_none_when_cargo_toml_missing() {
        // First-run scaffold path: the adapter manifest hasn't been
        // laid down yet, so the synthesiser must fall back to its
        // scaffold-convention default. Represented here by
        // pointing at a nested manifest whose sibling Cargo.toml
        // doesn't exist yet.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("crates/pending")).unwrap();
        // No Cargo.toml written under crates/pending/.

        let out = read_adapter_crate_name(root, Some("crates/pending/spin.toml"));
        assert!(out.is_none(), "missing Cargo.toml must yield None: {out:?}");
    }

    #[test]
    fn read_adapter_crate_name_returns_none_when_manifest_path_unset() {
        // `[adapters.<name>.adapter].manifest` is optional in
        // `edgezero.toml`. When omitted, the helper has nothing to
        // read and must return `None` so the caller falls back to
        // its scaffold convention.
        let dir = tempdir().unwrap();
        let out = read_adapter_crate_name(dir.path(), None);
        assert!(out.is_none());
    }

    #[test]
    fn read_crate_name_at_returns_none_when_crate_undeclared() {
        // No `.crate` declared -> caller falls back to ancestor / scaffold.
        let dir = tempdir().unwrap();
        assert_eq!(read_crate_name_at(dir.path(), None), Ok(None));
    }

    #[test]
    fn read_crate_name_at_reads_declared_crate_package_name() {
        let dir = tempdir().unwrap();
        let crate_dir = dir.path().join("crates/server");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\n",
        )
        .unwrap();
        assert_eq!(
            read_crate_name_at(dir.path(), Some("crates/server")),
            Ok(Some("server".to_owned()))
        );
    }

    #[test]
    fn read_crate_name_at_errors_when_declared_crate_cargo_toml_unreadable() {
        // `.crate` is declared but its Cargo.toml is missing. This is a
        // hard error, NOT a silent fallback that would mispoint the
        // synthesised fields at an ancestor package.
        let dir = tempdir().unwrap();
        let err = read_crate_name_at(dir.path(), Some("crates/missing"))
            .expect_err("a declared but unreadable crate must be fatal");
        assert!(
            err.contains("crates/missing") && err.contains("unreadable Cargo.toml"),
            "error names the declared crate: {err}"
        );
    }

    #[test]
    fn workspace_root_defaults_to_dir_when_no_cargo_toml() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested");
        fs::create_dir_all(&child).unwrap();

        assert_eq!(find_workspace_root(&child), child);
    }

    #[test]
    fn workspace_root_finds_nearest_manifest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();

        assert_eq!(find_workspace_root(&child), root);
    }

    #[test]
    fn workspace_root_stops_at_workspace_table() {
        let dir = tempdir().unwrap();
        let outer = dir.path();

        // Outer repo root with a Cargo.toml
        fs::write(
            outer.join("Cargo.toml"),
            "[workspace]\nmembers = [\"examples/*\"]",
        )
        .unwrap();

        // Inner workspace (e.g. examples/app-demo)
        let inner = outer.join("examples/app-demo");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            inner.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]",
        )
        .unwrap();

        // Crate inside the inner workspace
        let crate_dir = inner.join("crates/my-adapter");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"my-adapter\"",
        )
        .unwrap();

        // Should resolve to the inner workspace, not the outer repo root.
        assert_eq!(find_workspace_root(&crate_dir), inner);
    }

    #[test]
    fn path_distance_counts_divergence() {
        let left = Path::new("/a/b/c");
        let right = Path::new("/a/b/d/e");
        assert_eq!(path_distance(left, right), 3);
    }

    #[test]
    fn read_package_prefers_package_table() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"demo\"\n").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    #[test]
    fn read_package_falls_back_to_name() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "name = \"demo\"").unwrap();
        let name = read_package_name(&manifest).unwrap();
        assert_eq!(name, "demo");
    }

    #[test]
    fn find_manifest_upwards_matches_manifest_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let child = root.join("nested/level");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]").unwrap();
        fs::write(root.join("demo.toml"), "[cfg]\n").unwrap();

        let found = find_manifest_upwards(&child, "demo.toml").expect("manifest");
        assert_eq!(found, root.join("demo.toml"));
    }

    #[test]
    fn run_native_cli_missing_program_surfaces_install_hint() {
        let err = run_native_cli("edgezero-no-such-program-xyz", &[], "install the thing")
            .expect_err("missing program must error");
        assert!(err.contains("install the thing"), "got: {err}");
    }

    #[test]
    fn run_native_cli_nonzero_exit_is_error() {
        // `false` exits non-zero on every supported CI host (unix/macOS).
        let err = run_native_cli("false", &[], "hint").expect_err("non-zero exit must error");
        // Pin the exit-status branch specifically — `!is_empty()` would
        // also pass for the wrong (not-found / spawn) branch.
        assert!(
            err.contains("exited with status"),
            "expected the exit-status branch, got: {err}"
        );
    }
}
