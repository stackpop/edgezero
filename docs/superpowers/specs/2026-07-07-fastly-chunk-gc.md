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
  either a direct `BlobEnvelope` or a chunk pointer. Never GC'd. The
  root key is the logical store-config id, or the operator's
  `--key <override>` — a free-form `Option<String>` (`args.rs:311`,
  used directly in `config.rs:297`). Because `--key` is **not**
  validated today, GC does NOT infer roots by string-matching the chunk
  infix; it threads the logical root keys explicitly (see Local path).
  As defence-in-depth, the implementation SHOULD also reject a `--key`
  containing the reserved infix `.__edgezero_chunks.` at push time,
  since such a key would collide with the generated chunk namespace.
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

   (Locally, steps 1–3 happen as one in-memory `DocumentMut` rewrite
   followed by the existing single `fs::write` of `fastly.toml`
   (`cli.rs:999`). That write is a plain overwrite, NOT a durable
   atomic rename — but from GC's perspective there is no partial-commit
   window: the new entries and the orphan removals land in the same
   rewrite, so a reader never sees new chunks without the new pointer,
   nor a swept chunk that is still referenced.)

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

Rules — parse as `serde_json::Value` **first**, so a pointer-kind value
with missing or invalid fields still reaches the warning path instead
of being silently dropped by a failed struct deserialize (e.g.
`{"edgezero_kind":"fastly_config_chunks","version":2}` has no `chunks`
array and would fail a direct `FastlyChunkPointer` deserialize):

1. Parse `raw` as `serde_json::Value`. If that fails, or the top-level
   `edgezero_kind` field is absent or `!= POINTER_KIND` → `Ok(vec![])`,
   silent. (Direct `BlobEnvelope`, unrelated JSON, or a first push.)
2. The value IS pointer-kind. From here every failure is `Err(...)`
   (warn, delete nothing):
   - required fields missing / wrong types (no `chunks` array,
     `version` not an integer, …), or
   - `version != 1`, or
   - any `chunks[].key` does NOT start with
     `format!("{root_key}{CHUNK_KEY_INFIX}")`.
3. Otherwise → `Ok(keys)`, the `chunks[].key` values (all prefix-matched
   by step 2).

Unit tests: valid pointer → its keys; direct envelope → `Ok([])`;
unrelated JSON → `Ok([])`; wrong `edgezero_kind` → `Ok([])`; pointer-kind
with missing `chunks` and `version:2` → `Err` (NOT silently `Ok([])`);
`version` ≠ 1 → `Err`; a chunk key with a foreign prefix → `Err`.

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
  any interactive confirmation (non-interactive shell-out). Only ever
  pass `--key`: the subcommand also accepts `-a/--all`, which deletes
  **every** entry in the store — never construct that flag. Treat a
  "not found" / "does not exist" / "404" stderr as success (already
  gone).
