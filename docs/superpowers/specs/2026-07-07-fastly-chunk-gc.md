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

1. **Nothing is deleted until the new root pointer is committed.** The pointer
   write is the cutover: before it lands, the live pointer still references the
   prior chunks. Deletes run strictly after the commit.

   (Locally, this is one in-memory `DocumentMut` rewrite followed by the single
   existing `fs::write` — not a durable atomic rename, but from GC's perspective
   there is no partial-commit window: the new entries and the removals land in
   the same rewrite.)

2. **The just-superseded generation is NEVER deleted by the push that
   supersedes it (cloud).** Fastly's config store is eventually consistent: POPs
   may still be serving the previous pointer. This is unconditional — it holds
   even with a zero grace window. Local is exempt: one file, no POPs.

3. **Only validated, root-scoped keys are ever delete candidates.** A key must
   parse as `<root>.__edgezero_chunks.<hex-sha>.<index>` for that exact root.
   Deletes target the store's **actual** keys — never keys re-derived from a
   content address — so a hand-edited pointer cannot retarget a delete onto keys
   it never referenced.

4. **A key in the live keep-set is never deleted.** An identical-bytes re-push
   is therefore a no-op.

5. **GC failure never fails or blocks the push.** Any read/list/parse/delete
   failure degrades to a warning; the push still succeeds. Reclamation is
   **stateless and idempotent**, so a later push simply recomputes and retries.
   A leaked chunk is harmless; a failed push is not.

6. **Cloud reclamation obeys last-writer-wins.** It runs only while a post-commit
   read-back confirms the root still holds exactly what this push wrote. If a
   newer push has superseded us, we yield — costing nothing, because there is no
   state to go stale.

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

## Cloud reclamation: store-derived and grace-gated

> **Design history (PR #314 review).** Two earlier revisions were wrong and are
> recorded here so the mistakes are not repeated.
>
> 1. **Eager deletion** (delete the superseded generation right after the
>    commit, guarded by a read-back) is **unsafe**. Fastly Config Store is
>    eventually consistent and the read-back observes only the **control
>    plane**. After we write pointer N, POPs may still serve pointer N-1, which
>    references N-1's chunks. Deleting them immediately strips chunks from those
>    POPs — breaking reads on *every* re-push, not merely under concurrency.
>    Knowing *what* to delete was never the problem; knowing *when* it is safe is.
> 2. **A metadata sidecar** (record the superseded generation, reclaim it later)
>    is **unsound**. Fastly has no compare-and-swap, so a failed sidecar write, a
>    failed read-back, or a concurrent read-modify-write **permanently loses** a
>    generation, which can never be rediscovered. And the record itself overflows
>    the 8 000-char entry limit at ~71 generations, after which every subsequent
>    generation is silently lost.

The design carries **no metadata at all — the store IS the state.**

**Verified against the live Fastly API:**

- `fastly config-store-entry list --store-id=<id> --json` returns, per item,
  `item_key` and **`created_at`** (also `updated_at`, `store_id`, `item_value`).
- **The root entry's own `updated_at` is NOT usable.** Fastly does **not** bump
  it on `update --upsert`. Observed live: a root whose `updated_at` read
  `2026-07-07` was pointing at chunks created `2026-07-13`. Using it as the
  supersession clock would conclude the current generation had been stable for
  six days and reclaim the previous one immediately — the exact unsafe outcome
  we are trying to avoid.
- Chunk entries' own `created_at` **is** accurate and monotonic.

**The supersession clock, derived from the store.** A generation is superseded
exactly when **the next one is written**. So, ordering generations by
`created_at`:

```
superseded_at(G) = created_at( successor(G) )
```

with no successor meaning the generation was never referenced by any pointer
(e.g. a partially-committed push), in which case it ages from its own creation.

**Per root, after the commit:**

```
1. LWW read-back: does the root still hold exactly what we wrote?   (else yield)
2. list the store; group THIS root's actual chunk keys by content address
   -> generations, each with created_at = max(created_at of its chunks)
3. protected = keys(N, what we just wrote) ∪ keys(N-1, what we just superseded)
4. for each generation G, in created_at order:
       if keys(G) ∩ protected ≠ ∅            -> skip (live, or still at POPs)
       if now - superseded_at(G) < GRACE     -> skip (inside the grace window)
       else delete G's ACTUAL keys from the listing
```

Three properties this buys:

- **N-1 is always protected**, unconditionally, regardless of the grace window.
  That is the eventual-consistency guarantee: a POP still serving the previous
  pointer keeps its chunks.
- **Deletes target the store's real keys**, never keys re-derived from a content
  address — so a hand-edited pointer whose SHA and key suffixes disagree can
  never retarget a delete onto keys the pointer never referenced.
- **The pre-existing backlog is reclaimed for free.** Orphans leaked before this
  feature existed are ordinary unreferenced generations; no tracking was needed.

**Grace window:** `EDGEZERO_FASTLY_GC_GRACE_SECS`, default **86 400 (24h)**.
Fastly documents no propagation bound, so the default is deliberately generous;
operators on a faster cadence can lower it.

**Key-shape validation.** `chunk_key_generation` only recognises
`<root>.__edgezero_chunks.<hex-sha>.<index>`. A foreign or malformed key is
never grouped, and therefore never becomes a delete target.

**Failure handling.** A failing `list`, a failing read-back, a suspicious prior
pointer, or a failing delete all degrade to a warning; the push still succeeds
and a later push simply retries (reclamation is idempotent and stateless — there
is nothing to lose).

**The LOCAL path prunes eagerly** and that is correct: `fastly.toml` is a single
file Viceroy reads at startup — no propagation window, no POPs.

## Concurrency model: last-writer-wins

The config value is last-writer-wins: the root key is one entry and
`update --upsert` means the push whose pointer lands last defines the live
config. Concurrent pushes are supported.

Reclamation only runs while a post-commit read-back confirms the root still
holds exactly what this push wrote; if a newer push has superseded us, we yield
and let that push reclaim. Because the design is **stateless**, yielding costs
nothing — there is no record to go stale, and the next push recomputes
everything from the store.

Residual (stated honestly): Fastly exposes no compare-and-delete, so a newer
push could in principle intervene between our read-back and a delete. The blast
radius is bounded by the same three gates that govern every delete — the key
must be unreferenced by the live pointer, not in the just-superseded generation,
and older than the grace window. GC is a best-effort reclaimer, not a
transactional one, and a missing chunk surfaces as an integrity error on read
rather than wrong data.

## `prior_chunk_keys` helper (`chunked_config.rs`)

Unchanged from the original design and still load-bearing: it yields the
just-superseded generation's keys (validated, prefix-scoped to the root) which
the cloud path adds to `protected`, and which the local path prunes against.

- `Ok(keys)` — a valid v1 chunk pointer, keys prefix-matched to this root.
- `Ok(vec![])` — a direct `BlobEnvelope`, absent, or not pointer-shaped (silent).
- `Err(msg)` — pointer-*kind* but invalid (bad version, foreign-prefix key):
  warn and reclaim nothing for that root.

Parsed `serde_json::Value`-first, so a pointer-kind value with missing fields
reaches the warning path instead of being silently dropped.

## Algorithm

```
# validate first (both paths), before any expansion or I/O
reject if any logical key contains CHUNK_KEY_INFIX      # reserved namespace
reject if any logical key appears more than once        # see the duplicate-root invariant

for each logical (root_key, body):
    expanded       = prepare_fastly_config_entries(root_key, body)   # this root only
    new_keys       = { k for (k, _) in expanded }                    # includes root_key
    new_root_value = value of expanded.last()

# --- cloud ---
prior_keys = prior_chunk_keys(root, read(root))     # BEFORE the commit
push expanded                                       # chunks first, pointer last (commit)
reclaim_orphan_generations(root, new_keys, new_root_value, prior_keys, now, grace)

# --- local ---
prune orphans (prior_keys - new_keys) inside the same fastly.toml rewrite
```

## Cloud path (`push_config_entries`)

- **Dry-run stays offline** — no store-id resolution, no remote read. It reports
  reclamation intent without a count (the count depends on the listing).
- **Pre-commit:** read each root's prior pointer → `prior_keys` (the generation
  about to be superseded). `Err` → warn, reclaim nothing for that root.
