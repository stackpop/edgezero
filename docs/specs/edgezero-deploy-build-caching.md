# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.13

**Related:** `docs/specs/edgezero-deploy-github-action.md`,
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI (native) with **no caching**, so every deploy
recompiles the whole dependency graph (~10 min for `stackpop/trusted-server-deployer`, which
checks out a **separate** application repo and builds its CLI). Caching must work for that
**cross-repository deployer** topology.

## 2. Trust model and v1 shape

- The build compiles **trusted code** (the deploy target); `build.rs` is trusted.
- Caching runs **only for authorized deployer events/refs (fail before compiling otherwise)**;
  the runtime credential and the narrow app-checkout PAT are explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**; every writer on the selected/default
  cache branch is trusted; the deployer's protected workflow allowlists `app-repository`/`app-ref`.
- **The reusable workflow is the only SUPPORTED producer.** Build + deploy run in one **pinned
  container** (§3.7). Provenance is a **consistency check, not producer authentication** — an
  app-controlled or other-job archive could self-assert the same metadata/`binary-sha256`, so a
  "reject a bare composite artifact" rule is a supported-path convention, **not** a security
  guarantee (workflow-bound artifact attestation is §7 future work). The direct composite is
  retired as a standalone/cross-job producer and exists only as an internal `$/` step.
- **v1 scope narrows for enforceability:** **`crates.io`-only** dependency graphs (§3.9),
  **GitHub-hosted `linux/amd64` runners only** (§3.7), and **default-features-only** builds (§3.8).

## 3. Design

### 3.1 Reusable-workflow contract

Inputs: `app-repository` (default caller), `app-ref` (full SHA if repo set), **`app-repo-id`**
(**string**, a canonical decimal id, **always required** — provenance is unconditional and a
checkout cannot derive it), `working-directory` (default `.`, beneath `workspace-root`),
**`workspace-root`** (required), `app-cli-package` (required), `app-cli-bin` (default: package),
`app-cli-artifact` (required unique per matrix leg; default `edgezero-cli`), `cache` (bool,
default `false`), `cache-key-suffix` (≤64 `[A-Za-z0-9._-]`, hashed), `disclosure-acknowledged`
(bool, default `false`), `timeout-minutes` (number, default 30). **No `rust-toolchain` / no
feature inputs** — the container bakes the toolchain and v1 is default-features-only. Secret
`app-checkout-token` (narrow `contents: read` PAT for a **non-public** cross-repo app).

Outputs = the `ExpectedIdentity` set (§3.9) + `app-cli-artifact` + `app-cli-version`
(informational). Job-level **`permissions: { contents: read }`** (caller must grant ≥ that);
`persist-credentials: false` normative. **Minimum GitHub Actions Runner 2.336.0** (self-repo `$/`
references, §3.2), superseding the parent's 2.327.1 floor.

### 3.2 Self-composite reference

The reusable workflow invokes its own composite via **`$/.github/actions/build-app-cli`** (needs
runner **2.336.0+**; a narrow pin-gate exemption + actionlint suppression).

### 3.3 Action-owned Cargo variables

The action **sets** `CARGO_HOME` and `CARGO_TARGET_DIR` to exact computed identity-scoped paths
under `RUNNER_TEMP`, and `CARGO_ENCODED_RUSTFLAGS=""`. These are **action-owned** and exempt from
the config/env closure (§3.8), which governs only **caller-inherited** `CARGO_*`/config.

### 3.4 Own BOTH restore and save; four-root prune

v1 owns both halves with `actions/cache/restore` + `save` (no rust-cache, which exposes only
`cache-hit`, not its key/paths):

