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

/// Per-entry value limit enforced by Fastly Config Store. Used by the CLI writer
/// to gate direct-vs-chunked storage, and by the pointer validator (which both
/// the CLI and the runtime resolver call) — a chunked envelope is always larger
/// than this, so a pointer claiming otherwise is not one this writer emitted.
pub(crate) const FASTLY_CONFIG_ENTRY_LIMIT: usize = 8_000;
/// Maximum length of a Config Store KEY, in CHARACTERS. Fastly's docs disagree
/// (the guide says 255, the API reference says 256); we take the STRICTER 255 so
/// a key we emit is valid under either. The writer must not emit a physical key
/// longer than this — a derived chunk key adds ~85 characters to the root, so a
/// near-limit root would otherwise fail only mid-write. Also used by the pointer
/// validator (CLI and runtime): a pointer referencing an over-limit key is not
/// one this writer could have produced.
pub(crate) const FASTLY_CONFIG_KEY_LIMIT: usize = 255;
/// Target payload size per chunk (kept under the entry limit to leave room for
/// the key and any protocol overhead). The writer never emits a chunk larger
/// than this, so the pointer validator (CLI and runtime) uses it as an upper
/// bound on any single chunk's declared length.
pub(crate) const CHUNK_PAYLOAD_TARGET: usize = 7_000;
/// Infix inserted between the root key and the content-address in a chunk key:
/// `<root>.__edgezero_chunks.<sha256>.<index>`. Used by the writer and by the
/// pointer validator (CLI and runtime) to recognise a canonical chunk key.
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

/// A chunk reference from a validated pointer, for `config gc`.
#[cfg(any(feature = "cli", test))]
pub(crate) struct GcChunkRef {
    pub key: String,
    pub len: usize,
    pub sha256: String,
}

/// A validated v1 pointer's contents, for `config gc`.
#[cfg(any(feature = "cli", test))]
pub(crate) struct GcPointer {
    /// Validated: non-empty, one generation, dense `0..n-1` in order.
    pub chunks: Vec<GcChunkRef>,
    pub envelope_len: usize,
    pub envelope_sha256: String,
}

/// What a root's value IS, once classified for `config gc`.
#[cfg(any(feature = "cli", test))]
pub(crate) enum GcRootValue {
    /// A valid v1 chunk pointer. Its METADATA is validated; its CONTENT must
    /// still be verified against the store -- see [`gc_verify_generation`].
    Chunked(GcPointer),
    /// A valid, integrity-checked envelope stored inline. References no chunks.
    Direct,
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Compute the lowercase-hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
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
/// Reject a physical Config Store key that exceeds the store's key limit,
/// before any write is attempted.
#[cfg(any(feature = "cli", test))]
fn check_config_key_len(key: &str) -> Result<(), String> {
    // CHARACTERS, not bytes: Fastly's limit is a character count, so a non-ASCII
    // `--key` must be measured by `chars().count()`, not `len()` (UTF-8 bytes).
    let char_len = key.chars().count();
    if char_len > FASTLY_CONFIG_KEY_LIMIT {
        return Err(format!(
            "config-store key is {char_len} characters, over Fastly's \
             {FASTLY_CONFIG_KEY_LIMIT}-character limit. Chunked storage derives keys of the form \
             `<root>.__edgezero_chunks.<64-char-sha>.<index>`, adding ~85 characters to the root, \
             so a long store id or `--key` override can push the derived key past the limit. Use a \
             shorter store id / `--key`."
        ));
    }
    Ok(())
}

#[cfg(any(feature = "cli", test))]
pub(crate) fn prepare_fastly_config_entries(
    root_key: &str,
    envelope_json: &str,
) -> Result<Vec<(String, String)>, String> {
    // The root key itself is a physical key on both paths (the direct entry and
    // the pointer entry), so it must fit the store's key limit before anything
    // is written.
    check_config_key_len(root_key)?;

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
        // A chunk key adds the infix + a 64-char SHA + an index to the root key
        // (~85 chars). A root that is itself valid can push the derived key past
        // the store limit; reject it here rather than fail mid-write with some
        // chunks already committed.
        check_config_key_len(&chunk_key)?;
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
        // Redacted: `verify()`'s message names the stored hashes, which are
        // config-controlled strings that may hold anything.
        envelope.verify().map_err(|_err| {
            format!(
                "BlobEnvelope at key `{root_key}` failed its integrity check (details redacted)"
            )
        })?;
        return Ok(root_value);
    }

    // --- Path 2: chunk pointer ---
    // The pointer entry itself is a single Config Store value, so the writer can
    // never emit one larger than the entry limit. Reject an over-limit pointer up
    // front — this bounds how many chunk refs it can carry (hence the fetch
    // fan-out) before any parsing.
    if root_value.len() > FASTLY_CONFIG_ENTRY_LIMIT {
        return Err(format!(
            "chunk pointer at `{root_key}` is larger than the {FASTLY_CONFIG_ENTRY_LIMIT}-character \
             entry limit and so is not one this writer could have stored"
        ));
    }
    let pointer: FastlyChunkPointer = serde_json::from_str(&root_value).map_err(|err| {
        format!(
            "value at key `{root_key}` is neither a valid BlobEnvelope nor a valid \
             chunk pointer ({})",
            redact_json_err(&err)
        )
    })?;

    if pointer.edgezero_kind != POINTER_KIND {
        // The stored `edgezero_kind` is not echoed: it is a value-controlled
        // string on a path whose diagnostics are logged.
        return Err(format!(
            "chunk pointer at `{root_key}` has an unknown `edgezero_kind` (value redacted); \
             expected `{POINTER_KIND}`"
        ));
    }
    if pointer.version != 1 {
        return Err(format!(
            "chunk pointer at `{root_key}` has unsupported version \
             {}; expected 1",
            pointer.version
        ));
    }
    // Validate the pointer METADATA before fetching anything: a shape the writer
    // could never emit (non-canonical keys, mixed generations, gaps, per-chunk
    // lengths over the payload target, a sum that disagrees with `envelope_len`,
    // or an `envelope_len` small enough to have been stored directly) is
    // rejected up front rather than driving a fan-out of chunk fetches whose
    // hashes could only fail at the end. Same validator the CLI/GC path uses.
    validate_pointer_chunks(root_key, &pointer)?;

