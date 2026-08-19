# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.6

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) with **no caching**, so
every deploy recompiles the whole dependency graph (~10 min for
`stackpop/trusted-server-deployer`, which checks out a **separate** application repo and
builds its CLI). Caching must work for that **cross-repository deployer** topology.

## 2. Trust model

- **The build compiles trusted code.** The deployer builds the app it is about to deploy;
  the deploy step already runs that app's CLI with the provider token, so the app's
  `build.rs` is **already trusted** at build time. Caching does not widen the boundary.
- **The runtime credential is trusted, not hidden.** The runner injects
  `ACTIONS_RUNTIME_TOKEN` into the job's Node actions (artifact upload, checkout cleanup)
  regardless of whether the compile shell holds it, and a detached same-UID process could
  read it. "Skip rust-cache ⇒ no cache token" is therefore **false** and is removed. The
  real boundary is: **the cached workflow runs only for authorized deployer events/refs
  (fail before compiling otherwise), and for those the runtime credential is explicitly
  trusted** (the compiled code is the deploy target).
- **The deployer owns and writes its cache** (caches are repo-scoped to the deployer), so
  the cross-repository build warms normally.

A reusable workflow (`on: workflow_call`) owns the build job; a composite cannot keep the
provider token out of the job. The build job has **no provider credential and no OIDC**.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`: `app-repository` (default: caller repo), `app-ref` (required full SHA when
`app-repository` set), `working-directory` (default `.`), `app-cli-package` (required),
`app-cli-bin` (default: package name), `rust-toolchain` (default `auto`), `app-cli-artifact`
(default `edgezero-cli`), `cache` (default `false`), `cache-key-suffix`, `timeout-minutes`.
**No `runs-on`** (fixed, §3.8). **`persist-credentials: false` normative.**

`secrets`: `app-checkout-token` (optional narrow `contents: read` PAT for a private app;
private `app-repository` without it fails closed).

`outputs` (single CLI per call): `app-cli-artifact`, `app-cli-bin`, `app-cli-package`,
`app-cli-version`, `app-cli-source-revision`, `app-cli-workspace-id`, `app-cli-abi-id`.
`workspace-id` and `abi-id` are **deterministic pure functions of static inputs**
(`workspace-id` = canonicalization of `app-repository` + the workspace-root path relative
to the app Git root; `abi-id` = the fixed image + forced CPU baseline, §3.8), so a matrix
consumer computes its **own** expected values from its static matrix inputs — it never
reads them from the artifact. **Matrix handoff never uses the shared workflow outputs**
(GitHub keeps only the last leg's); each leg deploys the artifact it named.

### 3.2 Self-composite reference

Via **`$/.github/actions/build-app-cli`** (self-repo, commit-aligned). Requires a **narrow
pin-gate exemption** for `$/.github/actions/...` and a **targeted actionlint suppression**
(removed when upstream actionlint supports `$/`).

### 3.3 Internal lifecycle boundary

```
authorize writer (§3.5 — fail before compile if unauthorized)
  → prepare (resolve/install toolchain; export RUSTUP_TOOLCHAIN; host triple; identities; validated dirs)
  → validate + reset the stable target (§3.4)
  → rust-cache restore (§3.5)
  → compile + stage + upload (single owner; MUST NOT reset the restored target)
```

The **public `build-app-cli` composite** keeps its all-in-one uncached behavior but is
**upgraded to also emit `workspace-id` and `abi-id`** (§3.7), so provenance is
producer-agnostic; it gains no `cache` input. The reusable workflow consumes `cache`.

### 3.4 Stable target

Stable path **outside** the per-invocation workspace (survives composite cleanup for the
post-save); **reset before every restore, never after**; path + key scoped by app-repo +
workspace identity.

### 3.5 rust-cache pin, key, and writer authorization

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`**.

