# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — revised twice after review

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

A deploy performs up to **two cold Rust compiles**, back to back, neither cached
by default:

1. **`build-app-cli`** compiles the application's own CLI package (native)
   `--release` into a throwaway `mktemp` `CARGO_TARGET_DIR`, with **no caching of
   any kind** (registry, git, or `target/`). Every deploy recompiles the whole
   dependency graph — the dominant cost (~10 min for
   `stackpop/trusted-server-deployer`).
2. **`deploy-fastly`** compiles the app to `wasm32-wasip1`; its `target/` cache is
   opt-in and only active under `build-mode: always`.

The lifecycle actions (`healthcheck`/`rollback`/`config-push`) compile nothing and
are out of scope. This spec adds caching to `build-app-cli`, the largest cost and
the one a downstream deployer cannot fix on its own.

**A note on build vs. buy.** Robustly caching Rust builds in CI is genuinely hard
— path stability, key composition, cache cleaning, private-source trust — which is
why `Swatinem/rust-cache` exists. This repo has chosen an in-house exact-key cache
(pin policy, no unpinned third-party actions). This spec re-derives the necessary
invariants explicitly; a future decision to adopt a pinned third-party cache would
subsume most of §3, and is recorded as an alternative in §9.

## 2. Goals and non-goals

**Goals**

- Give `build-app-cli` **opt-in** caching that reuses unchanged dependency
  compilation across revisions.
- Make the trust boundary **enforceable by the action's control flow**, not just
  documented.
- Reuse `deploy-fastly`'s in-house cache mechanics and a shared key derivation.

**Non-goals**

- No changes to the lifecycle actions.
- No new third-party cache action (see §9 for the recorded alternative).
- No deployer-specific config in shipped docs (per-repo tuning, e.g. for
  `trusted-server-deployer`, is delivered separately after implementation).
- No change to the default WASM `build-mode`.

## 3. Design

### 3.1 Stable paths — the generation lives in the KEY, never the path

`actions/cache` folds the `path` set into an internal cache **version**; a restore
hits only when **both** the key/restore-prefix **and** the version (paths) match.
Varying a path per run or per generation therefore **defeats every cross-run
restore**. Cargo compounds this: fingerprint/dep-info files embed **absolute
paths**, so relocating `CARGO_TARGET_DIR`/`CARGO_HOME` invalidates reuse even
locally.

Therefore the cached directories are at **fixed, stable paths** for every
invocation and every revision:

- `EDGEZERO__CACHE__ROOT = $RUNNER_TEMP/edgezero-build-cache` (a stable string),
- `CARGO_HOME = $EDGEZERO__CACHE__ROOT/cargo-home`,
- `CARGO_TARGET_DIR = $EDGEZERO__CACHE__ROOT/target`.

All generation/identity distinctions live in the cache **key** (§3.3), never in the
path. (This replaces today's per-invocation `mktemp` target dir; the isolation it
provided is re-established by the cross-step ownership protocol in §3.9.)

### 3.2 Two caches, with Cargo's recommended Cargo-home layout

**(a) Dependency cache.** Caches only the parts of `CARGO_HOME` that are
verified/regenerable per Cargo's own CI guidance — **`registry/index`,
`registry/cache`, and `git/db`** — and **excludes the extracted `registry/src` and
`git/checkouts`**. This matters: extracted sources and Git checkouts contain
`build.rs`/proc-macro code, so caching them would restore **executable input** from
an unsigned archive. `.crate` files in `registry/cache` are re-verified by checksum
on extraction and `git/db` is re-checked-out, so this layout is safe to restore
without the target cache's trusted-input requirement. Keyed exactly on the lockfile
(§3.3); populated completely by a `cargo fetch --locked` phase (§3.7) **before**
save, so an incomplete Cargo home is never frozen.

**(b) Target cache.** Caches `CARGO_TARGET_DIR`, which **is** trusted executable
input (compiled artifacts and build-script outputs from a prior run). It is
governed by the writer/reader trust boundary in §3.10. Keyed per generation
(§3.3), restored by prefix (§3.4).

### 3.3 Cache keys — separate grammars, exact

Extract `resolve-project.sh`'s key derivation into a shared `deploy-core` helper so
`build-app-cli` and `deploy-fastly` cannot drift. Free-form components are hashed;
every key begins with a **cache-schema version** constant so the format can be
rotated centrally.

**Shared identity prefix `P`** (both caches), in this fixed order:

```
edgezero-<schema>-<cache-kind>-<os>-<arch>-<host-target-triple>
  -<toolchain-channel>-<rustc-commit-hash>
  -<app-repo-id>-<workspace-id>-<app-cli-package>-<app-cli-bin>-<cli-profile>
  -<suffix-hash>
```

