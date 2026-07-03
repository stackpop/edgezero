//! Process-env loader used by `run_serve` to expose the
//! provision-written `.env` file to the spawned adapter.
//!
//! "Existing env wins" — a line whose key is already set in the
//! caller's environment is skipped. The `.env` file supplies
//! defaults for the local run; the caller's environment is the
//! source of truth.
//!
//! Distinct from `edgezero_adapter::env_file`, which owns the
//! dedup + append logic for provision-side WRITES. This module
//! only goes file → `std::env` for local dev consumption at
//! `edgezero serve` startup.

use std::env;
use std::fs;
use std::path::Path;

/// Load `KEY=value` lines from `path` into the process environment.
///
/// Line rules:
/// * Leading whitespace on the line is trimmed.
/// * Empty lines and lines starting with `#` are skipped.
/// * Lines without an `=` are skipped silently.
/// * Whitespace around the value is trimmed, then one surrounding
///   pair of double quotes (if any) is stripped.
/// * If the key is already present in the process environment, the
///   line is skipped — existing env wins.
///
/// # Errors
///
/// Returns an error string when the file cannot be read.
pub(crate) fn load_into_process_env(path: &Path) -> Result<(), String> {
    let raw = fs::read_to_string(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    for line in raw.lines() {
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
        if env::var(key).is_ok() {
            continue;
        }
        let trimmed_val = val_raw.trim();
        let value = trimmed_val
            .strip_prefix('"')
            .and_then(|stripped| stripped.strip_suffix('"'))
            .unwrap_or(trimmed_val);
        env::set_var(key, value);
    }
    Ok(())
}
