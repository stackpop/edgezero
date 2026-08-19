# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.5, decisions made (plan-ready)

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) into a throwaway
`mktemp` `CARGO_TARGET_DIR` with **no caching**, so every deploy recompiles the whole
dependency graph (~10 min for `stackpop/trusted-server-deployer`, which checks out a
**separate** application repository and builds its CLI). Caching must work for that
**cross-repository deployer** topology and must not weaken the credential model.

## 2. Trust model and decision

- **The build compiles trusted code.** The deployer checks out the application it is
  about to deploy and the deploy step already runs that app's CLI with the provider
  token — so the app's `build.rs`/proc-macros are **already trusted** at build time.
  Caching does not widen the trust boundary.
- **The deployer owns and writes its cache.** GitHub caches are repository-scoped to the
  **deployer** (the repo whose workflow runs), which is therefore both writer and reader,
  so the cross-repository build **warms normally**.
- **A reusable workflow owns the credential-free build job** (`on: workflow_call`); a
  composite cannot keep later caller steps out of the job. The build job has **no provider
  credential and no OIDC** — only a `contents: read` `GITHUB_TOKEN`, the runtime cache
  token (only when authorized, §3.5), and a narrow app-checkout PAT for a private app.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
             (deployer-owned; no PROVIDER token; checks out the app; caches; uploads the CLI)
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`: `app-repository` (default: caller repo), `app-ref` (required full SHA when
`app-repository` is set), `working-directory` (default `.`), `app-cli-package` (required),
`app-cli-bin` (default: package name), `rust-toolchain` (default `auto`),
`app-cli-artifact` (default `edgezero-cli`), `cache` (default `false`), `cache-key-suffix`,
`timeout-minutes`.

- **No `runs-on` input** — the runner is fixed (§3.8).
- **`persist-credentials: false` is normative** on the app checkout (the narrow-PAT scope,
  §2, bounds the checkout action's post-step credential residual).

`secrets`: `app-checkout-token` (optional) — a stored fine-grained PAT (`contents: read`
on the app repo) for a **private** application repository; a private `app-repository`
without it fails closed. Never a provider credential.

`outputs` (single CLI per call): `app-cli-artifact`, `app-cli-bin`, `app-cli-package`,
`app-cli-version`, `app-cli-source-revision`, **`app-cli-workspace-id`**, **`app-cli-abi-id`**.
**Matrix handoff uses the unique per-leg `app-cli-artifact` names, never these shared
outputs** (GitHub keeps only the last leg's); each leg's expected identity is passed
per-leg (§3.7).

### 3.2 Referencing the workflow's own composite

Via **`$/.github/actions/build-app-cli`** (self-repo, commit-aligned). A **narrow pin-gate
exemption** for `$/.github/actions/...` and a **targeted actionlint suppression** are
required (scoped to that prefix; removed when upstream actionlint supports `$/`).

### 3.3 Internal lifecycle boundary

rust-cache runs **between resolution and compilation**:

```
prepare (authorize writer §3.5; resolve/install toolchain; export RUSTUP_TOOLCHAIN;
         host triple; canonical workspace + app identity; validated dirs + cache identities)
  → validate + reset the stable target (§3.4)
  → rust-cache restore (§3.5)   [only present when authorized]
  → compile + stage + upload    (single upload owner; MUST NOT reset the restored target)
```

`prepare` emits a private, validated handoff. The **public `build-app-cli` composite is
unchanged and gains no `cache` input**; the reusable workflow consumes `cache`/`cache-key-suffix`.

### 3.4 Stable target: reset-before-every-restore, identity-scoped

Stable path **outside** the per-invocation workspace (survives composite cleanup for the
job-end save); **reset before every restore, never after**; the target path and `prefix-key`
include canonical app-repository + workspace identity.

### 3.5 rust-cache pin, key, and writer authorization

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`**.

