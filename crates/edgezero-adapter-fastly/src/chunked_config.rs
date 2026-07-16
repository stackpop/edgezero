//! Fastly chunk-pointer storage for oversized app-config envelopes.
//!
//! Fastly Config Store enforces an 8 000-character limit per entry value.
//! When a logical `(key, envelope_json)` pair exceeds that limit, this
//! module splits the envelope into content-addressed chunk entries plus a
//! root pointer entry that is written LAST.
//!
//! The pointer JSON shape (spec 9.2):
//! ```json
//! {
//!   "edgezero_kind": "fastly_config_chunks",
//!   "version": 1,
//!   "envelope_sha256": "<sha256-of-full-envelope-json-bytes>",
//!   "envelope_len": 12345,
//!   "data_sha256": "<BlobEnvelope.sha256 -- diagnostic only>",
//!   "chunks": [
//!     { "key": "app_config.__edgezero_chunks.<sha>.0", "sha256": "<sha>", "len": 7000 }
//!   ]
//! }
//! ```

use sha2::{Digest as _, Sha256};

/// Per-entry value limit enforced by Fastly Config Store. Used by the
/// CLI writer to gate direct-vs-chunked storage; the runtime resolver
/// reads chunk lengths from the pointer struct, not this constant.
#[cfg(any(feature = "cli", test))]
pub(crate) const FASTLY_CONFIG_ENTRY_LIMIT: usize = 8_000;
/// Target payload size per chunk (kept under the entry limit to leave
/// room for the key and any protocol overhead). CLI writer only.
#[cfg(any(feature = "cli", test))]
pub(crate) const CHUNK_PAYLOAD_TARGET: usize = 7_000;
/// Infix inserted between the root key and the content-address in a
/// chunk key: `<root>.__edgezero_chunks.<sha256>.<index>`. CLI writer
/// only; the resolver reads chunk keys from the pointer struct.
#[cfg(any(feature = "cli", test))]
pub(crate) const CHUNK_KEY_INFIX: &str = ".__edgezero_chunks.";
/// `edgezero_kind` discriminant stored in the pointer JSON. Used by
/// BOTH the writer (when serialising the pointer) AND the resolver
/// (when validating the parsed pointer) -- stays unconditional.
pub(crate) const POINTER_KIND: &str = "fastly_config_chunks";

