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
(`crates/edgezero-adapter-fastly/src/cli/push_cloud.rs::write_entries`)
and the local writer
(`crates/edgezero-adapter-fastly/src/cli/push_local.rs::write_entries`
→ `provision_local::write_fastly_local_config_store`) insert-or-update
physical entries and never delete anything.

Because a config change changes the envelope bytes, it changes
`envelope_sha256`, which changes **every** chunk key. The new push
writes a fresh chunk set and overwrites the root pointer in place, so
reads stay correct — the live pointer only ever references the current
chunk set. But the **previous** generation's chunk entries are left
behind, unreferenced and unreclaimed:

- **Cloud**: orphan entries linger in the remote Config Store.
- **Local**: orphan keys linger in the `fastly.toml`
  `[local_server.config_stores.<name>.contents]` table (upserted at
  `provision_local.rs:559`; the existing code comment at
  `provision_local.rs:528` notes chunks "become unreferenced" but
  never deletes them).

Every chunked re-push leaks one generation of chunks. This spec adds
a **best-effort, post-commit garbage-collection sweep** that deletes
the chunk entries the *previous* root pointer referenced and the *new*
pointer no longer does.

## Scope

- Applies to the Fastly adapter's `config push` writeback path only
  (Stage 7), both cloud and `--local`.
- Deletes **chunk data entries** only. It NEVER deletes a root pointer
  key: the pointer at `<root_key>` is overwritten in place by the
  existing upsert and remains the stable, live entry.
- Does not touch sibling logical keys (e.g. `app_config` vs
  `app_config_staging`): the sweep is scoped per root key to the chunk
  keys that root's own previous pointer referenced.

## Terminology

- **Root key**: the stable logical entry key (e.g. `app_config`). Holds
  either a direct `BlobEnvelope` or a chunk pointer. Never GC'd.
- **Previous pointer**: the value stored at the root key *before* this
  push overwrites it.
- **Prior chunk keys**: `previous_pointer.chunks[].key` — the exact
  chunk entries the previous generation referenced. Empty if the
  previous value was a direct envelope, missing, or not a pointer.
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

2. **Delete only what the previous pointer referenced.** The delete-set
   is derived from `previous_pointer.chunks[].key`, not from a prefix
   scan or a store-wide enumeration. This guarantees the sweep can only
   ever touch keys that were part of this root's own prior generation.

3. **A key in the new keep-set is never deleted.** Even though a SHA
   change makes overlap empty in practice, the sweep subtracts the
   keep-set unconditionally so an identical-bytes re-push is a no-op.

4. **GC failure never fails or blocks the push.** The push's success
   criterion is unchanged: new chunks + new pointer committed. The
   sweep is optimistic — any read/parse/delete failure degrades to a
   warning folded into the returned status lines; the push still
   reports success. A leaked chunk is harmless; a failed push is not.

## Algorithm

For each logical `(root_key, envelope_json)` entry in the push:

```
# --- before writing ---
prev        = read_root_value(root_key)          # may be absent
prior_keys  = prior_chunk_keys(prev)             # [] unless prev is a pointer
expanded    = prepare_fastly_config_entries(root_key, envelope_json)
new_keys    = { k for (k, _) in expanded } ∪ { root_key }

# --- write (existing behaviour, unchanged) ---
push expanded            # chunks first, root pointer last  (commit point)

# --- sweep (new, best-effort, only after push succeeds) ---
for k in prior_keys − new_keys:
    try delete_entry(k)  except e: warn("failed to delete orphan chunk `{k}`: {e}")
```

`prior_chunk_keys` reuses the existing pointer schema: parse the prior
value as a `FastlyChunkPointer`; if it parses and
`edgezero_kind == POINTER_KIND`, return `chunks[].key`; otherwise
return `[]`. A direct `BlobEnvelope` fails to parse as a pointer and
yields `[]` — correct, since a direct value has no chunks. This means
the **shrink-to-direct** case (config drops back under 8 000 chars) is
handled for free: the new value is direct, `new_keys = { root_key }`,
and every prior chunk key is swept.

## Cloud path (`push_cloud.rs`)

- **Read prior value**: reuse `fetch_remote_config_store_entry(store_id,
  root_key)` (already present for chunk resolution). Returns
  `Ok(Some(raw))`, `Ok(None)` (absent → no prior chunks), or `Err`
  (degrade to warning, skip GC for that root).
- **Store id**: `write_entries` already resolves `resolved_id` via
  `resolve_remote_config_store_id`. The prior-value read must move to
  after that resolution and before the committer loop. Because the read
  needs the store id, hoist `resolve_remote_config_store_id` above the
  per-root prior-value reads (it currently sits just before the commit
  loop — move it up; the dry-run branch returns before it, unchanged).
