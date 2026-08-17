use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::io::{ErrorKind, Write as _};
use std::process::{ChildStdin, Command, Stdio};

use edgezero_adapter::registry::{ReadConfigEntry, ResolvedStoreId};

use crate::chunked_config::{prepare_fastly_config_entries, resolve_fastly_config_value_typed};

use super::{
    ConfigStoreLookup, FASTLY_INSTALL_HINT, classify_resolved_read, expand_root,
    reject_duplicate_root_keys, reject_generated_key_collisions, reject_reserved_root_keys,
};

/// Cloud-mode `push_config_entries`: resolve the platform config-store
/// id via `fastly config-store list --json`, then shell out per
/// physical entry to `fastly config-store-entry update --upsert --stdin`.
pub(super) fn write_entries(
    store: &ResolvedStoreId,
    entries: &[(String, String)],
    dry_run: bool,
) -> Result<Vec<String>, String> {
    // Resolve the platform config-store id on demand via
    // `fastly config-store list --json` (matched by name =
    // `store.platform`), then `fastly config-store-entry update
    // --store-id=<id> --key=<k> --upsert --stdin` per physical
    // entry. Entries are logical blob-envelope entries from
    // the CLI (one (key, envelope_json) per push); oversized
    // Fastly values are expanded below into chunk entries plus
    // a root pointer by `chunked_config::prepare_fastly_config_entries`.
    let logical = store.logical.as_str();
    let name = store.platform.as_str();
    if entries.is_empty() {
        return Ok(vec![format!(
            "no config entries to push to fastly config-store `{name}` (logical id `{logical}`)"
        )]);
    }
    // Reject reserved keys before any expansion or I/O.
    reject_reserved_root_keys(entries)?;
    reject_duplicate_root_keys(entries)?;
    // Expand each logical root into its physical entries (chunks + pointer, or
    // a single direct entry). Collecting them all first surfaces a
    // pointer-too-large error before touching the remote store. A cloud push
    // does NOT reclaim, so — unlike the local path — it keeps no per-root
    // keep-set / root-value GC bookkeeping.
    let mut physical_entries: Vec<(String, String)> = Vec::new();
    for (key, body) in entries {
        let (expanded, ..) = expand_root(key, body)?;
        physical_entries.extend(expanded);
    }
    if dry_run {
        // Report intent without shelling out. Stays fully offline: no
        // store-id resolution, no remote read (so no GC count).
        let mut out = Vec::with_capacity(entries.len().saturating_mul(2).saturating_add(1));
        out.push(format!(
            "would resolve fastly config-store `{name}` (logical id `{logical}`) via `fastly config-store list --json` and push entries:"
        ));
        for (key, body) in entries {
            let expanded = prepare_fastly_config_entries(key, body)
                .unwrap_or_else(|_| vec![(key.clone(), body.clone())]);
            if expanded.len() == 1 {
                out.push(format!(
                    "  would push `{key}` as direct entry ({}B)",
                    body.len()
                ));
            } else {
                let chunk_count = expanded.len().saturating_sub(1);
                out.push(format!(
                    "  would push `{key}` as chunked ({chunk_count} chunks + 1 pointer, {}B total)",
                    body.len()
                ));
            }
        }
        return Ok(out);
    }
    let resolved_id =
        resolve_remote_config_store_id(name)?.ok_or_else(|| no_matching_store_error(name))?;
    // A cloud push does NOT reclaim orphaned chunks: Fastly's config store is
    // eventually consistent and records no pointer-supersession time, so
    // reclamation is the explicit, operator-invoked `config gc`.
    //
    // Preflight: refuse if a generated chunk key would clobber an existing
    // root-like sibling in the remote store. Uses a completeness-strict key
    // listing (value-tolerant) and describes only the rare colliding keys.
    let remote_keys = list_config_store_keys(&resolved_id)?;
    reject_generated_key_collisions(&physical_entries, &remote_keys, |chunk_key| {
        fetch_remote_config_store_entry(&resolved_id, chunk_key).map(Some)
    })?;
    push_entries_with_committer(&physical_entries, |key, value| {
        create_config_store_entry(&resolved_id, key, value)
    })?;
    Ok(vec![format!(
        "pushed {} physical entries ({} logical) to fastly config-store `{name}` (logical id `{logical}`, id={resolved_id})",
        physical_entries.len(),
        entries.len()
    )])
}

