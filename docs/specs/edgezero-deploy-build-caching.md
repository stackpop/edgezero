# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.14 (sccache pivot)

**Related:** `docs/specs/edgezero-deploy-github-action.md`,
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI (native) with **no caching**, so every deploy
recompiles the whole dependency graph (~10 min for `stackpop/trusted-server-deployer`, which
checks out a **separate** application repo and builds its CLI). Caching must work for that
**cross-repository deployer** topology **and for real EdgeZero apps, whose crates are unpublished
git dependencies** (so a crates.io-only rule is unusable).

## 2. Trust model and v1 shape

- The build compiles **trusted code** (the deploy target); `build.rs` is trusted.
- Caching runs **only for authorized deployer events/refs (fail before compiling otherwise)**;
  the runtime credential and the narrow app-checkout PAT are explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**; every writer that can write the
  deployer's **current-/default-branch** cache is trusted; the deployer's protected workflow
  allowlists `app-repository`/`app-ref`.
- **The reusable workflow is the only SUPPORTED producer** (build + deploy in one **pinned
  container**, §3.6). Provenance is a **consistency check, not producer authentication** (an
  other-job archive can self-assert; attestation is §7). The direct composite is an internal `$/`
  step only.
- **GitHub-hosted `linux/amd64` runners only** (no reliable ephemeral self-hosted predicate).

## 3. Design

### 3.1 Cache mechanism: fresh target + action-owned sccache (no `target/` pruner)

Rather than caching and pruning `target/` (whose unit graph and intermediate layout Cargo treats
as **internal and unstable**), v1 uses **`sccache`** — the compiler cache Cargo itself recommends
for shared dependency acceleration:

- **`CARGO_TARGET_DIR` is FRESH every run** (an action-owned path under `RUNNER_TEMP`, never
  cached, never inside the checkout) — so there is no stale-`target/`, no source-in-target, no
  workspace-crate-output, and no unit-graph classification problem.
- **`RUSTC_WRAPPER` is set (action-owned) to a pinned `sccache`** baked into the container.
  `sccache` stores compiled rustc outputs in `SCCACHE_DIR` (an action-owned path), **keyed by the
  content of the preprocessed source + compiler + flags**. Correctness is content-addressed:
  restoring an older `SCCACHE_DIR` is always safe (a cached object is used only when its inputs
  match), so there is no immutable-cache staleness and **no custom pruning**. `sccache` bounds its
  own size (`SCCACHE_CACHE_SIZE`, LRU) — the cached directory is self-managing.
- **Cache contents = `SCCACHE_DIR` only** (compiled objects + sccache's index). **No `.crate`
  sources, no `registry/src`, no `git/*`, no `CARGO_HOME/bin`, no config, no credentials** are
  cached — so a cold build's `registry/src` extraction is irrelevant to the audit, and **no
  dependency source is ever cached** (only compiled objects). Re-downloading crates each run is the
  small remaining cost; caching `.crate` archives is §7.
- **Any dependency source is supported** (crates.io, the public **EdgeZero git repo** the generator
  emits, other git deps) — sccache caches their compilation regardless of source. The old
  crates.io-only restriction is **removed**; `cache: false` and `cache: true` resolve dependencies
  identically (caching never changes resolution).

### 3.2 Own restore + save, coarse rolling key

`actions/cache/restore` + `save` over **`SCCACHE_DIR` only**:

- **Key** = `edgezero-sccache-v1-<platform-id>-<suffix-hash>-<generation>`, restore-keys prefix
  `edgezero-sccache-v1-<platform-id>-<suffix-hash>-`. `<generation>` is `github.run_id` (unique per
  run), so each run **saves a fresh generation** (never colliding with an immutable prior entry)
  and **restores the newest matching prefix**. `platform-id` = the container digest (which encodes
  toolchain + ABI); `suffix-hash` = the validated `cache-key-suffix`. No lockfile/manifest hashing
  is needed — sccache content-addresses internally.
- **Restore → audit → build → best-effort save.** After restore, **audit** that the restored path
  is exactly `SCCACHE_DIR` and contains only sccache's blob/index layout (fail closed / **discard
  and build cold once** on a corrupt or unexpected restore). After the build, `actions/cache/save`
  under the run's `<generation>` key is **best-effort** (its failures are warnings). Bump the
  `-v1-` namespace whenever the mechanism changes.

