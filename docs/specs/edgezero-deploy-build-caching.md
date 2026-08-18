# EdgeZero Deploy Actions — Build Caching Spec

**Status:** Design (proposed) — v6, caching via a credential-free build job

**Related:** `docs/specs/edgezero-deploy-github-action.md` (the deploy-actions spec),
`docs/specs/edgezero-deploy-action-implementation-plan.md`,
`docs/specs/edgezero-deploy-adoption-guide.md`, `docs/guide/deploy-github-actions.md`

## 1. Problem

`build-app-cli` compiles the application's own CLI package (native) into a throwaway
`mktemp` `CARGO_TARGET_DIR` with **no caching**, so every deploy recompiles the whole
dependency graph (~10 min for `stackpop/trusted-server-deployer`).

Five prior design rounds tried to cache this build _in place_ — inside the same job
as the token-bearing deploy, with in-process credential scrubbing. Every version
foundered on the same class of problem: making a cache **safe to write and correct**
while a provider token is in scope, and while re-deriving key/path/ownership
machinery by hand. The blockers were fundamentally about **the token being present in
the job**, not about caching itself.

## 2. Decision: the security boundary is job isolation

Run `build-app-cli` in a **dedicated job that holds no provider credentials**, and
enable caching only there. `build-app-cli` is already credential-free and already
publishes the compiled CLI as an artifact that a separate deploy job downloads — so
this is a topology the actions already support, not a new mechanism.

Making the credential-free property a **job boundary** (rather than in-process
scrubbing) dissolves the recurring blockers:

- **No save-before-token ordering problem** and **no post-hook timing problem** — the
  job never holds a token, so _when_ the cache saves is irrelevant to credential
  safety. This re-enables a cache that saves at job end.
- **No trusted-Cargo-boundary regression** — `cargo`/`cargo metadata` running with the
  GitHub file-command channels live has nothing to exfiltrate in a tokenless job.
- **No bespoke key/path/ownership design** — because the job is a normal Rust CI
  build, it can use a **maintained cache** (`Swatinem/rust-cache`, pinned) that
  already solves cache-path stability, key composition, cleaning, and restore.

What remains is one **irreducible precondition**, true of _any_ dependency-reuse
cache and now the whole security story (§4).

## 3. Design

### 3.1 Topology

Caching is a supported **workflow shape**, enforced by the actions:

```
job build   (no secrets):  checkout app → build-app-cli (cache: true) → upload artifact
job deploy  (secrets):     download artifact → deploy-fastly / lifecycle
```

The build job passes **no** `FASTLY_API_TOKEN`/provider secrets. The deploy job holds
the token and consumes the prebuilt CLI. The existing artifact handoff
(`app-cli-meta.json` + the binary) is unchanged.

### 3.2 Enforceable boundary (not just documented)

The boundary is verifiable, so the action enforces it: when `cache: true`,
`build-app-cli` **asserts that no known provider credential is present** in its
environment (the same alias set it already scrubs) and **fails closed** if one is —
turning "run this in a credential-free job" from a doc note into a checked
precondition. A deployer who puts the token in the build job gets a clear error, not
a silent secret-bearing cache write.

### 3.3 Caching mechanism

Within the credential-free build job the action runs **`Swatinem/rust-cache`, pinned
by full commit SHA** (a `# vX.Y.Z` trailer; the pin gate and `zizmor` accept a SHA;
add a regression test that a bare `@v2` is rejected). rust-cache owns key derivation
(lockfile + `rustc` + workspace + `prefix-key`), path stability, cache cleaning, and
restore/save; its **job-end post-hook save is now safe** because the job is
tokenless. `cache-key-suffix` maps to rust-cache's `prefix-key`.

To let rust-cache cache the build, two small action changes are required:

- **Stable target dir.** When `cache: true`, build into a stable, rust-cache-visible
  `CARGO_TARGET_DIR` instead of the current `mktemp` (rust-cache and Cargo's
  absolute-path dep-info both require a stable path). `cache: false` keeps today's
  `mktemp` + reset behavior exactly.
- **Deterministic artifact discovery.** Force `--target <resolved-host-triple>`,
  constrain `build.build-dir` to the owned target root, and read the executable path
  from Cargo's `--message-format=json` compiler-artifact message — so a repo
  `.cargo/config.toml` / `CARGO_BUILD_TARGET` cannot move the binary out from under a
  constructed path.

The resolver values that feed rust-cache (the stable target dir, the host triple, the
`prefix-key`) and the artifact path are derived **inside the tokenless build job**, so
passing them between steps is not a credential regression — there is no secret in the
job to leak through a resolver-output file. They are still validated before use
(canonical owned paths; a `prefix-key` bounded and hashed from `cache-key-suffix`).

### 3.4 Security — the one remaining precondition

