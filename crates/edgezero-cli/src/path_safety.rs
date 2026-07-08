//! Path containment for CLI entry points that resolve
//! manifest-declared paths and let adapters write files through
//! them. See spec §"Path containment (MUST)".

use std::path::{Component, Path, PathBuf};

/// Reject absolute paths and `..` traversal for the
/// `[adapters.<name>.adapter].manifest` and `.crate` strings, then
/// assert:
///   1. each path resolves under the project root (defence in depth);
///   2. when both `.crate` and `.manifest` are set, the manifest
///      path resolves under the crate path -- the spec's
///      stronger promise that local provision never creates
///      files outside the adapter crate or its gitignored
///      local-state dirs.
///
/// Callers SHOULD pass the absolute manifest-loader root when
/// they have it, but the helper defensively normalises so a
/// relative `args.manifest.parent()` ("" or "examples/app-demo")
/// compares correctly.
/// Security-only subset of [`assert_provision_paths_contained`]:
/// runs Step 1 (absolute-path rejection, `..` traversal rejection,
/// under-project-root containment) but skips Step 2 (manifest must
/// sit inside the adapter crate dir). Callers that only care about
/// path-traversal safety and NOT the "local write stays under the
/// adapter crate" invariant should use this variant. Notably: cloud
/// `config push` -- vendor CLI dispatch, no local file writes, but
/// the adapter still joins `manifest_root` with the declared
/// manifest path for service-id lookup, so path traversal is still
/// a real risk.
pub(crate) fn assert_provision_paths_safe(
    project_root: &Path,
    adapter_manifest_path: Option<&str>,
    adapter_crate_path: Option<&str>,
) -> Result<(), String> {
    assert_provision_paths_impl(
        project_root,
        adapter_manifest_path,
        adapter_crate_path,
        /* strict_local = */ false,
    )
}

pub(crate) fn assert_provision_paths_contained(
    project_root: &Path,
    adapter_manifest_path: Option<&str>,
    adapter_crate_path: Option<&str>,
) -> Result<(), String> {
    assert_provision_paths_impl(
        project_root,
        adapter_manifest_path,
        adapter_crate_path,
        /* strict_local = */ true,
    )
}

