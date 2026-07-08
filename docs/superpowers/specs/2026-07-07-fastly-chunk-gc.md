# Fastly chunked-config GC: reclaim orphaned chunk entries on re-push

## Motivation

When a Fastly Config Store value exceeds the 8 000-character entry
limit, `config push` splits it into content-addressed chunk entries
plus a root pointer, per the chunked-storage design
(`crates/edgezero-adapter-fastly/src/chunked_config.rs`). Chunk keys
are content-addressed by the envelope hash:

```
<root_key>.__edgezero_chunks.<envelope_sha256>.<idx>
```

The push path is **upsert-only**. Both the cloud writer
(`FastlyCliAdapter::push_config_entries`, `cli.rs:349`) and the local
writer (`FastlyCliAdapter::push_config_entries_local`, `cli.rs:421` →
`write_fastly_local_config_store`, `cli.rs:926`) insert-or-update
physical entries and never delete anything.

Because a config change changes the envelope bytes, it changes
`envelope_sha256`, which changes **every** chunk key. The new push
writes a fresh chunk set and overwrites the root pointer in place, so
reads stay correct — the live pointer only ever references the current
chunk set. But the **previous** generation's chunk entries are left
behind, unreferenced and unreclaimed:

- **Cloud**: orphan entries linger in the remote Config Store.
- **Local**: orphan keys linger in the `fastly.toml`
  `[local_server.config_stores.<name>.contents]` table (upserted in
  `write_fastly_local_config_store` at `cli.rs:926`, which reads the
  doc, inserts each physical entry, and never removes stale keys).

Every chunked re-push leaks one generation of chunks. This spec adds
a **best-effort, post-commit garbage-collection sweep** that deletes
the chunk entries the *previous* root pointer referenced and the *new*
pointer no longer does.

> **Code-layout note.** This spec targets the layout on `main`, where
> the Fastly adapter is monolithic (`cli.rs` + `chunked_config.rs`).
> An in-flight branch (`feature/provision-local-impl`) splits `cli.rs`
> into `cli/push_cloud.rs`, `cli/push_local.rs`, and
> `cli/provision_local.rs`. If that branch merges first, the same
> changes land in those files instead — the functions and logic are
> identical, only the file boundaries move. Rebase the implementation
> onto whichever layout is current.

## Scope

- Applies to the Fastly adapter's `config push` writeback path only
  (Stage 7), both cloud and `--local`.
- Deletes **chunk data entries** only. It NEVER deletes a root pointer
  key: the pointer at `<root_key>` is overwritten in place by the
  existing upsert and remains the stable, live entry.
- Does not touch sibling logical keys (e.g. `app_config` vs
  `app_config_staging`): the delete-set is scoped, per root key, to
  chunk keys that match that root's own generated chunk prefix AND were
  named by that root's previous pointer.

## Terminology

- **Root key**: the stable logical entry key (e.g. `app_config`). Holds
  either a direct `BlobEnvelope` or a chunk pointer. Never GC'd. Root
  keys are store-config ids and MUST NOT contain the reserved chunk
  infix `.__edgezero_chunks.` (they never do — they are validated
  identifiers, and `prepare_fastly_config_entries` only ever appends
  the infix to form chunk keys).
- **Previous pointer**: the value stored at the root key *before* this
  push overwrites it.
- **Prior chunk keys**: the keys named by `previous_pointer.chunks[]`,
  *after* validation and prefix filtering (see `prior_chunk_keys`).
  Empty if the previous value was a direct envelope, missing, or not a
  valid v1 pointer.
- **New keep-set**: the physical keys this push writes for that root —
  the new chunk keys plus the root key itself.
- **Orphans**: `prior_chunk_keys − new_keep_set`. In practice this is
  all prior chunk keys, because a changed config changes the SHA and
  therefore every chunk key; but the set-difference is the correct,
  race-safe formulation (a re-push of *identical* bytes re-derives the
  same keys and deletes nothing).

## Invariants (MUST)