- `cache-bin: false`; `cache-workspace-crates: false`.
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, where `tuple-hash` is the
  SHA-256 of a **canonical length-prefixed tuple** (not a hyphen-join, which collides:
  `(foo-bar,baz)` vs `(foo,bar-baz)`) of: `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, **workspace-root `Cargo.toml` content hash** (so a virtual-root
  `[profile]`/`[patch]`/`[workspace]` change — which rust-cache does not key, since a virtual
  root is not a package — busts the key), `hosted-image-id`, `abi-id`, and the
  bounded `cache-key-suffix`.
- `workspaces:` maps the canonical workspace root to the stable target (relative path).
- **`RUSTUP_TOOLCHAIN`** exported for restore, compile, and post-save.

**Writer authorization — fail before compiling.** Saving/caching runs **only** for an
authorized deployer context, decided **before compilation**: the event is `push` to a
protected deployer ref, `workflow_dispatch`, or `schedule` (the daily-deploy adopter) on a
trusted ref, **and** the checkout `HEAD` **==** the resolved app SHA. Any other context —
including a fork PR to the deployer — **does not run the cached workflow at all** (it must
fail before compilation, so an unauthorized ref never compiles holding the runtime
credential). For authorized contexts the runtime credential is **explicitly trusted** (§2).
There is **no** "runs without a token" path and no such test.

**Empty-save residual (accepted).** Stock rust-cache swallows a post-hook `cargo metadata`
failure and can publish an empty immutable entry that an exact hit never repairs. v1
**accepts** this — recover by rotating `cache-key-suffix`; no "no empty save" guarantee. An
owned/forked save is §7. rust-cache's post hook runs bare `cargo metadata` (no `--locked`);
the parent's "all Cargo commands `--locked`" is scoped to **our** invocations, and this
keying-only exception is documented.

### 3.6 Cargo execution model, target, and config

- **Preserve the current cwd (`working-directory`)** for the compile — the workspace-root
  cwd of v6.5 would drop member-local `.cargo/config`, changing semantics. To keep
  compile-vs-rust-cache config chains identical, `cache: true` **rejects (fails closed) any
  member-local `.cargo/config` between `working-directory` and the workspace root**, so no
  member-local config exists to disagree about.
- **Reject out-of-root members.** `cache: true` fails closed if any workspace member's
  canonical manifest path is not beneath the canonical workspace root (rust-cache's lexical
  `startsWith` classification would otherwise cache such a member as a dependency).
- **Implicit host-target only.** Fail closed if `build.target`/`CARGO_BUILD_TARGET` selects
  an explicit/non-host target.
- **Forced CPU baseline.** Force the GNU x64 default (`x86-64`) and **reject `RUSTFLAGS`/
  `target-cpu` that raise it** (e.g. `target-cpu=native`), so `abi-id` describes the binary.
- **Confined build dirs.** `CARGO_TARGET_DIR` and `build.build-dir` forced to the owned
  root (Cargo ≥ 1.91 for `build.build-dir`; older fails closed).
- Executable = the single JSON `compiler-artifact` matching `package_id`, binary
  `target.name`, `target.kind` ⊇ `bin`, non-null `executable`, after `build-finished`,
  canonicalized beneath the owned target with execute permission.

### 3.7 Provenance and the public APIs

`app-cli-meta.json` (both producers — the composite and the workflow) carries a **schema
version** and `app-repo`, `source-revision`, `app-cli-package`, `app-cli-bin`,
`workspace-id`, `abi-id`, from the actual checkout under a clean-tree requirement.

**`validate-app-cli-provenance` action** — inputs: the downloaded artifact dir + the
**required** expected `app-repo`, `source-revision`, `app-cli-package`, `app-cli-bin`,
`workspace-id`, `abi-id`; behavior: strict-schema parse of `app-cli-meta.json`, reject
unknown schema, compare **every** field (never defaulting an expected value from the
artifact), fail closed on any mismatch; output: the validated CLI path. No CLI execution.

Every consumer calls it **before any downloaded CLI runs** (including `--help`) and before
credentials are exposed. Checkout-backed consumers derive expected `app-repo`/
`source-revision`/`workspace-id` from their checkout; checkout-less consumers
(`healthcheck-fastly`, `rollback-fastly`) and matrix legs supply the deterministic values
(§3.1) as required inputs; expected `abi-id` is the deterministic image/CPU value.

**`active-version-fastly` action** — inputs: the artifact + the full expected identity +
`service-id` + provider token; behavior: run `validate-app-cli-provenance` first, then
invoke the validated CLI's `active-version`; output: `version` (**empty on a first-ever
deploy = success**), non-zero on operational failure. Replaces the guide's manual
extract-and-run recovery; recovery calls this action.

This is a **consistency check, not a cryptographic boundary** (a trusted app can
self-assert), catching wrong repo/revision/package/binary/workspace/abi handoffs.

### 3.8 Runner and ABI

- Fixed **literal `ubuntu-24.04`** hosted image; the workflow **verifies the runner is
  GitHub-hosted** (a hosted-only environment marker, not the label alone) and that
  `ImageOS`/`ImageVersion` are present, failing closed otherwise.
- **`abi-id`** = canonical(`ImageOS`, `ImageVersion`, glibc version, forced `x86-64`
  baseline) — deterministic, in the key and provenance.
- **Directional compatibility predicate.** A consumer is compatible iff it is
  GitHub-hosted Linux x64 of the **same `ImageOS`**, its `ImageVersion` **≥** the producer's,
  its glibc **≥** the producer's, and CPU **≥** `x86-64` (always true) — checked against
  `abi-id` **before** the binary reaches a credentialed step. Non-hosted/musl/older-glibc
  consumers are rejected (positive and negative tests). A full DT_NEEDED/arbitrary-consumer
  contract is §7.

### 3.9 Security — reader/writer trust and cache ownership

- **Cache ownership = the deployer repository.** A public deployer running untrusted PR
  workflows could expose cached **private application dependency source** to its own fork
  PRs; the reader-trust precondition is stated against the **deployer** repo's PR posture.
- **Reader trust.** The cache stores dependency source (`.crate`, `git/db`) and `target/`;
  rust-cache resolves the whole workspace, so any private dependency in it is exposed.
- **Writer trust.** §3.5 — authorized deployer event/ref + `HEAD == resolved SHA`, decided
  before compilation; the runtime credential is explicitly trusted there.
- **Build-environment identity.** The literal image + `abi-id` cover libc/CPU/image.

## 4. Testing

- **Contract:** `workflow_call` I/O/secrets; `$/…` + carve-outs; single-CLI outputs; a
  matrix case where each leg computes its own deterministic `workspace-id`/`abi-id` and no
  wrong-artifact handoff occurs; cross-repo (+PAT) **warm second run**; private fail-closed;
  `persist-credentials: false`.
- **Writer authorization:** unauthorized event/ref or `HEAD != resolved SHA` **fails before
  compilation**; authorized SHA caches; fork-PR to the deployer does not run the cached
  workflow.
- **Cache lifecycle/key:** reset-before/after; target survives post; the canonical tuple
  hash distinguishes `(foo-bar,baz)` from `(foo,bar-baz)` and busts on a **root-`Cargo.toml`
  profile mutation with unchanged `Cargo.lock`**; two CLIs in one workspace get distinct
  keys.
- **Cargo/config:** member-local `.cargo/config`, out-of-root member, forced-target override,
  and raised `target-cpu` all **fail closed**; cached-vs-direct **parity with member-local
  rustflags is a fail-closed case**; virtual root supported (root manifest keyed); Cargo <
  1.91 fails closed; JSON selection; native build-script/proc-macro parity.
- **Provenance/ABI/APIs:** `validate-app-cli-provenance` rejects wrong repo/revision/package/
  bin/workspace/abi and unknown schema (incl. a same-repo/SHA wrong-package artifact — no
  self-validation); `active-version-fastly` validates then runs, empty-version = success;
  recovery routes through it; an incompatible-ABI consumer is rejected directionally
  (positive+negative); the direct composite and the reusable workflow both emit
  `workspace-id`/`abi-id` (direct, cache:false, and cached production flows).

## 5. Docs and migration

- Scope the parent's exact-key/target-only caching language to **`deploy-fastly.cache`**;
  define **`build-app-cli.cache`** as a separate **rolling, deployer-owned** cache.
- Correct the guide/adoption-guide claims that consumers own checkout/runner/timeout and the
  actions never call `checkout` — false for the reusable workflow. Document the two-job
  cross-repository topology (`trusted-server-deployer` example), the fixed runner/ABI policy,
  the new `validate-app-cli-provenance` + `active-version-fastly` actions and the upgraded
  composite provenance outputs, the writer-authorization rule, and the §3.9 preconditions
  (deployer as cache owner). Test direct, `cache: false`, and cached production flows.
- Pin gate/`zizmor`/actionlint: rust-cache SHA + non-SHA regression; the `$/` carve-outs.
- Public-surface golden: the reusable-workflow I/O, the composite's new provenance outputs,
  the consumer expected-identity inputs, and both new actions.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build, a warm run restores
compiled **dependencies** (the bulk of the ~10 min); app + workspace-local crates recompile.

## 7. Out of scope / future

- An owned/forked rust-cache **save** that refuses to save on its own metadata failure.
- A full DT_NEEDED/CPU-feature contract for arbitrary (non-same-family) consumers; a
  self-hosted trust mode with an immutable image identity.
- `cli-profile`; private-registry/git dependency authentication; non-Fastly adapters.

## 8. History

v2–v5 (token in job) → v6 (composite can't isolate) → v6.1 (reusable workflow) → v6.2/6.3
(workflow/API, fixed runner) → v6.4 (cross-repo deployer owns cache) → v6.5 (decisions) →
**v6.6**: drop the false "no cache token" boundary (fail-before-compile for unauthorized
refs; explicitly trust the runtime credential for the trusted deploy-target build);
deterministic `workspace-id`/`abi-id` for matrix consumers; canonical-tuple key hash (+
root-manifest hash for virtual roots); preserve `working-directory` cwd and **reject**
member-local/out-of-root/forced-target/raised-CPU cases; forced `x86-64` baseline with a
directional ABI predicate on a literal `ubuntu-24.04`; upgrade the composite to emit
provenance too; specify `validate-app-cli-provenance` and `active-version-fastly`; concrete
writer event list incl. `schedule`; `--locked` exception documented.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile` interface signatures; the provenance/ABI-id canonicalization byte
layouts and length bounds; and the exact writer-authorization predicate expression. No open
**design** decisions remain — these are string/interface mechanics.
