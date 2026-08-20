# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.10

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
- Caching runs **only for authorized deployer events/refs (fail before compiling otherwise)**,
  where the runtime credential and the narrow app-checkout PAT are explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**; every writer on the selected/default
  cache branch is trusted, and the **deployer's protected workflow** allowlists which
  `app-repository`/`app-ref` it builds (source **authorization** = deployer policy; the action
  proves checkout **fidelity**, §3.6).

The reusable workflow (`on: workflow_call`) owns the build job in a **pinned container**, and
**CLI-executing consumers also run through the same pinned container** (§3.7) so build and
run ABI are one immutable digest.

## 3. Design

### 3.1 Reusable-workflow contract

| Input                     | Type    | Req.         | Default        | Meaning                                                                                                                                                     |
| ------------------------- | ------- | ------------ | -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `app-repository`          | string  | no           | caller repo    | `owner/repo`.                                                                                                                                               |
| `app-ref`                 | string  | if repo      | —              | Full commit SHA.                                                                                                                                            |
| `app-repo-id`             | number  | no           | —              | Immutable GitHub repo id; if given, the producer **verifies** it against its API lookup and outputs it; matrix/checkout-less consumers pass the same value. |
| `working-directory`       | string  | no           | `.`            | Compile cwd; beneath `workspace-root`.                                                                                                                      |
| `workspace-root`          | string  | **yes**      | —              | Cargo workspace root, relative to the checkout.                                                                                                             |
| `app-cli-package`         | string  | yes          | —              | Cargo package.                                                                                                                                              |
| `app-cli-bin`             | string  | no           | package name   | Binary target.                                                                                                                                              |
| `rust-toolchain`          | string  | no           | `auto`         | Must resolve to a **fully pinned** version/dated channel under `cache: true` (a bare moving channel fails closed, §3.5).                                    |
| `app-cli-artifact`        | string  | yes (matrix) | `edgezero-cli` | **Unique per matrix leg.**                                                                                                                                  |
| `cache`                   | boolean | no           | `false`        | Enable caching.                                                                                                                                             |
| `cache-key-suffix`        | string  | no           | `""`           | Length/charset-validated (≤64, `[A-Za-z0-9._-]`) then hashed.                                                                                               |
| `disclosure-acknowledged` | boolean | no           | `false`        | Operator consent when app visibility ≠ public (§3.9).                                                                                                       |
| `timeout-minutes`         | number  | no           | `30`           | Job timeout.                                                                                                                                                |

Secret `app-checkout-token`: narrow `contents: read` PAT, required for a **non-public
cross-repository** app (private **or internal**); a private **same-repository** app uses the
default `GITHUB_TOKEN`; public cross-repo needs none; otherwise fail closed.

Outputs: `app-cli-artifact`, `app-cli-bin`, `app-cli-package`, `app-cli-version`
(**informational**), `app-cli-source-revision`, **`app-cli-repo-id`**, `app-cli-workspace-id`,
`app-cli-platform-id` (container digest), `app-cli-toolchain-id`. The identity values are
static, so a **`compute-app-cli-identity`** helper action (§3.9) lets a matrix caller derive
its own `expected-*` without the shared outputs (which hold only the last matrix leg).
`workspace-root` is required for **all** handoffs.

**Permissions:** job-level **`permissions: { contents: read }`** (forces `id-token`/all →
`none`; tested against a caller granting `id-token: write`); the caller must grant ≥
`contents: read` (a called workflow can only reduce). `persist-credentials: false` normative.

### 3.2 Self-composite reference

`$/.github/actions/build-app-cli` with a narrow pin-gate exemption for `$/.github/actions/...`
and a targeted actionlint suppression.

### 3.3 Paths (rust-cache-relative)

In the pinned container, `CARGO_HOME` is the container's **default clean `~/.cargo`** (the
container has no runner-global config) and `CARGO_TARGET_DIR` is the workspace's own
`target/` — both cached by rust-cache with **paths relative to the workspace root** (v2.9.2
joins the `workspaces:` RHS beneath the root, so an absolute path resolves wrong). The
checkout path is stable, so these are stable. Target reset before every restore, never after.

### 3.4 rust-cache pin, key, metadata guard

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