1. **Nothing is deleted until the new root pointer is committed.** The
   root pointer write is the atomic cutover: before it lands, the live
   pointer still references the prior chunks; after it lands, the prior
   chunks are unreachable. Deletes run strictly after the pointer
   commits.

   Ordering per root key:

   1. Write all new chunk entries (new-SHA keys) — upsert
   2. Write the new root pointer — upsert ← **commit point**
   3. Delete `prior_chunk_keys − new_keep_set` — best-effort

   (Locally, steps 1–3 collapse into one atomic `fs::write` of the
   whole `fastly.toml`, so there is no partial-commit window and the
   local sweep can neither leak nor corrupt.)

2. **Delete only validated, prefix-matched prior references.** The
   delete-set is derived from `previous_pointer.chunks[].key`, filtered
   to keys with the exact prefix `"{root_key}.__edgezero_chunks."`, and
   only when the previous pointer is a valid v1 pointer. It is NOT
   derived from a prefix scan or a store-wide enumeration. This bounds
   the blast radius: a malformed or hand-edited pointer can never cause
   GC to delete a key outside its own root's chunk namespace.

3. **A key in the new keep-set is never deleted.** Even though a SHA
   change makes overlap empty in practice, the sweep subtracts the
   keep-set unconditionally so an identical-bytes re-push is a no-op.

4. **GC failure never fails or blocks the push.** The push's success
   criterion is unchanged: new chunks + new pointer committed. The
   sweep is optimistic — any read/parse/delete failure degrades to a
   warning folded into the returned status lines; the push still
   reports success. A leaked chunk is harmless; a failed push is not.

## `prior_chunk_keys` helper (`chunked_config.rs`)

Add next to the existing `FastlyChunkPointer` schema
(`chunked_config.rs:47`):

```rust
/// Validate a prior root value and return the chunk keys it referenced,
/// scoped to `root_key`'s own chunk namespace. Used only for GC.
///
/// Returns:
/// - `Ok(keys)`  — value is a valid v1 chunk pointer; `keys` are its
///   `chunks[].key` entries that match `"{root_key}.__edgezero_chunks."`.
/// - `Ok(vec![])` — value is a direct `BlobEnvelope`, absent, or not
///   pointer-shaped at all (normal: first push, or was-direct). Silent.
/// - `Err(msg)`  — value IS pointer-kind (`edgezero_kind == POINTER_KIND`)
///   but fails validation: unsupported `version`, or a referenced key
///   falls outside this root's chunk prefix. Caller logs `msg` as a
///   warning and skips GC for this root (deletes nothing).
pub(crate) fn prior_chunk_keys(
    root_key: &str,
    raw: &str,
) -> Result<Vec<String>, String>;
```

Rules:

- Attempt to deserialize `raw` into `FastlyChunkPointer`. On failure
  (missing fields — e.g. a direct `BlobEnvelope`, or unrelated JSON) →
  `Ok(vec![])`, silent.
- If it deserializes but `edgezero_kind != POINTER_KIND` → `Ok(vec![])`,
  silent (not our pointer).
- If `edgezero_kind == POINTER_KIND` but `version != 1` → `Err(...)`,
  warn.
- Compute `prefix = format!("{root_key}{CHUNK_KEY_INFIX}")`. If every
  `chunks[].key` starts with `prefix` → `Ok(matching keys)`. If any key
  does NOT match the prefix → `Err(...)`, warn, and return no keys (do
  not partially delete a suspicious pointer).

Unit tests: valid pointer → its keys; direct envelope → `Ok([])`;
unrelated JSON → `Ok([])`; wrong `edgezero_kind` → `Ok([])`; `version`
≠ 1 → `Err`; a chunk key with a foreign prefix → `Err`.

## Algorithm

For each logical `(root_key, envelope_json)` entry in the push:

```
# --- before writing ---
prev        = read_root_value(root_key)          # may be absent
prior       = prior_chunk_keys(root_key, prev)   # Ok([]) unless prev is a valid v1 pointer
expanded    = prepare_fastly_config_entries(root_key, envelope_json)
new_keys    = { k for (k, _) in expanded } ∪ { root_key }

# --- write (existing behaviour, unchanged) ---
push expanded            # chunks first, root pointer last  (commit point)

# --- sweep (new, best-effort, only after push succeeds) ---
match prior:
    Err(msg): warn(msg)                                  # suspicious pointer; skip GC
    Ok(keys):
        for k in keys − new_keys:
            try delete_entry(k) except e: warn("failed to delete orphan chunk `{k}`: {e}")
```

