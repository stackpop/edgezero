# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6.7

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's CLI package (native) with **no caching**, so
every deploy recompiles the whole dependency graph (~10 min for
`stackpop/trusted-server-deployer`, which checks out a **separate** application repo and
builds its CLI). Caching must work for that **cross-repository deployer** topology.

## 2. Trust model

- **The build compiles trusted code** (the deploy target), so `build.rs` is already trusted;
  caching does not widen the boundary.
- **The runtime credential is trusted, not hidden.** The runner injects
  `ACTIONS_RUNTIME_TOKEN` into the job's Node actions regardless, so "skip rust-cache ⇒ no
  cache token" is false and removed. The boundary is: the cached workflow runs **only for
  authorized deployer events/refs (fail before compiling otherwise)**, and there the runtime
  credential is explicitly trusted.
- **The deployer owns and writes its repo-scoped cache**, so the cross-repository build warms.

A reusable workflow (`on: workflow_call`) owns the build job; a composite cannot keep later
caller steps (or the provider token) out of the job.

```
job build  = uses: stackpop/edgezero/.github/workflows/build-app-cli.yml@<ref>
job deploy = (provider secrets) download artifact → deploy-fastly / lifecycle
```

## 3. Design

### 3.1 Reusable-workflow public contract

`inputs`: `app-repository` (default: caller repo), `app-ref` (required full SHA when
`app-repository` set), `working-directory` (default `.`), **`workspace-root`** (static path
of the Cargo workspace root relative to the checkout, so `workspace-id` is known without a
runtime Cargo discovery), `app-cli-package` (required), `app-cli-bin` (default: package
name), `rust-toolchain` (default `auto`), `app-cli-artifact` (default `edgezero-cli`),
`cache` (default `false`), `cache-key-suffix`, `timeout-minutes`. **No `runs-on`** (fixed,
§3.8). **`persist-credentials: false` normative.**

**Normative job permissions.** The reusable workflow declares, at the job level,
**`permissions: { contents: read }`** — which forces `id-token` and every unspecified scope
to `none` regardless of what the caller granted (a caller granting `id-token: write` is
tested to still yield `none` in the build job). No OIDC.

`secrets`: `app-checkout-token` (optional narrow `contents: read` PAT for a private app;
private `app-repository` without it fails closed).