- `cache-kind` is literally `deps` or `target` — the two caches never share a key
  space.
- `app-repo-id` (the application Git remote/root identity) and `workspace-id` (the
  hash of the Cargo workspace root's path relative to the app Git root, as the
  deploy key already does) prevent two different checked-out apps at the same
  deployer path, or nested workspaces, from colliding.
- `app-cli-package` **and** `app-cli-bin` are both included: two packages can expose
  the same binary name.
- `suffix-hash` is the SHA-256 (length-bounded input) of `cache-key-suffix` (§3.12).

**Dependency key** = `P(deps)` + `-<Cargo.lock-hash>`. Exact; no restore-prefix.

**Target key** = `P(target)` + `-<Cargo.lock-hash>` + `-<source-revision>`, with
`source-revision` as the **final** component and
**`restore-keys = P(target)-<Cargo.lock-hash>-`** (the same string minus the final
revision). Because the revision is last, dropping it yields a true prefix.

**Field matrices** (also the test contract, §4):

| Key changes when…                                                            | `deps` key |                    `target` key                     |
| ---------------------------------------------------------------------------- | :--------: | :-------------------------------------------------: |
| schema/os/arch/host-triple/toolchain/rustc/repo/workspace/package/bin/suffix |     ✓      |                          ✓                          |
| `Cargo.lock` content                                                         |     ✓      |                          ✓                          |
| `cli-profile`                                                                |     ✓      |                          ✓                          |
| `source-revision`                                                            |     —      | ✓ (final component; absent from the restore prefix) |

### 3.4 Restore semantics — latest accessible, no ancestry

GitHub restore-key prefix matching returns the **most recently created, accessible,
version-matching** cache — it has **no Git-ancestry awareness**. So a target-cache
restore yields "the latest accessible compatible generation," which may come from a
concurrent run, a retry, a sibling branch, or the default branch — **not
necessarily the immediate parent commit**. Tag-scoped caches are further isolated
by tag, so successive release tags do **not** form a rolling chain.

