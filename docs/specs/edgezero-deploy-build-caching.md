# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.3, reusable build workflow

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) into a throwaway
`mktemp` `CARGO_TARGET_DIR` with **no caching**, so every deploy recompiles the whole
dependency graph (~10 min for `stackpop/trusted-server-deployer`). Safe caching
requires a build environment that provably exposes **no provider credential** to the
build/cache steps.

## 2. Decision: a reusable workflow owns the credential-free build job

A composite action cannot enforce a credential-free job (the caller adds later steps to
the same job; rust-cache's `post:` hook saves at job cleanup after them). Caching is
delivered by a **reusable workflow** (`on: workflow_call`) that owns the build job
end-to-end; a caller invokes it as a job and **cannot inject steps into it**.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
             (owns a provider-credential-free job; caches; uploads the CLI artifact)
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

**Credential wording (precise).** A reusable workflow still receives a
permissions-limited `GITHUB_TOKEN`, and `actions/checkout` consumes it even with
`persist-credentials: false`. The guarantee is **"no provider credential or OIDC
capability is exposed to the build/cache steps"** — not "no token exists." The workflow
declares `permissions: { contents: read }`, **no `id-token`**, requests **no provider
secrets**, and passes none to the build/cache steps.

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`: `app-repository` (default: caller repo), `app-ref` (required full SHA when
`app-repository` is set), `working-directory` (default `.`), `app-cli-package`
(required), `app-cli-bin` (default: package name), `rust-toolchain` (default `auto`),
`app-cli-artifact` (default `edgezero-cli`), `cache` (default `false`),
`cache-key-suffix`, `timeout-minutes`.

- **No `runs-on` input.** The runner is fixed by the workflow (§3.8); it is not
  caller-controlled, because a caller-supplied label cannot be validated before GitHub
  has already dispatched the job (and any checkout secret) to that runner.
- **`persist-credentials: false` is normative** on the app checkout — not advisory —
  and the resulting Git config (no token left for app-controlled Git operations) is
  tested.

`secrets`: `app-checkout-token` (optional) for a **private** application repo in the
separate-repo layout. Because a reusable-workflow _calling job cannot contain steps_, a
GitHub-App token cannot be minted in an earlier caller step (and that action revokes the
token at job cleanup anyway). For v1, **private cross-repository caching requires a
stored fine-grained PAT** with `contents: read` on the app repo, passed as this secret;
a private `app-repository` without it fails closed. It is a checkout credential, never a
provider credential.

`outputs` (single CLI per call): `app-cli-artifact`, `app-cli-bin`, `app-cli-package`,
`app-cli-version`. **Matrix handoff must not use these shared outputs** — GitHub retains
only the last successful matrix leg's output, so two CLIs from the same repo/SHA could
select the wrong artifact. A matrix caller uses the **unique per-leg `app-cli-artifact`
names** (computed independently) or an aggregate manifest; provenance (§3.7) also binds
package/binary/workspace identity so a wrong-artifact selection is rejected.

### 3.2 Referencing the workflow's own composite

The reusable workflow references its own composite via **`$/.github/actions/build-app-cli`**
(resolves against the called workflow's repository and running commit — the only
version-aligned reference; `./…` would resolve in the caller's checkout). Because the pin
gate treats `$/…` as unpinned and actionlint 1.7.7 does not parse it, a **narrow pin-gate
exemption** for `$/.github/actions/...` (a self-repo, commit-aligned local reference) and
a **targeted actionlint suppression** are required, scoped to that prefix and removed
when upstream supports `$/`.

### 3.3 Internal lifecycle boundary

rust-cache must run **between resolution and compilation**, so `build-app-cli`'s internals
are split into a boundary the reusable workflow orchestrates:

```
prepare (resolve/install toolchain; export RUSTUP_TOOLCHAIN; host triple;
         canonical workspace + app identity; validated target/build-dir + cache identities)
  → validate + reset the stable target (§3.4)
  → Swatinem/rust-cache restore (§3.5)
  → compile + stage + upload      (single designated upload owner; MUST NOT reset the restored target)
```

`prepare` emits a **private, validated** handoff (toolchain, package id, canonical
workspace root, target/build directories, cache identities) — not app-controlled free
strings. The **public `build-app-cli` composite is unchanged** and gains **no `cache`
input** (a direct `build-app-cli(cache: true)` would reintroduce the isolation problem);
the reusable workflow consumes `cache` / `cache-key-suffix`. The exact
`prepare`/`compile` interface signatures are pinned in the implementation plan (§9).

### 3.4 Stable target: reset-before-every-restore, identity-scoped

- The cached `CARGO_TARGET_DIR` is a **stable path outside** the per-invocation workspace
  (so composite cleanup cannot delete it before the job-end save).
- Because rust-cache does not clear the target on a miss, the workflow **resets the
  guarded stable target before every restore, and never after** — restore repopulates it
  deterministically, so a remote miss cannot reuse stale local artifacts.
- The target path and `prefix-key` both include canonical **application-repository and
  workspace identity** (plus image/env and suffix, §3.5), so two apps/workspaces from one
  deployer repo never collide.

### 3.5 rust-cache pin and configuration

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

The pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`** (a
regression test rejects **every** non-SHA rust-cache ref), alongside the general
version-tag policy.

- `cache-bin: false`; `cache-workspace-crates: false` (excludes crates by rust-cache's
  **lexical `startsWith`**, not Cargo's `workspace_members`).
- `prefix-key` replaces rust-cache's default with
  `edgezero-build-app-cli-v1-<app-repo-id>-<workspace-id>-<hosted-image-id>-<suffix-hash>`.
- `workspaces:` maps the canonical Cargo workspace to the stable target as a path
  **relative to that workspace**.
- **Toolchain binding:** `RUSTUP_TOOLCHAIN` (the resolved app toolchain) is exported for
  the **restore, compile, and post-save** environments, so rust-cache's bare `rustc`
  (key) and bare `cargo metadata` (post hook) use the same toolchain as the compile — not
  the runner default. (Note rust-cache hashes _all_ installed rustup toolchains.)
- **Writer trust — `save-if` binds to the checkout, not just the event.** A trusted
  `workflow_dispatch`/deployer push can still supply an arbitrary cross-repo `app-ref`, so
  saving is restricted to **same-repository trusted-trigger builds**; **cross-repository
  builds are RESTORE-ONLY** (`save-if` false) unless trusted lineage for the app SHA is
  independently established (a follow-up). Fork-PR contexts never write.
- **No save on a broken post hook.** rust-cache swallows a `cargo metadata --all-features`
  failure, prunes on an empty package list, and still saves an empty entry that (being
  immutable, and an exact hit not re-saving) can never repair itself. The workflow
  therefore **verifies the identical metadata command succeeds and precludes the save when
  it cannot** (e.g. via `save-if` / `cache-on-failure: false` and a pre-save metadata
  check). Save is also conditional: an exact hit does not save; concurrent misses are
  first-writer-wins.

### 3.6 Native build semantics + JSON artifact discovery (no forced target)

Forcing `--target <host>` is **not** used (explicit-target mode changes host build-script
/ proc-macro `RUSTFLAGS` application and artifact layout, breaking `cache: false` parity).
The build keeps native semantics; the executable is located from Cargo's
`--message-format=json` stream — the single `compiler-artifact` matching the resolved
`package_id`, requested binary `target.name`, `target.kind` containing `bin`, non-null
`executable`, after `build-finished` — canonicalized beneath the owned target and checked
for execute permission. An incompatible configured target (`.cargo/config.toml` /
`CARGO_BUILD_TARGET`) fails closed. `cache: true` requires **Cargo ≥ 1.91** (for
`build.build-dir`), failing closed otherwise. Build-script/proc-macro/`RUSTFLAGS` tests
lock the native semantics.

### 3.7 Provenance — consistency check across every consumer

`app-cli-meta.json` gains `app-repo`, `source-revision`, `app-cli-package`, `app-cli-bin`,
and a **schema version**, all derived from the **actual checkout** (not trusted caller
strings), under a **clean/unchanged checkout** requirement. A **single strict schema and
one shared validator** are defined; every consumer — `deploy-fastly`, staging
`healthcheck-fastly`, `rollback-fastly`, `config-push-fastly` — validates repository,
revision, package, binary, and workspace identity **before any downloaded CLI is executed
at all** (including `--help`) and before provider credentials are exposed. Consumers
without a checkout receive explicit expected-repository/revision inputs;
`healthcheck-fastly`/`rollback-fastly` gain those inputs. The **lost-version recovery
flow** (which today downloads and runs the CLI with `FASTLY_API_TOKEN`) is **routed
through the same validator first**. This is a **consistency check, not a cryptographic
boundary** (an app-controlled artifact can self-assert). Exact field names, value
normalization, and the unknown-schema policy are pinned in the implementation plan (§9).

### 3.8 Runner policy

For v1, `cache: true` runs on a **single hard-coded GitHub-hosted Linux x64 image**
(exact `ubuntu-<version>` label, not caller-controlled), so `ImageOS`/`ImageVersion` are
meaningful and enforceable and the native GNU/Linux artifact matches the deploy baseline
(build/deploy OS baselines must be compatible). Persistent self-hosted runners are **not**
supported for caching (the workflow cannot erase credentials/processes left by prior jobs;
a cache namespace does not remediate them). A separate self-hosted trust mode with an
immutable image identity is future work.

### 3.9 Security — reader/writer trust and cache ownership

- **Cache ownership is the CALLER/deployer repository.** In the separate-repo topology the
  cache belongs to the **deployer** repo, not the checked-out app repo — so a **public
  deployer** can expose cached **private app dependencies** to **its own** fork PRs even if
  the app repo runs no untrusted workflows. The reader-trust precondition is therefore
  stated against the **deployer** repo's PR-trust posture.
- **Reader trust.** A dependency-reuse cache stores dependency **source** (`.crate`
  archives, `git/db`) and compiled `target/`, and rust-cache resolves the **whole
  workspace**, so private dependencies **anywhere** in it are exposed to cache readers.
  Such a deployer MUST NOT enable `cache: true` (or must use ACL storage).
- **Writer trust.** §3.5 `save-if`: same-repo trusted triggers only; cross-repo
  restore-only.
- **Build-environment identity.** The fixed hosted image (§3.8) plus `ImageOS`/
  `ImageVersion` in the `prefix-key` keep the keyed environment meaningful; rust-cache
  keys `CC`/`CFLAGS`/`RUST*` but not libc/linker/image, which the fixed image covers.

## 4. Testing

- **Workflow contract:** `workflow_call` inputs/outputs/secrets; `$/.github/actions/...`
  resolution + pin/actionlint carve-outs; single-CLI outputs and a matrix case proving no
  wrong-artifact handoff via shared outputs; cross-repo (`app-repository`/`app-ref` + PAT)
  incl. private fail-closed; `persist-credentials: false` Git config.
- **Runner:** the job runs only on the fixed hosted image; missing image variables fail
  closed.
- **Toolchain binding:** a non-default app toolchain drives restore/compile/**post-save**.
- **Post-hook safety:** an induced `cargo metadata` failure produces **no save** (no empty
  cache published).
- **Artifact discovery/Cargo:** JSON selection; native build-script/proc-macro/`RUSTFLAGS`
  parity vs `cache: false`; hostile/extensionless `.cargo/config` and `CARGO_BUILD_TARGET`
  fail closed; Cargo < 1.91 fails closed; external path deps.
- **Provenance:** every consumer (deploy/healthcheck/rollback/config-push, **and recovery**)
  rejects mismatched repo/revision/package/bin/workspace before any CLI execution.
- **Cache behavior:** two-run restore, network blocked — a known **registry** dependency is
  `fresh: true`, workspace crates rebuild, artifact differs between revisions; `save-if`
  writes only on a same-repo trusted trigger; cross-repo build does not save.

## 5. Docs and migration

- Scope the parent's exact-key / target-only caching language to **`deploy-fastly.cache`**;
  define **`build-app-cli.cache`** as a separate **rolling** cache. `deploy-fastly`
  unchanged.
- Correct the guide/adoption-guide claims that **consumers own checkout, runner, and
  timeout, and the actions never call `checkout`** — false for the reusable workflow, which
  owns its job, checks out the app, and sets the (fixed) runner and timeout. Document the
  two-job topology and the §3.9 reader/writer preconditions (naming the **deployer** repo
  as cache owner) prominently.
- Pin gate / `zizmor` / actionlint: rust-cache SHA + non-SHA regression; the narrow `$/`
  carve-outs.
- Public-surface golden: the reusable-workflow inputs/outputs.

## 6. Default and effect

**Off by default.** With `cache: true`, a warm build restores compiled **dependencies**
(the bulk of the ~10 min); app + workspace-local crates still recompile. rust-cache's
save-time cleaning prunes the newly saved entry; it saves only on a miss under a same-repo
trusted trigger.

## 7. Out of scope / future

- `cli-profile` (a faster CLI build profile — a separate, cache-free win).
- Private-registry/git dependency **authentication** for the cached build.
- A self-hosted trust mode with immutable image identity; trusted-lineage cross-repo
  **saves**.
- Caching for non-Fastly adapters.

## 8. History (for the record)

v2–v5 (in-place, token in the job) → v6 (composite can't enforce isolation) → v6.1
(reusable workflow — right unit) → v6.2 (concrete workflow/API + lifecycle) → **v6.3**
(fixed runner; writer-trust bound to the checkout with cross-repo restore-only;
`RUSTUP_TOOLCHAIN` restored; no-save-on-broken-post-hook; single-CLI/matrix outputs;
normative `persist-credentials` + stored-PAT private path; provenance across every consumer
incl. recovery; cache owned by the deployer repo).

## 9. Deferred to the implementation plan

Contract-level precision that the plan pins as concrete, testable steps (not open design
questions): the exact `prepare`/`compile` internal interface signatures; the provenance
schema literal, field normalization, and unknown-schema policy; the exact `save-if`
predicate expression and the pre-save metadata-success check; and the precise
`app-repo-id`/`workspace-id`/`hosted-image-id` canonicalization and length bounds.
