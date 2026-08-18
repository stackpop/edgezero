# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.2, reusable build workflow (implementation-ready)

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

A composite action cannot enforce a credential-free job (the caller adds later steps
to the same job; rust-cache's `post:` hook saves at job cleanup after them). Caching is
therefore delivered by a **reusable workflow** (`on: workflow_call`) that owns the
build job end-to-end; a caller invokes it as a job and **cannot inject steps into it**.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
             (owns a provider-credential-free job; caches; uploads the CLI artifact)
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

**Credential wording (precise).** A reusable workflow still automatically receives a
permissions-limited `GITHUB_TOKEN`, and `actions/checkout` consumes it even with
`persist-credentials: false`. The guarantee is therefore **"no provider credential or
OIDC capability is exposed to the build/cache steps"** — not "no token exists." The
workflow declares `permissions: { contents: read }`, **no `id-token`**, requests **no
provider secrets**, and passes none to the build/cache steps.

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`:

| Input               | Req.  | Meaning                                                                                    |
| ------------------- | ----- | ------------------------------------------------------------------------------------------ |
| `app-repository`    | no    | `owner/repo` of the application (default: the caller repository).                          |
| `app-ref`           | yes\* | Full commit SHA to check out (required when `app-repository` is set).                      |
| `working-directory` | no    | App dir under the checkout (default `.`).                                                  |
| `app-cli-package`   | yes   | Cargo package to build.                                                                    |
| `app-cli-bin`       | no    | Binary name (default: the package name).                                                   |
| `rust-toolchain`    | no    | Explicit toolchain or `auto` (default `auto`).                                             |
| `app-cli-artifact`  | no    | Uploaded artifact name (default `edgezero-cli`); must be unique per build job in a matrix. |
| `cache`             | no    | `true`\|`false` (default `false`).                                                         |
| `cache-key-suffix`  | no    | Namespaces / rotates the cache (§3.5). Not an ACL.                                         |
| `runs-on`           | no    | Constrained to a GitHub-hosted Linux x64 image label (§3.8).                               |
| `timeout-minutes`   | no    | Job timeout (default set by the workflow).                                                 |

`secrets`: `app-checkout-token` (optional) — a **narrowly scoped** token for checking
out a **private** application repository in the separate-repo layout (a called
workflow's checkout otherwise defaults to the **caller** repository). If a private
`app-repository` is set without it, the workflow fails closed. This preserves the
adoption guide's private cross-repo layout; it is the **only** secret the build job
accepts, and it is a checkout credential, not a provider credential.

`outputs`: mirror the composite's four — `app-cli-artifact`, `app-cli-bin`,
`app-cli-package`, `app-cli-version` — so the deploy job consumes them unchanged.

### 3.2 Referencing the workflow's own composite

A remote reusable workflow must **not** use `./.github/actions/build-app-cli` (that
resolves inside the **caller's** checkout). It uses GitHub's
**`$/.github/actions/build-app-cli`** syntax, which resolves against the **called
workflow's repository and running commit** — the only version-aligned reference.

Because the repo's pin gate rejects `$/…` as unpinned and actionlint 1.7.7 does not yet
parse it, the spec requires: a **narrow pin-gate exemption** for
`$/.github/actions/...` (a self-repository, commit-aligned local reference — inherently
pinned) and a **targeted actionlint suppression**, both scoped to that exact prefix and
removed when upstream actionlint supports `$/`.

### 3.3 Internal lifecycle boundary

The current `build-app-cli` composite does resolve/install → reset target → compile →
stage → upload as **one** invocation, but rust-cache must run **between resolution and
compilation**. Split the internals into a composable boundary the reusable workflow
orchestrates:

```
resolve/install (toolchain, host triple, workspace + app identity, target-dir contract)
  → validate + reset the stable target (§3.4)
  → Swatinem/rust-cache restore (§3.5)
  → compile + stage + upload   (single, designated upload owner)
```

The **public `build-app-cli` composite is unchanged** (the uncached, in-job all-in-one)
and gains **no `cache` input** — a direct `build-app-cli(cache: true)` surface is not
supported (it would reintroduce the composite-can't-enforce-isolation problem). The
**reusable workflow** consumes `cache` / `cache-key-suffix` and drives the split steps
itself.

### 3.4 Stable target: reset-before-every-restore, identity-scoped

- The cached `CARGO_TARGET_DIR` is a **stable path outside** the per-invocation
  workspace (so composite cleanup cannot delete it before rust-cache's job-end save).
- rust-cache does **not** clear the target on a miss and only lightly cleans on a
  partial restore, so a remote miss (changed image/suffix/app) could otherwise reuse
  **stale local** artifacts. The workflow therefore **resets the guarded stable target
  before every restore, and never after** — restore repopulates it deterministically.
- The **target path and the `prefix-key` both include canonical application-repository
  and workspace identity** (in addition to image/env and suffix identity, §3.5), so two
  applications or two workspaces driven from one deployer repository never collide.

### 3.5 rust-cache pin and configuration

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

The pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`** (a
regression test rejects **every** non-SHA rust-cache ref, not only `@v2`), alongside
the repo's general version-tag policy for other actions.

Configuration and behavior notes:

- `cache-bin: false`; `cache-workspace-crates: false` — the latter excludes crates by
  **path containment** under the workspace, not Cargo's `workspace_members` set.
- `prefix-key` **replaces** rust-cache's default with
  `edgezero-build-app-cli-v1-<app-repo-id>-<workspace-id>-<hosted-image-id>-<suffix-hash>`.
- `workspaces:` maps the **canonical Cargo workspace** to the stable target given as a
  path **relative to that workspace** (rust-cache does not interpret an absolute RHS as
  expected).
- `save-if:` gates writes to **trusted triggers only** (e.g. `push`/`workflow_dispatch`
  on a trusted ref), never fork-PR contexts — the **writer-trust** control.
- rust-cache hashes **every installed rustup toolchain**, not only the
  `RUSTUP_TOOLCHAIN`-selected one; and its post hook runs `cargo metadata --all-features`
  over the **whole resolved workspace** (not only the CLI package), swallowing failures
  (which can prune before save). Consequences: private dependencies **anywhere in the
  resolved workspace** trigger the reader-trust concern (§3.9); post-metadata
  failure/offline behavior is tested.
- **Save is conditional:** an exact hit performs no save; concurrent misses are
  first-writer-wins (Actions caches are immutable).

### 3.6 Native build semantics + JSON artifact discovery (no forced target)

Forcing `--target <host>` is **not** used: Cargo's explicit-target mode differs from
native even when the triple equals the host (target `RUSTFLAGS` stop applying to host
build scripts/proc-macros; artifacts are laid out differently), which would break the
"`cache: false` behaves exactly as today" guarantee. Instead:

- the build keeps **native semantics** (as today), and
- the executable is located from Cargo's `--message-format=json` stream — the **single**
  `compiler-artifact` whose `package_id` matches the resolved package, whose
  `target.name` is the requested binary, whose `target.kind` contains `bin`, and whose
  `executable` is non-null, after a successful `build-finished`; the path is canonicalized
  beneath the owned target and checked for execute permission.
- If repository configuration (`.cargo/config.toml` / `CARGO_BUILD_TARGET`) selects a
  target incompatible with the native host build, the workflow **fails closed** with a
  clear message rather than silently caching a cross-target layout.
- `cache: true` **requires Cargo ≥ 1.91** (for the `build.build-dir` control that keeps
  intermediates inside the owned target); it fails closed on older Cargo.

Tests cover build scripts, proc-macros, and `RUSTFLAGS` to lock the native semantics.

### 3.7 Artifact provenance (a consistency check, not a trust boundary)

- `app-cli-meta.json` gains `app-repo`, `source-revision`, and a **schema version**,
  derived from the **actual checkout** (not trusted caller strings).
- Every consumer — `deploy-fastly`, staging healthcheck, rollback, config push —
  validates `app-repo` + `source-revision` against its own checkout (or against explicit
  expected-repository/revision inputs when the consumer has no checkout) **before any
  downloaded CLI is executed at all**, including `--help`, and before provider
  credentials are exposed.
- This is a **consistency check**, not a cryptographic trust boundary: an app-controlled
  artifact can self-assert metadata. It catches wrong-repo/wrong-revision handoffs, not a
  malicious build.

### 3.8 Runner policy

For v1, `cache: true` runs **only** on a GitHub-hosted Linux x64 image (or a dedicated
**ephemeral one-job** runner). Persistent self-hosted runners are **not** supported for
caching: the reusable workflow prevents caller-added steps, but it cannot erase
credentials or processes left by previous jobs on a persistent runner, and a cache
namespace does not remediate residual credentials. `runs-on` is constrained accordingly.

### 3.9 Security — reader and writer trust, environment identity

- **Reader trust (enabling precondition).** A dependency-reuse cache necessarily stores
  dependency **source** (`registry/cache` `.crate` archives, `git/db` repos) and
  compiled `target/`, readable across refs including fork PRs; a key is a **namespace,
  not an ACL**. Because rust-cache resolves the **whole workspace**, private dependencies
  **anywhere** in it are exposed. A repo running untrusted PR workflows with any private
  workspace dependency MUST NOT enable `cache: true` (or must use ACL storage).
- **Writer trust.** `save-if` restricts cache writes to trusted triggers (§3.5).
- **Build-environment identity.** rust-cache keys `CC`/`CFLAGS`/`CXX`/`CMAKE`/`CARGO`
  and `RUST*`, but not compiler/linker/libc/header versions or the runner image (and
  hosted images update over time). The `prefix-key` includes a **hosted-image identity**
  (`ImageOS`/`ImageVersion`); the fixed hosted-x64 runner policy (§3.8) keeps it
  meaningful. Missing image variables fail closed.

## 4. Testing

- **Workflow contract:** `workflow_call` inputs/outputs; the `$/.github/actions/...`
  reference resolves and is exempted in the pin gate + actionlint; unique artifact names
  across a matrix; cross-repository (`app-repository`/`app-ref` + checkout secret)
  including the private fail-closed.
- **Lifecycle/target:** stale pre-existing stable target is reset before restore; the
  target survives past composite cleanup for the post-save; distinct app/workspace
  identities do not collide.
- **Toolchain binding:** an app toolchain ≠ runner default drives restore/compile/save.
- **Artifact discovery/Cargo:** JSON `compiler-artifact` selection; native build-script/
  proc-macro/`RUSTFLAGS` semantics unchanged vs `cache: false`; hostile `.cargo/config`
  (incl. extensionless) and `CARGO_BUILD_TARGET` fail closed; Cargo < 1.91 fails closed;
  external path dependencies.
- **Provenance:** every consumer (deploy-fastly, healthcheck, rollback, config-push)
  rejects a mismatched `app-repo`/`source-revision` **before** any CLI execution.
- **Cache behavior:** two-run restore with **network blocked** — a known **registry**
  dependency reports `fresh: true`, workspace-local crates rebuild, the artifact differs
  between revisions; `save-if` writes only on a trusted trigger; conditional (no-op) save
  on an exact hit; post-metadata failure/offline handling.

## 5. Docs and migration

- **Scope the parent's exact-key / target-only caching language to `deploy-fastly.cache`**
  and define `build-app-cli.cache` separately as a **rolling** dependency/build cache
  (rust-cache contents, rolling prefix, conditional job-end save, reader/writer trust).
  `deploy-fastly` is unchanged.
- The guide/adoption-guide statements that **consumers own checkout, runner, and timeout,
  and the actions never call `checkout`** become **false for the reusable workflow** and
  must be corrected: the reusable workflow owns its job, checks out the app, and sets
  `runs-on`/`timeout-minutes` (within the v1 hosted-x64 policy). Document the two-job
  topology and the §3.9 reader/writer preconditions prominently.
- Pin gate / `zizmor` / actionlint: the rust-cache SHA + non-SHA regression, and the
  narrow `$/.github/actions/...` exemption/suppression.
- Public-surface golden: the reusable-workflow inputs/outputs.

## 6. Default and effect

**Off by default.** With `cache: true` in the two-job topology, a warm build restores
compiled **dependencies** (the bulk of the ~10 min); the app crate and workspace-local
crates still recompile. rust-cache's **save-time cleaning prunes the newly saved entry**;
it does not retroactively bound existing entries, and it saves only on a miss under a
trusted trigger.

## 7. Out of scope / future

- `cli-profile` (a faster CLI build profile — a separate, cache-free win).
- Private-registry/git dependency **authentication** for the cached build.
- Caching for non-Fastly adapters.

## 8. History (for the record)

- **v2–v5, in-place in the deploy job:** blocked by the token being in the job.
- **v6, composite + rust-cache:** correct pivot, but a composite cannot enforce a
  tokenless job.
- **v6.1, reusable workflow:** right unit to own the job; workflow/API + lifecycle
  details were still under-specified.
- **v6.2:** concrete workflow contract (inputs/outputs/secrets/cross-repo),
  `$/`-self-reference with pin/actionlint carve-outs, internal resolve→restore→compile
  boundary, reset-before-every-restore with app/workspace-scoped identity, native
  semantics + JSON discovery (no forced target), provenance across every consumer,
  hosted-x64 runner policy, and writer-trust `save-if`.
