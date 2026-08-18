//! Shared `#[cfg(test)]` helpers for the split `cli` test modules.
//!
//! These fixtures build fake `fastly` shim scripts and synthetic
//! config-store listings/envelopes used by the `gc`, `push_cloud`,
//! `push_local`, and `provision_*` test modules. They are centralised here so
//! the split modules share one honest set of fixtures.

#![allow(
    dead_code,
    reason = "shared test fixtures; not every module exercises every helper"
)]

#[cfg(unix)]
use super::FastlyCliAdapter;
#[cfg(unix)]
use crate::chunked_config::prepare_fastly_config_entries;
#[cfg(unix)]
use edgezero_adapter::registry::{Adapter as _, AdapterPushContext, ResolvedStoreId};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use tempfile::tempdir;

/// Invoke `config gc` on the config store at `dir` via the adapter boundary.
#[cfg(unix)]
pub(crate) fn run_gc(
    dir: &Path,
    older_than_secs: u64,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    FastlyCliAdapter.gc_config_entries(
        dir,
        None,
        None,
        &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
        &AdapterPushContext::new(),
        older_than_secs,
        dry_run,
    )
}

// Shared fixture names. Pinning these as consts keeps the setup-vs-assertion
// pair in sync -- a typo in one place no longer silently divorces from the
// other. These are the LOGICAL store ids the fastly adapter operates on.
pub(crate) const TEST_KV_ID: &str = "sessions";
pub(crate) const TEST_CONFIG_ID: &str = "app_config";
pub(crate) const TEST_SECRET_ID: &str = "default";

/// Build a tempdir containing a `fastly` shim that serves a store list with
/// `TEST_CONFIG_ID`, and returns `stdout_body`/`stderr_body`/`exit_code` for
/// describe calls.
#[cfg(unix)]
pub(crate) fn fake_fastly_returning(
    stdout_body: &str,
    stderr_body: &str,
    exit_code: i32,
) -> tempfile::TempDir {
    fake_fastly_returning_with_keys(stdout_body, stderr_body, exit_code, &[])
}

