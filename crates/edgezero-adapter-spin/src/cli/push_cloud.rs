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
                "blob at key `{key}` is {} bytes — exceeds the {MAX_ARGV_BYTES_PER_INVOCATION}-byte safe-argv-per-invocation cap for `spin cloud key-value set`. Restructure your typed app-config into multiple types and split across [stores.config] ids (spec 9.4).",
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
    let total = entries.len();
    // Cursor into `entries`. Chunks are built in input order by
    // `chunk_entries` and contain `chunk.len()` entries each, so
    // committed entries are always `entries[0..cursor]` and the
    // current chunk's source entries are
    // `entries[cursor..cursor + chunk.len()]`. This lets us produce
    // a Fastly-shaped partial-failure diagnostic when a mid-stream
    // chunk shellout fails.
    let mut cursor: usize = 0;
    for chunk in chunks {
        let chunk_len = chunk.len();
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
            cursor = cursor.saturating_add(chunk_len);
            continue;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_ascii_lowercase();
        let chunk_end = cursor.saturating_add(chunk_len);
        // `.get(..)` keeps strict-clippy's `indexing_slicing` happy
        // without changing semantics — `cursor` and `chunk_end` are
        // produced from `entries.len()` and `chunk_entries`, both of
        // which guarantee the ranges are in-bounds (so a `None` from
        // `.get` is unreachable). Fall through to an empty slice if
        // the invariant is ever violated rather than panicking on a
        // hot error path.
        let committed: Vec<&str> = entries
            .get(..cursor)
            .unwrap_or(&[])
            .iter()
            .map(|(key, _value)| key.as_str())
            .collect();
        let failed_chunk: Vec<&str> = entries
            .get(cursor..chunk_end)
            .unwrap_or(&[])
            .iter()
            .map(|(key, _value)| key.as_str())
            .collect();
        let not_attempted: Vec<&str> = entries
            .get(chunk_end..)
            .unwrap_or(&[])
            .iter()
            .map(|(key, _value)| key.as_str())
            .collect();
        // Mirror Fastly's diagnostic shape so the operator can resume
        // from a known boundary: which keys are already on Fermyon
        // Cloud, which keys are in the failing chunk's commit-or-not
        // state (the cloud API is atomic per shellout, so a non-zero
        // exit means none of `failed` made it), and which keys never
        // had `set` attempted at all.
        let partial = if cursor > 0 || !not_attempted.is_empty() {
            format!(
                "\n  Committed (safe to skip on retry): {committed:?}\n  Failed chunk: {failed_chunk:?}\n  Not attempted (re-push these): {not_attempted:?}\n  Resume: re-run with only the failed + not-attempted keys, or with all entries (set is idempotent — committed keys will just be overwritten with the same value)."
            )
        } else {
            String::new()
        };
        if stderr_lower.contains("not logged in") || stderr_lower.contains("authentication") {
            return Err(format!(
                "`spin cloud key-value set` reports not authenticated. Run `spin cloud login` and retry. Stderr: {stderr}{partial}"
            ));
        }
        if stderr_lower.contains("not linked")
            || stderr_lower.contains("no store linked")
            || stderr_lower.contains("link")
        {
            // `spin cloud link key-value` takes the label POSITIONALLY
            // (`<LABEL>`) and the cloud store name via `--store
            // <STORE>` per fermyon/cloud-plugin's
            // `src/commands/link.rs::KeyValueStoreLinkCommand`. There
            // is NO `--label` flag on link (despite there being one
            // on `set`).
            return Err(format!(
                "`spin cloud key-value set --app {app_name} --label {label}` reports that the label is not linked to a cloud KV store. Run `spin cloud link key-value --app {app_name} --store <store-name> {label}` (or create + link via the Fermyon dashboard) and retry. Stderr: {stderr}{partial}"
            ));
        }
        return Err(format!(
            "`spin cloud key-value set --app {app_name} --label {label} <{chunk_len} pairs>` failed (status {status}) after committing {committed_count} of {total} entries: {stderr}{partial}",
            status = output.status.code().unwrap_or(-1),
            committed_count = cursor,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use edgezero_core::test_env::PathPrepend;

    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::io::Write as _;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(unix)]
    use std::path::Path as StdPath;
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use tempfile::{TempDir, tempdir};

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

    // ---------- Hermetic mock-spin argv capture ----------
    //
    // `chunk_entries` + `format_pair` cover the per-pair / chunking
    // logic in isolation, but they don't prove `write_batch`
    // assembles the EXACT argv we promise: `spin cloud key-value set
    // --app <APP> --label <LABEL> KEY=VALUE [KEY=VALUE …]`. Past
    // command-shape regressions (the `--store` mistake before the
    // app-scoped label fix) would have been caught by an
    // end-to-end argv test. These tests stand up a temp dir with a
    // fake `spin` script that records its argv to a sibling file,
    // prepend it to `PATH`, and assert the captured argv matches.

    /// Variant of [`fake_spin`] that succeeds on the first `n - 1`
    /// invocations and fails on the `n`-th. Used to drive the
    /// partial-failure diagnostic in `write_batch` — chunk 1
    /// succeeds, chunk 2 fails, chunk 3+ are never attempted. Uses
    /// a counter file in the tempdir so the script can tell which
    /// invocation it is.
    #[cfg(unix)]
    fn fake_spin_fail_on_nth(
        out_path: &StdPath,
        fail_on_invocation: u32,
        stderr_to_emit: &str,
    ) -> TempDir {
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("spin");
        let stderr_path = dir.path().join("stderr.txt");
        let counter_path = dir.path().join("invocations.txt");
        fs::write(&stderr_path, stderr_to_emit).expect("write stderr payload");
        let mut script = fs::File::create(&script_path).expect("create script");
        write!(
            script,
            r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> '{out}'; done
n=$(cat '{counter}' 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > '{counter}'
if [ "$n" = "{fail_on}" ]; then
  cat '{stderr_file}' >&2
  exit 1
fi
exit 0
"#,
            out = out_path.display(),
            counter = counter_path.display(),
            fail_on = fail_on_invocation,
            stderr_file = stderr_path.display(),
        )
        .expect("write script body");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// Build a tempdir containing a script named `spin` that
    /// records its argv (one per line) to `out_path` and exits 0.
    /// Returns the tempdir (so it stays alive for the test's
    /// lifetime) and the path of the script.
    #[cfg(unix)]
    fn fake_spin(out_path: &StdPath, stderr_to_emit: Option<&str>, exit_code: i32) -> TempDir {
        let dir = tempdir().expect("tempdir");
        let script_path = dir.path().join("spin");
        // Write the stderr payload to a SEPARATE file rather than
        // interpolating it into the script body — payloads contain
        // backticks, `$`, and other shell-active characters
        // (`` `app_config` `` etc.) that would otherwise be
        // re-interpreted by the shell and corrupt the capture.
        let stderr_path = dir.path().join("stderr.txt");
        if let Some(payload) = stderr_to_emit {
            fs::write(&stderr_path, payload).expect("write stderr payload");
        }
        let mut script = fs::File::create(&script_path).expect("create script");
        // /bin/sh writes each arg on its own line. `printf '%s\n'`
        // is portable across macOS / Linux (unlike `echo`).
        write!(
            script,
            r#"#!/bin/sh
for arg in "$@"; do printf '%s\n' "$arg" >> '{out}'; done
if [ -f '{stderr_file}' ]; then cat '{stderr_file}' >&2; fi
exit {exit}
"#,
            out = out_path.display(),
            stderr_file = stderr_path.display(),
            exit = exit_code,
        )
        .expect("write script body");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        dir
    }

    /// A process-wide mutex serialising `PATH`-mutating tests in
    /// this module so two parallel tests don't race on the env.
    /// Delegates to the shared `cli::env_mutation_guard()` so this
    /// suite also serialises against `provision_local`'s PATH-
    /// mutating tests (both suites prepend a fake `spin` shim).
    #[cfg(unix)]
    fn path_mutation_guard() -> &'static Mutex<()> {
        super::super::env_mutation_guard()
    }

    #[cfg(unix)]
    #[test]
    fn write_batch_assembles_app_label_keyvalue_argv_against_a_mock_spin() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let out_dir = tempdir().expect("out dir");
        let argv_log = out_dir.path().join("argv.txt");
        let fake_dir = fake_spin(&argv_log, None, 0);
        let _path = PathPrepend::new(fake_dir.path());

        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("svc.timeout".to_owned(), "1500".to_owned()),
        ];
        write_batch("my-app", "app_config", &entries).expect("mock spin returns 0");

        let argv_contents = fs::read_to_string(&argv_log).expect("read argv");
        let args: Vec<&str> = argv_contents.lines().collect();
        assert_eq!(
            args,
            vec![
                "cloud",
                "key-value",
                "set",
                "--app",
                "my-app",
                "--label",
                "app_config",
                "greeting=hello",
                "svc.timeout=1500",
            ],
            "argv shape must match Fermyon's documented `spin cloud key-value set --app APP --label LABEL KEY=VALUE` form"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_batch_translates_not_logged_in_stderr_to_actionable_error() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let out_dir = tempdir().expect("out dir");
        let argv_log = out_dir.path().join("argv.txt");
        let fake_dir = fake_spin(&argv_log, Some("error: not logged in to Fermyon Cloud"), 1);
        let _path = PathPrepend::new(fake_dir.path());

        let err = write_batch("my-app", "app_config", &[("k".to_owned(), "v".to_owned())])
            .expect_err("non-zero exit must surface as error");
        assert!(
            err.contains("spin cloud login"),
            "auth-error path must suggest `spin cloud login`: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_batch_translates_unlinked_stderr_to_actionable_link_hint() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let out_dir = tempdir().expect("out dir");
        let argv_log = out_dir.path().join("argv.txt");
        let fake_dir = fake_spin(
            &argv_log,
            Some("error: label `app_config` is not linked to a store"),
            1,
        );
        let _path = PathPrepend::new(fake_dir.path());

        let err = write_batch("my-app", "app_config", &[("k".to_owned(), "v".to_owned())])
            .expect_err("unlinked label must surface as error");
        // The hint must use the corrected Fermyon syntax: `--app
        // <APP> --store <STORE> <LABEL>` (positional label, NOT
        // `--label`). The full error includes BOTH the failing SET
        // command (which legitimately uses `--label app_config`)
        // AND the LINK hint, so the negative check must scope to
        // the link region.
        assert!(
            err.contains("spin cloud link key-value")
                && err.contains("--app my-app")
                && err.contains("--store <store-name>"),
            "hint must suggest the corrected link command shape: {err}"
        );
        let link_start = err
            .find("spin cloud link key-value")
            .expect("link hint present");
        // String indexing here is safe: `find` returns a char-boundary
        // byte offset, and `spin cloud link key-value` is ASCII so
        // slicing from that offset can't split a UTF-8 character.
        // Take everything from "link key-value" up to the next
        // sentence-boundary (the parenthetical fallback) so we
        // don't accidentally re-include the SET command's
        // `--label` arg.
        let link_tail = err.get(link_start..).expect("link tail");
        let link_scope = link_tail.split('(').next().unwrap_or(link_tail);
        assert!(
            !link_scope.contains("--label"),
            "the `link key-value` command takes the label POSITIONALLY, not via --label. Link-region of error: {link_scope}"
        );
        // Sanity: the link region ENDS with the positional label.
        assert!(
            link_scope.trim_end().ends_with("app_config`") || link_scope.contains(" app_config "),
            "link region must mention the positional label `app_config`: {link_scope}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_batch_chunks_large_batch_into_multiple_invocations_against_mock_spin() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let out_dir = tempdir().expect("out dir");
        let argv_log = out_dir.path().join("argv.txt");
        let fake_dir = fake_spin(&argv_log, None, 0);
        let _path = PathPrepend::new(fake_dir.path());

        // Same fixture as `chunk_entries_splits_when_aggregate_exceeds_cap`
        // -- 7 x ~30 KiB entries split into >=3 chunks. The mock
        // records each invocation's argv, separated by repeats of
        // the static header (`cloud key-value set --app ...`); the
        // header appears once per invocation, so we count
        // occurrences.
        let big = "z".repeat(30_usize * 1024_usize);
        let entries: Vec<(String, String)> = (0_u32..7_u32)
            .map(|i| (format!("k{i}"), big.clone()))
            .collect();
        write_batch("my-app", "app_config", &entries).expect("mock spin");

        let argv_contents = fs::read_to_string(&argv_log).expect("read argv");
        let mid_set_count = argv_contents.matches("\nset\n").count();
        let starts_with_set = usize::from(argv_contents.starts_with("cloud\nkey-value\nset\n"));
        let invocations = mid_set_count.saturating_add(starts_with_set);
        assert!(
            invocations >= 3,
            "expected >=3 mock invocations from chunked batch, got {invocations} (argv log: {argv_contents})"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_batch_partial_failure_reports_committed_failed_and_not_attempted_keys() {
        // Cloud pushes shell out once per chunk, and Fermyon Cloud's
        // `set` is atomic per shellout — so if chunk 1 succeeds and
        // chunk 2 fails, chunk 1's keys are live in Cloud while
        // chunk 3+ never had `set` attempted. Mirror the Fastly
        // diagnostic shape: name committed / failed / not-attempted
        // so the operator can resume from a known boundary.
        let _lock = path_mutation_guard().lock().expect("guard");
        let out_dir = tempdir().expect("out dir");
        let argv_log = out_dir.path().join("argv.txt");
        // Fail on the 2nd invocation: chunk 1 commits, chunk 2 errors,
        // chunk 3 (if produced) never runs.
        let fake_dir = fake_spin_fail_on_nth(&argv_log, 2, "error: backend write failed");
        let _path = PathPrepend::new(fake_dir.path());

        // Same shape as the chunking test — 7 x ~30 KiB entries
        // produce >=3 chunks.
        let big = "z".repeat(30_usize * 1024_usize);
        let entries: Vec<(String, String)> = (0_u32..7_u32)
            .map(|i| (format!("k{i}"), big.clone()))
            .collect();

        let err =
            write_batch("my-app", "app_config", &entries).expect_err("second chunk must fail");

        // Diagnostic must call out all three buckets.
        assert!(
            err.contains("Committed (safe to skip on retry):"),
            "must label committed bucket: {err}"
        );
        assert!(
            err.contains("Failed chunk:"),
            "must label failed-chunk bucket: {err}"
        );
        assert!(
            err.contains("Not attempted (re-push these):"),
            "must label not-attempted bucket: {err}"
        );
        // After committing some entries, the count must appear.
        assert!(
            err.contains("after committing"),
            "must surface committed count in header: {err}"
        );
        // Resume hint references idempotency.
        assert!(
            err.contains("Resume:") && err.contains("idempotent"),
            "must include resume hint: {err}"
        );
        // Underlying stderr is preserved so the operator sees the
        // real failure reason.
        assert!(
            err.contains("backend write failed"),
            "must include upstream stderr: {err}"
        );
        // Sanity: at least one key from the input must show up in
        // the committed list (specifically `k0`, which lives in the
        // first chunk).
        assert!(
            err.contains("\"k0\""),
            "committed bucket must include at least `k0` from chunk 1: {err}"
        );
    }

    // ---------- read_config_entry: Fermyon Cloud branch ----------

    /// Branch 2: `read_config_entry` returns `Unsupported` when the
    /// deploy command indicates Fermyon Cloud (no per-key `get` in
    /// the cloud CLI as of v1).
    #[test]
    fn read_config_entry_returns_unsupported_for_fermyon_cloud_deploy_cmd() {
        use crate::cli::SpinCliAdapter;
        use edgezero_adapter::registry::{
            Adapter as _, AdapterPushContext, ReadConfigEntry, ResolvedStoreId,
        };
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("spin.toml"),
            "spin_manifest_version = 2\n[application]\nname = \"x\"\nversion = \"0\"\n[component.demo]\nsource = \"a.wasm\"\n",
        )
        .expect("write spin.toml");
        let mut ctx = AdapterPushContext::new();
        ctx.manifest_adapter_deploy_cmd = Some("spin deploy");
        let result = SpinCliAdapter
            .read_config_entry(
                dir.path(),
                Some("spin.toml"),
                None,
                &ResolvedStoreId::new("app_config".to_owned(), "app_config".to_owned()),
                "greeting",
                &ctx,
            )
            .expect("cloud branch returns Ok(Unsupported)");
        assert!(
            matches!(result, ReadConfigEntry::Unsupported(_)),
            "Fermyon Cloud must return Unsupported"
        );
    }
}
