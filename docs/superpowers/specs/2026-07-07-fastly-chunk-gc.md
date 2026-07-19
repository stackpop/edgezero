# Fastly chunked-config GC: reclaim orphaned chunk entries (operator-invoked)

## Motivation

When a Fastly Config Store value exceeds the 8 000-character entry
limit, `config push` splits it into content-addressed chunk entries
plus a root pointer, per the chunked-storage design
(`crates/edgezero-adapter-fastly/src/chunked_config.rs`). Chunk keys
are content-addressed by the envelope hash:

```
<root_key>.__edgezero_chunks.<envelope_sha256>.<idx>
```

**The problem, as it stood before this spec:** the push path was
**upsert-only** — both the cloud writer
(`FastlyCliAdapter::push_config_entries`) and the local writer
(`FastlyCliAdapter::push_config_entries_local` →
`write_fastly_local_config_store`) inserted-or-updated physical entries
and never deleted anything.

> **As built, this is now true of the CLOUD writer only.** A cloud `config
> push` still reclaims nothing (§4 explains why that cannot be made safe);
> the LOCAL writer prunes the prior generation's chunks eagerly in the same
> `fastly.toml` rewrite (§3), which is safe because it is a single file
> Viceroy reads at startup — no propagation window, no POPs.

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
- **Cloud** reclaims **nothing automatically** — a `config push` writes chunks
  then the pointer and stops. On an eventually-consistent store there is no safe
  automatic delete (three automatic designs were built and demolished; see
  "Cloud reclamation"). Reclamation is a separate, **operator-invoked** `config
  gc` that deletes only unreferenced chunks the operator has explicitly asserted
  are old enough to be safe.

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
   as `<root>.__edgezero_chunks.<64-char-lowercase-sha>.<canonical-decimal-index>`
   — nothing shorter, uppercased, or with a leading-zero index. Deletes target
   the store's ACTUAL keys — never keys re-derived from a content address.

4. **Destructive paths FAIL CLOSED.** If `config gc` cannot read the listing
   exactly (missing field), classify a root, validate a referenced key, read a
   `created_at`, or confirm the listing is complete, it aborts and deletes
   **nothing**. An unreadable state must never fail open into deletion.

5. **The operator's `--older-than` is the safety assertion, and it is REQUIRED
   for `--yes`.** The machine cannot know when a pointer stopped being served;
   the operator can. A destructive run must not guess it; a dry-run previews
   every key and age so the assertion is reviewable. It is **store-wide** (it
   covers every root in the swept store) and **may not be zero** — enforced at
   the destructive boundary itself (`gc_config_entries`), not only in the CLI,
   because the trait method is public and a rule that lives only in one caller
   is not a rule.

6. **`config gc` requires no concurrent `config push`.** No CAS means it cannot
   atomically observe-and-delete; the operator serialises it against pushes.

7. **A delete failure is a non-zero exit.** Independent generations are all
   attempted; deletion STOPS within a generation at its first failure (see
   invariant 11). If any delete fails, the command exits non-zero naming them, so
   automation can detect it.

8. **Local eager pruning is safe and stays.** One file, read at Viceroy startup:
   no propagation window, no POPs.

9. **Push failure semantics are unchanged.** Reclamation is never part of a
   push, so it can never fail one.

10. **A root is never deleted, whatever its key looks like — and root-vs-chunk
    is decided by VALUE, not key shape.** GC classifies any entry whose value is
    a pointer as a root, so a pointer parked at a chunk-shaped key still has its
    references counted live (round 9). Delete candidates must additionally
    round-trip to this writer's exact output. Push-time reserved-key rejection
    cannot protect entries that already exist.

