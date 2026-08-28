# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.16 (sccache pivot, hardened)

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
- **`RUSTC_WRAPPER` is set (action-owned) to the pinned `sccache`** (an **absolute path**,
  `/usr/local/bin/sccache`, §3.3) baked into the container. sccache keys a rustc invocation on its
  **preprocessed source, `dep-info` inputs, compiler arguments, dependency artifacts, a subset of
  the environment, and the working directory** (v0.10) — so a cached object is reused only when all
  of those match, and **restoring an older `SCCACHE_DIR` never yields an incorrect object**.
  **Correctness caveat (accepted risk, not a condition apps satisfy):** sccache's own Rust guidance
  warns it may **not** cache correctly when a **`build.rs` or a proc-macro reads files or environment
  not declared as inputs** (undeclared inputs). Rust has **no general mechanism for a proc-macro to
  declare its filesystem inputs**, so this cannot be posed as a precondition an application meets — it
  is simply the risk `cache: true` **accepts**. v1 does not detect it; enabling `cache: true` is an
  **explicit acceptance** of possible staleness for build scripts / proc-macros with undeclared
  inputs (documented on the input), with the fallback that a wrong object still fails the downstream
  provenance/ABI checks. No custom pruning; `SCCACHE_CACHE_SIZE` bounds each snapshot (§3.2).
