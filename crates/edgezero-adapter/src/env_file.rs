//! Line-oriented env-file dedup shared by all adapters that
//! write provision-owned `.env` / `.dev.vars` files. Key-
//! normalised: a line whose key matches an existing commented
//! OR uncommented entry is skipped. See spec §"Merge mechanics"
//! → "Line-oriented".

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Schema-version header prepended to every provision-written
/// line-oriented file (`.env`, `.dev.vars`). Matches the header
/// synthesised TOML files (`wrangler.toml`, `fastly.toml`,
/// `spin.toml`, `runtime-config.toml`) carry. Kept as a single
/// crate-level constant so a future spec bump touches one line.
pub const EDGEZERO_PROVISION_HEADER: &str = "# edgezero-provision: v1";

/// Append each `<key>=<value>` line iff its normalised key does
/// NOT already appear in the file (commented OR uncommented).
/// Existing lines are preserved byte-for-byte. Creates the file
/// (and parent dirs) when absent.
///
/// # Errors
/// Returns an error string when the file cannot be read, when
/// the parent directory cannot be created, or when the write
/// fails.
#[inline]
pub fn append_lines_dedup(path: &Path, new_lines: &[String], dry_run: bool) -> Result<(), String> {
    append_lines_dedup_with_header(path, None, new_lines, dry_run)
}

/// Same as [`append_lines_dedup`], but also ensures the file's first
/// content line is `header`. When `Some(hdr)` and the existing file
/// does not already contain a trimmed line matching `hdr`, the header
/// is prepended to the write output. Matches the spec's schema-
/// version-header contract: each provision-written line-oriented
/// file starts with `# edgezero-provision: v1` (or the equivalent
/// version comment), and re-provision does not duplicate it.
///
/// The header is compared to existing lines via trimmed-equality —
/// `normalised_key` returns `None` for comment-only lines like the
/// header, so the ordinary dedup path can't self-check it.
///
/// # Errors
/// Same as [`append_lines_dedup`].
#[inline]
pub fn append_lines_dedup_with_header(
    path: &Path,
    header: Option<&str>,
    new_lines: &[String],
    dry_run: bool,
) -> Result<(), String> {
    let mut existing = String::new();
    if path.exists() {
        existing =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    }
    let existing_keys: BTreeSet<String> = existing.lines().filter_map(normalised_key).collect();

    // Header decision: prepend only when the caller asked for one AND
    // the existing file has no trimmed-equal line already. Empty files
    // ("" plus absent) count as "no header present" so a fresh
    // provision writes it.
    let header_needed = header.filter(|hdr| {
        let trimmed_hdr = hdr.trim();
        !existing.lines().any(|line| line.trim() == trimmed_hdr)
    });

    let mut to_append = String::new();
    for line in new_lines {
        // Reject embedded newlines: a KEY=VALUE with a `\n` in the
        // VALUE would split into TWO lines in the emitted file, the
        // second of which the runtime env-loader picks up as an
        // arbitrary KEY=VALUE injection (spec §"env-file format
        // integrity"). The value is either an operator-provided env
        // override or a placeholder we control; both must be single-
        // line by contract.
        if line.contains('\n') || line.contains('\r') {
            return Err(format!(
                "refusing to write line-oriented entry with embedded newline/carriage-return to {} -- \
                 an env-var value with `\\n` or `\\r` would split into multiple lines and let a \
                 downstream reader see an unintended KEY=VALUE injection",
                path.display()
            ));
        }
        let Some(key) = normalised_key(line) else {
            continue;
        };
        if existing_keys.contains(&key) {
            continue;
        }
        to_append.push_str(line);
        if !line.ends_with('\n') {
            to_append.push('\n');
        }
    }

    // Nothing to do when there are neither new dedup'd lines nor a
    // missing header to prepend. `dry_run` short-circuits any write.
    if (to_append.is_empty() && header_needed.is_none()) || dry_run {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
    }

    let mut combined = String::new();
    if let Some(hdr) = header_needed {
        combined.push_str(hdr);
        if !hdr.ends_with('\n') {
            combined.push('\n');
        }
    }
    combined.push_str(&existing);
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&to_append);
    fs::write(path, combined).map_err(|err| format!("write {}: {err}", path.display()))?;

    // Restrictive permissions: `.env` / `.dev.vars` / `.edgezero/.env`
    // are the operator's secret-carriage files (Cloudflare runtime
    // secrets, Spin SPIN_VARIABLE_* placeholders, Axum
    // `<key_value>=` seeds). fs::write creates with mode 0666 & ~umask
    // which is typically 0644 on Unix -- readable by every other UID
    // on the host. Tighten to 0600 after the write so only the
    // invoking user can read the placeholders and any real values the
    // operator later fills in. Best-effort: a permission-set failure
    // is not fatal (some filesystems, e.g. NFS mounts, may reject
    // chmod), but we surface the error string so the operator can
    // investigate.
    #[cfg(unix)]
    set_restrictive_mode(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_restrictive_mode(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("chmod 0600 {}: {err}", path.display()))
}

