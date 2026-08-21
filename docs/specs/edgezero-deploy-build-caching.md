# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.12

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
- **The reusable workflow is the ONLY public producer.** Build + deploy run in one **pinned
  container** (§3.7); provenance and container execution are unconditional. The **direct
  `build-app-cli` composite is retired as a standalone/cross-job producer** — it exists only as
  an internal step the reusable workflow invokes via `$/`. Lifecycle consumers accept **only**
  the reusable workflow's artifact (with provenance); a bare composite artifact is rejected.
- **v1 caches PUBLIC, statically-classifiable dependency graphs only** (§3.9).

## 3. Design

### 3.1 Reusable-workflow contract

Inputs: `app-repository` (default caller), `app-ref` (full SHA if repo set), **`app-repo-id`**
(number, required under cache; producer-verified), `working-directory` (default `.`, beneath
`workspace-root`), **`workspace-root`** (required), `app-cli-package` (required), `app-cli-bin`
(default: package), `app-cli-artifact` (required unique per matrix leg; default `edgezero-cli`),
`cache` (bool, default `false`), `cache-key-suffix` (≤64 `[A-Za-z0-9._-]`, hashed),
`disclosure-acknowledged` (bool, default `false`, §3.9), `timeout-minutes` (number, default 30).
**No `rust-toolchain` input** — the container bakes it. Secret `app-checkout-token` (narrow
`contents: read` PAT for a **non-public** cross-repo app).

Outputs = the `ExpectedIdentity` set (§3.9) plus `app-cli-artifact`, `app-cli-version`
(informational). Job-level **`permissions: { contents: read }`** (caller must grant ≥ that);
`persist-credentials: false` normative.

### 3.2 Self-composite reference

The reusable workflow invokes its own composite via **`$/.github/actions/build-app-cli`** (narrow
pin-gate exemption + actionlint suppression).

### 3.3 Action-owned paths

`CARGO_HOME` and `CARGO_TARGET_DIR` are explicit action-owned identity-scoped stable paths under
`RUNNER_TEMP`, mounted **writable** into the container (§3.7), never inside the app checkout.

### 3.4 Own BOTH restore and save (no rust-cache save-if)

rust-cache exposes only `cache-hit`, **not** its computed primary key or path list, so a
standalone `actions/cache/save` cannot reproduce its key. v1 therefore **owns both halves** with
`actions/cache/restore` + `actions/cache/save` (no rust-cache), controlling the key, the path
list, and cleaning:

- **Key** = `edgezero-build-app-cli-v1-<tuple-hash>` + `-<lock-hash>`, `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, codegen-critical root-manifest sections, `platform-id` (container digest, which
  encodes toolchain + ABI), and the validated `cache-key-suffix`; `lock-hash` = the tracked
  `Cargo.lock` hash. Exact key; a matching restore-key prefix (minus `lock-hash`) allows a warm
  fallback across dependency edits.
- **One approved path list** (nothing else): `CARGO_TARGET_DIR`, `CARGO_HOME/registry/index`,
  `CARGO_HOME/registry/cache`, `CARGO_HOME/git/db`. **Never** `CARGO_HOME/bin`, config,
  credentials, `registry/src`, or `git/checkouts`.
- **Ordered save sequence, no app command afterward:** build → package the CLI → **final
  lock/HEAD/tree/submodule checks (§3.8)** → derive the retained artifact graph → **prune** the
  target to dependency artifacts (drop workspace-crate outputs) and **audit** the path list
  against the approved set (fail closed on any extra/credential/config path) → `actions/cache/save`.
  Save is **best-effort persistence** (`actions/cache/save` reports several failures as
  warnings); a failed save never fails the build, and a failed prune/audit **skips** the save.
- **Metadata order:** pre-restore `cargo metadata --no-deps --locked` (no fetch, clean home);
  restore into reset dirs; post-restore `cargo metadata --locked` with the **build's exact feature
  set** (§3.8), `Cargo.lock` verified byte-identical. Fail closed on any.

### 3.5 Identity

`git-root` (path, confinement); `app-repo` (`owner/repo`); `app-repo-id` (immutable numeric,
required-under-cache, producer-verified, emitted). `workspace-root` canonicalized, confined
beneath `git-root`, `working-directory` beneath it, asserted `== cargo metadata.workspace_root`.
`workspace-id` = hash(`app-repo-id`, workspace-root rel `git-root`). `platform-id` = the container
manifest digest.

### 3.6 Writer fidelity vs. source authorization

Action fidelity: cache runs only on `push`/`workflow_dispatch`/`schedule` on a **protected
deployer ref** with `HEAD == resolved app SHA`. Deployer authorization: the protected workflow
allowlists the app identity; every cache-branch writer is trusted. Normative in the guide.

### 3.7 Container (full build+deploy runtime) and the Docker launcher

- **Image:** EdgeZero-published, public, retained, **single-manifest `linux/amd64`**, pinned by
  **manifest digest**, from a versioned in-repo Dockerfile. It **bakes the full runtime** the
  pipeline needs: the pinned Rust toolchain, **`wasm32-wasip1`**, the **pinned Fastly CLI**
  (matching `versions.json`), and `git jq tar curl cc`. `platform-id` = its digest.
- **Immutability at runtime:** the container runs **`--read-only` root filesystem, non-root
  user**, with **only explicit writable mounts** (a `tmpfs` `/tmp`, `CARGO_TARGET_DIR`,
  `CARGO_HOME`, a provider/Fastly home, a package/output dir), so app-controlled `build.rs`
  cannot alter the baked toolchain/libraries before linking. Same digest ⇒ same ABI **and** an
  unmodifiable toolchain.
- **One action-owned launcher `run-app-cli-in-container`** (used by the producer and every
  CLI-executing consumer, which stay composites but never rely on the caller's `jobs.<job>.container`):
  - **Runner contract (fail closed otherwise):** a **host-level Linux x64 job** (not already
    inside a job container), a **local Docker daemon** (reject a remote daemon that cannot bind
    runner-local paths), **anonymous image pull** by digest, an **ephemeral runner**; defined
    **UID/GID mapping** (the container's non-root build user maps to the runner UID so the
    writable mounts are owned correctly); self-hosted prerequisites documented.
  - **Env:** only the required provider token + `EDGEZERO_*`; **no GitHub file-command channels**
    inside the container.
  - **Signals/outputs:** a **host↔container readiness handshake**; the **`mutation-attempted`
    signal is published host-side to `$GITHUB_OUTPUT` BEFORE launching the mutating CLI**
    (preserving the existing pre-mutation signal ordering); a **named container** with host-side
    **signal forwarding** (SIGTERM/SIGINT → `docker stop` with a bounded timeout → `docker rm`)
    and **post-cancel reconciliation** as today.

### 3.8 Cargo config/source closure, feature graph, env scrub

- **Config closure:** the effective Cargo config over the full chain (cwd → `/`, **including the
  working directory itself**, plus `CARGO_HOME`) **and** `CARGO_*` env — **fail closed unless
  every present key is on a minimal allowlist** (registry **index** URLs, `net.retry`,
  `http.timeout`, `http.check-revoke`). Any other key (wrappers, `RUSTC*`, `RUSTDOC`, rustflags,
  `[env]`, `[profile.*]`, `[target.*]` runner/linker, `paths`, source replacement, `include`,
  `build.target`/`build.build-dir`, `net.git-fetch-with-cli`, `net.offline`, credential providers)
  **fails closed** — this is an allowlist, not a blanket file rejection.
- **Env scrub to baseline:** `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, `CARGO_BUILD_RUSTFLAGS`,
  target-qualified rustflags, `AR`, `LD`, `LDFLAGS`, **`CC`, `CFLAGS`, `CXX`, `CXXFLAGS`,
  `CPPFLAGS`, `CMAKE_*`**, `PKG_CONFIG_*`, `BINDGEN_EXTRA_CLANG_ARGS`, `CPATH`, `LIBRARY_PATH`.
  Reject raised `target-cpu`/`target-feature`; native `-march` in a `build.rs` remains an app
  responsibility (bounded by the read-only container, not eliminated).
