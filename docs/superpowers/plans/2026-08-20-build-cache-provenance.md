# Build Cache Provenance Implementation Plan (plan 3 of 5)

> **Execution:** Start after plan 2 is merged and its gate revision is active. Rotate the gate before
> landing any new protected test/helper, then keep implementation changes separate.

**Goal:** Make the reusable workflow the sole build-only producer of a deterministic protocol-1 app
CLI artifact and make every consumer independently validate exact caller and platform identity before
the binary can execute.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.29 Sections
3, 5.1, 5.3, 5.4, 6, 7, and 9.

## 1. Fixed actions and boundaries

- Pin upload to exact `actions/upload-artifact@v7.0.1` and download to exact
  `actions/download-artifact@v8.0.1`.
- The artifact payload is the validator-produced deterministic ustar file. Artifact service wrapping
  is transport only and is never parsed as provenance.
- The producer accepts no provider token/input and performs no provider mutation. The app checkout
  token is host-only and absent before any container starts.
- `expected.json` has one typed producer: baked `edgezero-provenance-validator write-expected`.
  Shell, jq, workflow expressions, and generic JSON writers do not encode either protocol document.
- Validation remains two invocations: trusted parse/extract, host output check, then hardened
  credential-free binary smoke.
- Every expected-write, provenance-package, provenance-validate, and binary-smoke target uses the
  design's exact `/usr/bin/env -S` placeholder protocol; no profile relies on Docker/image environment
  replacement alone.
- Plan 1 already implemented and baked the protocol owner, schema, canonical golden/malformed
  fixtures, and compiled fixture manifest into pinned image `D`. This plan integrates those bytes and
  may add only host-side workflow/action fixtures outside the canonical image context. It must not
  change the protocol crate, schema, baked fixtures, Dockerfile, image-context manifest, or `image.json`.
  A defect in any of those stops this plan and requires a new gate/source/image/pin cycle before work
  resumes.

## 2. Gate update and integration fixtures

- [ ] Hash and consume the already reviewed independent golden bytes for exact `expected.json`,
      `app-cli-meta.json`, deterministic ustar, extracted binary, and every schema/duplicate/JCS/path/
      size/ELF failure. Add host-side integration assertions that do not call the production encoder as
      their oracle and do not edit or duplicate the baked fixture authority.
- [ ] Add workflow/action structural tests for build-only permissions, exact-version action pins,
      literal `runs-on: ubuntu-24.04`, checkout credential removal, artifact-name uniqueness, no
      platform output, typed identity output, exact EdgeZero/app checkout separation, no app-relative
      local action resolution, exact mount split, cleanup on every exit, and no provider input or secret.
- [ ] Parse the reusable workflow and require its first executable step to be the fixed inline
      producer bootstrap. Bind the three runner fields, four `job.workflow_*` fields,
      `job.check_run_id`, and `app-ref` directly from their exact contexts or declared input; validate
      the exact runner/workflow/version/SHA/generation contract before any checkout, cache restore/
      save, artifact work, Docker, repository code, or credential use. Then require the EdgeZero
      action-source checkout, a second fixed inline checkout-verification step, and plan 2's three-
      field runner helper in that exact order before app checkout or other work. The verifier uses
      only inline commands and runner tools to inspect the checkout, executes or sources no file from
      it, and proves its exact root/repository/HEAD/clean/content contract before any local action or
      helper executes. Negative fixtures cover missing, reordered, late, skipped, continued, or
      failure-masked assertions, caller-env-derived, differently cased, malformed, and self-hosted
      values, local execution before verification, and non-cleanup always-run paths; a `runs-on` label
      or host-command check never substitutes for the context predicate.
- [ ] Add repository-wide structural tests that candidate revision `H` replaces
      `.github/actions/build-app-cli/action.yml` with the exact fail-closed retirement stub: its first
      executable step calls the runner helper without `if`, continuation, or failure masking; its next
      step always fails with migration guidance, and it has no producer input/output, build helper,
      cache, Docker, or artifact-upload path. Reject any
      executable repository workflow or runnable documentation producer call to that composite at
      `H`. The four gated prepublication documents remain explicitly non-runnable until plan 5 removes
      their legacy guidance at `R`; immutable older exact versions remain historical behavior, not a
      supported compatibility path in `H` or `V`.
