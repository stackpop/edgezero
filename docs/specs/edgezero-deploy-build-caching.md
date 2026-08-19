# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.9

**Related:** `docs/specs/edgezero-deploy-github-action.md`,
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI (native) with **no caching**, so every deploy
recompiles the whole dependency graph (~10 min for `stackpop/trusted-server-deployer`, which
checks out a **separate** application repo and builds its CLI). Caching must work for that
**cross-repository deployer** topology.

## 2. Trust model

- The build compiles **trusted code** (the deploy target); `build.rs` is already trusted.
- The runner injects `ACTIONS_RUNTIME_TOKEN` into the job regardless, so caching runs **only
  for authorized deployer events/refs (fail before compiling otherwise)**, where the runtime
  credential **and the narrow app-checkout PAT** are explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**, and — because every writer on a
  cache branch shares the namespace — **every workflow able to write the deployer's selected/
  default-branch cache is trusted**. The **deployer's protected workflow** is responsible for
  constraining which `app-repository`/`app-ref` it builds (source **authorization** is the
  deployer's policy; the action only proves checkout **fidelity**, §3.6).

A reusable workflow (`on: workflow_call`) owns the build job, which runs in a **pinned
container** (§3.7) so the build/consume ABI is an immutable digest, not a mutable hosted image.

## 3. Design

### 3.1 Reusable-workflow contract

| Input                     | Type    | Req.         | Default        | Meaning                                                                                                   |
| ------------------------- | ------- | ------------ | -------------- | --------------------------------------------------------------------------------------------------------- |
| `app-repository`          | string  | no           | caller repo    | `owner/repo`; resolved to an immutable repo id (§3.5).                                                    |
| `app-ref`                 | string  | if repo      | —              | Full commit SHA.                                                                                          |
| `working-directory`       | string  | no           | `.`            | Compile cwd; must resolve beneath `workspace-root`.                                                       |
| `workspace-root`          | string  | **yes**      | —              | Cargo workspace root, relative to the checkout (§3.5).                                                    |
| `app-cli-package`         | string  | yes          | —              | Cargo package.                                                                                            |
| `app-cli-bin`             | string  | no           | package name   | Binary target.                                                                                            |
| `rust-toolchain`          | string  | no           | `auto`         | Toolchain or `auto` (recorded in identity, §3.8).                                                         |
| `app-cli-artifact`        | string  | yes (matrix) | `edgezero-cli` | Artifact name; **unique per matrix leg**.                                                                 |
| `cache`                   | boolean | no           | `false`        | Enable caching.                                                                                           |
| `cache-key-suffix`        | string  | no           | `""`           | Namespaces the cache.                                                                                     |
| `disclosure-acknowledged` | boolean | no           | `false`        | Operator consent that the deployer's artifact/cache reader set may see the (possibly private) app (§3.9). |
| `timeout-minutes`         | number  | no           | `30`           | Job timeout (explicit numeric default; omitted ⇒ 0 in workflow-call, so a default is set).                |

Secret: `app-checkout-token` — narrow `contents: read` PAT, required **only for a private
cross-repository** app (a private **same-repository** app uses the default `GITHUB_TOKEN`;
public cross-repo needs none); a private cross-repo app without it fails closed.

Outputs: `app-cli-artifact`, `app-cli-bin`, `app-cli-package`, `app-cli-version`
(**informational, not validated**), `app-cli-source-revision`, `app-cli-workspace-id`,
`app-cli-platform-id` (= the **container digest**, §3.7), `app-cli-toolchain-id` (§3.8).
`workspace-id`/`platform-id`/`toolchain-id` are **static/deterministic** (container digest is
known from the pinned reference), so a matrix consumer computes its own; **matrix handoff never
uses the shared workflow outputs**. `workspace-root` is required for **all** reusable-workflow
handoffs (not only `cache: true`) so `workspace-id` is always computable.

**Permissions.** Job-level **`permissions: { contents: read }`** (forces `id-token`/all to
`none`; tested against a caller granting `id-token: write`). A called workflow can only
**reduce** caller permissions, so the caller must grant **≥ `contents: read`** (a caller
prerequisite). `persist-credentials: false` normative (Git-persistence only).

### 3.2 Self-composite reference

`$/.github/actions/build-app-cli` with a narrow pin-gate exemption for `$/.github/actions/...`
and a targeted actionlint suppression.

### 3.3 Stable, deterministic paths

`CARGO_HOME` and `CARGO_TARGET_DIR` are **deterministic identity-scoped stable paths** exported
unchanged from preflight through post-save; the target is **reset before every restore, never
after**; deps survive post-save.

### 3.4 rust-cache pin, key, metadata guards

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

- `cache-bin: false`; `cache-workspace-crates: false`.
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, **only the codegen-critical root-manifest sections** (`[profile.*]`, `[patch]`,
  workspace codegen config — **not** the whole `Cargo.toml`, so ordinary dependency edits leave
  rust-cache's rolling restore-prefix usable), `platform-id` (container digest), and
  `toolchain-id`. `cache-key-suffix` is appended so it can be rotated. (rust-cache's own exact
  suffix keys the lockfile/deps.)
- `workspaces:` maps the canonical workspace root to the stable target; `RUSTUP_TOOLCHAIN`
  exported for restore/compile/post-save.
- **Metadata preflight mirrors rust-cache exactly.** The guard runs the **same** invocation
  rust-cache uses — `cargo metadata --format-version=1 --all-features --locked` from the
  **workspace root** with `CARGO_ENCODED_RUSTFLAGS=""` — so success **proves** rust-cache's
  pre-restore and post-hook metadata will succeed; its result is reused. `Cargo.lock` is
  verified byte-identical after restore. (Fail closed on either.)
- **Empty-save residual accepted** (rotate `cache-key-suffix`; owned save = §10).

### 3.5 Identity (three distinct concepts)

- **`git-root`** — the local path (`git rev-parse --show-toplevel` from `working-directory`),
  for path confinement only.
- **`app-repo`** — the canonical `owner/repo` full name.
- **`app-repo-id`** — the **immutable GitHub numeric repository id** (resolved via the API/
  context from `app-repo`), used in the key and provenance. Both producers resolve it the same
  way, so nested checkouts and the direct composite agree.

`workspace-root` is canonicalized, confined beneath `git-root`, `working-directory` beneath it,
and asserted **exactly equal to `cargo metadata.workspace_root`**. `workspace-id` = canonical
hash of `app-repo-id` + the workspace-root path relative to `git-root`.

### 3.6 Writer authorization (fidelity) vs. source authorization (policy)

- **Action-enforced fidelity:** caching runs only when the event is `push`/`workflow_dispatch`/
  `schedule` on a **protected deployer ref** **and** `HEAD == resolved app SHA`. Otherwise the
  cached workflow does not run (fail before compilation).
- **Deployer-enforced authorization:** `HEAD == SHA` only proves the requested SHA was checked
  out — it does **not** prove the SHA/repo is trusted. The **deployer's protected workflow must
  constrain/allowlist the `app-repository`/`app-ref`** it will build+cache. The spec documents
  that **every writer on the selected/default cache branch is trusted**, so a deployer that lets
  `workflow_dispatch` build an arbitrary repo into its cache namespace has authorized that
  writer. This division is stated normatively in the guide.

### 3.7 Container image and ABI

- The cached build job runs in a **pinned container referenced by digest**
  (`container: image@sha256:…`), so the toolchain, glibc, linker, and system libraries are an
  **immutable** identity. **`platform-id` = the container digest** (static, in the key/outputs).
- **Consumers must run the binary against the same container digest** (the deploy/lifecycle
  steps that execute the CLI use the same pinned container), so DT_NEEDED libraries, glibc, and
  CPU-visible ISA are reproducible — no dependence on the mutable hosted image. The validator
  still **recomputes** the ELF's `DT_NEEDED` + glibc symbol versions from the extracted binary
  as defense in depth.
- **Direct composite:** unchanged; for cross-job reuse it must record an **operator-supplied
  immutable environment id** (e.g. the same container digest) or be consumed **same-job**;
  otherwise its artifact is not guaranteed portable. Hosted-runner ABI is not a portability
  guarantee.

### 3.8 Cargo config/source closure and codegen baseline

- Isolated deterministic `CARGO_HOME` (§3.3).
- **Config closure — full chain, safe-key ALLOWLIST, env included.** Cargo config
  (`config`/`config.toml`) is read from the cwd up through **every parent to `/`** plus
  `CARGO_HOME`, **and** Cargo honors `CARGO_*` environment overrides. Under `cache: true` the
  effective configuration (files **and** `CARGO_*` env) fails closed unless every present key is
  on a **minimal explicit allowlist**: registry **index** URLs (no credential providers),
  `net.retry`/`net.offline`, `http.timeout`/`http.check-revoke`. Explicitly **rejected**:
  `net.git-fetch-with-cli`, any credential provider, `[env]`, `build.rustc*`/wrappers,
  `build.rustflags`, `[target.*]` rustflags/runner/linker, `[profile.*]` overrides, `paths`,
  source replacement/mirrors, `include`, `build.target`/`build.build-dir`, and any `CARGO_*`
  variable that mirrors a rejected key.
- **Reject external path dependencies** outside the workspace root.
- **Reject all Rust-flag channels** (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
  `CARGO_BUILD_RUSTFLAGS`, target-qualified, `[build.rustflags]`) and inject one baseline;
  reject raised `target-cpu`/`target-feature`. Native/assembly portability (a `build.rs` C
  compiler with `-march`) is an **application responsibility**, bounded by the fixed container.
- Confined build dirs (Cargo ≥ 1.91). Executable = the single JSON `compiler-artifact` after
  `build-finished`.
- **Source freezing.** Require a canonical Git root and assert the **initial `HEAD` SHA is
  UNCHANGED** and the tree **clean (tracked + untracked + recursive submodules)** **before and
  after** all app-controlled commands (a `build.rs` can `checkout` a different commit and leave
  a clean tree relative to the new HEAD; comparing HEAD identity catches that). Reject symlinks
  escaping the workspace. The residual (in-revision `include!`/`#[path]` to escaping paths) is
  covered by the trust model, and provenance is a **consistency check, not tamper-proof**.

### 3.9 Provenance, disclosure, and public actions

**Versioned JSON schema `app-cli-meta.json`** (schema `v1`; all fields required, non-null, in a
fixed key order; strings unless noted):

```
schema: "edgezero.app-cli-meta/v1"
app-repo            (owner/repo)      app-repo-id (number)   source-revision (40-hex)
app-cli-package     app-cli-bin       app-cli-version (informational)
workspace-id        platform-id (container digest)          toolchain-id
producer-mode       ("reusable-workflow" | "composite")
abi: { libc-family: "gnu", glibc-version, dt-needed: [string], glibc-symver: [string] }
```

**Validated identity** (by consumers) = `app-repo-id`, `source-revision`, `app-cli-package`,
`app-cli-bin`, `workspace-id`, `platform-id`, **`toolchain-id`** (so a different toolchain fails
validation) — **not** `app-cli-version`.

**`validate-app-cli-provenance` action** — inputs: `artifact-tar` (exactly one) + required
`expected-app-repo-id`, `expected-source-revision`, `expected-app-cli-package`,
`expected-app-cli-bin`, `expected-workspace-id`, `expected-platform-id`, `expected-toolchain-id`.
Extract into a fresh owned root (unique expected members; reject traversal/symlink/special),
strict-schema parse (reject unknown schema), recompute ELF DT_NEEDED/symver from the binary,
compare every expected field, fail closed on mismatch. Output: `app-cli-path` (canonical regular
executable beneath the owned root). No CLI execution.

**`active-version-fastly` action** — inputs: `artifact-tar`, all `expected-*`,
**`fastly-service-id`** (matching the existing convention, not `service-id`), `fastly-api-token`;
runs the validator, then `active-version`; output `version` (empty on first deploy = success).

**Every existing consumer** (deploy-fastly, config-push, healthcheck, rollback, recovery) gains
the same `expected-*` inputs (checkout-backed consumers derive repo/revision/workspace from
their checkout; checkout-less consumers and matrix legs pass the deterministic values;
`platform-id`/`toolchain-id` are the deterministic container/toolchain values) and validates
**before any downloaded CLI runs** (incl. `--help`) and before credentials. Exact per-consumer
input tables are added in the plan's public-surface work and enumerated in the golden test.

**Artifact disclosure.** The compiled CLI artifact belongs to the deployer workflow and is
downloadable by **any deployer-repository reader** (broader than "public deployer" — internal/
private deployers can still have a wider reader set than the app). For any **private
cross-repository** app the workflow **requires `disclosure-acknowledged: true`** (operator
consent that the deployer's reader set may see the app) or **fails closed**. Visibility is not
inferred from PAT presence.

## 4. Testing

Contract/permissions (exact tables; caller `id-token: write` ⇒ `none`; caller < `contents:
read` fails; unique matrix artifacts + real two-leg handoff computing static ids; three
checkout cases; `disclosure-acknowledged` required for private cross-repo). Writer fidelity
(unauthorized/unprotected-dispatch/`HEAD != SHA` fail before compile). Cache/key (reset order;
deps survive post; codegen-only root-section hash leaves the rolling prefix usable; container
**digest change** busts the key; toolchain change busts the key). Config/source (a wrapper/
rustflags/env/profile/target-link/source-replacement/`git-fetch-with-cli`/credential-provider
anywhere up to `/` or via `CARGO_*` fails closed; external path dep fails; preflight mirrors
rust-cache and its failure fails closed; post-restore `Cargo.lock` mutation fails; **HEAD
change or dirty tree/submodule before or after fails**; escaping symlink fails). Provenance/ABI
(one-tar/owned-root/no-traversal/confined; recomputed DT_NEEDED/symver; wrong repo-id/revision/
package/bin/workspace/platform/**toolchain**/unknown-schema rejected; every consumer validates
before `--help`/exec/creds; a **different container digest** consumer rejected; both producers
emit the schema). ABI/direct (direct cross-job reuse without an immutable env id is not
portable).

## 5. Docs and migration

Scope the parent's exact-key/target-only caching language to `deploy-fastly.cache`; define
`build-app-cli.cache` as a separate rolling, deployer-owned cache; add the parent's **runner
(container), provenance, and writer-authorization-division** updates. Correct the "consumers own
checkout/runner/timeout; actions never call `checkout`" claims. Document the container image, the
three checkout cases, the `disclosure-acknowledged` requirement, and the two-job topology. Pin
gate/`zizmor`/actionlint: rust-cache SHA + non-SHA regression, `$/` carve-outs. Public-surface
golden: the §3.1/§3.9 tables + every consumer's `expected-*` inputs + both actions.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build in the pinned container,
a warm run restores compiled **dependencies** (the bulk of the ~10 min); app + workspace-local
crates recompile.

## 7. Out of scope / future

Owned/forked rust-cache **save** refusing an empty save; hermetic native-build sandboxing;
directional cross-container ABI; `cli-profile`; private-registry/git dependency authentication;
non-Fastly adapters.

## 8. History

… v6.6 (canonical-tuple key) → v6.7 (static ids, config/source closure start, same-image ABI)
→ v6.8 (ImageVersion in key, full-chain config allowlist start, clean-before/after, contract
tables) → **v6.9**: pinned **container digest** as `platform-id` (immutable two-job ABI,
resolving hosted-image rollover and self-hosted ABI); split `git-root`/`app-repo`/immutable
`app-repo-id`; minimal config allowlist incl. `CARGO_*` env and rejecting `git-fetch-with-cli`/
credential providers; `disclosure-acknowledged` consent; `expected-*` for every consumer +
`workspace-root` always required; writer fidelity vs deployer authorization division; preflight
mirrors rust-cache's exact metadata call; assert unchanged `HEAD` before/after; hash only
codegen-critical root sections; complete versioned schema incl. `toolchain-id`; `timeout-minutes`
default, PAT scoped to private cross-repo, `fastly-service-id`.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile` signatures; the identity/allowlist canonicalization byte layouts and
the exact safe-key set; per-consumer `expected-*` input tables (enumerated, from §3.9); and the
writer-fidelity predicate expression.

## 10. Future: owned save

An owned/forked rust-cache save phase that refuses to save on its own post-hook metadata
failure would eliminate the accepted empty-save residual (§3.4).
