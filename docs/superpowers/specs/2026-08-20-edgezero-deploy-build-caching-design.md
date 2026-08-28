# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.15 (sccache pivot, hardened)

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
  **Correctness caveat (opt-in risk):** sccache's own Rust guidance warns it may **not** cache
  correctly when a **`build.rs` or a proc-macro reads files or environment not declared as inputs**
  (undeclared inputs). v1 does not detect this; enabling `cache: true` is an **explicit acceptance**
  that the app's build scripts/proc-macros declare their inputs (documented on the input). No custom
  pruning; `SCCACHE_CACHE_SIZE` bounds each snapshot (§3.2).
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

`actions/cache/restore` + `save` over **`SCCACHE_DIR` only**:

- **Key** = `<family>-<generation>`, `<family>` = `edgezero-sccache-v1-<platform-id>-<suffix-hash>`,
  restore-keys prefix `<family>-`. `<generation>` = `<github.run_id>-<github.run_attempt>-<app-cli-artifact>`
  — `run_attempt` distinguishes **re-runs** (which keep the same `run_id`) and `app-cli-artifact`
  (unique per matrix leg, §3.8) distinguishes **matrix legs**, so no two saving jobs collide on a
  key, and each restores the newest entry in its `<family>`. `platform-id` = the container digest;
  `suffix-hash` = the validated `cache-key-suffix` (§3.8). No lockfile/manifest hashing — sccache
  content-addresses internally.
- **Bounded storage.** `SCCACHE_CACHE_SIZE` is a fixed **2 GiB** (action-owned), so each saved
  snapshot is bounded well under GitHub's **10 GiB per-repository** cache limit; aggregate storage
  is bounded by GitHub's own LRU eviction over the family's generations (older generations are
  evicted; a busy repo may re-warm occasionally — an accepted cost of the rolling scheme).
- **Restore → audit → build → stop-server → best-effort save.** After restore, **audit** that the
  restored path is exactly `SCCACHE_DIR` and contains only sccache's blob/index layout (**discard
  and build cold once** on a corrupt/unexpected restore). Run `sccache --show-stats` for
  observability. Before save, **`sccache --stop-server`** flushes and shuts the server down so
  `SCCACHE_DIR` is consistent on disk. `actions/cache/save` under the run's `<generation>` key is
  **best-effort** (failures are warnings). Bump the `-v1-` family namespace whenever the mechanism
  changes.

### 3.3 Action-owned Cargo/sccache environment

The build runs under a **constructed minimal environment** (`env -i` + an explicit allowlist),
not scrub-then-reject, so there is nothing to miss: only the action-owned variables and an
allowlist of benign ones exist. Action-owned (fixed, exact values — the rustup-image layout means
`PATH` and `RUSTUP_HOME` are **required** for rustc to start): `PATH=/usr/local/cargo/bin:/usr/bin:/bin`,
`RUSTUP_HOME=/usr/local/rustup`, `CARGO_HOME` (§below), `RUSTC_WRAPPER=/usr/local/bin/sccache`
(absolute), `RUSTUP_TOOLCHAIN`, `CARGO_TARGET_DIR` (fresh), `SCCACHE_DIR`, `SCCACHE_CACHE_SIZE=2G`,
`HOME`, `TMPDIR`, `CARGO_ENCODED_RUSTFLAGS=""`, `CARGO_INCREMENTAL=0`. A **caller-supplied**
`RUSTC`/`RUSTC_WRAPPER`/`RUSTC_WORKSPACE_WRAPPER`/`RUSTDOC`/`RUSTFLAGS`/native-tool/`PATH` var simply
is **not present** in the constructed env (never inherited).

**Cache-hit stability requires ALL sccache hash inputs to be fixed across runs** (v0.10 hashes the
**cwd** too, so a varying path turns every warm build cold). The container therefore fixes, at
**constant in-container paths regardless of the host checkout location**: the writable working copy
at **`/work/app`** (the compile **cwd**, §3.6), `CARGO_TARGET_DIR=/work/target`,
`CARGO_HOME=/work/cargo-home`, `SCCACHE_DIR=/work/sccache`, `HOME=/work/home`, `TMPDIR=/work/tmp`
(writable tmpfs). Identical source built from different host paths must produce sccache hits (§4).

