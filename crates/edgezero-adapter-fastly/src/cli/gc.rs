//! `config gc` reclamation core for the Fastly adapter.
//!
//! Operator-invoked garbage collection of orphaned chunk entries in a Fastly
//! config store. Deliberately separate from `config push`: on an
//! eventually-consistent store a chunk may only be deleted once the pointer that
//! referenced it has stopped being served everywhere, and Fastly records no
//! pointer-supersession time — so the operator supplies `--older-than` as the
//! safety assertion the platform cannot make.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::ErrorKind;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chunked_config::{
    CHUNK_KEY_INFIX, GcPointer, GcRootValue, chunk_key_generation, chunk_key_index, chunk_lengths,
    gc_classify_root, gc_verify_generation, prepare_fastly_config_entries, sha256_hex,
    value_announces_our_kind, value_is_future_format, value_is_inert_foreign,
    verify_writer_split_layout,
};

use super::FASTLY_INSTALL_HINT;
use super::push_cloud::{
    no_matching_store_error, redact_describe_response, redact_stderr,
    resolve_remote_config_store_id, strict_stdout,
};

/// The reclamation plan for `config gc`: the orphan chunk entries to delete
/// (with their ages) plus the counts for the summary line. Produced by
/// `plan_gc_reclamation` (which owns every safety guard); consumed by
/// `gc_fastly_config_store` (which reports and deletes).
struct GcPlan {
    /// Whole generations to reclaim, each a list of `(key, age_secs)`. Grouped,
    /// not flat: a generation is provable only as a UNIT (see
    /// `prove_generation`), so deleting part of one destroys the very evidence
    /// that licenses deleting the rest.
    doomed: Vec<Vec<(String, u64)>>,
    /// The root keys retained as live/protected — the config entries GC will NOT
    /// delete, sorted. Surfaced so a run shows what it is KEEPING, not only what
    /// it would delete, making the sweep reviewable.
    kept_roots: Vec<String>,
    live_count: usize,
    retained_recent: usize,
    roots: usize,
    /// Chunk-shaped entries we could NOT prove our writer produced, so left
    /// untouched. Surfaced so an operator can see we declined to judge them.
    unprovable: usize,
    /// Non-fatal problems to print — see `GcClassification::warnings`.
    warnings: Vec<String>,
}

/// What one pass of `config gc`'s delete loop actually did.
struct GcDeleteOutcome {
    /// Entries whose delete returned success.
    deleted: usize,
    /// Keys whose delete returned non-zero.
    failed: Vec<String>,
    /// Survivors of a generation in which an earlier sibling's delete had
    /// ALREADY succeeded before a later one failed. These are definitely an
    /// incomplete generation now, so they can never be proved (or reclaimed)
    /// again -- manual removal only.
    stranded: Vec<String>,
    /// Members of a generation whose ONLY failure was on a delete with no
    /// confirmed prior sibling success. A failed remote delete has UNKNOWN
    /// outcome (Fastly may have committed it before returning an error), so we
    /// cannot say whether the generation is still whole. A re-run reclaims it if
    /// it is, or reports it as an unprovable fragment if it is not.
    uncertain: Vec<String>,
}

/// The result of classifying a store's entries for reclamation.
struct GcClassification {
    /// Chunk keys a live root pointer references, each verified against its
    /// content-address. Never deletable.
    live: HashSet<String>,
    /// Keys whose OWN value is a runtime-readable root — a valid direct envelope
    /// or a pointer — regardless of what their key looks like. Never deletable.
    protected: HashSet<String>,
    /// Count of entries classified as roots, for the summary line.
    roots: usize,
    /// Non-fatal problems the operator should see — currently roots that are
    /// not runtime-readable and so can never be reclaimed automatically.
    warnings: Vec<String>,
}

/// One `config-store-entry list` item.
///
/// `item_value` IS captured — `config gc` must parse root pointers to learn
/// which chunks are live, and one listing avoids a `describe` per root. It is
/// the config payload: it may be read in memory but must NEVER be logged or
/// surfaced (see `redact_describe_response` / `redact_stderr`).
struct ConfigStoreItem {
    created_at: String,
    item_key: String,
    item_value: String,
}

/// Unix epoch seconds. Push-time only (the `cli` feature is native).
fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Run `fastly config-store-entry list --store-id=<id> --json` and return each
/// item's `item_key`, `item_value`, and `created_at`.
///
/// The item VALUE is KEPT (not discarded): `config gc` classifies each root by
/// its value (`gc_classify_root`) and reconstructs live generations from the
/// chunk values, so all three fields are required. The value is used internally
/// only and is NEVER echoed into a diagnostic — parse failures redact it via
/// `redact_describe_response`.
fn list_config_store_entries(store_id: &str) -> Result<Vec<ConfigStoreItem>, String> {
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
    // A BARE ARRAY ONLY. The installed Fastly CLI returns the complete store as
    // a top-level array with no cursor/paging flags. Any other shape (e.g. an
    // `{"items":[...], ...}` envelope) may carry pagination metadata we do not
    // follow -- and a page that omitted a ROOT while listing its chunks would
    // make live chunks look orphaned. The completeness guard cannot see a root
    // that isn't there, so we refuse rather than reclaim from a partial view.
    let array = parsed.as_array().ok_or_else(|| {
        format!(
            "refusing to reclaim: `fastly config-store-entry list --json` did not return a bare \
             array (response: {}). This build only supports an unpaginated listing; a partial view \
             could hide a root and orphan its live chunks. Nothing was deleted.",
            redact_describe_response(&stdout)
        )
    })?;
    // FAIL CLOSED on any malformed entry. A missing/non-string field on a
    // reclamation input must NEVER be silently skipped or defaulted to empty:
    // skipping a root hides the chunks it references (they'd look orphaned and
    // get deleted while live), and an empty `item_value` makes a real root
    // parse as "references nothing" — same catastrophe. If we can't read the
    // listing exactly, we delete nothing.
    let mut items = Vec::with_capacity(array.len());
    for (idx, entry) in array.iter().enumerate() {
        // Name the offending KEY, not just the index: `item_key` is readable even
        // when another field is empty, so the operator can see WHICH entry to fix.
        let key_hint = entry
            .get("item_key")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .map_or_else(|| format!("#{idx}"), |key| format!("`{key}`"));
        let field = |name: &str| -> Result<String, String> {
            let raw = entry
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    format!(
                        "`fastly config-store-entry list` entry {key_hint} is missing a string \
                         `{name}` field; refusing to reclaim (nothing deleted)"
                    )
                })?;
            // An EMPTY field is as dangerous as a missing one: an empty root value
            // would classify as "references nothing" and orphan its live chunks.
            if raw.is_empty() {
                return Err(format!(
                    "`fastly config-store-entry list` entry {key_hint} has an empty `{name}` field; \
                     refusing to reclaim (nothing deleted). If this is a legitimate empty-valued \
                     entry, remove it or give it a value before running `config gc`."
                ));
            }
            Ok(raw.to_owned())
        };
        items.push(ConfigStoreItem {
            created_at: field("created_at")?,
            item_key: field("item_key")?,
            item_value: field("item_value")?,
        });
    }

    // DUPLICATE KEYS => fail closed. A key must appear once; a store cannot
    // really hold two entries under one key, so duplicate rows mean we are not
    // reading the store we think we are (a merged/paginated view, or a CLI
    // change). Left alone, the last row silently wins for BOTH the live-set
    // lookup and `created_at`, so conflicting rows could age a recent key into
    // eligibility and schedule the same key for two deletes.
    let mut seen: HashSet<&str> = HashSet::with_capacity(items.len());
    if let Some(duplicate) = items
        .iter()
        .find(|item| !seen.insert(item.item_key.as_str()))
    {
        return Err(format!(
            "refusing to reclaim: `fastly config-store-entry list` returned key `{}` more than \
             once. A key is unique in a config store, so this listing does not describe one \
             consistent view of it (nothing was deleted).",
            duplicate.item_key
        ));
    }

    Ok(items)
}

/// RFC 3339 (`2026-07-13T03:27:42Z`) -> unix seconds, rounded UP on any fraction.
///
/// `timestamp()` FLOORS the sub-second part, and the current time the age gate
/// compares against is also floored. A creation floored DOWN makes a key look
/// OLDER: a true age of 59.002s (created `...:42.998Z`) would compute as 60s and
/// pass a 60s `--older-than` almost a full second early. Rounding creation UP
/// keeps the computed age conservative -- a key never ages into deletion early.
fn parse_rfc3339_secs(raw: &str) -> Option<u64> {
    let stamp = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let secs = stamp.timestamp();
    let rounded_up = if stamp.timestamp_subsec_nanos() > 0 {
        secs.checked_add(1)?
    } else {
        secs
    };
    u64::try_from(rounded_up).ok()
}

/// Report what a sweep is KEEPING, not only what it would delete, so the run is
/// reviewable: each RETAINED root by key, plus the referenced-chunk total those
/// roots hold (already summarised). A root listed here is never a delete
/// candidate.
fn append_kept_roots_report(out: &mut Vec<String>, kept_roots: &[String], live_count: usize) {
    if kept_roots.is_empty() {
        out.push("keeping 0 retained root(s)".to_owned());
        return;
    }
    out.push(format!(
        "keeping {} retained root(s) ({live_count} referenced chunk(s) held by them):",
        kept_roots.len()
    ));
    for key in kept_roots {
        out.push(format!("  keeping `{key}`"));
    }
}