- **Delete helper**: new `delete_config_store_entry(store_id, key)`
  shelling `fastly config-store-entry delete --store-id=<id>
  --key=<key>`, mirroring the spawn/stderr handling of
  `create_config_store_entry`. Treat a "not found" / "does not exist" /
  "404" stderr as success (already gone).
- **Cost**: one extra `describe` per logical root before the push, plus
  one `delete` per orphan after. No store-wide `list` call.

## Local path (`push_local.rs` / `write_fastly_local_config_store`)

The local `contents` table is held entirely in memory in a single
`DocumentMut`, so GC here is fully reliable and needs no round-trips:

- Before the upsert loop at `provision_local.rs:559`, read each root
  key's current value from `contents_tbl` (the prior pointer), compute
  `prior_chunk_keys`, and after inserting the new physical entries,
  `contents_tbl.remove(k)` for each orphan `k`.
- Because the whole file is rewritten atomically by the trailing
  `fs::write`, steps 1–3 of the ordering invariant collapse into a
  single write — there is no partial-commit window locally, so the
  local sweep cannot leak or corrupt.
- The signature of `write_fastly_local_config_store` takes only
  `entries` today. To compute `prior_chunk_keys` it must inspect the
  pre-existing `contents_tbl` for each root key present in `entries`;
  this is available in-function (the table is read before the upsert
  loop). No new parameters are required — the prior values are read
  from the table being edited.

## Dry-run

Both paths already have a dry-run branch that reports direct-vs-chunked
intent per key. Extend it: for each root key, report how many orphan
chunks *would* be deleted. The cloud dry-run reads the prior value
(same `fetch`) to count; if that read fails during dry-run, report
"unknown (prior read failed)" rather than erroring. Example lines:

```
  would push `app_config` as chunked (12 chunks + 1 pointer, 84210B total)
  would delete 9 orphan chunks from the previous generation of `app_config`
```

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

## Testing

Reuse the existing fake-`fastly` shim harness in `push_cloud.rs` tests
and the tempdir/`DocumentMut` harness in `provision_local.rs` /
`push_local.rs` tests.

### `chunked_config.rs` unit tests (`prior_chunk_keys`)

- Pointer value → returns its `chunks[].key` in order.
- Direct `BlobEnvelope` value → returns `[]`.
- Unparseable / unrelated JSON → returns `[]`.
- Pointer with non-matching `edgezero_kind` → returns `[]`.

### Cloud (`push_cloud.rs`)

- **Deletes prior, keeps new**: fake fastly serves a prior pointer for
  the root; after push, assert `config-store-entry delete` is invoked
  for each prior chunk key and NOT for any new chunk key or the root
  key.
- **Shrink-to-direct**: prior value is a chunk pointer, new value is
  direct (≤ 8 000 chars); assert all prior chunk keys are deleted and
  the root key is upserted (not deleted).
- **First push (no prior)**: prior read returns not-found; assert zero
  delete calls.
- **Identical re-push**: prior pointer and new pointer reference the
  same keys; assert zero delete calls (keep-set subtraction).
- **Delete failure degrades to warning**: delete shell-out exits
  non-zero with a non-"not found" stderr; assert the push still returns
  success and the status includes a warning naming the failed chunk key.
- **Prior read failure degrades to warning**: `describe` on the root
  errors (non-not-found); assert push succeeds, GC skipped, warning
  present.
- **Ordering**: extend the existing argv-log fake to assert every
  `delete` argv appears strictly after the root-pointer `update` argv.

### Local (`push_local.rs` / `provision_local.rs`)

- **Prunes orphan chunk keys from `contents`**: seed a `fastly.toml`
  whose `contents` table holds a prior pointer + its chunk keys; push a
  changed (still-chunked) config; assert the new chunk keys are present,
  the prior chunk keys are absent, and sibling logical keys are
  untouched.
- **Shrink-to-direct locally**: prior chunked → new direct; assert all
  prior chunk keys removed and root key holds the direct envelope.
- **Sibling coexistence preserved**: a push of `app_config` must not
  remove `app_config_staging` chunk keys (per Spec 12.7 coexistence).

## Files touched

| File | Change |
| --- | --- |
| `crates/edgezero-adapter-fastly/src/chunked_config.rs` | Add `pub(crate) fn prior_chunk_keys(&str) -> Vec<String>` + unit tests |
| `crates/edgezero-adapter-fastly/src/cli/push_cloud.rs` | Hoist store-id resolution; per-root prior read; `delete_config_store_entry`; post-commit sweep; dry-run counts; tests |
| `crates/edgezero-adapter-fastly/src/cli/push_local.rs` | Dry-run orphan counts; wire sweep into local write; tests |
| `crates/edgezero-adapter-fastly/src/cli/provision_local.rs` | `write_fastly_local_config_store` prunes orphan chunk keys after upsert; tests |
