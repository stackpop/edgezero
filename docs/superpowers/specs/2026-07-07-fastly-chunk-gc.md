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

Every chunked re-push leaks one generation of chunks. This spec reclaims them:

- **Local** prunes eagerly, inside the same `fastly.toml` rewrite. Safe: one
  file, read by Viceroy at startup — no propagation window.
- **Cloud** reclaims orphaned generations that are unreferenced by the live
  pointer AND have been superseded for longer than a grace window, deriving
  both facts from the store itself. It is **never** safe to delete the
  just-superseded generation immediately (see "Cloud reclamation").

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
  `app_config_staging`): every candidate key must parse as
  `<root>.__edgezero_chunks.<hex-sha>.<index>` for *that* root, so the shared
  string prefix of a sibling root never matches.

## Terminology

- **Root key**: the stable logical entry key (e.g. `app_config`). Holds
  either a direct `BlobEnvelope` or a chunk pointer. Never GC'd. The
  root key is the logical store-config id, or the operator's
  `--key <override>` — a free-form `Option<String>` (`args.rs:311`,
  used directly in `config.rs:297`). GC never infers roots by
  string-matching the chunk infix; roots come from the caller's logical
  entries. In addition, the Fastly adapter **MUST reject** (hard error,
  not warning) any logical key containing the reserved infix
  `.__edgezero_chunks.`, at the top of both `push_config_entries` and
  `push_config_entries_local`, before any expansion or I/O. Such a key
  collides with the generated chunk namespace and has no valid use; a
  push must not be able to write into another key's chunk space. The
  check lives at the Fastly adapter boundary (the infix is a
  Fastly-specific concept), not in generic `edgezero-cli`.
- **Previous pointer**: the value stored at the root key *before* this
  push overwrites it.
- **Prior chunk keys**: the keys named by `previous_pointer.chunks[]`,
  *after* validation and prefix filtering (see `prior_chunk_keys`).
  Empty if the previous value was a direct envelope, missing, or not a
  valid v1 pointer.
- **New keep-set**: the physical keys this push writes for that root —
  the new chunk keys plus the root key itself. It is built by expanding
  **that root's own** `(root_key, body)` via
  `prepare_fastly_config_entries` and taking those keys — NOT by
  prefix-scanning the flattened multi-root physical set (which would
  reintroduce infix inference and mis-handle shared prefixes or a
  free-form key).
- **Generation**: the set of chunk entries sharing one content address
  (`envelope_sha256`). Chunk keys are grouped into generations by parsing them.
- **Orphan generation**: a generation not referenced by the **live** root
  pointer. Cloud reclaims an orphan only once it has also aged past the grace
  window; local prunes `prior_chunk_keys − new_keep_set` immediately.

## Invariants (MUST)

1. **A cloud push never deletes.** It writes chunks, then the pointer. Any
   reclamation is a separate, operator-invoked `config gc`.

2. **Nothing is deleted that a live pointer references.** `config gc` computes
   the live set by parsing every root's pointer. Local prunes only
   `prior_chunk_keys − new_keep_set`.

3. **Only canonical, root-scoped keys are delete candidates.** A key must parse
   as `<root>.__edgezero_chunks.<hex-sha>.<index>`. Deletes target the store's
   ACTUAL keys — never keys re-derived from a content address.

4. **Destructive paths FAIL CLOSED.** If `config gc` cannot classify a root, or
   cannot read an entry's `created_at`, it aborts and deletes **nothing**. An
   unreadable state must never fail open into deletion.

5. **The operator's `--older-than` is the safety assertion.** The machine cannot
   know when a pointer stopped being served; the operator can. A `--dry-run`
   prints every key and age it would delete so that assertion is reviewable.

6. **Local eager pruning is safe and stays.** One file, read at Viceroy startup:
   no propagation window, no POPs.

7. **Push failure semantics are unchanged.** Reclamation is never part of a
   push, so it can never fail one.

## Two further invariants (PR #314 review)

