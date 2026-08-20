# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.11

**Related:** `docs/specs/edgezero-deploy-github-action.md`,
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI (native) with **no caching**, so every deploy
recompiles the whole dependency graph (~10 min for `stackpop/trusted-server-deployer`, which
checks out a **separate** application repo and builds its CLI). Caching must work for that
**cross-repository deployer** topology.

## 2. Trust model and v1 shape

- The build compiles **trusted code** (the deploy target); `build.rs` is already trusted.
- Caching runs **only for authorized deployer events/refs (fail before compiling otherwise)**;
  the runtime credential and the narrow app-checkout PAT are explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**; every writer on the selected/default
  cache branch is trusted, and the **deployer's protected workflow** allowlists which
  `app-repository`/`app-ref` it builds.
- **v1 is container-only.** Cross-job caching + provenance handoff is supported **only** through
  the reusable workflow running in one **pinned container image**; the producer and every
  CLI-executing consumer run in the **same** container digest, so build/run ABI **and the
  toolchain** are one immutable identity. The direct composite remains for **same-job/local**
  use and produces **no cross-job provenance** in v1. This removes the same-job/operator-env
  producer modes and the mutable-host ABI problem entirely.
- **v1 caches PUBLIC dependency graphs only** (§3.9).

## 3. Design

### 3.1 Reusable-workflow contract

| Input                     | Type    | Req.            | Default        | Meaning                                                                                                  |
| ------------------------- | ------- | --------------- | -------------- | -------------------------------------------------------------------------------------------------------- |
| `app-repository`          | string  | no              | caller repo    | `owner/repo`.                                                                                            |
| `app-ref`                 | string  | if repo         | —              | Full commit SHA.                                                                                         |
| `app-repo-id`             | number  | **yes** (cache) | —              | Immutable GitHub repo id (a checkout cannot derive it); producer **verifies** it against its API lookup. |
| `working-directory`       | string  | no              | `.`            | Compile cwd; beneath `workspace-root`.                                                                   |
| `workspace-root`          | string  | **yes**         | —              | Cargo workspace root, relative to the checkout.                                                          |
| `app-cli-package`         | string  | yes             | —              | Cargo package.                                                                                           |
| `app-cli-bin`             | string  | no              | package name   | Binary target.                                                                                           |
| `app-cli-artifact`        | string  | yes (matrix)    | `edgezero-cli` | **Unique per matrix leg.**                                                                               |
| `cache`                   | boolean | no              | `false`        | Enable caching.                                                                                          |
| `cache-key-suffix`        | string  | no              | `""`           | ≤64, `[A-Za-z0-9._-]`, then hashed.                                                                      |
| `disclosure-acknowledged` | boolean | no              | `false`        | Consent covering artifacts, caches, and logs (§3.9).                                                     |
| `timeout-minutes`         | number  | no              | `30`           | Job timeout.                                                                                             |

There is **no `rust-toolchain` input** under caching: the **container bakes one pinned
toolchain**, so `toolchain-id` is a property of the image, not a runtime selector. An app whose
`.tool-versions`/`rust-toolchain` requests a different toolchain **fails closed** (a different
toolchain ⇒ a different container image, which is future work).

Secret `app-checkout-token`: narrow `contents: read` PAT, required for a **non-public**
(private or internal) cross-repository app; a private same-repo app uses the default token;
public cross-repo needs none.

Outputs: `app-cli-artifact`, `app-cli-bin`, `app-cli-package`, `app-cli-version`
(**informational**), `app-cli-source-revision`, `app-cli-repo-id`, `app-cli-workspace-id`,
`app-cli-platform-id` (= the container manifest digest, which encodes toolchain + ABI),
**`app-cli-container-ref`** (the full pinned reference). Identity values are static, so
`compute-app-cli-identity` (§3.9) lets a matrix caller derive its own `expected-*`.

**Permissions:** job-level **`permissions: { contents: read }`** (forces `id-token`/all →
`none`; tested); the caller must grant ≥ `contents: read` (a called workflow can only reduce).
`persist-credentials: false` normative.

### 3.2 Self-composite reference

`$/.github/actions/build-app-cli` with a narrow pin-gate exemption for `$/.github/actions/...`
and a targeted actionlint suppression.

### 3.3 Action-owned paths