/// Cloud-mode `read_config_entry`: shell out to `fastly
/// config-store-entry describe --store-id=<id> --key=<k> --json`,
/// then resolve chunk pointers via the same store when needed.
pub(super) fn read_entry(store: &ResolvedStoreId, key: &str) -> Result<ReadConfigEntry, String> {
    let name = store.platform.as_str();
    // A TYPED absence: `Ok(None)` (list succeeded, no store matched) is the
    // only path to MissingStore. Any operational failure stays `Err` and fails
    // closed -- an incomplete read must never read as absence and authorise an
    // overwrite of healthy remote state.
    let Some(store_id) = resolve_remote_config_store_id(name)? else {
        return Ok(ReadConfigEntry::MissingStore);
    };
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "describe",
            store_arg.as_str(),
            key_arg.as_str(),
            "--json",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        let stdout = strict_stdout(output.stdout, "config-store-entry describe --json")?;
        // Parse the JSON and extract the `item_value` field.
        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
            format!(
                "failed to parse `fastly config-store-entry describe` JSON (parse error \
                 redacted; response: {})",
                redact_describe_response(&stdout)
            )
        })?;
        let value = parsed
            .get("item_value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry describe` JSON has no string `item_value` field; \
                     fastly CLI may have changed its output schema. (response: {})",
                    redact_describe_response(&stdout)
                )
            })?;
        // Resolve chunk pointers. A chunk describe that fails could not be FULLY
        // read; confirm whether the chunk is genuinely ABSENT against the
        // complete store listing (authoritative), never the describe 404.
        let store_keys: RefCell<Option<Result<HashSet<String>, String>>> = RefCell::new(None);
        let fetch_failed: Cell<bool> = Cell::new(false);
        let resolved = resolve_fastly_config_value_typed(key, value.to_owned(), |chunk_key| {
            match fetch_remote_config_store_entry(&store_id, chunk_key) {
                Ok(found) => Ok(Some(found)),
                Err(_describe_err) => {
                    match confirm_key_absent_cached(&store_keys, &store_id, chunk_key) {
                        Ok(true) => Ok(None), // genuinely gone → repairable Corrupt
                        Ok(false) => {
                            fetch_failed.set(true);
                            Err("a referenced chunk is present in the store but its value \
                                 could not be read (incomplete read)"
                                .to_owned())
                        }
                        Err(list_err) => {
                            fetch_failed.set(true);
                            Err(list_err)
                        }
                    }
                }
            }
        });
        return classify_resolved_read(resolved, value, fetch_failed.get());
    }
    // The describe failed. Absence is CONFIRMED only by a complete listing
    // that omits the key -- never by a describe 404, which a proxy/endpoint or
    // auth failure produces just the same.
    if confirm_entry_absent(&store_id, key)? {
        return Ok(ReadConfigEntry::MissingKey);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` exited \
         with status {} but the key IS present in the store listing (an operational failure, \
         not absence); nothing was changed.\nstderr: {}",
        output.status,
        redact_stderr(&stderr)
    ))
}

/// Fetch a single entry value from a remote Fastly Config Store entry by
/// key, using `fastly config-store-entry describe --store-id=<id> --key=<k>
/// --json`. Used by the chunk-pointer resolver to fan out to chunk entries.
///
/// `Ok(value)` when the entry exists; `Err` on ANY failure, INCLUDING a
/// not-found. Absence is NOT decided here (a describe 404 is not proof) -- the
/// caller confirms it against the complete store listing.
fn fetch_remote_config_store_entry(store_id: &str, key: &str) -> Result<String, String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "describe",
            store_arg.as_str(),
            key_arg.as_str(),
            "--json",
        ])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if output.status.success() {
        let stdout = strict_stdout(output.stdout, "config-store-entry describe --json")?;
        let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
            format!(
                "failed to parse `fastly config-store-entry describe` JSON for key \
                 `{key}` (parse error redacted; response: {})",
                redact_describe_response(&stdout)
            )
        })?;
        let value = parsed
            .get("item_value")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry describe` JSON has no string `item_value` \
                     field for key `{key}`; fastly CLI may have changed its output schema. \
                     (response: {})",
                    redact_describe_response(&stdout)
                )
            })?;
        return Ok(value.to_owned());
    }
    // `Err` on ANY non-success, INCLUDING a not-found. A describe 404 alone is not
    // proof of absence, so the caller CONFIRMS a genuine absence against the
    // complete store listing rather than trusting this stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry describe --store-id={store_id} --key={key} --json` \
         exited with status {}\nstderr: {}",
        output.status,
        redact_stderr(&stderr)
    ))
}

/// The COMPLETE set of item keys in a store, via `config-store-entry list`.
///
/// Absence is CONFIRMED against this, never against a describe 404: the listing
/// is completeness-strict (fails closed on a paginated / non-bare-array view and
/// on a duplicate key), so a key's absence from it is authoritative.
fn list_config_store_keys(store_id: &str) -> Result<HashSet<String>, String> {
    let store_arg = format!("--store-id={store_id}");
    let output = Command::new("fastly")
        .args(["config-store-entry", "list", store_arg.as_str(), "--json"])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`fastly config-store-entry list --store-id={store_id} --json` exited with status {}\nstderr: {}",
            output.status,
            redact_stderr(&stderr)
        ));
    }
    let stdout = strict_stdout(output.stdout, "config-store-entry list --json")?;
    let parsed: serde_json::Value = serde_json::from_str(&stdout).map_err(|_err| {
        format!(
            "failed to parse `fastly config-store-entry list` JSON (parse error redacted; \
             response: {})",
            redact_describe_response(&stdout)
        )
    })?;
    let array = parsed.as_array().ok_or_else(|| {
        format!(
            "refusing to confirm absence: `fastly config-store-entry list --json` did not return a \
             bare array (response: {}). A paginated or partial view could hide a present key and \
             turn it into a false absence that authorises an overwrite.",
            redact_describe_response(&stdout)
        )
    })?;
    let mut keys = HashSet::with_capacity(array.len());
    for (idx, entry) in array.iter().enumerate() {
        let key = entry
            .get("item_key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "`fastly config-store-entry list` entry #{idx} is missing a string `item_key`; \
                     refusing to confirm absence on an unreadable listing"
                )
            })?;
        if key.is_empty() {
            return Err(format!(
                "`fastly config-store-entry list` entry #{idx} has an empty `item_key`; refusing \
                 to confirm absence on an unreadable listing"
            ));
        }
        if !keys.insert(key.to_owned()) {
            return Err(format!(
                "`fastly config-store-entry list` returned duplicate key `{key}`; refusing to \
                 confirm absence on an ambiguous listing"
            ));
        }
    }
    Ok(keys)
}

/// Confirm `key` is ABSENT from the store via a complete listing (authoritative).
/// `Ok(true)` = the listing succeeded and omits the key. `Ok(false)` = the key IS
/// present. `Err` = the listing itself failed. All three fail closed for the
/// caller: only `Ok(true)` is a genuine absence.
fn confirm_entry_absent(store_id: &str, key: &str) -> Result<bool, String> {
    Ok(!list_config_store_keys(store_id)?.contains(key))
}

/// Cached form of [`confirm_entry_absent`] for chunk fetches: lists the store at
/// most ONCE per read (a whole lost generation would otherwise list per chunk).
fn confirm_key_absent_cached(
    cache: &RefCell<Option<Result<HashSet<String>, String>>>,
    store_id: &str,
    key: &str,
) -> Result<bool, String> {
    let mut slot = cache.borrow_mut();
    if slot.is_none() {
        *slot = Some(list_config_store_keys(store_id));
    }
    match slot.as_ref() {
        Some(Ok(keys)) => Ok(!keys.contains(key)),
        Some(Err(err)) => Err(err.clone()),
        // Unreachable: populated just above. Fail closed rather than unwrap.
        None => Err("internal error: store listing cache was not populated".to_owned()),
    }
}