### 3.3 Action-owned Cargo/sccache environment

The build runs under a **constructed minimal environment** (`env -i` + an explicit allowlist),
not scrub-then-reject, so there is nothing to miss: only the action-owned variables and an
allowlist of benign ones exist. Action-owned (fixed, exact): `CARGO_HOME`, `CARGO_TARGET_DIR`
(fresh), `HOME`, `TMPDIR`, `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE`, `RUSTC_WRAPPER=sccache`,
`RUSTUP_TOOLCHAIN`, `CARGO_ENCODED_RUSTFLAGS=""`, `CARGO_INCREMENTAL=0`. A **caller-supplied**
`RUSTC`/`RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER`/`RUSTDOC`/`RUSTFLAGS`/native-tool var simply is
**not present** in the constructed env (never inherited). The effective **Cargo config** over the
full chain (cwd → `/`, incl. the working directory, plus `CARGO_HOME`) must contain only benign
allowlisted keys (registry index URLs, `net.retry`, `http.timeout`/`check-revoke`); anything else
fails closed. Default-features-only; `Cargo.lock` must be a tracked, regular file. External path
deps outside the workspace root are rejected. Fixed internal container paths (`CARGO_HOME`,
`CARGO_TARGET_DIR`, `HOME`, writable `/tmp`).

### 3.4 Identity

`git-root` (path, confinement); `app-repo` (`owner/repo`); **`app-repo-id`** (canonical decimal
**string**, always required, **verified via the GitHub REST API to belong to `app-repository`**).
`workspace-root` canonicalized, confined beneath `git-root`, `working-directory` beneath it,
asserted `== cargo metadata.workspace_root`. `workspace-id` = hash(`app-repo-id`, workspace-root
rel `git-root`). `platform-id` = the container digest, **read inside every action from `image.json`
at the same EdgeZero SHA — never caller-supplied**; `container-ref` = `<repo>@<platform-id>`.

### 3.5 Writer fidelity vs. source authorization

Cache runs only on `push`/`workflow_dispatch`/`schedule` on a **protected deployer ref** with
`HEAD == resolved app SHA` (action fidelity); the deployer's protected workflow allowlists the app
identity, and every writer of the deployer's **current-/default-branch** cache scope is trusted
(deployer authorization). Normative in the guide.

### 3.6 Container, runner, launcher

- **Image:** EdgeZero-published, **public** (anonymous pull) + retained, single-manifest
  `linux/amd64`, pinned by **manifest digest**, from a versioned in-repo Dockerfile baking the
  pinned Rust toolchain, `wasm32-wasip1`, the pinned **`sccache`**, the pinned **Fastly CLI**
  (`versions.json`), and `git jq tar curl cc`. Run **`--read-only`, non-root**, explicit writable
  mounts only. `platform-id` = its digest.
- **Runner: GitHub-hosted `linux/amd64` only** (fail closed on self-hosted). Host-level job, local
  Docker daemon.
- **One launcher `run-app-cli-in-container`** with **enumerated mounts** (never `RUNNER_TEMP`
  wholesale):
  - **Writable working COPY of the checkout.** The CLI runs arbitrary manifest commands via
    `sh -c` in the manifest root and may create `dist/`, `node_modules/`, generated manifests,
    etc. — so the working directory is a **disposable writable copy (or overlay)** of the app
    checkout, not read-only source. The **read-only original** is used for the before/after source
    checks (§3.7). (v1 alternative: prohibit manifest-command overrides; the writable overlay is
    preferred.)
  - **Other writable (specific):** `CARGO_TARGET_DIR`, `CARGO_HOME`, `SCCACHE_DIR`, a Fastly/
    provider `HOME`, a package/output dir. **Read-only:** the validated CLI binary, and — for
    config-push — the **specific inline-config temp file** (by exact path). UID/GID mapping so the
    non-root container user owns the mounts.
  - **env:** only the required provider token + `EDGEZERO_*`; no GitHub file-command channels
    inside the container.
  - **signals/outputs:** host↔container readiness handshake; **`mutation-attempted` published
    host-side to `$GITHUB_OUTPUT` before launching the mutating CLI**; named container + host-side
    signal forwarding (`docker stop -t <deadline within GitHub's cancellation grace>` → `docker rm`)
    - post-cancel reconciliation.

