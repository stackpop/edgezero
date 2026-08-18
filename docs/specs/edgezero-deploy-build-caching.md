# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v5, mirrors `deploy-fastly`'s exact-key cache

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's own CLI package (native) `--release`
into a throwaway `mktemp` `CARGO_TARGET_DIR`, with **no caching of any kind** — so
every deploy recompiles the whole dependency graph (~10 min for
`stackpop/trusted-server-deployer`), and it is the one cost a downstream deployer
cannot fix on its own. `deploy-fastly` already caches its `wasm32-wasip1` build with
an **exact-key, credential-free** cache. This spec applies that **same proven
pattern** to `build-app-cli`.

The lifecycle actions (`healthcheck`/`rollback`/`config-push`) compile nothing and
are out of scope.

## 2. Decision: mirror `deploy-fastly`, not a third-party cache

Earlier rounds explored an elaborate in-house rolling cache and then
`Swatinem/rust-cache`. Both were rejected on review:

- A **rolling / restore-prefix** cache adds ancestry assumptions GitHub does not
  guarantee, generation churn, and key/path complexity.
- **rust-cache** saves via a JavaScript `post:` hook that GitHub runs at **job
  end** — after the caller's token-bearing deploy and after this composite's
  cleanup — which **breaks the credential-free-save-before-deploy invariant**,
  post-save cleanup, and same-job testing. It also archives whole `registry/`/`git/`
  trees (including some extracted sources/checkouts) and `CARGO_HOME/bin`, caching
  private source and executable state, and runs `cargo metadata` with the GitHub
  file-command channels live, defeating the trusted-Cargo boundary.

The design that fits this action's security model is the one already in the repo:
an **exact-key** `actions/cache/restore` + `save` pair with **explicit timing**,
credential-free, over paths the action controls. `build-app-cli` mirrors it. An
exact lock key means dependencies are compiled once per lockfile and reused; only
the app crate recompiles per revision. This is the ~10-min win, achieved simply.

## 3. Design

### 3.1 Inputs on `build-app-cli`