The effective **Cargo config** over the full chain (cwd → `/`, incl. the working directory, plus
`CARGO_HOME`) must contain only benign allowlisted keys (registry index URLs, `net.retry`,
`http.timeout`/`check-revoke`); anything else fails closed. Default-features-only; `Cargo.lock` must
be a tracked, regular file. External path deps outside the workspace root are rejected.

### 3.4 Identity

`git-root` (path, confinement); `app-repo` (`owner/repo`); **`app-repo-id`** (canonical decimal
**string**, always required, **verified via the GitHub REST API to belong to `app-repository`**).
`app-ref` must be a **full 40-hex commit SHA** (short refs/branches/tags rejected). `workspace-root`
canonicalized, confined beneath `git-root`, `working-directory` beneath it, asserted
`== cargo metadata.workspace_root`.

**All identity hashes are SHA-256 over a canonical, length-framed encoding** — each field encoded as
its UTF-8 bytes prefixed by its byte length as a fixed-width decimal (so no field boundary is
ambiguous), fields concatenated in a fixed order. `workspace-id` = that hash over
(`app-repo-id`, workspace-root path relative to `git-root`); `suffix-hash` = that hash over the
validated `cache-key-suffix`. **Golden vectors** for each hash are committed with the plan.
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
- **Separate container instances.** The credential-free **build** and the token-bearing **deploy**
  run in **distinct container instances** (never one long-lived container); the build instance holds
  no provider token.
- **One launcher `run-app-cli-in-container`** with a **complete fixed mount table** (constant
  in-container paths, so sccache's cwd/path hashing is stable regardless of the host checkout
  location; never `RUNNER_TEMP` wholesale):

  | In-container path | Mode | Source |
  | --- | --- | --- |
  | `/work/app` (compile cwd) | **writable** | a **verified faithful copy** of the app checkout |
  | `/work/target` | writable | fresh `CARGO_TARGET_DIR` |
  | `/work/cargo-home` | writable | `CARGO_HOME` |
  | `/work/sccache` | writable | `SCCACHE_DIR` (restored) |
  | `/work/home`, `/work/tmp` | writable (tmpfs) | provider/Fastly `HOME`, `TMPDIR` |
  | the package/output dir | writable | staged CLI / Fastly `pkg/` |
  | the validated CLI binary | read-only | consumer input |
  | the specific inline-config temp file | read-only | config-push only, by exact path |

  UID/GID mapping so the non-root container user owns the writable mounts.
  - **Writable working COPY.** The CLI runs arbitrary manifest commands via `sh -c` in the manifest
    root and may create `dist/`, `node_modules/`, generated manifests — so `/work/app` is a
    disposable writable copy. The copy is a **verified faithful copy of the read-only original** —
    equivalent in content, file modes, symlink targets, and submodule state, with **hardlinks broken**
    (a real copy, e.g. `cp -a` + a content-hash comparison, not a bind of the original) — so the bytes
    compiled are exactly the frozen source (§3.7).
  - **env:** only the required provider token + `EDGEZERO_*`; no GitHub file-command channels inside
    the container.
  - **signals/outputs:** host↔container readiness handshake; **`mutation-attempted` published
    host-side to `$GITHUB_OUTPUT` before launching the mutating CLI**; named container + host-side
    signal forwarding (`docker stop -t <deadline within GitHub's cancellation grace>` → `docker rm`) +
    post-cancel reconciliation.

### 3.7 Source freezing, provenance, disclosure, actions

- **Source freezing:** the writable `/work/app` copy is proven a **faithful copy** of the read-only
  original (§3.6) before compilation, so the frozen source and the executed bytes are the same. On
  the **read-only original**, assert the initial `HEAD` SHA unchanged + tree clean (tracked +
  untracked + recursive submodules) **before and after** all app-controlled commands; reject escaping
  symlinks. Consumers additionally **verify their mounted checkout's repository id, `HEAD`, and
  workspace against the artifact before and after commands**.
