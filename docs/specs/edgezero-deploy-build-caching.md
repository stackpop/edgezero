# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.8

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) with **no caching**, so
every deploy recompiles the whole dependency graph (~10 min for
`stackpop/trusted-server-deployer`, which checks out a **separate** application repo and
builds its CLI). Caching must work for that **cross-repository deployer** topology.

## 2. Trust model

- **The build compiles trusted code** (the deploy target), so `build.rs` is already trusted.
- **The runtime credential is trusted, not hidden.** The runner injects
  `ACTIONS_RUNTIME_TOKEN` into the job's Node actions regardless, so caching runs **only for
  authorized deployer events/refs (fail before compiling otherwise)**, where the runtime
  credential **and the narrow app-checkout PAT** are explicitly trusted (a trusted `build.rs`
  could read either; both are in the trusted boundary, and the PAT is kept `contents: read`).
- **The deployer owns and writes its repo-scoped cache.**

A reusable workflow (`on: workflow_call`) owns the build job.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

## 3. Design

### 3.1 Reusable-workflow contract

| Input               | Type    | Req.             | Default        | Meaning                                                                                         |
| ------------------- | ------- | ---------------- | -------------- | ----------------------------------------------------------------------------------------------- |
| `app-repository`    | string  | no               | caller repo    | `owner/repo` of the app.                                                                        |
| `app-ref`           | string  | if repo          | —              | Full commit SHA (required when `app-repository` is set).                                        |
| `working-directory` | string  | no               | `.`            | Compile cwd; must resolve **beneath** `workspace-root`.                                         |
| `workspace-root`    | string  | **yes** (cache)  | —              | Cargo workspace root, relative to the checkout (§3.3).                                          |
| `app-cli-package`   | string  | yes              | —              | Cargo package.                                                                                  |
| `app-cli-bin`       | string  | no               | package name   | Binary target.                                                                                  |
| `rust-toolchain`    | string  | no               | `auto`         | Toolchain or `auto`.                                                                            |
| `app-cli-artifact`  | string  | **yes** (matrix) | `edgezero-cli` | Artifact name; **must be unique per matrix leg** (the default is only safe for a single build). |
| `cache`             | boolean | no               | `false`        | Enable caching.                                                                                 |
| `cache-key-suffix`  | string  | no               | `""`           | Namespaces the cache.                                                                           |
| `timeout-minutes`   | number  | no               | (set by wf)    | Job timeout.                                                                                    |

Secret: `app-checkout-token` (narrow `contents: read` PAT; required for a private
`app-repository`, else fail closed).

| Output                    | Meaning                                                         |
| ------------------------- | --------------------------------------------------------------- |
| `app-cli-artifact`        | Uploaded artifact name.                                         |
| `app-cli-bin`             | Binary name.                                                    |
| `app-cli-package`         | Package name.                                                   |
| `app-cli-version`         | **Informational** crate version (NOT validated identity, §3.7). |
| `app-cli-source-revision` | Built revision.                                                 |
| `app-cli-workspace-id`    | Static id from `app-repository` + `workspace-root`.             |
| `app-cli-platform-id`     | Static id (§3.8): image label + **ImageVersion** + baseline.    |

