//! Line-oriented env-file dedup shared by all adapters that
//! write provision-owned `.env` / `.dev.vars` files. Key-
//! normalised: a line whose key matches an existing commented
//! OR uncommented entry is skipped. See spec §"Merge mechanics"
//! → "Line-oriented".

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

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
    let mut existing = String::new();
    if path.exists() {
        existing =
            fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    }
    let existing_keys: BTreeSet<String> = existing.lines().filter_map(normalised_key).collect();

    let mut to_append = String::new();
    for line in new_lines {
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
    if to_append.is_empty() || dry_run {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("create {}: {err}", parent.display()))?;
        }
    }
    let mut combined = existing;
    if !combined.is_empty() && !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&to_append);
    fs::write(path, combined).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(())
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
    fn creates_file_when_absent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested/subdir/.env");
        assert!(!path.exists());
        append_lines_dedup(&path, &["NEW=x".to_owned()], false).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "NEW=x\n");
    }
}