// ---------------------------------------------------------------------------
// Private pointer schema
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, serde::Serialize)]
struct FastlyChunkPointer {
    chunks: Vec<FastlyChunkRef>,
    data_sha256: String,
    edgezero_kind: String,
    envelope_len: usize,
    envelope_sha256: String,
    version: u8,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct FastlyChunkRef {
    key: String,
    len: usize,
    sha256: String,
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Compute the lowercase-hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Prepare the physical Config Store entries for a single logical
/// `(root_key, envelope_json)` pair.
///
/// - If `envelope_json.len() <= 8_000`: returns one `(root_key, envelope_json)` tuple unchanged.
/// - Otherwise: splits into UTF-8-safe 7 000-byte chunks, returns chunk
///   entries first and the root pointer entry last.
///
/// The helper does **not** write to any store; callers own the I/O.
///
/// # Errors
///
/// Returns an error string if the pointer JSON itself would exceed 8 000
/// characters (extremely unlikely in practice; recommends restructuring).
#[cfg(any(feature = "cli", test))]
pub(crate) fn prepare_fastly_config_entries(
    root_key: &str,
    envelope_json: &str,
) -> Result<Vec<(String, String)>, String> {
    if envelope_json.len() <= FASTLY_CONFIG_ENTRY_LIMIT {
        return Ok(vec![(root_key.to_owned(), envelope_json.to_owned())]);
    }

    let env_bytes = envelope_json.as_bytes();
    let envelope_sha256 = sha256_hex(env_bytes);

    // Extract data_sha256 from the envelope JSON for diagnostic purposes.
    let data_sha256 = serde_json::from_str::<serde_json::Value>(envelope_json)
        .ok()
        .and_then(|val| {
            val.get("sha256")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();

    // Split env_bytes into UTF-8-safe chunks of at most CHUNK_PAYLOAD_TARGET bytes.
    let mut chunks: Vec<(String, String, usize)> = Vec::new(); // (key, value, len)
    let mut start = 0_usize;
    let mut idx = 0_usize;
    while start < env_bytes.len() {
        // Walk forward by CHUNK_PAYLOAD_TARGET bytes, then retreat to the
        // last codepoint boundary so we never split a multi-byte codepoint.
        let end_raw = (start.saturating_add(CHUNK_PAYLOAD_TARGET)).min(env_bytes.len());
        // Find the largest valid UTF-8 boundary <= end_raw.
        let end = find_utf8_boundary(envelope_json, start, end_raw);
        let chunk_bytes = env_bytes.get(start..end).ok_or_else(|| {
            format!("chunk boundary [{start}..{end}] out of range for `{root_key}`")
        })?;
        let chunk_str = str::from_utf8(chunk_bytes).map_err(|err| {
            format!("chunk [{start}..{end}] for `{root_key}` is not valid UTF-8: {err}")
        })?;
        let chunk_key = format!("{root_key}{CHUNK_KEY_INFIX}{envelope_sha256}.{idx}");
        chunks.push((chunk_key, chunk_str.to_owned(), chunk_bytes.len()));
        start = end;
        idx = idx.saturating_add(1);
    }

    // Build chunk refs with per-chunk SHAs.
    let chunk_refs: Vec<FastlyChunkRef> = chunks
        .iter()
        .map(|(key, value, len)| FastlyChunkRef {
            key: key.clone(),
            sha256: sha256_hex(value.as_bytes()),
            len: *len,
        })
        .collect();

    let pointer = FastlyChunkPointer {
        chunks: chunk_refs,
        data_sha256,
        edgezero_kind: POINTER_KIND.to_owned(),
        envelope_len: env_bytes.len(),
        envelope_sha256,
        version: 1,
    };

    let pointer_json = serde_json::to_string(&pointer)
        .map_err(|err| format!("failed to serialise chunk pointer for `{root_key}`: {err}"))?;

    if pointer_json.len() > FASTLY_CONFIG_ENTRY_LIMIT {
        return Err(format!(
            "chunk pointer for `{root_key}` is {} characters, which exceeds the \
             Fastly Config Store 8 000-character entry limit (the config is too \
             large even for chunked storage). Restructure your typed app-config \
             into multiple types split across [stores.config] ids, or use an \
             adapter with a larger single-value limit.",
            pointer_json.len()
        ));
    }

    // Assemble: chunks first, root pointer last.
    let mut entries: Vec<(String, String)> = Vec::with_capacity(chunks.len().saturating_add(1));
    for (key, value, _) in chunks {
        entries.push((key, value));
    }
    entries.push((root_key.to_owned(), pointer_json));
    Ok(entries)
}

/// Find the largest byte offset `<= end_raw` that is a valid UTF-8
/// codepoint boundary within `src`.  `start` is used as a hint so we
/// don't scan the entire string from zero each time. CLI writer only.
#[cfg(any(feature = "cli", test))]
fn find_utf8_boundary(src: &str, start: usize, end_raw: usize) -> usize {
    if end_raw >= src.len() {
        return src.len();
    }
    // Walk backwards from end_raw until we land on a valid char boundary.
    let mut boundary = end_raw;
    while boundary > start && !src.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    boundary
}

/// Resolve a logical `(root_key, root_value)` pair from the store,
/// handling both direct `BlobEnvelope` values and chunk-pointer values.
///
/// Algorithm:
/// 1. Try to parse `root_value` as a `BlobEnvelope`. If it parses,
///    verify it. A valid direct envelope is returned unchanged; a parsed
///    envelope whose SHA/version verification fails is an error.
/// 2. Only when `root_value` does not parse as a `BlobEnvelope`, try to
///    parse it as a `FastlyChunkPointer`. Unknown `edgezero_kind` / version
///    is an error.
/// 3. Fetch each chunk via the `fetch` callback.
/// 4. Verify each chunk's length and SHA against the pointer.
/// 5. Concatenate in pointer order, verify `envelope_len` + `envelope_sha256`,
///    then return the reconstructed envelope JSON string.
///
/// # Errors
///
/// Returns a descriptive error string naming the root key or the failing
/// chunk key when any integrity check fails.
pub(crate) fn resolve_fastly_config_value<F>(
    root_key: &str,
    root_value: String,
    mut fetch: F,
) -> Result<String, String>
where
    F: FnMut(&str) -> Result<Option<String>, String>,
{
    use edgezero_core::blob_envelope::BlobEnvelope;

    // --- Path 1: direct BlobEnvelope ---
    if let Ok(envelope) = serde_json::from_str::<BlobEnvelope>(&root_value) {
        envelope.verify().map_err(|err| {
            format!("BlobEnvelope at key `{root_key}` failed integrity check: {err}")
        })?;
        return Ok(root_value);
    }

    // --- Path 2: chunk pointer ---
    let pointer: FastlyChunkPointer = serde_json::from_str(&root_value).map_err(|err| {
        format!(
            "value at key `{root_key}` is neither a valid BlobEnvelope nor a valid \
             chunk pointer: {err}"
        )
    })?;

    if pointer.edgezero_kind != POINTER_KIND {
        return Err(format!(
            "chunk pointer at `{root_key}` has unknown edgezero_kind \
             `{}`; expected `{POINTER_KIND}`",
            pointer.edgezero_kind
        ));
    }
    if pointer.version != 1 {
        return Err(format!(
            "chunk pointer at `{root_key}` has unsupported version \
             {}; expected 1",
            pointer.version
        ));
    }

    // Fetch, verify, and concatenate all chunks.
    let mut reconstructed = String::with_capacity(pointer.envelope_len);
    for chunk_ref in &pointer.chunks {
        let chunk_value = fetch(&chunk_ref.key)?.ok_or_else(|| {
            format!(
                "missing chunk `{}` referenced by pointer at `{root_key}`",
                chunk_ref.key
            )
        })?;

        // Verify length.
        if chunk_value.len() != chunk_ref.len {
            return Err(format!(
                "chunk `{}` (referenced by `{root_key}`) has length {} but pointer \
                 records {}",
                chunk_ref.key,
                chunk_value.len(),
                chunk_ref.len,
            ));
        }

        // Verify SHA.
        let actual_sha = sha256_hex(chunk_value.as_bytes());
        if actual_sha != chunk_ref.sha256 {
            return Err(format!(
                "chunk `{}` (referenced by `{root_key}`) SHA mismatch: \
                 expected `{}`, got `{actual_sha}`",
                chunk_ref.key, chunk_ref.sha256,
            ));
        }

        reconstructed.push_str(&chunk_value);
    }

    // Verify total envelope length.
    if reconstructed.len() != pointer.envelope_len {
        return Err(format!(
            "reconstructed envelope for `{root_key}` has length {} but pointer \
             records {}",
            reconstructed.len(),
            pointer.envelope_len,
        ));
    }

    // Verify total envelope SHA.
    let actual_env_sha = sha256_hex(reconstructed.as_bytes());
    if actual_env_sha != pointer.envelope_sha256 {
        return Err(format!(
            "reconstructed envelope for `{root_key}` SHA mismatch: \
             expected `{}`, got `{actual_env_sha}`",
            pointer.envelope_sha256,
        ));
    }

    Ok(reconstructed)
}

/// Validate a prior root value and return the chunk keys it referenced,
/// scoped to `root_key`'s own chunk namespace. Used only for chunk GC on
/// re-push (Stage 7 writeback).
///
/// Parse as `serde_json::Value` FIRST so a pointer-kind value with
/// missing/invalid fields still reaches the warning path instead of being
/// silently dropped by a failed struct deserialize.
///
/// Returns:
/// - `Ok(keys)`  -- value is a valid v1 chunk pointer; `keys` are its
///   `chunks[].key` entries (all confirmed to match
///   `"{root_key}{CHUNK_KEY_INFIX}"`).
/// - `Ok(vec![])` -- value is a direct `BlobEnvelope`, absent, or not
///   pointer-shaped at all (normal: first push, or was-direct). Silent.
/// - `Err(msg)`  -- value IS pointer-kind (`edgezero_kind == POINTER_KIND`)
///   but fails validation: malformed, unsupported `version`, or a
///   referenced key falls outside this root's chunk prefix. Callers log
///   `msg` as a warning and skip GC for this root (delete nothing).
#[cfg(any(feature = "cli", test))]
pub(crate) fn prior_chunk_keys(root_key: &str, raw: &str) -> Result<Vec<String>, String> {
    // 1. Parse loosely. Not-JSON, or not our pointer kind => silent.
    let value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(_) => return Ok(Vec::new()),
    };
    if value
        .get("edgezero_kind")
        .and_then(serde_json::Value::as_str)
        != Some(POINTER_KIND)
    {
        // Direct BlobEnvelope, unrelated JSON, or first push.
        return Ok(Vec::new());
    }

    // 2. It IS pointer-kind: from here every failure WARNS (Err), never silent.
    let pointer: FastlyChunkPointer = serde_json::from_value(value).map_err(|err| {
        format!("prior chunk pointer at `{root_key}` is malformed: {err}; skipping chunk GC")
    })?;
    if pointer.version != 1 {
        return Err(format!(
            "prior chunk pointer at `{root_key}` has unsupported version {}; skipping chunk GC",
            pointer.version
        ));
    }

    // 3. Every referenced key must be a CANONICAL chunk key of this root — the
    //    same validator that gates deletion. These keys are pruned locally and
    //    protected as "live" in the cloud, so a non-canonical or foreign key is
    //    a hard error, not a silently-kept key.
    let mut keys = Vec::with_capacity(pointer.chunks.len());
    for chunk_ref in pointer.chunks {
        if chunk_key_generation(root_key, &chunk_ref.key).is_none() {
            return Err(format!(
                "prior chunk pointer at `{root_key}` references a non-canonical chunk key `{}`; skipping chunk GC",
                chunk_ref.key
            ));
        }
        keys.push(chunk_ref.key);
    }
    Ok(keys)
}

/// Extract the content-address (envelope SHA) of the generation a chunk key
/// belongs to, for `root_key`. Returns `None` when `key` is not a well-formed
/// chunk key of that root.
///
/// This is how reclamation groups the store's ACTUAL keys into generations. It
/// validates the shape (`<root>.__edgezero_chunks.<hex-sha>.<index>`) rather
/// than trusting it: a hand-edited or foreign key never becomes a delete target.
#[cfg(any(feature = "cli", test))]
pub(crate) fn chunk_key_generation(root_key: &str, key: &str) -> Option<String> {
    if root_key.is_empty() {
        return None;
    }
    let prefix = format!("{root_key}{CHUNK_KEY_INFIX}");
    let rest = key.strip_prefix(&prefix)?;
    let (sha, index) = rest.rsplit_once('.')?;
    // Canonical shape ONLY — this gates a destructive delete, so it must match
    // exactly what `prepare_fastly_config_entries` emits and nothing else:
    // - a 64-char LOWERCASE hex SHA-256 (`format!("{:x}", Sha256::digest(..))`),
    // - a canonical decimal index (no leading zeros; `usize` `Display`).
    // Anything else (short/uppercase hash, `00`, `007`) is foreign or
    // hand-edited and must never become a delete candidate.
    if !is_canonical_sha256_hex(sha) || !is_canonical_index(index) {
        return None;
    }
    Some(sha.to_owned())
}

/// Exactly 64 lowercase hex characters.
#[cfg(any(feature = "cli", test))]
fn is_canonical_sha256_hex(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A canonical decimal index: all ASCII digits, and no leading zero unless the
/// value is exactly `0`.
#[cfg(any(feature = "cli", test))]
fn is_canonical_index(index: &str) -> bool {
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    index == "0" || !index.starts_with('0')
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // ---- helpers ----

    /// Build a valid `BlobEnvelope` JSON string of approximately `target_len`
    /// characters by padding the data payload.
    fn make_envelope_json(target_len: usize) -> String {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        // Build a minimal envelope first to measure overhead.
        let padding = "x".repeat(target_len.saturating_add(64));
        let data = json!({ "pad": padding });
        let raw =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
        // If it's too long shrink the pad; if too short grow it. Two iterations
        // are always sufficient because each character in `pad` is one byte.
        if raw.len() >= target_len {
            let overhead = raw.len().saturating_sub(padding.len());
            let adjusted_pad = "x".repeat(target_len.saturating_sub(overhead));
            let data2 = json!({ "pad": adjusted_pad });
            serde_json::to_string(&BlobEnvelope::new(data2, "2026-06-22T00:00:00Z".into())).unwrap()
        } else {
            raw
        }
    }

    // ---- prepare tests ----

    #[test]
    fn exactly_8000_chars_returns_one_direct_entry() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT);
        // Ensure the fixture actually hits the boundary.
        assert_eq!(
            envelope.len(),
            FASTLY_CONFIG_ENTRY_LIMIT,
            "fixture must be exactly 8000 chars"
        );
        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        assert_eq!(entries.len(), 1, "exactly 8000 chars => one direct entry");
        assert_eq!(entries[0].0, "app_config");
        assert_eq!(entries[0].1, envelope);
    }

    #[test]
    fn chars_8001_returns_chunks_plus_root_pointer_last() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        assert!(envelope.len() > FASTLY_CONFIG_ENTRY_LIMIT);

        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        assert!(entries.len() >= 2, "must have at least one chunk + pointer");

        // Last entry is the root key with a pointer JSON.
        let (last_key, last_val) = entries.last().unwrap();
        assert_eq!(last_key, "app_config", "root pointer must be LAST");

        let pointer: FastlyChunkPointer =
            serde_json::from_str(last_val).expect("last entry must be pointer JSON");
        assert_eq!(pointer.edgezero_kind, POINTER_KIND);
        assert_eq!(pointer.version, 1);
        // All chunk entries precede the root pointer.
        for (key, _) in &entries[..entries.len().saturating_sub(1)] {
            assert!(
                key.contains(CHUNK_KEY_INFIX),
                "non-final entries must be chunk keys: {key}"
            );
        }
    }

    #[test]
    fn chunk_splitting_preserves_multi_byte_utf8() {
        // Construct an envelope whose padding contains emoji (4 bytes each).
        // We intentionally size it so a naive byte-boundary split would land
        // mid-codepoint.
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        // crab emoji: 4 bytes each; 3000 crabs = 12 000 bytes of payload
        let emoji_block = "\u{1F980}".repeat(3_000);
        let data = json!({ "crabs": emoji_block });
        let envelope =
            serde_json::to_string(&BlobEnvelope::new(data, "2026-06-22T00:00:00Z".into())).unwrap();
        assert!(
            envelope.len() > FASTLY_CONFIG_ENTRY_LIMIT,
            "emoji envelope must be oversized"
        );

        let entries = prepare_fastly_config_entries("emoji_key", &envelope).unwrap();
        // Every non-pointer entry must be valid UTF-8 (it will be, but this
        // is the assertion that catches a bad boundary).
        for (key, value) in &entries[..entries.len().saturating_sub(1)] {
            assert!(
                str::from_utf8(value.as_bytes()).is_ok(),
                "chunk at `{key}` is not valid UTF-8"
            );
        }
        // Reconstruct manually and compare.
        let reconstructed: String = entries[..entries.len().saturating_sub(1)]
            .iter()
            .map(|(_, val)| val.as_str())
            .collect();
        assert_eq!(
            reconstructed, envelope,
            "concatenated chunks must equal original"
        );
    }

    #[test]
    fn pointer_too_large_returns_error_naming_root_key() {
        // Build an envelope large enough that the pointer JSON exceeds 8 000 chars.
        // With CHUNK_PAYLOAD_TARGET = 7 000 and key "app_config":
        //   each chunk key = ~10 + 1 + 64 + 1 + digits = ~78 chars
        //   each chunk_ref JSON  = ~170 chars
        //   pointer overhead     = ~250 chars
        //   need 250 + N * 170 > 8 000  =>  N >= 46 chunks
        //   envelope size needed = 46 * 7 000 = 322 000 bytes
        let long_envelope = "a".repeat(322_000);
        // This is not valid JSON but prepare_fastly_config_entries only
        // inspects envelope_json as bytes for SHA computation and length;
        // the data_sha256 extraction silently falls back to "".
        let key = "app_config";
        let result = prepare_fastly_config_entries(key, &long_envelope);
        match result {
            Err(msg) => {
                assert!(msg.contains(key), "error must name root key `{key}`: {msg}");
                assert!(
                    msg.contains("8 000") || msg.contains("8000"),
                    "error must mention the limit: {msg}"
                );
            }
            Ok(entries) => {
                // If the pointer still fits for this input, the fixture needs
                // to be enlarged. Verify it actually produced an oversized
                // pointer by checking its length.
                let (_, pointer_json) = entries.last().unwrap();
                assert!(
                    pointer_json.len() > FASTLY_CONFIG_ENTRY_LIMIT,
                    "pointer was only {} chars; need larger fixture to trigger error path",
                    pointer_json.len()
                );
            }
        }
    }

    // ---- resolve tests ----

    #[test]
    fn resolver_returns_direct_envelope_unchanged() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;

        let envelope = BlobEnvelope::new(json!({"hello": "world"}), "2026-06-22T00:00:00Z".into());
        let json_str = serde_json::to_string(&envelope).unwrap();

        // The closure must never be called for a direct envelope.
        let result = resolve_fastly_config_value("my_key", json_str.clone(), |_chunk_key| {
            Err("fetch must not be called for a direct envelope".to_owned())
        });
        assert_eq!(result.unwrap(), json_str);
    }