- **Cache contents = `SCCACHE_DIR` only** (compiled objects + sccache's index). **No `.crate`
  sources, no `registry/src`, no `git/*`, no `CARGO_HOME/bin`, no config, no credentials** are
  cached — so a cold build's `registry/src` extraction is irrelevant to the audit, and **no
  dependency source is ever cached** (only compiled objects). Re-downloading crates each run is the
  small remaining cost; caching `.crate` archives is §7.
- **Public, anonymously-fetchable sources only.** sccache caches the compilation of any source, but
  the minimal build environment (§3.3) carries **no credentials**, so the dependency graph must be
  **anonymously fetchable** — `crates.io` and **public git** (e.g. the public EdgeZero repo the
  generator emits). Private git/registries, SSH auth, `.netrc`, and credential providers are **not
  supported** (a credential design is §7); `cache: false` and `cache: true` resolve dependencies
  identically (caching never changes resolution).

### 3.2 Own restore + save, coarse rolling key

`actions/cache/restore` + `save` over **one stable host path** (below):

- **Stable host cache path (required).** `actions/cache` folds the **on-disk path** it archives into
  the cache **version**, so a per-run `mktemp` path would make *every* restore miss regardless of a
  matching key. The action therefore uses **one fixed host path — `${RUNNER_TEMP}/edgezero-sccache-v1`**
  (constant across runs of a given runner-arch), **emptied before restore**, and bind-mounted at the
  constant in-container `SCCACHE_DIR=/work/sccache` (§3.6). Only `SCCACHE_DIR` is archived.
- **Key** = `<family>-<generation>`, `<family>` = `edgezero-sccache-v1-<platform-id>-<suffix-hash>`,
  restore-keys prefix `<family>-`. `<generation>` = `<github.run_id>-<github.run_attempt>-<invocation-id>`,
  where **`invocation-id` is unique across every cache-writing invocation** — not merely per matrix
  leg but per reusable-workflow call in a run (two calls in one run share `run_id`/`run_attempt` and
  can share a default `app-cli-artifact`, so the artifact **name alone is insufficient**). It is the
  **`suffix-hash`-bound `app-cli-artifact`** (required unique per writer, §3.8) **hashed into the key**;
  `run_attempt` additionally distinguishes **re-runs** (same `run_id`). Each writer thus saves a
  **distinct immutable entry** and restores the **newest** in its `<family>`. `platform-id` = the
  container digest; `suffix-hash` = the validated `cache-key-suffix` (§3.8). No lockfile/manifest
  hashing — sccache content-addresses internally.
- **Concurrent lineages (accepted).** Concurrent matrix/sibling writers each restore the same newest
  snapshot and **fork** it; entries are immutable and **not merged**, so only one lineage's warmth is
  carried forward per family and the others' incremental warmth is **lost** (re-warmed next run). v1
  **accepts** this rather than partitioning per-leg families (which would multiply cold starts);
  partitioned lineages are §7.
- **Bounded snapshot, repository-global eviction (accepted).** `SCCACHE_CACHE_SIZE` is a fixed
  **2 GiB** (action-owned), bounding **each snapshot** well under GitHub's **10 GiB per-repository**
  cache limit. **Aggregate storage is not family-local:** every successful run saves a **new immutable
  entry**, and GitHub's eviction is **repository-wide LRU** — it can evict **unrelated** caches (other
  workflows' entries) once the repo total is exceeded, and raising the repo cache quota may be
  **billable**. v1 **explicitly accepts** repository-global LRU/thrashing under the rolling scheme (no
  action-side cleanup; the actor lacks a cross-workflow cache-delete permission by default). Bump the
  `-v1-` family namespace when the mechanism changes.
- **Restore → audit → build → stop-server → best-effort save, with fail-cold contracts.** After
  restore, **audit** that the restored path is exactly `SCCACHE_DIR` and contains only sccache's
  blob/index layout. **Any restore, audit, or sccache-read failure resets to a cold build** (discard
  the restored dir, build once from empty) rather than aborting. Run `sccache --show-stats` for
  observability. Before save, **`sccache --stop-server`** flushes and shuts the server down so
  `SCCACHE_DIR` is consistent on disk; **if `--stop-server` fails, the save is SKIPPED** (never
  archive a live/again-mutating cache). `actions/cache/save` under the run's `<generation>` key is
  otherwise **best-effort** (failures are warnings).

### 3.3 Action-owned Cargo/sccache environment

Every action runs under a **constructed minimal environment** (`env -i` + an explicit allowlist),
not scrub-then-reject, so there is nothing to miss: only the action-owned variables and an
enumerated allowlist exist. **`PATH` = `/usr/local/bin:/usr/local/cargo/bin:/usr/bin:/bin`** — it
**must include `/usr/local/bin`**, where the container installs the **Fastly CLI** and **`sccache`**
(the deploy/validation profiles otherwise cannot find `fastly`). The rustup-image layout means
`PATH` and `RUSTUP_HOME` are **required** for rustc to start.

**Enumerated env profiles** (each an exact, closed set — no inherited namespace):

- **compile/build:** `PATH` (above), `RUSTUP_HOME=/usr/local/rustup`, `CARGO_HOME` (§below),
  `RUSTC_WRAPPER=/usr/local/bin/sccache` (absolute), `RUSTUP_TOOLCHAIN`, `CARGO_TARGET_DIR` (fresh),
  `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE=2G`, `HOME`, `TMPDIR`, `CARGO_ENCODED_RUSTFLAGS=""`,
  `CARGO_INCREMENTAL=0`. **No** `sccache`/wrapper vars in the deploy/validation profiles.
- **validation (`validate-app-cli-provenance`):** `PATH`, `HOME`, `TMPDIR` only (no cargo/sccache, no
  token) — it recomputes ELF metadata and runs the hardened smoke (§3.7).
- **deploy (`active-version-fastly`, config-push):** `PATH`, `HOME`, `TMPDIR`, the **single** provider
  token (`FASTLY_API_TOKEN`), and an **enumerated** `EDGEZERO_*` allowlist — the specific public
  variables the deploy CLI reads are **listed by name** (not the whole `EDGEZERO_*` namespace); an
  unlisted `EDGEZERO_*` is not present.

A **caller-supplied** `RUSTC`/`RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER`/`RUSTDOC`/`RUSTFLAGS`/
native-tool/`PATH` var simply is **not present** in any constructed profile (never inherited).

**Cache-hit stability requires ALL sccache hash inputs to be fixed across runs** (v0.10 hashes the
**cwd** too, so a varying path turns every warm build cold). The container therefore fixes, at
**constant in-container paths regardless of the host checkout location**: the writable working copy
of the **whole repository** at **`/work/repo`** (preserving its layout, §3.6), the compile **cwd** at
**`/work/repo/<working-directory-relative-to-git-root>`** (a **constant** path for a given app, so
enclosing Cargo config, parent workspaces, and sibling path-dependencies are all preserved — a
flattened single-directory mount would break `working-directory: apps/api`), `CARGO_TARGET_DIR=/work/target`,
`CARGO_HOME=/work/cargo-home`, `SCCACHE_DIR=/work/sccache`, `HOME=/work/home`, `TMPDIR=/work/tmp`
(writable tmpfs). Identical source built from different host paths must produce sccache hits (§4).

The effective **Cargo config** over the full chain (cwd → `/`, incl. the working directory, plus
`CARGO_HOME`) must contain only benign allowlisted keys (registry index URLs, `net.retry`,
`http.timeout`/`check-revoke`); anything else fails closed. Default-features-only; `Cargo.lock` must
be a tracked, regular file. External path deps outside the workspace root are rejected.

### 3.4 Identity

`git-root` (path, confinement); `app-repo` (`owner/repo`); **`app-repo-id`** (canonical decimal
**string**, always required, **verified via the GitHub REST API to belong to `app-repository`**).
**Credential for the repo-id lookup:** the API verification uses the **`app-checkout-token`** secret
(§3.8) — the only credential able to read a **private** app repo's metadata — and runs **host-side
only** in `compute-app-cli-identity`. It is **never forwarded into any container, working copy,
artifact, or cache**: the build/validate containers carry no GitHub token (§3.3 profiles), so the
token cannot leak into compiled output or the sccache archive. `app-ref` must be a **full 40-hex
commit SHA** (short refs/branches/tags rejected). `workspace-root` canonicalized, confined beneath
`git-root`, `working-directory` beneath it, asserted `== cargo metadata.workspace_root`.

**All identity hashes are SHA-256 over a canonical, length-framed encoding.** Each field is encoded
as its UTF-8 bytes prefixed by a length frame: the byte length as **ASCII decimal with no leading
zeros** followed by a single `:` separator (`<len>:<bytes>`), fields concatenated in a fixed order —
so no field boundary is ambiguous and no fixed width can overflow. **Path fields are normalized
first** — expressed **relative to `git-root`**, `/`-separated, no `.`/`..`/empty segments, no
trailing slash, NFC — so the same logical path hashes identically across runners. `workspace-id` =
that hash over (`app-repo-id`, normalized workspace-root path relative to `git-root`);
`suffix-hash` = that hash over the validated `cache-key-suffix`. **Golden vectors** (including the
exact `<len>:<bytes>` framing) for each hash are committed with the plan.
`platform-id` = the container digest, **read inside every action from `image.json` at the same
EdgeZero SHA — never caller-supplied**; `container-ref` = `<repo>@<platform-id>`.

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
- **Separate container instances, one shared working copy.** The credential-free **build** and the
  token-bearing **deploy** run in **distinct container instances** (never one long-lived container);
  the build instance holds no provider token. They **share a single `/work/repo` working copy**: it is
  made **once** as a faithful copy of the checkout, the build instance compiles into it (and into the
  fresh `/work/target`), and the **same copy — now carrying the build's derived outputs — is remounted
  into the deploy instance** (as derived build state, not re-copied), so generated files (`dist/`,
  staged `pkg/`, produced manifests) reach the deploy step without a lossy re-clone. Freeze
  assertions (§3.7) run against the **read-only original**, never this mutated copy.
- **One launcher `run-app-cli-in-container`** with a **complete fixed mount table** (constant
  in-container paths, so sccache's cwd/path hashing is stable regardless of the host checkout
  location; never `RUNNER_TEMP` wholesale):

  | In-container path | Mode | Source |
  | --- | --- | --- |
  | `/work/repo` (repo root; compile cwd = `/work/repo/<working-directory>`) | **writable** | a **verified faithful copy** of the whole app checkout, layout preserved |
  | `/work/target` | writable | fresh `CARGO_TARGET_DIR` |
  | `/work/cargo-home` | writable | `CARGO_HOME` |
  | `/work/sccache` | writable | `SCCACHE_DIR` (restored from the stable host path, §3.2) |
  | `/work/home`, `/work/tmp` | writable (tmpfs) | provider/Fastly `HOME`, `TMPDIR` |
  | the package/output dir | writable | staged CLI / Fastly `pkg/` |
  | the validated CLI binary | read-only | consumer input |
  | the specific inline-config temp file | read-only | config-push only, by exact path |

  UID/GID mapping so the non-root container user owns the writable mounts.
  - **Writable working COPY (whole repo, layout preserved).** The CLI runs arbitrary manifest commands
    via `sh -c` in the manifest root and may create `dist/`, `node_modules/`, generated manifests — so
    `/work/repo` is a disposable writable copy of the **entire repository** (not the flattened working
    directory), preserving parent Cargo config, enclosing workspaces, and sibling path-dependencies.
    The copy is a **verified faithful copy of the read-only original** — equivalent in content, file
    modes, symlink targets, and **initialized-submodule** state (submodules must be checked out at
    their recorded commits; an uninitialized/dirty submodule fails closed), with **hardlinks broken**
    (a real copy, e.g. `cp -a` + a content-hash comparison, not a bind of the original). **Ignored
    files are excluded:** the copy carries only what `source-revision` represents — tracked files plus
    initialized submodules; git-ignored/untracked build detritus is **absent** (excluded before the
    copy), so the compiled bytes are exactly the frozen source (§3.7).
  - **env:** only the required provider token + `EDGEZERO_*`; no GitHub file-command channels inside
    the container.
  - **signals/outputs:** host↔container readiness handshake; **`mutation-attempted` published
    host-side to `$GITHUB_OUTPUT` before launching the mutating CLI**; named container + host-side
    signal forwarding (`docker stop -t <deadline within GitHub's cancellation grace>` → `docker rm`) +
    post-cancel reconciliation.

### 3.7 Source freezing, provenance, disclosure, actions

- **Source freezing:** the writable `/work/repo` copy is proven a **faithful copy** of the read-only
  original (§3.6) before compilation — **tracked files + initialized submodules only, git-ignored/
  untracked detritus excluded**, so the copy is exactly what `source-revision` represents — and that
  **same copy (now with build outputs) is reused for the deploy instance** (§3.6), so the frozen
  source, the executed bytes, and the deployed artifacts are one lineage. On the **read-only
  original**, assert the initial `HEAD` SHA unchanged + tree clean (tracked + untracked + recursive
  submodules) **before and after** all app-controlled commands; reject escaping symlinks. Consumers
  additionally **verify their mounted checkout's repository id, `HEAD`, and workspace against the
  artifact before and after commands**.
- **`ExpectedIdentity`:** `app-repo-id` (decimal string), `source-revision` (full SHA, explicit),
  `app-cli-package`, `app-cli-bin`, `workspace-id` — **caller-supplied and checkout-verified**;
  `platform-id`/`container-ref` are **derived inside every action from same-SHA `image.json`, not
  accepted from the caller**.
- **Schema/canonicalization (normative, with golden vectors):** `app-cli-meta.json` is **canonical
  JSON per RFC 8785 (JCS)** — the exact escaping, number serialization, key ordering, and whitespace
  rules are JCS's, not "minimal forms" — and duplicate keys are **rejected before parse** (JSON
  Schema cannot). It is validated by a committed **JSON Schema 2020-12** file **plus** the JCS +
  dup-key procedural pass. Meta ≤ **64 KiB**. Fields = `ExpectedIdentity` + `app-cli-version`
  (informational) + `binary-sha256` + `binary-size` + `abi`. **`abi` is recomputed ELF metadata**,
  each field an exact form: `machine` = the ELF `e_machine` **as its canonical string name**
  (e.g. `"x86_64"`); `interp` = the `PT_INTERP` path **as a string, or JSON `null` for a static
  binary** (no `PT_INTERP`); `needed` = the **direct** `DT_NEEDED` entries **as a sorted string array**
  (`[]` for a static binary) — **transitive** libraries are not listed (they are resolved, not
  recorded, by the loadability proof). `dlopen`-at-runtime libraries are **out of scope** (not in
  `DT_NEEDED`, not asserted). `abi` is a **consistency/loadability** contract, not a full ABI model.