| Input              | Default | Meaning                                                                                                                                                            |
| ------------------ | ------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `cache`            | `false` | `true` \| `false`. Enable the cache. **Off by default** (§6), matching `deploy-fastly`. No `auto` — the lockfile is mandatory, so a no-lock branch is unreachable. |
| `cache-key-suffix` | `""`    | Folded into the exact key (SHA-256'd, length-bounded) to namespace / rotate a cache. A namespace, **not** access control (§3.8).                                   |

`cli-profile` and private-**registry** authentication are **out of scope for v1**
(§7); the cached build uses the same dependency model `deploy-fastly`'s cached build
already assumes.

### 3.2 Cache contents and stable paths

`actions/cache` folds the cached `path` set into its internal version, and Cargo's
dep-info embeds absolute paths — so the cached directories are at **fixed, stable
paths** across invocations and revisions (not the current `mktemp`):

- `CARGO_HOME = $RUNNER_TEMP/edgezero-cli-cache/cargo-home`
- `CARGO_TARGET_DIR = $RUNNER_TEMP/edgezero-cli-cache/target`

The `path:` list given to `actions/cache` is **explicit and minimal**, so we — not a
third-party tool — decide what is stored:

- `CARGO_HOME/registry/index`, `CARGO_HOME/registry/cache`, `CARGO_HOME/git/db` —
  the verified/regenerable parts; **excludes** extracted `registry/src`,
  `git/checkouts`, and `CARGO_HOME/bin`, so **no dependency source or installed
  executable is cached**;
- `CARGO_TARGET_DIR` — the compiled outputs (dependency artifacts; the app binary is
  rebuilt anyway). This is compiled executable state, governed by the writer-trust
  precondition (§3.8).

### 3.3 Exact key (shared with `deploy-fastly`)

Extract `resolve-project.sh`'s key derivation into a shared `deploy-core` helper so
both actions compute keys identically. The `build-app-cli` key is **exact — no
restore-prefix** (aligning with the parent spec's existing exact-cache mandate), a
hyphen-joined string of, in order:

`edgezero-cli-<schema>-<os>-<arch>-<host-target-triple>-<toolchain-channel>-<rustc-commit>-<app-repo-id>-<workspace-id>-<app-cli-package>-<app-cli-bin>-<lock-hash>-<suffix-hash>`

- `app-repo-id` (application Git identity) and `workspace-id` (hash of the workspace
  root relative to the app Git root, as the deploy key already does) prevent two
  apps at one deployer path, or two packages exposing the same binary, from
  colliding;
- `suffix-hash` is the length-bounded SHA-256 of `cache-key-suffix`;
- the key is derived from committed files (`Cargo.lock`) and the resolved toolchain
  within the **existing credential-free build boundary** — no new app-controlled
  command runs outside that boundary.

Because the key changes with the lockfile/toolchain/identity, a bump starts a fresh
entry; an exact match reuses the prior compiled dependencies.

### 3.4 Step graph and explicit save timing

1. Resolve project (app identity, `source-revision`, toolchain) — **committed-source
   guard** (§3.6).
2. Install the resolved toolchain (the key needs `rustc`); the build continues to
   use `cargo +<toolchain>` as today.
3. Compute the exact key (§3.3).
4. `actions/cache/restore` into the fixed paths (§3.2). On a hit, **do not**
   `reset_owned_dir` the target dir (the current unconditional reset would erase the
   restore); the reset is retained only on the uncached path.
5. The credential-free CLI build (the existing provider-env re-exec/scrub is
   unchanged), then package the binary + `app-cli-meta.json` as today.
6. **Committed-source re-check** (§3.6), then `actions/cache/save` under the exact
   key.

Save is an **explicit step that runs here — after the credential-free build, before
the caller's token-bearing deploy/lifecycle steps** — exactly as `deploy-fastly`
saves before its deploy. There is no job-end post-hook, so a build script cannot
persist a secret written by a later token step into the cache.

### 3.5 `cache: false` — today's behavior, unchanged

With `cache: false` the action behaves exactly as today: a throwaway `mktemp`
`CARGO_TARGET_DIR`, an online build, and the existing owned-directory reset and
cleanup. No cache step runs.

### 3.6 Committed-source provenance gate

`build-app-cli` today builds immediately after `cargo metadata` with **no dirty-tree
guard**, so a dirty tree can seed a cache under a clean `HEAD`. The action MUST:

- assert committed source (`assert_committed_source`) **before** the build, and
- **re-check** immediately before `cache/save`, refusing to save if the tree became
  dirty (a `build.rs` can modify source after the first check).

Coverage matches `deploy-fastly`'s existing guard (tracked files); dirty submodules
and external path dependencies are a **documented limitation** shared with the
current deploy cache, not newly introduced here. This closes a latent correctness
gap independent of caching.

### 3.7 Concurrency and cleanup (self-hosted)

Fixed shared paths mean two same-key invocations on a persistent runner could race.
A cross-step **ownership file** at `$RUNNER_TEMP/edgezero-cli-cache/owner` is claimed
(atomic create) before restore and released after save; an invocation that cannot
claim it **skips restore and save**, builds in a private `mktemp` target dir
(correct, uncached), and cleans it up — never touching the shared paths. On ephemeral
runners (the supported model) this never triggers. The owner always removes the local
cached directories after save (remote eviction does not clean self-hosted files).

### 3.8 Security — trusted writers AND readers

The cache preserves `build-app-cli`'s credential-free posture (the save runs before
any token, §3.4) and stores **no dependency source** (§3.2). Remaining preconditions
for enabling `cache: true`, identical to `deploy-fastly`'s cache posture:

- **Trusted writers.** `CARGO_TARGET_DIR` is compiled executable input to later
  builds; a cache-writing run must not be triggered by an untrusted ref, and the
  provenance gate (§3.6) binds the saved entry to committed source.
- **Trusted readers.** Actions caches are readable across refs, including fork PRs
  reading base/default-branch caches; `cache-key-suffix` is a **namespace, not an
  ACL**. Because dependency source is not cached (§3.2), the exposure is limited to
  compiled artifacts; a repository running untrusted PR workflows that still considers
  that sensitive leaves `cache: false`.
- **No job-level secrets** in the build step beyond what the action injects.

Default `false` (§6) matches `deploy-fastly` and the parent's opt-in posture.

## 4. Testing

Mirror `deploy-fastly`'s existing `cache-smoke` shape:

- **Contract (`run.sh`):** the shared key helper produces the exact
  `build-app-cli` key with all §3.3 fields (a change to any field, the lockfile, or
  the suffix changes the key; the suffix is hashed/length-bounded); the two actions'
  helpers agree on shared fields; `cache: false` runs no cache step and keeps the
  `mktemp`+reset path; `cache: true` skips the reset on a hit.
- **Smoke (seed + restore-hit, two runs):** run 1 populates the cache; run 2 at a
  new revision on the same lockfile restores it (cache-hit recorded) and does **not**
  re-download or recompile dependencies (assert known dependency artifacts are fresh,
  with network blocked during run 2 — no timing assertions). A negative
  (`cache: false`) case does not restore.
- **Ordering / provenance:** `cache/save` occurs before any token-bearing step; a
  dirty tree is refused before build and before save (§3.6); ownership contention ⇒
  the loser builds uncached and touches no shared path (§3.7).

## 5. Docs and spec-migration impact

Restricted to `build-app-cli`; **`deploy-fastly` is unchanged** and retains its
immediate credential-free save before deployment.

- `docs/specs/edgezero-deploy-github-action.md`: extend the existing exact-cache
  section to cover `build-app-cli` (same exact-key, no-prefix model — no conflict
  with its current mandate); add the two inputs and the shared key helper.
- `docs/specs/edgezero-deploy-action-implementation-plan.md`: add the step graph
  (§3.4) and the committed-source gate.
- `docs/specs/edgezero-deploy-adoption-guide.md` + `docs/guide/deploy-github-actions.md`:
  document `cache` / `cache-key-suffix`, the writer/reader preconditions, and the
  existing "cache the WASM build too" recipe.
- The action public-surface golden test gains the two new inputs.
- No deployer-specific configuration in any of them.

## 6. Default and expected effect

**Off by default**, matching `deploy-fastly`. A deployer meeting the preconditions
sets `cache: true` and, after a first warm run per lockfile, avoids dependency
download **and recompilation** — the app crate still recompiles per revision, but the
dependency graph (the bulk of the ~10 min) is reused. Because the key is exact and
the paths fixed, there is no restore-prefix churn.

> **Zero-config was considered and declined.** The maintainer chose `cache` **off**
> for the §3.8 trust reasons and parent-spec consistency; downstream repos opt in.

## 7. Out of scope / future

- `cli-profile` (a faster CLI build profile) — orthogonal to caching; revisit
  separately.
- Private-**registry** and private-**git** dependency authentication for the cached
  build — the phased credentialed-fetch approach had unresolved holes (git auth,
  warm-cache auth-skip); v1 assumes `deploy-fastly`'s current dependency model.
- Cross-revision "rolling" warming via restore-prefixes (rejected; exact key only).
- Caching for non-Fastly adapters when those wrappers land.