    #[test]
    fn resolver_reconstructs_chunked_envelope_via_fetch_callback() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();

        // Separate pointer (last) from chunks.
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();

        let result = resolve_fastly_config_value(&root_key, pointer_json, |chunk_key| {
            Ok(chunk_map.get(chunk_key).cloned())
        });

        assert_eq!(
            result.unwrap(),
            envelope,
            "reconstructed must equal original"
        );
    }

    #[test]
    fn resolver_errors_on_missing_chunk_naming_chunk_key() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();

        // Fetch callback returns None for every chunk key.
        let err = resolve_fastly_config_value(&root_key, pointer_json, |_| Ok(None))
            .expect_err("missing chunk must error");
        assert!(
            err.contains("missing chunk"),
            "error must say 'missing chunk': {err}"
        );
        assert!(err.contains("root"), "error must name the root key: {err}");
    }

    #[test]
    fn resolver_errors_on_chunk_hash_mismatch() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();
        let first_chunk_key = entries[0].0.clone();

        let err = resolve_fastly_config_value(&root_key, pointer_json, |chunk_key| {
            if chunk_key == first_chunk_key {
                // Return corrupted content (same length, different bytes).
                let original = chunk_map[chunk_key].clone();
                let corrupted: String = original.chars().map(|_| 'Z').collect();
                Ok(Some(corrupted))
            } else {
                Ok(chunk_map.get(chunk_key).cloned())
            }
        })
        .expect_err("corrupt chunk must error");
        assert!(
            err.contains("SHA mismatch") || err.contains("sha") || err.contains("mismatch"),
            "error must mention hash mismatch: {err}"
        );
        assert!(
            err.contains(&first_chunk_key),
            "error must name the failing chunk key: {err}"
        );
    }

    #[test]
    fn resolver_errors_on_chunk_length_mismatch() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();
        let first_chunk_key = entries[0].0.clone();

        let err = resolve_fastly_config_value(&root_key, pointer_json, |chunk_key| {
            if chunk_key == first_chunk_key {
                // Trim one character (not byte!) from the first chunk to
                // create a length mismatch without breaking UTF-8.
                let original = chunk_map[chunk_key].clone();
                let trimmed: String = original
                    .chars()
                    .take(original.chars().count().saturating_sub(1))
                    .collect();
                Ok(Some(trimmed))
            } else {
                Ok(chunk_map.get(chunk_key).cloned())
            }
        })
        .expect_err("length-mismatched chunk must error");
        assert!(
            err.contains("length") || err.contains("len"),
            "error must mention length: {err}"
        );
        assert!(
            err.contains(&first_chunk_key),
            "error must name the failing chunk key: {err}"
        );
    }

    #[test]
    fn resolver_errors_on_full_envelope_hash_mismatch() {
        // Build a pointer where envelope_sha256 is wrong but chunks verify
        // individually. Achieved by manually constructing a pointer with a
        // tampered envelope_sha256.
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();

        // Tamper with envelope_sha256 in the pointer JSON.
        let mut pointer: FastlyChunkPointer = serde_json::from_str(&pointer_json).unwrap();
        pointer.envelope_sha256 = "ff".repeat(32);
        let tampered_pointer_json = serde_json::to_string(&pointer).unwrap();

        let err = resolve_fastly_config_value(&root_key, tampered_pointer_json, |chunk_key| {
            Ok(chunk_map.get(chunk_key).cloned())
        })
        .expect_err("envelope hash mismatch must error");
        assert!(
            err.contains("SHA mismatch") || err.contains("mismatch"),
            "error must mention hash mismatch: {err}"
        );
        assert!(err.contains("root"), "error must name the root key: {err}");
    }

    #[test]
    fn resolver_errors_on_malformed_pointer() {
        // A value that is neither a valid BlobEnvelope nor a valid pointer.
        let err = resolve_fastly_config_value("my_key", "not json at all".to_owned(), |_| Ok(None))
            .expect_err("malformed pointer must error");
        assert!(
            err.contains("my_key"),
            "error must name the root key: {err}"
        );
    }

    #[test]
    fn resolver_errors_on_unknown_edgezero_kind() {
        let pointer = FastlyChunkPointer {
            chunks: Vec::new(),
            data_sha256: String::new(),
            edgezero_kind: "some_other_kind".to_owned(),
            envelope_len: 0,
            envelope_sha256: String::new(),
            version: 1,
        };
        let json_str = serde_json::to_string(&pointer).unwrap();
        let err = resolve_fastly_config_value("my_key", json_str, |_| Ok(None))
            .expect_err("unknown kind must error");
        assert!(
            err.contains("unknown edgezero_kind") || err.contains("edgezero_kind"),
            "error must mention edgezero_kind: {err}"
        );
        assert!(err.contains("my_key"), "error must name root key: {err}");
    }

    #[test]
    fn resolver_errors_on_pointer_version_not_1() {
        let pointer = FastlyChunkPointer {
            chunks: Vec::new(),
            data_sha256: String::new(),
            edgezero_kind: POINTER_KIND.to_owned(),
            envelope_len: 0,
            envelope_sha256: String::new(),
            version: 2,
        };
        let json_str = serde_json::to_string(&pointer).unwrap();
        let err = resolve_fastly_config_value("my_key", json_str, |_| Ok(None))
            .expect_err("unsupported version must error");
        assert!(
            err.contains("version") || err.contains("unsupported"),
            "error must mention version: {err}"
        );
        assert!(err.contains("my_key"), "error must name root key: {err}");
    }

    // ---- prior_chunk_keys tests ----

    #[test]
    fn prior_chunk_keys_returns_valid_v1_pointer_keys() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        let (_, pointer_json) = entries.last().unwrap();

        let keys = prior_chunk_keys("app_config", pointer_json).expect("valid pointer");

        let expected: Vec<String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn prior_chunk_keys_returns_empty_for_direct_envelope() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT);
        assert_eq!(
            prior_chunk_keys("app_config", &envelope).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn prior_chunk_keys_returns_empty_for_unrelated_json() {
        assert_eq!(
            prior_chunk_keys("app_config", r#"{"hello":"world"}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn prior_chunk_keys_returns_empty_for_wrong_kind() {
        let raw = r#"{"edgezero_kind":"other","version":1,"chunks":[],"data_sha256":"","envelope_len":0,"envelope_sha256":""}"#;
        assert_eq!(
            prior_chunk_keys("app_config", raw).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn prior_chunk_keys_rejects_unsupported_pointer_version() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        let (_, pointer_json) = entries.last().unwrap();
        let mut pointer: FastlyChunkPointer = serde_json::from_str(pointer_json).unwrap();
        pointer.version = 2;
        let raw = serde_json::to_string(&pointer).unwrap();

        let err = prior_chunk_keys("app_config", &raw).expect_err("version 2 should warn");
        assert!(err.contains("unsupported version"), "{err}");
    }

    #[test]
    fn prior_chunk_keys_rejects_foreign_chunk_prefix() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        let (_, pointer_json) = entries.last().unwrap();
        let mut pointer: FastlyChunkPointer = serde_json::from_str(pointer_json).unwrap();
        pointer.chunks[0].key =
            pointer.chunks[0]
                .key
                .replacen("app_config", "app_config_staging", 1);
        let raw = serde_json::to_string(&pointer).unwrap();

        let err = prior_chunk_keys("app_config", &raw).expect_err("foreign chunk should warn");
        assert!(err.contains("non-canonical"), "{err}");
    }

    // ---- chunk_key_generation (canonical only) ----

    #[test]
    fn chunk_key_generation_extracts_the_sha() {
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("app_config", &envelope).unwrap();
        let (chunk_key, _) = &entries[0];
        let sha = chunk_key_generation("app_config", chunk_key).expect("a chunk key");
        assert_eq!(sha.len(), 64, "64-char sha256 hex");
        assert!(chunk_key.contains(&sha));
    }

    #[test]
    fn chunk_key_generation_requires_canonical_shape() {
        let good = "a".repeat(64);
        let base = format!("app_config.__edgezero_chunks.{good}");
        // Canonical.
        assert!(chunk_key_generation("app_config", &format!("{base}.0")).is_some());
        assert!(chunk_key_generation("app_config", &format!("{base}.17")).is_some());

        // Different root / the root itself.
        assert!(
            chunk_key_generation("app_config", &format!("other.__edgezero_chunks.{good}.0"))
                .is_none()
        );
        assert!(chunk_key_generation("app_config", "app_config").is_none());
        // Empty root.
        assert!(chunk_key_generation("", &format!(".__edgezero_chunks.{good}.0")).is_none());
        // Short SHA (< 64).
        assert!(
            chunk_key_generation("app_config", "app_config.__edgezero_chunks.abc123.0").is_none()
        );
        // Uppercase SHA.
        let upper = "A".repeat(64);
        assert!(
            chunk_key_generation(
                "app_config",
                &format!("app_config.__edgezero_chunks.{upper}.0")
            )
            .is_none()
        );
        // Non-hex SHA of the right length.
        let nonhex = "z".repeat(64);
        assert!(
            chunk_key_generation(
                "app_config",
                &format!("app_config.__edgezero_chunks.{nonhex}.0")
            )
            .is_none()
        );
        // Leading-zero / non-numeric / missing index.
        assert!(chunk_key_generation("app_config", &format!("{base}.00")).is_none());
        assert!(chunk_key_generation("app_config", &format!("{base}.007")).is_none());
        assert!(chunk_key_generation("app_config", &format!("{base}.x")).is_none());
        assert!(chunk_key_generation("app_config", &base).is_none());
    }

    // Regression for the Value-first rule: a value that IS pointer-kind but
    // is missing required fields (`chunks`, `data_sha256`, …) must WARN, not
    // be silently dropped by a failed struct deserialize.
    #[test]
    fn prior_chunk_keys_warns_on_pointer_kind_with_missing_fields() {
        let raw = r#"{"edgezero_kind":"fastly_config_chunks","version":2}"#;
        let err = prior_chunk_keys("app_config", raw)
            .expect_err("pointer-kind but malformed must warn, not Ok([])");
        assert!(!err.is_empty(), "{err}");
    }
}