The **shrink-to-direct** case (config drops back under 8 000 chars) is
handled for free: the new value is a direct envelope,
`new_keys = { root_key }`, and every prior chunk key is swept.

## Cloud path (`push_config_entries`, `cli.rs:349`)

- **Dry-run stays offline.** The `dry_run` early return (`cli.rs:385`)
  is *before* `resolve_remote_config_store_id` (`cli.rs:410`) and must
  stay that way — dry-run never resolves the store id and never fetches
  remote state. It reports GC intent without a count, e.g.
  `"  would delete orphaned prior-generation chunks of `app_config` (count determined at push time)"`.
  (Rationale: counting orphans needs the store id + a remote read,
  which would make dry-run hit the network. Not worth it; the local
  dry-run below gives an exact count offline for the common case.)
- **Read prior value (real push only).** After `resolve_remote_config_store_id`
  succeeds, for each logical root call the existing
  `fetch_remote_config_store_entry(store_id, root_key)` (`cli.rs:671`)
  to get `prev`. `Ok(None)` → no prior chunks; `Err` → warn, skip GC
  for that root. This read happens *before* the committer loop.
- **Delete helper.** New `delete_config_store_entry(store_id, key)`
  shelling `fastly config-store-entry delete --store-id=<id>
  --key=<key> --auto-yes`, mirroring the spawn/stderr handling of
  `create_config_store_entry` (`cli.rs:1073`). `--auto-yes` suppresses
  any interactive confirmation (non-interactive shell-out). Treat a
  "not found" / "does not exist" / "404" stderr as success (already
  gone).
- **Sweep after commit.** After `push_entries_with_committer`
  (`cli.rs:1022`) returns `Ok`, run the per-root sweep. Fold delete
  failures and prior-read/parse warnings into the returned status
  `Vec<String>` (the push still returns `Ok`).
- **Cost**: one extra `describe` per logical root before the push, plus
  one `delete` per orphan after. No store-wide `list` call.

## Local path (`push_config_entries_local` / `write_fastly_local_config_store`)

The local `contents` table is held entirely in memory in a single
`DocumentMut`, so GC here is fully reliable and needs no round-trips or
new threaded metadata. The writer already receives the flattened
`physical_entries`; it recovers logical roots without extra structure:

- **Root inference.** A physical entry is a **root** iff its key does
  NOT contain the reserved infix `CHUNK_KEY_INFIX`
  (`.__edgezero_chunks.`). Every chunk key contains it; every root key
  (direct or pointer) does not. This holds because
  `prepare_fastly_config_entries` is the only producer of these keys
  and only appends the infix to chunk keys. (Guarded by the root-key
  invariant in Terminology.)
- **Sweep inside the atomic write.** In `write_fastly_local_config_store`
  (`cli.rs:926`), before the upsert loop, snapshot each root's *old*
  value from the existing `contents_tbl`. After inserting the new
  physical entries, for each inferred root compute
  `prior_chunk_keys(root, old_value)` and `contents_tbl.remove(k)` for
  each orphan `k` in `prior − new_keys`. All within the one
  `DocumentMut` that the trailing `fs::write` persists.
- **Warnings.** `prior_chunk_keys` returning `Err` for a root → collect
  the message; the local push returns it as an extra status line. A
  removal is infallible on an in-memory table, so the only local
  warnings are suspicious-pointer ones.
- **Dry-run (offline, exact count).** `push_config_entries_local`'s
  dry-run (`cli.rs:459`) already reads no remote state. Extend it to
  read the current `fastly.toml`, compute `prior_chunk_keys` per root,
  and report the exact orphan count, e.g.
  `"  would delete 9 orphan chunks from the previous generation of `app_config`"`.
  If the file is absent or a root has no prior pointer, report 0.

## Existing tests that MUST change

- `second_oversized_push_converges_runtime_on_new_envelope`
  (`cli.rs:3317`). Today it asserts old A-generation chunks **remain**
  in the local `contents` table after push B (loop at `cli.rs:3416`
  asserting `chunks_b.contains(chunk_key)`, and the "no GC in v1"
  comments at `cli.rs:3303`/`:3324`/`:3360`/`:3405`/`:3414`). With GC
  this inverts: after push B, every A-chunk MUST be **absent**
  (`!chunks_b.contains(...)`), B-chunks present, and the runtime read
  still reconstructs envelope B. Update the assertions and rewrite the
  stale "no GC" commentary.