- **One exact feature graph:** metadata and build use the **same** feature set (the CLI's default
  features, or an explicit set) — no `--all-features` mismatch. **`Cargo.lock` must be a tracked,
  regular file** (fail closed otherwise). External path deps outside the workspace root are
  rejected.
- **Source freezing:** initial `HEAD` SHA unchanged + tree clean (tracked + untracked + recursive
  submodules) **before and after** all app-controlled commands; reject escaping symlinks.

### 3.9 Identity, provenance, disclosure, public actions

**`ExpectedIdentity` (one table, used identically by producer, helper, validator, every
consumer):** `app-repo-id`, `source-revision`, `app-cli-package`, `app-cli-bin`, `workspace-id`,
`platform-id`, `container-ref`. `platform-id`/`container-ref` are a **same-EdgeZero-SHA constant**
(from `image.json` at that SHA), **not** caller-selected. This exact set appears in the workflow
outputs, `compute-app-cli-identity` outputs, the validator inputs, and every consumer's inputs.

**Schema:** `app-cli-meta.json` validated against a committed **JSON Schema 2020-12** file
(`$schema` = the 2020-12 dialect; `additionalProperties: false`; nested `required`; `app-repo-id`
as a canonical decimal string; digests/revisions by `pattern`; arrays `uniqueItems` + sorted;
duplicate JSON keys rejected pre-parse). Fields = the `ExpectedIdentity` set + `app-cli-version`

- **`binary-sha256` + `binary-size`** + an `abi` object.

**Archive contract:** the artifact is a tar with **exactly** two members — the binary and
`app-cli-meta.json` — normalized names, **extra members rejected**, a size limit, and the binary's
recorded `binary-sha256`/`binary-size` re-verified on extraction.

**`validate-app-cli-provenance`** (runs in a fresh pinned container, `LD_*` scrubbed): inputs =
`artifact-tar` + the full `ExpectedIdentity`; extract to a fresh owned root (exactly the two
members; reject traversal/symlink/special/extra); JSON-Schema-validate; re-verify binary
digest/size; run the ABI check (§3.7); compare every `ExpectedIdentity` field. Output `app-cli-path`.

**`active-version-fastly`**: inputs = `artifact-tar`, `ExpectedIdentity`, `fastly-service-id`,
`fastly-api-token`; validates, then runs `active-version` via the launcher; **output `version`**
(empty on a first-ever **production** deploy = success). **Recovery is PRODUCTION-only** —
`active-version` cannot identify a staged draft, so the staging path does not use it.

**`compute-app-cli-identity`**: inputs = `app-repository`/`app-repo-id`, `workspace-root`,
`app-cli-package`/`app-cli-bin`, the same-SHA `container-ref`; outputs the full `ExpectedIdentity`
for matrix callers.

**Public-deps-only, statically classified.** Rather than trying to reject every auth signal
(Cargo credential files/providers, Git credential helpers, `.netrc`, `GIT_ASKPASS`, global Git/SSH
config, unauthenticated private-network registries — unbounded), v1 **statically classifies the
`Cargo.lock` source set** and fails closed unless every source is **crates.io** or a **git host on
an explicit public allowlist** (e.g. `github.com` public repos). Path deps outside the workspace
are already rejected. The pre-restore `--no-deps` metadata against an empty scrubbed home confirms
static resolvability.

**Two named caches, one policy.** `build-app-cli.cache` (this spec) and the existing
`deploy-fastly.cache` are named distinctly, and **both** carry the **same** normative
disclosure/source policy: `disclosure-acknowledged` (covering artifacts, both caches, and logs) is
required whenever any reader set exceeds the app's (non-public app or private deps), and **both are
public-deps-only**.

## 4. Testing