/// Strip at most ONE leading `#` + adjacent whitespace, then
/// parse `<key>=<value>` and return the trimmed key. Returns
/// `None` for blank lines and comment-only lines.
///
/// Single-`#` semantics matter: `## KEY=value` (double hash —
/// the markdown-style heading shape some operators use as
/// section separators inside `.env` files) is NOT treated as
/// a commented `KEY=value` line; it returns `Some("# KEY")`
/// (with the second `#` embedded in the key) so dedup does NOT
/// collapse `## KEY=v` and `KEY=v` into each other.
pub(crate) fn normalised_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // Strip exactly ONE leading `#`, then any whitespace that
    // follows it — `# KEY=value`, `#KEY=value`, and `KEY=value`
    // all normalise to the same key; `## KEY` does NOT.
    let after_hash = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let stripped = after_hash.trim_start();
    let (raw_key, _) = stripped.split_once('=')?;
    let key = raw_key.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn appends_new_lines_and_skips_existing_keys() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "AAA=existing\n").unwrap();
        append_lines_dedup(&path, &["AAA=NEW".to_owned(), "BBB=NEW".to_owned()], false).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        // AAA stays at the operator value; BBB appended.
        assert!(after.contains("AAA=existing"));
        assert!(after.contains("BBB=NEW"));
        assert!(!after.contains("AAA=NEW"));
    }

    #[test]
    fn dedup_treats_commented_and_uncommented_form_as_same_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        // Operator already uncommented + edited the override line.
        fs::write(&path, "EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=staging\n").unwrap();
        // Re-provision would otherwise re-add the commented form.
        append_lines_dedup(
            &path,
            &["# EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY=app_config".to_owned()],
            false,
        )
        .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        let occurrences = after
            .lines()
            .filter(|line| {
                normalised_key(line).as_deref() == Some("EDGEZERO__STORES__CONFIG__APP_CONFIG__KEY")
            })
            .count();
        assert_eq!(
            occurrences, 1,
            "commented override must NOT reappear: {after}"
        );
    }

    #[test]
    fn dry_run_makes_no_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "KEEP=me\n").unwrap();
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        append_lines_dedup(&path, &["NEW=x".to_owned()], true).unwrap();
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn normalised_key_strips_at_most_one_leading_hash() {
        // Uncommented and single-hash forms dedup against each other:
        assert_eq!(normalised_key("KEY=v"), Some("KEY".into()));
        assert_eq!(normalised_key("#KEY=v"), Some("KEY".into()));
        assert_eq!(normalised_key("# KEY=v"), Some("KEY".into()));
        assert_eq!(normalised_key("  # KEY=v"), Some("KEY".into()));

        // Double-hash leaves the second `#` in the key → DIFFERENT
        // normalised key. Operator section separators using `## …`
        // stay intact.
        assert_eq!(normalised_key("## KEY=v"), Some("# KEY".into()));

        // Comment-only lines return None.
        assert_eq!(normalised_key("# comment"), None);
        assert_eq!(normalised_key("### header"), None);
        assert_eq!(normalised_key(""), None);
    }

    #[test]
    fn rejects_new_lines_with_embedded_newline_to_prevent_env_injection() {
        // An operator whose env-overlay value contains `\n` would
        // otherwise split into a second `KEY=VALUE` line on emit,
        // silently injecting an unintended env-var into the runtime.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        let injection = "EDGEZERO__STORES__KV__SESSIONS__NAME=sess\nMALICIOUS=1".to_owned();
        let err = append_lines_dedup(&path, &[injection], false)
            .expect_err("newline in value must be rejected");
        assert!(
            err.contains("embedded newline"),
            "error names the defect: {err}"
        );
        // File must not have been written.
        assert!(
            !path.exists(),
            "no file created on rejection: {}",
            path.display()
        );
    }

    #[test]
    fn rejects_new_lines_with_embedded_carriage_return() {
        // CR-only injection would work on Windows-style parsers; also
        // reject so the contract is line-terminator-agnostic.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        let injection = "KEY=value\rSECOND=2".to_owned();
        let err = append_lines_dedup(&path, &[injection], false).expect_err("CR must be rejected");
        assert!(
            err.contains("carriage-return"),
            "error names the defect: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn written_file_has_mode_0600_to_protect_operator_secrets() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".dev.vars");
        append_lines_dedup(&path, &["SECRET_KEY=placeholder".to_owned()], false).unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "provision-written env files must be owner-read/write only; got {mode:o}"
        );
    }

    #[test]
    fn creates_file_when_absent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/subdir/.env");
        assert!(!path.exists());
        append_lines_dedup(&path, &["NEW=x".to_owned()], false).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "NEW=x\n");
    }

    #[test]
    fn header_is_prepended_on_first_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        append_lines_dedup_with_header(
            &path,
            Some("# edgezero-provision: v1"),
            &["AAA=1".to_owned()],
            false,
        )
        .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with("# edgezero-provision: v1"),
            "header must be first line: {after}"
        );
        assert!(after.contains("AAA=1"));
    }

    #[test]
    fn header_is_not_reprepended_when_already_present() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# edgezero-provision: v1\nAAA=existing\n").unwrap();
        append_lines_dedup_with_header(
            &path,
            Some("# edgezero-provision: v1"),
            &["BBB=NEW".to_owned()],
            false,
        )
        .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        let header_count = after
            .lines()
            .filter(|line| line.trim() == "# edgezero-provision: v1")
            .count();
        assert_eq!(header_count, 1, "header must appear exactly once: {after}");
        assert!(after.contains("AAA=existing"));
        assert!(after.contains("BBB=NEW"));
    }

    #[test]
    fn header_is_prepended_when_operator_file_has_no_header() {
        // Operator wrote the file by hand before provision ever ran;
        // a subsequent provision must prepend the header.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "AAA=operator_set\n").unwrap();
        append_lines_dedup_with_header(
            &path,
            Some("# edgezero-provision: v1"),
            &["BBB=NEW".to_owned()],
            false,
        )
        .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(
            after.starts_with("# edgezero-provision: v1"),
            "header must be prepended above operator content: {after}"
        );
        assert!(after.contains("AAA=operator_set"));
        assert!(after.contains("BBB=NEW"));
    }

    #[test]
    fn header_matches_ignore_leading_and_trailing_whitespace() {
        // If the operator hand-indented the header, we still count
        // it as present and don't add a second one.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "  # edgezero-provision: v1  \nAAA=x\n").unwrap();
        append_lines_dedup_with_header(
            &path,
            Some("# edgezero-provision: v1"),
            &["BBB=x".to_owned()],
            false,
        )
        .unwrap();
        let after = fs::read_to_string(&path).unwrap();
        let header_count = after
            .lines()
            .filter(|line| line.trim() == "# edgezero-provision: v1")
            .count();
        assert_eq!(header_count, 1, "trim-equality must dedup: {after}");
    }

    #[test]
    fn header_dry_run_does_not_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".env");
        // File missing entirely — dry-run must NOT create it.
        append_lines_dedup_with_header(
            &path,
            Some("# edgezero-provision: v1"),
            &["AAA=x".to_owned()],
            true,
        )
        .unwrap();
        assert!(!path.exists(), "dry-run must not create file");
    }
}