/// Convert `fastly` stdout to a `String`, FAILING CLOSED on invalid UTF-8 rather
/// than substituting U+FFFD. A lossy replacement inside a JSON string could
/// mutate a stored root value or chunk and yield parseable-but-WRONG data on a
/// path that drives an overwrite or a deletion. Diagnostics only ever see
/// redacted output, so stderr stays lossy.
pub(super) fn strict_stdout(stdout: Vec<u8>, command: &str) -> Result<String, String> {
    String::from_utf8(stdout).map_err(|_err| {
        format!(
            "`fastly {command}` returned non-UTF-8 output; refusing to act on it -- a lossy \
             conversion could mutate a stored value. Nothing was changed."
        )
    })
}

// -------------------------------------------------------------------
// `config push` helpers
// -------------------------------------------------------------------

/// Drive a sequential per-entry commit loop and produce the
/// partial-failure diagnostic when the committer fails mid-way.
/// Pure (no I/O) so the diagnostic shape is unit-testable without
/// the fastly CLI on PATH; production calls it with a closure that
/// shells out via `create_config_store_entry`. On success returns
/// the count of committed entries; on failure returns an error
/// string naming committed / failed / not-attempted keys so the
/// operator can resume from a known boundary.
fn push_entries_with_committer<F>(
    entries: &[(String, String)],
    mut committer: F,
) -> Result<usize, String>
where
    F: FnMut(&str, &str) -> Result<(), String>,
{
    let mut pushed: Vec<String> = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if let Err(err) = committer(key, value) {
            let remaining: Vec<&str> = entries
                .iter()
                .skip(pushed.len().saturating_add(1))
                .map(|(remaining_key, _)| remaining_key.as_str())
                .collect();
            return Err(format!(
                "fastly push failed at entry `{key}` while committing {committed} of {total} entries.\n  \
                 The failed entry's outcome is UNKNOWN: Fastly may have committed it before the error \
                 (a timeout can arrive after the write lands), including when it is the root pointer.\n  \
                 Recovery: re-run the SAME `config push`. It is idempotent -- chunk keys are content-addressed \
                 and writes use `--upsert` -- so entries already written are rewritten harmlessly and any \
                 missing ones are filled. Do NOT hand-delete the failed key.\n  \
                 Already written (a retry rewrites them): {pushed:?}\n  \
                 Failed: `{key}` (outcome unknown) -- {err}\n  \
                 Not attempted: {remaining:?}",
                committed = pushed.len(),
                total = entries.len(),
            ));
        }
        pushed.push(key.clone());
    }
    Ok(pushed.len())
}

/// Shell `fastly config-store-entry update --upsert --stdin` with
/// the value piped through stdin instead of `--value=<value>` on
/// argv.
///
/// Two reasons for this exact invocation:
///
/// 1. `--upsert` (vs. the original `create` subcommand): the prior
///    `create` form errored on any key that already existed in the
///    config store, which made `config push` non-repeatable —
///    after the first push, every follow-up push triggered by a
///    config edit would fail at the first unchanged key.
///    `update --upsert` is documented as "insert or update", which
///    matches the convergent semantic the other config-push paths
///    already have (axum overwrites the JSON, cloudflare's
///    `wrangler kv bulk put` overwrites, spin's
///    `cloud key-value set` overwrites).
///
/// 2. `--stdin` (vs. `--value=<value>`): `--value=` exposed every
///    config entry's bytes in `ps`/`/proc/<pid>/cmdline` listings
///    AND was bounded by the host's `ARG_MAX` (4 KiB to 256 KiB
///    depending on platform — easy to trip with a JSON blob).
///    `--stdin` reads the value from stdin instead — keeps value
///    bytes out of argv and lifts the size cap to whatever the OS
///    pipe buffer + the CLI's read accept (megabytes in practice).
fn create_config_store_entry(store_id: &str, key: &str, value: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let mut child = Command::new("fastly")
        .args([
            "config-store-entry",
            "update",
            store_arg.as_str(),
            key_arg.as_str(),
            "--upsert",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    // Take stdin OUT of the child and hand it to a helper that writes the value
    // and drops the handle on return — closing the pipe so the CLI sees EOF.
    // Do NOT early-return on a write error: if the child died before reading
    // (bad args, auth failure), the write fails with BrokenPipe while the USEFUL
    // diagnostic is the child's own stderr. Reap the child FIRST (avoids a
    // zombie), then surface its stderr/status -- folding the pipe error in only
    // as secondary context.
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open stdin pipe to `fastly`".to_owned())?;
    let write_result = write_value_to_fastly_stdin(stdin, value);
    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait on `fastly`: {err}"))?;
    // Redact stderr: a Fastly error can quote the stored config value back, which
    // would put credentials into CI logs.
    let stderr = redact_stderr(&String::from_utf8_lossy(&output.stderr));
    if let Err(err) = write_result {
        return Err(format!(
            "failed to write the value to `fastly` stdin ({err}); `fastly config-store-entry update --store-id={store_id} --key={key} --upsert --stdin` exited with status {}\nstderr: {}",
            output.status, stderr
        ));
    }
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "`fastly config-store-entry update --store-id={store_id} --key={key} --upsert --stdin` exited with status {}\nstderr: {}",
        output.status, stderr
    ))
}

/// Write `value` to the child's stdin, then drop the handle as it falls out of
/// scope on return — closing the pipe so the `fastly` CLI sees EOF. Taking
/// `stdin` by value gives a natural scope-end drop rather than an explicit
/// `drop()`, which also keeps this valid on targets where `ChildStdin` is a
/// non-Drop stub.
fn write_value_to_fastly_stdin(mut stdin: ChildStdin, value: &str) -> Result<(), String> {
    stdin
        .write_all(value.as_bytes())
        .map_err(|err| format!("failed to write value to `fastly` stdin: {err}"))
}

