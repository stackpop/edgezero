# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — revised after review

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/guide/deploy-github-actions.md` (the how-to)

## 1. Problem

A deploy through the EdgeZero actions performs up to **two cold Rust compiles**,
back to back, and neither is cached by default:

1. **`build-app-cli`** compiles the application's own CLI package (native) with
   `cargo build --release`, into a throwaway per-invocation `CARGO_TARGET_DIR`
   (`mktemp`). There is **no caching of any kind** — not the Cargo registry, not
   git dependencies, not `target/`. Every deploy recompiles the entire dependency
   graph from scratch. On a real app this is the dominant cost (observed ~10
   minutes for `stackpop/trusted-server-deployer`).
2. **`deploy-fastly`** compiles the application to `wasm32-wasip1`. This build's
   `target/` **can** be cached (`actions/cache/restore` + `save`, seeded by a
   credential-free build), but only when `build-mode: always`; the default
   (`auto → never` for Fastly) compiles the WASM cold inside `fastly compute
deploy`.

The three lifecycle actions (`healthcheck-fastly`, `rollback-fastly`,
`config-push-fastly`) download the prebuilt CLI and run it — they compile nothing
and are out of scope.

The `build-app-cli` compile is both the largest cost and the one a downstream
deployer **cannot** address on its own: the action exposes no cache knob for it.
This spec adds caching for it. The parent deploy spec's existing "target-cache is
opt-in, off by default" posture is preserved (see §6).

## 2. Goals and non-goals

**Goals**

- Give `build-app-cli` **opt-in** caching that (a) skips re-downloading and
  re-compiling unchanged dependencies and (b) lets successive commits reuse the
  prior build's `target/` for an incremental app-crate recompile.
- Reuse the in-house exact-key cache mechanics `deploy-fastly` already uses
  (`actions/cache/restore` + `save`, a shared key derivation), not a third-party
  cache action.
- Make the trust boundary of caching **explicit and enforceable**, not assumed.

**Non-goals**

- No changes to the lifecycle actions (they do not compile).
- No new third-party caching action.
- No deployer-specific configuration in the shipped docs. Per-repo tuning (e.g.
  what `stackpop/trusted-server-deployer` should set) is delivered as a separate
  recommendation after implementation, not baked into the spec or guide.
- No change to the default WASM `build-mode` (`auto → never`).

## 3. Design

### 3.1 Two caches, not one

GitHub Actions cache entries are **immutable**: an entry saved under a key can be
restored but never re-saved under that same key. A single "stable" key (over the
lockfile and toolchain, invariant across source changes) therefore freezes the
cache at the **first** snapshot for that key — later runs restore that snapshot,
recompile the app crate locally, and cannot persist the result; concurrent
first-runs are first-writer-wins. That warms dependencies but never warms across
commits.

So `build-app-cli` uses **two** caches with different key strategies:

**(a) Dependency cache — the Cargo home.** Caches `CARGO_HOME/registry` +
`CARGO_HOME/git` (downloaded crate sources and the registry index), keyed on the
**lockfile** (§3.2). It changes only when `Cargo.lock` changes, so an immutable,
exact key is correct: one save per lockfile, restored thereafter. This eliminates
re-download.

**(b) Target cache — the compiled outputs.** Caches the CLI build's
`CARGO_TARGET_DIR`, keyed on a **generation** key that includes the app's
`source-revision` (§3.2), with **restore-keys prefixes** that fall back to the most
recent prior generation sharing the same lockfile/toolchain/profile. Each commit
therefore saves its **own** entry (progressively warming across commits) while
restoring the nearest prior `target/` so Cargo recompiles only what changed.

**Trust implication (explicitly accepted).** A rolling target cache means a prior
run's compiled artifacts are trusted input to a later build. This is acceptable
ONLY under the trust boundary in §3.6 (trusted writers only). The dependency cache
(a) does not carry this implication beyond the standard "downloaded source is
verified by Cargo's checksums against the lockfile."

### 3.2 Cache-key composition

Extract `resolve-project.sh`'s key derivation into a shared `deploy-core` helper so
`build-app-cli` and `deploy-fastly` cannot drift. Both keys begin with a
**cache-schema version** constant so the format can be rotated centrally.

Common identity fields (all hashed where free-form):

- schema version;
- OS + arch + **host target triple** (the CLI is a native build; a cache is not
  portable across host triples);
- resolved toolchain channel + **`rustc --version --verbose` commit hash**;
- **Cargo workspace identity** — the hash of the workspace root's path relative to
  the app Git root (as the deploy key already does), so nested workspaces or two
  CLIs in one repo never collide;
- **`app-cli-bin`** (the binary name) and **`cli-profile`** (§3.5) — different
  binaries or profiles must not share a `target/` generation;
- a hashed, length-bounded **`cache-key-suffix`** (§3.4).

Dependency cache key adds: the `Cargo.lock` content hash.
Target cache key adds: the `Cargo.lock` hash **and** `source-revision`, with
restore-keys = the same prefix minus `source-revision` (nearest prior generation).

Because a lockfile/toolchain/profile/workspace change alters the key, a bump
starts a fresh generation; stale outputs are never silently reused.

### 3.3 `CARGO_HOME` and `CARGO_TARGET_DIR` ownership

- **`CARGO_HOME`** is set to an **action-owned** directory under the per-invocation
  workspace (not the runner's `~/.cargo`), so the dependency cache is over paths
  the action owns rather than shared runner state. A committed **`.cargo/config.toml`
  in the application repo is still honored** (Cargo reads it from the working
  directory tree), so a private-registry configuration a deployer commits to their
  app keeps working; secrets/tokens for such registries are supplied by the
  deployer as documented, never read from the runner's inherited home.
- **`CARGO_TARGET_DIR`** moves from a throwaway `mktemp` to a **key-scoped**
  action-owned path (one directory per target-cache key). Because the path is
  derived from the (hashed) key, distinct keys never share a directory.
- **Concurrency / self-hosted.** Two invocations with the **same** key on a
  persistent runner would otherwise race on one `target/`. The build takes an
  advisory lock on the key-scoped path and, failing to acquire it, falls back to a
  private `mktemp` target dir for that run (correct but uncached) rather than
  corrupting a shared directory. On ephemeral runners (the supported model) this
  never triggers. Cleanup: an uncached run removes its dir as today; a cached run's
  key-scoped dir is left for `actions/cache/save` and is bounded by the cache
  eviction policy, not the action.

### 3.4 Inputs on `build-app-cli`

| Input              | Default   | Meaning                                                                                             |
| ------------------ | --------- | --------------------------------------------------------------------------------------------------- |
| `cache`            | `false`   | `true` \| `false`. Enable the two caches above. **Off by default** (see §6). No `auto` — see below. |
| `cache-key-suffix` | `""`      | Free-form token folded into both keys after hashing + a length bound. Namespaces / rotates a cache. |
| `cli-profile`      | `release` | Cargo profile for the CLI build (§3.5).                                                             |

**No `auto`.** `build-app-cli` already requires a committed `Cargo.lock`
unconditionally (`cargo metadata --locked`, `cargo build --locked` both fail
without it), so a "disable caching but continue when the lock is missing" branch is
unreachable — the build fails first regardless. `cache` is therefore a plain
boolean; when `true`, the lockfile is guaranteed present and the keys are always
derivable.

**`cache-key-suffix` is hashed and length-bounded** before use because the local
`target/` path is derived from the key — an over-long or path-hostile suffix must
not shape a filesystem path. It is validated (bounded length, folded through the
key hash), never interpolated raw into a path.

### 3.5 `cli-profile`

The application CLI is a build-time orchestration tool (provider API calls,
shell-outs, no hot loops), so `release` optimization adds compile time for no
runtime benefit; a leaner profile is a real win on top of caching. But the profile
governs both the compile and **where the binary lands**, so it must be specified,
not hand-waved:

- **Allowed values:** `release` (default), `dev`, or the name of a profile the
  **application's own `Cargo.toml` defines** (`[profile.<name>]`). No other values.
- **Effective flags:** `release` → `cargo build --release`; `dev` → `cargo build`
  (Cargo's default profile, which enables debuginfo and does NOT optimize); a named
  profile → `cargo build --profile <name>`.
- **Artifact discovery:** the built binary is read from `CARGO_TARGET_DIR/<dir>/<bin>`
  where `<dir>` is `release` for `release`, `debug` for `dev`, and `<name>` for a
  named profile — NOT hard-coded to `release/` as today. The action resolves the
  directory from the profile.
- **Workspace overrides:** a `[profile.<name>]` in the app workspace can change a
  built-in profile's semantics; that is the app's own declaration and is respected.
  The action does not inject profiles.
- **Keying:** `cli-profile` is part of the target-cache key (§3.2), so `dev` and
  `release` builds never share a generation.

### 3.6 Security — trust boundary (replaces the "credential-free by construction" claim)

Caching a build's output is only safe when the writer is trusted and the build's
environment carries no secret a build script could copy into a cached path. The
`build-app-cli` scrub removes the **declared** provider aliases and blanks
`BASH_ENV`/`ENV`, but — as the action's own comments state — it is **not** a
same-UID isolation boundary: app-controlled `build.rs`/proc-macros run with access
to whatever the job's UID can read (Cargo registry tokens in `CARGO_HOME`,
SSH/Git credentials, job-level secrets in the environment, credential files on
disk). Any of those could be written into `registry/`, `git/`, or `target/` and
then persisted to a cache other runs restore. Caching `registry/`/`git/` can also
expose **private dependency source**.

Caching is therefore gated on an explicit threat model, documented as **required
preconditions** for enabling `cache: true`:

- **Trusted writers only.** Cache-writing runs must be trusted — not fork-PR / a
  cache poisoned by untrusted input. (GitHub isolates fork-PR caches, but a repo
  that runs deploys from untrusted refs must not enable this.)
- **`persist-credentials: false`** on the application checkout, so no Git token is
  left in the tree for a build script to harvest into `git/`.
- **No job-level secrets in the `build-app-cli` step's environment** beyond what
  the action itself injects; provider credentials are already scoped away from this
  step by design.
- **Explicit private-dependency policy.** A repo with private crate dependencies
  decides whether their source may live in a repo-scoped cache; if not, it leaves
  `cache: false` (or scopes with `cache-key-suffix`).

This is why the default is **`false`** (§3.4) and consistent with the parent
spec's "opt-in" posture: caching is a deployer decision made against these
preconditions, not an automatic default.

## 4. Testing

- **Contract tests (`run.sh`):**
  - both keys incorporate `cache-key-suffix` (a different suffix changes both keys)
    and the suffix is hashed/length-bounded (an over-long suffix does not blow the
    key or the path);
  - the target key changes with `source-revision`, `cli-profile`, `app-cli-bin`,
    and workspace identity; the dependency key changes only with `Cargo.lock`;
  - the shared key helper produces matching keys from `build-app-cli` and
    `resolve-project` inputs where they overlap;
  - `cli-profile` maps to the correct `--profile`/`--release` flag and the correct
    `target/<dir>` for artifact discovery (`release`→`release`, `dev`→`debug`,
    `foo`→`foo`).
- **Generation smoke (A/B/C, replaces the single-marker test):** three successive
  builds at **distinct source revisions** A, B, C sharing one lockfile. Assert (1)
  A seeds a target generation; (2) B restores A's generation (nearest prior via
  restore-keys), recompiles only the app crate, and saves its **own** B generation;
  (3) C restores **B's** generation, not A's — proving the cache warms across
  commits rather than freezing at the first snapshot. A dependency-cache hit is
  asserted separately (unchanged lock → registry/git restored, no re-download).
- **Trust-boundary assertion:** the build step still blanks the declared provider
  aliases and `BASH_ENV`/`ENV` on the cached path; the docs preconditions are
  covered by a rendered-guide check, not code.

## 5. Docs impact

- `docs/guide/deploy-github-actions.md`: document `cache` / `cache-key-suffix` /
  `cli-profile`; the **security preconditions** for enabling `cache: true`; and the
  "cache the WASM deploy build too" recipe (`build-mode: always` + `cache: true` on
  `deploy-fastly`).
- `docs/specs/edgezero-deploy-github-action.md`: add the `build-app-cli` inputs and
  note the shared cache-key helper and the two-cache model.
- No deployer-specific configuration in either.

## 6. Default and expected effect

Caching is **off by default**, matching the parent spec's posture and the trust
boundary in §3.6. A deployer that meets the preconditions sets `cache: true` and
gets:

- **Dependency cache** (lock-keyed): re-download of unchanged crates disappears
  after the first run for a given lockfile.
- **Target cache** (generation-keyed + restore-keys): each commit restores the
  nearest prior `target/` and recompiles only what changed, then saves its own
  generation — so the second and later commits are incremental, not full rebuilds.

> **Open decision for the maintainer.** The original goal favored a zero-config
> default (`cache: auto` on) so downstream repos speed up on a version bump alone.
> This revision defaults **off** for the security reasons in §3.6 and to match the
> parent's opt-in posture. Flipping the default to on is possible but only with the
> §3.6 preconditions treated as guaranteed for all consumers — a conscious
> risk-acceptance, documented here rather than assumed.

## 7. Out of scope / future

- Compiler-level caching (`sccache`) shared across jobs.
- Prebuilding and publishing the CLI as a release artifact reused across deploys.
- Caching for non-Fastly adapters (Cloudflare/Spin), when those wrappers land.
