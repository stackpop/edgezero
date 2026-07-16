//! `.env` file loader used by `run_serve` to expose the
//! provision-written `.env` file to the spawned adapter.
//!
//! **Pre-2026-07-09 (PR #287 review, blocking #3)** this module
//! wrote into `std::env::set_var` directly. On Unix, `setenv`/
//! `getenv` are not thread-safe: they operate on the shared
//! `environ` array with no synchronisation. A multithreaded
//! downstream process calling `run_serve` from one thread while
//! another thread reads `std::env::var(...)` observes a torn
//! read that the C standard flags as undefined behaviour. The
//! repository's own `shared_test_guards` + `edgezero_core::test_env`
//! guards document this constraint for tests (and edition 2024 marks
//! `env::set_var` `unsafe` for exactly this reason) — the same rule
//! applies at runtime.
//!
//! The parser returns an owned `Vec<(String, String)>` overlay
//! that `run_serve` threads into the spawned child via
//! `std::process::Command::env(...)`. Each `Command::env` call
//! stores the entry in an internal per-process map — no shared
//! state, no `setenv`. The child inherits the overlay at exec
//! time.
//!
//! "Existing env wins" (a line whose key is already set in the
//! caller's environment is dropped) is enforced by the caller,
//! not here, so the parser stays pure and testable without a
//! process-env guard.
//!
//! Distinct from `edgezero_adapter::env_file`, which owns the
//! dedup + append logic for provision-side WRITES. This module
//! only parses file → overlay for local dev consumption at
//! `edgezero serve` startup.

use std::fs;
use std::path::Path;

/// Parse `KEY=value` lines from `path` into an owned
/// `(key, value)` overlay.
///
/// Line rules:
/// * Leading whitespace on the line is trimmed.
/// * Empty lines and lines starting with `#` are skipped.
/// * Lines without an `=` are skipped silently.
/// * Lines whose key contains a NUL byte or `=` are REJECTED
///   with an error (both are `Command::env` contract violations
///   and would either panic or silently misroute the value).
/// * Whitespace around the value is trimmed, then one surrounding
///   pair of double quotes (if any) is stripped.
/// * Values containing NUL are rejected with an error (same
///   reason).
/// * The caller decides whether an entry whose key is already
///   present in the process environment survives — the parser
///   emits every well-formed line.
///
/// # Errors
///
/// Returns an error string when the file cannot be read or a
/// line carries a NUL / `=` in the key or a NUL in the value.
pub(crate) fn parse_env_overlay(path: &Path) -> Result<Vec<(String, String)>, String> {
    let raw = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let mut out: Vec<(String, String)> = Vec::new();
    for (line_no_zero, line) in raw.lines().enumerate() {
        let line_no = line_no_zero.saturating_add(1);
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key_raw, val_raw)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key_raw.trim();
        if key.is_empty() {
            continue;
        }
        if key.as_bytes().contains(&0) {
            return Err(format!(
                "{}:{line_no}: env-file key contains a NUL byte, which is invalid on the \
                 target platform",
                path.display()
            ));
        }
        let trimmed_val = val_raw.trim();
        let value = trimmed_val
            .strip_prefix('"')
            .and_then(|stripped| stripped.strip_suffix('"'))
            .unwrap_or(trimmed_val);
        if value.as_bytes().contains(&0) {
            return Err(format!(
                "{}:{line_no}: env-file value for `{key}` contains a NUL byte",
                path.display()
            ));
        }
        out.push((key.to_owned(), value.to_owned()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_env_file(dir: &TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join(".env");
        let mut file = File::create(&path).expect("create .env");
        file.write_all(contents.as_bytes()).expect("write");
        path
    }

    #[test]
    fn parses_key_value_lines_into_ordered_overlay() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_env_file(
            &dir,
            "# banner\nA=first\nB=\"quoted\"\n\n  C=indented\n=no_key\nkey_without_eq\n",
        );
        let overlay = parse_env_overlay(&path).expect("parse succeeds");
        assert_eq!(
            overlay,
            vec![
                ("A".to_owned(), "first".to_owned()),
                ("B".to_owned(), "quoted".to_owned()),
                ("C".to_owned(), "indented".to_owned()),
            ]
        );
    }

    #[test]
    fn rejects_nul_byte_in_key() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_env_file(&dir, "A\0B=value\n");
        let err = parse_env_overlay(&path).expect_err("NUL key must error");
        assert!(err.contains("NUL byte"), "{err}");
    }

    #[test]
    fn rejects_nul_byte_in_value() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_env_file(&dir, "KEY=first\0second\n");
        let err = parse_env_overlay(&path).expect_err("NUL value must error");
        assert!(err.contains("NUL byte"), "{err}");
    }

    #[test]
    fn missing_file_yields_readable_error() {
        use std::path::Path;
        let err = parse_env_overlay(Path::new("/this/does/not/exist.env"))
            .expect_err("missing file must error");
        assert!(err.contains("/this/does/not/exist.env"), "{err}");
    }
}