    // Fetch, verify, and concatenate all chunks.
    //
    // NOT `with_capacity(pointer.envelope_len)`: that length is untrusted stored
    // metadata, and this runs in the edge guest. A pointer declaring `usize::MAX`
    // would abort the worker on a capacity overflow before any of the checks
    // below could reject it. Grow from the bytes we actually fetch instead.
    let mut reconstructed = String::new();
    for (position, chunk_ref) in pointer.chunks.iter().enumerate() {
        let chunk_value = fetch(&chunk_ref.key)
            .map_err(|_err| {
                // The callback's error carries the chunk KEY (pointer-controlled),
                // so it is not propagated verbatim. Position locates the fault.
                format!(
                    "chunk {position} referenced by pointer at `{root_key}` could not be fetched \
                     (details redacted)"
                )
            })?
            .ok_or_else(|| {
                format!("missing chunk {position} referenced by pointer at `{root_key}`")
            })?;

        // Verify length.
        if chunk_value.len() != chunk_ref.len {
            return Err(format!(
                "chunk {position} (referenced by `{root_key}`) has length {} but the pointer \
                 records {}",
                chunk_value.len(),
                chunk_ref.len,
            ));
        }

        // Verify SHA. Neither hash is echoed: the expected one comes from the
        // stored pointer, so it is value-controlled.
        if sha256_hex(chunk_value.as_bytes()) != chunk_ref.sha256 {
            return Err(format!(
                "chunk {position} (referenced by `{root_key}`) does not match the SHA-256 the \
                 pointer records for it (hashes redacted)"
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
        // Neither SHA is echoed: the expected one is `pointer.envelope_sha256`,
        // a stored, value-controlled string that a malformed pointer can set to
        // anything (a secret), and this runs on the read path where diagnostics
        // are logged.
        return Err(format!(
            "reconstructed envelope for `{root_key}` does not match the pointer's \
             `envelope_sha256` (hashes redacted)"
        ));
    }

    // Parse AND verify the inner envelope, exactly as the direct path does.
    //
    // The outer checks above only prove the chunks reassemble to the bytes the
    // pointer names; they say nothing about the `BlobEnvelope` INSIDE. Without
    // this, a reconstructed value whose embedded `sha256` is wrong (a secret)
    // sails through the resolver, and core's own `verify()` then formats that
    // stored hash into an HTTP 500. Redact to a category so the resolver
    // guarantees -- on BOTH paths -- that whatever it returns is a verified
    // envelope and no stored value escapes.
    let envelope: BlobEnvelope = serde_json::from_str(&reconstructed).map_err(|err| {
        format!(
            "reconstructed value at key `{root_key}` is not a valid config envelope ({})",
            redact_json_err(&err)
        )
    })?;
    envelope.verify().map_err(|_err| {
        format!(
            "reconstructed envelope at key `{root_key}` failed its integrity check (details \
             redacted -- the stored hashes are value-controlled)"
        )
    })?;

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
    //    The serde error is REDACTED -- its Display quotes the offending value
    //    ("invalid type: string \"hunter2\", expected u8"), and a stored config
    //    may hold credentials. Position + category only.
    let pointer: FastlyChunkPointer = serde_json::from_value(value).map_err(|err| {
        format!(
            "prior chunk pointer at `{root_key}` is malformed ({}); skipping chunk GC",
            redact_json_err(&err)
        )
    })?;
    if pointer.version != 1 {
        return Err(format!(
            "prior chunk pointer at `{root_key}` has unsupported version {}; skipping chunk GC",
            pointer.version
        ));
    }

    // 3. The pointer must be INTERNALLY CONSISTENT, not merely well-typed.
    //    `chunks` is the authoritative live set on the GC path, so a pointer
    //    that parses but under-reports its chunks (empty list, an omitted
    //    index, a stale generation mixed in, lengths that cannot add up to the
    //    envelope) would leave real live chunks looking orphaned. Only accept a
    //    chunk list this writer could actually have emitted.
    validate_pointer_chunks(root_key, &pointer)
        .map_err(|err| format!("{err}; skipping chunk GC"))?;

    Ok(pointer.chunks.into_iter().map(|chunk| chunk.key).collect())
}

/// Describe a `serde_json` failure WITHOUT quoting the offending value.
///
/// `serde_json::Error`'s `Display` embeds the input that failed to parse, and
/// these diagnostics are logged verbatim. Stored app config may hold secrets, so
/// only the position and category ever escape.
///
/// Unconditional: the runtime resolver needs it too. A guest reading a malformed
/// pointer must not log the value either — that path runs on every request.
fn redact_json_err(err: &serde_json::Error) -> String {
    use serde_json::error::Category;

    let category = match err.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "schema",
        Category::Eof => "unexpected end of input",
    };
    format!(
        "{category} error at line {} column {} -- value redacted",
        err.line(),
        err.column()
    )
}

/// Is this pointer's chunk list one `prepare_fastly_config_entries` could have
/// produced? Anything else is treated as unreadable rather than authoritative.
///
/// Checks, in order: non-empty; every key canonical for this root; a SINGLE
/// generation matching `envelope_sha256`; indexes exactly `0..n-1`, each once,
/// in order; per-chunk lengths within what the writer emits; and
/// `sum(len) == envelope_len`, computed with CHECKED arithmetic.
///
/// Called on BOTH the CLI/GC path and the runtime resolver, so a pointer shape
/// the writer could never emit is rejected everywhere, not just during GC.
///
/// **Diagnostics name a chunk's POSITION, never its key.** `chunks[].key` is
/// pointer-controlled and, on this path, not yet validated -- a malformed
/// pointer can carry any string at all there (`prod-db-password=hunter2`), and
/// these messages are logged verbatim.
///
/// **Lengths are untrusted input.** They are attacker-supplied `usize`s, so they
/// are bounded against what the writer can actually emit and summed with
/// `checked_add`. Saturating arithmetic would let a metadata-consistent pointer
/// declare `usize::MAX` and survive validation.
fn validate_pointer_chunks(root_key: &str, pointer: &FastlyChunkPointer) -> Result<(), String> {
    let bad = |detail: &str| format!("chunk pointer at `{root_key}` {detail}");

    if pointer.chunks.is_empty() {
        // An oversized envelope always splits into >= 2 chunks, so an empty
        // list is never something we wrote -- and it would report "references
        // nothing", orphaning the real chunks.
        return Err(bad("references no chunks at all"));
    }

    let mut total_len = 0_usize;
    for (position, chunk_ref) in pointer.chunks.iter().enumerate() {
        // Every referenced key must be a CANONICAL chunk key of this root --
        // the same validator that gates deletion.
        let Some((generation, index)) = chunk_key_parts(root_key, &chunk_ref.key) else {
            return Err(bad(&format!(
                "references a non-canonical chunk key at position {position}"
            )));
        };
        // The physical key must fit the store's key limit — the writer never
        // emits one that does not, so an over-limit key is not ours.
        if chunk_ref.key.chars().count() > FASTLY_CONFIG_KEY_LIMIT {
            return Err(bad(&format!(
                "references a chunk key at position {position} longer than the \
                 {FASTLY_CONFIG_KEY_LIMIT}-character store limit"
            )));
        }
        // One pointer names exactly one generation. A mixed list means some
        // other generation's chunks are being reported as this one's.
        if generation != pointer.envelope_sha256 {
            return Err(bad(&format!(
                "references a chunk at position {position} belonging to a different generation \
                 than the pointer's own `envelope_sha256`"
            )));
        }
        // Indexes are dense, unique and ordered: 0, 1, ... n-1. This is what
        // rejects an omitted, duplicated or reordered index -- each of which
        // would silently shrink the live set.
        if index != position {
            return Err(bad(&format!(
                "references chunk index {index} at position {position}, but a chunk list must be \
                 indexed 0..n-1 with no gaps, duplicates or reordering"
            )));
        }
        if chunk_ref.len == 0 {
            return Err(bad(&format!(
                "references a zero-length chunk at position {position}"
            )));
        }
        // The writer never emits a chunk larger than its payload target, so a
        // bigger one is not ours -- and this is what stops an absurd declared
        // length ever reaching an allocation.
        if chunk_ref.len > CHUNK_PAYLOAD_TARGET {
            return Err(bad(&format!(
                "references a chunk at position {position} declaring {} bytes, more than the \
                 {CHUNK_PAYLOAD_TARGET}-byte maximum this writer emits",
                chunk_ref.len
            )));
        }
        // Every chunk EXCEPT the last is a full payload: the writer walks
        // `CHUNK_PAYLOAD_TARGET` bytes then retreats at most 3 to the previous
        // UTF-8 boundary (the longest codepoint is 4 bytes), so a non-last chunk
        // is always >= CHUNK_PAYLOAD_TARGET - 3. This pins the split layout to
        // what the writer emits and bounds the chunk count (hence the runtime
        // fetch fan-out) to about `envelope_len / CHUNK_PAYLOAD_TARGET`; a
        // hand-authored pointer of many tiny chunks is rejected.
        let is_last = position == pointer.chunks.len().saturating_sub(1);
        if !is_last && chunk_ref.len < CHUNK_PAYLOAD_TARGET.saturating_sub(3) {
            return Err(bad(&format!(
                "references a non-final chunk at position {position} of only {} bytes; this writer \
                 fills every chunk but the last to within 3 bytes of {CHUNK_PAYLOAD_TARGET}",
                chunk_ref.len
            )));
        }
        total_len = total_len.checked_add(chunk_ref.len).ok_or_else(|| {
            bad("declares chunk lengths that overflow when summed (not a real envelope)")
        })?;
    }

    // The parts must add up to the whole. If they don't, the pointer does not
    // describe the envelope it claims to, so its chunk list is not trustworthy.
    if total_len != pointer.envelope_len {
        return Err(bad(&format!(
            "declares `envelope_len` {} but its chunk lengths sum to {total_len}",
            pointer.envelope_len
        )));
    }
    // We only chunk what does not fit directly, so a pointer describing an
    // envelope that WOULD have fit is not one we wrote.
    if pointer.envelope_len <= FASTLY_CONFIG_ENTRY_LIMIT {
        return Err(bad(&format!(
            "declares an envelope of {} bytes, which fits the {FASTLY_CONFIG_ENTRY_LIMIT}-byte \
             entry limit and so would never have been chunked by this writer",
            pointer.envelope_len
        )));
    }

    Ok(())
}

/// Does this value announce itself as a chunk pointer?
///
/// A cheap discriminant for "this entry is a ROOT, wherever it happens to live".
/// GC must not decide root-vs-chunk by KEY SHAPE: the runtime resolver follows
/// whatever pointer it is handed, so a pointer parked at a chunk-shaped key
/// still makes its references live. Chunk payloads are raw envelope fragments
/// and essentially never parse as JSON, let alone carry `edgezero_kind`.
#[cfg(any(feature = "cli", test))]
pub(crate) fn value_is_pointer_kind(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|value| {
        value
            .get("edgezero_kind")
            .and_then(serde_json::Value::as_str)
            == Some(POINTER_KIND)
    })
}

/// Classify a ROOT value for `config gc` — the live-set input on a DESTRUCTIVE
/// path, so it is fail-closed. Unlike [`prior_chunk_keys`] (which is lenient for
/// the push path and treats unrelated/empty JSON as "references nothing"), this
/// accepts ONLY:
///
/// - a valid, integrity-checked direct `BlobEnvelope` → `Direct`;
/// - a valid v1 chunk pointer → `Chunked` (metadata validated).
///
/// Anything else — an empty string, a truncated/partial value, unrelated JSON,
/// or a pointer that fails validation — is `Err`. A root we cannot classify has
/// unknown references, so we must reclaim NOTHING rather than treat its live
/// chunks as orphaned.
///
/// **Metadata is not proof.** A `Chunked` result says the pointer is
/// self-consistent, NOT that it honestly describes its generation: a pointer can
/// drop its last chunk ref AND restate `envelope_len` as the remaining sum, and
/// every metadata check still passes while the dropped chunk silently leaves the
/// live set. Callers MUST reconstruct the referenced chunks and put them through
/// [`gc_verify_generation`] before trusting the live set.
#[cfg(any(feature = "cli", test))]
pub(crate) fn gc_classify_root(root_key: &str, raw: &str) -> Result<GcRootValue, String> {
    use edgezero_core::blob_envelope::BlobEnvelope;

    if raw.trim().is_empty() {
        return Err(format!(
            "root `{root_key}` has an empty value; refusing to reclaim (cannot tell what it references)"
        ));
    }
    // Every diagnostic below is REDACTED: serde's Display quotes the offending
    // input, `verify()`'s names the stored hashes, and BOTH are attacker- or
    // config-controlled strings that may hold credentials.
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|err| {
        format!(
            "root `{root_key}` is not valid JSON ({}); refusing to reclaim",
            redact_json_err(&err)
        )
    })?;
    if value
        .get("edgezero_kind")
        .and_then(serde_json::Value::as_str)
        == Some(POINTER_KIND)
    {
        let pointer: FastlyChunkPointer = serde_json::from_value(value).map_err(|err| {
            format!(
                "root `{root_key}` is a chunk pointer but is malformed ({}); refusing to reclaim",
                redact_json_err(&err)
            )
        })?;
        if pointer.version != 1 {
            return Err(format!(
                "root `{root_key}` chunk pointer has unsupported version {}; refusing to reclaim",
                pointer.version
            ));
        }
        validate_pointer_chunks(root_key, &pointer)?;
        return Ok(GcRootValue::Chunked(GcPointer {
            chunks: pointer
                .chunks
                .into_iter()
                .map(|chunk| GcChunkRef {
                    key: chunk.key,
                    len: chunk.len,
                    sha256: chunk.sha256,
                })
                .collect(),
            envelope_len: pointer.envelope_len,
            envelope_sha256: pointer.envelope_sha256,
        }));
    }
    // Otherwise it must be a valid, integrity-checked direct envelope.
    let envelope: BlobEnvelope = serde_json::from_value(value).map_err(|err| {
        format!(
            "root `{root_key}` is neither a valid chunk pointer nor a valid config envelope ({}); \
             refusing to reclaim",
            redact_json_err(&err)
        )
    })?;
    envelope.verify().map_err(|_err| {
        format!(
            "root `{root_key}` envelope failed its integrity check (details redacted -- the \
             stored hashes are config-controlled); refusing to reclaim"
        )
    })?;
    Ok(GcRootValue::Direct)
}