/// `config gc` for Fastly: delete chunk entries that no LIVE root pointer
/// references and that are older than the operator's `older_than_secs`.
///
/// Why this is a separate, operator-invoked command rather than part of `config
/// push`: see `Adapter::gc_config_entries`. The operator's `--older-than` is the
/// safety assertion the platform cannot make. A dry-run prints exactly which
/// keys would go, with ages, so the assertion is reviewable.
///
/// Fails CLOSED: if the listing is unreadable, or a root's value cannot be
/// classified, nothing is deleted.
pub(super) fn gc_fastly_config_store(
    store_name: &str,
    older_than_secs: u64,
    dry_run: bool,
) -> Result<Vec<String>, String> {
    // THE destructive boundary enforces its own precondition. The CLI rejects a
    // zero window too, but `gc_config_entries` is a public trait method any
    // caller can reach directly -- a safety rule that lives only in the CLI is
    // not a safety rule. A zero window asserts nothing: it makes every orphan
    // eligible, including one superseded a second ago whose pointer POPs are
    // still serving. (A dry-run may preview at zero; it deletes nothing.)
    if !dry_run && older_than_secs == 0 {
        return Err(
            "refusing to reclaim: a destructive `config gc` requires a non-zero `--older-than` \
             window. Zero asserts nothing -- it would make every orphan eligible, including \
             chunks a pointer POPs are still serving. Nothing was deleted."
                .to_owned(),
        );
    }
    let resolved_id = resolve_remote_config_store_id(store_name)?
        .ok_or_else(|| no_matching_store_error(store_name))?;
    let items = list_config_store_entries(&resolved_id)?;
    let plan = plan_gc_reclamation(&items, unix_now_secs(), older_than_secs)?;
    let GcPlan {
        doomed,
        kept_roots,
        live_count,
        retained_recent,
        roots,
        unprovable,
        warnings,
    } = plan;

    let doomed_count: usize = doomed.iter().map(Vec::len).sum();
    let mut out = vec![format!(
        "fastly config-store `{store_name}` (id={resolved_id}): {} entries, {roots} root(s), {live_count} referenced chunk(s), {doomed_count} orphan(s) in {} generation(s) older than {older_than_secs}s, {retained_recent} orphan(s) too recent",
        items.len(),
        doomed.len(),
    )];
    out.extend(warnings);
    append_kept_roots_report(&mut out, &kept_roots, live_count);
    if unprovable > 0 {
        // NEVER silent: these entries look like chunk keys but we could not
        // prove our writer produced them, so we left them alone. Say so, or the
        // summary reads as "everything reclaimable was reclaimed".
        out.push(format!(
            "  {unprovable} chunk-shaped entr(ies) left untouched: they are not byte-identical to what this writer would produce (wrong content-address, a split this writer would not choose, an incomplete generation, or a count it would never emit), so EdgeZero cannot claim them"
        ));
    }
    if doomed_count == 0 {
        out.push("nothing to reclaim".to_owned());
        return Ok(out);
    }
    if dry_run {
        // A dry-run only PLANS: list every candidate and stop. Nothing is
        // attempted, so there is no confirmed/failed/skipped distinction yet.
        for (key, age) in doomed.iter().flatten() {
            out.push(format!("  would delete `{key}` (age {age}s)"));
        }
        // `--yes` ALWAYS requires an explicit non-zero `--older-than` (a
        // destructive run must not guess the window), so the apply instruction
        // names both -- "re-run with --yes" alone would be rejected.
        out.push(format!(
            "dry-run: {doomed_count} orphan chunk(s) planned for deletion; re-run with \
             `--yes --older-than <dur>` (a non-zero window is required) to apply"
        ));
        return Ok(out);
    }
    // Real run: `doomed_count` is the PLANNED count. Do NOT pre-print each key as
    // "deleting" -- execution stops at a generation's first failure, so some
    // planned keys are never attempted. `execute_gc_deletes` reports the real
    // per-key outcome (deleted / FAILED / skipped) as it happens.
    out.push(format!(
        "reclaiming {doomed_count} planned orphan chunk(s) across {} generation(s)",
        doomed.len()
    ));

    let GcDeleteOutcome {
        deleted,
        failed,
        stranded,
        uncertain,
    } = execute_gc_deletes(&resolved_id, &doomed, &mut out);
    out.push(format!(
        "reclaimed {deleted} of {doomed_count} orphan chunk entries"
    ));
    if failed.is_empty() {
        return Ok(out);
    }
    // Partial/total failure must be a non-zero exit so automation can see it.
    let mut diagnostic = format!(
        "{}\nconfig gc: {} of {doomed_count} deletes FAILED ({})",
        out.join("\n"),
        failed.len(),
        failed.join(", ")
    );
    // A generation whose only failure was on an unconfirmed delete: the outcome
    // is UNKNOWN (Fastly may have committed it), so a re-run is worth trying but
    // may find a fragment.
    if !uncertain.is_empty() {
        write!(
            diagnostic,
            ".\nNOTE: a failed remote delete has an unknown outcome -- Fastly may have applied it \
             before returning an error. Re-run `config gc`: it reclaims each affected generation \
             if it is still whole, or reports it as an unprovable fragment (\"left untouched\") if \
             a delete did commit. If reported as a fragment, remove the survivors by hand:\n{}",
            recovery_commands(&resolved_id, &uncertain)
        )
        .map_err(|err| format!("failed to format the gc diagnostic: {err}"))?;
    }
    // A generation with a CONFIRMED prior delete: definitely a fragment now.
    if !stranded.is_empty() {
        write!(
            diagnostic,
            ".\nWARNING: {} entr(ies) are now an INCOMPLETE generation because a sibling was \
             already deleted before the failure: {}. `config gc` proves a generation by \
             reassembling it, so it can no longer prove these and will never reclaim them -- \
             re-running will NOT help. They are inert (no pointer references them). Remove them \
             by hand once you are satisfied they are unreferenced:\n{}",
            stranded.len(),
            stranded.join(", "),
            recovery_commands(&resolved_id, &stranded),
        )
        .map_err(|err| format!("failed to format the gc diagnostic: {err}"))?;
    }
    Err(diagnostic)
}

