# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.4, reusable build workflow (deployer-owned cache)

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) into a throwaway
`mktemp` `CARGO_TARGET_DIR` with **no caching**, so every deploy recompiles the whole
dependency graph (~10 min for `stackpop/trusted-server-deployer`, which checks out a
**separate** application repository and builds its CLI). Caching must work for that
**cross-repository deployer** topology and must not weaken the existing credential model.

## 2. Trust model and decision

### 2.1 The build compiles trusted code

The primary topology is a **deployer** repository that checks out a **separate**
application repository at a chosen ref and builds + deploys it (e.g.
`trusted-server-deployer` building `trusted-server`). The deploy step already **runs the
application's own CLI with the provider token** — so under the project's existing "trust
the code you deploy" model, the application's `build.rs`/proc-macros are **already
trusted** at build time. Caching that build therefore does **not** widen the trust
boundary: it compiles the same code the deploy will run.

### 2.2 The deployer owns and writes its cache

GitHub caches are **repository-scoped to the repository whose workflow runs** — here the
**deployer**. The deployer is therefore both the **writer and reader** of its own cache
namespace, so the cross-repository build **warms normally** (an earlier v6.3 rule made
cross-repo restore-only, which — combined with repo-scoping — could never warm; that rule
is removed).

### 2.3 A reusable workflow owns the credential-free build job