## New tests

Reuse the fake-`fastly` shim + tempdir/`DocumentMut` harnesses already
in the `cli.rs` test module.

### `chunked_config.rs` — `prior_chunk_keys`

- Valid v1 pointer → its `chunks[].key` in order.
- Direct `BlobEnvelope` → `Ok([])`.
- Unrelated / unparseable JSON → `Ok([])`.
- `edgezero_kind` ≠ `POINTER_KIND` → `Ok([])`.
- `version` ≠ 1 → `Err`.
- A `chunks[].key` outside `"{root_key}.__edgezero_chunks."` → `Err`,
  returns no keys.

### Cloud (`push_config_entries`)

- **Deletes prior, keeps new**: fake fastly serves a prior pointer for
  the root; after push assert `config-store-entry delete --auto-yes` is
  invoked for each prior chunk key and NOT for any new chunk key or the
  root key.
- **Shrink-to-direct**: prior is a chunk pointer, new value is direct
  (≤ 8 000 chars) → all prior chunk keys deleted, root key upserted
  (not deleted).
- **First push (no prior)**: prior read returns not-found → zero delete
  calls.
- **Identical re-push**: prior and new reference the same keys → zero
  delete calls.
- **Delete failure degrades to warning**: delete exits non-zero with a
  non-"not found" stderr → push returns `Ok`, status includes a warning
  naming the failed chunk key.
- **Prior read failure degrades to warning**: `describe` on the root
  errors (non-not-found) → push `Ok`, GC skipped, warning present.
- **Suspicious pointer degrades to warning**: prior value is
  pointer-kind with `version` 2 → push `Ok`, no deletes, warning.
- **Ordering**: extend the argv-log fake to assert every `delete` argv
  appears strictly after the root-pointer `update` argv.
- **Dry-run stays offline**: dry-run makes no `list`/`describe`/`delete`
  calls and prints the no-count GC intent line.

### Local (`push_config_entries_local` / `write_fastly_local_config_store`)

- **Prunes orphan chunk keys**: seed a `fastly.toml` whose `contents`
  holds a prior pointer + its chunks; push a changed (still-chunked)
  config → new chunk keys present, prior chunk keys absent, sibling
  logical keys untouched. (This is the inverted `cli.rs:3317` test.)
- **Shrink-to-direct locally**: prior chunked → new direct → all prior
  chunk keys removed, root holds the direct envelope.
- **Sibling coexistence preserved**: a push of `app_config` must not
  remove `app_config_staging` chunk keys (Spec 12.7 coexistence).
- **Dry-run exact count**: reports the correct orphan count and writes
  nothing.

## Non-goals

- **Reclaiming pre-existing leaks.** This sweep only deletes the
  generation the *current* live pointer references. Chunks orphaned by
  pushes that predate this feature (or by a prior sweep that partially
  failed) are not enumerated and not reclaimed. Steady state going
  forward: each push cleans the immediately-prior generation, so at most
  one stale generation exists between pushes. A one-time full reclaim
  (prefix-scan the store and delete unreferenced `__edgezero_chunks`
  keys) is deferred to a possible future `config gc` command and is out
  of scope here.
- **Transactional multi-key GC.** Each root key is swept independently;
  there is no cross-key atomicity beyond what each path already
  provides (per-entry for cloud, whole-file for local).

## Files touched

| File | Change |
| --- | --- |
| `crates/edgezero-adapter-fastly/src/chunked_config.rs` | Add `pub(crate) fn prior_chunk_keys(root_key, raw) -> Result<Vec<String>, String>` (validates v1 + prefix-scopes) + unit tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (`push_config_entries`, `:349`) | Per-root prior read via `fetch_remote_config_store_entry`; `delete_config_store_entry` helper (`--auto-yes`); post-commit sweep; offline dry-run GC-intent line; tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (`push_config_entries_local`, `:421`) | Dry-run exact orphan counts; tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (`write_fastly_local_config_store`, `:926`) | Infer roots (no chunk infix), snapshot old values, prune orphans in the same atomic write |
| `crates/edgezero-adapter-fastly/src/cli.rs` (test `:3317`) | Invert: assert old chunks are deleted after re-push; drop "no GC in v1" commentary |