/// As [`fake_fastly_returning`], but also serves `config-store-entry list`
/// with a bare array of the `entry_list_keys` as `item_key` entries. A
/// describe FAILURE is confirmed against this listing: keys present here read
/// as a present-but-unreadable hard error, keys absent read as `MissingKey`.
#[cfg(unix)]
pub(crate) fn fake_fastly_returning_with_keys(
    stdout_body: &str,
    stderr_body: &str,
    exit_code: i32,
    entry_list_keys: &[&str],
) -> tempfile::TempDir {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempdir().expect("tempdir");
    let script_path = dir.path().join("fastly");
    let stdout_file = dir.path().join("stdout_payload.txt");
    let stderr_file = dir.path().join("stderr_payload.txt");
    let list_file = dir.path().join("list_payload.txt");
    let entry_list_file = dir.path().join("entry_list_payload.txt");
    // Store-list JSON: bare array with one entry matching TEST_CONFIG_ID.
    let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
    let entry_list_json = {
        let items: Vec<String> = entry_list_keys
            .iter()
            .map(|key| format!(r#"{{"item_key":{}}}"#, serde_json::to_string(key).unwrap()))
            .collect();
        format!("[{}]", items.join(","))
    };
    fs::write(&stdout_file, stdout_body).expect("write stdout payload");
    fs::write(&stderr_file, stderr_body).expect("write stderr payload");
    fs::write(&list_file, list_json).expect("write list payload");
    fs::write(&entry_list_file, entry_list_json).expect("write entry list payload");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  cat '{}'\n  exit 0\nfi\ncat '{}'\ncat '{}' >&2\nexit {exit_code}\n",
        list_file.display(),
        entry_list_file.display(),
        stdout_file.display(),
        stderr_file.display(),
    );
    fs::write(&script_path, script).expect("write fastly script");
    let mut perms = fs::metadata(&script_path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("chmod +x");
    dir
}

/// Build a fake `fastly` that logs each argv token (one per line) to
/// `out_path`, handles the list call correctly, and exits 0 for both calls.
#[cfg(unix)]
pub(crate) fn fake_fastly_argv_log(out_path: &Path) -> tempfile::TempDir {
    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempdir().expect("tempdir");
    let script_path = dir.path().join("fastly");
    let list_file = dir.path().join("list_payload.txt");
    let entry_file = dir.path().join("entry_payload.txt");
    let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
    // item_value must be a valid BlobEnvelope JSON so the resolver accepts it.
    let envelope_json = serde_json::to_string(&BlobEnvelope::new(
        json!({"v": "logged"}),
        "2026-06-22T00:00:00Z".into(),
    ))
    .expect("serialize");
    let entry_json = format!(
        r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
        serde_json::to_string(&envelope_json).expect("escape")
    );
    fs::write(&list_file, list_json).expect("write list payload");
    fs::write(&entry_file, &entry_json).expect("write entry payload");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  echo '[]'\n  exit 0\nfi\ncat '{}'\nexit 0\n",
        out_path.display(),
        list_file.display(),
        entry_file.display(),
    );
    fs::write(&script_path, script).expect("write script");
    let mut perms = fs::metadata(&script_path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("chmod +x");
    dir
}

/// Build a valid `BlobEnvelope` JSON string of approximately `target_len` bytes.
pub(crate) fn make_test_envelope(target_len: usize) -> String {
    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::json;
    let pad = "x".repeat(target_len.saturating_add(64));
    let data = json!({ "pad": pad });
    let raw =
        serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
    if raw.len() >= target_len {
        let overhead = raw.len().saturating_sub(pad.len());
        let adjusted = "x".repeat(target_len.saturating_sub(overhead));
        let data2 = json!({ "pad": adjusted });
        serde_json::to_string(&BlobEnvelope::new(data2, "2026-06-22T00:00:00Z".into())).unwrap()
    } else {
        raw
    }
}

/// Build a fake `fastly` script whose describe response depends on
/// the `--key=<k>` argument: `key_responses` maps key names to JSON
/// item-value responses. Falls back to exit 1 "not found" for unknown keys.
#[cfg(unix)]
pub(crate) fn fake_fastly_with_key_dispatch(
    _dir: &Path,
    key_responses: &[(String, String)],
) -> tempfile::TempDir {
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    let fake_dir = tempdir().expect("tempdir");
    let list_file = fake_dir.path().join("list.json");
    let list_json = format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#);
    fs::write(&list_file, list_json).expect("write list");
    // The `config-store-entry list` response: a bare array of the keys present
    // in `key_responses`. Absence confirmation lists the store and checks
    // membership, so a key omitted here reads as CONFIRMED absent. Only
    // `item_key` is needed (the keys-only listing is value-tolerant).
    let entry_list_file = fake_dir.path().join("entry_list.json");
    let entries_json = {
        let items: Vec<String> = key_responses
            .iter()
            .map(|(key, _)| format!(r#"{{"item_key":{}}}"#, serde_json::to_string(key).unwrap()))
            .collect();
        format!("[{}]", items.join(","))
    };
    fs::write(&entry_list_file, entries_json).expect("write entry list");
    // Write each key response to a named file.
    let mut dispatch_lines = String::new();
    for (key, response) in key_responses {
        let resp_file = fake_dir.path().join(format!("resp_{key}.json"));
        fs::write(&resp_file, response).expect("write resp");
        // Use exact-match: iterate argv and compare each token literally
        // so that a root key like "app_config" does NOT match a chunk key
        // like "app_config.__edgezero_chunks.abc.0".
        writeln!(
            dispatch_lines,
            "  for arg in \"$@\"; do if [ \"$arg\" = \"--key={key}\" ]; then cat '{}'; exit 0; fi; done",
            resp_file.display()
        )
        .expect("write to String is infallible");
    }
    // `config-store` (store list) and `config-store-entry list` are served
    // from their files; a `describe` for an unknown key exits 1 "not found",
    // which the caller then CONFIRMS against the entry list.
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"config-store\" ]; then\n  cat '{}'\n  exit 0\nfi\nif [ \"$1\" = \"config-store-entry\" ] && [ \"$2\" = \"list\" ]; then\n  cat '{}'\n  exit 0\nfi\n{dispatch_lines}echo 'Error: item not found' >&2\nexit 1\n",
        list_file.display(),
        entry_list_file.display()
    );
    let script_path = fake_dir.path().join("fastly");
    fs::write(&script_path, &script).expect("write script");
    let mut perms = fs::metadata(&script_path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("chmod");
    fake_dir
}

/// Fake `fastly` for cloud chunk-GC tests. Logs each `config-store-entry` op to
/// `oplog`. `root_describe_seq` gives the successive raw `item_value`s returned
/// when the ROOT key is described. `entry_list` is served for
/// `config-store-entry list`. `fail_delete_key` makes that one delete exit
/// non-zero. `describe_hard_error` makes the FIRST describe of each key fail.
#[cfg(unix)]
pub(crate) fn fake_fastly_gc(
    root_key: &str,
    root_describe_seq: &[String],
    entry_list: &[(String, String, String)],
    fail_delete_key: Option<&str>,
    describe_hard_error: bool,
    oplog: &Path,
) -> tempfile::TempDir {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    // Rendered with handlebars. Triple-stache `{{{ }}}` disables HTML
    // escaping (paths are not markup); the shell's own `${var}` /
    // `$(( ))` use single braces so they are literal text to handlebars.
    const TEMPLATE: &str = r#"#!/bin/sh
if [ "$1" = "config-store" ]; then cat '{{{list}}}'; exit 0; fi
sub="$2"
key=""
for arg in "$@"; do case "$arg" in --key=*) key="${arg#--key=}";; esac; done
if [ "$sub" = "list" ]; then printf 'list\n' >> '{{{oplog}}}'; cat '{{{entries}}}'; exit 0; fi
if [ "$sub" = "update" ]; then cat >/dev/null; printf 'update %s\n' "$key" >> '{{{oplog}}}'; exit 0; fi
if [ "$sub" = "delete" ]; then printf 'delete %s\n' "$key" >> '{{{oplog}}}'; printf 'delete-argv %s\n' "$*" >> '{{{oplog}}}'; if [ "$key" = "{{{fail}}}" ]; then echo 'Error: 404 item not found' >&2; exit 1; fi; exit 0; fi
if [ "$sub" = "describe" ]; then
  printf 'describe %s\n' "$key" >> '{{{oplog}}}'
  cfile='{{{dir}}}/count_'"$key"
  n=0; [ -f "$cfile" ] && n=$(cat "$cfile"); n=$((n+1)); printf '%s' "$n" > "$cfile"
  {{#if hard_error}}if [ "$n" = "1" ]; then echo 'Error: internal server error' >&2; exit 1; fi{{/if}}
  rf='{{{dir}}}/resp_'"$key"'_'"$n"'.json'
  if [ -f "$rf" ]; then cat "$rf"; exit 0; fi
  echo 'Error: item not found' >&2; exit 1
fi
echo 'unexpected' >&2; exit 1
"#;
    let dir = tempdir().expect("tempdir");
    let list_file = dir.path().join("list.json");
    fs::write(
        &list_file,
        format!(r#"[{{"name":"{TEST_CONFIG_ID}","id":"store-abc123"}}]"#),
    )
    .expect("list");
    let entries_file = dir.path().join("entries.json");
    fs::write(&entries_file, entry_list_json(entry_list)).expect("entries");
    for (index, value) in root_describe_seq.iter().enumerate() {
        let wrapped = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(value).expect("escape")
        );
        let nth = index.saturating_add(1);
        fs::write(
            dir.path().join(format!("resp_{root_key}_{nth}.json")),
            wrapped,
        )
        .expect("resp");
    }
    let data = serde_json::json!({
        "list": list_file.display().to_string(),
        "entries": entries_file.display().to_string(),
        "oplog": oplog.display().to_string(),
        "dir": dir.path().display().to_string(),
        "fail": fail_delete_key.unwrap_or(""),
        "hard_error": describe_hard_error,
    });
    let script = handlebars::Handlebars::new()
        .render_template(TEMPLATE, &data)
        .expect("render fake fastly script");
    let script_path = dir.path().join("fastly");
    fs::write(&script_path, script).expect("script");
    let mut perms = fs::metadata(&script_path).expect("meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).expect("chmod");
    dir
}

/// Like `fake_fastly_gc`, but serves a VERBATIM `config-store-entry list`
/// payload so a test can present a shape `entry_list_json` cannot build.
#[cfg(unix)]
pub(crate) fn fake_fastly_gc_raw_list(
    root_key: &str,
    raw_listing: &str,
    oplog: &Path,
) -> tempfile::TempDir {
    use std::fs;
    let dir = fake_fastly_gc(root_key, &[], &[], None, false, oplog);
    fs::write(dir.path().join("entries.json"), raw_listing).expect("raw entries");
    dir
}

/// A `config-store-entry list --json` payload. The item VALUE is a
/// placeholder: reclamation must only ever use keys and timestamps.
#[cfg(unix)]
pub(crate) fn entry_list_json(items: &[(String, String, String)]) -> String {
    let entries: Vec<serde_json::Value> = items
        .iter()
        .map(|(key, created, value)| {
            serde_json::json!({
                "item_key": key,
                "created_at": created,
                "item_value": value,
            })
        })
        .collect();
    serde_json::to_string(&entries).expect("entry list json")
}

/// An RFC-3339 stamp `secs` in the past (the shape Fastly returns).
#[cfg(unix)]
pub(crate) fn stamp_secs_ago(secs: u64) -> String {
    let delta = chrono::Duration::seconds(i64::try_from(secs).unwrap_or(0));
    let now = chrono::Utc::now();
    now.checked_sub_signed(delta)
        .unwrap_or(now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Every chunk of `envelope` as the listing would return it: REAL keys and
/// REAL payload bytes.
#[cfg(unix)]
pub(crate) fn listed_generation(
    root_key: &str,
    envelope: &str,
    secs_ago: u64,
) -> Vec<(String, String, String)> {
    let (chunks, _) = chunked_parts(root_key, envelope);
    let stamp = stamp_secs_ago(secs_ago);
    chunks
        .into_iter()
        .map(|(key, value)| (key, stamp.clone(), value))
        .collect()
}

/// The ROOT entry as the listing would return it: its value is the pointer,
/// which is how `config gc` learns which chunks are live.
#[cfg(unix)]
pub(crate) fn listed_root(
    root_key: &str,
    envelope: &str,
    secs_ago: u64,
) -> (String, String, String) {
    let (_, pointer) = chunked_parts(root_key, envelope);
    (root_key.to_owned(), stamp_secs_ago(secs_ago), pointer)
}

/// A chunked envelope with a distinct payload per tag, padded to `pad`
/// characters so a caller can force a given number of chunks.
#[cfg(unix)]
pub(crate) fn gen_envelope_padded(tag: &str, pad: usize) -> String {
    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::json;
    let data = json!({ tag: "x".repeat(pad) });
    serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
        .expect("envelope")
}

/// A chunked envelope with a distinct payload per tag.
#[cfg(unix)]
pub(crate) fn gen_envelope(tag: &str) -> String {
    use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
    use edgezero_core::blob_envelope::BlobEnvelope;
    use serde_json::json;
    let data = json!({ tag: "x".repeat(FASTLY_CONFIG_ENTRY_LIMIT) });
    serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".to_owned()))
        .expect("envelope")
}

/// Split a chunked envelope into (chunk `(key, value)` pairs, root pointer).
#[cfg(unix)]
pub(crate) fn chunked_parts(root_key: &str, envelope: &str) -> (Vec<(String, String)>, String) {
    let entries = prepare_fastly_config_entries(root_key, envelope).expect("expand");
    let (_, pointer) = entries.last().expect("pointer").clone();
    let chunks = entries[..entries.len().saturating_sub(1)].to_vec();
    (chunks, pointer)
}

/// Just the chunk KEYS of a generation (for delete assertions).
#[cfg(unix)]
pub(crate) fn chunk_keys_of(root_key: &str, envelope: &str) -> Vec<String> {
    let (chunks, _) = chunked_parts(root_key, envelope);
    chunks.into_iter().map(|(key, _)| key).collect()
}

#[cfg(unix)]
pub(crate) fn oplog_has(oplog: &Path, line: &str) -> bool {
    use std::fs;
    fs::read_to_string(oplog)
        .unwrap_or_default()
        .lines()
        .any(|entry| entry == line)
}