/// Parse `fastly config-store list --json` output and return the
/// platform `id` of the store whose `name` matches `name`. Accepts
/// both a bare array (`[ {"id": "...", "name": "..."}, ... ]`)
/// and an `{"items": [...]}` envelope so this stays compatible
/// across fastly CLI versions.
fn find_config_store_id(stdout: &str, name: &str) -> ConfigStoreLookup {
    let parsed: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(err) => {
            return ConfigStoreLookup::SchemaDrift(format!("stdout did not parse as JSON: {err}"));
        }
    };
    let Some(array) = parsed
        .as_array()
        .or_else(|| parsed.get("items").and_then(serde_json::Value::as_array))
    else {
        return ConfigStoreLookup::SchemaDrift(format!(
            "expected a bare array `[...]` or an `{{\"items\": [...]}}` envelope; got JSON of shape `{}`",
            shape_summary(&parsed)
        ));
    };
    // FAIL CLOSED on any malformed or duplicate row: a `NotFound` here becomes a
    // MissingStore that AUTHORISES an overwrite, so a listing we cannot read
    // exactly must never look like a definite absence. Every row must carry a
    // non-empty string `name` and `id`, and names must be unique.
    let mut seen_names = HashSet::with_capacity(array.len());
    let mut found: Option<String> = None;
    for (idx, entry) in array.iter().enumerate() {
        let name_field = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty());
        let id_field = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty());
        let (Some(entry_name), Some(entry_id)) = (name_field, id_field) else {
            return ConfigStoreLookup::SchemaDrift(format!(
                "store-list entry #{idx} is missing a non-empty string `name` or `id`; refusing to \
                 treat a store as absent on a listing this build cannot read exactly"
            ));
        };
        if !seen_names.insert(entry_name.to_owned()) {
            return ConfigStoreLookup::SchemaDrift(format!(
                "store-list has a duplicate `name` (`{entry_name}`); refusing to resolve a store id \
                 on an ambiguous listing"
            ));
        }
        if entry_name == name {
            found = Some(entry_id.to_owned());
        }
    }
    found.map_or(ConfigStoreLookup::NotFound, ConfigStoreLookup::Found)
}