11. **A generation is PLANNED and PROVEN atomically; physical deletion is not
    transactional.** A generation is provable only as a unit (its whole chunk set
    must round-trip), so the plan commits a generation whole or not at all. The
    STORE has no multi-key transaction, though, so the deletes themselves are
    sequential and a later one can fail after earlier siblings succeeded —
    leaving a partial generation on the platform. Deletion therefore stops at a
    generation's first failure to minimise that window. A
    failed remote delete has an UNKNOWN outcome — Fastly may commit it before
    returning an error — so nothing is promised as cleanly retryable: a failure
    with no confirmed prior sibling delete leaves the generation UNCERTAIN (a
    re-run may reclaim it, or surface it as an unprovable fragment), and a
    failure PART-WAY through (a sibling already confirmed deleted) strands the
    survivors permanently. Either way the command names the affected keys and the
    manual delete commands, and does NOT pretend a re-run is guaranteed to help.

12. **A failed delete is always a failure.** There is no "already gone" stderr
    special case: `not found`/`404` text cannot distinguish a missing key from a
    missing store, an auth failure or a 500, and reporting those as reclamation
    is worse than a retry. Retries are free — `gc` re-lists, so a key that is
    genuinely gone never becomes a candidate again.

13. **A diagnostic never quotes a stored value — on ANY path.** `serde_json`'s
    `Display` embeds the offending input; `BlobEnvelope::verify` names the stored
    hashes; and `chunks[].key` is pointer-controlled and unvalidated where it is
    reported. All are config- or attacker-supplied strings that may hold
    credentials, and these lines are logged verbatim. Diagnostics carry a chunk's
    POSITION and an error CATEGORY, never a stored string. This binds the runtime
    read path as much as GC.

14. **Untrusted metadata never sizes an allocation.** `envelope_len` and the
    per-chunk lengths come from the store. They are bounded against what the
    writer emits and summed with checked arithmetic, and no buffer is reserved
    from them — a pointer declaring `usize::MAX` would otherwise abort the
    process (including the edge guest, on the read path) before any check could
    reject it.

15. **The chunked read path verifies the INNER envelope, like the direct path,
    and validates pointer metadata before fetching.** The outer pointer checks
    (per-chunk and full-envelope hashes) prove the chunks reassemble to the bytes
    the pointer names, but say nothing about the `BlobEnvelope` inside. The
    resolver parses and `verify()`s the reconstructed envelope and returns only a
    redacted category — otherwise a reconstructed value with a wrong embedded
    `sha256` (a secret) reaches core, whose own `verify()` formats that stored
    hash into an HTTP 500. Both the resolver and core's extractor redact
    (including the typed-deserialize path, whose serde error otherwise quotes a
    field value into the response). The resolver ALSO runs the same pointer
    metadata validation the CLI/GC path uses (canonical keys, one generation,
    dense indexes, per-chunk length bound, checked sum), so a shape the writer
    could never emit is rejected up front rather than driving a fan-out of chunk
    fetches (invariant 14 holds on the read path, not just in GC).

> **Nested self-scoped pointers are handled.** A pointer parked at a chunk-shaped
> key whose own chunks are scoped to that key produces chunk keys containing the
> reserved infix twice. `chunk_key_generation_any` splits on the LAST infix, so
> those doubly-nested chunks are recognised as chunks (not misread as
> unclassifiable roots), the holder classifies as a root, its references are
> counted live, and store-wide GC continues rather than aborting.

16. **Root-vs-chunk is decided by VALUE, and any runtime-readable root is
    protected.** An entry whose value is a valid direct envelope OR a pointer is
    a root the runtime could read at that key, so it is never a delete candidate
    — even at a chunk-shaped key. A small envelope padded with trailing
    whitespace chunks so that chunk 0 is itself a complete, verifying envelope;
    protecting it (and thereby leaving its generation unreclaimed) is safe, and a
    real chunk fragment does not parse so is unaffected.

17. **Derived keys are validated against the store's key limit before I/O,
    counted in CHARACTERS.** A chunk key adds ~85 characters (infix + 64-char SHA
    + index) to the root, so a root that is itself valid can produce a physical
    key over Fastly's key limit. `prepare_fastly_config_entries` rejects any
    over-limit key up front (counting `chars()`, not UTF-8 bytes, so a non-ASCII
    `--key` is measured the way Fastly measures it), so a push never fails
    mid-write with some chunks committed. Fastly's docs disagree (guide 255, API
    reference 256); we take the stricter **255** so an emitted key is valid under
    either.