- [ ] Add substitution fixtures: caller-supplied expected JSON, alternate schema/fixtures, candidate
      validator, shell-generated JSON, tar implementation, binary execution in parser container,
      writable parser input, and a second downloaded artifact must all fail.
- [ ] Land and activate this gate update using the plan-1 rotation and rollback procedure before
      modifying producer or consumer actions. Prove its changed paths are outside
      `image-context-paths.txt`; otherwise stop for a new image release.

## 3. Caller identity and authority export

- [ ] Consume plan 2's sole identity-calculation helper over a validated authority
      checkout; do not add another identity implementation. Require exact
      repository ID from authenticated GitHub API, full lowercase source SHA, workspace/cwd
      containment, tracked regular lockfile, and credential-free locked Cargo metadata agreement.
      Derive the package version from that same metadata result; no caller input may override it.
- [ ] Produce exact bounded package/bin names and the design's length-framed workspace hash. Reject
      non-UTF-8/escaping paths, symlink roots, malformed repository ID/SHA, duplicate outputs, and
      submodule mismatch.
- [ ] Invoke plan 2's trusted authority materializer: fetch exact app/submodule commits without a
      worktree, inspect all committed filter/config/submodule policy before it can execute, then
      checkout and materialize only through the absolute verified Git LFS binary. Remove credentials
      and their host channel before Copy A or any container exists. Produce tracked/submodule-only,
      `.git`-free, non-hardlinked Copy A. Prove forbidden filters/hooks/origins are never reached and
      the token never appears in Copy A, argv, environment, mounted data, logs, artifacts, or cache;
      reverify the read-only authority after compilation.

## 4. Expected identity and package

- [ ] Derive `PlatformIdentity` only from the invoking action revision's validated `image.json`.
      Reject tags, indexes, malformed records, mismatched protocol, and caller overrides.
- [ ] Invoke `write-expected` in the exact expected-write profile with only fresh `/work/expected` and
      tmpfs. Require canonical `/work`, the baked schema, create-new/no-replace publication, exactly one
      regular output, and complete host cleanup after abnormal exit. Invoke it through the design's
      fixed `/usr/bin/env -S` placeholder protocol and prove the target receives only `PATH`, `HOME`,
      and `TMPDIR` despite image/Docker poison variables.
- [ ] Select `cached-compile` only when validated boolean `cache:true`; select `uncached-compile` for
      the default `cache:false`. Compile the named app CLI in that selected profile and retain its
      exact host digest, size, mode, path, and single-link identity. Invoke `package` with either
      profile's output plus read-only expected input and a fresh `/work/packaged`; require exactly one
      deterministic `artifact.tar`. Use the exact three-variable environment launcher and test cache-on
      and cache-off producer/package paths independently.
- [ ] Run `package` twice over identical inputs and compare bytes. Validate archive member order,
      headers, modes, owner fields, checksums, padding, end blocks, size caps, metadata digest/ELF
      closure, and no extra filesystem entry.
- [ ] Validate `app-cli-artifact` as 1..128 ASCII bytes matching the design regex and unique in the
      run. Upload exactly the literal file path with `archive:true`, `compression-level:0`,
      `include-hidden-files:false`, `if-no-files-found:error`, `overwrite:false`, and
      `retention-days:1`; require nonempty artifact id/digest outputs and no wildcard/multiple path.

## 5. Reusable workflow interface

- [ ] Implement the exact design input set and defaults on a job with literal
      `runs-on: ubuntu-24.04`. Make the fixed inline producer bootstrap its first executable step.
      Before checkout, validate booleans, numeric `timeout-minutes` as an integer in 1..120, full
      lowercase `app-ref`, canonical positive `job.check_run_id`, exact context-derived
      `runner.environment:github-hosted`, `runner.os:Linux`, and `runner.arch:X64`, all four exact
      workflow identity properties, the canonical stable action version parsed from
      `job.workflow_ref`, and its resolved workflow SHA. The bootstrap has no `if` or
      `continue-on-error`, cannot mask failure, and success-gates every later non-cleanup step.