A dependency-reuse cache **necessarily stores dependency source**: `registry/cache`
holds `.crate` archives (full crate source), `git/db` holds dependency repos, and
`target/` can hold build-script-generated source and embedded data. GitHub Actions
caches are **readable across refs, including fork PRs reading base/default-branch
caches**, and a cache key is a **namespace, not an ACL**. Therefore:

- **Reader trust is the enabling precondition.** A repository that runs untrusted PR
  workflows and has **private dependencies** MUST NOT enable `cache: true` unless it
  accepts that fork PRs can read that dependency source (or uses storage with real
  ACLs). This is stated plainly in the guide; it is not something the action can
  paper over.
- **Writer trust.** The cached `target/` is executable input to later builds in the
  same repo scope; cache-writing runs must not be triggered by untrusted refs.
- **No credential in the cache** is now structural: the job has no token (§3.2), so no
  provider secret can reach a cached path regardless of save timing.

**Build-environment homogeneity.** rust-cache keys on `Cargo.lock` + `rustc` +
workspace, not the runner image, libc, linker, C toolchain, or `CC`/`CFLAGS`/`CMAKE`
inputs. A build script can therefore produce native artifacts that Cargo considers
_Fresh_ on a differently-provisioned runner. Caching assumes **homogeneous runner
images** (the hosted-runner default); a fleet with heterogeneous images must namespace
by environment via `cache-key-suffix` (or not enable caching). This is documented as a
precondition, not enforced.

### 3.5 Inputs on `build-app-cli`

| Input              | Default | Meaning                                                                                                      |
| ------------------ | ------- | ------------------------------------------------------------------------------------------------------------ |
| `cache`            | `false` | `true` \| `false`. Enable rust-cache in a credential-free job (§3.2 enforces tokenlessness). Off by default. |
| `cache-key-suffix` | `""`    | Passed to rust-cache `prefix-key` to namespace / rotate a cache. A namespace, **not** an ACL (§3.4).         |

`cli-profile` and private-**registry/git** authentication remain out of scope for v1
(§6).

## 4. Testing

- **Boundary enforcement (`run.sh`):** `cache: true` with a provider alias present
  fails closed; absent, it proceeds; `cache: false` runs no cache step and keeps the
  `mktemp` + reset path.
- **Artifact discovery:** the binary is located via the JSON compiler-artifact
  message under the forced host target, not a constructed `target/release/<bin>` path;
  a repo `CARGO_BUILD_TARGET` override does not break discovery.
- **Pin regression:** `Swatinem/rust-cache@v2` (tag) fails the SHA requirement for
  that action; the pinned SHA passes.
- **Smoke (two runs, separate jobs/runs):** run 1 (credential-free build job)
  populates the cache and uploads the CLI artifact; run 2 at a new revision on the
  same lockfile restores it and, with **network blocked**, completes the build with
  known registry dependencies **Fresh** (not recompiled) — asserted by dependency
  artifact freshness, not timing. The two revisions have observably different CLI
  behavior and distinct artifact names.

## 5. Docs and migration

- `docs/guide/deploy-github-actions.md` + `docs/specs/edgezero-deploy-adoption-guide.md`:
  the **two-job topology** (§3.1) becomes the documented caching pattern; the current
  single-job same-repo example gains a cached-build-job variant. State the §3.4
  reader-trust precondition prominently.
- `docs/specs/edgezero-deploy-github-action.md`: note that caching is scoped to a
  credential-free build job, the enforced tokenless precondition, and the pinned
  rust-cache; the existing `deploy-fastly` exact-cache language is untouched
  (`deploy-fastly` is unchanged).
- `docs/specs/edgezero-deploy-action-implementation-plan.md`: the `build-app-cli`
  changes (enforcement, stable target dir, artifact discovery, rust-cache pin).
- Pin gate / `zizmor`: add the `Swatinem/rust-cache` SHA and the bare-tag regression
  test.
- Public-surface golden: the two new inputs.

## 6. Default, effect, out of scope

**Off by default.** In the two-job topology with `cache: true`, a warm build reuses
compiled dependencies (the bulk of the ~10 min); the app crate still recompiles per
revision. rust-cache's cleaning bounds cache size.

Out of scope for v1: `cli-profile` (a faster CLI build profile — a separate,
cache-free win worth revisiting); private-registry/git dependency authentication;
caching for non-Fastly adapters.

## 7. Why not the earlier approaches (for the record)

- **In-place in the deploy job (v2–v5):** required in-process credential scrubbing +
  save-before-token timing + a hand-built key/path/ownership design; five rounds of
  review showed this is a large, security-sensitive project that fights the token
  being in the job. Job isolation removes the token, not the caching.
- **In-house exact/rolling cache:** re-derives rust-cache; exact-lock keys cannot warm
  when manifests/features/profiles change without the lockfile, and rolling keys add
  ancestry GitHub does not guarantee.
- **rust-cache in the deploy job:** its post-hook save runs after the token deploy —
  unsafe. The pivot to a tokenless job is exactly what makes rust-cache usable.