`workspace-id`/`platform-id` are **static, deterministic**, so a matrix consumer computes
its own from static inputs; it never reads them from the artifact. **Matrix handoff never
uses the shared workflow outputs** (GitHub keeps only the last leg's).

**Permissions.** The reusable workflow declares job-level **`permissions: { contents: read }`**
(forcing `id-token`/all others to `none`; a caller granting `id-token: write` is tested to
still yield `none`). A called workflow can only **reduce** caller permissions, so the caller
must grant **at least** `contents: read` — documented as a caller prerequisite.
**`persist-credentials: false` is normative** (Git-persistence only, not same-UID isolation).

### 3.2 Self-composite reference

Via **`$/.github/actions/build-app-cli`** with a narrow pin-gate exemption for
`$/.github/actions/...` and a targeted actionlint suppression.

### 3.3 Workspace root, cwd, and Git identity

`workspace-root` (required under `cache: true`) is **canonicalized and confined beneath the
app Git root**; `working-directory` must resolve **beneath** it; and it is asserted **exactly
equal to `cargo metadata --format-version=1 .workspace_root`** (fail closed on any mismatch),
so the key, cleanup target, and provenance describe the workspace Cargo actually builds.
`app-repo`/Git root are the **canonical `git rev-parse --show-toplevel`** from
`working-directory` (defined for both producers, including nested checkouts).

### 3.4 Stable, deterministic paths

`CARGO_HOME` and `CARGO_TARGET_DIR` are **deterministic, identity-scoped stable paths outside
the per-invocation workspace**, exported **unchanged from preflight through post-save** (a
per-invocation path would enter rust-cache's env/path hash and defeat warm restores; deleting
before the post hook would break save). The target is **reset before every restore and NEVER
after**; dependency artifacts survive post-save cleanup.

### 3.5 rust-cache pin, key, writer authorization, metadata guards

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`**.

- `cache-bin: false`; `cache-workspace-crates: false`.
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of: `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, workspace-root `Cargo.toml` content hash, **`platform-id` (image label +
  `ImageVersion` + `x86-64` baseline)**, and the bounded `cache-key-suffix`. Including
  `ImageVersion` means a weekly image rollout starts a fresh cache (accepted cost) rather than
  restoring objects built against different system libraries.
- `workspaces:` maps the canonical workspace root to the stable target (relative path).
  `RUSTUP_TOOLCHAIN` exported for restore, compile, post-save.

**Writer authorization — fail before compiling.** Caching runs only when the event is `push`,
`workflow_dispatch`, or `schedule` on a **protected deployer ref** **and** `HEAD == resolved
app SHA`. A `workflow_dispatch` on an unprotected/user-selected branch, a fork PR, or any
other context **does not run the cached workflow**.

**Metadata guards.** A **`cargo metadata --locked` preflight** must succeed (fail closed), and
**`Cargo.lock` is verified byte-identical after restore** (fail closed), bounding rust-cache's
swallowed bare-`metadata` (which runs before restore and in the post hook). The parent's
"all Cargo commands `--locked`" is scoped to our invocations.

**Empty-save residual (accepted):** a post-hook metadata failure can publish an empty
immutable entry — recover via `cache-key-suffix` rotation; owned/forked save = §7.

### 3.6 Cargo config/source closure and codegen baseline

- **Isolated deterministic `CARGO_HOME`** (§3.4).
- **Full-chain config closure with a SAFE-KEY ALLOWLIST.** Cargo reads `config`/`config.toml`
  from the invocation dir up through **every filesystem parent to `/`** plus `CARGO_HOME`
  (in the cross-repo layout, deployer-owned config **above** the app workspace is effective).
  Under `cache: true` the effective config is scanned over that full chain and **fails closed
  unless every present key is on an explicit safe allowlist** (registries, source registry
  URLs, `net.*`, `http.*` retry/timeout). Anything else — `[env]`, `build.rustc`/`rustc-wrapper`/
  `rustc-workspace-wrapper`, `build.rustflags`, `[target.<triple>]` rustflags/runner/linker,
  `[profile.*]` overrides, `paths`, source replacement/mirrors, `include` — **fails closed**.
- **Reject external path dependencies** whose canonical source is outside the workspace root.
- **Reject every incoming Rust-flag channel** (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
  `CARGO_BUILD_RUSTFLAGS`, target-qualified rustflags, `[build.rustflags]`) and **inject one
  canonical baseline**; reject `build.target`/`CARGO_BUILD_TARGET` (implicit host only) and
  raised `target-cpu`/`target-feature`. **Native/assembly portability** (a `build.rs` invoking
  a C compiler with its own `-march`) is an **application responsibility**, bounded by the
  same-image requirement (§3.8), not hermetically sandboxed in v1.
- **Confined build dirs** (`CARGO_TARGET_DIR` + `build.build-dir` to the owned root; Cargo ≥
  1.91). Executable = the single JSON `compiler-artifact` after `build-finished`.

### 3.7 Source cleanliness, provenance, and public actions

**Cleanliness.** A canonical Git root is required and source is asserted **clean — tracked,
untracked, and recursive submodules — BEFORE and AFTER all app-controlled commands** (a
`build.rs` can mutate source after an initial check); dirty at either point fails closed.

**Provenance.** `app-cli-meta.json` (both producers) carries a schema version and `app-repo`,
`source-revision`, `app-cli-package`, `app-cli-bin`, `app-cli-version` (informational),
`workspace-id`, `platform-id`, plus **runtime ABI metadata** (§3.8). Identity **validated** by
consumers is `app-repo`, `source-revision`, `app-cli-package`, `app-cli-bin`, `workspace-id`,
`platform-id` — **not** `app-cli-version` (runtime-derived; not an externally-known handoff
identity).

**`validate-app-cli-provenance` action**

| Input                      | Req. | Meaning                               |
| -------------------------- | ---- | ------------------------------------- |
| `artifact-tar`             | yes  | Path to **exactly one** artifact tar. |
| `expected-app-repo`        | yes  | —                                     |
| `expected-source-revision` | yes  | —                                     |
| `expected-app-cli-package` | yes  | —                                     |
| `expected-app-cli-bin`     | yes  | —                                     |
| `expected-workspace-id`    | yes  | —                                     |
| `expected-platform-id`     | yes  | —                                     |

Behavior: extract into a **fresh action-owned root** (unique expected members = the binary +
`app-cli-meta.json`; reject traversal/symlinks/special files); strict-schema parse (reject
unknown schema); **recompute the ELF's `DT_NEEDED` + required glibc symbol versions from the
extracted binary** and check them against the running environment (and the recorded metadata);
compare every expected field (never defaulting from the artifact); fail closed on any mismatch.
Output: `app-cli-path` — a canonical regular executable **confined beneath the owned root**. No
CLI execution.

**`active-version-fastly` action** — inputs: `artifact-tar`, all `expected-*` identity,
`service-id`, `fastly-api-token`; runs `validate-app-cli-provenance`, then the validated CLI's
`active-version`; output `version` (**empty on first-ever deploy = success**), non-zero on
failure. Recovery calls this action.

Every consumer (deploy-fastly, config-push, healthcheck, rollback, recovery) supplies the
`expected-*` inputs (checkout-backed consumers derive repo/revision/workspace from their
checkout; checkout-less consumers and matrix legs pass the deterministic values) and validates
**before any downloaded CLI runs** (incl. `--help`) and before credentials.

### 3.8 Runner and ABI

- **Cached (reusable-workflow) path:** fixed literal **`ubuntu-24.04`**; verify GitHub-hosted
  and `ImageOS`/`ImageVersion` present (fail closed). `platform-id` = canonical(image label,
  **`ImageVersion`**, `x86-64`). **Consumers must run on the same literal image AND the same
  `ImageVersion`** (exact equality; cross-image directional analysis = §7); the validator's
  recomputed `DT_NEEDED`/glibc-symver check is the defense-in-depth backstop.
- **Direct-composite path (unchanged runner support):** the existing composite still supports
  Linux x86-64 including **ephemeral self-hosted**. It records its **actual runtime
  environment** (OS/arch/hosted-status/`ImageOS`/`ImageVersion` when hosted, glibc) as its
  `platform-id`/ABI metadata, and its consumers require an **exact environment match**. The
  hosted-`ubuntu-24.04` requirement is **cached-path only**; the direct path is a separate ABI
  policy (no breaking change).

### 3.9 Security preconditions (operator-enforced)

- **Cache reader trust.** Fork PRs can read the deployer's base-branch caches; another
  PR-triggered workflow in the deployer repo can restore them. `cache: true` requires the
  deployer to **prohibit untrusted-PR workflow execution** or that all cached dependency source
  is **disclosure-safe** (no private deps).
- **Artifact disclosure (independent of caching).** The compiled CLI artifact belongs to the
  **deployer** workflow; anyone with read on the deployer repo can download it. A **public
  deployer building a private app** therefore exposes the private binary even with
  `cache: false`. The workflow **fails closed** when the app is private (private
  `app-repository` / a checkout PAT) and the deployer repo is public (repo visibility is
  queryable), unless the caller explicitly opts into the disclosure.

## 4. Testing

- **Contract/permissions:** exact I/O/types (the tables above) before the golden test; a caller
  granting `id-token: write` yields `none`; a caller granting less than `contents: read` fails;
  matrix legs compute their own static `workspace-id`/`platform-id` and use **unique** artifact
  names (a real two-leg handoff); cross-repo (+PAT) warm second run; private fail-closed.
- **Writer authz:** unauthorized event, unprotected `workflow_dispatch` ref, or `HEAD !=
resolved SHA` **fails before compilation**; fork-PR does not run cached.
- **Cache/key:** reset-before / no-reset-after / deps-survive-post; tuple distinguishes
  `(foo-bar,baz)`/`(foo,bar-baz)`; **`ImageVersion` rollover busts the key**; root-`Cargo.toml`
  profile mutation with unchanged `Cargo.lock` busts the key.
- **Workspace/config/source:** `workspace-root` ≠ `cargo metadata.workspace_root` fails; a
  config **above** the app workspace setting a wrapper/rustflags/env/profile/target-link/source-
  replacement fails closed (allowlist); external path dep fails; every rustflag channel + raised
  CPU fails; preflight `--locked` failure and post-restore `Cargo.lock` mutation fail; **dirty
  source (tracked, untracked, submodule, and build-script mutation) fails before and after**.
- **Provenance/ABI/APIs:** one-tar/owned-root/no-traversal/confined-executable; recomputed
  `DT_NEEDED`/symver; rejects wrong repo/revision/package/bin/workspace/platform and unknown
  schema (incl. same-repo/SHA wrong-package); every consumer validates **before `--help`, CLI
  execution, and credential exposure**; `active-version-fastly` empty-version = success; a
  non-`ubuntu-24.04`/older-`ImageVersion` consumer is rejected; both producers emit
  `workspace-id`/`platform-id`; direct and cached ABI policies.

## 5. Docs and migration

- Scope the parent's exact-key/target-only caching language to **`deploy-fastly.cache`**; define
  **`build-app-cli.cache`** as a separate rolling, deployer-owned cache; and add the parent's
  **runner and provenance** updates (the reusable workflow owns checkout/runner/timeout; the new
  provenance outputs and validation/active-version actions; the same-image cached-path policy).
- Correct the guide/adoption-guide "consumers own checkout/runner/timeout; actions never call
  `checkout`" claims. Document the three checkout cases (same-repo default token; public
  cross-repo no token; private cross-repo PAT), the reader/artifact-disclosure preconditions,
  and the two-job topology.