`CARGO_HOME` and `CARGO_TARGET_DIR` are **explicit action-owned, identity-scoped stable paths
under `RUNNER_TEMP`** (never inside the app checkout — that would violate the parent's
action-owned-target rule, risk deleting an app path, and make cleanliness depend on
`.gitignore`). rust-cache caches: the **target** via a `workspaces:` mapping whose RHS is a
**relative path that resolves outside the checkout** (rust-cache accepts a relative RHS; an
absolute one is joined beneath the root and resolves wrong), and **`CARGO_HOME`** via
`cache-directories:` (the relative `workspaces:` contract is for **targets only**, not the
Cargo home). Target reset before every restore, never after.

### 3.4 rust-cache pin, key, owned save, metadata order

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

- `cache-bin: false`; `cache-workspace-crates: false`; **`save-if: false`** — v1 **owns the
  save** (§below), because v2.9.2 swallows metadata/cleanup failures and can still save an
  **unpruned** `target` (retaining workspace-crate artifacts under a rolling, revisionless key).
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, `tuple-hash` = SHA-256 of a
  canonical length-prefixed tuple of `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, codegen-critical root-manifest sections only, and `platform-id` (the container
  digest, which already encodes the toolchain), plus the validated `cache-key-suffix`.
- **Owned fail-closed save.** After the build, an explicit step **prunes** the target to
  dependency artifacts only, **verifies non-empty**, and saves via `actions/cache/save`
  (fail-closed: no prune success or an empty result ⇒ no save). No unpruned/empty entry is ever
  published.
- **Metadata order (no pre-restore network / home pollution).** Pre-restore runs **structural
  `cargo metadata --no-deps --locked`** (no dependency fetch, so the clean Cargo home is not
  populated before restore). Restore into the reset dirs. Post-restore runs the **full
  `cargo metadata --all-features --locked`** (mirroring rust-cache's own call, deps now present)
  and verifies `Cargo.lock` byte-identical. Fail closed on any.

### 3.5 Identity

`git-root` (local path, confinement only); `app-repo` (`owner/repo`); **`app-repo-id`**
(immutable numeric id, a **required cache input**, producer-verified, emitted). `workspace-root`
canonicalized, confined beneath `git-root`, `working-directory` beneath it, asserted **`==
cargo metadata.workspace_root`**. `workspace-id` = hash(`app-repo-id`, workspace-root path
relative to `git-root`). `platform-id` = the container manifest digest (encodes glibc, linker,
system libs, **and the baked toolchain**).

### 3.6 Writer fidelity vs. source authorization

Action-enforced fidelity: caching runs only on `push`/`workflow_dispatch`/`schedule` on a
**protected deployer ref** with `HEAD == resolved app SHA`. Deployer-enforced authorization:
the deployer's protected workflow allowlists `app-repository`/`app-ref`; every cache-branch
writer is trusted. Documented normatively.

### 3.7 Container image and consumer execution

- **Image contract:** an EdgeZero-**published, public, retained, single-manifest `linux/amd64`**
  image, pinned by **manifest digest**, built from a versioned in-repo Dockerfile, baking one
  pinned Rust toolchain + glibc + linker + system libraries. Its digest is itself pinned/checked
  by the pin gate. `platform-id` = that digest.
- **One action-owned Docker launcher.** The producer runs in the container. CLI-executing
  **consumers stay composites** but execute the validated CLI through **one shared
  `run-app-cli-in-container` launcher** (`docker run` against the pinned digest) with a defined
  contract: **mounts** (the validated CLI + a read-only app checkout + a scratch output dir),
  **env** (only the required provider token + `EDGEZERO_*`; **no GitHub file-command channels**),
  **credentials** limited to that token, **cancellation** propagated to the container, and
  **cleanup** of the container + scratch. This is the single topology (no reliance on the
  caller's `jobs.<job>.container`, which a composite cannot set).
- **ABI is guaranteed by construction:** producer and consumer use the **same digest**, so
  glibc/interpreter/`DT_NEEDED`/toolchain match. The validator (§3.9) runs **inside a fresh
  pinned container with `LD_*` and loader variables scrubbed** and checks `PT_INTERP` exists,
  `DT_NEEDED` resolve, no escaping `RPATH`/`RUNPATH`, and symbol versions are provided — defense
  in depth over the digest identity. Full cross-image ABI analysis = §7.

### 3.8 Cargo config/source closure and env scrub

- **Full-chain config closure (cwd → `/` + `CARGO_HOME`, including the working directory
  itself), minimal allowlist, env included.** Fail closed unless every present key (files **and**
  `CARGO_*`/compiler env) is on the allowlist (registry **index** URLs, `net.retry`,
  `http.timeout`, `http.check-revoke`). Rejected/unset: `net.git-fetch-with-cli`, `net.offline`,
  credential providers, `[env]`, `build.rustc`, `RUSTC`, `RUSTC_WRAPPER`,
  `RUSTC_WORKSPACE_WRAPPER`, `RUSTDOC`, wrappers, `build.rustflags`, `[target.*]` rustflags/
  runner/linker, `[profile.*]` overrides, `paths`, source replacement/mirrors, `include`,
  `build.target`/`build.build-dir`, and **any config anywhere in the chain including the working
  directory**.
- **Scrub native-build channels** to baseline: `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`,
  `CARGO_BUILD_RUSTFLAGS`, target-qualified rustflags, and **`AR`, `LD`, `LDFLAGS`,
  `PKG_CONFIG_*`, `BINDGEN_EXTRA_CLANG_ARGS`, `CPATH`, `LIBRARY_PATH`**. Reject raised
  `target-cpu`/`target-feature` and `build.target`/`CARGO_BUILD_TARGET`; reject external path
  deps outside the workspace root. Confined build dirs (Cargo ≥ 1.91, in the container).
  Native/assembly `-march` in a `build.rs` remains an application responsibility.
- **Source freezing:** assert the **initial `HEAD` SHA unchanged** and the tree clean (tracked +
  untracked + recursive submodules) **before and after** all app-controlled commands; reject
  escaping symlinks. Consistency check, not tamper-proof.

### 3.9 Provenance, disclosure, helpers

- **Machine-readable schema.** `app-cli-meta.json` is validated against a committed **JSON
  Schema (draft 2020-12)** file: `$schema` = the 2020-12 dialect URI; a separate
  `edgezero-schema-version` discriminator; `additionalProperties: false` at every level; nested
  `required`; **`app-repo-id` encoded as a canonical decimal string**; digests/revisions/ids by
  `pattern`; arrays `uniqueItems` + canonically sorted; **duplicate JSON keys rejected before
  parsing**. Fields: repo/repo-id/source-revision/package/bin/version(informational)/
  workspace-id/platform-id/container-ref, and an `abi` object (machine, interpreter, libc,
  glibc-version, dt-needed[], glibc-symver[]).
- **`validate-app-cli-provenance`** (runs in a fresh pinned container, `LD_*` scrubbed) — inputs:
  `artifact-tar` (exactly one) + required `expected-app-repo-id`, `expected-source-revision`,
  `expected-app-cli-package`, `expected-app-cli-bin`, `expected-workspace-id`,
  `expected-platform-id`. Extract into a fresh owned root (unique expected members; reject
  traversal/symlink/special); JSON-Schema-validate; run the ABI algorithm (§3.7); compare every
  expected field. Output `app-cli-path`. No CLI execution.
- **`active-version-fastly`** — inputs: `artifact-tar`, all `expected-*`, `fastly-service-id`,
  `fastly-api-token`; validates, then runs `active-version` **through the launcher (§3.7)**.
- **`compute-app-cli-identity`** — inputs: `app-repository`/`app-repo-id`, `workspace-root`,
  `app-cli-package`/`app-cli-bin`, the pinned **`container-ref`**; outputs `workspace-id`,
  `platform-id`, **and `container-ref`** for a matrix caller's `expected-*`.
- **Per-consumer identity — every consumer REQUIRES the full expected set** (`app-repo-id`,
  `workspace-id`, `platform-id`, `app-cli-package`, `app-cli-bin`) from the producer outputs /
  helper; a checkout-backed consumer uses its checkout **only to verify** `source-revision` and
  the workspace location, **never to derive** the numeric id or `workspace-id` (a checkout has
  neither).
- **Disclosure consent covers artifacts, caches, and logs.** rust-cache stores registry/git
  **source** and native outputs, so even a **public** app can leak **private dependency** source
  via the cache. `disclosure-acknowledged: true` is required whenever any of those reader sets
  exceeds the app's (non-public app, or private deps), else fail closed. Because private-source
  authentication is out of scope, `cache: true` **rejects** `SSH_AUTH_SOCK`, credential-bearing
  dependency URLs, `CARGO_REGISTRIES_*_TOKEN`, and other private-source auth — **v1 caches only
  public dependency graphs**.

## 4. Testing

Contract/permissions; writer fidelity; **owned save** (a pruned non-empty save succeeds; an
induced prune/cleanup failure or empty target ⇒ **no save**); metadata order (pre-restore
`--no-deps` fetches nothing; post-restore full metadata + `Cargo.lock` identity); **true
cold-to-warm reuse** with the owned save; action-owned target/`CARGO_HOME` under `RUNNER_TEMP`
with a relative `workspaces:` RHS (a tracked/unignored `target/` in the checkout is untouched);
config anywhere including the working directory + every native-env channel fail closed; source
freezing (HEAD change/dirty/submodule before or after fails); JSON-Schema validation
(additionalProperties/dupes/bounds/decimal-string id); ABI algorithm in a scrubbed container
with **loader fixtures**; a **real wrong-runtime** consumer rejected; **helper/producer identity
parity**; the **Docker launcher** (mounts/env/no-file-commands/outputs/cancellation/cleanup);
**internal cross-repo authentication**; disclosure required for non-public/private-dep; private-
source auth signals fail closed; **every recovery path** through `active-version-fastly`.

## 5. Rollout, docs, migration

- **Atomic rollout.** Container execution, expanded metadata, disclosure, and required expected
  identities change **every** build — not only `cache: true`. Producer, `compute-app-cli-identity`,
  `validate-app-cli-provenance`, all consumers, and direct recovery must ship **at one EdgeZero
  SHA** so a deployer pinning that SHA gets a consistent surface.
- Scope the parent's exact-key/target-only caching language to `deploy-fastly.cache`; add its
  **container-runner, action-owned-target, provenance, and container-only-handoff** updates.
  Correct the "consumers own checkout/runner/timeout; actions never call `checkout`" claims.
- Docs: the container image contract + Dockerfile, the launcher, the three checkout cases (incl.
  internal), the disclosure rule (artifacts/caches/logs), `compute-app-cli-identity`, and the
  two-job topology.
- Pin gate/`zizmor`/actionlint: rust-cache SHA + non-SHA regression; `$/` carve-outs; the
  container digest pin. Public-surface golden: §3.1/§3.9 tables, per-consumer inputs, all three
  actions, the committed JSON Schema file.

## 6. Default and effect

**Off by default** (caching). Container execution and provenance are unconditional (§5). With
`cache: true` on an authorized deployer build, a warm run restores compiled **public
dependencies** (the bulk of the ~10 min); app + workspace-local crates recompile.

## 7. Out of scope / future

Cross-image / directional ABI and alternate/runtime-installed toolchains (a second container);
same-job/operator-env cross-job provenance; hermetic native-build sandboxing; private-registry/
git dependency **authentication** (and thus private-dep caching); `cli-profile`; non-Fastly
adapters.

## 8. History

… v6.8 (ImageVersion key) → v6.9 (pinned container as `platform-id`) → v6.10 (in-container
consumers, normative-schema intent, per-consumer table) → **v6.11**: v1 is **container-only**
(drop same-job/operator-env modes and the direct cross-job path); the container **bakes the
toolchain** (`toolchain-id` folds into the digest; no `rust-toolchain` input under cache); **own
a fail-closed save** (`save-if: false` + pruned non-empty `actions/cache/save`); pre-restore
`--no-deps` then post-restore full metadata (no pre-restore fetch/home pollution); action-owned
target + `CARGO_HOME` under `RUNNER_TEMP` with a relative `workspaces:` RHS and `cache-directories`
for the home; every consumer **requires** the full expected identity; one action-owned **Docker
launcher**; committed **JSON Schema 2020-12** with decimal-string ids; ABI algorithm in a scrubbed
container; disclosure covers **caches/logs** and `cache: true` is **public-deps-only** (reject
private-source auth); scrub `AR`/`LD`/`LDFLAGS`/`PKG_CONFIG_*`/`BINDGEN_*`/`CPATH`/`LIBRARY_PATH`
and reject working-dir config; **atomic same-SHA rollout**.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile`/launcher/helper signatures; the Dockerfile contents + digest-pin check;
the committed JSON Schema file; identity/allowlist canonicalization byte layouts; and the
writer-fidelity predicate expression.

## 10. Future: owned save

Superseded — v1 already owns the save (§3.4).