**Diagnostics must never echo config payloads.** The `describe` shell-outs
interpolated their raw stdout — i.e. the stored config envelope, which may hold
credentials — into errors that the CLI logs verbatim. Because the GC prior-read
runs on *every* cloud push, this exposed the previous config on any schema
drift. Errors now report only the response's **size and top-level field-name
shape** (`redact_describe_response`), never a value. Enforced by sentinel-secret
tests on both the read and push paths.

**A batch must not name the same logical root twice.** GC builds one plan per
entry and snapshots every plan against the *same* prior generation. With
`[(root, A), (root, B)]` the last tuple wins the upsert (root = B), yet A's plan
would still reclaim `prior − A_keys` — which includes B's freshly-written chunks
— leaving the final pointer dangling. Duplicate root keys are therefore a **hard
error** in both push paths, before any expansion or I/O. (Rejecting beats
silently coalescing: a duplicated key is a caller bug, and picking a winner would
hide it.)

## Cloud reclamation: NOT automatic. `config gc`, invoked by the operator.

> **This is the single most important section. Three automatic designs were
> built and each was demolished in review. They are recorded so nobody tries a
> fourth.**
>
> 1. **Eager delete** (remove the superseded generation right after the commit,
>    guarded by a read-back) — **unsafe**. The store is eventually consistent and
>    the read-back only observes the **control plane**. POPs may still be serving
>    the previous pointer, which references the chunks being deleted. This breaks
>    reads on *every* re-push, not merely under concurrency.
> 2. **Metadata sidecar** (record the superseded generation; reclaim it later) —
>    **unsound**. Fastly has no compare-and-swap, so a failed write, a failed
>    read-back, or a concurrent push **permanently loses** a generation; and the
>    record overflows the 8 000-char entry limit at ~71 generations.
> 3. **Store-derived clock** (`superseded_at(G) = created_at(successor(G))`) —
>    **unsound**. Chunk creation is not a pointer transition. Counterexample:
>    chunked **A** → direct **B** → direct **C**. During C, A is the only chunk
>    generation listed, so it has *no successor* and ages from its own creation —
>    and is deleted, though B superseded it seconds ago and POPs may still serve
>    A. Partial pushes and chunk/pointer write gaps break it identically.

**The impossibility, stated plainly.** To delete a chunk safely you must know
that *the pointer which referenced it has stopped being served everywhere, for
longer than the propagation window*. Fastly:

- **does not record** that fact — `updated_at` is **not** bumped by
  `update --upsert` (verified against the live API: a root reading `updated_at =
  2026-07-07` was pointing at chunks created `2026-07-13`);
- **offers no CAS** with which we could record it ourselves safely;
- and its chunk `created_at` **is not a proxy** for it (design 3 above).

The fact simply is not available to the machine. **It is available to the
operator**, who knows their own deploy history. So the operator supplies it.

### What a cloud `config push` does

**It reclaims nothing.** It writes chunks, then the pointer. That is all. Cloud
storage therefore accretes orphaned generations exactly as it does today — this
is **not a regression**, and it is the only safe automatic behaviour.

### `config gc` (`Adapter::gc_config_entries`)

```
config gc --adapter fastly [--older-than <dur>] [--dry-run]
```

1. One `config-store-entry list --json`.
2. Classify every non-chunk entry as a **root**; parse its value with
   `prior_chunk_keys` → the chunk keys that root's pointer **references**. The
   union over all roots is the **live** set. (The listing already carries
   `item_value`, so this costs no extra `describe` calls.)
3. Candidates = chunk entries **not** in the live set, whose `created_at` is
   older than `--older-than`.
4. Delete them.

**`--older-than` is the operator's safety assertion**: *"nothing created before
this is still being served."* Only they can make it.

**It fails CLOSED.** If a root's value cannot be classified, or an entry's
`created_at` cannot be read, `config gc` **aborts and deletes nothing** — an
unreadable state must never fail open on a destructive path.

**A `--dry-run` prints every key it would delete, with its age**, so the
assertion is reviewable before it is acted on.