- `cache-bin: false`; `cache-workspace-crates: false` (rust-cache's lexical `startsWith`).
- `prefix-key` =
  `edgezero-build-app-cli-v1-<app-repo-id>-<workspace-id>-<app-cli-package>-<app-cli-bin>-<hosted-image-id>-<abi-id>-<suffix-hash>`
  (adds `app-cli-package`/`app-cli-bin` so two CLIs in one workspace never share an exact
  immutable key and strand a matrix leg).
- `workspaces:` maps the **canonical workspace root** to the stable target as a path
  **relative to that root** (§3.6).
- **`RUSTUP_TOOLCHAIN`** exported for restore, compile, and post-save.

**Writer authorization — authorize before compiling (not restore-only after).** Because
app `build.rs` can call the Actions cache API **directly** (bypassing rust-cache's
`save-if`) whenever the runtime cache token is present, a "restore-only via `save-if`"
rule is not an application-code boundary. Instead:

- The workflow **authorizes the writer before compiling**: the deployer event/ref is on a
  **trusted allowlist** (e.g. `push`/`workflow_dispatch` on the deployer's protected refs)
  **and** the checkout `HEAD` **==** the resolved app SHA.
- **Only an authorized build runs rust-cache at all** — so the runtime **cache token is
  present in the job only for an authorized SHA**. An unauthorized ref builds **with no
  cache step** (no cache token to bypass), still producing the artifact.
- Under §2 the app is trusted, so an authorized SHA writing the deployer's cache is the
  same trust as the deploy. Every compiled-and-cached SHA is therefore a deliberately
  authorized deployer-wide cache writer, checked **before** compilation.

**Empty-save residual (accepted, no false guarantee).** Stock rust-cache swallows a
post-hook `cargo metadata` failure and can still publish an **empty immutable** entry that
an exact hit never repairs. v1 **accepts this residual** — recover by rotating
`cache-key-suffix` — and makes **no** "no empty save" guarantee or test. An owned/forked
save phase that refuses to save on its own metadata failure is **§7 future work**. (Save
is otherwise conditional: exact hit → no save; first-writer-wins on concurrent misses.)

### 3.6 Cargo execution model, target, and config

- **One cwd = the canonical workspace root.** All cargo commands (metadata, build) and
  rust-cache run from the **workspace root** (where `Cargo.lock` lives); the package is
  selected with **`-p <app-cli-package>`**, not by `cd`-ing into a nested member. This gives
  a single, consistent `.cargo/config` chain and correct rust-cache member classification
  regardless of `working-directory`.
- **Implicit host-target only.** `cache: true` **fails closed** if a `build.target` /
  `CARGO_BUILD_TARGET` selects an explicit or non-host target (which would change
  build-script/proc-macro `RUSTFLAGS` and layout); native host mode is required.
- **Confined build dirs.** `CARGO_TARGET_DIR` and `build.build-dir` are **forced** to the
  owned stable target root (Cargo ≥ 1.91 for `build.build-dir`; older Cargo fails closed).
- **Reject under-hashable configs.** `cache: true` **fails closed** when the effective Cargo
  config chain that rust-cache does not fully key would change artifacts — **ancestor or
  extensionless `.cargo/config`, `include`d configs, source replacement/mirrors, local build
  wrappers, or external path dependencies outside the workspace**. A virtual-workspace root
  manifest is supported. v1 caches only the standard layout; the rest are a clear error, not
  a silent mis-cache.
- The executable is the single `--message-format=json` `compiler-artifact` matching
  `package_id`, binary `target.name`, `target.kind` ⊇ `bin`, non-null `executable`, after
  `build-finished`, canonicalized beneath the owned target with execute permission.

### 3.7 Provenance — required expected identity for every consumer

`app-cli-meta.json` carries a **schema version** and the full identity — `app-repo`,
`source-revision`, `app-cli-package`, `app-cli-bin`, `workspace-id`, `abi-id` — derived from
the **actual checkout** under a clean-tree requirement. A **single strict schema and one
shared, callable validation action** are defined. Every consumer supplies its **own expected
identity as REQUIRED inputs** (never defaulting to the artifact's own metadata, which would
let a wrong same-repo/SHA package self-validate) and calls the validator **before any
downloaded CLI executes** (including `--help`) and before provider credentials are exposed:

- consumers **with a checkout** (deploy-fastly, config-push) derive expected `app-repo`/
  `source-revision`/`workspace-id` from that checkout and take expected `app-cli-package`/
  `app-cli-bin`/`abi-id` as required inputs;
- **checkout-less** consumers (`healthcheck-fastly`, `rollback-fastly`) and the
  **lost-version recovery** flow take **all** expected-identity fields as required inputs.

**Recovery gets a typed surface.** A public **`active-version-fastly`** action (and the
shared validation action) replaces the guide's manual "extract the binary and run it with
`FASTLY_API_TOKEN`": recovery validates identity, then invokes the typed action — no
hand-run CLI. Matrix flows pass each leg's expected identity explicitly.

This is a **consistency check, not a cryptographic boundary** (a trusted app can self-assert
metadata); it catches wrong repo/revision/package/binary/workspace handoffs.

### 3.8 Runner policy and ABI

- `cache: true` runs on a **single literal GitHub-hosted Linux x64 image label** and the
  workflow **verifies the runner is GitHub-hosted** (a hosted-only environment marker, not
  the label alone) and that `ImageOS`/`ImageVersion` are present, failing closed otherwise.
- **`abi-id`** records the binary's ABI baseline — `ImageOS`/`ImageVersion`, glibc version,
  and a conservative CPU baseline (`x86-64-v2`) — and is part of the cache key and provenance.
- **Consumption compatibility.** v1 requires the deploy/lifecycle consumer to run on a
  **GitHub-hosted Linux x64 image of the same family**, verified against `abi-id` before the
  binary reaches a credential-bearing step. Non-hosted, musl, older-glibc, or lower-CPU
  consumers are **rejected** in v1 (a full DT_NEEDED/CPU compatibility contract for arbitrary
  consumers is §7 future work). Persistent self-hosted **build** runners are unsupported.

### 3.9 Security — reader/writer trust and cache ownership

- **Cache ownership = the deployer repository.** A public deployer running untrusted PR
  workflows could expose cached **private application dependency source** to **its own** fork
  PRs; the reader-trust precondition is stated against the **deployer** repo's PR posture.
- **Reader trust.** The cache stores dependency source (`.crate`, `git/db`) and `target/`;
  rust-cache resolves the whole workspace, so any private dependency in it is exposed to the
  deployer repo's cache readers.
- **Writer trust.** §3.5 — authorize-before-compile; the cache token exists only for an
  authorized SHA.
- **Build-environment identity.** The literal hosted image + `abi-id` cover libc/CPU/image,
  which rust-cache's key does not.

## 4. Testing

- **Workflow contract:** `workflow_call` inputs/outputs/secrets; `$/.github/actions/...` +
  carve-outs; single-CLI outputs + a matrix no-wrong-artifact case; cross-repository (+ PAT)
  **warm second run**; private fail-closed; `persist-credentials: false` Git config.
- **Writer authorization:** an unauthorized event/ref or `HEAD != resolved SHA` runs **no
  cache step** (no cache token in the job); an authorized SHA caches; fork-PR to the deployer
  never caches.
- **Cache lifecycle:** reset-before-restore, **no reset after**; target **survives past
  post**; two CLIs in one workspace get distinct keys; exact/partial/miss.
- **Cargo/target/config:** one-cwd config-chain equality; `-p` package selection from root;
  forced host-target (a `CARGO_BUILD_TARGET` override **fails closed**); confined build dirs;
  ancestor/extensionless/included config, source replacement, local wrapper, external path
  dep all **fail closed**; virtual root supported; Cargo < 1.91 fails closed; JSON selection;
  native build-script/proc-macro/`RUSTFLAGS` parity.
- **Provenance/ABI:** every consumer (deploy/healthcheck/rollback/config-push **and
  recovery via the typed action**) rejects a mismatched repo/revision/package/bin/workspace/
  abi **before** any CLI execution; a same-repo/SHA wrong-package artifact is rejected (no
  self-validation); an incompatible-ABI consumer is rejected.

## 5. Docs and migration

- Scope the parent's exact-key / target-only caching language to **`deploy-fastly.cache`**;
  define **`build-app-cli.cache`** as a separate **rolling, deployer-owned** cache.
  `deploy-fastly` unchanged.
- Correct the guide/adoption-guide claims that consumers own checkout/runner/timeout and that
  the actions never call `checkout` — false for the reusable workflow. Document the two-job
  cross-repository topology (worked `trusted-server-deployer` example), the fixed runner/ABI
  policy, the new **`active-version-fastly`** + validation actions, the writer-authorization
  rule, and the §3.9 reader/writer preconditions (deployer as cache owner).
- Pin gate / `zizmor` / actionlint: rust-cache SHA + non-SHA regression; the `$/` carve-outs.
- Public-surface golden: the reusable-workflow inputs/outputs, the new consumer
  expected-identity inputs, and the `active-version-fastly` action.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build, a warm run restores
compiled **dependencies** (the bulk of the ~10 min); app + workspace-local crates recompile.
rust-cache saves only on a miss for an authorized SHA.

## 7. Out of scope / future

- An owned/forked rust-cache **save** phase that refuses to save on its own post-hook
  metadata failure (v1 accepts the residual + suffix rotation).
- A full DT_NEEDED/CPU-feature compatibility contract for **arbitrary** (non-same-family)
  consumers; a self-hosted trust mode with an immutable image identity.
- `cli-profile` (a faster CLI build profile — a separate, cache-free win).
- Private-registry/git dependency **authentication**; caching for non-Fastly adapters.

## 8. History (for the record)

v2–v5 (in-place, token in job) → v6 (composite can't isolate) → v6.1 (reusable workflow) →
v6.2 (workflow/API) → v6.3 (fixed runner, RUSTUP_TOOLCHAIN) → v6.4 (cross-repository deployer
owns+writes cache; trusted-build reframe) → **v6.5** (decisions made: accept the empty-save
residual, no false guarantee; one workspace-root cwd with `-p`; authorize-writer-before-compile
so the cache token exists only for an authorized SHA; required per-consumer expected identity +
typed `active-version-fastly`/validation actions; package/bin in the key; implicit host-target +
confined build dirs + reject under-hashable configs; literal hosted image + `abi-id` +
same-family consumption).

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile` interface signatures; the provenance schema literal and field
normalization; the exact writer-authorization predicate expression; and the
`app-repo-id`/`workspace-id`/`hosted-image-id`/`abi-id`/`suffix-hash` canonicalization and
length bounds. No open **design** decisions remain here — these are string/interface
mechanics.
