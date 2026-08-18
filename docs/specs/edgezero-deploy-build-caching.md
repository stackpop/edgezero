# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — build-vs-buy resolved: adopt pinned `rust-cache`

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

A deploy performs up to **two cold Rust compiles**, back to back, neither cached by
default:

1. **`build-app-cli`** compiles the application's own CLI package (native)
   `--release` into a throwaway `mktemp` `CARGO_TARGET_DIR`, with **no caching of any
   kind** (registry, git, or `target/`). Every deploy recompiles the whole
   dependency graph — the dominant cost (~10 min for
   `stackpop/trusted-server-deployer`).
2. **`deploy-fastly`** compiles the app to `wasm32-wasip1`; its `target/` cache is
   opt-in and only active under `build-mode: always`.

The lifecycle actions (`healthcheck`/`rollback`/`config-push`) compile nothing and
are out of scope. This spec adds **opt-in** caching to `build-app-cli`, the largest
cost and the one a downstream deployer cannot fix on its own.

## 2. Decision: adopt a pinned third-party cache

Two earlier design rounds showed that a _correct_ in-house cache must re-derive a
large body of subtle invariants — cache-path stability (`actions/cache` folds the
path into its internal version), Cargo's absolute-path dep-info, the safe
Cargo-home layout (index/cache/db but **not** extracted sources/checkouts),
generation keying, restore-prefix semantics without ancestry, and cache cleaning to
bound churn. **`Swatinem/rust-cache` already solves all of these.** The project
therefore adopts it, **pinned by full commit SHA** (with a `# vX.Y.Z` trailer),
which the pin gate and `zizmor` accept. This reverses the earlier "no third-party
cache action" non-goal — a deliberate build-vs-buy decision.

rust-cache owns: caching `~/.cargo/{registry/index,registry/cache,git/db}` and the
build `target/` (pruned to keep dependency artifacts, not the workspace crates'
own outputs); key derivation over the lockfile + `rustc` version + workspace +
`prefix-key`; restore-prefix partial hits; and post-job save with cleaning. This
spec specifies only the **wiring and the concerns rust-cache does not cover**.

## 3. Design

### 3.1 Inputs on `build-app-cli`