- **`ExpectedIdentity`:** `app-repo-id` (decimal string), `source-revision` (full SHA, explicit),
  `app-cli-package`, `app-cli-bin`, `workspace-id` — **caller-supplied and checkout-verified**;
  `platform-id`/`container-ref` are **derived inside every action from same-SHA `image.json`, not
  accepted from the caller**.
- **Schema/canonicalization (normative, with golden vectors):** `app-cli-meta.json` is **canonical
  JSON per RFC 8785 (JCS)** — the exact escaping, number serialization, key ordering, and whitespace
  rules are JCS's, not "minimal forms" — and duplicate keys are **rejected before parse** (JSON
  Schema cannot). It is validated by a committed **JSON Schema 2020-12** file **plus** the JCS +
  dup-key procedural pass. Meta ≤ **64 KiB**. Fields = `ExpectedIdentity` + `app-cli-version`
  (informational) + `binary-sha256` + `binary-size` + `abi` (`{ machine, interp, needed: [sorted str] }`).
- **Archive contract (normative):** a **deterministic `ustar` tar** (POSIX ustar **only** — `pax`
  extended headers are **rejected**, so there is no ambiguous PAX extension surface) with **exactly
  two** regular members, `app-cli-meta.json` then the `app-cli-bin` binary — any extra/duplicate/
  renamed member, any symlink/hardlink/device/global-extended header, trailing bytes, or
  path-traversal name is **rejected**; total logical size ≤ **512 MiB**, meta ≤ 64 KiB, and the
  binary member size **equals** `binary-size` exactly, with its sha256 re-verified.
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

sccache — **cross-run warm reuse is asserted via `sccache --show-stats`, not by disabling the
network** (only `SCCACHE_DIR` is cached, so Cargo still needs to fetch dependency **sources** before
invoking rustc): the warm run does `cargo fetch` **online**, then asserts the compile's sccache cache
**hit rate rose** and wall-time dropped versus cold. (If an offline compile is wanted, `cargo fetch`
**prefetches sources before** the network is disabled for the rustc phase only.) Also: a corrupt
restored `SCCACHE_DIR` triggers one cold rebuild; the audited cache path is exactly `SCCACHE_DIR`;
**identical source built from two different host checkout paths yields sccache hits** (fixed
`/work/app` cwd); **a public git dependency (the EdgeZero repo) builds and caches**; `sccache
--stop-server` runs before save. Container/runner/launcher (self-hosted fails closed; read-only
rootfs; separate build/deploy container instances; the faithful `/work/app` copy matches the original
in content/modes/symlinks/submodules with hardlinks broken; a manifest command creating `dist/`
succeeds in the copy while the original stays clean; enumerated fixed mount table only; host-side
`mutation-attempted` before mutation; cancellation `docker stop -t`+reconcile). Env/config
(constructed minimal env includes `PATH`/`RUSTUP_HOME` and an absolute `RUSTC_WRAPPER`; a caller
`RUSTC_WRAPPER`/`PATH` is absent, not merely rejected; non-allowlisted config anywhere fails).
Identity (`app-repo-id` API-verified; `app-ref` rejected unless a full 40-hex SHA; hash golden
vectors; `platform-id` from `image.json`, not caller; consumer re-verifies checkout id/HEAD/workspace
before+after). Provenance (JCS canonical + dup-key rejection; ustar-only exactly-two-members, `pax`
rejected, binary size equality; **ABI loadability** — resolve `DT_NEEDED` in the image + a hardened
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
fix + publish-visibility ordering land in the container sub-plan.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile (checksum-verified Fastly CLI +
pinned sccache) + digest-pin + **verify-by-digest-then-PR** GHCR publish; the committed JSON Schema +
golden vectors; and the writer-fidelity / API-repo-id-binding / canonical-JSON predicate expressions.