- Pin gate/`zizmor`/actionlint: rust-cache SHA + non-SHA regression; `$/` carve-outs.
- Public-surface golden: the tables in §3.1/§3.7 (exact names/types/defaults) and both actions.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build, a warm run (same image
version) restores compiled **dependencies** (the bulk of the ~10 min); app + workspace-local
crates recompile.

## 7. Out of scope / future

- An owned/forked rust-cache **save** that refuses to save on its own metadata failure.
- **Cross-image / directional** ABI compatibility (real `DT_NEEDED`/symbol-version/CPU analysis)
  and hermetic native-build sandboxing; a self-hosted trust mode with an immutable image.
- `cli-profile`; private-registry/git dependency authentication; non-Fastly adapters.

## 8. History

… v6.5 (decisions) → v6.6 (runtime-token correction, canonical-tuple key) → v6.7 (static
platform/workspace ids, config/source closure start, locked-metadata guard, hosted permissions,
same-image ABI, hardened extraction) → **v6.8**: `ImageVersion` in the key with exact-equality
consumption and ELF metadata recomputed from the binary; `app-cli-version` demoted to
informational + unique matrix artifact names; `workspace-root` required/canonical/`==
cargo metadata.workspace_root`; full-chain (cwd→/) config closure via a **safe-key allowlist**;
clean source **before and after** incl. submodules; the private-app/public-deployer
artifact-disclosure precondition; deterministic stable `CARGO_HOME`; reject **all** rustflag
channels (native/assembly = app responsibility); the direct composite keeps its runner support
under a separate ABI policy; concrete contract tables; caller-permission and three-checkout-case
precision.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile` interface signatures; the identity/config-closure canonicalization
byte layouts and the safe-key allowlist's exact key set; and the writer-authorization predicate
expression. String/interface mechanics only.