### 3.7 Source freezing, provenance, disclosure, actions

- **Source freezing:** on the **read-only original** checkout, assert the initial `HEAD` SHA
  unchanged + tree clean (tracked + untracked + recursive submodules) **before and after** all
  app-controlled commands (commands run in the writable copy); reject escaping symlinks. Consumers
  additionally **verify their mounted checkout's repository id, `HEAD`, and workspace against the
  artifact before and after commands**.
- **`ExpectedIdentity`:** `app-repo-id` (decimal string), `source-revision` (full SHA, explicit),
  `app-cli-package`, `app-cli-bin`, `workspace-id` — **caller-supplied and checkout-verified**;
  `platform-id`/`container-ref` are **derived inside every action from same-SHA `image.json`, not
  accepted from the caller**.
- **Schema/canonicalization (normative, with golden vectors):** `app-cli-meta.json` is
  **canonical JSON** — UTF-8, keys **lexicographically sorted at every level**, no duplicate keys
  (a duplicate-key-rejecting parser is required; JSON Schema cannot do this), minimal number/string
  forms — validated by a committed **JSON Schema 2020-12** file **plus** the procedural
  canonical/dup-key pass. Numeric caps: meta ≤ **64 KiB**. Fields = `ExpectedIdentity` +
  `app-cli-version` (informational) + `binary-sha256` + `binary-size` + `abi`
  (`{ machine, interp, needed: [sorted str] }`).
- **Archive contract (normative):** a **`ustar`/`pax` tar** with **exactly two** regular members,
  `app-cli-meta.json` then the `app-cli-bin` binary — **any extra/duplicate/renamed member,
  symlink, hardlink, device, or path-traversal header is rejected**; total logical size ≤ **512
  MiB**, binary ≤ `binary-size`, meta ≤ 64 KiB; the extracted binary's sha256/size re-verified.
- **`validate-app-cli-provenance`** (fresh pinned container, minimal env): enforce the archive
  contract; canonical-JSON + JSON-Schema validate; re-verify binary digest/size; **ABI loadability
  proof** — recompute `PT_INTERP`, `DT_NEEDED`, and search paths from the binary, **resolve every
  required library inside the immutable image**, then run a **credential-free, network-disabled
  `--help` smoke**; compare every caller `ExpectedIdentity` field. Output `app-cli-path`.
- **`active-version-fastly`** — inputs: `artifact-tar`, `ExpectedIdentity`, `fastly-service-id`,
  `fastly-api-token`; validates, runs `active-version` via the launcher; output `version` (empty on
  a first-ever **production** deploy = success). **Recovery is PRODUCTION-only.**
- **`compute-app-cli-identity`** — inputs: `app-repository`/`app-repo-id`, `source-revision`,
  `workspace-root`, `app-cli-package`/`app-cli-bin`; reads `platform-id`/`container-ref` from
  same-SHA `image.json`; outputs the full `ExpectedIdentity`.
- **Disclosure (enforceable):** because the action cannot compare reader sets, require
  **`disclosure-acknowledged: true` for every cross-repository build** (`app-repo-id` ≠ the deployer
  repo id), **exempting only equal repository ids**. The sccache cache holds **compiled objects**
  (not dependency source), so the exposure it acknowledges is compiled artifacts; `deploy-fastly.cache`
  carries the same acknowledgement.

### 3.8 Reusable-workflow contract

Inputs: `app-repository`, `app-ref`, **`app-repo-id`** (string, always required), `working-directory`
(`.`), `workspace-root` (required), `app-cli-package` (required), `app-cli-bin`, `app-cli-artifact`
(**unique per matrix leg**), `cache` (default `false`), `cache-key-suffix`, `disclosure-acknowledged`
(required-true for cross-repo), `timeout-minutes` (30). **No `rust-toolchain`/feature inputs.** Secret
`app-checkout-token`. Job `permissions: { contents: read }` (caller grants ≥ that);
`persist-credentials: false`. **Runner floor 2.336.0** (self-repo `$/`).