fn assert_provision_paths_impl(
    project_root: &Path,
    adapter_manifest_path: Option<&str>,
    adapter_crate_path: Option<&str>,
    strict_local: bool,
) -> Result<(), String> {
    // Treat "" as ".": Path::parent() returns "" for a bare
    // `--manifest edgezero.toml`, and Path::new("").join(...) does
    // NOT prepend anything, so starts_with would fail silently.
    let root_raw = if project_root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        project_root
    };
    let root = lexical_normalize(root_raw);
    // When `root` normalises to "." (caller passed "" or "." --
    // a bare `--manifest edgezero.toml` or an explicit
    // cwd-relative path), the joined-vs-root `starts_with`
    // check is structurally broken: `lexical_normalize` strips
    // the leading `./` from the join, leaving e.g.
    // `crates/cf/wrangler.toml` -- which does NOT start with
    // ".". Skip Step 1's containment check in that case; the
    // absolute + `..` rejection below already guarantees the
    // candidate sits under cwd, and Step 2 (manifest-inside-
    // crate) compares two paths that BOTH go through the same
    // normalisation so the leading-dot strip cancels out
    // there. The relative-root test fixtures
    // (`accepts_relative_root_default`,
    // `accepts_empty_root_string_as_dot`) only pass with this
    // short-circuit in place.
    let do_step1_starts_with = root != Path::new(".");

    // Step 1: each path is project-relative + no `..` + (when
    // root is concretely-rooted) resolves under the project root.
    for (label, maybe_raw) in [
        ("[adapters.<name>.adapter].manifest", adapter_manifest_path),
        ("[adapters.<name>.adapter].crate", adapter_crate_path),
    ] {
        let Some(raw) = maybe_raw else { continue };
        let candidate = Path::new(raw);
        if candidate.is_absolute() {
            return Err(format!(
                "{label} must be a project-relative path; got absolute `{raw}`"
            ));
        }
        if candidate
            .components()
            .any(|comp| matches!(comp, Component::ParentDir))
        {
            return Err(format!(
                "{label} must not contain `..` traversal; got `{raw}`"
            ));
        }
        if do_step1_starts_with {
            let normalized = lexical_normalize(&root.join(candidate));
            if !normalized.starts_with(&root) {
                return Err(format!(
                    "{label} resolves outside project root `{}`: `{}`",
                    root.display(),
                    normalized.display()
                ));
            }
        }
    }

    // Step 2 (strict-local only): BOTH `.manifest` AND `.crate` must
    // be declared, AND the manifest must resolve inside the adapter
    // crate dir. Closes the spec's stronger promise for local-mode
    // writes:
    //   - Without `.manifest`, the adapter synth's
    //     `PathBuf::from("<default>.toml")` fallback lands generated
    //     manifests at the project root instead of under the crate,
    //     and `read_adapter_crate_name` can't walk up to the crate's
    //     Cargo.toml (it has no manifest path to start from).
    //   - Without `.crate`, we can't PROVE the manifest lives inside
    //     the adapter crate — a permissive `.manifest = "wrangler.toml"`
    //     at project root would silently land generated wrangler.toml
    //     outside any adapter crate, again defeating the
    //     read_adapter_crate_name upward walk and the containment
    //     invariant.
    //   - When both are set, the manifest MUST resolve inside the
    //     crate dir. Otherwise `crate = "crates/cf"` +
    //     `manifest = "tmp/wrangler.toml"` would pass Step 1 but
    //     write outside the adapter crate.
    //
    // Cloud dispatch does not run this step — legitimate cloud
    // fixtures use e.g. `manifest = "wrangler.toml"` at the project
    // root alongside a crate under `crates/`, which is safe for
    // vendor-CLI dispatch even though it fails the strict-local
    // containment.
    if !strict_local {
        return Ok(());
    }
    let Some(manifest_raw) = adapter_manifest_path else {
        return Err(
            "[adapters.<name>.adapter].manifest is required for `--local` provision \
             and config paths; without it the synthesiser falls back to a project-\
             root filename outside the adapter crate and cannot honour a renamed \
             adapter crate's Cargo.toml"
                .to_owned(),
        );
    };
    let Some(crate_raw) = adapter_crate_path else {
        return Err(
            "[adapters.<name>.adapter].crate is required for `--local` provision \
             and config paths; without it the CLI cannot prove the declared \
             manifest lives inside the adapter crate directory, and \
             `read_adapter_crate_name`'s upward walk has no crate-root anchor"
                .to_owned(),
        );
    };
    let crate_resolved = lexical_normalize(&root.join(Path::new(crate_raw)));
    let manifest_resolved = lexical_normalize(&root.join(Path::new(manifest_raw)));
    if !manifest_resolved.starts_with(&crate_resolved) {
        return Err(format!(
            "[adapters.<name>.adapter].manifest `{manifest_raw}` must \
             resolve inside [adapters.<name>.adapter].crate `{crate_raw}`; \
             resolved manifest path `{}` is not under crate path `{}`",
            manifest_resolved.display(),
            crate_resolved.display()
        ));
    }
    Ok(())
}