/// Check that `assembled` really is the generation named by `generation_sha`.
///
/// A chunk key embeds the SHA-256 of the WHOLE envelope it belongs to, so
/// reassembling a chunk set either reproduces the content-address its own keys
/// name, or it does not. Nothing an inconsistent store says about lengths,
/// indexes or counts survives this.
///
/// **This is a consistency check, NOT proof of authorship — do not mistake it
/// for one.** Content-addressing is not a signature: anyone can pick an envelope
/// E, compute `H = sha256(E)`, and store the pieces under `H`. No preimage
/// attack is involved, so passing here says the bytes are internally consistent,
/// not that `EdgeZero` wrote them. Callers deciding to DELETE must additionally
/// require the entries to be byte-identical to this writer's own output — see
/// `prove_generation` in `cli.rs`.
///
/// Used for BOTH destructive decisions:
/// - a live pointer's chunks must reconstruct the envelope it claims (else its
///   chunk list is not the true live set);
/// - a delete candidate's generation must reconstruct a real envelope (a
///   necessary, but not sufficient, condition for reclaiming it).
#[cfg(any(feature = "cli", test))]
pub(crate) fn gc_verify_generation(generation_sha: &str, assembled: &str) -> Result<(), String> {
    use edgezero_core::blob_envelope::BlobEnvelope;

    // Neither hash is echoed: `generation_sha` comes from the chunk KEYS
    // (pointer-controlled) and `actual` is derived from the reassembled config
    // bytes. Both are stored/derived strings, and this message is logged.
    if sha256_hex(assembled.as_bytes()) != generation_sha {
        return Err(
            "the chunk set does not reassemble to the generation its own keys name (hashes \
             redacted), so these chunks are not the generation they claim"
                .to_owned(),
        );
    }
    // Content-addressing proves the bytes are SELF-CONSISTENT, not that EdgeZero
    // wrote them (a forger can content-address their own envelope -- see this
    // function's doc comment). Parsing here just confirms the reassembled bytes
    // are a config envelope at all; the authorship-adjacent decision (delete or
    // not) is `prove_generation`'s round-trip against the writer, in cli.rs.
    let envelope: BlobEnvelope = serde_json::from_str(assembled).map_err(|err| {
        format!(
            "the chunk set reassembles to its content-address but does not parse as a config \
             envelope ({})",
            redact_json_err(&err)
        )
    })?;
    envelope.verify().map_err(|_err| {
        "the chunk set reassembles to a config envelope that fails its own integrity check \
         (details redacted -- the stored hashes are config-controlled)"
            .to_owned()
    })?;
    Ok(())
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
    chunk_key_parts(root_key, key).map(|(generation, _)| generation)
}