- [ ] Declare required string inputs for repository/ref/id/workspace/package/bin/artifact, optional
      string defaults for working directory/suffix/app env, boolean defaults for cache/disclosure, the
      required `rust-toolchain`, numeric timeout default, and the required checkout secret exactly as specified. Parse the
      received numeric timeout as an integer in 1..120 and reject every undeclared compatibility,
      platform, provider, ambient-environment, Cargo, and arbitrary-argument surface.
- [ ] In the checkout-independent bootstrap, require `job.workflow_repository` and
      `job.workflow_file_path` to identify the exact EdgeZero reusable workflow. Require
      `job.workflow_ref` to name canonical exact stable version `V` in published use or the
      distinct exact patch version `C` only in the disposable release fixture; `C` has no prerelease
      suffix and its GitHub Release must report `prerelease:true`. Require `job.workflow_sha` to be
      the ref's resolved full SHA and require canonical positive `job.check_run_id`. Only after that
      step passes, use exact `actions/checkout@v7.0.1` with persisted credentials disabled to check
      out only `stackpop/edgezero` at ref `job.workflow_sha` into a fixed private action-source root.
      As the next executable step, use only fixed inline workflow commands and runner tools to verify
      the real fixed root, exact repository identity and HEAD, clean tree, and absence of submodule,
      LFS, sparse, and untracked content. Execute no checked-out path before that verification. Then
      run plan 2's three-field runner helper and only afterward use plan 2's prevalidated object-first
      materializer for the app at full `app-ref` in a distinct authority root. Verify authority and
      Copy A export contracts, root separation, and that every local composite/helper path resolves
      beneath the verified EdgeZero root rather than app data.
- [ ] Wire plan 2's already gated cache primitive and exact restore/save action versions into this
      workflow. Add the first public cache-off, cold, warm, corrupt-restore, save-denied, and
      warning-only save hosted runs; preserve evidence for exactly one Cargo compile/build invocation
      after metadata preflight and for token absence.
- [ ] Replace the legacy `.github/actions/build-app-cli` producer with the gated retirement stub and
      migrate `.github/workflows/deploy-action.yml` plus its producer fixtures away from local
      composite calls. Exercise reusable-workflow behavior through the trusted non-public harness until
      plan 5 can run literal candidate `C`; no repository workflow may retain an alternate producer.
      Structural tests require literal `runs-on: ubuntu-24.04` on every step-based job containing a
      public EdgeZero action reference and no `runs-on` or `steps` on its job-level reusable-workflow
      caller.
- [ ] Expose only `artifact-name`, trusted `action-version`, resolved `action-revision`, and
      CallerExpectedIdentity fields. Never expose the host artifact
      path, container ref, platform digest/protocol, checkout token, cache path, or provider state.
- [ ] Test matrix legs with unique artifact names and independent identity comparison. Reject shared
      aggregate outputs, duplicate names in one run, empty names, and cross-leg identity reuse.

## 6. Consumer identity and action validation primitives

- [ ] Create `.github/actions/compute-app-cli-identity/action.yml` as the public identity action that
      wraps plan 2's sole identity-calculation helper. Create
      `.github/actions/validate-app-cli-provenance/action.yml` as the no-output public validation
      action. Implement the shared private artifact/expected/parse/extract/recheck/smoke helpers that
      later provider actions call. Implement only the expected-write, provenance-package,
      provenance-validate, and binary-smoke runner profiles here; plan 4 adds provider profiles.
- [ ] Make the first executable step of both public actions call plan 2's sole runner-eligibility
      helper with step-local values bound directly from `runner.environment`, `runner.os`, and
      `runner.arch`. Reject missing/malformed/self-hosted values before token handling, authority
      materialization, artifact download, or Docker execution; caller inputs and caller `env` cannot
      substitute these bindings. The helper receives no `job.workflow_*`, `job.check_run_id`, app,
      action, cache, or provider identity. Contract tests require this to remain the first executable
      internal action step with no `if`, continuation, or failure masking; every later non-cleanup
      internal step remains success-gated. Support and hosted fixtures require a caller's step-based
      job containing the action reference to declare literal `runs-on: ubuntu-24.04`; the composite
      cannot observe that label and never treats it as security evidence.