## Two further invariants (PR #314 review)

**Diagnostics must never echo config payloads.** The `describe` shell-outs
interpolated their raw stdout — i.e. the stored config envelope, which may hold
credentials — into errors that the CLI logs verbatim. Because the GC prior-read
runs on *every* cloud push, this exposed the previous config on any schema
drift. Errors now report only the response's **size and top-level field COUNT**
(never the field NAMES, which are provider/stored data) (`redact_describe_response`), never a value. Enforced by sentinel-secret
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
config gc --adapter fastly [--older-than <dur>] [--yes]
```

Dry-run by default; deletes only with `--yes`.

1. One `config-store-entry list --json`, parsed **fail-closed**: every entry
   must carry string `item_key`, `item_value`, and `created_at`, or the whole
   run aborts. A missing/empty field is never skipped or defaulted — skipping a
   root would hide the chunks it references and get them deleted while live.
2. Classify entries as roots by their **VALUE, not their key shape**: any entry
   whose value is a chunk pointer OR a valid direct envelope is a
   runtime-readable root, wherever it happens to live. For a pointer,
   `gc_classify_root` yields the **canonical** chunk keys it references, and the
   union over all roots is the **live** set; a direct-envelope root references no
   chunks but is itself protected from deletion. (The listing already carries
   `item_value`, so this costs no extra `describe` calls.)

   **Key shape is not authoritative for roots either** (round 9). The runtime
   resolver follows whatever pointer it is handed, so a valid pointer parked at a
   chunk-SHAPED key (`shadow.__edgezero_chunks.<sha>.0`) still makes its
   references live. Excluding chunk-shaped keys from the root scan up front —
   symmetric with the mistake this module already rejects for delete candidates —
   would leave those references looking orphaned and delete config that reads
   depend on. A chunk PAYLOAD is a raw envelope fragment and does not announce
   `edgezero_kind`, so it is never mistaken for a root.

   **`gc_classify_root`, not `prior_chunk_keys`.** `prior_chunk_keys` serves the
   push path, where an unrecognised value legitimately means "nothing to prune",
   so it answers `Ok([])`. Reusing it on a GC value would read that same answer
   as "references nothing" and orphan the root's own **live** chunks.
   `gc_classify_root` therefore does NOT call it: it independently deserialises
   the value, accepts only a valid direct envelope or a valid v1 pointer, runs
   `validate_pointer_chunks`, and errors on everything else (empty, truncated,
   unrelated) — it never yields the dangerous `Ok([])`.

   A pointer must also be **internally consistent**, not merely well-typed: a
   non-empty chunk list, one generation matching `envelope_sha256`, indexes
   exactly `0..n-1` (no gaps, duplicates or reordering), non-zero lengths summing
   to `envelope_len`. A pointer that parses but under-reports its chunks would
   silently shrink the live set.

3. **Delete only what is byte-identical to this writer's own output.** Entries
   are grouped by key shape, which is only trustworthy for a store we wrote. A
   store may predate this feature or be shared, and push-time reserved-key
   rejection cannot protect entries that already exist — so an ordinary root, or
   another tool's data, may sit at a chunk-shaped key.

   A candidate generation is reclaimable only if re-running
   `prepare_fastly_config_entries` over its reassembled bytes yields **exactly**
   those keys and values: same direct-vs-chunked threshold, same UTF-8-safe
   7 000-byte boundaries, same content-addressed keys, same count. A group that
   fails is **left untouched and reported** — not fatal, because one foreign
   entry must not block reclaiming the rest of the store forever.

   > **Content-addressing is NOT proof of authorship, and this spec does not
   > claim it is.** Hashing is not a signature: a foreign writer can pick
   > envelope E, compute `H = sha256(E)`, split E exactly as we would, and store
   > the parts under our reserved namespace. No preimage attack is involved, and
   > that group is byte-identical to ours — we cannot distinguish it and we will
   > reclaim it. Separating the two would need trusted generation metadata or an
   > authenticated marker, and this store offers neither (any writer with store
   > access could forge either, and there is nowhere to keep a key). **The
   > property we actually guarantee is: we never delete an entry that is not a
   > faithful reproduction of our own writer's output for the bytes it holds.**
   > The residual is accepted because the `.__edgezero_chunks.` namespace is
   > reserved by convention and push-time validation rejects logical keys inside
   > it.

4. **Duplicate keys fail closed.** A key is unique in a config store, so
   duplicate rows mean the listing is not one consistent view of it; last-row-wins
   could age a recent key into eligibility.
5. **Completeness guard (fail closed).** Every live-referenced key MUST appear in
   the listing. If one is absent, the listing is incomplete (e.g. paginated) or
   the store is inconsistent — either way we cannot decide what is orphaned, so
   we delete nothing.
6. **Supersession-age guard — best-effort defence in depth, NOT an independent
   safety proof.** For each root, the newest `created_at` among its *live* chunks
   approximates when its current config went live. When the live value **is** a
   chunked generation, that time is a real lower bound on when the root's orphans
   were superseded, so this catches the design-3 counterexample that a chunk's own
   age cannot see (B deployed seconds ago ⇒ the live generation is seconds old ⇒
   A's months-old chunks are retained).

   **Where it is blind — read this before trusting it.** A root whose live value
   is *direct* (or that is gone) has no live chunks and yields no signal at all;
   and a re-push that reuses an existing content-address writes no new chunk, so
   the newest live `created_at` can predate the transition. This guard therefore
   applies only as an **additional restriction on top of the chunk's own age** —
   never as a substitute for it, and never as grounds to relax `--older-than`.
   **The operator's `--older-than` assertion (§4) remains the only sound basis
   for any deletion.** Both ages must clear the window; the more restrictive
   (the minimum) wins.

**`--older-than` is the operator's safety assertion, and it is about the whole
PHYSICAL STORE** — *"NO root in this store changed within this window, and no
writer is targeting it, so nothing POPs may still be serving is deleted."*
`config gc` sweeps **every** root in the selected store, so the assertion must
cover every root in it, not just the config the operator has in mind. A sibling
root re-pushed minutes ago is enough to make a wide window unsafe — especially
if it changed to a value small enough to store directly, since that leaves no
live chunk for the supersession-age guard to date it by (see its blind spots
above). Only the operator can make this assertion, so:

- it is **REQUIRED for `--yes`** (a destructive run must not guess it);
- **`--older-than 0` is REJECTED for `--yes`.** A zero window asserts nothing: it
  makes every orphan eligible, including one superseded a second ago whose pointer
  POPs are still serving. Choose a window that is (a) at least Fastly's
  propagation time and (b) no longer than the time since ANY root in this store
  last changed —
  so the window you assert is one you actually observed;
- a **dry-run without it** previews *every* orphan and its age (threshold 0) so
  the operator can choose one from real data — previewing at zero is safe because
  a dry-run deletes nothing;
- a **dry-run with it** previews exactly what `--yes` would delete.

**Fails closed** on: an unreadable listing, a listing that is not a bare JSON
array (an `{"items":[...]}` envelope may carry pagination we do not follow, and a
page that omitted a root would make its live chunks look orphaned), an empty
`item_key`/`item_value`/`created_at`, a root value that is not *either* a valid
direct envelope *or* a valid pointer (an empty or truncated pointer must not read
as "references nothing"), a non-canonical referenced key, an unreadable
`created_at`, or an incomplete listing — all abort with nothing deleted.

**Non-zero exit on delete failure.** Independent generations are all attempted,
but deletion STOPS within a generation at its first failure (a half-deleted
generation can never be proved again). If any delete fails the command exits
non-zero naming the failed keys, so automation detects it. A failed remote delete
has an UNKNOWN outcome — Fastly may have committed it before returning an error —
so the command does not promise idempotent retry: it reports each affected
generation as either uncertain (re-run may reclaim it, or surface it as an
unprovable fragment) or definitely stranded (a confirmed sibling was already
deleted; manual removal only), with shell-safe recovery commands.

### Local is different, and eager pruning there is correct

`fastly.toml` is a single file Viceroy reads at startup: there is no propagation
window and no POP that could still be serving the previous pointer. The local
path prunes the prior generation immediately, inside the same rewrite.

## Concurrency model

The config value is last-writer-wins: the root is one entry and `update --upsert`
means the push whose pointer lands last defines the live config. Concurrent
pushes are supported.

Because a push **never deletes**, there is no push-time reclamation race.

**`config gc` requires that no `config push` runs against the same store while it
runs.** This is an explicit operational contract, not a nicety: Fastly offers no
compare-and-swap, so `config gc` cannot atomically observe-and-delete. It lists
once and deletes from that snapshot; a concurrent rollback that repoints the root
to old content-addressed chunks *after* the listing would leave `config gc`
deleting keys that are live again. The supersession-age and completeness guards
shrink this window sharply but cannot close it without CAS the platform does not
provide. The operator serialises `config gc` against pushes — the same party who
supplies `--older-than` is the one who knows a deploy is not in flight.

## `prior_chunk_keys` helper (`chunked_config.rs`)

**Local push path only.** It yields the chunk keys a pointer **references**,
validated and prefix-scoped to that root.

> **`config gc` does not call it at all** — see §"Reclamation algorithm". Its
> `Ok(vec![])`-for-unrecognised-values contract is right for a push (nothing to
> prune) and dangerous for a delete (it would read as "references nothing" and
> orphan a root's live chunks). GC classifies via `gc_classify_root`, which
> INDEPENDENTLY deserialises the value, validates it with `validate_pointer_chunks`,
> and adds content verification on top — it never delegates to `prior_chunk_keys`.
>
> The asymmetry is safe in the direction it fails: local pruning computes
> `prior − new`, so an under-reporting pointer prunes **less** (leaks, which is
> inert) rather than deleting something live. GC computes the live set, where the
> same under-reporting would delete a live chunk — which is why only GC pays for
> full content verification.

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
list                 # bare array, non-empty fields, no duplicate keys, else abort
live = union over roots of:
    gc_classify_root(root, root_value)          # valid envelope | valid pointer, else abort
    -> if chunked: reassemble its chunks from the listing and CHECK the bytes
       hash to envelope_sha256, else abort      # metadata alone is not proof
doomed = whole GENERATIONS of non-live chunk entries that
    (a) reassemble to the content-address their own keys name, AND round-trip
        through prepare_fastly_config_entries to EXACTLY these keys+values
        # i.e. byte-identical to this writer's own output -- NOT proof of
        # authorship; see the note in the invariants section
    (b) clear --older-than by BOTH their own age and their root's live-config age
    # a generation that does not round-trip is left UNTOUCHED and reported
delete doomed        # generations are PLANNED/PROVEN whole; physical delete is
                     # NOT transactional -- deletes are sequential and STOP at a
                     # generation's first
                     # failure (a half-deleted generation can never be proved
                     # again, so ploughing on strands its survivors for good)
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
    *count* degrades.) For this to be reachable, the CLI's diff read
    (`read_config_entry_local`) must ALSO degrade these to `Unsupported`
    rather than erroring — otherwise the orchestration aborts before the
    writer's count runs. A LOCAL `Unsupported` therefore takes the normal
    consent path (not the Spin-Cloud dry-run rejection).
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
- The adapter's push dry-run makes NO WRITE and NO delete, and runs no GC scan.
  It is NOT fully offline, though: the CLI does one LOGICAL read-back to render
  the diff, which for a chunked value is READ-ONLY but SEVERAL provider calls
  (describe the root pointer, then describe each referenced chunk to reassemble
  it) — not a single describe. An infeasible push is rejected by the body-aware
  preflight BEFORE that read.
- **Payload redaction**: schema-drift stdout *and* failing stderr never echo the
  stored value (sentinel-secret tests on both the read and push paths).

### `config gc` (`gc_config_entries`)

- **Never deletes a live chunk**, however old it is (referenced by a root
  pointer ⇒ untouchable).
- **Reclaims unreferenced chunks older than `--older-than`** — where "older"
  requires **BOTH** the chunk's own age **AND** (when known) its root's
  live-config age to clear the window; the more restrictive wins.
- **Retains unreferenced chunks younger than `--older-than`**, including a chunk
  written seconds ago under a root that has been stable for a year (a concurrent
  push may have written it and not yet committed its pointer). An old-looking root
  never licenses deleting a young chunk.
- **Rejects `--older-than 0` on `--yes`** — a zero window asserts nothing.
- **Dry-run** names every key + age it would delete, and deletes nothing.
- **Fails closed on an unreadable `created_at`** — aborts, deletes nothing.
- **Fails closed on an unclassifiable root** — aborts, deletes nothing.
- **Never deletes an entry that is not byte-identical to this writer's own
  output** — a candidate generation must reassemble to the content-address its
  keys name AND round-trip through `prepare_fastly_config_entries` to exactly
  those keys and values (which pins the split boundaries, the chunked-vs-direct
  threshold, and the count — a lone "chunk" can never pass). An entry that is
  merely chunk-SHAPED — plain text, unrelated JSON, someone's real config, or
  another tool's content-addressed data — is left untouched and reported, never
  deleted. This is NOT a proof of authorship; see §"Reclamation algorithm".
- **Fails closed when a live pointer under-reports its chunks** — a pointer that
  drops a chunk ref AND restates `envelope_len` to match passes every metadata
  check, so its chunks are reassembled and hashed against `envelope_sha256`.
- **Fails closed on a pointer whose chunk list is internally inconsistent** —
  an empty list, a gap/duplicate/reordering in the indexes, a mixed generation,
  or lengths that do not sum to `envelope_len`.
- **Fails closed on duplicate listing keys** — the listing is not one consistent
  view of the store.
- Delete argv passes `--key` + `--auto-yes` and **never** `--all`.
- **Every failed delete is a failure.** There is no "already gone" stderr special
  case: `not found`/`404` text cannot distinguish a missing key from a missing
  store, an auth failure or a 500, and reporting those as reclamation is worse
  than a retry.
- **Deletion stops at a generation's first failure.** A failed remote delete has
  an UNKNOWN outcome (Fastly may commit it before returning an error), so a
  failure with no confirmed prior sibling delete leaves the generation UNCERTAIN:
  a re-run may reclaim it if still whole, or surface it as an unprovable fragment
  if the delete did commit. `gc` re-lists, so this is safe to attempt.
- **A failure part-way through a generation strands its survivors, and `gc` says
  so.** They are an incomplete generation that can never be proved again, so `gc`
  will never reclaim them; the command names them and prints the manual delete
  commands rather than claiming a re-run will help. They are inert — no pointer
  references them.

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
| `crates/edgezero-adapter-fastly/src/cli.rs` (push) | `reject_reserved_root_keys`, `reject_duplicate_root_keys`, `expand_root` — a cloud push writes only, reclaiming nothing |
| `crates/edgezero-adapter-fastly/src/cli.rs` (`config gc`) | `gc_config_entries` → `gc_fastly_config_store`, `list_config_store_entries` (fail-closed field parsing), `chunk_key_generation_any`, `parse_rfc3339_secs`, `unix_now_secs`, `delete_config_store_entry` (`--key --auto-yes`, never `--all`) |
| `crates/edgezero-adapter/src/registry.rs` | `Adapter::gc_config_entries` trait method (default `Err`) |
| `crates/edgezero-cli/src/{args,config}.rs` | `ConfigGcArgs` (`--older-than` optional, required for `--yes`), `run_config_gc` |
| `crates/edgezero-cli/src/templates/cli/src/main.rs.hbs` | generated CLI wires the `Gc` subcommand |
| `crates/edgezero-adapter-fastly/src/cli.rs` (local) | `write_fastly_local_config_store` takes exact per-root keep-sets and prunes in the same rewrite; `local_contents_table` + best-effort dry-run counts |
| `crates/edgezero-adapter-fastly/src/cli.rs` (diagnostics) | `redact_describe_response` + `redact_stderr` — diagnostics never echo a config payload |
| `crates/edgezero-adapter-fastly/Cargo.toml` | `handlebars` dev-dependency (fake-`fastly` test shim) |
