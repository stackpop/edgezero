//! Shell out to `spin cloud key-value set` to seed Fermyon Cloud KV
//! stores during `config push --adapter spin`.
//!
//! Fermyon Cloud is the only Spin deployment target with a first-class
//! external KV write API, and it's already gated by `spin cloud login`
//! (the operator authenticates the platform CLI separately, same
//! pattern as `wrangler` for Cloudflare and `fastly` for Fastly).
//!
//! ## Command shape
//!
//! Per [fermyon/cloud-plugin's `src/commands/key_value.rs`](https://github.com/fermyon/cloud-plugin/blob/main/src/commands/key_value.rs)
//! `SetCommand`, the `set` subcommand accepts two mutually-exclusive
//! addressing modes:
//!
//! - `--store STORE KEY=VALUE [...]` — target the cloud KV store by
//!   its actual resource name. Useful when the operator knows the
//!   cloud-side store name directly.
//! - `--app APP --label LABEL KEY=VALUE [...]` — target the cloud KV
//!   store via the app-scoped label that's mapped to it (per
//!   [Fermyon's label model](https://developer.fermyon.com/cloud/linking-applications-to-resources-using-labels)).
//!
//! We use the **app-scoped label model** because that's what
//! `EdgeZero`'s mental model produces: `store.platform` is the env-
//! resolved label written into `spin.toml`'s `key_value_stores`,
//! NOT the cloud-side store resource name. The app name comes from
//! `spin.toml`'s `[application].name`.
//!
//! Key/value pairs are POSITIONAL arguments in `key=value` form
//! (parsed by `spin_common::arg_parser::parse_kv`), and the command
//! accepts MULTIPLE pairs in one invocation — so a 1000-entry batch
//! is one shellout, not 1000.
//!
//! Auto-detection: the dispatcher activates this writer when the
//! manifest's `[adapters.spin.commands].deploy` shells to `spin
//! deploy` or `spin cloud deploy` (operator intent: this app deploys
//! to Fermyon Cloud) AND the operator did NOT pass `--local`.

use std::io;
use std::mem;
use std::process::Command;

/// Approximate worst-case argv size we'll squeeze into ONE `spin cloud
/// key-value set` invocation before chunking into multiple
/// invocations. Linux's typical `ARG_MAX` is 128 KiB-2 MiB; macOS is
/// 256 KiB. Stay well under the floor so a long argv doesn't `E2BIG`.
const MAX_ARGV_BYTES_PER_INVOCATION: usize = 96 * 1024;

/// Detect whether the spin adapter's deploy command targets Fermyon
/// Cloud. Looks for `spin deploy` or `spin cloud deploy` as a substring
/// of the configured command. Substring match (not equality) so a
/// pre-deploy hook like `cd dist && spin deploy --provider …` still
/// trips it.
#[must_use]
pub(crate) fn deploy_command_targets_fermyon_cloud(deploy_cmd: Option<&str>) -> bool {
    let Some(cmd) = deploy_cmd else {
        return false;
    };
    cmd.contains("spin cloud deploy") || cmd.contains("spin deploy")
}

/// Build the `key=value` argv strings for one chunk. Each entry's
/// representation is `<key>=<value>` (the same shape
/// `spin_common::arg_parser::parse_kv` will split back on the FIRST
/// `=`). We don't escape `=` in keys because `parse_kv` splits on the
/// first occurrence — a key containing `=` would silently truncate at
/// the upstream side; we reject early instead.
pub(crate) fn format_pair(key: &str, value: &str) -> Result<String, String> {
    if key.contains('=') {
        return Err(format!(
            "key `{key}` contains `=`, which `spin cloud key-value set`'s `KEY=VALUE` parser would split silently. Rename the config key without `=`."
        ));
    }
    Ok(format!("{key}={value}"))
}