`outputs` (single CLI per call): `app-cli-artifact`, `app-cli-bin`, `app-cli-package`,
**`app-cli-version`**, `app-cli-source-revision`, `app-cli-workspace-id`, `app-cli-platform-id`.
`workspace-id` (from `app-repository` + `workspace-root`) and `platform-id` (the fixed image
label + forced CPU baseline, §3.8) are **static, deterministic** — a matrix consumer computes
its own from static inputs; it never reads them from the artifact. Runtime **producer ABI
metadata** (image version, glibc, ELF needs, §3.8) is separate and lives only in provenance.
**Matrix handoff never uses the shared workflow outputs** (GitHub keeps only the last leg's).

### 3.2 Self-composite reference

Via **`$/.github/actions/build-app-cli`** (self-repo, commit-aligned) with a **narrow
pin-gate exemption** for `$/.github/actions/...` and a **targeted actionlint suppression**.

### 3.3 Internal lifecycle boundary

```
authorize writer (§3.5 — fail before compile if unauthorized)
  → prepare (resolve/install toolchain; RUSTUP_TOOLCHAIN; host triple; identities;
             config/source policy §3.6; validated dirs)
  → cargo metadata --locked preflight (§3.5)
  → validate + reset the stable target (§3.4)
  → rust-cache restore  → verify Cargo.lock byte-identical (§3.5)
  → compile + stage + upload (single owner; MUST NOT reset the restored target)
```

The public `build-app-cli` composite keeps its uncached behavior but is **upgraded to emit
`workspace-id` + `platform-id`** so provenance is producer-agnostic; it gains no `cache` input.

### 3.4 Stable target

Stable path **outside** the per-invocation workspace; **reset before every restore, and
NEVER after**; path + key scoped by app-repo + workspace identity.

### 3.5 rust-cache pin, key, writer authorization, metadata guards

```yaml
uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
```

Pin gate requires a **40-hex SHA specifically for `Swatinem/rust-cache`**.

- `cache-bin: false`; `cache-workspace-crates: false`.
- **`prefix-key` = `edgezero-build-app-cli-v1-<tuple-hash>`**, `tuple-hash` = SHA-256 of a
  **canonical length-prefixed tuple** of: `app-repo-id`, `workspace-id`, `app-cli-package`,
  `app-cli-bin`, workspace-root `Cargo.toml` content hash (keys virtual-root
  `[profile]`/`[patch]`/`[workspace]` changes), **`platform-id`** (static), and the bounded
  `cache-key-suffix`. (Runtime ABI metadata is **not** in the key.)
- `workspaces:` maps the canonical workspace root (from `workspace-root`) to the stable
  target (relative path). **`RUSTUP_TOOLCHAIN`** exported for restore, compile, post-save.

**Writer authorization — fail before compiling.** Caching runs only when the event is
`push`, `workflow_dispatch`, or `schedule` on a **protected/trusted deployer ref** (a
`workflow_dispatch` on an arbitrary user-selected branch is **not** authorized — the ref must
be protected) **and** the checkout `HEAD` **==** the resolved app SHA. Anything else —
including a fork PR to the deployer — **does not run the cached workflow** (fails before
compilation). For authorized contexts the runtime credential is explicitly trusted.

**Metadata guards (bounding rust-cache's bare `cargo metadata`).** rust-cache runs bare
`cargo metadata` both before restore (membership) and in its post hook, swallowing failures.
The workflow therefore runs a **`cargo metadata --locked` preflight** that must succeed
(fail closed) and **verifies `Cargo.lock` is byte-identical after restore** (fail closed on
mutation). The parent's "all Cargo commands `--locked`" is scoped to our invocations; the
rust-cache bare-metadata exception affects only keying and is bounded by these guards.

**Empty-save residual (accepted):** stock rust-cache can publish an empty immutable entry on
a post-hook metadata failure that an exact hit never repairs — recover by rotating
`cache-key-suffix`; no "no empty save" guarantee. Owned/forked save = §7.

### 3.6 Cargo execution, target, config, and source closure

- **Preserve the current `working-directory` cwd** for the compile.
- **Isolated `CARGO_HOME`** (action-owned) so runner-global config (wrappers/mirrors) does
  not silently affect a cached build.
- **Fail closed (complete policy)** under `cache: true` if the **effective in-tree Cargo
  configuration** — the full precedence chain from cwd up to the workspace root, including
  **extensionless `config`, ancestor files, and recursive `include`s** — sets any
  codegen-affecting or source-affecting key rust-cache does not hash: a `rustc-wrapper`/
  `rustc-workspace-wrapper`, a `target.*.runner`/linker override, **source replacement/
  mirrors**, a `[patch]` outside the root manifest, or `include` directives. Plain registry/
  net config is fine.
- **Reject external path dependencies** (fail closed) whose canonical source is **outside the
  workspace root** — not only out-of-root workspace **members** — because rust-cache strips
  their path from manifests, omits source-less packages from the lock hash, and keeps them as
  deps. v1 requires all sources beneath the workspace root.
- **Implicit host-target only** (reject `build.target`/`CARGO_BUILD_TARGET`); **forced CPU
  baseline** (reject `RUSTFLAGS`/`target-cpu`/`target-feature`/`CFLAGS`/`CXXFLAGS` that deviate
  from the `x86-64` baseline); **confined build dirs** (`CARGO_TARGET_DIR` + `build.build-dir`
  to the owned root, Cargo ≥ 1.91).
- Executable = the single JSON `compiler-artifact` (matched `package_id`, binary `target.name`,
  `target.kind` ⊇ `bin`, non-null `executable`) after `build-finished`.

### 3.7 Provenance and public APIs

`app-cli-meta.json` (both producers) carries a **schema version** and `app-repo`,
`source-revision`, `app-cli-package`, `app-cli-bin`, **`app-cli-version`**, `workspace-id`,
`platform-id`, and **runtime ABI metadata** (image version, glibc version, the ELF's
`DT_NEEDED` list + required glibc symbol versions, §3.8) — from the actual checkout/build.

**`validate-app-cli-provenance` action** — inputs: **exactly one** artifact tar + the
**required** expected `app-repo`, `source-revision`, `app-cli-package`, `app-cli-bin`,
`app-cli-version`, `workspace-id`, `platform-id`. It **owns extraction**: a fresh
action-owned root, the tar's **unique expected members only** (the binary +
`app-cli-meta.json`), **no traversal/symlinks/special files**, then a strict-schema parse
(reject unknown schema), compares **every** expected field (never defaulting from the
artifact), and returns a **canonical regular executable path confined beneath the owned
root**. No CLI execution. Every consumer calls it before any downloaded CLI runs (incl.
`--help`) and before credentials.

**`active-version-fastly` action** — inputs: the tar + full expected identity + `service-id`

- token; runs `validate-app-cli-provenance`, then the validated CLI's `active-version`;
  output `version` (**empty on first-ever deploy = success**), non-zero on failure. Recovery
  calls this action (replacing the manual extract-and-run).

Consistency check, not a cryptographic boundary.

### 3.8 Runner and ABI

- Fixed **literal `ubuntu-24.04`**; verify the runner is **GitHub-hosted** (a hosted-only
  marker) and `ImageOS`/`ImageVersion` present, else fail closed.
- **`platform-id`** (static, in key/outputs) = canonical(image label, `x86-64` baseline).
- **Runtime ABI metadata** (provenance): `ImageVersion`, glibc version, and the produced
  ELF's `DT_NEEDED` + required glibc symbol versions.
- **Compatibility guarantee (narrowed, honest).** v1 requires the deploy/lifecycle consumer
  to run on the **same literal `ubuntu-24.04` image** — so DT_NEEDED libraries, glibc, and CPU
  are trivially satisfied; the consumer additionally re-checks the recorded ABI metadata as
  defense in depth and rejects a mismatch **before** the binary reaches a credentialed step.
  Cross-image / directional compatibility (with real ELF `DT_NEEDED` + symbol-version and CPU
  analysis for arbitrary consumers) is **§7 future work**.

### 3.9 Security — reader/writer trust and cache ownership

- **Cache ownership = the deployer repository.** Rejecting fork-PR calls to _this_ workflow
  does **not** stop **another** PR-triggered workflow in the deployer repo from restoring its
  base-branch caches (GitHub allows fork PRs to read base caches). Therefore **`cache: true`
  is a hard precondition** that the deployer either **prohibits untrusted-PR workflow
  execution** or all cached dependency source is **disclosure-safe** (no private deps). The
  action cannot see other workflows, so this is an operator precondition, stated plainly.
- **Reader trust.** The cache stores dependency source + `target/` for the whole workspace.
- **Writer trust.** §3.5 — protected-ref authorized event + `HEAD == resolved SHA`, before
  compilation; runtime credential explicitly trusted there.
- **Build-environment identity.** Literal image + `platform-id` (key) + runtime ABI metadata
  (provenance).

## 4. Testing

- **Contract/permissions:** `workflow_call` I/O/secrets incl. `app-cli-version`; a caller
  granting `id-token: write` still yields `none` in the build job; `$/…` carve-outs;
  matrix legs compute their own static `workspace-id`/`platform-id`; cross-repo (+PAT) **warm
  second run**; private fail-closed; `persist-credentials: false`.
- **Writer authorization:** unauthorized event, unprotected `workflow_dispatch` ref, or
  `HEAD != resolved SHA` **fails before compilation**; a fork-PR does not run the cached
  workflow.
- **Cache lifecycle/key:** **reset before restore; NO reset after**; **dependency artifacts
  survive post-save cleanup**; canonical tuple distinguishes `(foo-bar,baz)` from
  `(foo,bar-baz)` and busts on a root-`Cargo.toml` profile mutation with unchanged
  `Cargo.lock`.
- **Cargo/config/source:** member-local, extensionless, ancestor, or `include`d config
  setting a wrapper/runner/source-replacement, an external path dependency, a forced target,
  and a raised `target-cpu`/`target-feature` all **fail closed**; the `cargo metadata --locked`
  preflight failing and a post-restore `Cargo.lock` mutation both fail closed; cached-vs-direct
  member-local-rustflags is a fail-closed case; virtual root supported; Cargo < 1.91 fails
  closed.
- **Provenance/ABI/APIs:** `validate-app-cli-provenance` enforces one-tar / owned-root /
  no-traversal / unique-members / confined-executable and rejects wrong repo/revision/package/
  bin/version/workspace/platform and unknown schema (incl. same-repo/SHA wrong-package — no
  self-validation); `active-version-fastly` validates then runs, empty-version = success; a
  non-`ubuntu-24.04` consumer is rejected before any CLI execution; both producers emit
  `workspace-id`/`platform-id` (direct, `cache: false`, cached flows).

## 5. Docs and migration

- Scope the parent's exact-key/target-only caching language to **`deploy-fastly.cache`**;
  define **`build-app-cli.cache`** as a separate rolling, deployer-owned cache.
- Correct the guide/adoption-guide claims that consumers own checkout/runner/timeout and the
  actions never call `checkout`. Document the two-job cross-repository topology, the fixed
  `ubuntu-24.04`/same-image policy, the `validate-app-cli-provenance` + `active-version-fastly`
  actions and the composite's new provenance outputs, the protected-ref writer rule, and the
  §3.9 reader precondition (deployer prohibits untrusted-PR execution or has no private deps).
- Pin gate/`zizmor`/actionlint: rust-cache SHA + non-SHA regression; `$/` carve-outs.
- Public-surface golden: exact names/types/defaults for all new inputs/outputs (incl.
  `workspace-root`, `app-cli-version`, `platform-id`) and both new actions, defined **before**
  the golden test.

## 6. Default and effect

**Off by default.** With `cache: true` on an authorized deployer build, a warm run restores
compiled **dependencies** (the bulk of the ~10 min); app + workspace-local crates recompile.

## 7. Out of scope / future

- An owned/forked rust-cache **save** that refuses to save on its own metadata failure.
- **Cross-image / directional** ABI compatibility (real `DT_NEEDED`/symbol-version/CPU
  analysis) for non-same-image consumers; a self-hosted trust mode with an immutable image.
- `cli-profile`; private-registry/git dependency authentication; non-Fastly adapters.

## 8. History

… v6.4 (cross-repo deployer owns cache) → v6.5 (decisions) → v6.6 (runtime-token correction,
canonical-tuple key, composite provenance) → **v6.7**: split static `platform-id`/`workspace-id`
(+ a static `workspace-root` input) from runtime ABI provenance; complete fail-closed Cargo
config/source closure (isolated `CARGO_HOME`; reject wrappers/runners/source-replacement/
includes/ancestors/external-path-deps); `cargo metadata --locked` preflight + post-restore
`Cargo.lock` identity; normative job `permissions: { contents: read }` (no OIDC, tested);
narrow ABI to same literal `ubuntu-24.04` with recorded `DT_NEEDED`/glibc-symver; hardened
validator extraction contract; restore `app-cli-version`; protected-ref `workflow_dispatch`;
fix the reset-after test.

## 9. Deferred to the implementation plan (mechanics only)

Exact `prepare`/`compile` interface signatures; the provenance-schema and identity
canonicalization byte layouts and length bounds; and the exact writer-authorization /
config-closure predicate expressions. No open **design** decisions remain — string/interface
mechanics only.