A composite action cannot keep the build job free of the provider token (the caller adds
later steps; rust-cache's `post:` hook saves at job cleanup after them). Caching is
delivered by a **reusable workflow** (`on: workflow_call`) that owns the build job:

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
             (deployer-owned; no PROVIDER token; checks out the app; caches; uploads the CLI)
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

**Credential wording (precise).** The build job still receives a permissions-limited
`GITHUB_TOKEN` (deployer `contents: read`), the runtime **cache token**, and — for a
private app — a narrowly scoped **app-checkout credential**. It receives **no provider
credential and no OIDC** (`permissions: { contents: read }`, no `id-token`, no provider
secrets). A compromised (untrusted) application could observe those tokens via a detached
process, but a compromised deployment target already defeats the deploy itself; this is
the **same** trust assumption, not a new one, and the app-checkout credential is kept
narrow (`contents: read` on the app repo — reading code already checked out) to bound it.

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`: `app-repository` (default: caller repo), `app-ref` (required full SHA when
`app-repository` is set), `working-directory` (default `.`), `app-cli-package` (required),
`app-cli-bin` (default: package name), `rust-toolchain` (default `auto`),
`app-cli-artifact` (default `edgezero-cli`), `cache` (default `false`), `cache-key-suffix`,
`timeout-minutes`.

- **No `runs-on` input** — the runner is fixed (§3.8); a caller label cannot be validated
  before GitHub dispatches the job (and any checkout secret) to it.
- **`persist-credentials: false` is normative** on the app checkout, and the resulting Git
  config is tested. Note this does not remove the checkout action's **post-step**
  credential exposure (the runner re-evaluates `INPUT_TOKEN` at cleanup); the narrow-PAT
  scoping (§2.3) is what bounds that residual.

`secrets`: `app-checkout-token` (optional) — a **narrowly scoped, stored fine-grained
PAT** (`contents: read` on the app repo) for a **private** application repository (a called
workflow cannot mint a GitHub-App token in a caller step, and that action revokes at job
cleanup). A private `app-repository` without it fails closed. Never a provider credential.

`outputs` (single CLI per call): `app-cli-artifact`, `app-cli-bin`, `app-cli-package`,
`app-cli-version`, `app-cli-source-revision`. **Matrix handoff must not use these shared
outputs** (GitHub keeps only the last leg's); a matrix caller uses the **unique per-leg
`app-cli-artifact`** names, and provenance (§3.7) binds package/binary/workspace so a
wrong-artifact selection is rejected.

### 3.2 Referencing the workflow's own composite

The reusable workflow references its own composite via **`$/.github/actions/build-app-cli`**
(self-repo, commit-aligned; `./…` would resolve in the caller's checkout). A **narrow
pin-gate exemption** for `$/.github/actions/...` and a **targeted actionlint suppression**
are required (scoped to that prefix, removed when upstream actionlint supports `$/`).

### 3.3 Internal lifecycle boundary

rust-cache runs **between resolution and compilation**, so the composite's internals split
into a workflow-orchestrated boundary:

```
prepare (resolve/install toolchain; export RUSTUP_TOOLCHAIN; host triple;
         canonical workspace + app identity; validated target/build-dir + cache identities)
  → validate + reset the stable target (§3.4)
  → Swatinem/rust-cache restore (§3.5)
  → compile + stage + upload   (single upload owner; MUST NOT reset the restored target)
```

`prepare` emits a **private, validated** handoff (not app-controlled free strings). The
**public `build-app-cli` composite is unchanged and gains no `cache` input**; the reusable
workflow consumes `cache`/`cache-key-suffix`. Exact `prepare`/`compile` signatures are
pinned in the plan (§9).

### 3.4 Stable target: reset-before-every-restore, identity-scoped

- The cached `CARGO_TARGET_DIR` is a **stable path outside** the per-invocation workspace
  (surviving composite cleanup for the job-end save).
- The workflow **resets the guarded stable target before every restore, never after** (rust-cache
  does not clear it on a miss), so a remote miss cannot reuse stale local artifacts.
- The target path and `prefix-key` include canonical **app-repository + workspace
  identity** so two apps/workspaces from one deployer never collide.

### 3.5 rust-cache pin, configuration, and writer authorization

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`** (regression test
rejects every non-SHA rust-cache ref).

- `cache-bin: false`; `cache-workspace-crates: false` (rust-cache's lexical `startsWith`).
- `prefix-key` = `edgezero-build-app-cli-v1-<app-repo-id>-<workspace-id>-<hosted-image-id>-<suffix-hash>`.
- `workspaces:` maps the canonical Cargo workspace to the stable target as a path
  **relative to that workspace**.
- **`RUSTUP_TOOLCHAIN`** (resolved app toolchain) exported for **restore, compile, and
  post-save**, so rust-cache's bare `rustc`/`cargo metadata` match the compile.
- **Writer authorization.** Saving requires a **trusted deployer event/ref allowlist**
  (e.g. `push`/`workflow_dispatch` on the deployer's protected refs) **and the actual
  checkout `HEAD` == the resolved app SHA** — `workflow_dispatch` supplying a same-repo
  `app-ref` alone is insufficient. Fork-PR contexts to the deployer **never write**. All
  mismatches are restore-only. The deployer writing its own cache for a ref it chose to
  deploy is the **same** trust as the deploy itself (§2.1).
- **No empty save.** The workflow verifies the post-hook `cargo metadata` command succeeds
  and precludes the save when it cannot. Because stock rust-cache swallows a post-hook
  metadata failure and can still save an empty entry (immutable, non-repairing under an
  exact hit), the **residual** is documented — recover by rotating `cache-key-suffix`; a
  fully owned save phase (or a pinned fork that refuses to save on its own metadata
  failure) is **§7 future work**. Save is conditional (exact hit → no save;
  first-writer-wins on concurrent misses).

### 3.6 Native build semantics + JSON artifact discovery + one Cargo cwd

- No forced `--target` (explicit-target mode changes host build-script/proc-macro
  `RUSTFLAGS` and layout, breaking `cache: false` parity). Native semantics; the executable
  is the single `--message-format=json` `compiler-artifact` matching `package_id`, binary
  `target.name`, `target.kind` ⊇ `bin`, non-null `executable`, after `build-finished`,
  canonicalized beneath the owned target with execute permission.
- **One Cargo working directory.** The compile and rust-cache's workspace root use the
  **same** cwd (the resolved `working-directory`), so the effective `.cargo/config` chain is
  identical for both. Ancestor and **extensionless** `.cargo/config`, a virtual-workspace
  root `Cargo.toml`, and external **path dependencies** are either **identity-bound into the
  key** or **rejected** (fail closed) rather than silently under-hashed.
- `cache: true` requires **Cargo ≥ 1.91** (for `build.build-dir`), else fail closed.

### 3.7 Provenance — one complete identity, per-consumer source

`app-cli-meta.json` carries a **schema version** and a **complete identity**: `app-repo`,
`source-revision`, `app-cli-package`, `app-cli-bin`, and **`workspace-id`** — all derived
from the **actual checkout** under a clean/unchanged-tree requirement. A **single strict
schema and one shared validator** (a callable script/action) are defined. Every consumer
validates the full identity **before any downloaded CLI executes** (including `--help`) and
before provider credentials are exposed:

- consumers **with a checkout** (deploy-fastly, config-push) compare against their own
  checkout;
- **checkout-less** consumers (`healthcheck-fastly`, `rollback-fastly`, and the
  **lost-version recovery** flow) gain **explicit expected `app-repo`/`source-revision`/
  `app-cli-package`/`workspace-id` inputs** and call the shared validator **before** the
  credential-bearing CLI invocation.

This is a **consistency check, not a cryptographic boundary** (a trusted app can self-assert
metadata); it catches wrong-repo/revision/package handoffs. Exact field names/normalization
and the unknown-schema policy are pinned in the plan (§9).

### 3.8 Runner policy and ABI baseline

`cache: true` runs on a **single literal GitHub-hosted Linux x64 image label** (a specific
`ubuntu-<version>`, not a range), so `ImageOS`/`ImageVersion` are enforceable. The produced
binary is native GNU/Linux x64; the workflow **defines and enforces a glibc/ABI baseline**
and records it in provenance so a consumer on an incompatible Linux (older glibc, musl,
self-hosted) is rejected **before** the binary reaches a credential-bearing step. Persistent
self-hosted build runners are unsupported for caching (residual credentials/processes from
prior jobs); an immutable-image self-hosted mode is §7 future work.

### 3.9 Security — reader/writer trust and cache ownership

- **Cache ownership = the deployer repository.** A **public deployer** running untrusted PR
  workflows could expose cached **private application dependency source** to **its own** fork
  PRs; the reader-trust precondition is stated against the **deployer** repo's PR posture (a
  private/no-fork-PR deployer like `trusted-server-deployer` satisfies it).
- **Reader trust.** The cache stores dependency **source** (`.crate`, `git/db`) and
  `target/`, and rust-cache resolves the **whole workspace**, so any private dependency in it
  is exposed to cache readers of the **deployer** repo.
- **Writer trust.** §3.5 — trusted deployer trigger + `HEAD == resolved app SHA`; the same
  trust as the deploy.
- **Build-environment identity.** The fixed literal image (§3.8) plus `ImageOS`/`ImageVersion`
  in the `prefix-key` cover libc/linker/image, which rust-cache's key does not.

## 4. Testing

- **Workflow contract:** `workflow_call` inputs/outputs/secrets; `$/.github/actions/...`
  resolution + carve-outs; single-CLI outputs + a matrix no-wrong-artifact case;
  cross-repository (`app-repository`/`app-ref` + PAT) **including a warm second run**, and
  private fail-closed; `persist-credentials: false` Git config.
- **Writer authorization:** save only on the trusted allowlist **and** `HEAD == resolved app
SHA`; a mismatched `app-ref` is restore-only; fork-PR never writes.
- **Cache lifecycle:** reset-before-restore and **no reset after**; target **survives past
  post**; identity collisions (two apps/workspaces) don't share; exact / partial / miss
  behavior; **post-only** metadata failure → **no save**.
- **Toolchain binding:** a non-default app toolchain drives restore/compile/**post-save**.
- **Cargo/artifact:** JSON selection; native build-script/proc-macro/`RUSTFLAGS` parity;
  one-cwd config-chain equality; ancestor/extensionless config, virtual root, and path deps
  bound-or-rejected; Cargo < 1.91 fails closed.
- **ABI:** an incompatible-libc consumer is rejected before any CLI execution.
- **Provenance:** every consumer (deploy/healthcheck/rollback/config-push **and recovery**)
  rejects a mismatched repo/revision/package/bin/workspace before any CLI execution.
- **Token exposure:** the build/cache steps carry no provider credential/OIDC; the
  app-checkout PAT is `contents: read`-scoped.

## 5. Docs and migration

- Scope the parent's exact-key / target-only caching language to **`deploy-fastly.cache`**;
  define **`build-app-cli.cache`** as a separate **rolling, deployer-owned** cache.
  `deploy-fastly` unchanged.
- Correct the guide/adoption-guide claims that consumers own checkout/runner/timeout and the
  actions never call `checkout` — false for the reusable workflow. Document the two-job
  topology (with the cross-repository deployer as the worked example), the fixed runner/ABI
  baseline, and the §3.9 reader/writer preconditions (naming the **deployer** as cache owner).
- Pin gate / `zizmor` / actionlint: rust-cache SHA + non-SHA regression; the `$/` carve-outs.
- Public-surface golden: the reusable-workflow inputs/outputs and the new checkout-less
  consumer provenance inputs.

## 6. Default and effect

**Off by default.** With `cache: true` a warm deployer build restores compiled
**dependencies** (the bulk of the ~10 min); app + workspace-local crates recompile.
rust-cache's save-time cleaning prunes the newly saved entry; it saves only on a miss under
an authorized writer (§3.5).

## 7. Out of scope / future

- A fully owned/forked rust-cache **save** phase that refuses to save on its own post-hook
  metadata failure (v1 documents the residual + suffix rotation).
- `cli-profile` (a faster CLI build profile — a separate, cache-free win).
- Private-registry/git dependency **authentication** for the cached build.
- A self-hosted trust mode with an immutable image identity.
- Caching for non-Fastly adapters.

## 8. History (for the record)

v2–v5 (in-place, token in the job) → v6 (composite can't isolate) → v6.1 (reusable workflow)
→ v6.2 (workflow/API + lifecycle) → v6.3 (fixed runner, RUSTUP_TOOLCHAIN, matrix outputs,
persist-credentials) → **v6.4** (cross-repository deployer as the primary topology: the build
compiles trusted deploy-target code, the deployer owns and writes its own cache — so cross-repo
warms; writer authorization binds `HEAD == resolved app SHA`; complete provenance identity to
every consumer incl. recovery; one Cargo cwd with bound/rejected local sources; literal image +
ABI baseline).

## 9. Deferred to the implementation plan

Exact `prepare`/`compile` interface signatures; the provenance schema literal, field
normalization, and unknown-schema policy; the `save-if`/`HEAD == resolved app SHA` predicate
expressions and the pre-save metadata-success check; the glibc/ABI baseline value and its
check; and the `app-repo-id`/`workspace-id`/`hosted-image-id` canonicalization and length
bounds.