- `cache-bin: false`; `cache-workspace-crates: false`.
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, **codegen-critical root-manifest sections only** (`[profile.*]`, `[patch]`,
  workspace codegen config), `platform-id` (container digest), `toolchain-id`, and the
  validated `cache-key-suffix`. (rust-cache's exact suffix keys the lockfile/deps; the fixed
  tuple hash keeps the total key well under GitHub's 512-char limit.)
- `RUSTUP_TOOLCHAIN` exported for restore/compile/post-save.
- **Metadata guard (independent bound, not reuse).** rust-cache runs its **own** bare
  `cargo metadata` (with `--no-deps` pre-restore, differently at save) and has **no API to
  reuse an external result**; the workflow therefore runs an **independent**
  `cargo metadata --format-version=1 --all-features --locked` (workspace root,
  `CARGO_ENCODED_RUSTFLAGS=""`) that must succeed — this **bounds** but does not **prove**
  rust-cache's calls succeed; the honest residual is documented. `Cargo.lock` is verified
  byte-identical after restore. `net.offline=true` is **rejected** (it would fail the
  pre-restore metadata against an initially empty `CARGO_HOME`).
- **Empty-save residual accepted** (rotate `cache-key-suffix`; owned save = §10).

### 3.5 Identity

- **`git-root`** (local path), **`app-repo`** (`owner/repo`), **`app-repo-id`** (immutable
  numeric id): supplied by the caller and **verified** by the producer against its API lookup,
  emitted as an output, and used by matrix/checkout-less consumers (a checkout carries only the
  name, and `github.repository_id` is the deployer's, not the app's).
- `workspace-root` canonicalized, confined beneath `git-root`, `working-directory` beneath it,
  asserted **`== cargo metadata.workspace_root`**. `workspace-id` = hash(`app-repo-id`,
  workspace-root path relative to `git-root`).
- **`toolchain-id`** = hash of canonical `rustc -vV` (release, commit-hash, commit-date, host)
  - `cargo -Vv`. Under `cache: true`, `rust-toolchain` (or the resolved `auto` value from the
    app's pinned `.tool-versions`/`rust-toolchain`) **must be a fully pinned version or dated
    channel**; a bare moving channel (`stable`, `nightly`) fails closed.

### 3.6 Writer fidelity vs. source authorization

Action-enforced **fidelity**: caching runs only on `push`/`workflow_dispatch`/`schedule` on a
**protected deployer ref** with `HEAD == resolved app SHA`. Deployer-enforced
**authorization**: `HEAD == SHA` proves only that the requested SHA was checked out; the
**deployer's protected workflow must allowlist the `app-repository`/`app-ref`** — documented,
with "every cache-branch writer is trusted."

### 3.7 Container image and ABI

- **Image contract:** an EdgeZero-**published, public, retained, single-manifest `linux/amd64`**
  image, pinned by its **manifest digest** (not an index digest, which could select another
  architecture). Built from a versioned in-repo Dockerfile; it fixes the Rust toolchain, glibc,
  linker, and system libraries. **`platform-id` = that manifest digest.**
- **Producer and CLI-executing consumers both run in the pinned container.** Consumers do not
  merely compare a caller-supplied `platform-id` (GitHub exposes the container id/network, not
  its digest, and a composite cannot set the caller's container): the deploy/lifecycle CLI is
  executed **inside the pinned container** — the lifecycle steps set the job's `container:` to
  the pinned digest, or the action invokes the binary with `docker run` against that digest — so
  run ABI == build ABI on any host. A **real wrong-runtime** case is tested, not merely a wrong
  `expected-platform-id`.
- **ISA is bounded, not reproduced.** A container **shares the host CPU/kernel**, so ISA is not
  container-fixed; it is bounded by the **forced `x86-64` baseline** and rejection of raised
  `target-cpu`/`target-feature` (§3.8). A `build.rs` invoking a C compiler with `-march=native`
  can still emit host-specific code — **native/assembly portability is an application
  responsibility**, stated plainly (the container does not bound it).

### 3.8 Cargo config/source closure

- **Config closure — full chain (cwd→`/` + `CARGO_HOME`), minimal allowlist, env included.**
  Fail closed unless every present key (config files **and** `CARGO_*`/compiler env) is on the
  allowlist: registry **index** URLs (no credential providers), `net.retry`, `http.timeout`,
  `http.check-revoke`. Explicitly **rejected/unset** across preflight/restore/compile/post-save:
  `net.git-fetch-with-cli`, `net.offline`, credential providers, `[env]`, `build.rustc`,
  **`RUSTC`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTDOC`**, wrappers, `build.rustflags`,
  `[target.*]` rustflags/runner/linker, `[profile.*]` overrides, `paths`, source replacement/
  mirrors, `include`, `build.target`/`build.build-dir`. **Also reject any `.cargo/config[.toml]`
  strictly between the workspace root and `working-directory`**, so the root-run metadata and the
  cwd-run compile see the same chain.
- **Reject all Rust-flag channels** (`RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
  `CARGO_BUILD_RUSTFLAGS`, target-qualified, `[build.rustflags]`); inject one baseline; reject
  raised `target-cpu`/`target-feature`; reject external path deps outside the workspace root;
  reject `build.target`/`CARGO_BUILD_TARGET`. Confined build dirs (Cargo ≥ 1.91).
- **Source freezing:** assert the **initial `HEAD` SHA is unchanged** and the tree clean
  (tracked + untracked + recursive submodules) **before and after** all app-controlled commands;
  reject escaping symlinks. Provenance is a consistency check, not tamper-proof.

### 3.9 Provenance, disclosure, helpers

**Normative JSON schema `app-cli-meta.json`** (`$schema` = `edgezero.app-cli-meta/v1`,
`additionalProperties: false`, all fields required non-null, arrays canonically sorted, no
duplicate keys, strings bounded):

| Field             | Type   | Format / notes                                                                                          |
| ----------------- | ------ | ------------------------------------------------------------------------------------------------------- |
| `schema`          | string | `edgezero.app-cli-meta/v1`                                                                              |
| `app-repo`        | string | `owner/repo`                                                                                            |
| `app-repo-id`     | number | ≥ 1                                                                                                     |
| `source-revision` | string | 40-hex                                                                                                  |
| `app-cli-package` | string | crate name                                                                                              |
| `app-cli-bin`     | string | bin name                                                                                                |
| `app-cli-version` | string | semver (informational)                                                                                  |
| `workspace-id`    | string | 64-hex                                                                                                  |
| `toolchain-id`    | string | 64-hex                                                                                                  |
| `producer`        | object | discriminated `platform` (below)                                                                        |
| `abi`             | object | `{ machine: "x86-64", interpreter, libc: "gnu", glibc-version, dt-needed: [str], glibc-symver: [str] }` |

**Discriminated `producer.platform`:** `{ mode: "container", digest }` |
`{ mode: "same-job" }` | `{ mode: "operator-env", env-id }`. **v1's validated cross-job handoff
requires `mode: "container"`**; a `same-job`/`operator-env` artifact is rejected for cross-job
provenance unless the consumer supplies a matching `env-id`.

**`validate-app-cli-provenance` action** — inputs: `artifact-tar` (exactly one) + required
`expected-app-repo-id`, `expected-source-revision`, `expected-app-cli-package`,
`expected-app-cli-bin`, `expected-workspace-id`, `expected-platform-id`, `expected-toolchain-id`.
Extract into a fresh owned root (unique expected members; reject traversal/symlink/special);
strict-schema validate; **compare binary → recorded → running environment** (recompute ELF
`e_machine`, interpreter, `DT_NEEDED`, glibc symver from the extracted binary and require they
match both the recorded `abi` and the runtime); compare every expected field. Fail closed.
Output `app-cli-path` (canonical regular executable beneath the owned root). No CLI execution.

**`active-version-fastly` action** — inputs: `artifact-tar`, all `expected-*`,
`fastly-service-id`, `fastly-api-token`; validates, then runs `active-version` **inside the
pinned container**; output `version` (empty on first deploy = success).

**`compute-app-cli-identity` action** — inputs: `app-repository`/`app-repo-id`,
`workspace-root`, `app-cli-package`/`app-cli-bin`, the container ref, the pinned toolchain;
outputs the deterministic `workspace-id`/`platform-id`/`toolchain-id` a matrix caller passes as
`expected-*`. The full container reference is a **published constant** (repo docs + an output).

**Per-consumer expected-identity (derive vs. require):**

| Consumer           | derives (from its checkout)                | requires (inputs)                                          |
| ------------------ | ------------------------------------------ | ---------------------------------------------------------- |
| deploy-fastly      | app-repo-id, source-revision, workspace-id | app-cli-package, app-cli-bin, platform-id, toolchain-id    |
| config-push        | same                                       | same                                                       |
| healthcheck-fastly | — (checkout-less)                          | **all** `expected-*` (from the build job outputs / helper) |
| rollback-fastly    | — (checkout-less)                          | **all** `expected-*`                                       |
| recovery           | — (checkout-less)                          | **all** `expected-*`                                       |

**Artifact disclosure:** the artifact is downloadable by any deployer-repo reader. When the app
visibility is **not public** (private or internal), the workflow **requires
`disclosure-acknowledged: true`** or fails closed (independent of caching; not inferred from PAT
presence).

## 4. Testing

Contract/permissions (exact tables; `id-token: write` caller ⇒ `none`; caller < `contents:
read` fails; unique matrix artifacts + real two-leg handoff via `compute-app-cli-identity`;
same-repo/public-cross/private-cross checkout cases; `disclosure-acknowledged` required for
private **and internal**). Writer fidelity (unauthorized/unprotected-dispatch/`HEAD != SHA`
fail before compile). Cache/key (reset order; deps survive post; codegen-only root-section hash
keeps the rolling prefix; **container-digest and toolchain change** bust the key; relative
`workspaces:` target; suffix length/charset). Config/source (any wrapper/`RUSTC*`/`RUSTDOC`/
rustflags/env/profile/target-link/source-replacement/`git-fetch-with-cli`/credential-provider
anywhere up to `/` or via env fails; config between root and cwd fails; external path dep fails;
preflight failure fails; post-restore `Cargo.lock` mutation fails; **HEAD change or dirty tree/
submodule before or after fails**; escaping symlink fails). Provenance/ABI (schema validation
incl. additionalProperties/dupes/bounds; **binary↔recorded↔runtime** ABI incl. ELF machine +
interpreter; producer-mode discrimination; wrong repo-id/revision/package/bin/workspace/
platform/toolchain rejected; **a real wrong-runtime consumer is rejected**; every consumer
validates before `--help`/exec/creds). ABI/native (a `build.rs -march=native` is documented as
out of scope, not silently cached-portable).

## 5. Docs and migration

Scope the parent's exact-key/target-only caching language to `deploy-fastly.cache`; define
`build-app-cli.cache` as a rolling deployer-owned cache; add the parent's **container-runner,
provenance, producer-mode, and writer-authorization-division** updates. Correct the "consumers
own checkout/runner/timeout; actions never call `checkout`" claims. Document the container
image contract + Dockerfile source, the three checkout cases (incl. internal), the
`disclosure-acknowledged` rule, `compute-app-cli-identity`, and the two-job topology where
CLI-executing consumers run in the container. Pin gate/`zizmor`/actionlint: rust-cache SHA +
non-SHA regression; `$/` carve-outs; **the container image digest is itself pinned/checked**.
Public-surface golden: the §3.1/§3.9 tables, per-consumer inputs, and all three actions.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build in the pinned container,
a warm run restores compiled **dependencies** (the bulk of the ~10 min); app + workspace-local
crates recompile.

## 7. Out of scope / future

Owned/forked rust-cache save; hermetic native-build (`-march`) sandboxing; cross-container /
directional ABI; multi-arch images; `cli-profile`; private-registry/git dependency
authentication; non-Fastly adapters.

## 8. History

… v6.7 (static ids, closure start) → v6.8 (ImageVersion key, contract tables) → v6.9 (pinned
container as `platform-id`, identity split, disclosure consent) → **v6.10**: CLI-executing
consumers run **in** the pinned container (real ABI enforcement) + binary↔recorded↔runtime ELF
check; a public single-manifest `linux/amd64` **manifest-digest** image contract with ISA
stated as bounded-not-reproduced; pinned-toolchain requirement + `toolchain-id` from `rustc -vV`;
caller-supplied/producer-verified `app-repo-id` + output; discriminated producer `platform`
schema (container/same-job/operator-env, v1 handoff = container); disclosure for **internal**
too; reject `RUSTC*`/`RUSTDOC`/`net.offline` and config between root and cwd; relative
`workspaces:` target + clean container `CARGO_HOME`; the metadata guard is an independent
**bound, not a reuse**; normative JSON schema; `compute-app-cli-identity` helper + per-consumer
derive/require table.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/helper signatures; identity/allowlist canonicalization byte layouts;
the Dockerfile contents and its digest-pinning check; and the writer-fidelity predicate
expression.

## 10. Future: owned save

An owned/forked rust-cache save that refuses to save on its own post-hook metadata failure
would remove the accepted empty-save residual (§3.4).