- **Key** = `edgezero-build-app-cli-v2-<tuple-hash>` + `-<lock-hash>`. `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, **the content hash of EVERY workspace member `Cargo.toml`** (root + members, so
  a feature/manifest edit that leaves `Cargo.lock` unchanged still busts the key), `platform-id`
  (container digest), and the validated `cache-key-suffix`; `lock-hash` = the tracked `Cargo.lock`
  hash. Restore-key prefix (minus `lock-hash`) gives a warm fallback across lockfile edits. **The
  `-v2-` namespace is bumped whenever key/path/prune/Cargo-invocation/source-policy semantics
  change.**
- **Approved path roots (only):** `CARGO_TARGET_DIR`, `CARGO_HOME/registry/index`,
  `CARGO_HOME/registry/cache`, `CARGO_HOME/git/db`.
- **Prune ALL FOUR roots to the current approved graph before save** (a restore-key fallback can
  restore stale sources/objects that must not be re-saved):
  - `target/`: classify every package with `source == null` in `cargo metadata` as **local**;
    disable incremental; using the build's `--message-format=json` event stream to map units→
    output files, **remove every fingerprint/dep/build-script/native/incremental output belonging
    to a local unit or a unit reverse-dependent on one** (in-root path deps outside
    `workspace_members` are local too).
  - `registry/cache`, `registry/index`, `git/db`: reduce to **only** the sources named by the
    current `Cargo.lock` (drop archives/repos from an older lockfile), and re-assert the source
    policy (§3.9) on what remains.
- **Ordered save, no app command afterward:** build → package CLI → final lock/HEAD/tree/submodule
  checks (§3.8) → four-root prune + audit (fail closed on any extra/credential/config/`bin`/
  `src`/`checkouts` path) → `actions/cache/save`. Save is **best-effort** (its failures are
  warnings, never failing the build); a failed prune/audit **skips** the save.
- **Metadata order:** pre-restore `cargo metadata --no-deps --locked` (no fetch); restore into
  reset dirs; post-restore `cargo metadata --locked` with the **default feature set** (matching
  the build), `Cargo.lock` verified byte-identical. Fail closed on any.

### 3.5 Identity

`git-root` (path, confinement); `app-repo` (`owner/repo`); `app-repo-id` (canonical **decimal
string**, always required, producer-verified, emitted). `workspace-root` canonicalized, confined
beneath `git-root`, `working-directory` beneath it, asserted `== cargo metadata.workspace_root`.
`workspace-id` = hash(`app-repo-id`, workspace-root rel `git-root`). `platform-id` = the container
digest.

### 3.6 Writer fidelity vs. source authorization

Cache runs only on `push`/`workflow_dispatch`/`schedule` on a **protected deployer ref** with
`HEAD == resolved app SHA` (action fidelity); the deployer's protected workflow allowlists the app
identity and every cache-branch writer is trusted (deployer authorization). Normative in the guide.

### 3.7 Container, runner, and the Docker launcher

- **Image:** EdgeZero-published, **public** (anonymous pull) + retained, **single-manifest
  `linux/amd64`**, pinned by **manifest digest**, from a versioned in-repo Dockerfile baking the
  pinned Rust toolchain, `wasm32-wasip1`, the pinned **Fastly CLI** (`versions.json`), and
  `git jq tar curl cc`. `platform-id` = its digest. Run **`--read-only`, non-root**, explicit
  writable mounts only.
- **Runner: GitHub-hosted `linux/amd64` only** (fail closed on self-hosted — GitHub exposes no
  reliable ephemeral predicate; a trusted self-hosted mode is §7). Host-level job (not already in
  a job container), **local Docker daemon**.
- **One launcher `run-app-cli-in-container`** with **enumerated mounts** (never `RUNNER_TEMP`
  wholesale — that exposes GitHub command files):
  - **read-only:** the app checkout (source), the validated CLI binary (artifact), and — for
    config-push — the **specific inline-config temp file** (a single path, not its parent).
  - **writable (nested, specific):** `CARGO_TARGET_DIR`, `CARGO_HOME`, a **Fastly/provider HOME**,
    and a **package/output** dir (Fastly builds write `pkg/`; deploy writes Cargo targets +
    manifest-command outputs). UID/GID mapping so the non-root container user owns the mounts.
  - **env:** only the required provider token + `EDGEZERO_*`; **no GitHub file-command channels**
    inside the container.
  - **signals/outputs:** host↔container readiness handshake; the **`mutation-attempted` signal is
    published host-side to `$GITHUB_OUTPUT` before launching the mutating CLI**; named container +
    host-side signal forwarding (SIGTERM/SIGINT → `docker stop -t <deadline>` fitting GitHub's
    cancellation grace window → `docker rm`) + post-cancel reconciliation.

### 3.8 Cargo config/source closure, features, env scrub

- **Config/env closure (caller-inherited only; action-owned vars §3.3 exempt):** the effective
  Cargo config over the full chain (cwd → `/`, **including the working directory**, plus
  `CARGO_HOME`) **and** caller `CARGO_*` env — **fail closed unless every present key is on a
  minimal allowlist** (registry **index** URLs, `net.retry`, `http.timeout`, `http.check-revoke`).
  Anything else (wrappers, `RUSTC*`, `RUSTDOC`, rustflags, `[env]`, `[profile.*]`, `[target.*]`
  runner/linker, `paths`, source replacement, `include`, `build.target`/`build.build-dir`,
  `net.git-fetch-with-cli`, `net.offline`, credential providers) fails closed.
- **Env scrub to baseline:** `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` (then set by §3.3),
  `CARGO_BUILD_RUSTFLAGS`, target-qualified rustflags, `AR`, `LD`, `LDFLAGS`, `CC`, `CFLAGS`,
  `CXX`, `CXXFLAGS`, `CPPFLAGS`, `CMAKE_*`, `PKG_CONFIG_*`, `BINDGEN_EXTRA_CLANG_ARGS`, `CPATH`,
  `LIBRARY_PATH`. Reject raised `target-cpu`/`target-feature`.
- **Default-features-only** for v1 (no feature input); metadata and build use the **same default
  feature set**. **`Cargo.lock` must be a tracked, regular file.** External path deps outside the
  workspace root are rejected.
- **Source freezing:** initial `HEAD` SHA unchanged + tree clean (tracked + untracked + recursive
  submodules) **before and after** all app-controlled commands; reject escaping symlinks.

### 3.9 Identity contract, provenance, disclosure, public actions

**`ExpectedIdentity` (one table used identically by producer, `compute-app-cli-identity`,
`validate-app-cli-provenance`, and every consumer):** `app-repo-id` (canonical decimal string in
**every** mode), `source-revision` (full SHA, passed explicitly — the helper takes it as an input,
not from a checkout), `app-cli-package`, `app-cli-bin`, `workspace-id`, `platform-id`,
`container-ref`. `platform-id`/`container-ref` are read from **`image.json` at the same EdgeZero
SHA**, never caller-selected.

**Schema:** `app-cli-meta.json` validated by a committed **JSON Schema 2020-12** file
(`additionalProperties: false`, nested `required`, `app-repo-id` decimal-string `pattern`,
digests/revisions `pattern`, arrays `uniqueItems`) **plus a procedural pass** the validator runs
(JSON Schema cannot enforce canonical-sorted order or reject duplicate keys): a **duplicate-key-
rejecting parse** and a canonical-order check. Fields = `ExpectedIdentity` + `app-cli-version` +
`binary-sha256` + `binary-size` + `abi` (`{ machine: "x86-64", interp, needed: [str] }`).

**Archive contract:** a tar of **exactly two** members named `app-cli-meta.json` and the
`app-cli-bin` value; **any extra/renamed member, symlink, traversal, or special file is rejected**;
total size ≤ **512 MiB** and each member ≤ its declared `binary-size`/a JSON cap; the extracted
binary's sha256/size re-verified against the meta.

**`validate-app-cli-provenance`** (runs in a fresh pinned container, `LD_*` scrubbed): inputs =
`artifact-tar` + the full `ExpectedIdentity`; enforce the archive contract; JSON-Schema + procedural
validate; re-verify binary digest/size; **ABI algorithm** — assert the binary is `ELF x86-64`, its
`PT_INTERP` is the container's loader, and it has no escaping `RPATH`/`RUNPATH` (same digest ⇒
libraries present by construction); compare every `ExpectedIdentity` field. Output `app-cli-path`.

**`active-version-fastly`**: inputs = `artifact-tar`, `ExpectedIdentity`, `fastly-service-id`,
`fastly-api-token`; validates, runs `active-version` via the launcher; **output `version`** (empty
on a first-ever **production** deploy = success). **Recovery is PRODUCTION-only** (`active-version`
cannot identify a staged draft; staging does not use it).

**`compute-app-cli-identity`**: inputs = `app-repository`/`app-repo-id`, **`source-revision`**,
`workspace-root`, `app-cli-package`/`app-cli-bin`; reads `container-ref`/`platform-id` from the
same-SHA `image.json`; outputs the full `ExpectedIdentity`.

**Public-deps-only — `crates.io` ONLY.** v1 fails closed unless **every** `Cargo.lock` source is
the `crates.io` registry. Git sources, alternate/private registries, and path deps outside the
workspace are rejected (this sidesteps the unbounded "is this git host/URL public and anonymous"
problem; git-dep support is §7). The pre-restore `--no-deps` metadata confirms static resolvability.

**Two named caches, one policy.** `build-app-cli.cache` (this spec) and `deploy-fastly.cache` (the
existing WASM cache) both carry the **same** normative policy: `disclosure-acknowledged` (covering
artifacts, both caches, and logs) is required when any reader set exceeds the app's, and **both are
crates.io-only**.

## 4. Testing

Own restore+save (four-root prune removes local-unit target outputs and stale registry/git sources;
approved-path audit rejects credential/config/`bin`/`src`/`checkouts`; save after final checks with
no app command after; failed save warns; failed prune skips save; cold-to-warm reuse with a lockfile
fallback; **feature/member-manifest edit with unchanged `Cargo.lock` busts the key**). Container/
runner (self-hosted fails closed; read-only rootfs blocks toolchain mutation; `wasm32-wasip1` +
pinned Fastly CLI present). Launcher (rejects a caller job container/remote daemon; enumerated
mounts only — no wholesale `RUNNER_TEMP`; config-push inline file mounted by exact path; host-side
`mutation-attempted` before mutation; cancellation `docker stop -t` within the grace window +
reconcile). Config/env (non-allowlisted caller key anywhere fails; action-owned `CARGO_HOME`/
`CARGO_TARGET_DIR` accepted; every native-env channel scrubbed; non-tracked/non-regular `Cargo.lock`
fails). Source (any non-`crates.io` source in `Cargo.lock` fails closed). Provenance (one
`ExpectedIdentity` string-typed end to end; schema + procedural dup-key/order; archive exactly-two-
members/size/digest; ABI ELF/interp/RPATH in a scrubbed container; a real wrong-runtime rejected;
provenance is consistency-only — an other-job self-asserted archive is documented as
not-authenticated). Recovery production-only via `active-version-fastly` `version` output. Disclosure
for both caches. Runner floor 2.336.0.

## 5. Rollout, docs, migration

**Atomic same-SHA rollout** (container image, reusable workflow, all three actions, consumers,
recovery); the direct-composite producer is retired, so adopters (one job today) migrate to the
**two-job** topology; the runner floor rises to **2.336.0**. Scope the parent's exact-key/
target-only caching language to `deploy-fastly.cache`; apply the disclosure/crates.io-only policy to
**both** named caches; add the parent's container-runner, action-owned-target, provenance,
single-producer, and 2.336.0 updates; correct the "consumers own checkout/runner/timeout; actions
never call `checkout`" claims. Pin gate/`zizmor`/actionlint: container digest pin, `$/` carve-outs.
Public-surface golden: the `ExpectedIdentity` table, the committed JSON Schema, all three actions.

## 6. Default and effect

**Off by default** (caching). Container execution + provenance are unconditional. With `cache: true`
on an authorized deployer build, a warm run restores compiled **crates.io dependencies** (the bulk
of the ~10 min); app + workspace-local crates recompile.

## 7. Out of scope / future

Workflow-bound artifact **attestation** (producer authentication); **git / alternate-registry**
dependencies (and thus non-`crates.io` sources); trusted **self-hosted** runner mode; cross-image/
directional ABI; alternate toolchains (a second container); non-default feature sets; `cli-profile`;
non-Fastly adapters.

## 8. History

… v6.10 (in-container consumers) → v6.11 (container-only, baked toolchain) → v6.12 (own restore+save,
full runtime container, one launcher, retire direct producer) → **v6.13**: narrow for
enforceability — **crates.io-only** sources (drop git-host allowlist/submodules/credential chasing),
**GitHub-hosted-only** runners (undetectable ephemeral self-hosted), **default-features-only**; hash
**every member manifest** + `-v2-` namespace; **prune all four cache roots** with a local-unit
(`source==null`) + build-event-stream retention rule; weaken the producer claim to
**supported-not-authenticated**; enumerate exact **launcher mounts** + cancellation deadline;
`app-repo-id` a **decimal string always required** + an explicit `source-revision` helper input +
`container-ref` from same-SHA `image.json`; a concrete **ABI/archive** contract + **procedural**
dup-key/order validation; **action-owned vs caller `CARGO_*`** split; runner floor **2.336.0**.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile (incl. the checksum-verified
Fastly CLI) + digest-pin + GHCR public/retention publish; the committed JSON Schema file; the
four-root prune's exact file-classification from the build event stream; and the
writer-fidelity/`crates.io`-classification predicate expressions.