/// Lexically normalise: collapse `.` components and pass `..`
/// through unchanged (caller already rejected `..`). No
/// `fs::canonicalize` -- paths may not exist on first-run
/// bootstrap, and canonicalising would resolve operator-set
/// symlinks on the project root.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Prefix(_)
            | Component::RootDir
            | Component::ParentDir
            | Component::Normal(_) => out.push(comp.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn rejects_absolute_manifest_path() {
        let err =
            assert_provision_paths_contained(Path::new("."), Some("/etc/wrangler.toml"), None)
                .unwrap_err();
        assert!(err.contains("must be a project-relative path"), "{err}");
    }

    #[test]
    fn rejects_parent_traversal_in_manifest_path() {
        let err =
            assert_provision_paths_contained(Path::new("."), Some("../outside/spin.toml"), None)
                .unwrap_err();
        assert!(err.contains("must not contain `..` traversal"), "{err}");
    }

    #[test]
    fn rejects_parent_traversal_in_crate_path() {
        let err =
            assert_provision_paths_contained(Path::new("."), None, Some("../escape")).unwrap_err();
        assert!(err.contains("must not contain `..` traversal"), "{err}");
    }

    #[test]
    fn accepts_relative_root_default() {
        assert_provision_paths_contained(
            Path::new("."),
            Some("crates/edgezero-adapter-spin/spin.toml"),
            Some("crates/edgezero-adapter-spin"),
        )
        .unwrap();
    }

    #[test]
    fn accepts_nested_relative_root() {
        assert_provision_paths_contained(
            Path::new("examples/app-demo"),
            Some("crates/app-demo-adapter-spin/spin.toml"),
            Some("crates/app-demo-adapter-spin"),
        )
        .unwrap();
    }

    #[test]
    fn accepts_empty_root_string_as_dot() {
        // args.manifest.parent() returns "" for a bare `--manifest edgezero.toml`.
        // Both .manifest and .crate declared per the strict-local rule.
        assert_provision_paths_contained(
            Path::new(""),
            Some("crates/edgezero-adapter-spin/spin.toml"),
            Some("crates/edgezero-adapter-spin"),
        )
        .unwrap();
    }

    #[test]
    fn rejects_manifest_outside_adapter_crate() {
        // Crate = "crates/cf", but manifest = "tmp/wrangler.toml"
        // (sibling of the crate, NOT inside it). Step 1 passes
        // (both under project root); step 2 must catch the
        // crate-vs-manifest mismatch.
        let err = assert_provision_paths_contained(
            Path::new("."),
            Some("tmp/wrangler.toml"),
            Some("crates/cf"),
        )
        .unwrap_err();
        assert!(err.contains("must resolve inside"), "{err}");
    }

    #[test]
    fn accepts_manifest_under_adapter_crate() {
        assert_provision_paths_contained(
            Path::new("."),
            Some("crates/cf/wrangler.toml"),
            Some("crates/cf"),
        )
        .unwrap();
    }

    #[test]
    fn rejects_missing_manifest_when_crate_declared_in_local_mode() {
        // Regression: an operator who declares `[adapters.<x>.adapter].crate
        // = "crates/server"` but forgets `.manifest = "..."` would slip past
        // the pre-2026-07 containment guard (Step 2 short-circuited on the
        // missing `.manifest`). Downstream, `Adapter::synthesise_baseline_manifest`
        // falls back to `PathBuf::from("axum.toml")` at the project root —
        // OUTSIDE `crates/server` — and `read_adapter_crate_name` cannot walk
        // up to the crate's Cargo.toml (it has no manifest path to start from),
        // so the emitted manifest points at the wrong Cargo package too.
        //
        // Strict-local now rejects a missing `.manifest` outright so both
        // failure modes are impossible.
        let err = assert_provision_paths_contained(Path::new("."), None, Some("crates/server"))
            .unwrap_err();
        assert!(
            err.contains("[adapters.<name>.adapter].manifest is required for `--local`"),
            "missing .manifest must be rejected in local mode: {err}"
        );
    }

    #[test]
    fn rejects_missing_manifest_even_without_declared_crate_in_local_mode() {
        // The invariant is `--local always writes to a declared manifest
        // path`, not `--local always names an adapter crate`. An operator
        // who leaves BOTH knobs unset falls back to root-level defaults —
        // still a containment leak.
        let err = assert_provision_paths_contained(Path::new("."), None, None).unwrap_err();
        assert!(
            err.contains("[adapters.<name>.adapter].manifest is required for `--local`"),
            "missing .manifest must be rejected even when .crate is also unset: {err}"
        );
    }

    #[test]
    fn rejects_missing_crate_when_manifest_declared_in_local_mode() {
        // Regression: an earlier iteration of the guard permitted
        // `.crate = None` as long as `.manifest` was set — reasoning
        // that the manifest path itself was under the project root.
        // But WITHOUT `.crate`, the CLI cannot PROVE the manifest
        // lives inside any adapter crate: an operator who writes
        // `[adapters.cloudflare.adapter].manifest = "wrangler.toml"`
        // at the project root would slip past the containment
        // invariant. `read_adapter_crate_name`'s upward walk also
        // has no crate-root anchor without `.crate`.
        //
        // Strict-local now requires BOTH fields.
        let err =
            assert_provision_paths_contained(Path::new("."), Some("crates/cf/wrangler.toml"), None)
                .unwrap_err();
        assert!(
            err.contains("[adapters.<name>.adapter].crate is required for `--local`"),
            "missing .crate must be rejected in local mode: {err}"
        );
    }

    #[test]
    fn safe_variant_still_allows_missing_manifest_and_missing_crate() {
        // Cloud dispatch (assert_provision_paths_safe) legitimately runs
        // without `.manifest` and without `.crate` for adapters that
        // manage manifests via their vendor CLI. Missing-field rejection
        // is strict-local only.
        assert_provision_paths_safe(Path::new("."), None, Some("crates/cf")).unwrap();
        assert_provision_paths_safe(Path::new("."), None, None).unwrap();
        assert_provision_paths_safe(Path::new("."), Some("crates/cf/wrangler.toml"), None).unwrap();
    }
}