/// One-line type label for a `serde_json::Value` (for diagnostic
/// error messages — not a canonical JSON-schema description).
fn shape_summary(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Resolve the platform config-store id on demand: shell out to
/// `fastly config-store list --json`, parse the JSON, match by `name`.
///
/// Returns a TYPED absence: `Ok(None)` ONLY when the list call SUCCEEDS and no
/// store matches (a genuine absence). An operational failure (missing binary,
/// spawn/list failure, schema drift) stays `Err` -- callers that read for a diff
/// must not treat an operational failure as "store absent" and overwrite.
pub(super) fn resolve_remote_config_store_id(name: &str) -> Result<Option<String>, String> {
    let output = Command::new("fastly")
        .args(["config-store", "list", "--json"])
        .output()
        .map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                format!("`fastly` not found on PATH; {FASTLY_INSTALL_HINT}")
            } else {
                format!("failed to spawn `fastly`: {err}")
            }
        })?;
    if !output.status.success() {
        return Err(format!(
            "`fastly config-store list --json` exited with status {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = strict_stdout(output.stdout, "config-store list --json")?;
    match find_config_store_id(&stdout, name) {
        ConfigStoreLookup::Found(id) => Ok(Some(id)),
        ConfigStoreLookup::NotFound => Ok(None),
        ConfigStoreLookup::SchemaDrift(detail) => Err(format!(
            "could not parse `fastly config-store list --json` output: {detail}.\n  The fastly CLI may have changed its JSON schema in a recent version. Please file a bug report at https://github.com/stackpop/edgezero/issues with the fastly CLI version (`fastly version`) and the raw stdout. Workaround: pin to a known-compatible fastly CLI version."
        )),
    }
}

/// Message for a genuinely-absent store, for the write/GC callers that treat
/// absence as a hard error (they cannot operate on a store that does not exist).
pub(super) fn no_matching_store_error(name: &str) -> String {
    format!(
        "no fastly config-store matches `{name}` (did you run `edgezero provision --adapter fastly`?)"
    )
}

/// Summarise a `fastly ... describe` response for diagnostics WITHOUT
/// leaking its contents. The response body is the stored config value, so a
/// schema-drift diagnostic must never echo the payload: report only its size and
/// its top-level *shape*, never a value.
pub(super) fn redact_describe_response(stdout: &str) -> String {
    let len = stdout.len();
    serde_json::from_str::<serde_json::Value>(stdout).map_or_else(
        |_err| format!("{len} bytes, not valid JSON"),
        |value| match value {
            serde_json::Value::Object(map) => {
                // Object KEYS are stored/provider-controlled data, so only the
                // COUNT is reported, never the key names.
                format!("{len} bytes, JSON object with {} field(s)", map.len())
            }
            other @ (serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
            | serde_json::Value::Array(_)) => {
                format!("{len} bytes, JSON {}", shape_summary(&other))
            }
        },
    )
}

/// Summarise a failing `fastly` invocation's stderr WITHOUT echoing it. The
/// `describe` and `update --stdin` paths carry the stored config value, so a
/// Fastly error that quotes the payload back would put credentials into CI logs.
pub(super) fn redact_stderr(stderr: &str) -> String {
    let len = stderr.trim().len();
    format!(
        "{len} bytes suppressed (may echo the stored config value); re-run the `fastly` command directly to inspect it"
    )
}

#[cfg(test)]
mod tests {
    use super::super::FastlyCliAdapter;
    #[cfg(unix)]
    use super::super::path_mutation_guard;
    use super::*;
    use crate::chunked_config::CHUNK_KEY_INFIX;
    use crate::cli::test_support::*;
    use edgezero_adapter::registry::{Adapter as _, AdapterPushContext};
    #[cfg(unix)]
    use edgezero_core::test_env::PathPrepend;
    #[cfg(unix)]
    use std::fs;
    use tempfile::tempdir;

    // ---------- push_entries_with_committer ----------

    #[test]
    fn push_entries_with_committer_returns_count_when_all_succeed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let pushed = push_entries_with_committer(&entries, |_, _| Ok(())).expect("all succeed");
        assert_eq!(pushed, 3);
    }

    #[test]
    fn push_entries_with_committer_zero_entries_is_ok() {
        let pushed = push_entries_with_committer(&[], |_, _| Ok(())).expect("empty is fine");
        assert_eq!(pushed, 0);
    }

    #[test]
    fn push_entries_with_committer_failure_surfaces_committed_failed_not_attempted() {
        // Mock committer: succeed for first 2 keys, fail at third.
        let entries = vec![
            ("k1".to_owned(), "v1".to_owned()),
            ("k2".to_owned(), "v2".to_owned()),
            ("k3".to_owned(), "v3".to_owned()),
            ("k4".to_owned(), "v4".to_owned()),
            ("k5".to_owned(), "v5".to_owned()),
        ];
        let mut calls: usize = 0;
        let err = push_entries_with_committer(&entries, |key, _| {
            calls = calls.saturating_add(1);
            if key == "k3" {
                Err("simulated fastly stderr".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("middle failure must error");
        // Committer was invoked for k1, k2, k3 and stopped.
        assert_eq!(calls, 3_usize, "no retries beyond failure point");
        // Error names all three categories.
        assert!(err.contains("k1") && err.contains("k2"), "committed: {err}");
        assert!(
            err.contains("Failed: `k3`"),
            "failed entry named exactly: {err}"
        );
        assert!(
            err.contains("k4") && err.contains("k5"),
            "not-attempted: {err}"
        );
        assert!(err.contains("simulated fastly stderr"), "inner err: {err}");
        // Counts are sane.
        assert!(
            err.contains("committing 2 of 5 entries"),
            "committed/total count: {err}"
        );
        // The failed entry's outcome is UNKNOWN and recovery is a full idempotent
        // re-run, not a hand-resume from a claimed boundary.
        assert!(
            err.contains("UNKNOWN") && err.contains("outcome unknown"),
            "failed outcome must be stated unknown: {err}"
        );
        assert!(
            err.contains("re-run the SAME") && err.contains("idempotent"),
            "recovery must be a full idempotent re-run: {err}"
        );
        assert!(
            !err.contains("safe to skip on retry"),
            "must not claim committed entries can be skipped from a known boundary: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_first_entry_failure_reports_zero_committed() {
        let entries = vec![
            ("only".to_owned(), "val".to_owned()),
            ("never".to_owned(), "tried".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |_, _| Err("nope".to_owned()))
            .expect_err("first-entry failure");
        assert!(err.contains("committing 0 of 2"), "zero committed: {err}");
        assert!(
            err.contains("Failed: `only`"),
            "first-entry failure named: {err}"
        );
        assert!(
            err.contains("never"),
            "second entry as not-attempted: {err}"
        );
    }

    #[test]
    fn push_entries_with_committer_last_entry_failure_reports_n_minus_one_committed() {
        let entries = vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "2".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ];
        let err = push_entries_with_committer(&entries, |key, _| {
            if key == "c" {
                Err("late failure".to_owned())
            } else {
                Ok(())
            }
        })
        .expect_err("last-entry failure");
        assert!(err.contains("committing 2 of 3"), "n-1 committed: {err}");
        assert!(
            err.contains("Not attempted: []"),
            "zero not-attempted when the last entry fails: {err}"
        );
    }

    // ---------- find_config_store_id ----------

    #[test]
    fn find_config_store_id_matches_bare_array_by_name() {
        let stdout = format!(
            r#"[
                {{"id": "abc123", "name": "{TEST_CONFIG_ID}"}},
                {{"id": "def456", "name": "other_store"}}
            ]"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "abc123"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_tolerates_items_envelope() {
        let stdout = format!(
            r#"{{"items": [
                {{"id": "xyz789", "name": "{TEST_CONFIG_ID}"}}
            ]}}"#
        );
        match find_config_store_id(&stdout, TEST_CONFIG_ID) {
            ConfigStoreLookup::Found(id) => assert_eq!(id, "xyz789"),
            ConfigStoreLookup::NotFound => panic!("expected Found, got NotFound"),
            ConfigStoreLookup::SchemaDrift(detail) => {
                panic!("expected Found, got SchemaDrift({detail})")
            }
        }
    }

    #[test]
    fn find_config_store_id_distinguishes_not_found_from_match_failure() {
        // JSON parses cleanly, entries are well-formed
        // (`name` + `id` strings present), but no entry matches
        // → NotFound. Operator likely needs to run `provision`.
        let stdout = r#"[{"id": "abc", "name": "other"}]"#;
        assert!(matches!(
            find_config_store_id(stdout, "missing"),
            ConfigStoreLookup::NotFound
        ));
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_on_malformed_json() {
        // Unparseable bytes are NOT a "store not found" — they're
        // a "fastly CLI output format changed" signal. Operator
        // needs different recovery (file a bug, pin CLI version)
        // than for the "store doesn't exist yet" case.
        let drift = find_config_store_id("not json", "anything");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "non-JSON stdout must be schema drift, got {drift:?}"
        );
        let empty = find_config_store_id("", "anything");
        assert!(
            matches!(empty, ConfigStoreLookup::SchemaDrift(_)),
            "empty stdout must be schema drift, got {empty:?}"
        );
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_shape_unexpected() {
        // JSON parses but the top-level is neither a bare array
        // nor an `{items: [...]}` envelope.
        let stdout = r#"{"namespace": "fastly", "list": []}"#;
        match find_config_store_id(stdout, "any") {
            ConfigStoreLookup::SchemaDrift(detail) => {
                assert!(
                    detail.contains("bare array") || detail.contains("items"),
                    "schema-drift detail names the expected shapes: {detail}"
                );
            }
            ConfigStoreLookup::Found(id) => panic!("expected SchemaDrift, got Found({id})"),
            ConfigStoreLookup::NotFound => panic!("expected SchemaDrift, got NotFound"),
        }
    }

    #[test]
    fn find_config_store_id_flags_schema_drift_when_entries_lack_name_id() {
        // Array of objects but none have BOTH string `name` and
        // string `id` fields — suggests schema rename (e.g.
        // fastly renamed `name` → `title`).
        let stdout = format!(r#"[{{"title": "{TEST_CONFIG_ID}", "uid": "abc"}}]"#);
        let drift = find_config_store_id(&stdout, TEST_CONFIG_ID);
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "entries lacking name/id must be schema drift, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_fails_closed_on_a_malformed_row() {
        // A row that lacks a non-empty `name`/`id` could BE the requested store
        // (its unreadable name might have matched). Treating the listing as a
        // definite NotFound would authorise an overwrite of a store that exists,
        // so a malformed row must be SchemaDrift (a hard error), not NotFound --
        // even when another row is well-formed.
        let stdout = format!(
            r#"[{{"name": "", "id": "abc"}}, {{"name": "{TEST_CONFIG_ID}", "id": "store-1"}}]"#
        );
        let drift = find_config_store_id(&stdout, "some-other-store");
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "an empty-name row must fail closed, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_rejects_duplicate_names() {
        // A duplicate name means we are not reading one consistent view of the
        // store, so resolving an id off it is ambiguous -> fail closed.
        let stdout = format!(
            r#"[{{"name": "{TEST_CONFIG_ID}", "id": "a"}}, {{"name": "{TEST_CONFIG_ID}", "id": "b"}}]"#
        );
        let drift = find_config_store_id(&stdout, TEST_CONFIG_ID);
        assert!(
            matches!(drift, ConfigStoreLookup::SchemaDrift(_)),
            "a duplicate name must fail closed, got {drift:?}"
        );
    }

    #[test]
    fn find_config_store_id_returns_not_found_for_empty_array() {
        // Empty array IS a valid "store doesn't exist yet" signal,
        // not schema drift — fastly CLI legitimately returns `[]`
        // when no config-stores exist.
        let drift = find_config_store_id("[]", "any");
        assert!(
            matches!(drift, ConfigStoreLookup::NotFound),
            "empty array must be NotFound, got {drift:?}"
        );
    }

    // ---------- push_config_entries (dry-run + error paths) ----------

    #[test]
    fn push_dry_run_does_not_invoke_fastly() {
        let dir = tempdir().expect("tempdir");
        let entries = vec![
            ("greeting".to_owned(), "hello".to_owned()),
            ("feature.new_checkout".to_owned(), "false".to_owned()),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true,
            )
            .expect("dry-run succeeds");
        // First line names the resolve+publish flow; then one preview line per
        // key. A push no longer reclaims anything (see `config gc`), so there is
        // no GC-intent line.
        assert_eq!(out.len(), 1 + entries.len(), "header + per-entry preview");
        assert!(
            out[0].contains("would resolve fastly config-store `app_config`")
                && out[0].contains("push entries"),
            "dry-run header describes the would-be flow: {out:?}"
        );
        assert!(
            out.iter().any(|line| line.contains("`greeting`")),
            "dry-run lists `greeting`: {out:?}"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("`feature.new_checkout`")),
            "dry-run lists `feature.new_checkout`: {out:?}"
        );
    }

    #[test]
    fn push_with_no_entries_reports_no_op_without_invoking_fastly() {
        let dir = tempdir().expect("tempdir");
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[],
                &AdapterPushContext::new(),
                false,
            )
            .expect("zero-entry push is fine");
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("no config entries"),
            "status line names the no-op: {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_present_on_success() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Fake fastly: list succeeds with app_config → store-abc123;
        // describe returns valid JSON with item_value that is a BlobEnvelope.
        let envelope = serde_json::to_string(&BlobEnvelope::new(
            json!({"hello": "fastly"}),
            "2026-06-22T00:00:00Z".into(),
        ))
        .expect("serialize");
        let entry_json = format!(
            r#"{{"item_value":{},"store_id":"store-abc123"}}"#,
            serde_json::to_string(&envelope).expect("escape")
        );
        let fake = fake_fastly_returning(&entry_json, "", 0);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("fake fastly exit-0 must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, envelope);
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_key_when_confirmed_absent() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // describe exits non-zero, and the complete store listing (empty here)
        // CONFIRMS the key is absent → MissingKey (not decided by the 404 alone).
        let fake = fake_fastly_returning("", "Error: item not found", 1);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("not-found maps to MissingKey (not Err)");
        assert!(
            matches!(result, ReadConfigEntry::MissingKey),
            "not-found stderr => MissingKey"
        );
    }

    /// The Fastly impl distinguishes store-not-found from key-not-found via
    /// `resolve_remote_config_store_id`: when the list call exits non-zero and
    /// the error string contains "not found", `read_config_entry` returns
    /// `MissingStore` without ever calling the describe subcommand.
    #[cfg(unix)]
    #[test]
    fn read_remote_fails_closed_when_the_list_call_itself_errors() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The list call EXITS NON-ZERO with "not found"-shaped stderr. That is an
        // OPERATIONAL failure (auth/network/server), not proof the store is absent
        // -- the absence signal is a SUCCESSFUL list that omits the store. So this
        // must fail closed (a hard error the operator retries), NEVER MissingStore:
        // reading it as absence could authorise an overwrite of a store we never
        // actually queried.
        let fake_dir = tempdir().expect("tempdir");
        let stderr_file = fake_dir.path().join("stderr_payload.txt");
        fs::write(&stderr_file, "Error: config store not found for service").expect("write stderr");
        let script_path = fake_dir.path().join("fastly");
        let script = format!(
            "#!/bin/sh\ncat '{stderr}' >&2\nexit 1\n",
            stderr = stderr_file.display(),
        );
        fs::write(&script_path, script).expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        let _path = PathPrepend::new(fake_dir.path());
        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "greeting",
            &AdapterPushContext::new(),
        );
        assert!(
            result.is_err(),
            "a failed list call must fail closed, not read as MissingStore"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_remote_returns_missing_store_when_the_store_is_genuinely_absent() {
        use std::os::unix::fs::PermissionsExt as _;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The list call SUCCEEDS and returns a valid, empty store array. The store
        // is genuinely absent -> `no fastly config-store matches` -> MissingStore.
        let fake_dir = tempdir().expect("tempdir");
        let script_path = fake_dir.path().join("fastly");
        fs::write(&script_path, "#!/bin/sh\necho '[]'\nexit 0\n").expect("write script");
        let mut perms = fs::metadata(&script_path).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).expect("chmod +x");
        let _path = PathPrepend::new(fake_dir.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("a successful list that omits the store maps to MissingStore");
        assert!(
            matches!(result, ReadConfigEntry::MissingStore),
            "store absent from a successful list => MissingStore"
        );
    }

    /// Verify that `read_config_entry` invokes
    /// `fastly config-store-entry describe --store-id=<id> --key=<key> --json`
    /// (after the resolve step that calls `fastly config-store list --json`).
    #[cfg(unix)]
    #[test]
    fn read_remote_invokes_correct_argv() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());
        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "greeting",
                &AdapterPushContext::new(),
            )
            .expect("argv-log fake must succeed");
        assert!(
            matches!(result, ReadConfigEntry::Present(_)),
            "expected Present from argv-log fake"
        );
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // The describe call must include these args (resolve call args
        // are also captured but we only assert the describe shape here).
        assert!(
            captured.contains("config-store-entry"),
            "must invoke config-store-entry; got:\n{captured}"
        );
        assert!(
            captured.contains("describe"),
            "must pass describe subcommand; got:\n{captured}"
        );
        assert!(
            captured.contains("--store-id=store-abc123"),
            "must pass resolved store id; got:\n{captured}"
        );
        assert!(
            captured.contains("--key=greeting"),
            "must pass --key=<key>; got:\n{captured}"
        );
        assert!(
            captured.contains("--json"),
            "must pass --json flag; got:\n{captured}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_rejects_reserved_key() {
        let dir = tempdir().expect("tempdir");
        let bad_key = format!("app_config{CHUNK_KEY_INFIX}deadbeef.0");
        let err = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &[(bad_key.clone(), "{}".to_owned())],
                &AdapterPushContext::new(),
                false,
            )
            .expect_err("reserved key must be rejected");
        assert!(err.contains(&bad_key), "names the key: {err}");
    }

    /// Schema drift must never echo the config payload — including OBJECT KEYS,
    /// which are provider/stored data. App config can hold credentials; CLI
    /// status lines are logged verbatim and CI logs are retained/shared. Only a
    /// size + field COUNT may be reported.
    #[cfg(unix)]
    #[test]
    fn read_config_entry_schema_drift_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_abc123";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // The sentinel is an OBJECT KEY (not a value): the earlier redactor joined
        // keys into the diagnostic, so this is what pins the key-disclosure fix.
        let drift = format!(r#"{{"{SENTINEL}":"x"}}"#);
        let fake = fake_fastly_returning(&drift, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("schema drift must error")
        };
        assert!(
            !err.contains(SENTINEL),
            "error must not leak an object KEY from the config payload: {err}"
        );
        assert!(
            err.contains("bytes") && err.contains("field(s)"),
            "error should carry a redacted size + field COUNT: {err}"
        );
    }

    /// The FAILURE branch leaks too: a Fastly error that quotes the stored
    /// value back in stderr must not reach the user-facing error.
    #[cfg(unix)]
    #[test]
    fn read_config_entry_stderr_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_stderr1";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // A hard failure that echoes the value. The key IS present in the store
        // listing, so absence confirmation fails and the read surfaces the
        // (redacted) describe stderr on the hard-error path.
        let stderr = format!("Error: internal failure processing value {SENTINEL}");
        let fake = fake_fastly_returning_with_keys("", &stderr, 1, &["cfg"]);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        let Err(err) = result else {
            panic!("hard stderr failure must error")
        };
        assert!(
            !err.contains(SENTINEL),
            "stderr must be redacted, not echoed: {err}"
        );
        assert!(
            err.contains("suppressed"),
            "error should say the stderr was suppressed: {err}"
        );
    }

    /// The WRITE path leaks too: a failing `config-store-entry update --upsert`
    /// whose stderr quotes the value being written must be redacted.
    #[cfg(unix)]
    #[test]
    fn upsert_stderr_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_upsert1";
        let _lock = path_mutation_guard().lock().expect("guard");
        // A fake `fastly` that fails every call, echoing the value in stderr.
        let stderr = format!("Error: rejected value {SENTINEL}");
        let fake = fake_fastly_returning("", &stderr, 1);
        let _path = PathPrepend::new(fake.path());

        let err = create_config_store_entry("store-abc", "cfg", SENTINEL)
            .expect_err("a failing upsert must error");
        assert!(
            !err.contains(SENTINEL),
            "upsert stderr must be redacted, not echoed: {err}"
        );
        assert!(
            err.contains("suppressed"),
            "error should say the stderr was suppressed: {err}"
        );
    }

    /// `config gc` reads `item_value` for every entry (to classify roots). A
    /// malformed listing whose values carry secrets must fail closed WITHOUT
    /// echoing any value. (Replaces the old push prior-read redaction tests,
    /// which are now vacuous: a cloud push performs no pre-commit read.)
    #[cfg(unix)]
    #[test]
    fn gc_list_failure_does_not_leak_payload() {
        const SENTINEL: &str = "SUPER_SECRET_TOKEN_gc_list";
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let good = entry_list_json(&listing);
        // A valid entry whose VALUE contains the sentinel, plus a malformed
        // sibling (no created_at) to trip the fail-closed path.
        let mut array: serde_json::Value = serde_json::from_str(&good).unwrap();
        let arr = array.as_array_mut().unwrap();
        arr.push(serde_json::json!({
            "item_key": "some.__edgezero_chunks.deadbeef.0",
            "item_value": SENTINEL,
        }));
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            None,
            false,
            &dir.path().join("ops.log"),
        );
        fs::write(
            fake.path().join("entries.json"),
            serde_json::to_string(&array).unwrap(),
        )
        .expect("overwrite entries");
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            !err.contains(SENTINEL),
            "the fail-closed error must not echo a stored value: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_direct_entry_at_exactly_8000_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        assert_eq!(envelope.len(), FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        // One physical entry written (direct).
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "must write root key directly: {captured}"
        );
        assert!(
            out[0].contains("1 physical entries (1 logical)"),
            "summary reports 1 physical entry: {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_writes_chunks_and_root_pointer_for_8001_chars() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let argv_log = dir.path().join("argv.txt");
        let fake = fake_fastly_argv_log(&argv_log);
        let _path = PathPrepend::new(fake.path());

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        assert!(envelope.len() > FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = vec![(TEST_CONFIG_ID.to_owned(), envelope)];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                false,
            )
            .expect("push must succeed");
        let captured = fs::read_to_string(&argv_log).expect("argv log");
        // At least one chunk key must appear before the root key.
        assert!(
            captured.contains(".__edgezero_chunks."),
            "chunk keys must be written: {captured}"
        );
        // Root pointer must also be written.
        assert!(
            captured.contains(&format!("--key={TEST_CONFIG_ID}")),
            "root pointer must be written: {captured}"
        );
        // Root key must be LAST in the log (chunk lines come before it).
        let root_pos = captured.rfind(&format!("--key={TEST_CONFIG_ID}")).unwrap();
        let chunk_pos = captured.find(".__edgezero_chunks.").unwrap();
        assert!(
            chunk_pos < root_pos,
            "chunk writes must precede root pointer write: chunk_pos={chunk_pos} root_pos={root_pos}"
        );
        assert!(out[0].contains("logical"), "summary line present: {out:?}");
    }

    #[cfg(unix)]
    #[test]
    fn push_config_entries_dry_run_reports_direct_vs_chunked() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let dir = tempdir().expect("tempdir");

        let direct_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT);
        let chunked_envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));

        let entries = vec![
            ("cfg_direct".to_owned(), direct_envelope),
            ("cfg_chunked".to_owned(), chunked_envelope),
        ];
        let out = FastlyCliAdapter
            .push_config_entries(
                dir.path(),
                None,
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                &entries,
                &AdapterPushContext::new(),
                true, // dry_run
            )
            .expect("dry-run must not error");

        // No shellout happens; output must describe intent.
        let combined = out.join("\n");
        assert!(
            combined.contains("would push `cfg_direct` as direct entry"),
            "must report direct: {combined}"
        );
        assert!(
            combined.contains("would push `cfg_chunked` as chunked"),
            "must report chunked: {combined}"
        );
    }

    // ---------- chunked read integration tests ----------

    #[cfg(unix)]
    #[test]
    fn read_config_entry_resolves_direct_value_unchanged() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = BlobEnvelope::new(json!({"hello": "world"}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&json_str).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                "cfg",
                &AdapterPushContext::new(),
            )
            .expect("read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(value, json_str, "direct envelope passes through unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reconstructs_chunked_envelope() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Build a key→response map for every physical entry.
        let mut key_responses: Vec<(String, String)> = Vec::new();
        for (pk, pv) in &physical {
            let resp = format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap());
            key_responses.push((pk.clone(), resp));
        }
        // The root key should return the pointer.
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        key_responses.push((TEST_CONFIG_ID.to_owned(), ptr_resp));

        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter
            .read_config_entry(
                dir.path(),
                Some("fastly.toml"),
                None,
                &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
                TEST_CONFIG_ID,
                &AdapterPushContext::new(),
            )
            .expect("chunked read must succeed");
        let ReadConfigEntry::Present(value) = result else {
            panic!("expected Present");
        };
        assert_eq!(
            value, envelope,
            "reconstructed envelope must equal original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reports_corrupt_on_a_confirmed_absent_chunk() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        // Only provide the root pointer; omit chunk responses so the chunk fetch
        // gets a CLEAN not-found (`Error: item not found`, no operational marker).
        let ptr_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(pointer_json).unwrap()
        );
        let key_responses = vec![(TEST_CONFIG_ID.to_owned(), ptr_resp)];
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        // The chunk describe fails, and the complete store listing (which holds
        // only the root pointer) CONFIRMS the chunk is absent. The blob spec makes
        // persistent chunk loss REPAIRABLE by re-pushing, so the read reports
        // `Corrupt` (a push overwrites to repair), NOT a hard error -- otherwise
        // `config push` could never fix it. Absence is confirmed by the listing,
        // never by the describe 404 alone, so a proxy/auth failure (where the
        // listing also fails, or shows the chunk present) stays a hard error.
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a confirmed-absent chunk must be repairable Corrupt, not a hard error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reports_corrupt_on_chunk_hash_mismatch() {
        use crate::chunked_config::FASTLY_CONFIG_ENTRY_LIMIT;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");

        let envelope = make_test_envelope(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let physical = prepare_fastly_config_entries(TEST_CONFIG_ID, &envelope).unwrap();
        let (_, pointer_json) = physical.last().unwrap();
        let mut key_responses: Vec<(String, String)> = Vec::new();
        // Corrupt first chunk's content.
        let (first_chunk_key, first_chunk_val) = &physical[0];
        let corrupted: String = first_chunk_val.chars().map(|_| 'Z').collect();
        let corrupt_resp = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(&corrupted).unwrap()
        );
        key_responses.push((first_chunk_key.clone(), corrupt_resp));
        // Remaining chunks as normal.
        for (pk, pv) in physical
            .iter()
            .take(physical.len().saturating_sub(1))
            .skip(1)
        {
            key_responses.push((
                pk.clone(),
                format!(r#"{{"item_value":{}}}"#, serde_json::to_string(pv).unwrap()),
            ));
        }
        key_responses.push((
            TEST_CONFIG_ID.to_owned(),
            format!(
                r#"{{"item_value":{}}}"#,
                serde_json::to_string(pointer_json).unwrap()
            ),
        ));
        let fake = fake_fastly_with_key_dispatch(dir.path(), &key_responses);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            TEST_CONFIG_ID,
            &AdapterPushContext::new(),
        );
        // A chunk-hash mismatch at an EXISTING entry is corrupt stored state the
        // push repairs by overwriting, so the CLI read reports `Corrupt`. (The
        // RUNTIME path keeps a hash mismatch as Internal — see config_store.rs.)
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a chunk-hash mismatch at an existing entry must be Corrupt (repairable), not an error"
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_config_entry_reports_corrupt_for_malformed_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        // Root value ANNOUNCES our chunk-pointer kind but is malformed. The
        // `describe` SUCCEEDS (the entry exists), so a resolve failure is CORRUPT
        // stored state, not an IO error: the read must report `Corrupt` so a push
        // can overwrite it (in-band repair), NOT hard-error and block recovery.
        let bad_json = r#"{"edgezero_kind":"fastly_config_chunks","some_field":"x"}"#;
        let item_json = format!(
            r#"{{"item_value":{}}}"#,
            serde_json::to_string(bad_json).unwrap()
        );
        let fake = fake_fastly_returning(&item_json, "", 0);
        let _path = PathPrepend::new(fake.path());

        let result = FastlyCliAdapter.read_config_entry(
            dir.path(),
            Some("fastly.toml"),
            None,
            &ResolvedStoreId::from_logical(TEST_CONFIG_ID),
            "cfg",
            &AdapterPushContext::new(),
        );
        assert!(
            matches!(result, Ok(ReadConfigEntry::Corrupt(_))),
            "a malformed pointer at an EXISTING entry must be Corrupt (repairable), not an error"
        );
    }
}