- **Archive contract (normative), with normalized headers:** a **deterministic `ustar` tar** (POSIX
  ustar **only** — `pax` extended headers are **rejected**, so there is no ambiguous PAX extension
  surface) with **exactly two** regular members in fixed order, `app-cli-meta.json` then the
  `app-cli-bin` binary. **Header fields are normalized to fixed values** so byte-equality is
  reproducible: `uid`/`gid` = `0`, `uname`/`gname` = empty, `mtime` = `0`, `mode` = `0644` (meta) /
  `0755` (binary), `typeflag` = `0` (regular), `prefix` = empty and each `name` a fixed literal
  (`app-cli-meta.json`, the `app-cli-bin` basename) — **not** the producer's path. Any extra/
  duplicate/renamed member, any symlink/hardlink/device/global-extended header, non-zero `mtime`/
  non-zero `uid`/`gid`, trailing bytes, or path-traversal name is **rejected**; total logical size ≤
  **512 MiB**, meta ≤ 64 KiB, and the binary member size **equals** `binary-size` exactly, with its
  sha256 re-verified.
- **`validate-app-cli-provenance`** (fresh pinned container, minimal env, hardened): enforce the
  archive contract; JCS + JSON-Schema validate; re-verify binary digest/size; **ABI loadability
  proof** — recompute `PT_INTERP`, `DT_NEEDED`, and search paths from the binary, **resolve every
  required library inside the immutable image**, then run a **credential-free `--help` smoke**. The
  smoke runs the archive-supplied binary under **`--network=none --read-only --user 1001
  --cap-drop=ALL --security-opt=no-new-privileges`, a bounded `--memory`/`--pids-limit`, and a wall
  timeout** (Docker enforces these directly). Compare every caller `ExpectedIdentity` field. Output
  `app-cli-path`.