| Input                  | Default   | Meaning                                                                                                                                           |
| ---------------------- | --------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cache`                | `false`   | `true` \| `false`. Gate the rust-cache steps. **Off by default** (§5). No `auto` — the lockfile is mandatory, so a no-lock branch is unreachable. |
| `cache-key-suffix`     | `""`      | Passed to rust-cache's `prefix-key` to namespace / rotate the cache. A namespace, **not** access control (§3.6).                                  |
| `cli-profile`          | `release` | Cargo profile for the CLI build (§3.5).                                                                                                           |
| `registry-credentials` | `""`      | JSON map of registry name → token for the credentialed fetch phase (§3.4). Empty ⇒ public deps only; a private graph fails closed.                |

### 3.2 What rust-cache is given, and the target directory

When `cache: true`, the action runs `Swatinem/rust-cache@<sha>` (restore) before the
build and lets its post-step save. It is configured with:

- a **stable `CARGO_TARGET_DIR`** that rust-cache caches (via its `workspaces` /
  `cache-directories` inputs). This **replaces** today's throwaway `mktemp` target
  dir: rust-cache requires a stable, known location, and Cargo's absolute-path
  dep-info requires the path not to move between runs. The directory remains
  action-owned and is still removed on the non-cached (`cache: false`) path.
- `prefix-key` incorporating the hashed `cache-key-suffix` and the CLI identity
  (`app-cli-package`) so two apps or two suffixes never share an entry; rust-cache
  already folds in the lockfile, `rustc` version, and workspace.

The action MUST NOT reset (`reset_owned_dir`) the target dir when a cache was
restored — the current unconditional reset would erase the restore. The reset is
retained only on the uncached path.

### 3.3 Committed-source guard (new; a pre-existing gap)

`build-app-cli` today builds immediately after `cargo metadata`, with **no
dirty-tree guard**, so a dirty tree can seed a cache under a clean `HEAD`. Caching
makes that poison. The action MUST assert committed source
(`assert_committed_source`) **before** the cached build. (This closes a latent
correctness gap independent of caching.)

### 3.4 Private dependencies — credentialed fetch, then scrubbed offline build

An action-owned `CARGO_HOME`/target does not inherit runner Cargo credentials, and
the build step must stay credential-free — so private deps use a phased flow rather
than a token in the build env:

1. **Fetch (credentialed):** a dedicated step runs `cargo fetch --locked` with a
   deployer-supplied credential from `registry-credentials`
   (`CARGO_REGISTRIES_*_TOKEN` / a temp credentials file). This authenticates
   private registries and fully populates the Cargo home rust-cache saves.
2. **Scrub:** the credential file/env is removed before the build; no token reaches
   `build.rs`.
3. **Offline build:** the compile runs `cargo build --locked --offline`, resolving
   from the fetched home.

A committed `.cargo/config.toml` is honored for registry _config_; credentials come
**only** from `registry-credentials`, never the tree or runner home. Empty
`registry-credentials` with a private graph fails closed with a clear message.
Public-only graphs are unaffected.

### 3.5 `cli-profile` and artifact discovery

`cli-profile` (default `release`) selects the profile — `release`→`--release`,
`dev`→(default), a name the app's `Cargo.toml` defines→`--profile <name>`. Because a
repo `.cargo/config.toml` / `CARGO_BUILD_TARGET` can move the native target and
profiles change the output subdir, the built binary is **not** discovered by
constructing `target/<profile>/<bin>`. Instead the build:

- forces an explicit `--target <resolved-host-triple>`,
- constrains `build.build-dir` to the stable `CARGO_TARGET_DIR`, and
- reads the executable path from Cargo's `--message-format=json` compiler-artifact
  message for the CLI bin target.

rust-cache caches all profile subdirs under one key, so `dev` and `release` builds
coexist without collision; `cli-profile` need not be in the cache key.

**Reuse claim, stated precisely (§5):** the target cache yields **cross-revision
reuse of unchanged dependency compilation**. It does **not** make the changed app
crate compile incrementally — `release` is `incremental = false` — so the app crate
recompiles in full unless a deployer selects a custom incremental profile.

### 3.6 Security — trusted writers AND trusted readers

rust-cache does not change GitHub's cache trust model, so the boundary is the same
and is a **precondition for enabling `cache: true`**:

- **Trusted writers.** The `target/` cache is executable input to later builds; a
  cache-writing run must never be triggered by an untrusted ref.
- **Trusted readers / confidentiality.** Actions caches are readable by workflows on
  other refs, **including fork PRs reading base/default-branch caches**.
  `cache-key-suffix`/`prefix-key` is a **namespace, not an ACL**. A repository that
  runs untrusted PR workflows MUST NOT cache **private dependency source** unless it
  uses storage with real ACLs; it may still leave `cache: false`.
- **No job-level secrets** in the build step beyond what the action injects;
  provider credentials are already scoped away; registry tokens exist only in the
  scrubbed fetch phase (§3.4).

The `build-app-cli` scrub of declared provider aliases + `BASH_ENV`/`ENV` is
preserved on the cached path; it is **not** a same-UID isolation boundary, which is
why the writer/reader preconditions above are load-bearing. Default `false` (§5)
reconciles with the parent spec's opt-in posture.

### 3.7 Step graph

1. Resolve project (app identity, `source-revision`, toolchain) + committed-source
   guard (§3.3).
2. Install the resolved toolchain (rust-cache's key needs `rustc`).
3. `Swatinem/rust-cache@<sha>` restore (when `cache: true`), pointed at the stable
   `CARGO_TARGET_DIR`, with `prefix-key` from §3.2.
4. Credentialed fetch → scrub (§3.4).
5. Offline `cargo build` (no target reset on the restored path), then JSON artifact
   discovery (§3.5).
6. Package the binary + `app-cli-meta.json` as today.
7. rust-cache post-step saves (cleans + stores).

The existing credential-free re-exec/scrub fronting the build is preserved; cache
steps expose only key/path strings, never provider credentials.

## 4. Testing

rust-cache's own correctness (path stability, keying, cleaning, restore) is trusted
and not re-tested. Tests cover **our wiring and the concerns we own**:

- **Contract (`run.sh`):** `cache: false` runs no cache step and keeps the uncached
  `mktemp` + reset path; `cache: true` skips the reset and uses the stable target
  dir; `cache-key-suffix` reaches `prefix-key` hashed; `cli-profile` maps to the
  right flag and the binary is found via the JSON message (not a constructed path);
  the committed-source guard rejects a dirty tree before a cached build.
- **Fetch phase:** `registry-credentials` populates the home and the subsequent
  `--offline` build succeeds with no network; unset credentials + a private graph
  fails closed; the credential is absent from the build/save phase.
- **Smoke (A/B):** with `cache: true`, a second build at a new revision on the same
  lockfile completes with dependencies restored (no re-download, reduced compile),
  recording the matched cache key for observability — **without** asserting
  ancestry (rust-cache owns restore selection).
- **Negative:** a failed build does not poison a save; provider aliases/`BASH_ENV`
  remain blanked on the cached path.

## 5. Docs and spec-migration impact

This spec **amends** the parent deploy spec, which currently mandates target-only
**exact** caching, **forbids restore prefixes**, and implies no third-party cache
action. Normative updates:

- `docs/specs/edgezero-deploy-github-action.md`: replace the exact-only / no-prefix
  / no-third-party caching language with the pinned-`rust-cache` model and the
  writer/reader trust boundary; add the `build-app-cli` inputs.
- `docs/specs/edgezero-deploy-action-implementation-plan.md`: add the step graph
  (§3.7), the fetch/scrub/offline phase, and the rust-cache SHA pin.
- `docs/specs/edgezero-deploy-adoption-guide.md` + `docs/guide/deploy-github-actions.md`:
  document the inputs, the **security preconditions** (esp. the fork-PR reader
  caveat and the private-source rule), and the "cache the WASM build too" recipe
  (`build-mode: always` + `cache: true` on `deploy-fastly`).
- The pinned `Swatinem/rust-cache@<sha>` is added to the pin-gate's expectations and
  `zizmor` config (SHA pin, accepted by both).
- No deployer-specific configuration in any of them.

## 6. Default and expected effect

**Off by default**, per §3.6 and the parent's posture. A deployer that meets the
preconditions sets `cache: true` and gets, after a first warm run per lockfile:
dependency download/extract avoided, and **unchanged dependency compilation reused
across revisions** (the app crate still recompiles per §3.5). rust-cache's cleaning
bounds cache size, so churn is far lower than storing a full `target/` per commit.

> **Zero-config was considered and declined.** The original goal favored `cache` on
> by default so downstream repos speed up on a version bump alone; the maintainer
> chose **off** for the §3.6 trust reasons and parent-spec consistency. Downstream
> repos opt in after meeting the preconditions.

## 7. Out of scope / future

- Compiler-level caching (`sccache`) shared across jobs.
- Prebuilding and publishing the CLI as a release artifact.
- Caching for non-Fastly adapters when those wrappers land.
- Storage with real ACLs enabling private-source caching in untrusted-PR repos.

## 8. Open questions carried into planning

- The exact rust-cache version/SHA to pin, and which of its inputs (`workspaces`,
  `cache-directories`, `cache-targets`, `save-if`, `cache-all-crates`) the wiring
  sets.
- The `registry-credentials` schema and validation, modeled on the existing
  `provider-env` typing.
- Whether to expose the dependency-only benefit separately for public-only graphs.
