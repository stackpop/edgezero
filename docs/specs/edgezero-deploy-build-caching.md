# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.1, caching via a reusable build **workflow**

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's own CLI package (native) into a throwaway
`mktemp` `CARGO_TARGET_DIR` with **no caching**, so every deploy recompiles the whole
dependency graph (~10 min for `stackpop/trusted-server-deployer`). Caching it safely
requires a build environment that provably holds **no provider credential** while a
cache is written.

## 2. Decision: a reusable workflow owns the credential-free build job

A **composite action cannot enforce** a credential-free job: it runs inside the
caller's job, the caller may add later steps to that same job, and
`Swatinem/rust-cache`'s JavaScript `post:` hook runs during **job cleanup — after**
those steps. A workflow could pass an env check, run a token-bearing deploy later in
the same job, and then have the cache save a `target/` shaped by that step. So
"enforced" and "structural" are only achievable by a surface that **owns the whole
job**.

Therefore caching is delivered through a **reusable workflow** (`on: workflow_call`)
that owns the build job end-to-end. A caller invokes it as a job
(`jobs.build.uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>`) and
**cannot inject steps into it**. The workflow:

- declares minimal `permissions:` (`contents: read`) and **no `id-token`** (OIDC off);
- **requests no provider secrets** (no `secrets: inherit`; no provider inputs), so no
  token can exist in the job;
- checks out the app with `persist-credentials: false`;
- runs the pinned rust-cache + the existing `build-app-cli` composite + `upload-artifact`.

Because the reusable workflow owns the job, tokenlessness is **structural**. The
composite `build-app-cli` action is unchanged for the **uncached, in-job** use; the
`cache: true` path is supported **only** via the reusable workflow. The two-job
topology is:

```
job build  = uses: .../build-app-cli.yml@<ref>   (owns a tokenless job; caches; uploads artifact)
job deploy = (secrets) download artifact → deploy-fastly / lifecycle
```

## 3. Design

### 3.1 Toolchain-bound caching lifecycle

rust-cache invokes bare `rustc`/`cargo metadata`, so it would otherwise key and
restore against the **runner-default** compiler, not the app's resolved toolchain.
The reusable workflow's job runs this exact order:

1. checkout app (`persist-credentials: false`) at the caller-supplied full SHA;
2. resolve the app toolchain and **install/select** it, then **export
   `RUSTUP_TOOLCHAIN`** so every later `rustc`/`cargo`/rust-cache invocation uses it;
3. resolve the **host target triple** for that toolchain;
4. `Swatinem/rust-cache` **restore** (§3.2);
5. the `build-app-cli` compile (§3.3) — **no target reset on the restored path**;
6. stage the artifact (§3.4) and `upload-artifact`;
7. rust-cache **job-end save** — safe because the job is tokenless.

Test with an application toolchain **different from the runner default** to prove the
binding.

### 3.2 rust-cache pin and configuration

Pinned by full SHA (the pin gate requires a 40-hex SHA **specifically for
`Swatinem/rust-cache`**, alongside the repo's general version-tag policy; a regression
test rejects **every** non-40-hex rust-cache ref, not only `@v2`):

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Configuration:

- `cache-bin: false` (do not cache `CARGO_HOME/bin` executables),
  `cache-workspace-crates: false` (do not cache the app's own crates' outputs);
- `prefix-key` **replaces** rust-cache's default prefix with a versioned, namespaced
  value: `edgezero-build-app-cli-v1-<hosted-image-id>-<suffix-hash>` (§3.5, §3.6),
  where `suffix-hash` is the bounded SHA-256 of `cache-key-suffix`;
- `workspaces:` maps the **canonical Cargo workspace** to a target dir given as a
  **path relative to that workspace** — rust-cache's config does not interpret an
  absolute right-hand side as expected. The target lives **outside** the
  per-invocation action workspace (§3.3) so composite cleanup cannot delete it before
  the post-save.

### 3.3 Target directory, lifetime, concurrency

- The cached `CARGO_TARGET_DIR` is a **stable path outside** the per-invocation
  workspace that composite cleanup removes, so it survives until rust-cache's job-end
  save.
- The compile's existing unconditional target **reset is removed on the cached path**
  (it would erase a restore); reset happens only when initializing a new namespace or
  on the uncached path.
- Because the reusable workflow owns the job, there is **exactly one cache-enabled
  invocation per job** — the concurrent-invocation isolation the composite supports
  is not needed here and is not claimed.

### 3.4 Artifact discovery and Cargo compatibility

- `cache: true` **requires Cargo ≥ 1.91** (for `build.build-dir`); the workflow checks
  the resolved toolchain's Cargo version and fails closed with a clear message
  otherwise (no silent fallback).
- The build forces `--target <resolved-host-triple>` and constrains `build.build-dir`
  to the owned target root.
- The executable is selected from Cargo's `--message-format=json` stream: the **single**
  `compiler-artifact` whose `package_id` matches the resolved package, whose
  `target.name` is the requested binary, whose `target.kind` contains `bin`, and whose
  `executable` is non-null — after a successful `build-finished`. The path is
  canonicalized to lie beneath the owned target and checked for execute permission.
- rust-cache runs `cargo metadata --all-features` (no `--locked`); the plan's
  "all Cargo commands use `--locked`" contract is scoped to **our** invocations, and
  this exception is documented.