Only ever `--key` on delete; the subcommand also accepts `-a/--all`, which would
wipe the store — never construct that flag.

### Local is different, and eager pruning there is correct

`fastly.toml` is a single file Viceroy reads at startup: there is no propagation
window and no POP that could still be serving the previous pointer. The local
path prunes the prior generation immediately, inside the same rewrite.

## Concurrency model

The config value is last-writer-wins: the root is one entry and `update --upsert`
means the push whose pointer lands last defines the live config. Concurrent
pushes are supported.

Because a push **never deletes**, there is no push-time reclamation race to
reason about at all. `config gc` is a separate, operator-timed action; it deletes
only what no live pointer references and what the operator has asserted is old
enough. Running it concurrently with a push is still the operator's call — the
`--older-than` assertion is what makes it safe.

## `prior_chunk_keys` helper (`chunked_config.rs`)

Load-bearing in both paths: it yields the chunk keys a pointer **references**,
validated and prefix-scoped to that root.

- `Ok(keys)` — a valid v1 chunk pointer.
- `Ok(vec![])` — a direct `BlobEnvelope`, absent, or not pointer-shaped (silent).
- `Err(msg)` — pointer-*kind* but invalid (bad version, foreign-prefix key).

Parsed `serde_json::Value`-first, so a pointer-kind value with missing fields
reaches the error path instead of being silently dropped. In `config gc` an
`Err` here **aborts the whole run** (we cannot know what that root references).

`chunk_key_generation` recognises only the canonical shape
`<root>.__edgezero_chunks.<hex-sha>.<index>`, so a foreign or hand-edited key is
never a delete candidate.

## Algorithm

```
# validate first (both push paths), before any expansion or I/O
reject if any logical key contains CHUNK_KEY_INFIX      # reserved namespace
reject if any logical key appears more than once        # duplicate-root invariant

for each logical (root_key, body):
    expanded = prepare_fastly_config_entries(root_key, body)   # this root only
    new_keys = { k for (k, _) in expanded }                    # includes root_key

# --- cloud push ---
commit expanded (chunks first, pointer last).  NO deletes.

# --- local push ---
prune (prior_chunk_keys - new_keys) inside the same fastly.toml rewrite

# --- config gc (operator) ---
list -> live = union of prior_chunk_keys(root, root_value) over all roots
doomed = chunk entries not in live, older than --older-than
delete doomed        # fails closed on any unclassifiable/unreadable state
```

## Local path (`push_config_entries_local` / `write_fastly_local_config_store`)

The local `contents` table is held entirely in memory in a single
`DocumentMut`, so GC here is fully reliable and needs no round-trips.
Per-root keep-sets are computed from each root's own expansion and
passed to the writer explicitly — never inferred from the flattened set:

- **Pass exact per-root keep-sets (no inference).**
  `push_config_entries_local` expands each logical `(root_key, body)` via
  `prepare_fastly_config_entries` (once, reused for `physical_entries`)
  and passes `gc_roots: &[(String, HashSet<String>)]` — each root with
  its own exact new-key set — into `write_fastly_local_config_store`
  alongside `physical_entries`. No string-matching on the chunk infix; a
  `--key` containing `.__edgezero_chunks.` is separately rejected up
  front (Terminology). An empty `gc_roots` means "no GC" (setup-only
  writers pass `&[]`).
- **Sweep inside the single rewrite.** In `write_fastly_local_config_store`
  (`cli.rs:926`), before the upsert loop, snapshot each root's *old*
  value from the existing `contents_tbl`. After inserting the new
  physical entries, for each `(root, new_keys)` compute
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

### Cloud push (`push_config_entries`)

- **Never deletes anything** — a push writes chunks + pointer, nothing else.
- Reserved-key and duplicate-root keys are hard errors before any I/O.
- Dry-run stays offline (no `list`/`describe`/`delete`).
- **Payload redaction**: schema-drift stdout *and* failing stderr never echo the
  stored value (sentinel-secret tests on both the read and push paths).