Own restore+save (approved-path audit rejects a credential/config/`bin` path; prune produces a
non-empty dep-only target; save after the final checks with **no app command afterward**; a failed
save warns, does not fail; a failed prune/audit skips save; true cold-to-warm reuse with a
restore-key fallback across a dep edit). Container (read-only rootfs blocks toolchain mutation;
`wasm32-wasip1` + pinned Fastly CLI present; writable mounts owned via UID/GID mapping). Launcher
(rejects a caller job container / remote daemon / non-ephemeral; host-side `mutation-attempted`
published before mutation; cancellation forwards to `docker stop`/`rm` and reconciles). Config/env
(any non-allowlisted key anywhere including cwd fails; every native-env channel scrubbed;
`--all-features` mismatch removed; a non-tracked/non-regular `Cargo.lock` fails). Source
classification (a private-registry or non-allowlisted git source in `Cargo.lock` fails closed).
Provenance (one `ExpectedIdentity` end to end; schema incl. decimal-string id/dupes/bounds; archive
exactly-two-members + binary digest/size; ABI in a scrubbed container with loader fixtures; a real
wrong-runtime rejected). Recovery **production-only** through `active-version-fastly` with its
`version` output; staging path does not call it. Disclosure required for both caches;
`deploy-fastly.cache` gains the same policy. Direct composite rejected as a standalone producer.

## 5. Rollout, docs, migration

**Atomic same-SHA rollout** across the container image, reusable workflow, `compute-app-cli-identity`,
`validate-app-cli-provenance`, `active-version-fastly`, all consumers, and recovery — provenance +
container execution change **every** deploy, and the **direct-composite producer is retired**, so
adopters (e.g. `trusted-server-deployer`, currently one job) migrate to the **two-job** topology at
that SHA. Scope the parent's exact-key/target-only caching language to `deploy-fastly.cache`; apply
the disclosure/public-deps policy to **both** named caches; add the parent's container-runner,
action-owned-target, provenance, and single-producer updates; correct the "consumers own
checkout/runner/timeout; actions never call `checkout`" claims. Pin gate/`zizmor`/actionlint:
container digest pin (§ sub-plan 1), `$/` carve-outs. Public-surface golden: the `ExpectedIdentity`
table, the committed JSON Schema, and all three actions.

## 6. Default and effect

**Off by default** (caching). Container execution + provenance are unconditional. With `cache: true`
on an authorized deployer build, a warm run restores compiled **public dependencies** (the bulk of
the ~10 min); app + workspace-local crates recompile.

## 7. Out of scope / future

Cross-image / directional ABI; alternate/runtime toolchains (a second container); private-registry/
git dependency authentication (and private-dep caching); non-`crates.io`/non-allowlisted sources;
`cli-profile`; non-Fastly adapters.

## 8. History

… v6.9 (pinned container `platform-id`) → v6.10 (in-container consumers, schema intent) → v6.11
(container-only, baked toolchain, save-if:false, `--no-deps` order) → **v6.12**: **own both cache
restore and save** with `actions/cache` (no rust-cache; approved path list + fail-closed prune/audit

- ordered best-effort save); the container **bakes the full build+deploy runtime** (`wasm32-wasip1`
- pinned Fastly CLI) and runs **read-only/non-root with explicit writable mounts**; **one Docker
  launcher** with a runner contract, host-side `mutation-attempted` publication, and cancellation
  forwarding; **retire the direct composite as a public producer** (reusable workflow only); **one
  `ExpectedIdentity`** table used everywhere; **static `Cargo.lock` source classification** for
  public-deps-only; both named caches share the disclosure/source policy; add `CC/CFLAGS/CXX/CXXFLAGS/
CPPFLAGS/CMAKE_*` scrub, one exact feature graph, a tracked-regular `Cargo.lock`, the exact archive
  contract with binary digest/size, and **production-only recovery**.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile contents (now incl.
`wasm32-wasip1` + Fastly CLI) + digest-pin check; the committed JSON Schema file; the public-git
allowlist contents; and the writer-fidelity/source-classification predicate expressions.
