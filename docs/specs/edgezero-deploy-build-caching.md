# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed)

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
This spec adds one.

## 2. Goals and non-goals

**Goals**

- Make `build-app-cli`'s compile warm and incremental via **configurable**
  caching, reusing the exact-key cache design `deploy-fastly` already uses.
- Speed up downstream deployers on a version bump **without workflow edits** — a
  sensible `auto` default rather than opt-in.
- Preserve the existing security invariant: a build script can never persist a
  secret into a cache.

**Non-goals**

- No changes to the lifecycle actions (they do not compile).
- No new third-party caching action (`Swatinem/rust-cache`, `setup-rust-toolchain`
  built-in cache). The repo deliberately runs an in-house exact-key cache; this
  spec extends it, it does not replace it.
- No deployer-specific configuration in the shipped docs. Per-repo tuning (e.g.
  what `stackpop/trusted-server-deployer` should set) is delivered as a separate
  recommendation after implementation, not baked into the spec or guide.
- No change to the default WASM `build-mode` (`auto → never`). The spec only
  documents the existing recipe for caching that build too.

## 3. Design

### 3.1 New inputs on `build-app-cli`

| Input              | Default   | Meaning                                                                                                       |
| ------------------ | --------- | ------------------------------------------------------------------------------------------------------------- |
| `cache`            | `auto`    | `auto` \| `true` \| `false`. Whether to cache the CLI build. Mirrors `deploy-fastly`'s `cache`.               |
| `cache-key-suffix` | `""`      | Optional token folded into the cache key so a deployer can namespace or rotate its cache.                     |
| `cli-profile`      | `release` | Cargo profile for the CLI build. `release` (today's behavior) or a fast profile for the throwaway build tool. |

**`cache` resolution.** `auto` enables caching **only** when a committed
`Cargo.lock` exists at the CLI's Cargo workspace root — the exact key is derived
from that lockfile, so without it caching cannot be keyed correctly and is
disabled. `true` requires a `Cargo.lock` and **fails closed** with a clear message
when it is absent (the same rule `deploy-fastly` already enforces for its cache).
`false` disables caching unconditionally.

### 3.2 What is cached, and the target directory

When caching is enabled, `build-app-cli` wraps the compile in
`actions/cache/restore@v6` (before) and `actions/cache/save@v6` (after) — the same
pinned actions and pattern `deploy-fastly` uses — caching two paths:

- the CLI build's `CARGO_TARGET_DIR`, and
- the Cargo home (`registry/` + `git/`), so unchanged dependencies are neither
  re-downloaded nor recompiled.

The CLI build's `CARGO_TARGET_DIR` moves from a throwaway `mktemp` to a **stable,
cache-keyed, action-owned** path (still confined beneath the runner's action-owned
tool/temp root, still removed by the action's cleanup on a non-cached run). The
path is derived from the cache key, so two different keys never share a directory.

### 3.3 One shared cache-key derivation

`resolve-project.sh` already computes an exact cache key
(`edgezero-deploy-<os>-<arch>-<toolchain>-<target>-<cli-version>-<ws-id>-<build-args-id>-<source-revision>-<lock-hash>`).
Extract that derivation into a shared `deploy-core` helper so `build-app-cli` and
`deploy-fastly` compute keys the same way and cannot drift.

The `build-app-cli` key comprises: `Cargo.lock` hash + resolved toolchain +
OS/arch + CLI package name + `rustc` version + `cache-key-suffix`. It deliberately
omits the WASM `target` triple (this is the native CLI build) and the
provider-specific pieces. Because the key changes when the lockfile or toolchain
changes, a bump automatically starts a fresh cache — stale `target/` is never
reused.

### 3.4 `cli-profile`

The application CLI is a build-time orchestration tool: it makes provider API
calls and shells out, with no hot loops. `release` optimization therefore adds
compile time for no runtime benefit. `cli-profile` lets a deployer pick a fast
profile (e.g. `dev`: `opt-level = 0`, no debuginfo) for large speedups on top of
caching. The default stays `release` so nothing changes unless a deployer opts in.

### 3.5 Security invariant (unchanged)

`build-app-cli` is **credential-free by construction** — no provider token ever
reaches it (the token is scoped to the deploy/lifecycle steps only). Seeding and
saving a cache from this build therefore preserves the existing guarantee that a
build script cannot persist a secret into a cache. This is precisely why the CLI
build is a safe place to cache, unlike the token-bearing deploy. The cache is
confined to action-owned paths and keyed by content, never by a mutable ref.

## 4. Testing

- **Contract tests (`run.sh`):**
  - the cache key incorporates `cache-key-suffix` (a different suffix changes the
    key);
  - `cache: auto` resolves to enabled/disabled based on `Cargo.lock`
    presence/absence;
  - `cache: true` without a `Cargo.lock` fails closed with a named diagnostic;
  - the shared key helper produces identical keys from `build-app-cli` and
    `resolve-project` inputs where they overlap.
- **Smoke:** a `build-app-cli` cache populate + restore-hit case (mirroring the
  existing `cache-smoke`): a second run restores the marker from cache rather than
  rebuilding it, and a negative (`cache: false`) case does not.
- **Credential-free assertion:** the build step continues to blank the provider
  aliases and hold no token (existing invariant, re-asserted for the cached path).

## 5. Docs impact

- `docs/guide/deploy-github-actions.md`: document `cache` / `cache-key-suffix` /
  `cli-profile` on `build-app-cli`; add a "cache the WASM deploy build too" recipe
  (`build-mode: always` + `cache: true` on `deploy-fastly`).
- `docs/specs/edgezero-deploy-github-action.md`: add the `build-app-cli` inputs to
  its input table and note the shared cache-key helper.
- No deployer-specific configuration in either.

## 6. Expected effect

For a default deployer (WASM `build-mode: never`):

- Bumping to the new action version turns CLI-build caching on via `auto` — the
  first run seeds the cache, every run after is an incremental CLI build.
- Adding `build-mode: always` + `cache: true` to `deploy-fastly` warms the WASM
  build too, so a no-op redeploy avoids both cold compiles.

## 7. Out of scope / future

- Compiler-level caching (`sccache`) shared across jobs.
- Prebuilding and publishing the CLI as a release artifact reused across deploys.
- Caching for non-Fastly adapters (Cloudflare/Spin), when those wrappers land.