/// Partition `entries` into chunks small enough to keep the total
/// argv size under [`MAX_ARGV_BYTES_PER_INVOCATION`]. Each chunk is a
/// `Vec<String>` of `key=value` strings ready to splice into
/// `Command::args`. Catches both the per-pair size AND the cumulative
/// argv size so a single 256 KiB value bails out with a clear error.
pub(crate) fn chunk_entries(entries: &[(String, String)]) -> Result<Vec<Vec<String>>, String> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_bytes: usize = 0;
    for (key, value) in entries {
        let pair = format_pair(key, value)?;
        if pair.len() >= MAX_ARGV_BYTES_PER_INVOCATION {
            return Err(format!(
                "entry `{key}` is {} bytes — exceeds the {MAX_ARGV_BYTES_PER_INVOCATION}-byte safe-argv-per-invocation cap for `spin cloud key-value set`. Trim the value, or use a different backend (KV-with-blob, or a managed runtime-config backend) for entries this large.",
                pair.len()
            ));
        }
        // saturating_* keeps the strict-clippy `arithmetic_side_effects`
        // lint happy without changing semantics — argv sizes are
        // bounded well below usize::MAX in any realistic shape.
        let pair_overhead = pair.len().saturating_add(1);
        let projected = current_bytes.saturating_add(pair_overhead);
        if !current.is_empty() && projected > MAX_ARGV_BYTES_PER_INVOCATION {
            chunks.push(mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes = current_bytes.saturating_add(pair_overhead);
        current.push(pair);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    Ok(chunks)
}

/// Shell out `spin cloud key-value set --app <APP> --label <LABEL>
/// key1=value1 key2=value2 …` once per chunk. Captures stderr on
/// non-zero exit and surfaces it in the error string. Detects "not
/// logged in" specifically and points the operator at
/// `spin cloud login`.
///
/// `app_name` is the Fermyon Cloud application name (from
/// `spin.toml`'s `[application].name`); `label` is the
/// `key_value_stores` entry that the cloud-side store is linked to.
/// Together they address the cloud KV store through Fermyon's
/// app-scoped label model.
///
/// # Errors
/// Returns a human-readable error string on:
/// - any pair containing `=` in the key (would silently truncate);
/// - any pair whose `key=value` size exceeds the safe-argv cap;
/// - failure to spawn the `spin` binary (typically "command not
///   found" — the install hint points at the Spin install URL);
/// - non-zero exit from `spin cloud key-value set` (captured stderr
///   included);
/// - "not logged in" stderr (suggests `spin cloud login`);
/// - "not linked" / "no store linked to label" stderr (suggests
///   running `spin cloud link key-value` to map the label first).
pub(crate) fn write_batch(
    app_name: &str,
    label: &str,
    entries: &[(String, String)],
) -> Result<(), String> {
    let chunks = chunk_entries(entries)?;
    for chunk in chunks {
        let mut command = Command::new("spin");
        command
            .arg("cloud")
            .arg("key-value")
            .arg("set")
            .arg("--app")
            .arg(app_name)
            .arg("--label")
            .arg(label);
        for pair in &chunk {
            command.arg(pair);
        }
        let output = command.output().map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                "`spin` is not on PATH. Install the Spin CLI (https://spinframework.dev/) and re-run.".to_owned()
            } else {
                format!("failed to invoke `spin cloud key-value set`: {err}")
            }
        })?;

        if output.status.success() {
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_ascii_lowercase();
        if stderr_lower.contains("not logged in") || stderr_lower.contains("authentication") {
            return Err(format!(
                "`spin cloud key-value set` reports not authenticated. Run `spin cloud login` and retry. Stderr: {stderr}"
            ));
        }
        if stderr_lower.contains("not linked")
            || stderr_lower.contains("no store linked")
            || stderr_lower.contains("link")
        {
            return Err(format!(
                "`spin cloud key-value set --app {app_name} --label {label}` reports that the label is not linked to a cloud KV store. Run `spin cloud link key-value --app {app_name} --label {label} <store-name>` (or create + link via the Fermyon dashboard) and retry. Stderr: {stderr}"
            ));
        }
        return Err(format!(
            "`spin cloud key-value set --app {app_name} --label {label} <{} pairs>` failed (status {}): {stderr}",
            chunk.len(),
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_fermyon_cloud_from_spin_deploy() {
        assert!(deploy_command_targets_fermyon_cloud(Some("spin deploy")));
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "spin deploy --from crates/foo"
        )));
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "cd dist && spin deploy"
        )));
    }

    #[test]
    fn detect_fermyon_cloud_from_spin_cloud_deploy() {
        assert!(deploy_command_targets_fermyon_cloud(Some(
            "spin cloud deploy"
        )));
    }

    #[test]
    fn non_cloud_deploy_commands_are_not_detected() {
        assert!(!deploy_command_targets_fermyon_cloud(Some("echo no-op")));
        assert!(!deploy_command_targets_fermyon_cloud(Some(
            "kubectl apply -f spin.yaml"
        )));
        // Sanity: just having "spin" or "deploy" alone doesn't count.
        assert!(!deploy_command_targets_fermyon_cloud(Some("spin build")));
        assert!(!deploy_command_targets_fermyon_cloud(Some("./deploy.sh")));
    }

    #[test]
    fn missing_deploy_command_returns_false() {
        assert!(!deploy_command_targets_fermyon_cloud(None));
    }

    // ---------- argv shape ----------

    #[test]
    fn format_pair_emits_key_equals_value() {
        assert_eq!(format_pair("greeting", "hello").unwrap(), "greeting=hello");
        assert_eq!(
            format_pair("svc.timeout", "1500").unwrap(),
            "svc.timeout=1500"
        );
    }

    #[test]
    fn format_pair_allows_equals_in_value() {
        // `parse_kv` splits on the FIRST `=`, so values may contain
        // additional `=` characters (e.g. base64 padding, JSON).
        assert_eq!(
            format_pair("config_blob", "k=v&x=y").unwrap(),
            "config_blob=k=v&x=y"
        );
    }

    #[test]
    fn format_pair_rejects_equals_in_key() {
        let err = format_pair("bad=key", "value").unwrap_err();
        assert!(
            err.contains("bad=key") && err.contains("`=`"),
            "error names the bad key + the bad char: {err}"
        );
    }

    #[test]
    fn chunk_entries_packs_single_chunk_when_small() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let chunks = chunk_entries(&entries).expect("chunk");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], vec!["a=1", "b=2", "c=3"]);
    }

    #[test]
    fn chunk_entries_handles_empty_slice() {
        assert!(chunk_entries(&[]).unwrap().is_empty());
    }

    #[test]
    fn chunk_entries_splits_when_aggregate_exceeds_cap() {
        // Each pair is ~30 KiB; the cap is 96 KiB so we expect 3 per
        // chunk before needing to split, but with overhead the
        // chunker will pack 3 then move to a new chunk.
        let big = "x".repeat(30_usize * 1024_usize);
        let entries: Vec<(String, String)> = (0_u32..7_u32)
            .map(|i| (format!("k{i}"), big.clone()))
            .collect();
        let chunks = chunk_entries(&entries).expect("chunk");
        // 7 pairs * ~30 KiB = ~210 KiB total; should split into at
        // least 3 chunks (each chunk ~90 KiB before exceeding 96 KiB).
        assert!(
            chunks.len() >= 3,
            "expected >=3 chunks, got {}",
            chunks.len()
        );
        let total: usize = chunks.iter().map(Vec::len).sum();
        assert_eq!(total, 7, "every pair is preserved across chunks");
    }

    #[test]
    fn chunk_entries_rejects_oversized_single_pair() {
        let oversize = "y".repeat(MAX_ARGV_BYTES_PER_INVOCATION + 10);
        let entries = vec![("key".to_owned(), oversize)];
        let err = chunk_entries(&entries).expect_err("oversized single pair must error");
        assert!(
            err.contains("safe-argv-per-invocation cap"),
            "error names the cap: {err}"
        );
    }
}