- **A failed delete is informational, not actionable.** The push has
  already succeeded and the new pointer is live, so an un-deleted orphan
  is inert. Word the warning accordingly (e.g. "could not reclaim orphan
  chunk … ; it is inert and will be removed by a future `config gc`") —
  do NOT imply the operator should retry the push. There is no reclaim
  command in v1, so such an orphan persists as a pre-existing leak (see
  Non-goals) until that future tool ships.
- **Sweep after the whole commit.** The cloud writer flattens every
  logical root's physical entries into one `physical_entries` vec
  (`cli.rs:380`) and commits them in a single `push_entries_with_committer`
  loop (`cli.rs:411`, `:1022`). Prior-value reads for every root happen
  *before* that loop; the per-root sweep runs only *after* it returns
  `Ok` — i.e. after ALL roots' chunks + pointers are committed. Fold
  delete failures and prior-read/parse warnings into the returned status
  `Vec<String>` (the push still returns `Ok`).
- **Partial-commit failure ⇒ no GC.** If the committer fails partway,
  `push_config_entries` returns `Err` and no sweep runs for any root.
  Safe by construction: nothing is deleted, so the store is left with
  (at worst) a mix of old and new chunks, each still referenced by
  whichever pointer actually committed. Reclamation waits for the next
  successful push. Leak-safe over corrupt-safe, per invariant 4.
- **Cost**: one extra `describe` per logical root before the push, plus
  one `delete` per orphan after. No store-wide `list` call. Deletes are
  sequential `fastly` subprocess spawns — a large prior generation
  (e.g. 50 chunks) adds ~50 sequential shell-outs of post-push latency.
  Fastly exposes no bulk delete-by-key (`--all` is store-wide), so v1
  accepts this cost rather than parallelising spawns.

## Local path (`push_config_entries_local` / `write_fastly_local_config_store`)

The local `contents` table is held entirely in memory in a single
`DocumentMut`, so GC here is fully reliable and needs no round-trips.
To stay sound against a free-form `--key`, the logical root keys are
threaded explicitly rather than inferred from the flattened entries:

- **Thread logical roots (no inference).** `push_config_entries_local`
  already holds the logical `entries: &[(String, String)]`. Pass the
  logical root keys into `write_fastly_local_config_store` via a new
  parameter (e.g. `roots: &[&str]`) alongside `physical_entries`. No
  string-matching on the chunk infix — a `--key` that happens to
  contain `.__edgezero_chunks.` cannot mislead root detection.
- **Sweep inside the single rewrite.** In `write_fastly_local_config_store`
  (`cli.rs:926`), before the upsert loop, snapshot each threaded root's
  *old* value from the existing `contents_tbl`. After inserting the new
  physical entries, for each root compute
  `prior_chunk_keys(root, old_value)` and `contents_tbl.remove(k)` for
  each orphan `k` in `prior − new_keys`. All within the one
  `DocumentMut` that the trailing `fs::write` (`cli.rs:999`) persists —
  one in-memory rewrite, then one write.
- **Warnings.** `prior_chunk_keys` returning `Err` for a root → collect
  the message; the local push returns it as an extra status line. A
  removal is infallible on an in-memory table, so the only local
  warnings are suspicious-pointer ones.
- **Dry-run (offline, best-effort count).** `push_config_entries_local`'s
  dry-run (`cli.rs:459`) reads no remote state. Extend it to read the
  current `fastly.toml` and, per root, compute the orphan count as
  `prior_chunk_keys(root, old_value) − new_keys`, where `new_keys` is
  the **expanded** key set (`prepare_fastly_config_entries(root, body)`
  keys ∪ `{root}`) — the same expansion the real push does. Expanding
  is required: using the logical key alone would over-count an
  identical-bytes re-push (the new chunk keys would be missing from
  `new_keys`, making every prior chunk look like an orphan; the correct
  answer is `0`). Report e.g.
  `"  would delete 9 orphan chunks from the previous generation of `app_config`"`.
  Error semantics — the dry-run MUST NOT newly fail where it succeeds
  today (the current dry-run does not read the file at all), so GC
  counting degrades rather than erroring:
  - File absent, or a root has no prior pointer / a direct prior value
    → report `0`.
  - File present but unreadable, malformed TOML, `contents` not a table,
    or a root's value not a string → report
    `"would delete an unknown number of orphan chunks from the previous generation of `app_config` (unknown: could not read prior state)"`
    for that root and continue. (The real push still fails fatally on
    malformed TOML via the writer at `cli.rs:938`; only the dry-run
    *count* degrades.)
  - Prior value is pointer-kind-but-invalid (`prior_chunk_keys` → `Err`)
    → report the same line with `(unknown: suspicious prior pointer)`.

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
- **Free-form `--key` with the infix**: threading roots (not inference)
  means a root key containing `.__edgezero_chunks.` is still handled
  correctly — its own prior chunks are swept, siblings untouched.
- **Dry-run count**: reports the correct orphan count and writes
  nothing.
- **Dry-run identical re-push counts 0**: seed a chunked config, then
  dry-run the SAME bytes → reports `0` orphans (regression for computing
  `new_keys` from the expanded, not logical, entries).
- **Dry-run degrades, never fails**: a malformed `fastly.toml` makes the
  dry-run report `unknown` for the GC count while still printing the
  direct-vs-chunked intent lines and returning `Ok`.

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
| `crates/edgezero-adapter-fastly/src/cli.rs` (`push_config_entries_local`, `:421`) | Thread logical roots to the writer; best-effort dry-run orphan counts (degrade to "unknown", never newly fail); tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (`write_fastly_local_config_store`, `:926`) | New `roots: &[&str]` param; snapshot old values, prune orphans in the same in-memory rewrite before the existing `fs::write` (`:999`) |
| `crates/edgezero-adapter-fastly/src/cli.rs` (test `:3317`) | Invert: assert old chunks are deleted after re-push; drop "no GC in v1" commentary |
| `crates/edgezero-cli/src/config.rs` (`:297`) / `args.rs` (`:311`) — optional, defence-in-depth | Reject a `--key` containing `.__edgezero_chunks.` at push time (collides with the chunk namespace) |