/// Split a canonical chunk key into its `(generation, index)`.
///
/// The validating half of [`chunk_key_generation`], which discards the index.
/// Pointer validation needs both: the generation to prove a chunk list names one
/// generation, the index to prove the list is dense and ordered.
fn chunk_key_parts(root_key: &str, key: &str) -> Option<(String, usize)> {
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
    if !is_canonical_sha256_hex(sha) {
        return None;
    }
    let parsed = canonical_index(index)?;
    Some((sha.to_owned(), parsed))
}

/// Exactly 64 lowercase hex characters.
fn is_canonical_sha256_hex(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Parse a canonical decimal index: all ASCII digits, no leading zero unless the
/// value is exactly `0`, and it must fit `usize`.
///
/// Must be an index this writer could actually have emitted: the chunk loop
/// counts in `usize`, so a digit run that overflows it (or is absurdly long) is
/// not ours -- accept only what `prepare_fastly_config_entries` can produce.
fn canonical_index(index: &str) -> Option<usize> {
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if index != "0" && index.starts_with('0') {
        return None;
    }
    index.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use super::*;

    /// the index must be one this writer could emit. The
    /// chunk loop counts in `usize`, so a digit run that overflows it is not
    /// ours -- and this gate authorises DELETES.
    #[test]
    fn canonical_index_must_fit_usize() {
        let sha = "a".repeat(64);
        let root = "app_config";
        assert!(chunk_key_generation(root, &format!("{root}.__edgezero_chunks.{sha}.0")).is_some());
        assert!(chunk_key_generation(root, &format!("{root}.__edgezero_chunks.{sha}.7")).is_some());
        // 40 digits: all-ASCII-digit and no leading zero, but not a usize.
        let overflow = "9".repeat(40);
        assert!(
            chunk_key_generation(root, &format!("{root}.__edgezero_chunks.{sha}.{overflow}"))
                .is_none(),
            "an index that overflows usize is not a key we wrote"
        );
    }

    // ---- gc_classify_root / pointer consistency () ----

    /// A real pointer + its real chunks, for mutation in the tests below.
    fn pointer_fixture(root: &str) -> (String, FastlyChunkPointer) {
        let envelope = make_envelope_json(20_000);
        let entries = prepare_fastly_config_entries(root, &envelope).expect("expand");
        let (_, pointer_json) = entries.last().expect("pointer").clone();
        let pointer: FastlyChunkPointer = serde_json::from_str(&pointer_json).expect("parse");
        (pointer_json, pointer)
    }

    fn reserialise(pointer: &FastlyChunkPointer) -> String {
        serde_json::to_string(pointer).expect("serialise")
    }

    /// A valid pointer whose chunks are scoped to a CHUNK-SHAPED holder key
    /// classifies successfully (returns `Chunked` with those keys), not `Err`.
    /// The cross-root GC test only shows an unclassifiable pointer failing
    /// closed; this pins that a well-formed self-scoped pointer is accepted.
    #[test]
    fn gc_classify_root_accepts_a_pointer_rooted_at_a_chunk_shaped_key() {
        let holder = format!("app_config{CHUNK_KEY_INFIX}{}.0", "e".repeat(64));
        let (pointer_json, pointer) = pointer_fixture(&holder);
        let classified = gc_classify_root(&holder, &pointer_json)
            .expect("a valid self-scoped pointer classifies");
        let GcRootValue::Chunked(gc_pointer) = classified else {
            panic!("a pointer value must classify as Chunked");
        };
        assert_eq!(gc_pointer.chunks.len(), pointer.chunks.len());
        for chunk in &gc_pointer.chunks {
            assert!(
                chunk.key.starts_with(&format!("{holder}{CHUNK_KEY_INFIX}")),
                "each referenced key must be scoped to the holder: `{}`",
                chunk.key
            );
        }
    }

    /// The happy path: a real pointer classifies as its own chunk keys.
    #[test]
    fn gc_classify_root_accepts_a_real_pointer() {
        let root = "app_config";
        let (pointer_json, pointer) = pointer_fixture(root);
        let classified =
            gc_classify_root(root, &pointer_json).expect("a real pointer must classify");
        let GcRootValue::Chunked(gc_pointer) = classified else {
            panic!("a pointer value must classify as Chunked");
        };
        assert!(
            gc_pointer.chunks.len() >= 2,
            "an oversized envelope always splits into >= 2"
        );
        // The classifier must carry the pointer's refs VERBATIM: `config gc`
        // checks each stored chunk against these `len`/`sha256` values before
        // reassembling, so a lossy or reordered mapping here would silently
        // weaken every downstream content check.
        assert_eq!(gc_pointer.envelope_len, pointer.envelope_len);
        assert_eq!(gc_pointer.envelope_sha256, pointer.envelope_sha256);
        assert_eq!(gc_pointer.chunks.len(), pointer.chunks.len());
        for (got, want) in gc_pointer.chunks.iter().zip(pointer.chunks.iter()) {
            assert_eq!(got.key, want.key);
            assert_eq!(got.len, want.len);
            assert_eq!(got.sha256, want.sha256);
        }
        // ...and those refs actually describe the stored chunks.
        let entries =
            prepare_fastly_config_entries(root, &make_envelope_json(20_000)).expect("expand");
        for (chunk, (_, value)) in gc_pointer.chunks.iter().zip(entries.iter()) {
            assert_eq!(chunk.len, value.len(), "ref len must match the real chunk");
            assert_eq!(
                chunk.sha256,
                sha256_hex(value.as_bytes()),
                "ref sha256 must match the real chunk"
            );
        }
    }

    /// A direct envelope is a root that references no chunks.
    #[test]
    fn gc_classify_root_accepts_a_direct_envelope() {
        let envelope = make_envelope_json(200);
        let classified =
            gc_classify_root("app_config", &envelope).expect("a valid envelope must classify");
        assert!(
            matches!(classified, GcRootValue::Direct),
            "an inline envelope must classify as Direct (it references no chunks)"
        );
    }

    /// values that are NOT a valid envelope or pointer must
    /// fail closed. `Ok([])` here would mean "references nothing" and would make
    /// the root's own live chunks look orphaned.
    #[test]
    fn gc_classify_root_fails_closed_on_unclassifiable_values() {
        let root = "app_config";
        let (pointer_json, _) = pointer_fixture(root);
        // A VALID-JSON partial pointer: `chunks` truncated to one entry. This is
        // the case the earlier truncated-JSON test missed.
        let mut partial: FastlyChunkPointer = serde_json::from_str(&pointer_json).unwrap();
        partial.chunks.truncate(1);
        let cases: Vec<(&str, String)> = vec![
            ("empty", String::new()),
            ("whitespace", "   ".to_owned()),
            ("not json", "not-json-at-all".to_owned()),
            ("unrelated json", r#"{"some":"value"}"#.to_owned()),
            ("json scalar", "42".to_owned()),
            ("valid-JSON partial pointer", reserialise(&partial)),
        ];
        for (label, raw) in cases {
            assert!(
                gc_classify_root(root, &raw).is_err(),
                "a {label} root value must fail closed, not classify as \"references nothing\""
            );
        }
    }

    /// a pointer that PARSES but under-reports its chunks
    /// would shrink the live set and get real live chunks deleted. Every way a
    /// chunk list can lie must be rejected.
    #[test]
    fn pointer_chunk_list_must_be_internally_consistent() {
        let root = "app_config";
        let (_, base) = pointer_fixture(root);

        // 1. Empty list: never emitted (an oversized envelope splits into >= 2).
        let mut empty = clone_pointer(&base);
        empty.chunks.clear();
        // 2. Omitted index: chunks [0, 2] -- 1 is live but unreferenced.
        let mut omitted = clone_pointer(&base);
        omitted.chunks.remove(1);
        // 3. Duplicate index: [0, 0] -- reports fewer distinct keys than exist.
        let mut duplicated = clone_pointer(&base);
        let first = clone_ref(&base.chunks[0]);
        duplicated.chunks = vec![clone_ref(&first), first];
        // 4. Reordered: [1, 0] -- not a shape the writer emits.
        let mut reordered = clone_pointer(&base);
        reordered.chunks.reverse();
        // 5. Mixed generation: a key from another envelope's content-address.
        let mut mixed = clone_pointer(&base);
        let other = make_envelope_json(19_000);
        let other_entries = prepare_fastly_config_entries(root, &other).expect("expand");
        mixed.chunks[1] = FastlyChunkRef {
            key: other_entries[1].0.clone(),
            len: base.chunks[1].len,
            sha256: base.chunks[1].sha256.clone(),
        };
        // 6. Lengths that cannot add up to the declared envelope.
        let mut bad_len = clone_pointer(&base);
        bad_len.envelope_len = base.envelope_len.saturating_add(999);
        // 7. Zero-length chunk.
        let mut zero_len = clone_pointer(&base);
        zero_len.chunks[0].len = 0;

        for (label, pointer) in [
            ("an empty chunk list", empty),
            ("an omitted index", omitted),
            ("a duplicated index", duplicated),
            ("a reordered list", reordered),
            ("a mixed generation", mixed),
            ("an inconsistent envelope_len", bad_len),
            ("a zero-length chunk", zero_len),
        ] {
            let raw = reserialise(&pointer);
            assert!(
                gc_classify_root(root, &raw).is_err(),
                "a pointer with {label} must be rejected, not treated as the authoritative live set"
            );
            assert!(
                prior_chunk_keys(root, &raw).is_err(),
                "a pointer with {label} must also warn on the push path, not prune from a bad list"
            );
        }

        // Control: the unmutated fixture still passes, so the assertions above
        // are rejecting the mutation and not something incidental.
        gc_classify_root(root, &reserialise(&base))
            .expect("the unmutated fixture must still classify");
    }

    /// a parse diagnostic must never quote the stored
    /// value -- app config may hold credentials and these lines are logged.
    #[test]
    fn pointer_parse_errors_do_not_leak_stored_values() {
        const SENTINEL: &str = "s3cr3t-do-not-log";
        let root = "app_config";
        // `version` is typed `u8`; a string there makes serde quote it:
        // `invalid type: string "s3cr3t-do-not-log", expected u8`.
        let malformed = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":"{SENTINEL}","chunks":[],"data_sha256":"","envelope_len":0,"envelope_sha256":""}}"#
        );
        let Err(gc_err) = gc_classify_root(root, &malformed) else {
            panic!("a malformed pointer must fail closed on the gc path");
        };
        for err in [
            prior_chunk_keys(root, &malformed).expect_err("must warn"),
            gc_err,
        ] {
            assert!(
                !err.contains(SENTINEL),
                "diagnostic leaked a stored value: {err}"
            );
            assert!(
                err.contains("redacted"),
                "diagnostic should say the value was redacted: {err}"
            );
        }
    }

    fn clone_pointer(pointer: &FastlyChunkPointer) -> FastlyChunkPointer {
        FastlyChunkPointer {
            chunks: pointer.chunks.iter().map(clone_ref).collect(),
            data_sha256: pointer.data_sha256.clone(),
            edgezero_kind: pointer.edgezero_kind.clone(),
            envelope_len: pointer.envelope_len,
            envelope_sha256: pointer.envelope_sha256.clone(),
            version: pointer.version,
        }
    }

    fn clone_ref(chunk: &FastlyChunkRef) -> FastlyChunkRef {
        FastlyChunkRef {
            key: chunk.key.clone(),
            len: chunk.len,
            sha256: chunk.sha256.clone(),
        }
    }

    /// Round-7 [P1]: a COORDINATED partial pointer -- drop the last chunk ref AND
    /// restate `envelope_len` as the remaining sum. Generation, indexes, lengths
    /// and the sum all agree; only the CONTENT disagrees. Metadata validation
    /// cannot see it, so the dropped chunk would silently leave the live set and
    /// become deletable. Only reassembling and hashing catches this.
    #[test]
    fn coordinated_partial_pointer_fails_content_verification() {
        let root = "app_config";
        let envelope = make_envelope_json(20_000);
        let entries = prepare_fastly_config_entries(root, &envelope).expect("expand");
        let (_, pointer_json) = entries.last().expect("pointer").clone();
        let mut pointer: FastlyChunkPointer = serde_json::from_str(&pointer_json).unwrap();
        assert!(pointer.chunks.len() >= 3, "need >= 3 chunks for this case");

        let dropped = pointer.chunks.pop().expect("last chunk");
        pointer.envelope_len = pointer.chunks.iter().map(|chunk| chunk.len).sum();
        let doctored = serde_json::to_string(&pointer).expect("serialise");

        // METADATA validation passes -- that is the whole point of the attack.
        let classified = gc_classify_root(root, &doctored)
            .expect("the doctored pointer is metadata-consistent by construction");
        let GcRootValue::Chunked(gc_pointer) = classified else {
            panic!("expected a chunked root");
        };
        assert!(
            !gc_pointer
                .chunks
                .iter()
                .any(|chunk| chunk.key == dropped.key),
            "fixture check: the dropped chunk is absent from the pointer's list"
        );

        // CONTENT verification is what rejects it: the surviving chunks cannot
        // hash to the envelope the generation names.
        let assembled: String = entries[..entries.len().saturating_sub(2)]
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        let err = gc_verify_generation(&gc_pointer.envelope_sha256, &assembled)
            .expect_err("a partial chunk set must not verify as its generation");
        assert!(
            err.contains("not the generation they claim"),
            "expected a content-address mismatch, got: {err}"
        );

        // And the honest, complete set still verifies -- so the assertion above
        // is rejecting the omission, not something incidental.
        let whole: String = entries[..entries.len().saturating_sub(1)]
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        gc_verify_generation(&gc_pointer.envelope_sha256, &whole)
            .expect("the complete chunk set must verify");
    }

    /// Round-7/9 [P1]: an entry that merely LOOKS like a chunk key is not a chunk.
    /// Reassembling to the content-address its keys name is NECESSARY but not
    /// sufficient (a forger can content-address their own data with no preimage);
    /// `prove_generation` adds the writer round-trip. Here we check the necessary
    /// half: a value that does not even hash to its generation is never ours.
    #[test]
    fn only_a_real_generation_verifies() {
        let root = "app_config";
        let envelope = make_envelope_json(20_000);
        let entries = prepare_fastly_config_entries(root, &envelope).expect("expand");
        let generation = chunk_key_generation(root, &entries[0].0).expect("canonical key");

        for (label, value) in [
            ("plain text", "just some plain text"),
            ("unrelated json", r#"{"some":"value"}"#),
            ("someone's real config", make_envelope_json(200).as_str()),
        ] {
            assert!(
                gc_verify_generation(&generation, value).is_err(),
                "a {label} value must never verify as a generation we wrote"
            );
        }

        // The genuine article does verify.
        let whole: String = entries[..entries.len().saturating_sub(1)]
            .iter()
            .map(|(_, value)| value.as_str())
            .collect();
        gc_verify_generation(&generation, &whole).expect("the real generation must verify");
    }

    /// `chunks[].key` is pointer-controlled and NOT yet
    /// validated where we report it, so a malformed pointer can smuggle a secret
    /// into a log line. Diagnostics must name a position, never the key.
    #[test]
    fn pointer_key_does_not_leak_into_diagnostics() {
        const SENTINEL: &str = "prod-db-password=hunter2";
        let root = "app_config";
        let malformed = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"{SENTINEL}","len":10,"sha256":"x"}}],"data_sha256":"","envelope_len":10,"envelope_sha256":"{}"}}"#,
            "a".repeat(64)
        );
        let err = prior_chunk_keys(root, &malformed).expect_err("must warn");
        assert!(
            !err.contains(SENTINEL),
            "a pointer-controlled key must never reach a diagnostic: {err}"
        );
    }

    /// `envelope_len` and the per-chunk lengths are
    /// untrusted `usize`s that later size allocations. Saturating arithmetic let
    /// a metadata-consistent pointer declare `usize::MAX` and pass validation,
    /// which then reached `String::with_capacity` and aborted the process.
    #[test]
    fn absurd_pointer_lengths_are_rejected_before_allocating() {
        let root = "app_config";
        let sha = "a".repeat(64);
        let huge = usize::MAX;
        let overflowing = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.0","len":{huge},"sha256":"x"}},{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.1","len":{huge},"sha256":"y"}}],"data_sha256":"","envelope_len":{huge},"envelope_sha256":"{sha}"}}"#
        );
        assert!(
            prior_chunk_keys(root, &overflowing).is_err(),
            "chunk lengths that overflow when summed must be rejected"
        );

        // A single absurd chunk: no overflow, but far beyond what we emit.
        let oversized = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.0","len":{huge},"sha256":"x"}}],"data_sha256":"","envelope_len":{huge},"envelope_sha256":"{sha}"}}"#
        );
        assert!(
            prior_chunk_keys(root, &oversized).is_err(),
            "a chunk larger than the writer's payload target must be rejected"
        );
    }

    /// root-vs-chunk is decided by VALUE. A pointer value is a
    /// root wherever it lives; a chunk payload (raw envelope fragment) is not.
    #[test]
    fn value_is_pointer_kind_detects_pointers_only() {
        let root = "app_config";
        let (pointer_json, _) = pointer_fixture(root);
        assert!(
            value_is_pointer_kind(&pointer_json),
            "a real pointer value must be recognised as pointer-kind"
        );

        // A genuine chunk PAYLOAD: a raw fragment of envelope JSON.
        let entries =
            prepare_fastly_config_entries(root, &make_envelope_json(20_000)).expect("expand");
        assert!(
            !value_is_pointer_kind(&entries[0].1),
            "a chunk payload must NOT be mistaken for a pointer"
        );
        // Direct envelopes, unrelated JSON and non-JSON are not pointers either.
        assert!(!value_is_pointer_kind(&make_envelope_json(200)));
        assert!(!value_is_pointer_kind(r#"{"some":"value"}"#));
        assert!(!value_is_pointer_kind("not json at all"));
    }

    /// The runtime resolver rejects a pointer shape the writer could never emit
    /// BEFORE fetching any chunk -- the same metadata validation the CLI/GC path
    /// runs, so invariant 14 holds on the read path too.
    #[test]
    fn runtime_resolver_rejects_non_writer_pointer_shapes() {
        let root = "app_config";
        let sha = "a".repeat(64);
        // A metadata-consistent-looking pointer whose per-chunk length is far
        // over the writer's payload target (and whose envelope is too small to
        // have been chunked at all).
        let bogus = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.0","len":99999,"sha256":"x"}},{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.1","len":1,"sha256":"y"}}],"data_sha256":"","envelope_len":100000,"envelope_sha256":"{sha}"}}"#
        );
        // No fetch should even be attempted: fail on metadata alone.
        let mut fetched = false;
        let err = resolve_fastly_config_value(root, bogus, |_key| {
            fetched = true;
            Ok(None)
        })
        .expect_err("a non-writer pointer shape must be rejected");
        assert!(
            !fetched,
            "validation must reject before any chunk fetch: {err}"
        );

        // The real thing still resolves.
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries(root, &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();
        let resolved = resolve_fastly_config_value(&root_key, pointer_json, |key| {
            Ok(chunk_map.get(key).cloned())
        })
        .expect("a real chunked value must still resolve");
        assert_eq!(resolved, envelope);
    }

    /// A hand-authored pointer of MANY tiny chunks (metadata-consistent: dense,
    /// summing to `envelope_len`) is rejected — its non-final chunks are far
    /// below a full payload, so it is not writer output. This bounds the runtime
    /// fetch fan-out.
    #[test]
    fn pointer_with_many_tiny_chunks_is_rejected() {
        use std::fmt::Write as _;
        let root = "app_config";
        let sha = "a".repeat(64);
        // 100 chunks of 100 bytes each = 10_000 bytes (> entry limit, dense).
        let mut chunks = String::new();
        for idx in 0..100_u32 {
            if idx > 0 {
                chunks.push(',');
            }
            write!(
                chunks,
                r#"{{"key":"{root}{CHUNK_KEY_INFIX}{sha}.{idx}","len":100,"sha256":"x"}}"#
            )
            .expect("write to String");
        }
        let bogus = format!(
            r#"{{"edgezero_kind":"{POINTER_KIND}","version":1,"chunks":[{chunks}],"data_sha256":"","envelope_len":10000,"envelope_sha256":"{sha}"}}"#
        );
        let mut fetched = false;
        let err = resolve_fastly_config_value(root, bogus, |_key| {
            fetched = true;
            Ok(None)
        })
        .expect_err("a tiny-chunk fan-out must be rejected");
        assert!(!fetched, "must reject before fetching 100 chunks: {err}");
    }

    /// A root that is itself valid but long enough that its DERIVED chunk keys
    /// exceed the store limit must be rejected up front, not fail mid-write.
    #[test]
    fn oversized_derived_chunk_keys_are_rejected_before_write() {
        // A root ~200 chars: valid as a key on its own, but + ~85 for the chunk
        // suffix exceeds the limit.
        let long_root = "r".repeat(200);
        let big_envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let err = prepare_fastly_config_entries(&long_root, &big_envelope)
            .expect_err("derived chunk keys over the limit must be rejected");
        assert!(
            err.contains(&FASTLY_CONFIG_KEY_LIMIT.to_string()) && err.contains("limit"),
            "error must name the key limit: {err}"
        );

        // A root over the limit is rejected even for a DIRECT (unchunked) value.
        let over_limit_root = "r".repeat(FASTLY_CONFIG_KEY_LIMIT.saturating_add(1));
        assert!(
            prepare_fastly_config_entries(&over_limit_root, "{}").is_err(),
            "a root key over the limit must be rejected on the direct path too"
        );

        // A normal root is unaffected.
        prepare_fastly_config_entries("app_config", &big_envelope)
            .expect("a normal root must still expand");

        // The limit is CHARACTERS, not bytes: a multi-byte key at exactly the
        // character limit must be accepted even though its byte length exceeds
        // it. U+00E9 ('é') is 2 bytes, so 255 of them is 510 bytes, 255 chars.
        let multibyte_root = "\u{e9}".repeat(FASTLY_CONFIG_KEY_LIMIT);
        assert!(
            multibyte_root.len() > FASTLY_CONFIG_KEY_LIMIT,
            "fixture must be multi-byte"
        );
        prepare_fastly_config_entries(&multibyte_root, "{}")
            .expect("a key at the CHARACTER limit must be accepted regardless of byte length");
    }

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
            err.contains("does not match the SHA-256"),
            "error must say what failed: {err}"
        );
        // the diagnostic identifies the chunk by POSITION, not by
        // key. The key comes from the stored pointer, so echoing it would let a
        // malformed pointer smuggle a secret into a log line. Same for the
        // hashes it records.
        assert!(
            err.contains("chunk 0"),
            "error must locate the failing chunk by position: {err}"
        );
        assert!(
            !err.contains(&first_chunk_key),
            "a pointer-controlled key must not be echoed into a log line: {err}"
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
        assert!(err.contains("length"), "error must mention length: {err}");
        assert!(
            err.contains("chunk 0"),
            "error must locate the failing chunk by position: {err}"
        );
        assert!(
            !err.contains(&first_chunk_key),
            "a pointer-controlled key must not be echoed into a log line: {err}"
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

        // `envelope_sha256` is a stored, value-controlled string.
        // Set it to a SENTINEL secret and require the diagnostic not to echo it.
        let sentinel = "SUPER_SECRET_ENVELOPE_HASH";
        let mut pointer: FastlyChunkPointer = serde_json::from_str(&pointer_json).unwrap();
        pointer.envelope_sha256 = sentinel.to_owned();
        let tampered_pointer_json = serde_json::to_string(&pointer).unwrap();

        let err = resolve_fastly_config_value(&root_key, tampered_pointer_json, |chunk_key| {
            Ok(chunk_map.get(chunk_key).cloned())
        })
        .expect_err("envelope hash mismatch must error");
        // Whether metadata validation (the chunk keys name a different
        // generation than the tampered `envelope_sha256`) or the full-envelope
        // check catches it, the stored hash must never reach the diagnostic.
        assert!(
            !err.contains(sentinel),
            "the stored envelope_sha256 must never reach a diagnostic: {err}"
        );
        assert!(err.contains("root"), "error must name the root key: {err}");
    }

    /// the CHUNKED read path returns reconstructed bytes
    /// after checking only the OUTER pointer hash -- it never parses/verifies the
    /// inner `BlobEnvelope` the way the DIRECT path does. So an envelope whose
    /// embedded `sha256` is a secret passes the resolver, and core's later
    /// `verify()` formats that stored hash into an HTTP 500. The resolver must
    /// reject it here, redacted.
    #[test]
    fn chunked_read_verifies_inner_envelope() {
        use edgezero_core::blob_envelope::BlobEnvelope;
        use serde_json::json;
        const SENTINEL: &str = "SUPER_SECRET_INNER_SHA";

        // A structurally valid envelope whose inner `sha256` is a sentinel that
        // does NOT match its data -- so `verify()` fails and would name it.
        let mut envelope = BlobEnvelope::new(json!({"k":"v"}), "2026-06-22T00:00:00Z".into());
        envelope.sha256 = SENTINEL.to_owned();
        // Pad the SERIALISED envelope past the entry limit so it chunks.
        let mut envelope_json = serde_json::to_string(&envelope).unwrap();
        envelope_json.push_str(&" ".repeat(FASTLY_CONFIG_ENTRY_LIMIT));
        let entries = prepare_fastly_config_entries("root", &envelope_json).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        let chunk_map: HashMap<String, String> = entries[..entries.len().saturating_sub(1)]
            .iter()
            .cloned()
            .collect();

        let err = resolve_fastly_config_value(&root_key, pointer_json, |chunk_key| {
            Ok(chunk_map.get(chunk_key).cloned())
        })
        .expect_err("a reconstructed envelope with a bad inner sha must be rejected");
        assert!(
            !err.contains(SENTINEL),
            "the inner envelope hash must never reach a diagnostic: {err}"
        );
    }

    /// a fetch-callback failure carries the pointer-controlled
    /// chunk KEY. The resolver must locate the fault by position, not echo it.
    #[test]
    fn resolver_fetch_error_does_not_leak_chunk_key() {
        const SENTINEL: &str = "SUPER_SECRET_IN_A_CHUNK_KEY";
        let envelope = make_envelope_json(FASTLY_CONFIG_ENTRY_LIMIT.saturating_add(1));
        let entries = prepare_fastly_config_entries("root", &envelope).unwrap();
        let (root_key, pointer_json) = entries.last().unwrap().clone();
        // A VALID pointer (so metadata validation passes), whose first chunk key
        // the callback names in its failure — as the real store lookup would.
        let pointer: FastlyChunkPointer = serde_json::from_str(&pointer_json).unwrap();
        let secret_key = pointer.chunks[0].key.clone();

        let err = resolve_fastly_config_value(&root_key, pointer_json, |chunk_key| {
            Err(format!(
                "config store lookup failed for `{chunk_key}` -- {SENTINEL}"
            ))
        })
        .expect_err("a fetch failure must error");
        assert!(
            !err.contains(SENTINEL) && !err.contains(&secret_key),
            "a fetch failure must not echo the pointer-controlled chunk key: {err}"
        );
        assert!(
            err.contains("chunk 0") && err.contains("could not be fetched"),
            "the error must locate the fault by position: {err}"
        );
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