- **Commit:** unchanged — all roots' physical entries in one committer loop.
- **Post-commit:** `reclaim_orphan_generations` per root, as specified above.
- **Delete helper:** `fastly config-store-entry delete --store-id --key
  --auto-yes`. Only ever `--key`; the subcommand also accepts `-a/--all`, which
  would wipe the whole store — never construct that flag.
- **Cost:** one `describe` per root pre-commit, one read-back per root, one
  `list` per root post-commit, plus one `delete` per reclaimed chunk.

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

### Cloud (`push_config_entries`)

- **Never reclaims the just-superseded generation** — even with `GRACE=0`. This
  is the eventual-consistency guarantee and the single most important test.
- **Reclaims an aged orphan generation**: an older generation, superseded before
  the prior one and past the grace window, IS deleted; the live and
  just-superseded generations survive.
- **Retains an orphan inside the grace window**: nothing is deleted.
- **Yields when the root changed**: a differing read-back → no deletes, warning.
- **Identical re-push**: the sole generation is both the keep-set and the prior
  → doubly protected, no deletes.
- **Shrink-to-direct**: the whole chunk set is orphaned, but the just-superseded
  generation is still retained.
- **Suspicious prior pointer** / **prior-read failure** / **listing failure**:
  warn, delete nothing, push still succeeds.
- **Delete failure**: warns, push succeeds.
- **Delete argv**: every delete passes `--key` + `--auto-yes` and **never**
  `--all` (store-wide wipe).
- **Dry-run stays offline**: no `list`/`describe`/`delete`, reports intent only.
- **Payload redaction**: schema-drift stdout *and* failing stderr never echo the
  stored value (sentinel-secret tests on both the read and push paths).

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
| `crates/edgezero-adapter-fastly/src/chunked_config.rs` | `prior_chunk_keys` (validated, prefix-scoped) and `chunk_key_generation` (parses/validates a chunk key into its content address) + unit tests |
| `crates/edgezero-adapter-fastly/src/cli.rs` (cloud) | `reject_reserved_root_keys`, `reject_duplicate_root_keys`, `expand_root`, `list_config_store_entries`, `parse_rfc3339_secs`, `reclaim_orphan_generations`, `delete_config_store_entry` (`--key --auto-yes`, never `--all`), `gc_grace_secs` / `unix_now_secs` |
| `crates/edgezero-adapter-fastly/src/cli.rs` (local) | `write_fastly_local_config_store` takes exact per-root keep-sets and prunes in the same rewrite; `local_contents_table` + best-effort dry-run counts |
| `crates/edgezero-adapter-fastly/src/cli.rs` (diagnostics) | `redact_describe_response` + `redact_stderr` — diagnostics never echo a config payload |
| `crates/edgezero-adapter-fastly/Cargo.toml` | `handlebars` dev-dependency (fake-`fastly` test shim) |