- [ ] Give `compute-app-cli-identity` exactly the required string inputs `action-version`,
      `app-repository`, `app-ref`, `app-repo-id`, `workspace-root`, `app-cli-package`, `app-cli-bin`,
      and `rust-toolchain`, optional string `working-directory` default `.`, and required sensitive
      string input `app-checkout-token`, supplied by the caller from a GitHub secret and masked before
      use. Require its runner action repository/ref to equal the supplied exact version. Materialize
      one action-private authority with plan 2's gated object-first helper, remove the credential
      channel, recompute identity, and clean the authority on every exit. Output exactly
      `app-repo-id`, `source-revision`, `app-cli-package`, `app-cli-bin`, and `workspace-id`; never
      output a path, descriptor, opaque authority handle, token, or platform field.
- [ ] In each consuming action, create a private action workspace, derive local PlatformIdentity,
      download exactly one named artifact with current repository/run id, `merge-multiple:false`,
      `skip-decompress:false`, and `digest-mismatch:error`, and reject an invalid name, token, pattern,
      artifact id, foreign run/repository selection, or second payload. Require the destination to
      contain exactly one regular single-link `artifact.tar` before validation.
- [ ] In each consuming action, invoke baked `write-expected` into a fresh expected directory from
      only the consumer-verified caller fields and locally derived platform fields before invoking
      `validate`. Reject caller-supplied JSON/platform values, stale or preexisting output, duplicate
      output, substitution, and cross-action expected-file reuse; clean the directory on every exit.
- [ ] Consume plan 2's sole canonical launcher for every profile in this plan. Add only closed
      operation variants and profile data; do not duplicate env-file serialization, placeholder argv,
      Docker create/start/attach, timeout, or cleanup logic.
- [ ] In the consumer job, invoke `compute-app-cli-identity` once with the exact source inputs and
      checkout-token secret, then compare every typed output with the reusable-workflow outputs before
      invoking any provider action. Pass those verified caller fields to later actions; do not retain
      or pass an authority path because the identity action destroys its authority before returning.
      Source-free actions do not materialize source or mount Copy B; each compares artifact metadata
      with the supplied verified caller fields and recomputes PlatformIdentity locally, which no
      workflow output can substitute. Pass producer `action-version`; every action requires its own
      `github.action_repository/ref` to equal `stackpop/edgezero@<action-version>` before work.
- [ ] Run `validate` with read-only tar/expected/schema, fresh `/work/validated`, no network/credential/
      repository/Cargo/cache/binary execution, the exact `/usr/bin/env -S` closed-environment launch,
      and the exact design memory/pid/10-minute limits.
      Host-check exactly one
      mode-0755 regular single-link output with recorded digest and size.
- [ ] Start a new container for binary smoke with no network or credential and only the validated
      binary plus tmpfs. Use the fixed environment launcher to directly execute controlled-loader argv
      for dynamic binaries or the binary for static binaries, with only `PATH`, `HOME`, and `TMPDIR`
      at target-command entry. Recheck path/device/inode/digest/size/mode/link count before every later
      use.
- [ ] Ensure `if: always()` cleanup removes the private workspace and operation output parents without
      following links. Any cleanup or post-cleanup verification failure is fatal.
- [ ] Keep the extracted path, digest, size, mode, device, and inode in action-private step state only;
      expose no host binary path as a reusable-workflow or composite-action output. Make
      `validate-app-cli-provenance` validation-only with no outputs and prove its path is absent after
      successful cleanup.

## 7. Verification and merge

- [ ] Run all protocol crate tests, malformed/golden fixtures, producer/consumer contract tests,
      artifact upload/download tests, shellcheck, `scripts/run-actionlint.sh`, zizmor, and repository
      Rust/docs checks.
- [ ] Exercise corrupted transport, missing artifact, wrong artifact, replayed CallerExpectedIdentity,
      locally changed image pin, parser crash/timeout/SIGKILL, output replacement race, and smoke
      timeout. Every failure occurs before provider mutation.
- [ ] Merge through the one-entry queue and record the commit plus active gate revision for plan 4.
      Do not designate `P` until launcher/provider and adoption plans pass.

**Gate:** one deterministic archive and one typed expected identity cross the job boundary; the
consumer job recomputes caller identity without exporting authority state, and every consuming action
independently derives platform identity, checks both groups, and extracts without executing untrusted
bytes.
