//! Path containment for CLI entry points that resolve
//! manifest-declared paths and let adapters write files through
//! them. See spec §"Path containment (MUST)".

use std::fs;
use std::io::ErrorKind;
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
        // Portable rejection of Windows-shaped rooted / drive-relative
        // paths that `Path::is_absolute()` misses (regardless of the
        // host OS — a manifest file authored on Windows and shared
        // via a repo would otherwise bypass the guard on Unix hosts):
        //   - `\outside\spin.toml`: rooted but no drive prefix, so
        //     `Path::is_absolute()` returns false on both platforms.
        //     `Path::new("\\...")` on Unix normalises to a relative
        //     component with an embedded backslash rather than a
        //     leading root, so the `starts_with(&root)` check below
        //     doesn't catch it either.
        //   - `D:outside\spin.toml`: drive-relative (D:'s current
        //     directory, not D:'s root). `Path::is_absolute()`
        //     returns false; `Component::Prefix(_)` DOES appear on
        //     Windows for the `D:` prefix, but not on Unix.
        //
        // Match on the raw string to be OS-agnostic.
        if raw.starts_with('\\') || raw.starts_with('/') {
            return Err(format!(
                "{label} must not start with a directory separator; \
                 got rooted-without-drive `{raw}` (Windows rooted paths without a \
                 drive prefix bypass `Path::is_absolute()` and would escape the \
                 project root)"
            ));
        }
        if is_windows_drive_prefixed(raw) {
            return Err(format!(
                "{label} must not carry a Windows drive prefix; got `{raw}` \
                 (drive-relative paths like `D:outside` bypass `Path::is_absolute()`)"
            ));
        }
        // Belt-and-braces: any `Component::Prefix` (Windows-only under
        // std) also rejected. `Component::ParentDir` handled below.
        if candidate
            .components()
            .any(|comp| matches!(comp, Component::Prefix(_)))
        {
            return Err(format!(
                "{label} must not carry a path prefix component; got `{raw}`"
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
        // Symlink rejection: walk each component of the resolved
        // path (relative to `project_root`) and reject if any
        // existing intermediate is a symlink. `Component`s that
        // haven't been created yet on disk are fine — the adapter
        // will materialise them itself from EdgeZero-owned code.
        // The Step-1 lexical check only forbids literal `..` and
        // absolute paths, so an operator who plants
        // `crates/worker` as a symlink pointing at
        // `/tmp/outside/crate` still passes: `starts_with(root)` is
        // true because the string starts with the root, but
        // adapter `fs::write` calls follow the symlink and touch
        // `/tmp/outside/crate/...`. Reject the symlink component
        // BEFORE dispatch so the adapter never gets a chance to
        // escape.
        //
        // Note: this closes the ambient-file surface but is not a
        // full TOCTOU guard. A concurrent attacker who plants the
        // symlink between this check and the adapter's write can
        // still race. Fully closing that would require
        // directory-relative opens (`openat` + `O_NOFOLLOW`)
        // threaded through every adapter — tracked as follow-up
        // scope.
        if do_step1_starts_with {
            let joined = root.join(candidate);
            reject_symlink_components(&root, &joined, label)?;
        } else {
            // `root == "."` — walk from cwd via the candidate
            // directly.
            reject_symlink_components(Path::new("."), candidate, label)?;
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

/// True when the raw string carries a Windows drive prefix
/// (`A:`..=`Z:` or the lowercase equivalent), regardless of what
/// follows. Catches both drive-absolute (`C:\foo`) and
/// drive-relative (`D:foo`) shapes uniformly. Portable across
/// host platforms — we check the byte pattern, not
/// `Component::Prefix` (which is Windows-only under std).
fn is_windows_drive_prefixed(raw: &str) -> bool {
    let mut bytes = raw.bytes();
    matches!(
        (bytes.next(), bytes.next()),
        (Some(drive_letter), Some(b':')) if drive_letter.is_ascii_alphabetic()
    )
}

/// Walk from `start` inward one component at a time along
/// `candidate` and reject if any existing intermediate reports
/// `is_symlink()`. Components that don't exist yet (first-run
/// bootstrap where the adapter crate isn't materialised on disk)
/// are fine — the adapter will create them from EdgeZero-owned
/// code, and the strict-local invariant already forces the
/// resulting write path to sit inside the crate dir.
///
/// `symlink_metadata` does not follow symlinks; a broken symlink
/// still reports `is_symlink() == true`, so we catch dangling
/// links too.
///
/// `label` identifies the offending manifest field for the error
/// message ("`[adapters.<name>.adapter].manifest`" or ".crate").
fn reject_symlink_components(start: &Path, candidate: &Path, label: &str) -> Result<(), String> {
    let mut walk = start.to_path_buf();
    for comp in candidate
        .strip_prefix(start)
        .unwrap_or(candidate)
        .components()
    {
        walk.push(comp.as_os_str());
        match fs::symlink_metadata(&walk) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!(
                    "{label} resolves through a symlink at `{}`; \
                     symlinks in manifest-declared paths would let an adapter's \
                     `fs::write` follow the link off the project tree. Replace the \
                     symlink with a regular directory (or a symlink to a path INSIDE \
                     the project root — the guard walks each component individually)",
                    walk.display()
                ));
            }
            // Missing intermediate is fine — the adapter creates
            // it from EdgeZero-owned code.
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            // Ok(md) where md is not a symlink: continue walking.
            // Any other Err (permission denied, invalid input,
            // etc.) is surfaced as a hard error rather than
            // silently letting the path through.
            Ok(_) => {}
            Err(err) => {
                return Err(format!(
                    "{label}: failed to inspect `{}` for symlink safety: {err}",
                    walk.display()
                ));
            }
        }
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

    // ---------- Symlink rejection (PR #287 review blocking #2) ----------

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_adapter_crate_directory() {
        // Reviewer regression: `crate = "crates/worker"` passes the
        // lexical check when `crates/worker` is a symlink to a
        // directory outside the project — subsequent adapter
        // `fs::write` calls follow the symlink and mutate the
        // external target. The guard now rejects any symlink
        // component in the path.
        use std::os::unix::fs::symlink;
        let project = tempfile::TempDir::new().expect("project");
        let escape = tempfile::TempDir::new().expect("escape target");
        let crates_dir = project.path().join("crates");
        fs::create_dir_all(&crates_dir).unwrap();
        symlink(escape.path(), crates_dir.join("worker")).unwrap();

        // Plant a valid manifest inside the (symlinked) crate dir
        // so the strict-local containment check would otherwise
        // pass; the symlink rejection has to fire first.
        fs::write(escape.path().join("wrangler.toml"), "").unwrap();

        let err = assert_provision_paths_contained(
            project.path(),
            Some("crates/worker/wrangler.toml"),
            Some("crates/worker"),
        )
        .unwrap_err();
        assert!(
            err.contains("through a symlink"),
            "symlinked crate must be rejected: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_in_manifest_intermediate_component() {
        // The bypass also works when the CRATE dir is a real
        // directory but a sub-directory INSIDE it is a symlink
        // (e.g. `crates/worker` is real, but
        // `crates/worker/config` is a symlink into /tmp). The
        // reject walks every component.
        use std::os::unix::fs::symlink;
        let project = tempfile::TempDir::new().expect("project");
        let escape = tempfile::TempDir::new().expect("escape target");
        let worker = project.path().join("crates/worker");
        fs::create_dir_all(&worker).unwrap();
        symlink(escape.path(), worker.join("config")).unwrap();
        fs::write(escape.path().join("wrangler.toml"), "").unwrap();

        let err = assert_provision_paths_contained(
            project.path(),
            Some("crates/worker/config/wrangler.toml"),
            Some("crates/worker"),
        )
        .unwrap_err();
        assert!(
            err.contains("through a symlink"),
            "symlinked intermediate must be rejected: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn safe_variant_also_rejects_symlinked_paths() {
        // The safe variant guards cloud dispatch (which reads the
        // manifest for service-id lookup but doesn't write local
        // files). Symlink following would still let an adapter
        // read from outside the project — reject there too.
        use std::os::unix::fs::symlink;
        let project = tempfile::TempDir::new().expect("project");
        let escape = tempfile::TempDir::new().expect("escape target");
        let crates_dir = project.path().join("crates");
        fs::create_dir_all(&crates_dir).unwrap();
        symlink(escape.path(), crates_dir.join("worker")).unwrap();
        fs::write(escape.path().join("wrangler.toml"), "").unwrap();

        let err = assert_provision_paths_safe(
            project.path(),
            Some("crates/worker/wrangler.toml"),
            Some("crates/worker"),
        )
        .unwrap_err();
        assert!(err.contains("through a symlink"), "safe variant: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn accepts_regular_directory_and_first_run_missing_paths() {
        // Sanity: a plain directory tree (no symlinks) passes.
        // Also: the first-run scaffold path — the adapter crate
        // doesn't exist on disk yet — must not fail the walk.
        // `symlink_metadata` returns NotFound; the guard stops
        // walking and returns Ok.
        let project = tempfile::TempDir::new().expect("project");
        let crates_dir = project.path().join("crates");
        fs::create_dir_all(&crates_dir).unwrap();
        fs::create_dir_all(crates_dir.join("worker")).unwrap();
        // First run: no wrangler.toml file yet.
        assert_provision_paths_contained(
            project.path(),
            Some("crates/worker/wrangler.toml"),
            Some("crates/worker"),
        )
        .unwrap();
        // Really-first run: crate dir doesn't exist yet either.
        assert_provision_paths_contained(
            project.path(),
            Some("crates/newcrate/wrangler.toml"),
            Some("crates/newcrate"),
        )
        .unwrap();
    }

    // ---------- Windows-shape rejection (PR #287 review P1) ----------

    #[test]
    fn rejects_backslash_rooted_manifest_regardless_of_host_os() {
        // `\outside\spin.toml` is rooted-without-drive on Windows.
        // `Path::is_absolute()` returns false there (and on Unix
        // treats the whole string as a single relative component
        // with embedded backslashes), so both hosts previously let
        // it through. Manifest files shared via a repo can carry
        // this shape from a Windows author onto a Unix host too.
        let err = assert_provision_paths_safe(Path::new("."), Some(r"\outside\spin.toml"), None)
            .unwrap_err();
        assert!(
            err.contains("directory separator"),
            "rooted-without-drive rejected: {err}"
        );

        let err_c =
            assert_provision_paths_safe(Path::new("."), None, Some(r"\outside")).unwrap_err();
        assert!(err_c.contains("directory separator"), "crate: {err_c}");
    }

    #[test]
    fn rejects_forward_slash_rooted_manifest_when_is_absolute_would_miss_it() {
        // Symmetric belt-and-braces: any leading `/` is rejected as
        // a rooted path. Note `Path::new("/foo").is_absolute()` IS
        // true on Unix (already caught), but the explicit
        // string-prefix check makes the rejection consistent
        // regardless of host, and pins the intent so a future
        // refactor to `PathBuf` semantics doesn't drop it.
        let err =
            assert_provision_paths_safe(Path::new("."), Some("/etc/spin.toml"), None).unwrap_err();
        assert!(
            err.contains("must be a project-relative path") || err.contains("directory separator"),
            "leading `/` rejected: {err}"
        );
    }

    #[test]
    fn rejects_windows_drive_prefixed_manifest() {
        // `D:outside\spin.toml` is drive-relative (`D:`'s CWD).
        // `Path::is_absolute()` returns false on both hosts.
        // `Component::Prefix(_)` only appears on Windows for the
        // `D:` prefix; on Unix the whole thing is one component.
        // Match the raw byte prefix so both hosts reject.
        for poisoned in ["D:outside\\spin.toml", "D:outside/spin.toml", "C:foo"] {
            let err =
                assert_provision_paths_safe(Path::new("."), Some(poisoned), None).unwrap_err();
            assert!(
                err.contains("drive prefix"),
                "drive-relative `{poisoned}` rejected: {err}"
            );
        }
    }

    #[test]
    fn rejects_windows_drive_absolute_manifest() {
        // `C:\foo` IS absolute on Windows (`Path::is_absolute()`
        // returns true) so the outer `is_absolute()` check catches
        // it there. On Unix the same string looks relative and the
        // drive-prefix check catches it. Either way: rejected.
        let err = assert_provision_paths_safe(Path::new("."), Some(r"C:\wrangler.toml"), None)
            .unwrap_err();
        assert!(
            err.contains("must be a project-relative path")
                || err.contains("drive prefix")
                || err.contains("directory separator"),
            "drive-absolute path rejected: {err}"
        );
    }

    #[test]
    fn accepts_ordinary_relative_paths_that_happen_to_contain_backslashes_mid_string() {
        // Sanity: only LEADING backslash triggers rejection. A
        // pathological Unix filename that legitimately contains
        // backslashes in the middle (rare but legal) still passes.
        // On Windows the same string decomposes into components
        // and there's no rooted / drive prefix, so it also passes.
        assert_provision_paths_safe(
            Path::new("."),
            Some(r"crates/weird\file/wrangler.toml"),
            Some("crates"),
        )
        .unwrap();
    }
}