### `config gc` (`gc_config_entries`)

- **Never deletes a live chunk**, however old it is (referenced by a root
  pointer ⇒ untouchable).
- **Reclaims unreferenced chunks older than `--older-than`.**
- **Retains unreferenced chunks younger than `--older-than`.**
- **Dry-run** names every key + age it would delete, and deletes nothing.
- **Fails closed on an unreadable `created_at`** — aborts, deletes nothing.
- **Fails closed on an unclassifiable root** — aborts, deletes nothing.
- Delete argv passes `--key` + `--auto-yes` and **never** `--all`.

### Local (`push_config_entries_local` / `write_fastly_local_config_store`)

- **Prunes orphan chunk keys**: seed a `fastly.toml` whose `contents`
  holds a prior pointer + its chunks; push a changed (still-chunked)
  config → new chunk keys present, prior chunk keys absent, sibling
  logical keys untouched. (This is the inverted `cli.rs:3317` test.)
- **Shrink-to-direct locally**: prior chunked → new direct → all prior
  chunk keys removed, root holds the direct envelope.
- **Sibling coexistence preserved**: a push of `app_config` must not
  remove `app_config_staging` chunk keys (Spec 12.7 coexistence).
- **Suspicious prior pointer (real push)**: seed the root with a
  pointer-kind-but-invalid value (e.g. `version: 2`) **and** pre-seed
  real chunk-like keys under the root namespace (so "no deletes" is
  non-vacuous); a real local push of a new config returns a
  suspicious-pointer warning, leaves the pre-seeded chunk keys present,
  and still writes the new value.
- **Reserved key rejected**: `push_config_entries_local` with a logical
  key containing `.__edgezero_chunks.` returns `Err` before touching
  `fastly.toml` (file unchanged / not created).
- **Dry-run count**: reports the correct orphan count and writes
  nothing.
- **Dry-run identical re-push counts 0**: seed a chunked config, then
  dry-run the SAME bytes → reports `0` orphans (regression for computing
  `new_keys` from the expanded, not logical, entries).
- **Dry-run degrades, never fails**: a malformed `fastly.toml` makes the
  dry-run report `unknown` for the GC count while still printing the
  direct-vs-chunked intent lines and returning `Ok`.
- **Dry-run suspicious prior pointer**: seed the root with a
  pointer-kind-but-invalid value; the dry-run reports
  `(unknown: suspicious prior pointer)` for that root and still returns
  `Ok` without writing.

## Non-goals

- **Automatic cloud reclamation.** Out of scope because it is not achievable
  safely (see "Cloud reclamation"). Cloud orphans — including those that predate
  this feature — are reclaimed by the operator running `config gc`, which is IN
  scope and implemented.
- **Transactional multi-key GC.** Each root key is swept independently;
  there is no cross-key atomicity beyond what each path already
  provides (per-entry for cloud, whole-file for local).

## Files touched

| File | Change |
| --- | --- |
| `crates/edgezero-adapter-fastly/src/chunked_config.rs` | `prior_chunk_keys` (validated, prefix-scoped) and `chunk_key_generation` (parses/validates a chunk key into its content address) + unit tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (cloud) | `reject_reserved_root_keys`, `reject_duplicate_root_keys`, `expand_root`, `list_config_store_entries`, `parse_rfc3339_secs`, `reclaim_orphan_generations`, `delete_config_store_entry` (`--key --auto-yes`, never `--all`), `gc_grace_secs` / `unix_now_secs` |
| `crates/edgezero-adapter-fastly/src/cli.rs` (local) | `write_fastly_local_config_store` takes exact per-root keep-sets and prunes in the same rewrite; `local_contents_table` + best-effort dry-run counts |
| `crates/edgezero-adapter-fastly/src/cli.rs` (diagnostics) | `redact_describe_response` + `redact_stderr` — diagnostics never echo a config payload |
| `crates/edgezero-adapter-fastly/Cargo.toml` | `handlebars` dev-dependency (fake-`fastly` test shim) |