/// Render copy-pasteable `fastly config-store-entry delete` commands, one per
/// key, with EVERY interpolated value single-quoted for POSIX shells.
fn recovery_commands(store_id: &str, keys: &[String]) -> String {
    let commands = keys
        .iter()
        .map(|key| {
            format!(
                "  fastly config-store-entry delete --store-id={} --key={} --auto-yes",
                shell_single_quote(store_id),
                shell_single_quote(key),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "  # POSIX/bash (Linux/macOS). On Windows cmd/PowerShell the quoting \
         differs -- adapt it for your shell.\n{commands}"
    )
}

/// Single-quote a value for a POSIX shell: wrap in `'...'` and rewrite each
/// embedded `'` as `'\''`. Inside single quotes every other byte -- `$`, spaces,
/// `;`, `$(...)`, backticks -- is literal, so this neutralises any hostile key.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Delete each doomed generation, stopping a generation at its FIRST failure.
///
/// A generation is provable only as a whole (`prove_generation` reassembles it),
/// so a half-deleted one can never be proved again. Generations are independent,
/// so a failure in one does not stop the others.
fn execute_gc_deletes(
    resolved_id: &str,
    doomed: &[Vec<(String, u64)>],
    out: &mut Vec<String>,
) -> GcDeleteOutcome {
    let mut outcome = GcDeleteOutcome {
        deleted: 0,
        failed: Vec::new(),
        stranded: Vec::new(),
        uncertain: Vec::new(),
    };
    for generation in doomed {
        let mut deleted_here: Vec<&str> = Vec::new();
        for (key, _) in generation {
            match delete_config_store_entry(resolved_id, key) {
                Ok(()) => {
                    outcome.deleted = outcome.deleted.saturating_add(1);
                    deleted_here.push(key.as_str());
                    // CONFIRMED gone, per key, as it happens.
                    out.push(format!("  deleted `{key}`"));
                }
                Err(err) => {
                    out.push(format!("  FAILED to delete `{key}` ({err})"));
                    outcome.failed.push(key.clone());
                    // Everything in this generation we have NOT confirmed deleted
                    // -- the failed key itself, plus the ones we never reached.
                    let unconfirmed: Vec<String> = generation
                        .iter()
                        .map(|(member, _)| member.clone())
                        .filter(|member| !deleted_here.contains(&member.as_str()))
                        .collect();
                    // Distinguish the ones we NEVER ATTEMPTED (after the stop)
                    // from the failed key itself, so the report is not read as
                    // "all of these were tried and failed".
                    for skipped in unconfirmed.iter().filter(|member| *member != key) {
                        out.push(format!(
                            "  skipped `{skipped}` (not attempted: this generation's delete stopped at the failure above)"
                        ));
                    }
                    if deleted_here.is_empty() {
                        // No sibling is CONFIRMED gone. The failed delete's
                        // outcome is unknown: if it did not commit, the
                        // generation is whole and a re-run reclaims it; if it
                        // did, the re-run finds a fragment and reports it.
                        outcome.uncertain.extend(unconfirmed);
                    } else {
                        // A sibling is CONFIRMED gone, so this generation is
                        // definitely a fragment no future run can prove.
                        outcome.stranded.extend(unconfirmed);
                    }
                    break; // stop THIS generation; the others are independent
                }
            }
        }
    }
    outcome
}

/// Classify a store's entries: the live chunk set, the protected root keys, and
/// the root count.
///
/// Root-vs-chunk is decided by VALUE, not key shape. The runtime resolver reads
/// whatever value sits at a key, so ANY entry whose value is a valid direct
/// envelope or a chunk pointer is a runtime-readable root and must never be
/// deleted — even at a chunk-shaped key.
fn classify_store_entries(
    items: &[ConfigStoreItem],
    value_by_key: &HashMap<&str, &str>,
) -> Result<GcClassification, String> {
    let mut live: HashSet<String> = HashSet::new();
    let mut protected: HashSet<String> = HashSet::new();
    let mut roots = 0_usize;
    let mut warnings: Vec<String> = Vec::new();
    for item in items {
        let is_chunk_shaped = chunk_key_generation_any(&item.item_key).is_some();
        let classified = match gc_classify_root(&item.item_key, &item.item_value) {
            Ok(classified) => classified,
            // A chunk-shaped key whose value we cannot classify is a genuine
            // chunk fragment (a candidate) ONLY if BOTH hold: the value ANNOUNCES
            // no kind, AND NOTHING is nested beneath this key.
            Err(_)
                if is_chunk_shaped
                    && !value_announces_our_kind(&item.item_value)
                    && !value_is_future_format(&item.item_value)
                    && !items.iter().any(|other| {
                        other.item_key != item.item_key
                            && chunk_key_generation(&item.item_key, &other.item_key).is_some()
                    }) =>
            {
                continue; // a leaf chunk payload: a delete candidate
            }
            // A definitively FOREIGN entry at an ORDINARY key. The runtime returns
            // it verbatim and it references no chunks, so protect it as a
            // zero-reference root. Three guards keep this from masking corruption.
            Err(_)
                if value_is_inert_foreign(&item.item_value)
                    && !value_is_future_format(&item.item_value)
                    && !item.item_key.contains(CHUNK_KEY_INFIX) =>
            {
                roots = roots.saturating_add(1);
                protected.insert(item.item_key.clone());
                continue;
            }
            Err(err) => {
                return Err(format!(
                    "refusing to reclaim: could not classify root `{}` ({err}); nothing was deleted",
                    item.item_key
                ));
            }
        };
        // A runtime-readable root, wherever it lives: never a delete candidate.
        roots = roots.saturating_add(1);
        protected.insert(item.item_key.clone());
        let GcRootValue::Chunked(pointer) = classified else {
            continue; // A direct envelope references no chunks.
        };
        // The pointer's METADATA is self-consistent by here. That is not proof
        // that it honestly describes its generation, so reassemble what it
        // references and hold the bytes against its content-address.
        let assembled = assemble_pointer_chunks(&item.item_key, &pointer, value_by_key)?;
        // The reassembled value may be a NEWER inner format that `BlobEnvelope`
        // deserialize silently ignores. Fail closed.
        if value_is_future_format(&assembled) {
            return Err(format!(
                "refusing to reclaim: root `{}` reconstructs to a value in a newer format this \
                 build does not recognise. It may reference generations this build cannot see, so \
                 treating its outer chunks as the whole live set could delete live data. Nothing \
                 was deleted.",
                item.item_key
            ));
        }
        gc_verify_generation(&pointer.envelope_sha256, &assembled).map_err(|err| {
            format!(
                "refusing to reclaim: root `{}` names a chunk set that does not reconstruct the \
                 envelope it claims ({err}). Its chunk list is therefore not a trustworthy live \
                 set, and treating it as one could delete a live chunk. Nothing was deleted.",
                item.item_key
            )
        })?;
        // Same exact-split predicate the RUNTIME resolver applies. A pointer whose
        // boundaries are not the ones this writer emits reassembles correctly here
        // but is REJECTED at runtime, so warn (still protecting it).
        if let Err(err) =
            verify_writer_split_layout(&item.item_key, &assembled, &chunk_lengths(&pointer.chunks))
        {
            warnings.push(format!(
                "warning: root `{}` is NOT runtime-readable ({err}). Its chunks are kept, but this \
                 generation can never be proven writer-produced, so `config gc` will never reclaim \
                 it. Re-run `config push` for this key to rewrite it, then re-run `config gc`.",
                item.item_key
            ));
        }
        live.extend(pointer.chunks.into_iter().map(|chunk| chunk.key));
    }
    Ok(GcClassification {
        live,
        protected,
        roots,
        warnings,
    })
}

/// The reclamation plan for one store: which orphan chunk entries to delete, and
/// the counts for the summary line. Deriving it is where every safety guard
/// lives, so it is fail-closed throughout — any unreadable/incomplete state
/// returns `Err` and the caller deletes nothing.
fn plan_gc_reclamation(
    items: &[ConfigStoreItem],
    now: u64,
    older_than_secs: u64,
) -> Result<GcPlan, String> {
    let mut value_by_key: HashMap<&str, &str> = HashMap::with_capacity(items.len());
    let mut created_by_key: HashMap<&str, u64> = HashMap::with_capacity(items.len());
    for item in items {
        let Some(created) = parse_rfc3339_secs(&item.created_at) else {
            // Unparseable timestamp anywhere in the listing -> fail closed. On a
            // DELETE path we will not guess an age.
            return Err(format!(
                "refusing to reclaim: entry `{}` has an unreadable `created_at`; nothing was deleted",
                item.item_key
            ));
        };
        created_by_key.insert(item.item_key.as_str(), created);
        value_by_key.insert(item.item_key.as_str(), item.item_value.as_str());
    }

    // ---- 1. Classify entries: live chunks, protected roots, root count ----
    let GcClassification {
        live,
        protected,
        roots,
        warnings,
    } = classify_store_entries(items, &value_by_key)?;

    // ---- 2. Per-root live-config age (best-effort; see the guard below) ----
    // rsplit_once (the LAST infix): a chunk of a chunk-shaped root nests the infix
    // twice, and its root is everything before the LAST one.
    let root_live_since: HashMap<&str, u64> = live.iter().fold(HashMap::new(), |mut acc, key| {
        if let Some((root, _)) = key.rsplit_once(CHUNK_KEY_INFIX) {
            let created = *created_by_key.get(key.as_str()).unwrap_or(&0);
            let slot = acc.entry(root).or_insert(0);
            *slot = (*slot).max(created);
        }
        acc
    });

    // ---- 3. Candidates, grouped by GENERATION and proven writer-produced ----
    let mut groups: BTreeMap<(&str, String), Vec<&ConfigStoreItem>> = BTreeMap::new();
    for item in items {
        if live.contains(&item.item_key) {
            continue;
        }
        if protected.contains(&item.item_key) {
            continue;
        }
        let Some((root, _)) = item.item_key.rsplit_once(CHUNK_KEY_INFIX) else {
            continue; // a root
        };
        let Some(generation) = chunk_key_generation(root, &item.item_key) else {
            continue; // chunk-shaped but NOT canonical => never a key we emit
        };
        groups.entry((root, generation)).or_default().push(item);
    }

    let mut doomed: Vec<Vec<(String, u64)>> = Vec::new();
    let mut retained_recent = 0_usize;
    let mut unprovable = 0_usize;
    for ((root, generation), mut group) in groups {
        if prove_generation(root, &generation, &group).is_err() {
            // We cannot prove we wrote this, so we do not touch it. Skipped
            // rather than fatal: one foreign entry must not block reclamation of
            // the store forever. Reported in the summary.
            unprovable = unprovable.saturating_add(group.len());
            continue;
        }

        // Age the generation as a UNIT, by its youngest member.
        let group_age = group
            .iter()
            .map(|item| {
                now.saturating_sub(*created_by_key.get(item.item_key.as_str()).unwrap_or(&0))
            })
            .min()
            .unwrap_or(0);
        // BOTH ages must clear the operator's window; take the more restrictive.
        let effective_age = root_live_since.get(root).map_or(group_age, |live_since| {
            group_age.min(now.saturating_sub(*live_since))
        });
        if effective_age < older_than_secs {
            retained_recent = retained_recent.saturating_add(group.len());
            continue;
        }
        // Delete in canonical chunk-INDEX order (`.0`, `.1`, ...), NOT the remote
        // listing order, so preview order and stranding are deterministic.
        group.sort_by_key(|item| chunk_key_index(root, &item.item_key).unwrap_or(usize::MAX));
        doomed.push(
            group
                .iter()
                .map(|item| {
                    let age = now
                        .saturating_sub(*created_by_key.get(item.item_key.as_str()).unwrap_or(&0));
                    (item.item_key.clone(), age)
                })
                .collect(),
        );
    }

    let mut kept_roots: Vec<String> = protected.into_iter().collect();
    kept_roots.sort();

    Ok(GcPlan {
        doomed,
        kept_roots,
        live_count: live.len(),
        retained_recent,
        roots,
        unprovable,
        warnings,
    })
}

/// Reassemble the chunks a live pointer references, in index order, checking each
/// against the pointer's own per-chunk `len`/`sha256` along the way.
///
/// Fails closed when a referenced key is absent from the listing.
fn assemble_pointer_chunks(
    root_key: &str,
    pointer: &GcPointer,
    value_by_key: &HashMap<&str, &str>,
) -> Result<String, String> {
    let mut assembled = String::new();
    // The chunk KEY is pointer-controlled, so diagnostics name a POSITION, not
    // the key.
    for (position, chunk) in pointer.chunks.iter().enumerate() {
        let Some(value) = value_by_key.get(chunk.key.as_str()) else {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` references chunk {position}, which is \
                 absent from the store listing (the listing may be incomplete/paginated, or the \
                 store is already inconsistent); nothing was deleted"
            ));
        };
        if value.len() != chunk.len {
            return Err(format!(
                "refusing to reclaim: root `{root_key}` says chunk {position} is {} bytes but the \
                 store holds {}; nothing was deleted",
                chunk.len,
                value.len()
            ));
        }
        if sha256_hex(value.as_bytes()) != chunk.sha256 {
            return Err(format!(
                "refusing to reclaim: the stored value of chunk {position} does not match the \
                 SHA-256 that root `{root_key}` records for it; nothing was deleted"
            ));
        }
        assembled.push_str(value);
    }
    if assembled.len() != pointer.envelope_len {
        return Err(format!(
            "refusing to reclaim: root `{root_key}` declares an envelope of {} bytes but its \
             chunks reassemble to {}; nothing was deleted",
            pointer.envelope_len,
            assembled.len()
        ));
    }
    Ok(assembled)
}

/// Is this candidate generation byte-identical to what THIS writer would have
/// produced for the bytes it contains? The gate on every delete. `group` is
/// every listed entry sharing one `(root, generation)`.
fn prove_generation(
    root: &str,
    generation: &str,
    group: &[&ConfigStoreItem],
) -> Result<(), String> {
    let mut ordered: Vec<(usize, &str)> = Vec::with_capacity(group.len());
    for item in group {
        let index = item
            .item_key
            .rsplit_once('.')
            .and_then(|(_, index)| index.parse::<usize>().ok())
            .ok_or_else(|| format!("`{}` has no readable index", item.item_key))?;
        ordered.push((index, item.item_value.as_str()));
    }
    ordered.sort_by_key(|&(index, _)| index);
    for (position, &(index, _)) in ordered.iter().enumerate() {
        if index != position {
            return Err(format!(
                "indexes are not dense 0..n-1 (found {index} at position {position})"
            ));
        }
    }
    let assembled: String = ordered.iter().map(|&(_, value)| value).collect();

    // 1. The bytes must be the generation the keys name, and a real envelope.
    gc_verify_generation(generation, &assembled)?;

    // 2. ...and the writer, given those bytes, must produce EXACTLY these entries.
    let expected = prepare_fastly_config_entries(root, &assembled)
        .map_err(|err| format!("this writer could not re-derive the generation ({err})"))?;
    let Some(expected_chunks) = expected.get(..expected.len().saturating_sub(1)) else {
        return Err("this writer produced no chunk entries for these bytes".to_owned());
    };
    if expected_chunks.is_empty() {
        // The envelope fits directly, so the writer would never have chunked it.
        return Err(
            "these bytes fit the entry limit, so this writer would have stored them directly \
             rather than in chunks"
                .to_owned(),
        );
    }
    if expected_chunks.len() != ordered.len() {
        return Err(format!(
            "this writer would split these bytes into {} chunk(s), not {}",
            expected_chunks.len(),
            ordered.len()
        ));
    }
    for ((expected_key, expected_value), item) in
        expected_chunks.iter().zip(group_in_index_order(group))
    {
        if *expected_key != item.item_key {
            return Err(format!(
                "this writer would not have produced the key `{}`",
                item.item_key
            ));
        }
        if *expected_value != item.item_value {
            return Err(format!(
                "the stored value of `{}` is not the chunk this writer would have written at that \
                 index",
                item.item_key
            ));
        }
    }
    Ok(())
}

/// `group` sorted by chunk index, so it lines up with the writer's output order.
fn group_in_index_order<'item>(group: &[&'item ConfigStoreItem]) -> Vec<&'item ConfigStoreItem> {
    let mut ordered: Vec<&ConfigStoreItem> = group.to_vec();
    ordered.sort_by_key(|item| {
        item.item_key
            .rsplit_once('.')
            .and_then(|(_, index)| index.parse::<usize>().ok())
            .unwrap_or(usize::MAX)
    });
    ordered
}

/// Is this key a chunk key of ANY root? (`config gc` scans the whole store, so
/// it cannot scope to one root up front.) Validates the canonical shape.
fn chunk_key_generation_any(key: &str) -> Option<String> {
    // Split on the LAST infix, not the first: a chunk of a root that ITSELF
    // contains the infix has the infix twice, and its chunk suffix is after the
    // LAST one.
    let (root, _rest) = key.rsplit_once(CHUNK_KEY_INFIX)?;
    chunk_key_generation(root, key)
}

fn delete_config_store_entry(store_id: &str, key: &str) -> Result<(), String> {
    let store_arg = format!("--store-id={store_id}");
    let key_arg = format!("--key={key}");
    let output = Command::new("fastly")
        .args([
            "config-store-entry",
            "delete",
            store_arg.as_str(),
            key_arg.as_str(),
            "--auto-yes",
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
        return Ok(());
    }
    // EVERY non-zero delete is a failure -- no "already gone" special case, and
    // redact stderr: a Fastly error can quote the entry value back.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "`fastly config-store-entry delete --store-id={store_id} --key={key} --auto-yes` exited with status {}\n{}",
        output.status,
        redact_stderr(&stderr)
    ))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::path_mutation_guard;
    use super::*;
    #[cfg(unix)]
    use crate::cli::test_support::*;
    #[cfg(unix)]
    use edgezero_core::test_env::PathPrepend;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use tempfile::tempdir;

    #[test]
    fn parse_rfc3339_secs_rounds_a_fraction_up() {
        let whole = parse_rfc3339_secs("2026-01-01T00:00:42Z").expect("whole");
        // A fractional second rounds UP to the next whole second, so the computed
        // age stays conservative and a key never ages into deletion early.
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T00:00:42.998Z"),
            Some(whole + 1),
            "a fractional creation time must round UP, not floor"
        );
        // Even a tiny fraction rounds up.
        assert_eq!(
            parse_rfc3339_secs("2026-01-01T00:00:42.000001Z"),
            Some(whole + 1)
        );
        // A whole-second stamp is unchanged.
        assert_eq!(parse_rfc3339_secs("2026-01-01T00:00:42.000Z"), Some(whole));
    }

    // ---------- config gc (operator-invoked reclamation) ----------

    /// gc never deletes a chunk the LIVE root pointer references, however old.
    #[cfg(unix)]
    #[test]
    fn gc_never_deletes_live_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        // The live generation is ANCIENT, but it is referenced by the root.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 1, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "live chunk `{key}` must never be reclaimed; log:\n{log}\nout: {out:?}"
            );
        }
    }

    /// gc reclaims unreferenced chunks older than the operator's threshold.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_unreferenced_chunks_older_than_threshold() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        // The live config has been stable for 2 days; the operator asserts a 1-day
        // window. So everything superseded (<= when live went live, i.e. >= 2
        // days ago) is safely reclaimable.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800)); // a week old

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "orphan `{key}` older than the threshold must be reclaimed; out: {out:?}"
            );
        }
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "live chunk `{key}` must survive"
            );
        }
    }

    /// The soundness test (design-3 counterexample): a root whose
    /// current config was deployed seconds ago must NOT have its prior generation
    /// reclaimed, even if that generation's chunks are ANCIENT. The clock is the
    /// live config's age, not the orphan chunk's own creation time.
    #[cfg(unix)]
    #[test]
    fn gc_protects_recently_superseded_generation_with_old_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let prior = gen_envelope("prior");
        let prior_chunks = chunk_keys_of(TEST_CONFIG_ID, &prior);

        // Live config went live 30s ago; the prior generation's chunks are a year
        // old but were superseded only 30s ago -> POPs may still serve them.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 30)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 30));
        listing.extend(listed_generation(TEST_CONFIG_ID, &prior, 31_536_000)); // ~1 year

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // Even a generous 1-day threshold must NOT delete the prior generation,
        // because the live config has only been stable for 30 seconds.
        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &prior_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a generation superseded 30s ago must be retained despite old chunks: `{key}`; log:\n{log}"
            );
        }
    }

    /// a live root whose pointer drops its
    /// last chunk ref AND restates `envelope_len` as the remaining sum passes
    /// every metadata check. The dropped chunk is then absent from the live set
    /// and looks like a deletable orphan -- while the config still needs it.
    ///
    /// Guards the PLANNER's content verification (a unit test on
    /// `gc_verify_generation` alone does not prove the planner calls it).
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_when_a_live_pointer_underreports_its_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Padded so the generation is >= 3 chunks: this case needs a ref to
        // drop that still leaves a plausible multi-chunk set behind.
        let live = gen_envelope_padded("live", 20_000);
        let (chunks, pointer_json) = chunked_parts(TEST_CONFIG_ID, &live);
        assert!(chunks.len() >= 3, "need >= 3 chunks for this case");

        // Doctor the pointer: drop the last ref, restate envelope_len to match
        // the survivors. Generation, indexes, per-chunk lens and the sum all
        // still agree -- only the CONTENT does not.
        let mut pointer: serde_json::Value = serde_json::from_str(&pointer_json).expect("parse");
        let refs = pointer
            .get_mut("chunks")
            .and_then(serde_json::Value::as_array_mut)
            .expect("chunks array");
        refs.pop().expect("drop the last chunk ref");
        let surviving_len: u64 = refs
            .iter()
            .filter_map(|chunk| chunk.get("len").and_then(serde_json::Value::as_u64))
            .sum();
        pointer["envelope_len"] = serde_json::json!(surviving_len);
        let doctored = serde_json::to_string(&pointer).expect("serialise");

        // The store still physically holds ALL the chunks, including the one the
        // doctored pointer no longer names.
        let orphaned_by_omission = chunks.last().expect("last chunk").0.clone();
        let stamp = stamp_secs_ago(999_999);
        let mut listing = vec![(TEST_CONFIG_ID.to_owned(), stamp.clone(), doctored)];
        for (key, value) in chunks {
            listing.push((key, stamp.clone(), value));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("does not reconstruct the envelope it claims"),
            "expected a content-address mismatch on the live pointer, got: {err}"
        );
        assert!(
            !oplog_has(&oplog, &format!("delete {orphaned_by_omission}")),
            "a chunk the live config still needs must never be deleted because its pointer \
             under-reported it: `{orphaned_by_omission}`"
        );
    }

    /// a LONE entry whose value hashes to the generation
    /// its own key names would otherwise "prove" itself and be deleted. But our
    /// writer never emits a one-chunk generation (an oversized envelope always
    /// splits into >= 2), so a group of one is never ours -- it is a root-like
    /// value sitting at a chunk-shaped key. This is the case a pure hash check
    /// cannot catch on its own.
    #[cfg(unix)]
    #[test]
    fn gc_never_reclaims_a_lone_self_consistent_chunk() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        // A complete envelope stored at a chunk-shaped key whose generation IS
        // that envelope's own SHA -- so it reassembles to its content-address.
        let squatter_value = gen_envelope("someones-real-config");
        let self_sha = sha256_hex(squatter_value.as_bytes());
        let squatter_key = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{self_sha}.0");
        listing.push((
            squatter_key.clone(),
            stamp_secs_ago(31_536_000),
            squatter_value,
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !oplog_has(&oplog, &format!("delete {squatter_key}")),
            "a one-chunk 'generation' is never something this writer emitted, so it must not be \
             reclaimed even though it hashes to its own key: `{squatter_key}`; log:\n{log}"
        );
    }

    /// a delete that fails on a generation's FIRST key has
    /// an UNKNOWN outcome -- Fastly may have committed it before returning an
    /// error.  called this "whole and retryable", which is unsound: if the
    /// failed delete did commit, a re-run finds a fragment. The honest report is
    /// a NOTE that the outcome is uncertain, NOT a clean-retry promise. We still
    /// stop the generation so a CONFIRMED partial delete cannot happen.
    #[cfg(unix)]
    #[test]
    fn gc_first_delete_failure_is_reported_as_uncertain_not_clean_retry() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // The FIRST chunk of the doomed generation fails to delete.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&dead_chunks[0]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        assert!(
            err.contains("unknown outcome"),
            "a failed delete's outcome is unknown and must be reported as such: {err}"
        );
        assert!(
            !err.contains("will retry them"),
            "the disproven clean-retry promise must be gone: {err}"
        );
        // The siblings must NOT have been ATTEMPTED -- stopping is what prevents a
        // CONFIRMED partial delete.
        for key in dead_chunks.iter().skip(1) {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "after the first failure the generation must be left alone: `{key}`"
            );
        }
    }

    /// the stateful case. A remote delete that COMMITS but
    /// still reports failure leaves a real fragment. On the SECOND run that
    /// missing key makes the generation unprovable, so it must be reported as
    /// left-untouched (surfaced), never silently dropped.
    #[cfg(unix)]
    #[test]
    fn gc_committed_but_failed_delete_surfaces_as_unprovable_next_run() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");

        // SECOND run's world: the first chunk's delete committed last time, so it
        // is gone. The generation is now a fragment.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let mut dead_gen = listed_generation(TEST_CONFIG_ID, &dead, 604_800);
        let survivor = dead_gen[1].0.clone();
        dead_gen.remove(0); // the committed-deleted chunk is absent now
        listing.extend(dead_gen);

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        assert!(
            !oplog_has(&oplog, &format!("delete {survivor}")),
            "an unprovable fragment survivor must not be deleted: `{survivor}`"
        );
        assert!(
            out.iter()
                .any(|line| line.contains("not byte-identical to what this writer would produce")),
            "the surviving fragment must be SURFACED as left-untouched, not silently dropped: {out:?}"
        );
    }

    /// if a delete fails PART-WAY through a generation, the
    /// survivors are an incomplete generation that `prove_generation` can never
    /// verify again -- so `gc` will never reclaim them. Claiming "re-run to
    /// retry" there was false. Say plainly that recovery is manual.
    #[cfg(unix)]
    #[test]
    fn gc_reports_stranded_survivors_as_manual_recovery() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Padded to >= 3 chunks so a mid-generation failure leaves survivors.
        let live = gen_envelope("live");
        let dead = gen_envelope_padded("dead", 20_000);
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 3, "need >= 3 chunks");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // The SECOND chunk fails: the first is already gone by then.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&dead_chunks[1]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        assert!(
            err.contains("INCOMPLETE generation") && err.contains("re-running will NOT help"),
            "a stranded fragment must not be described as retryable: {err}"
        );
        // It must name the survivors and how to remove them by hand.
        for key in dead_chunks.iter().skip(2) {
            assert!(
                err.contains(key.as_str()),
                "the operator needs the exact surviving keys: `{key}` missing from: {err}"
            );
        }
        assert!(
            err.contains("fastly config-store-entry delete"),
            "give the operator the recovery command: {err}"
        );
        // And we stopped rather than deleting the rest.
        for key in dead_chunks.iter().skip(2) {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "deletion must stop at the first failure in a generation: `{key}`"
            );
        }
    }

    /// root keys are free-form, so a chunk key can hold
    /// shell metacharacters. Manual-recovery commands must render them so that
    /// pasting cannot execute or misparse -- single-quoted, with embedded quotes
    /// escaped.
    #[test]
    fn recovery_commands_are_shell_safe() {
        // A key crafted to run `id` and to break argument parsing if unquoted.
        let hostile = "app$(id).__edgezero_chunks.'; rm -rf /'.0".to_owned();
        let keys = [hostile.clone()];
        let rendered = recovery_commands("store-abc", &keys);

        // The dangerous substring is not sitting there unquoted.
        assert!(
            !rendered.contains("$(id)") || rendered.contains("'app$(id)"),
            "shell-active text must be inside single quotes: {rendered}"
        );
        // Every embedded single quote is closed-escaped-reopened, so no quote
        // context leaks.
        assert!(
            rendered.contains(r"'\''"),
            "embedded single quotes must be escaped as '\\'': {rendered}"
        );
        // Sanity: what a POSIX shell would parse back out of our --key argument
        // is EXACTLY the original key (round-trip through `sh`).
        let key_arg = rendered
            .split("--key=")
            .nth(1)
            .and_then(|rest| rest.split(" --auto-yes").next())
            .expect("a --key argument");
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s' {key_arg}"))
            .output()
            .expect("run sh");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            hostile,
            "the shell must parse the quoted argument back to the exact key"
        );
    }

    /// a valid DIRECT envelope at a chunk-shaped key is a
    /// runtime-readable root, but earlier classification only protected
    /// POINTER values there.
    ///
    /// Construction: pad a small valid envelope with trailing JSON whitespace
    /// past the entry limit. The writer chunks it; chunk 0 (the first 7 000
    /// bytes) is the whole envelope plus trailing spaces, which STILL parses and
    /// verifies as that envelope. So chunk 0's key holds a valid direct envelope
    /// -- a root -- yet the generation round-trips through the writer and passes
    /// every proof, so GC deletes chunk 0.
    #[cfg(unix)]
    #[test]
    fn valid_envelope_at_chunk_shaped_key_is_a_protected_root() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A small valid envelope + trailing whitespace over the entry limit.
        let envelope = BlobEnvelope::new(json!({"k":"v"}), "2026-06-22T00:00:00Z".into());
        let mut padded = serde_json::to_string(&envelope).unwrap();
        padded.push_str(&" ".repeat(8_200));
        let entries = prepare_fastly_config_entries(TEST_CONFIG_ID, &padded).expect("expand");
        assert!(entries.len() >= 3, "need >= 2 chunks + pointer");
        let holder_key = entries[0].0.clone();
        // Sanity: chunk 0's value IS a standalone valid envelope.
        let parsed: BlobEnvelope =
            serde_json::from_str(&entries[0].1).expect("chunk 0 must parse as an envelope");
        parsed.verify().expect("chunk 0 must verify as an envelope");

        // Seed the store with the chunk entries only -- NO live pointer refers
        // to them, so this generation looks orphaned. Aged old.
        let stamp = stamp_secs_ago(604_800);
        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        for (key, value) in &entries[..entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        drop(run_gc(dir.path(), 86_400, false));
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !oplog_has(&oplog, &format!("delete {holder_key}")),
            "an entry whose value is a valid direct envelope is a runtime-readable root and must \
             never be deleted, whatever its key looks like: `{holder_key}`; log:\n{log}"
        );
        // The SIBLING chunks must survive too: protecting the holder drops the
        // generation to an incomplete group, which is left unprovable — so
        // nothing in this generation is deleted, not just the holder.
        for (key, _) in &entries[1..entries.len().saturating_sub(1)] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a sibling of a protected root must also survive (the group is left \
                 unprovable): `{key}`; log:\n{log}"
            );
        }
    }

    /// A self-scoped pointer at a chunk-shaped holder key (its chunks nest the
    /// infix twice) must NOT abort store-wide GC: the doubly-nested chunks are
    /// recognised as chunks (via the LAST infix), so the holder classifies as a
    /// root, its references are counted live, and other roots still reclaim.
    #[cfg(unix)]
    #[test]
    fn gc_tolerates_a_self_scoped_pointer_at_a_chunk_shaped_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A pointer parked at a chunk-shaped key, with chunks scoped to itself.
        let holder_key = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "e".repeat(64));
        let nested = gen_envelope("nested");
        let nested_entries = prepare_fastly_config_entries(&holder_key, &nested).expect("expand");
        let (_, holder_pointer) = nested_entries.last().expect("pointer").clone();

        // A normal live root, and a normal orphan generation that SHOULD reclaim.
        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));
        listing.push((holder_key.clone(), stamp.clone(), holder_pointer));
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // The run must SUCCEED (not abort) and still reclaim the ordinary orphan.
        run_gc(dir.path(), 86_400, false).expect("store-wide GC must not abort");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "an ordinary orphan must still be reclaimed despite the self-scoped pointer: `{key}`"
            );
        }
        assert!(
            !oplog_has(&oplog, &format!("delete {holder_key}")),
            "the chunk-shaped holder root must never be deleted"
        );
    }

    /// A nested ORPHAN generation (chunks scoped to a chunk-shaped root, with NO
    /// live pointer referencing them) must be grouped and reclaimed, not silently
    /// dropped. Age and grouping split on the LAST infix, so the nested chunks are
    /// attributed to their real (nested) root.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_a_nested_orphan_generation() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A chunk-shaped root, and a full generation of chunks SCOPED to it — but
        // no pointer references them, so they are orphaned.
        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "f".repeat(64));
        let nested = gen_envelope("nested-orphan");
        let nested_entries = prepare_fastly_config_entries(&nested_root, &nested).expect("expand");
        let nested_chunks: Vec<String> = nested_entries[..nested_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &nested_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "a nested orphan generation must be reclaimed, not silently dropped: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// FAIL CLOSED: a MALFORMED pointer sitting at a chunk-shaped root that HAS a
    /// nested generation beneath it must abort GC, not let that nested generation
    /// be reclaimed. The truncated pointer cannot announce its discriminator, so
    /// it looks like a chunk fragment -- but its nested chunks are proven
    /// independently and would be deleted while their (unreadable) root can no
    /// longer name them. That is exactly the truncated-pointer data loss the
    /// spec forbids, so the whole run must refuse.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_malformed_pointer_at_a_chunk_shaped_root_with_nested_chunks() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "f".repeat(64));
        let nested = gen_envelope("nested");
        let nested_entries = prepare_fastly_config_entries(&nested_root, &nested).expect("expand");
        let nested_chunks: Vec<String> = nested_entries[..nested_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let stamp = stamp_secs_ago(604_800);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // A truncated pointer at the chunk-shaped nested root: it WAS a pointer,
        // now cut off, so it cannot announce its `edgezero_kind`.
        listing.push((
            nested_root.clone(),
            stamp.clone(),
            r#"{"chunks":[{"key":"#.to_owned(),
        ));
        // ...its aged, independently-provable nested generation.
        for (key, value) in &nested_entries[..nested_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp.clone(), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false)
            .expect_err("an unreadable nested root must fail closed, not be reclaimed");
        assert!(
            err.contains("refusing to reclaim"),
            "must fail closed, not delete: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &nested_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a nested generation under an unreadable root must NOT be deleted: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// Age attribution works per NESTED root: a nested orphan generation whose
    /// nested root's live config went live RECENTLY must be RETAINED (POPs may
    /// still serve the superseded generation), even though the orphan's own
    /// chunks are old. This pins that `root_live_since` splits on the last infix.
    #[cfg(unix)]
    #[test]
    fn gc_retains_a_nested_orphan_under_a_recently_changed_nested_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let nested_root = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "a".repeat(64));

        // The nested root's CURRENT (live) generation, created 30s ago.
        let live_nested = gen_envelope("live-nested");
        let live_entries = prepare_fastly_config_entries(&nested_root, &live_nested).expect("exp");
        let (_, live_pointer) = live_entries.last().expect("pointer").clone();

        // An OLD orphan generation under the SAME nested root (a week old).
        let old_nested = gen_envelope("old-nested-orphan");
        let old_entries = prepare_fastly_config_entries(&nested_root, &old_nested).expect("exp");
        let old_chunks: Vec<String> = old_entries[..old_entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // The nested root holds its live pointer; its live chunks are 30s old.
        listing.push((nested_root.clone(), stamp_secs_ago(30), live_pointer));
        for (key, value) in &live_entries[..live_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp_secs_ago(30), value.clone()));
        }
        // The old orphan chunks are a week old.
        for (key, value) in &old_entries[..old_entries.len().saturating_sub(1)] {
            listing.push((key.clone(), stamp_secs_ago(604_800), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // A generous 1-day window: the orphan's OWN chunks are older, but the
        // nested root's live config is only 30s old, so its orphan is retained.
        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &old_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a nested orphan under a recently-changed nested root must be retained: `{key}`; \
                 log:\n{log}"
            );
        }
    }

    /// A generation is aged by its YOUNGEST member, so a generation with one
    /// recent chunk is retained whole even if its other chunks are ancient.
    #[cfg(unix)]
    #[test]
    fn gc_ages_a_generation_by_its_youngest_member() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // `app_config` live is direct, so there is no live-config age signal —
        // aging falls to the generation's own chunks.
        let live_direct = gen_envelope_padded("live-direct", 100);
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        assert!(dead_chunks.len() >= 2, "need a multi-chunk generation");

        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            live_direct,
        )];
        // The doomed generation: chunk 0 written 30s ago (YOUNG), the rest a week
        // ago. Its youngest-member age (30s) is under the 1-day window.
        let dead_parts = chunked_parts(TEST_CONFIG_ID, &dead).0;
        for (idx, (key, value)) in dead_parts.iter().enumerate() {
            let age = if idx == 0 { 30 } else { 604_800 };
            listing.push((key.clone(), stamp_secs_ago(age), value.clone()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &dead_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a generation with a recent member must be retained WHOLE (aged by its youngest): \
                 `{key}`; log:\n{log}"
            );
        }
    }

    /// A delete failure in one generation must not stop an INDEPENDENT
    /// generation's deletes.
    #[cfg(unix)]
    #[test]
    fn gc_failure_in_one_generation_does_not_stop_another() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead_a = gen_envelope("dead-a");
        let dead_b = gen_envelope("dead-b");
        let a_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead_a);
        let b_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead_b);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead_a, 604_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead_b, 604_800));

        // Generation A's first delete fails.
        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&a_chunks[0]),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete is a failure");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        // Generation B must still have been reclaimed despite A's failure.
        for key in &b_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "an independent generation must still be reclaimed after another one fails: \
                 `{key}`; err: {err}; log:\n{log}"
            );
        }
    }

    /// key shape is not authoritative for ROOTS either.
    ///
    /// A valid pointer stored at a chunk-SHAPED key (`shadow.__edgezero_chunks.
    /// <sha>.0`) is skipped by the live-set scan, which excludes chunk-shaped
    /// keys up front. The runtime resolver follows any pointer it is given, so
    /// that pointer's references ARE live -- but GC never sees them, calls the
    /// generation orphaned, and deletes it.
    #[cfg(unix)]
    #[test]
    fn pointer_at_chunk_shaped_key_keeps_its_references_live() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // `app_config`'s CURRENT config is small enough to store directly, so
        // its own root references no chunks at all.
        let live_direct = gen_envelope_padded("live-direct", 100);
        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            live_direct,
        )];

        // An older chunked generation of `app_config` still exists...
        let referenced = gen_envelope("still-referenced");
        let referenced_chunks = chunk_keys_of(TEST_CONFIG_ID, &referenced);
        listing.extend(listed_generation(TEST_CONFIG_ID, &referenced, 604_800));

        // ...and a pointer at a CHUNK-SHAPED key references it. The resolver
        // would happily follow this, so those chunks are LIVE.
        let (_, referenced_pointer) = chunked_parts(TEST_CONFIG_ID, &referenced);
        let shadow_key = format!("shadow{CHUNK_KEY_INFIX}{}.0", "d".repeat(64));
        listing.push((shadow_key, stamp_secs_ago(604_800), referenced_pointer));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // The RESULT does not matter here (it may Err after the fix if the
        // shadow pointer's own chunks are incomplete); the invariant is purely
        // that no LIVE-referenced chunk is deleted, which the oplog proves.
        drop(run_gc(dir.path(), 86_400, false));
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &referenced_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a chunk a live pointer references must never be deleted, whatever the KEY of \
                 the entry holding that pointer looks like: `{key}`; log:\n{log}"
            );
        }
    }

    /// a FOREIGN writer needs NO preimage to satisfy a
    /// content-address. Pick envelope E, compute H = sha256(E), split E however
    /// you like, store the parts as `<root>.__edgezero_chunks.H.0` / `.1`. Under
    /// hash-only checking that group "proved" itself and was deleted.
    ///
    /// The round-trip closes it: the writer, given those same bytes, must emit
    /// exactly these keys and values. A split at boundaries we would never
    /// choose is not our output, so it is left alone.
    #[cfg(unix)]
    #[test]
    fn gc_never_reclaims_a_foreign_content_addressed_group() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        // A foreign writer's data: a valid envelope, content-addressed under our
        // reserved namespace, but split at ITS OWN boundary (not our 7 000-byte
        // UTF-8-safe one). Everything hashes correctly -- no preimage needed.
        let foreign = gen_envelope_padded("foreign-tool", 20_000);
        let generation = sha256_hex(foreign.as_bytes());
        let (head, tail) = foreign.split_at(1_234);
        let foreign_keys = [
            format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{generation}.0"),
            format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{generation}.1"),
        ];
        listing.push((
            foreign_keys[0].clone(),
            stamp_secs_ago(31_536_000),
            head.to_owned(),
        ));
        listing.push((
            foreign_keys[1].clone(),
            stamp_secs_ago(31_536_000),
            tail.to_owned(),
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &foreign_keys {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a group this writer would never have produced must not be reclaimed, however \
                 well it hashes: `{key}`; log:\n{log}"
            );
        }
    }

    /// an entry can be chunk-SHAPED without being a chunk
    /// -- a store may predate this feature or be shared, and push-time
    /// reserved-key rejection cannot protect what already exists. Deleting one
    /// would destroy live config.
    ///
    /// proof is CONTENT, not shape. A candidate generation is ours only
    /// if it reassembles to the content-address its own keys name. Unprovable
    /// entries are left UNTOUCHED and reported -- not fatal, because one foreign
    /// entry must not block reclaiming the rest of the store forever.
    #[cfg(unix)]
    #[test]
    fn gc_leaves_unprovable_chunk_shaped_entries_untouched() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        // A real orphan generation: provable, old -> must still be reclaimed.
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        // Pre-existing entries at chunk-shaped keys that we did NOT write: one
        // holding somebody's real config envelope, one holding plain text.
        // Both are old enough to look "eligible" on age alone.
        let envelope_squatter = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "b".repeat(64));
        let text_squatter = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{}.0", "c".repeat(64));
        listing.push((
            envelope_squatter.clone(),
            stamp_secs_ago(31_536_000),
            gen_envelope("someones-real-config"),
        ));
        listing.push((
            text_squatter.clone(),
            stamp_secs_ago(31_536_000),
            "just some plain text".to_owned(),
        ));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();

        for key in [&envelope_squatter, &text_squatter] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "an entry we cannot prove we wrote must never be deleted: `{key}`; log:\n{log}"
            );
        }
        // Left untouched must not mean silently ignored. The wording must not
        // over-claim either: these two entries fail for DIFFERENT reasons (a
        // wrong content-address vs a count this writer never emits), so the
        // summary says "not byte-identical to what this writer would produce"
        // rather than naming one specific check.
        assert!(
            out.iter()
                .any(|line| line.contains("not byte-identical to what this writer would produce")),
            "the summary must report what it declined to judge; out: {out:?}"
        );
        // ...and a genuine orphan generation is still reclaimed, so one foreign
        // entry does not block the store.
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "a provable orphan generation must still be reclaimed: `{key}`; log:\n{log}"
            );
        }
    }

    /// a key is unique in a config store, so duplicate rows
    /// mean the listing is not one consistent view. Left alone, last-row-wins on
    /// `created_at` could age a recent key into eligibility.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_duplicate_listing_keys() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let mut orphans = listed_generation(TEST_CONFIG_ID, &dead, 30);
        // The same key twice, with conflicting ages: young (real) then ancient.
        let (dup_key, _, dup_value) = orphans[0].clone();
        orphans.push((dup_key.clone(), stamp_secs_ago(31_536_000), dup_value));
        listing.extend(orphans);

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("more than once"),
            "expected a refusal naming the duplicate key, got: {err}"
        );
        assert!(
            !oplog_has(&oplog, &format!("delete {dup_key}")),
            "a duplicated row must not let a recent key be aged into eligibility"
        );
    }

    /// `gc_config_entries` is a public trait method, so the
    /// zero-window rule must live at the DESTRUCTIVE boundary, not only in the
    /// CLI that usually calls it. Rejected before any `fastly` invocation.
    #[cfg(unix)]
    #[test]
    fn gc_adapter_boundary_rejects_a_zero_window() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // Straight at the adapter, bypassing the CLI's own gate.
        let err = run_gc(dir.path(), 0, false).expect_err("a destructive zero window must fail");
        assert!(
            err.contains("non-zero `--older-than`"),
            "expected the boundary itself to reject zero, got: {err}"
        );
        assert!(
            !fs::read_to_string(&oplog)
                .unwrap_or_default()
                .contains("delete "),
            "nothing may be deleted under a zero window"
        );
        // A DRY-RUN at zero is still allowed: it previews and deletes nothing.
        run_gc(dir.path(), 0, true).expect("a dry-run may preview at zero");
    }

    /// a root whose value is TRUNCATED/unparseable must fail
    /// closed. It is pointer-shaped garbage -- we cannot tell what it references,
    /// so its (live!) chunks must not be judged orphaned. Regression guard: the
    /// push-path helper returns `Ok([])` for a non-pointer value, which on THIS
    /// path would read as "references nothing" and reclaim the whole store.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_truncated_root_pointer() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let (_, pointer) = chunked_parts(TEST_CONFIG_ID, &live);
        // A write that landed half-way: a valid PREFIX of the real pointer that
        // is no longer valid JSON. (Chars, not a byte slice -- never split a
        // codepoint.)
        let truncated: String = pointer.chars().take(40).collect();
        assert!(
            serde_json::from_str::<serde_json::Value>(&truncated).is_err(),
            "fixture must be unparseable to exercise the classifier: {truncated}"
        );

        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            truncated,
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("refusing to reclaim"),
            "expected a fail-closed refusal, got: {err}"
        );
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "nothing may be deleted when a root is unclassifiable: `{key}`"
            );
        }
    }

    /// an ENVELOPED listing (`{"items":[...]}`) may carry
    /// pagination we do not follow. A page that omitted a root would make that
    /// root's live chunks look orphaned -- and the completeness guard cannot see
    /// a root that isn't there. Refuse the shape outright.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_enveloped_listing() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 999_999)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));
        let enveloped = format!(
            r#"{{"items":{},"next_cursor":"abc"}}"#,
            entry_list_json(&listing)
        );

        let fake = fake_fastly_gc_raw_list(TEST_CONFIG_ID, &enveloped, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("bare array") && err.contains("Nothing was deleted"),
            "expected a refusal naming the unsupported listing shape, got: {err}"
        );
        assert!(
            !fs::read_to_string(&oplog)
                .unwrap_or_default()
                .contains("delete "),
            "an unsupported listing shape must delete nothing"
        );
    }

    /// a root with an EMPTY value is as dangerous as a
    /// missing one -- it would classify as "references nothing" and orphan its
    /// live chunks. The listing parser rejects it before any reasoning.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_empty_root_value() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let live_chunks = chunk_keys_of(TEST_CONFIG_ID, &live);
        let mut listing = vec![(
            TEST_CONFIG_ID.to_owned(),
            stamp_secs_ago(999_999),
            String::new(),
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 999_999));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 1, false).expect_err("must fail closed");
        assert!(
            err.contains("empty `item_value`"),
            "expected a refusal naming the empty field, got: {err}"
        );
        for key in &live_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "nothing may be deleted on an unreadable listing: `{key}`"
            );
        }
    }

    /// the orphan's OWN age is mandatory -- an old root does
    /// not license deleting a chunk written seconds ago (e.g. by a concurrent
    /// push that has not committed its pointer yet). Both ages must clear the
    /// window; the more restrictive wins.
    #[cfg(unix)]
    #[test]
    fn gc_retains_young_orphan_under_long_stable_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let fresh = gen_envelope("fresh");
        let fresh_chunks = chunk_keys_of(TEST_CONFIG_ID, &fresh);

        // The root's live config has been stable for a year -- so the live-config
        // clock alone would happily reclaim. But these chunks were written 10s
        // ago and no pointer names them yet.
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 31_536_000)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 31_536_000));
        listing.extend(listed_generation(TEST_CONFIG_ID, &fresh, 10));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &fresh_chunks {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a chunk written 10s ago must be retained under a 1-day window regardless of \
                 how stable its root is: `{key}`; log:\n{log}"
            );
        }
    }

    /// GC and the RUNTIME must agree on what a readable pointer is. A pointer
    /// whose chunks reassemble to the correct bytes along boundaries this writer
    /// would never choose is REJECTED by the runtime resolver, so GC must not
    /// silently report it as a healthy root: the guest cannot read it, and its
    /// generation can never satisfy `prove_generation`, so it is permanently
    /// unreclaimable. GC still keeps it (fail-closed) but must SAY so.
    #[cfg(unix)]
    #[test]
    fn gc_warns_that_a_non_writer_split_root_is_not_runtime_readable() {
        use crate::chunked_config::CHUNK_PAYLOAD_TARGET;
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // Just over the entry limit => a full chunk plus a short remainder with
        // room to absorb the shifted bytes.
        let envelope = gen_envelope("shifted");
        let sha = sha256_hex(envelope.as_bytes());
        // Re-split 2 bytes early: still within every metadata bound the pointer
        // validator checks, but NOT where this writer splits.
        let cut = CHUNK_PAYLOAD_TARGET.saturating_sub(2);
        let head = envelope.get(..cut).expect("ascii boundary");
        let tail = envelope.get(cut..).expect("ascii boundary");
        let key0 = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{sha}.0");
        let key1 = format!("{TEST_CONFIG_ID}{CHUNK_KEY_INFIX}{sha}.1");
        let pointer_json = serde_json::json!({
            "chunks": [
                {"key": key0, "len": head.len(), "sha256": sha256_hex(head.as_bytes())},
                {"key": key1, "len": tail.len(), "sha256": sha256_hex(tail.as_bytes())},
            ],
            "data_sha256": "",
            "edgezero_kind": "fastly_config_chunks",
            "envelope_len": envelope.len(),
            "envelope_sha256": sha,
            "version": 1_u8,
        })
        .to_string();

        let stamp = stamp_secs_ago(604_800);
        let listing = vec![
            (TEST_CONFIG_ID.to_owned(), stamp.clone(), pointer_json),
            (key0.clone(), stamp.clone(), head.to_owned()),
            (key1.clone(), stamp, tail.to_owned()),
        ];
        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, false).expect("gc must not abort on such a root");
        let rendered = out.join("\n");
        assert!(
            rendered.contains("NOT runtime-readable"),
            "GC must warn that this root is unreadable rather than call it healthy: {rendered}"
        );
        // This root is PROTECTED (kept) but not runtime-live, so the report must
        // list it as RETAINED and must NOT label it (or the store) "live".
        assert!(
            rendered.contains(&format!("keeping `{TEST_CONFIG_ID}`"))
                && rendered.contains("retained root(s)"),
            "an unreadable-but-protected root must be reported as retained: {rendered}"
        );
        assert!(
            !rendered.contains("live root") && !rendered.contains("live chunk"),
            "an unreadable root's chunks are protected/referenced, never labeled live: {rendered}"
        );
        // Fail-closed: nothing is deleted, including its chunks.
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in [&key0, &key1] {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "must not delete a chunk of an unreadable root: `{key}`; log:\n{log}"
            );
        }
    }

    #[test]
    fn kept_roots_report_wording_counts_and_empty_store() {
        // Empty: a single, unambiguous "nothing retained" line and no root list.
        let mut empty = Vec::new();
        append_kept_roots_report(&mut empty, &[], 0);
        assert_eq!(empty, vec!["keeping 0 retained root(s)".to_owned()]);

        // Non-empty: a heading naming the RETAINED-root count and the
        // REFERENCED-chunk count, then one line per root by key.
        let mut out = Vec::new();
        append_kept_roots_report(
            &mut out,
            &["app_config".to_owned(), "app_config_staging".to_owned()],
            5,
        );
        assert!(
            out[0].contains("keeping 2 retained root(s)")
                && out[0].contains("5 referenced chunk(s)"),
            "heading names the retained-root and referenced-chunk counts: {out:?}"
        );
        assert!(out.iter().any(|line| line == "  keeping `app_config`"));
        assert!(
            out.iter()
                .any(|line| line == "  keeping `app_config_staging`")
        );
        // Never the misleading "live" label -- a retained root may not be
        // runtime-live, and its chunks are protected/referenced, not live.
        assert!(
            !out.iter()
                .any(|line| line.contains("live root") || line.contains("live chunk")),
            "must not label retained roots/chunks as live: {out:?}"
        );
    }

    /// A legitimate FOREIGN sibling (the documented `greeting = "hello"`) must
    /// NOT block store-wide GC. The runtime returns such a value verbatim, so GC
    /// protects it as a zero-reference root and still reclaims an unrelated dead
    /// generation, rather than aborting the whole pass.
    #[cfg(unix)]
    #[test]
    fn gc_reclaims_despite_a_foreign_sibling_value() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        let mut listing = vec![
            listed_root(TEST_CONFIG_ID, &live, 172_800),
            // A plain, non-envelope, non-pointer sibling entry.
            (
                "greeting".to_owned(),
                stamp_secs_ago(172_800),
                "hello".to_owned(),
            ),
        ];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("a foreign sibling must not abort GC");
        for key in &dead_chunks {
            assert!(
                oplog_has(&oplog, &format!("delete {key}")),
                "the dead generation must still be reclaimed: `{key}`"
            );
        }
        assert!(
            !oplog_has(&oplog, "delete greeting"),
            "the foreign sibling must never be deleted"
        );
    }

    /// A DIRECT envelope from a NEWER writer at an ordinary key classifies as
    /// `Foreign` (no `edgezero_kind`), so without the future-format guard GC would
    /// wave it through as a zero-reference root and reclaim an otherwise-dead
    /// generation -- yet the newer format may reference those chunks under a
    /// scheme this build cannot read. GC must FAIL CLOSED and delete nothing.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_future_direct_envelope_at_an_ordinary_key() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let dead = gen_envelope("dead");

        // Envelope-shaped, no discriminator, VERSION 2 -> a newer direct envelope.
        let future = r#"{"data":{"x":1},"sha256":"0000000000000000000000000000000000000000000000000000000000000000","generated_at":"2026-01-01T00:00:00Z","version":2}"#;
        let mut listing = vec![(
            "app_config".to_owned(),
            stamp_secs_ago(172_800),
            future.to_owned(),
        )];
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let result = run_gc(dir.path(), 86_400, false);
        assert!(
            result.is_err(),
            "a future direct envelope must abort GC (fail closed)"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when GC fails closed; log:\n{log}"
        );
    }

    /// A valid v1 pointer whose chunks reassemble to a NEWER inner format. GC can
    /// validate the outer pointer and reassemble the bytes, but the reassembled
    /// value may reference generations this build cannot see, so trusting only the
    /// outer chunks as the live set could delete live data. GC must fail closed --
    /// `BlobEnvelope` deserialize alone would silently ignore the newer format.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_a_future_inner_generation() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        // A v1 envelope, chunked, then its inner `version` bumped to 2. The v1
        // pointer's content-address still matches the reassembled (v2) bytes.
        let v1 = gen_envelope("live");
        let mut v2_value: serde_json::Value = serde_json::from_str(&v1).expect("parse");
        v2_value["version"] = serde_json::json!(2_u32);
        let v2 = v2_value.to_string();

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &v2, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &v2, 172_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let result = run_gc(dir.path(), 86_400, false);
        assert!(
            result
                .as_ref()
                .is_err_and(|err| err.contains("newer format")),
            "a future inner generation must abort GC with a newer-format error: {result:?}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when GC fails closed; log:\n{log}"
        );
    }

    /// A dry-run lists exactly what it would delete, and deletes nothing.
    #[cfg(unix)]
    #[test]
    fn gc_dry_run_lists_but_deletes_nothing() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let out = run_gc(dir.path(), 86_400, true).expect("dry-run succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "a dry-run must not delete; log:\n{log}"
        );
        let rendered = out.join("\n");
        assert!(
            rendered.contains("would delete"),
            "lists intent: {rendered}"
        );
        for key in &dead_chunks {
            assert!(rendered.contains(key.as_str()), "names `{key}`: {rendered}");
        }
        // It must also report what it is KEEPING: the live root, by key.
        assert!(
            rendered.contains(&format!("keeping `{TEST_CONFIG_ID}`")),
            "must name the retained live root: {rendered}"
        );
    }

    /// An unreadable `created_at` on a DELETE path fails CLOSED.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_unreadable_timestamp() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 3_600)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 3_600));
        // An orphan whose timestamp is garbage.
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        for key in dead_chunks {
            listing.push((key, "not-a-timestamp".to_owned(), "X".to_owned()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("unreadable") && err.contains("nothing was deleted"),
            "must refuse to reclaim: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when the state is unreadable; log:\n{log}"
        );
    }

    /// A root whose pointer cannot be classified fails CLOSED — we cannot know
    /// what it references, so nothing may be deleted.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_unclassifiable_root() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let dead = gen_envelope("dead");
        // Root value is pointer-kind but invalid.
        let bad = r#"{"edgezero_kind":"fastly_config_chunks","version":2}"#.to_owned();
        let mut listing = vec![(TEST_CONFIG_ID.to_owned(), stamp_secs_ago(3_600), bad)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("could not classify root") && err.contains("nothing was deleted"),
            "must refuse to reclaim: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted when a root is unclassifiable; log:\n{log}"
        );
    }

    /// A listing entry missing a required field fails CLOSED — a defaulted/empty
    /// field could make a real root look like it references nothing, deleting
    /// live chunks.
    #[cfg(unix)]
    #[test]
    fn gc_fails_closed_on_malformed_listing_entry() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        let good = entry_list_json(&listing);
        // Inject an entry with NO item_value (drop that field entirely).
        let mut array: serde_json::Value = serde_json::from_str(&good).unwrap();
        array.as_array_mut().unwrap().push(serde_json::json!({
            "item_key": "some.__edgezero_chunks.deadbeef.0",
            "created_at": stamp_secs_ago(1000),
        }));
        // Serve that hand-built listing.
        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        fs::write(
            fake.path().join("entries.json"),
            serde_json::to_string(&array).unwrap(),
        )
        .expect("overwrite entries");
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("missing a string") && err.contains("item_value"),
            "must name the missing field: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        assert!(
            !log.lines().any(|line| line.starts_with("delete ")),
            "nothing may be deleted on a malformed listing; log:\n{log}"
        );
    }

    /// A failed delete is a non-zero exit that names the failed key(s), so
    /// automation can detect partial failure.
    #[cfg(unix)]
    #[test]
    fn gc_delete_failure_is_non_zero_exit() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let dead_chunks = chunk_keys_of(TEST_CONFIG_ID, &dead);
        let fail_key = dead_chunks.first().expect("a chunk").clone();

        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(
            TEST_CONFIG_ID,
            &[],
            &listing,
            Some(&fail_key),
            false,
            &oplog,
        );
        let _path = PathPrepend::new(fake.path());

        let err = run_gc(dir.path(), 86_400, false).expect_err("a failed delete must be non-zero");
        assert!(
            err.contains("deletes FAILED") && err.contains(&fail_key),
            "error names the failed key: {err}"
        );
    }

    /// Every reclamation delete passes `--key` + `--auto-yes` and NEVER `--all`.
    #[cfg(unix)]
    #[test]
    fn gc_delete_uses_key_and_auto_yes_never_all() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let dead = gen_envelope("dead");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        listing.extend(listed_generation(TEST_CONFIG_ID, &dead, 604_800));

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        run_gc(dir.path(), 86_400, false).expect("gc succeeds");
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        let argv_lines: Vec<&str> = log
            .lines()
            .filter(|line| line.starts_with("delete-argv "))
            .collect();
        assert!(!argv_lines.is_empty(), "a delete happened: {log}");
        for line in argv_lines {
            assert!(
                line.contains("--auto-yes"),
                "delete passes --auto-yes: {line}"
            );
            assert!(line.contains("--key="), "delete targets a --key: {line}");
            assert!(
                !line.contains("--all"),
                "delete must NEVER pass --all: {line}"
            );
        }
    }

    /// A non-canonical chunk-like key (short/uppercase SHA, leading-zero index)
    /// is NOT a delete candidate — the destructive validator is canonical-only.
    #[cfg(unix)]
    #[test]
    fn gc_never_deletes_non_canonical_keys() {
        let _lock = path_mutation_guard().lock().expect("guard");
        let dir = tempdir().expect("tempdir");
        let oplog = dir.path().join("ops.log");

        let live = gen_envelope("live");
        let mut listing = vec![listed_root(TEST_CONFIG_ID, &live, 172_800)];
        listing.extend(listed_generation(TEST_CONFIG_ID, &live, 172_800));
        // Foreign-shaped keys under the reserved infix but not canonical.
        let noncanonical = [
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.abc123.0"), // short sha
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.{}.00", "a".repeat(64)), // leading-zero idx
            format!("{TEST_CONFIG_ID}.__edgezero_chunks.{}.0", "A".repeat(64)), // uppercase
        ];
        for key in &noncanonical {
            listing.push((key.clone(), stamp_secs_ago(604_800), "X".to_owned()));
        }

        let fake = fake_fastly_gc(TEST_CONFIG_ID, &[], &listing, None, false, &oplog);
        let _path = PathPrepend::new(fake.path());

        // A key that is NOT canonical is not one we wrote, so it is not a
        // reclamation candidate. It sits in our reserved namespace, though, so
        // it is also not an ordinary root: we cannot say what it is. Since the
        // GC classifier fails closed on any root it cannot classify, the run
        // aborts and names it -- which satisfies this test's invariant (a
        // non-canonical key is never deleted) the strict way.
        let err = run_gc(dir.path(), 86_400, false).expect_err("must fail closed");
        assert!(
            err.contains("refusing to reclaim"),
            "expected a fail-closed refusal, got: {err}"
        );
        let log = fs::read_to_string(&oplog).unwrap_or_default();
        for key in &noncanonical {
            assert!(
                !oplog_has(&oplog, &format!("delete {key}")),
                "a non-canonical key must never be deleted: `{key}`; log:\n{log}"
            );
        }
        assert!(
            !log.contains("delete "),
            "a fail-closed run deletes nothing at all; log:\n{log}"
        );
    }
}