The spec therefore claims only: a build restores _some_ compatible prior generation
when one is accessible, and **records the matched key** (an action output +
summary line) for observability. It asserts **no** parent/ancestry relationship,
and correctness never depends on which generation restores (a wrong-but-compatible
`target/` only costs extra recompilation, never incorrect output — the exact
`target` key on save still identifies this revision's generation).

### 3.5 Committed-source guard (new, and a pre-existing gap)

`build-app-cli` today builds immediately after `cargo metadata`, with **no
dirty-tree guard** — so a dirty tree can produce artifacts that then seed a cache
labeled with the clean `HEAD` `source-revision`. Independent of caching this is a
latent correctness gap; caching makes it poison. The action MUST:

- assert committed source (reusing `assert_committed_source`) **before** restoring
  or building, and
- **re-check** immediately before `cache/save`, refusing to save if the tree became
  dirty during the build.

`source-revision` in the target key is only meaningful under this guard.

### 3.6 Step graph and the credential-free handoff

Keys depend on the resolved toolchain's `rustc` commit, so the toolchain must be
installed **before** keys are computed and caches restored. `build-app-cli` today
installs the toolchain and compiles inside one script, and unconditionally **resets
the target dir** (`reset_owned_dir`) — which would erase a restored cache. The
cached path therefore uses this ordered step graph:

1. Resolve project (app repo id, workspace id, `source-revision`, toolchain) —
   committed-source guard (§3.5).
2. Install the resolved toolchain.
3. Compute the `deps` and `target` keys (now that `rustc` is known).
4. `cache/restore` both caches into the fixed paths (§3.1). **Do not reset**
   `CARGO_TARGET_DIR` when a restore occurred.
5. Credentialed fetch → scrub → offline build (§3.7), artifact discovery (§3.8).
6. `cache/save` (deps if the fetch changed it; target under the exact key) — after
   the committed-source re-check and ownership check (§3.9).

The existing credential-free re-exec/scrub that fronts the build is preserved; the
cache steps expose only key/path strings, never provider credentials.

### 3.7 Private dependencies — credentialed fetch, then scrubbed offline build

An action-owned `CARGO_HOME` (§3.1) does not inherit the runner's `~/.cargo`
credentials, and the build step must remain credential-free — so authenticated
private dependencies need an explicit, phased flow rather than a token in the build
env:

1. **Fetch phase (credentialed).** A dedicated step runs `cargo fetch --locked`
   with a deployer-supplied credential from a new **`registry-credentials`** input
   (a JSON map of registry name → token, written to a temporary Cargo credentials
   file / `CARGO_REGISTRIES_*_TOKEN`). This both authenticates private registries
   and fully populates the dependency cache.
2. **Scrub.** The credentials file / env is removed before the build; the token
   never reaches `build.rs`.
3. **Offline build.** The compile runs `cargo build --locked --offline`, resolving
   everything from the fetched Cargo home — so no credential exists in the build or
   cache-save phase.

A committed `.cargo/config.toml` in the app repo is honored for registry _config_
(sources/replacements), but **credentials come only from `registry-credentials`**,
never the tree or the runner home. If `registry-credentials` is unset and the graph
needs auth, `cargo fetch` fails closed with a clear message. Repositories that use
no private registries are unaffected.

### 3.8 Artifact discovery — force the host target, read Cargo's JSON

A repo `.cargo/config.toml` or `CARGO_BUILD_TARGET` can silently change the native
target (binary under `target/<triple>/<profile>/`), and `build.build-dir` can
relocate intermediates — so constructing `target/<profile>/<bin>` is unreliable.
Instead the action:

- builds with an **explicit `--target <resolved-host-triple>`** (so the output
  layout is deterministic regardless of repo config),
- constrains `build.build-dir` to the fixed `CARGO_TARGET_DIR`, and
- obtains the executable path from Cargo's **`--message-format=json` compiler-
  artifact** message for the CLI bin target, rather than constructing it.

### 3.9 Concurrency and cleanup (self-hosted)

A single process-held lock cannot span the separate restore/build/save **steps**.
Instead a **cross-step ownership file** at `EDGEZERO__CACHE__ROOT/owner` is claimed
(atomic create) at the start of restore and released after save:

- The invocation that claims it owns the fixed paths for its duration; it restores,
  builds, and saves.
- A concurrent invocation that fails to claim it **skips both cache actions**,
  builds in a private `mktemp` `CARGO_TARGET_DIR` (correct, uncached), and cleans it
  up — never touching the shared paths.
- After `cache/save`, the owner **always removes the local cached directories** (and
  the owner file). Remote cache eviction does **not** clean persistent
  self-hosted-runner files, so the action must.

On ephemeral runners (the supported model) contention never arises; this protocol
only matters for persistent self-hosted runners, and is fail-safe (loser is
correct-but-uncached).

### 3.10 Security — trusted writers AND trusted readers

Caching a build's output is safe only with a trusted writer, and caching source is
confidential only with a trusted reader. The `build-app-cli` scrub removes declared
provider aliases and blanks `BASH_ENV`/`ENV` but is **not** a same-UID isolation
boundary. Enabling `cache: true` therefore requires, as **enforced or documented
preconditions**:

- **Trusted writers.** Cache-writing runs must be trusted, never triggered by
  untrusted refs. The target cache is executable input to later builds (§3.2b), so a
  poisoned writer poisons downstream builds.
- **Trusted readers / confidentiality.** GitHub Actions caches are readable by
  workflows on other refs, **including fork PRs reading base/default-branch
  caches**. `cache-key-suffix` is a **namespace, not access control**. A repository
  that executes untrusted PR workflows MUST NOT cache **private dependency source**
  (the dependency cache) unless it uses storage with real ACLs; such repos may still
  cache the target artifacts only if writers are trusted, or leave `cache: false`.
- **No job-level secrets** in the `build-app-cli` step beyond what the action
  injects; provider credentials are already scoped away; registry tokens exist only
  in the scrubbed fetch phase (§3.7).

These preconditions and the default (`false`, §3.12/§6) reconcile with the parent
spec's opt-in, exact-cache posture (which this spec amends — see §5).

### 3.11 `cli-profile`

The CLI is a build-time tool (API calls, shell-outs, no hot loops), so `--release`
mostly adds compile time. `cli-profile` (default `release`) selects the profile:

- **Allowed values:** `release`, `dev`, or a profile the **app's own `Cargo.toml`
  defines**. Effective flags: `release`→`--release`, `dev`→(default), named→
  `--profile <name>`.
- Artifact discovery is via Cargo's JSON message (§3.8), so the profile's output
  directory is never hard-coded.
- `cli-profile` is part of both keys (§3.3).

Note the reuse claim precisely (§6): the target cache yields **cross-revision reuse
of unchanged dependency artifacts**. It does **not** make the changed app crate
compile incrementally — Cargo's `release` profile sets `incremental = false`, so the
app crate recompiles in full unless the deployer selects a custom incremental
profile.

### 3.12 Inputs on `build-app-cli`

| Input                  | Default   | Meaning                                                                                                                                                             |
| ---------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cache`                | `false`   | `true` \| `false`. Enable the two caches. **Off by default** (§6). No `auto` (the lockfile is mandatory, so a no-lock branch is unreachable).                       |
| `cache-key-suffix`     | `""`      | Free-form; **SHA-256'd and length-bounded** before use (the paths are fixed, but the key must not be attacker-shaped). A namespace, **not** access control (§3.10). |
| `cli-profile`          | `release` | §3.11.                                                                                                                                                              |
| `registry-credentials` | `""`      | JSON map of registry name → token for the credentialed fetch phase (§3.7). Empty ⇒ public deps only; a private graph fails closed.                                  |

## 4. Testing

- **Key contract (`run.sh`):** the two field matrices in §3.3 exactly — a change to
  any shared field changes both keys; `Cargo.lock` changes both; `source-revision`
  changes only the `target` key and is absent from its restore prefix; the suffix is
  hashed/length-bounded; the shared helper matches `resolve-project` where fields
  overlap; artifact discovery reads the JSON message, not a constructed path.
- **Generation smoke (A/B/C):** three builds at distinct revisions A, B, C on one
  lockfile, each with a **unique `cache-key-suffix` per test run** and **distinct
  artifact names**, asserting: A seeds a `target` generation; B restores _a_
  compatible generation (matched key recorded), rebuilds, saves its own; C restores
  _a_ compatible generation and records **which** key matched (observability) —
  without asserting ancestry (§3.4). A separate case asserts the `deps` cache
  restores registry/git and the offline build succeeds with no network.
- **Negative cases:** `cache: true` build fails ⇒ no poisoned save; dirty tree ⇒
  refuse to save (§3.5); ownership contention ⇒ loser builds uncached and touches no
  shared path (§3.9); `registry-credentials` unset with a private graph ⇒ fetch
  fails closed (§3.7); trust-boundary: the build step still scrubs provider
  aliases/`BASH_ENV` on the cached path.

## 5. Docs and spec-migration impact

This spec **amends** the parent deploy spec, which currently mandates target-only
**exact** caching and **forbids restore prefixes**. Normative updates required:

- `docs/specs/edgezero-deploy-github-action.md`: replace the exact-only / no-prefix
  caching language with the two-cache model, the restore-prefix (target) rule, the
  writer/reader trust boundary, and the shared key helper; add the `build-app-cli`
  inputs.
- `docs/specs/edgezero-deploy-action-implementation-plan.md`: add the cache step
  graph (§3.6) and the fetch/scrub/offline phase.
- `docs/specs/edgezero-deploy-adoption-guide.md` and
  `docs/guide/deploy-github-actions.md`: document the inputs, the **security
  preconditions** (esp. the fork-PR reader caveat and private-source rule), and the
  "cache the WASM build too" recipe.
- No deployer-specific configuration in any of them.

## 6. Default, expected effect, and storage

**Off by default**, per §3.10 and the parent's posture. A deployer meeting the
preconditions sets `cache: true` and gets: dependency **download+extract** avoided
after the first run per lockfile; and **cross-revision reuse of unchanged
dependency compilation** via the target cache, so second-and-later commits skip
recompiling unchanged deps (the app crate still recompiles per §3.11).

**Storage/churn.** Saving a full `target/` archive per commit is not delta caching;
on a busy repo this can churn the repo's cache quota (older generations evicted).
The `target` key's identity fields keep generations scoped; deployers can bound
churn with `cache-key-suffix` scoping. This tradeoff is documented, not hidden.

> **Open decision (maintainer).** The original goal favored a zero-config default
> (`cache` on) so downstream repos speed up on a version bump alone. This spec
> defaults **off** for the §3.10 trust reasons and parent-spec consistency. Flipping
> to on would require treating §3.10's preconditions as guaranteed for **every**
> consumer — a conscious, documented risk-acceptance, not an assumption.

## 7. Out of scope / future

- Compiler-level caching (`sccache`) shared across jobs.
- Prebuilding and publishing the CLI as a release artifact reused across deploys.
- Caching for non-Fastly adapters when those wrappers land.
- Storage with real ACLs (e.g. a self-hosted cache backend) enabling private-source
  caching in untrusted-PR repositories.

## 8. Open questions carried into planning

- Exact bytes hashed for `app-repo-id` (remote URL vs. first-commit hash) and
  `workspace-id`.
- The `registry-credentials` schema and its validation (name charset, token
  handling) — modeled on the existing `provider-env` typing.
- Whether the dependency cache is worth enabling independently of the target cache
  for public-only graphs (download savings without the target trust surface).

## 9. Recorded alternative

Adopt a **pinned** third-party Rust cache (`Swatinem/rust-cache`) instead of the
in-house target cache. It already solves path stability, key composition, cache
cleaning, and prefix restore. It would subsume most of §3, at the cost of a pinned
third-party dependency the repo has so far avoided. Recorded for an explicit
build-vs-buy decision before implementation.