- **`active-version-fastly`** — inputs: `artifact-tar`, `ExpectedIdentity`, `fastly-service-id`,
  `fastly-api-token`; validates, runs `active-version` via the launcher; output `version` (empty on
  a first-ever **production** deploy = success). **Recovery is PRODUCTION-only.**
- **`compute-app-cli-identity`** — inputs: `app-repository`/`app-repo-id`, `source-revision`,
  `workspace-root`, `app-cli-package`/`app-cli-bin`, and the **`app-checkout-token`** secret used
  **host-side** to API-verify `app-repo-id` belongs to `app-repository` (the only credential that can
  read a private repo's metadata); the token is **never** passed to a container, working copy,
  artifact, or cache. Reads `platform-id`/`container-ref` from same-SHA `image.json`; outputs the full
  `ExpectedIdentity`.
- **Disclosure (enforceable):** because the action cannot compare reader sets, require
  **`disclosure-acknowledged: true` for every cross-repository build** (`app-repo-id` ≠ the deployer
  repo id), **exempting only equal repository ids**. The sccache cache holds **compiled objects**
  (not dependency source), so the exposure it acknowledges is compiled artifacts; `deploy-fastly.cache`
  carries the same acknowledgement.

### 3.8 Reusable-workflow contract

Inputs: `app-repository`, `app-ref`, **`app-repo-id`** (string, always required), `working-directory`
(`.`), `workspace-root` (required), `app-cli-package` (required), `app-cli-bin`, `app-cli-artifact`
(**required unique across every cache-writing invocation** — not only per matrix leg but per
reusable-workflow call in a run; the action **fails closed** on a collision it can detect, since two
calls sharing `run_id`/`run_attempt` and a default artifact name would otherwise write the same key),
`cache` (default `false`), `cache-key-suffix`, `disclosure-acknowledged` (required-true for cross-repo),
`timeout-minutes` (30). **No `rust-toolchain`/feature inputs.** Secret `app-checkout-token`. Job
`permissions: { contents: read }` (caller grants ≥ that); `persist-credentials: false`. **Runner floor
2.336.0** (self-repo `$/`).

**Matrix:** v1's shared workflow outputs are **single-build** (GitHub returns only the last matrix
leg's outputs). A **matrix caller uses unique per-leg `app-cli-artifact` names** (which also key each
leg's distinct cache lineage, §3.2) **and computes each leg's `ExpectedIdentity` via
`compute-app-cli-identity`** — it does not consume the shared outputs. Concurrent legs each restore
the newest snapshot and fork it without merging (§3.2, accepted).

## 4. Testing

sccache — **cross-run warm reuse is asserted via `sccache --show-stats`, not by disabling the
network** (only `SCCACHE_DIR` is cached, so Cargo still needs to fetch dependency **sources** before
invoking rustc): the warm run does `cargo fetch` **online**, then asserts the compile's sccache cache
**hit rate rose** and wall-time dropped versus cold. (If an offline compile is wanted, `cargo fetch`
**prefetches sources before** the network is disabled for the rustc phase only.) Also: a **stable host cache path** (`${RUNNER_TEMP}/edgezero-sccache-v1`, emptied before restore) —
a matching key **restores across runs** (proving the path is not a per-run `mktemp` that would force
version misses); a corrupt/failed restore **resets cold** (one rebuild from empty); a failed
`sccache --stop-server` **skips the save** (no live-cache archive); the audited cache path is exactly
`SCCACHE_DIR`; **identical source built from two different host checkout paths yields sccache hits**
(fixed `/work/repo/<working-directory>` cwd); a **nested working directory** (`working-directory:
apps/api` under a parent workspace) builds with its enclosing Cargo config/sibling path-deps intact;
**a public git dependency (the EdgeZero repo) builds and caches**; two writers with distinct
`app-cli-artifact` names save **distinct entries** (no key collision). Container/runner/launcher
(self-hosted fails closed; read-only rootfs; **separate build/deploy container instances sharing one
`/work/repo` copy** so build outputs reach deploy; the faithful `/work/repo` copy matches the original
in content/modes/symlinks/**initialized-submodule** state with hardlinks broken and **git-ignored
files excluded**; a manifest command creating `dist/` succeeds in the copy while the original stays
clean; enumerated fixed mount table only; host-side `mutation-attempted` before mutation; cancellation
`docker stop -t`+reconcile). Env/config (constructed minimal env; **`PATH` includes `/usr/local/bin`**
so `fastly` resolves; `RUSTUP_HOME` set and an absolute `RUSTC_WRAPPER`; the deploy profile exposes
only the **enumerated** `EDGEZERO_*` allowlist + the single token; a caller `RUSTC_WRAPPER`/`PATH` is
absent, not merely rejected; non-allowlisted config anywhere fails). Identity (`app-repo-id`
API-verified **with `app-checkout-token` host-side, never forwarded into a container/copy/artifact/
cache**; `app-ref` rejected unless a full 40-hex SHA; **length-framed `<len>:<bytes>` hash golden
vectors** with normalized paths; `platform-id` from `image.json`, not caller; consumer re-verifies
checkout id/HEAD/workspace before+after). Provenance (JCS canonical + dup-key rejection; ustar-only
exactly-two-members with **normalized headers** — zero `mtime`/`uid`/`gid`, fixed names — `pax`
rejected, binary size equality; **ABI loadability** — `abi` = recomputed `machine`/`interp`(`null` if
static)/direct-`DT_NEEDED`, transitive resolved in the image, `dlopen` out of scope — + a hardened
`--help` smoke (`--network=none --cap-drop=ALL --no-new-privileges`, memory/pids/timeout); a real
wrong-runtime rejected; provenance documented consistency-only). Disclosure required for every
cross-repo build (equal-id exempt). Recovery production-only.

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
publish (§ container sub-plan). → **v6.15 (hardened)**: add `PATH`/`RUSTUP_HOME` + an absolute
`RUSTC_WRAPPER` so rustc starts under `env -i`; narrow the sccache correctness claim (dep-info/args/
env/**cwd** hashing) and make the **undeclared-input (proc-macro/build.rs) risk** an explicit
cache opt-in; a **bounded, collision-free generation** (`run_id`-`run_attempt`-`artifact`, `SCCACHE_CACHE_SIZE=2G`,
`--stop-server` before save); a **complete fixed mount table** with a constant `/work/app` cwd (so
sccache's cwd hash is stable across host paths) and a **verified faithful working copy** (content/
modes/symlinks/submodules, hardlinks broken); **separate build/deploy container instances**; a **full
40-hex `app-ref`** and **length-framed hash encodings** with golden vectors; **RFC 8785 (JCS)** JSON +
**ustar-only** archive with binary-size equality; a **hardened validator smoke** (`--cap-drop=ALL`,
`no-new-privileges`, memory/pids/timeout); and a **warm test via `sccache --show-stats`** (online, since
dependency sources are not cached). Public, anonymously-fetchable sources only. Validator string-type
fix + publish-visibility ordering land in the container sub-plan. → **v6.16 (contract revision)**: a
**stable host cache path** (`${RUNNER_TEMP}/edgezero-sccache-v1`, emptied before restore) so
`actions/cache`'s path-in-version rule cannot force permanent misses; **whole-repo `/work/repo`**
working copy with the compile cwd at the relative `working-directory` (preserving nested-workspace
parent config/sibling path-deps — the flattened `/work/app` is gone), **git-ignored files excluded**
and **initialized submodules validated**, and the **same copy reused across the separate build/deploy
container instances** so build outputs reach deploy; storage restated as **repository-global LRU**
(evicts unrelated caches, may be billable) — not family-local; **generation keyed on an
`app-cli-artifact` unique across every cache-writing invocation** (fail-closed on a detectable
collision) with concurrent lineages **forked, not merged** (accepted); the **sccache undeclared-input
risk stated as accepted** (no proc-macro input-declaration mechanism exists) with **fail-cold** restore/
audit/read failures and a **skip-save on `--stop-server` failure**; **`PATH` includes `/usr/local/bin`**
(Fastly/sccache) with **enumerated compile/validation/deploy env profiles** (named `EDGEZERO_*`, not the
namespace); **`app-checkout-token` assigned to the host-side `app-repo-id` API check** and barred from
containers/copies/artifacts/caches; **length-framed `<len>:<bytes>` hash encoding** with normalized
relative paths, **normalized ustar headers** (zero `mtime`/`uid`/`gid`, fixed names), and **`abi` as
recomputed ELF metadata** (`machine`/`interp`=`null`-if-static/direct-`DT_NEEDED`; transitive resolved,
`dlopen` out of scope). Container sub-plan: two-tier pin policy (major action tags, image digests) and a
**canonical-repository** check in `check-image-pin.sh`.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile (checksum-verified Fastly CLI +
pinned sccache) + digest-pin + **verify-by-digest-then-PR** GHCR publish; the committed JSON Schema +
golden vectors; and the writer-fidelity / API-repo-id-binding / canonical-JSON predicate expressions.