### 3.5 Artifact provenance

Both jobs must check out the **same resolved full SHA**. `app-cli-meta.json` gains
`app-repo` and `source-revision` fields; the deploy job **validates** them (matching
its own checkout) **before** the downloaded CLI is run with provider credentials, so a
cache/artifact from a different repo or revision cannot receive the token.

### 3.6 Security and environment identity

- **Reader trust (the enabling precondition).** A dependency-reuse cache necessarily
  stores dependency **source** (`registry/cache` `.crate` archives, `git/db` repos) and
  compiled `target/`. Actions caches are readable across refs, including fork PRs
  reading base/default-branch caches; a key is a **namespace, not an ACL**. A repo that
  runs untrusted PR workflows and has **private dependencies** MUST NOT enable
  `cache: true` unless it accepts that exposure (or uses ACL storage).
- **Build-environment identity.** rust-cache keys `CC`/`CFLAGS`/`CXX`/`CMAKE`/`CARGO`
  and `RUST*` values, but **not** the compiler/linker/libc/header versions or the
  runner image — and GitHub-hosted images update regularly, so they are **not**
  homogeneous over a cache's lifetime. The `prefix-key` therefore includes a
  **hosted-image identity** (`ImageOS`/`ImageVersion`); self-hosted runners must supply
  an immutable environment namespace (via `cache-key-suffix`) or a pinned container.
- **Alias check is defense-in-depth only.** An in-job assertion that provider aliases
  are absent cannot prove job topology (it misses later inputs, checkout credentials,
  OIDC, arbitrary secret names/files, and persistent-runner state), and the composite
  already blanks aliases before its scripts run, so an in-script check sees scrubbed
  values. The **workflow owning the job** is the guarantee; any alias check is a
  secondary signal that must run **before** scrubbing and is **not** presented as proof.

### 3.7 Inputs

The reusable workflow surfaces, and forwards to the composite:

| Input              | Default | Meaning                                                                            |
| ------------------ | ------- | ---------------------------------------------------------------------------------- |
| `cache`            | `false` | Enable rust-cache (reusable-workflow path only). Off by default.                   |
| `cache-key-suffix` | `""`    | Bounded/hashed into `prefix-key` to namespace / rotate a cache. Not an ACL (§3.6). |

`cli-profile` and private-registry/git dependency authentication remain out of scope
for v1 (§7). With `cache: false`, `build-app-cli` behaves exactly as today (throwaway
`mktemp` target, reset, online build).

## 4. Testing

- **Pin regression:** any non-40-hex `Swatinem/rust-cache` reference fails the gate;
  the pinned SHA passes.
- **Toolchain binding:** an app toolchain ≠ runner default is the one used for restore,
  compile, and save (`RUSTUP_TOOLCHAIN` exported).
- **Artifact discovery:** the binary is selected via the JSON `compiler-artifact` rules
  under the forced host target; a hostile repo `.cargo/config.toml` / `CARGO_BUILD_TARGET`
  does not break it; Cargo < 1.91 fails closed.
- **Provenance:** the deploy job rejects an artifact whose `app-repo`/`source-revision`
  differ from its checkout before running it with credentials.
- **Smoke (freshness, two runs):** hold manifests, toolchain, keyed environment, and
  `GITHUB_JOB` constant; run 1 populates the cache; run 2 restores, then builds with
  **Cargo network access blocked** and asserts a known **registry** dependency reports
  `fresh: true`, **workspace-local** crates rebuild, and the application artifact
  differs between the two revisions.
- **Lifecycle:** hits, misses, `cache: false`, nested workspaces, suffix rotation,
  hostile Cargo config, and target survival past composite cleanup (post-save).

## 5. Docs and migration

- **Scope the parent's exact-key language to `deploy-fastly.cache`** and define
  `build-app-cli.cache` separately as a **rolling dependency/build cache** (rust-cache:
  registry/git + `target/`, rolling restore prefix omitting manifest/lock hashes,
  job-end save) with its own contents, trust assumptions, and timing —
  `deploy-fastly` is unchanged.
- New **reusable workflow** `.github/workflows/build-app-cli.yml` documented in the
  guide + adoption guide as the caching topology (two jobs), with the §3.6 reader-trust
  and homogeneity preconditions prominent.
- Pin gate / `zizmor`: add the rust-cache SHA and the non-40-hex regression test.
- Public-surface golden: the reusable-workflow inputs.

## 6. Default and effect

**Off by default.** In the two-job topology with `cache: true`, a warm build restores
compiled **dependencies** (the bulk of the ~10 min); the app crate and workspace-local
crates still recompile. rust-cache's **save-time cleaning prunes the newly saved
entry** (it does not retroactively bound existing entries).

## 7. Out of scope / future

- `cli-profile` (a faster CLI build profile — a separate, cache-free win).
- Private-registry/git dependency authentication.
- Caching for non-Fastly adapters.

## 8. History (for the record)

- **v2–v5, in-place in the deploy job:** required in-process scrubbing +
  save-before-token timing + hand-built key/path/ownership; blocked by the token being
  in the job.
- **v6, composite action + rust-cache in a "credential-free job":** correct pivot, but
  a composite **cannot enforce** the tokenless job (caller-added steps + post-hook save
  at cleanup). v6.1 moves the boundary to a **reusable workflow** that owns the job.