**Matrix:** v1's shared workflow outputs are **single-build** (GitHub returns only the last matrix
leg's outputs). A **matrix caller uses unique per-leg `app-cli-artifact` names and computes each
leg's `ExpectedIdentity` via `compute-app-cli-identity`** — it does not consume the shared outputs.

## 4. Testing

sccache (fresh `CARGO_TARGET_DIR` each run; `RUSTC_WRAPPER=sccache` action-owned; cold-to-warm shows
a sccache hit-rate rise and reduced compile with **network disabled** on the warm run; a corrupt
restored `SCCACHE_DIR` triggers one cold rebuild; the audited cache path is exactly `SCCACHE_DIR`;
**a git dependency (the EdgeZero repo) builds and caches**). Container/runner/launcher (self-hosted
fails closed; read-only rootfs; manifest command creating `dist/` succeeds in the writable copy while
the original stays clean; enumerated mounts only; host-side `mutation-attempted` before mutation;
cancellation `docker stop -t`+reconcile). Env/config (constructed minimal env — a caller
`RUSTC_WRAPPER` is absent, not merely rejected; non-allowlisted config anywhere fails). Identity
(`app-repo-id` API-verified against `app-repository`; `platform-id` from `image.json`, not caller;
consumer re-verifies checkout id/HEAD/workspace before+after). Provenance (canonical-JSON + dup-key;
archive exactly-two-members/format/size; **ABI loadability** — resolve `DT_NEEDED` in the image + a
network-disabled `--help`; a real wrong-runtime rejected; provenance documented consistency-only).
Disclosure required for every cross-repo build (equal-id exempt). Recovery production-only.

## 5. Rollout, docs, migration

**Atomic same-SHA rollout** (container image w/ sccache, reusable workflow, all three actions,
consumers, recovery); direct-composite producer retired → adopters migrate to the **two-job**
topology; runner floor **2.336.0**. Scope the parent's exact-key/target-only caching language to
`deploy-fastly.cache`; document that `build-app-cli.cache` is an **sccache disk cache** (compiled
objects, no source); apply the cross-repo disclosure rule to both caches; add the container-runner,
sccache, provenance, single-producer, and 2.336.0 updates; correct the "consumers own
checkout/runner/timeout; actions never call `checkout`" claims. Pin gate/`zizmor`/actionlint:
container digest pin, `$/` carve-outs. Public-surface golden: the `ExpectedIdentity` table, the
committed JSON Schema + **golden meta/archive vectors**, all three actions.

## 6. Default and effect

**Off by default** (caching). Container execution + provenance unconditional. With `cache: true` on
an authorized deployer build, sccache reuses compiled dependency objects across runs (the bulk of
the ~10 min); changed local crates recompile.

## 7. Out of scope / future

Caching checksum-verified `.crate` archives (download savings); workflow-bound artifact
**attestation**; native-tool (`cc`) sccache wrapping; trusted **self-hosted** runner mode;
cross-image/directional ABI; alternate toolchains (a second container); non-default features;
`cli-profile`; non-Fastly adapters.

## 8. History

… v6.11 (container-only) → v6.12 (own restore+save, full-runtime container) → v6.13 (crates.io-only,
hosted-only, four-root prune) → **v6.14 (sccache pivot)**: replace the unbuildable `target/` unit-graph
pruner and the unusable crates.io-only rule with a **fresh `CARGO_TARGET_DIR` + an action-owned pinned
`sccache` disk cache** (content-addressed, no pruning, any source incl. git deps, no source cached);
coarse rolling `run_id` generation key; **constructed minimal build env**; **writable working copy**
for manifest commands (read-only original for the freeze checks); `app-repo-id` **API-verified**,
`platform-id` **from `image.json` not the caller**, consumer **re-verifies checkout before+after**;
**disclosure required for every cross-repo build** (equal-id exempt); **ABI loadability** via resolved
`DT_NEEDED` + a network-disabled `--help`; normative **canonical-JSON + tar** contracts with golden
vectors; **matrix caller computes per-leg identity**; container plan gains a **verify-by-digest-then-PR**
publish (§ container sub-plan).

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile (checksum-verified Fastly CLI +
pinned sccache) + digest-pin + **verify-by-digest-then-PR** GHCR publish; the committed JSON Schema +
golden vectors; and the writer-fidelity / API-repo-id-binding / canonical-JSON predicate expressions.
