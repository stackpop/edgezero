# Build-Cache Container Implementation Plan (plan 1 of 5)

> **Execution:** Use `superpowers:subagent-driven-development` or
> `superpowers:executing-plans`. Follow the tasks in order and stop at every release checkpoint.

**Goal:** Publish and pin a public, leaf `linux/amd64` runtime image containing the exact EdgeZero
build/deploy toolchain and the trusted provenance validator required by build caching.

**Architecture:** A separately landed, immutable gate baseline `G` owns the validator, fixtures,
classifier, image verifier, publisher checker, exact Dockerfile, complete image-context manifest, and
organization-required workflow. Source revision `S` is an isolated canonical release request whose
repository image-context bytes remain identical to `G`. The publisher stages a fresh context solely from the
verified `G` checkout, captures and verifies immutable digest `D`, proves anonymous access, and opens
an idempotent, forward-only App-authored PR adding the image and release-evidence records. That pin
forms baseline `B`. Four remaining feature plans land on top. Their final passing action revision `P`
contains the unchanged `{D, S, protocol}` record and prepublication adoption documents with the gated
placeholder. After `P` qualifies, immutable stable release `V` is published at `P`; documentation
revision `R` replaces the placeholders, and consumers pin `V`.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.40.

**Tooling:** Rust, Docker BuildKit/buildx, GHCR, GitHub Actions, Bash 3.2, `jq`, `gh`, `actionlint`,
`shellcheck`, and `zizmor`.

## 1. Non-negotiable contracts

- Rust is the exact version in `.tool-versions` (`1.95.0` at plan time).
- Fastly CLI is the exact version/checksum in `.github/actions/deploy-fastly/versions.json`
  (`15.1.0` at plan time).
- sccache is exactly `0.10.0`, fetched as the upstream
  `sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz` client artifact and verified against upstream
  checksum `1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b`.
- The base is the official `rust:1.95.0-slim-bookworm` `linux/amd64` leaf manifest, resolved on
  2026-08-31 as `sha256:6f9e63259f12e1e599296f5ecfed2bae46de4af0ee0525dd8b89c046e236d5c5`
  and re-resolved immediately before the Dockerfile commit. No placeholder digest or checksum is
  committed.
- The final image is a leaf `linux/amd64` image manifest, not an OCI index.
- The final image contains an installed `wasm32-wasip1` target, not merely a rustc target-list entry.
- The project-owned validator, schema, and capability fixtures are baked and tested before push.
- Runtime is non-root uid/gid 1001 and works with a read-only root filesystem plus explicit tmpfs.
- Every target command is launched through baked GNU `/usr/bin/env` with the design's exact two-argument
  `-S` placeholder protocol. Image and Docker-created variables are absent from target-command entry,
  and no Docker or runtime argv element is constructed from an environment value.
- Every step-based repository-owned workflow job introduced by this plan declares literal
  `runs-on: ubuntu-24.04`. Its first executable step is a fixed inline bootstrap requiring
  context-derived `runner.environment:github-hosted`, `runner.os:Linux`, and `runner.arch:X64`
  before checkout, Docker, credential, or mutation work. The label selects the supported host image;
  runner labels and host command output are not substitutes for the context predicate. The bootstrap
  has no `if` or `continue-on-error`, cannot mask command failure, and gates every later protected
  operation. An `if: always()` cleanup needs no guard-success condition; any recovery path must
  be a fixed protocol-required recovery/reconciliation step and conjunctively require guard success
  and its protocol-specific transition marker. Every other non-cleanup always-run path fails.
- Every shell-backed first bootstrap/runner guard declares step-local `BASH_ENV: ""` and `ENV: ""`
  in workflow or composite metadata. Clearing either variable in the script body is too late because
  Bash may source the caller-selected file before executing that body.
- Every committed non-local external action and reusable workflow ref is a canonical exact stable
  `v<major>.<minor>.<patch>` release tag. Major/minor tags, prereleases, branches, commit SHAs, and
  floating refs fail. Docker image refs use immutable `sha256` digests. Local `./...` actions remain
  local refs. Third-party tag movement/deletion and future same-name branch ambiguity are explicitly
  accepted risks; EdgeZero `V` is an immutable release protected by no-bypass tag rules.
- In this plan, an action/workflow "pin" means that exact stable version tag. Full Git commit SHAs
  identify app source, resolved workflow execution, protected gate/release commits, and the
  organization required-workflow descriptor only; they are never written as non-local `uses:` refs.
- Bash scripts are Bash 3.2-compatible and `shellcheck -S warning` clean. CI helper scripts do not use
  Python. No AI bylines appear in commits or PRs.
- Publication never records a digest before the image passes authenticated verification and a clean,
  anonymous pull by digest.
- Candidate source never supplies the workflow, classifier, verifier, completion marker, or publisher
  policy used to approve itself. Gate code runs from full SHA `G` with no secret or mutation token.
- `T` is the exact event-selected subject head: authenticated synthetic merge `M` for
  `pull_request`, payload head equal to `github.sha` for `merge_group`, and protected head `Q` for
  `push`. It is not rollout commit `H` unless that particular final-candidate event actually selects
  `H`.
- The exact actionlint version is `1.7.12`; the installer carries reviewed SHA-256 values for its
  linux/darwin amd64/arm64 archives. Earlier actionlint releases are not release evidence because they
  reject the required `environment.deployment: false` syntax. Because 1.7.12 predates GitHub's
  `concurrency.queue`, the gate uses the design's exact yq-backed compatibility wrapper; no other
  actionlint diagnostic is ignored.
- The yq installer accepts only version `4.53.3` and URL
  `https://github.com/mikefarah/yq/releases/download/v4.53.3/yq_<os>_<arch>`. Its exact
  platform/SHA-256 pairs are linux/amd64
  `fa52a4e758c63d38299163fbdd1edfb4c4963247918bf9c1c5d31d84789eded4`, linux/arm64
  `578648e463a11c1b6db6010cbf41eafed6bee79466fcffa1bb446672cf7945ea`, darwin/amd64
  `b4ba1ecce3c47f00803f4f964de38394326c7a32eb6540616e04fb2935a0f08d`, and darwin/arm64
  `877de31753a4dd2401aa048937aa9a7fc4d5f6ce858cf31508c5802954297213`. Every other version,
  platform, URL, asset spelling, or digest fails before installation.

## 2. Dependency order

Although this is plan 1 of five, image publication cannot run first. Execute these gates:

The already committed Section 5.1 JSON tranche is retained as reviewed preparatory work. Do not begin
Section 5.2 or claim Task 1 complete until the Task 0 selector, placeholder, hosted-verifier
implementation, and bootstrap hosted-CI checks are green. The live transition proof is not a Task 0
prerequisite: the design keeps `docs/.edgezero-action-release.json` absent through `P`, creates stable
release `V` only after `P` qualifies, and exercises the real API/ref proof in the documentation-only
revision `R` under consumer plan Section 9. This exception records branch history; it does not relax
the landing order below.

1. Complete the repository-wide exact-version pin-gate migration and actionlint upgrade (Task 0).
2. Complete the protocol-owner validator, schema, fixtures, pin validator, image verifier, exact image
   source/context, fail-closed classifier, publisher contract checker, and protected workflow (Tasks
   1-2). Merge these as the separately reviewed protected gate baseline `G`.
3. Configure the organization required-workflow descriptor at exact SHA `G`, mandatory merge queue,
   protected environment, split tag rulesets, audit credentials, and dedicated GitHub App. Prove the rules
   and credential smoke before any candidate becomes `S` (Task 2).
4. Add only the canonical `release-request.json` for `G`. Run this isolated candidate through `G`,
   merge only through the queue, and require the API-visible exact post-merge push assertion. Record
   that default-branch commit as `S` (Task 3).
5. Run the already-landed publisher at `S`, verify digest `D`, and merge its ancestry-checked pin PR to
   create baseline `B` (Tasks 3-4).
6. Execute the four remaining cached-build, provenance-integration, launcher, and consumer plans on
   `B`; their final passing commit becomes action revision `P`.

Do not publish a provisional image without the validator. Do not use a placeholder `image.json` to
break the dependency cycle.

Within Task 2, implement and fixture-test both `check-build-container-publisher.sh` and
`verify-build-container-publication.sh` before completing the gate-update/rollback and pin branches of
`run-build-container-gate.sh`. The structural checker uses inert workflow fixtures before the real
publisher workflows exist; after those workflows are created, its golden test must accept their exact
committed forms before `G` is complete.

## 3. Planned file surface

Create:

- `.github/tools/edgezero-provenance-validator/{Cargo.toml,Cargo.lock}`
- `.github/tools/edgezero-provenance-validator/src/{lib,main,json_contract,archive,command,elf,extract,orchestration,self_test}.rs`
- `.github/tools/edgezero-provenance-validator/tests/cli.rs`
- `.github/docker/build-app-cli/provenance.schema.json`
- `.github/docker/build-app-cli/fixtures/provenance/**`
- `.github/docker/build-app-cli/fixtures/wasm-smoke.rs`
- `.github/docker/build-app-cli/fixtures/gnu-smoke.rs`
- `.github/docker/build-app-cli/Dockerfile`
- `.dockerignore`
- `.github/docker/build-app-cli/gate-paths.txt`
- `.github/docker/build-app-cli/image-context-paths.txt`
- `.github/docker/build-app-cli/verify-toolchain.sh`
- `.github/docker/build-app-cli/verify-published-image.sh`
- `.github/docker/build-app-cli/stage-build-context.sh`
- `.github/docker/build-app-cli/assert-build-container-context.sh`
- `.github/docker/build-app-cli/select-build-container-range.sh`
- `.github/docker/build-app-cli/assert-build-container-completion.sh`
- `.github/docker/build-app-cli/assert-build-container-dispatch-context.sh`
- `.github/docker/build-app-cli/assert-build-container-app-token.sh`
- `.github/docker/build-app-cli/verify-release-prerequisites.sh`
- `.github/docker/build-app-cli/write-publisher-prerequisite.sh`
- `.github/docker/build-app-cli/release-approval-gate.sh`
- `.github/docker/build-app-cli/write-image-release-record.sh`
- `.github/docker/build-app-cli/update-image-pin-pr.sh`
- `.github/docker/build-app-cli/classify-build-container-change.sh`
- `.github/docker/build-app-cli/run-build-container-gate.sh`
- `.github/docker/build-app-cli/check-build-container-publisher.sh`
- `.github/docker/build-app-cli/verify-build-container-publication.sh`
- `.github/docker/build-app-cli/verify-gate-rotation-lock.sh`
- `.github/actions/deploy-core/tests/verify-toolchain.test.sh`
- `.github/actions/deploy-core/tests/verify-published-image.test.sh`
- `.github/actions/deploy-core/tests/stage-build-context.test.sh`
- `.github/actions/deploy-core/tests/assert-build-container-context.test.sh`
- `.github/actions/deploy-core/tests/select-build-container-range.test.sh`
- `.github/actions/deploy-core/tests/assert-build-container-completion.test.sh`
- `.github/actions/deploy-core/tests/assert-build-container-dispatch-context.test.sh`
- `.github/actions/deploy-core/tests/assert-build-container-app-token.test.sh`
- `.github/actions/deploy-core/tests/verify-release-prerequisites.test.sh`
- `.github/actions/deploy-core/tests/write-publisher-prerequisite.test.sh`
- `.github/actions/deploy-core/tests/release-approval-gate.test.sh`
- `.github/actions/deploy-core/tests/write-image-release-record.test.sh`
- `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`
- `.github/actions/deploy-core/tests/classify-build-container-change.test.sh`
- `.github/actions/deploy-core/tests/run-build-container-gate.test.sh`
- `.github/actions/deploy-core/tests/check-build-container-publisher.test.sh`
- `.github/actions/deploy-core/tests/verify-build-container-publication.test.sh`
- `.github/actions/deploy-core/tests/verify-gate-rotation-lock.test.sh`
- `.github/actions/deploy-core/tests/build-container-workflows.test.sh`
- `.github/actions/deploy-core/tests/check-doc-action-pins.sh`
- `.github/actions/deploy-core/tests/check-doc-action-pins.mjs`
- `.github/actions/deploy-core/tests/check-doc-action-pins.test.mjs`
- `.github/actions/deploy-core/tests/install-actionlint.test.sh`
- `.github/actions/deploy-core/tests/install-yq.test.sh`
- `.github/actions/deploy-core/tests/run-actionlint.test.sh`
- `.github/workflows/build-container-ci.yml`
- `.github/workflows/publish-build-container.yml`
- `.github/workflows/rotate-build-container-gate.yml`
- `.github/CODEOWNERS`
- `scripts/install-actionlint.sh`
- `scripts/install-yq.sh`
- `scripts/run-actionlint.sh`

Created by the isolated source/release PR, not gate `G`:

- `.github/docker/build-app-cli/release-request.json`

Created by the pin PR, not source revision `S`:

- `.github/docker/build-app-cli/image.json`
- `.github/docker/build-app-cli/image-release-evidence.json`

Modify:

- `.github/docker/build-app-cli/check-image-pin.sh`
- `.github/actions/deploy-core/tests/check-image-pin.test.sh`
- `.github/actions/deploy-core/tests/check-action-pins.sh`
- `.github/actions/deploy-core/tests/run.sh`
- `.github/zizmor.yml`
- `.github/workflows/deploy-action.yml`
- `scripts/install-actionlint.sh`
- `docs/{package.json,package-lock.json}` (the exact Markdown parser used by the documentation gate)
- every existing `.github` workflow/composite containing a non-local external `uses:` ref
- the four deploy/adoption documents containing consumer `uses:` examples

## 4. Task 0: Enforce exact-version external references repository-wide

The current pin gate permits major/minor tags, prereleases, and commit SHAs. That is broader than
v6.40 and must be narrowed before adding the write-privileged publisher.

**Files:**

- Modify `.github/actions/deploy-core/tests/check-action-pins.sh` and its tests in `run.sh`.
- Create `.github/actions/deploy-core/tests/check-doc-action-pins.sh`.
- Modify `.github/zizmor.yml`.
- Modify `scripts/install-actionlint.sh` and the workflow environment that selects its version.
- Create `scripts/run-actionlint.sh` and its focused compatibility test.
- Modify external refs in `.github/workflows/{codeql,deploy-action,deploy-docs,fastly-installer-check,format,test}.yml`.
- Modify external refs in `.github/actions/{build-app-cli,config-push-fastly,deploy-fastly,healthcheck-fastly,rollback-fastly}/action.yml`.
- Modify examples in `docs/superpowers/specs/edgezero-deploy-github-action.md`,
  `docs/superpowers/plans/edgezero-deploy-action-implementation-plan.md`,
  `docs/superpowers/specs/edgezero-deploy-adoption-guide.md`, and `docs/guide/deploy-github-actions.md`.

- [x] Write failing pin-gate tests proving `@v1`, `@v1.2`, branches, prereleases, build metadata,
      full/abbreviated SHAs, malformed/leading-zero versions, and empty refs fail; canonical stable
      `@v1.2.3` passes; local actions and digest-pinned Docker actions remain valid. Generate invalid
      YAML fixtures under the test's temporary directory; do not commit them into a surface scanned
      by the production gate.
- [x] Resolve each existing external ref to a reviewed upstream exact stable patch release. Record
      the release URL and resolved commit in review evidence, prove the release tag exists and no
      same-named branch exists at review time, but write the version tag in YAML.
- [x] Change the structural YAML scanner to require canonical exact stable patch versions for every non-local external
      action and reusable workflow. Its default scan is exactly workflow `*.yml`/`*.yaml` files directly
      under `.github/workflows`, plus every repository-wide `action.yml`/`action.yaml`, pruning `.git`,
      `target`, and `node_modules`. Shell source and arbitrary YAML test data are not inputs. Do not add a
      low-privilege exception.
- [x] Reject empty and null `uses` scalars and count only parsed non-local external refs for the
      non-vacuity assertion. Encode each structurally extracted scalar so a multiline value cannot split
      into multiple shell records.
- [x] Require Docker action refs to match an immutable lowercase
      `docker://<name>@sha256:<64-lowercase-hex>` form; tags, uppercase hex, short digests, and other
      algorithms fail unless a separately reviewed digest algorithm is added to the policy.
- [x] Update the four prepublication adoption documents to use literal
      `<EDGEZERO_ACTION_VERSION>` where the future consumer will substitute stable release `V`;
      examples for third-party actions use real reviewed exact patch versions. Replace
      `ubuntu-latest` and every other runner label on a step-based consumer job containing a
      `steps[*].uses` EdgeZero action reference with literal `runs-on: ubuntu-24.04`. A caller job with
      `jobs.<id>.uses` must omit both `steps` and `runs-on`; its called workflow selects the runner.
- [x] Add `check-doc-action-pins.sh` to parse fenced YAML in every tracked Markdown file and implement
      both documentation states from the design. In bootstrap state, absent
      `docs/.edgezero-action-release.json` permits the exact EdgeZero placeholder only in the four
      named prepublication documents. In transition state, a candidate adds the exact JCS `{V,P}`
      record, changes only tracked Markdown plus that record, removes every placeholder, and uses one
      literal `V` in all EdgeZero refs. In released state, the base record cannot disappear; it is
      either byte-identical or replaced atomically with all documentation refs by a strictly greater
      stable version under the same hosted release/ref proof. Concrete third-party refs always use
      exact stable patch versions; major/minor/prerelease/branch/SHA refs fail. In all three
      documentation states, classify each parsed job by AST shape: a step-based job containing a
      `steps[*].uses` EdgeZero action reference requires literal `runs-on: ubuntu-24.04`; a job-level
      EdgeZero reusable-workflow call with `jobs.<id>.uses` must have no `steps` and no `runs-on`.
      Reject mixed shapes, absent or dynamic labels on step-based jobs, `ubuntu-latest`, other
      standard, larger, custom, and self-hosted labels. Add positive/negative state-transition,
      base/candidate, partial-update, downgrade/deletion, hidden-placeholder, mixed-version, job-
      shape, and runner-label cases to `run.sh`. In transition and released states, reject the literal
      placeholder anywhere in every tracked Markdown document, including prose and non-YAML fences;
      YAML parsing remains the authority for reference and job-shape checks.
- [x] Reconcile the implemented v6.29 selector with v6.31 before freezing `G`. Add the reproduced
      hosted case where payload base `37f1a137...` precedes synthetic first parent `593fc928...`, and
      head `8f68476d...` is the second parent. Authenticate the context-selected synthetic commit;
      require exactly `[F,J]` and `A` ancestor-or-equal to `F`; compare `F..M`, not `A..M` or latest
      main. Reject wrong/missing/reordered/extra parents, unavailable/shallow objects, replace/graft
      history, wrong repository/ref, and reversed/incomparable ancestry. Test a release record added
      upstream at `F`, intervening upstream executable changes, candidate-only mixed changes, and
      later main movement. Run every subject Git command with `--no-replace-objects` and the design's
      exact minimal child environment; inject and reject repository/worktree/common-dir, namespace,
      index, object/alternate, shallow-file, inline-config, replacement-base, XDG, and arbitrary
      ambient state. Keep these tasks unchecked until code, fixtures, and hosted CI agree.
- [x] Add the hosted transition verifier to gate `G`. With only read permissions and fixed
      no-redirect versioned requests, it proves record `V` is a published `draft:false`,
      `prerelease:false`, `immutable:true` release whose API target and anonymous peeled remote ref
      both equal record `P`. Bootstrap and unchanged released-state checks remain offline. Candidate
      code cannot replace the verifier or release record parser.
      Implementation uses Node's typed JSON and Git subprocess APIs plus exact `markdown-it@15.0.1`
      for Markdown fence parsing and pinned yq for YAML. The Bash entrypoint and colocated module,
      tests, and docs dependency manifests belong to `G`; install dependencies with
      `npm --prefix <gate-root>/docs ci --ignore-scripts`. Never resolve parser modules or run npm
      from the candidate subject checkout. Its release GET uses API version `2026-03-10`, exact gate
      user agent and media headers, and verifies the selected-version/content-type response headers;
      the earlier `2022-11-28` request is not compliant transition evidence. Pass authorization only
      through curl config stdin and give curl only runner `PATH` plus `LC_ALL=C`; fixtures reject
      inherited token/proxy/CA/home/XDG/curl-home/arbitrary values. Give anonymous release-ref Git only
      its existing closed temporary-home/config environment plus replacement-object hardening. Task 0
      qualifies the implementation with adversarial fixtures and a hosted bootstrap run; consumer plan
      Section 9 supplies the real immutable `V` transition proof when revision `R` can legally add the
      release record.
- [x] Retain global zizmor `ref-pin` as defense in depth and document that the structural scanner is
      stricter. Rewrite `.github/zizmor.yml`'s existing comment so it no longer claims full commit
      SHAs pass repository policy: `ref-pin` accepts symbolic refs, while the structural gate permits
      only exact stable patch tags. Update contradictory prose in all four named documents, not only
      their fenced YAML examples.
- [x] Upgrade actionlint to exactly `1.7.12`. Pin these reviewed release archives in
      `scripts/install-actionlint.sh`: linux/amd64
      `8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8`, linux/arm64
      `325e971b6ba9bfa504672e29be93c24981eeb1c07576d730e9f7c8805afff0c6`, darwin/amd64
      `5b44c3bc2255115c9b69e30efc0fecdf498fdb63c5d58e17084fd5f16324c644`, and darwin/arm64
      `aba9ced2dee8d27fecca3dc7feb1a7f9a52caefa1eb46f3271ea66b6e0e6953f`. Test exact version,
      supported tuples, unknown tuple rejection, and checksum mismatch. Add an actionlint regression
      fixture containing `environment: {name: build-container-release, deployment: false}`.
- [x] Before the publisher exists, add failing tests for pinned actionlint's two known syntax gaps.
      `run-actionlint.sh` requires mikefarah yq 4.53.3 and structurally permits workflow-level
      `concurrency.queue: max` only in the publisher and gate-rotation workflows, each with exact group
      `edgezero-build-container-publication` and literal `cancel-in-progress:false`. It permits exactly
      the four `job.workflow_*` properties only in approved expressions/checkout ref locations of
      `.github/workflows/build-app-cli.yml`. Reject duplicate/aliased/misplaced/wrong values,
      misspellings, extra job properties, other workflows, and dynamic expressions.
- [x] After structural validation, make line-count-preserving temporary copies that blank only the two
      approved queue lines and substitute same-type constants for only the approved job-context
      expressions. Run unfiltered actionlint 1.7.12 on those files and remap paths/lines; do not use
      `-ignore` or filter diagnostics. Raw canonical fixtures must emit exactly the reviewed unsupported
      queue/job-context diagnostic set, sanitized fixtures must pass, and every unrelated actionlint
      error must remain fatal.
- [x] Add `install-yq.test.sh` to the protected suite and gate manifest. Test exact version `4.53.3`,
      all four literal URL/platform/digest pairs above, successful exact-version execution, unknown
      version/OS/architecture and URL/asset-spelling rejection, checksum mismatch, and failure before
      installation for every unpinned tuple.
- [x] Scan that exact default surface, including reusable-workflow job-level `uses`, and require at
      least one parsed external ref so a broken parser cannot pass vacuously.
- [x] Run the pin suite, actionlint, and zizmor.

```bash
bash .github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.sh
scripts/run-actionlint.sh
zizmor --offline .github/workflows .github/actions
```

**Gate:** both structural scanners pass their exact surfaces and report non-zero parsed-reference
counts; no broad `rg` gate scans intentional invalid test strings.

## 5. Task 1: Implement the protocol-owner validator for gate baseline `G`

This task owns protocol-1 encoding and validation. No shell, `jq`, system `tar`, or general-purpose
archive crate may become a second wire implementation. It is a hard dependency of Task 2 and lands in
protected gate baseline `G` before source revision `S` is proposed.

**Files:**

- Create standalone workspace `.github/tools/edgezero-provenance-validator` with its own
  `Cargo.toml`, `[workspace]`, `Cargo.lock`, and
  `src/{lib,main,json_contract,archive,elf,extract}.rs`.
- Put module unit tests beside their implementation under `src/`; create only the true process-level
  integration test `.github/tools/edgezero-provenance-validator/tests/cli.rs`.
- Create `.github/docker/build-app-cli/provenance.schema.json`.
- Create `.github/docker/build-app-cli/fixtures/provenance/{valid,invalid}/**`.
- Do not modify or include the root workspace manifests; the validator has no external local path
  dependency.

### 5.1 JSON/schema tranche

- [x] Add one Draft 2020-12 schema and exact valid/invalid fixtures for both `expected.json` and
      `app-cli-meta.json` from design Section 6.2. Write colocated failing tests for RFC 8785 bytes,
      recursive duplicate-key rejection before object construction, every exact field/type/bound,
      unknown and missing fields, noncanonical decimal/hash/name values, schema/protocol mismatch,
      `container-ref` derivation, `workspace-id` rendering, and complete caller/platform identity
      mismatch. Workspace/suffix hash computation vectors belong to the cache-actions follow-on plan,
      not this protocol crate.
- [x] Test a closed typed canonical encoder. Protocol 1 contains only bounded strings, positive
      integers, null, fixed objects, and the `needed` array; no generic floating-point value is accepted.
- [x] Run `cargo test --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml json_contract::tests`; expected: non-zero for
      unimplemented behavior.
- [x] Implement only `json_contract.rs`; rerun the focused and full crate tests; expected: pass.
      Commit the green JSON/schema tranche.

### 5.2 Archive/extraction tranche

- [x] Add a byte-for-byte golden archive from design Section 6.3 plus malformed base-256/octal,
      checksum, embedded-NUL, PAX/GNU, sparse, duplicate, extra, traversal, link, special-file, header,
      order, size, padding, end-block, overflow, and trailing-data fixtures.
- [x] Write failing encoder, parser, and extraction tests. Assert two repeated encodes are identical,
      all payload padding is zero, exactly two end blocks precede EOF, and failure leaves the fresh output
      parent empty.
- [x] Run `cargo test --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml archive::tests`; expected: non-zero for
      unimplemented protocol behavior.
- [x] Implement `archive.rs` and `extract.rs` directly over bounded `Read + Seek`/`Write`; do not
      invoke system `tar`, add a tar crate, or load the allowed 512 MiB binary wholesale. Create outputs
      atomically and require the final regular file to have mode 0755 and link count one. Rerun focused
      and full crate tests; expected: pass. Commit the green archive/extraction tranche.

### 5.3 ELF/loadability tranche

- [x] Add controlled static/dynamic valid, wrong class/endian/type/architecture/interpreter,
      wrong `EI_VERSION`/`e_version`/`EI_OSABI`/`EI_ABIVERSION`/`EI_PAD`/`e_flags`/`e_ehsize`/
      `e_phentsize`, zero `e_phnum`, `PN_XNUM`,
      malformed/duplicate `PT_DYNAMIC`, unequal file/memory sizes, contradictory file-offset versus
      virtual-address dynamic tables, nonunique/nonreadable/non-file-backed mappings,
      missing/nonzero-after `DT_NULL`, conflicting string-table tags,
      unmapped/overlapping string ranges, malformed string/interpreter termination, RPATH/RUNPATH,
      AUDIT/DEPAUDIT/CONFIG/AUXILIARY/FILTER/POSFLAG rejection, System-V/GNU OSABI acceptance and every
      other OSABI rejection, libc-style dependency `PT_INTERP` acceptance only for the exact fixed
      interpreter, primary SONAME rejection, optional library SONAME equal to its filename, reserved
      primary/interpreter filenames, cross-object alias collisions, empty/oversized/slash/backslash/
      dollar-containing/duplicate SONAME rejection, NODEFLIB/LOADFLTR and unknown-flag rejection, every
      in-range and just-outside case for the closed numeric tag allowlist, exact
      `DT_FLAGS=0x0000001e` and `DT_FLAGS_1=0x5eff976f` mask boundaries, unknown standard/GNU/OS/processor
      tag rejection, duplicate rejection for every singleton tag, slash/backslash/dollar-containing
      dependency including every `$ORIGIN`, `$LIB`, and `$PLATFORM` spelling, missing
      direct/transitive flat-closure library, duplicate basename, dangling or escaping candidate,
      mixed architecture, duplicate-needed, interpreter dependency, and cycle fixtures for the
      controlled loader profile in design Section 6.4.
- [x] Write failing tests for machine, interpreter/null, byte-sorted duplicate-preserving direct
      `DT_NEEDED`, digest, size, exact `/opt/edgezero/runtime-lib` lookup, symlink/hardlink/subdirectory
      rejection, duplicate basename, interpreter parsing, the one `ld-linux-x86-64.so.2` alias resolving
      to the already-validated interpreter rather than a flat copy, and recursive dependency resolution against
      a synthetic image root. Add preload presence, cache-only/default-directory/hardware-capability
      substitution, direct-loader argv, and explicit `dlopen` non-claim fixtures.
- [x] Run `cargo test --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml elf::tests`; expected: non-zero for
      unimplemented inspection/loadability behavior.
- [x] Implement `elf.rs` with bounded ranged reads and checked offsets. Do not invoke `ldd`, the
      loader, or the artifact. Rerun focused and full crate tests; expected: pass. Commit the green ELF
      tranche.

### 5.4 CLI/capability tranche

- [x] Write failing library integration tests using a private synthetic-root harness for deterministic
      expected-write/package/validate round trips, identity mismatch, atomic cleanup, no-replace
      collision, and host-deletion recovery. This
      harness calls library entry points and is not a CLI option or production bypass. Write host process
      tests proving all output-producing commands reject every `--work-root` that does not canonicalize
      to literal `/work`, plus process tests for self-test fixture integrity. Run
      `cargo test --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml --test cli`;
      expected: non-zero until wired. Implement:

```text
edgezero-provenance-validator write-expected \
  --work-root /work \
  --app-repo-id <canonical-decimal-u64> \
  --source-revision <40-lowercase-hex> \
  --app-cli-package <package> \
  --app-cli-bin <binary> \
  --workspace-id sha256:<64-lowercase-hex> \
  --platform-id sha256:<64-lowercase-hex> \
  --provenance-protocol 1 \
  --output /work/expected/expected.json

edgezero-provenance-validator write-release-request \
  --work-root /work \
  --gate-sha <40-lowercase-hex> \
  --provenance-protocol 1 \
  --release-tag build-container-v<positive-decimal> \
  --output /work/release/release-request.json

edgezero-provenance-validator package \
  --work-root /work \
  --binary /work/input/app-cli \
  --schema /usr/local/share/edgezero/provenance.schema.json \
  --expected /work/input/expected.json \
  --app-cli-version <version> \
  --archive /work/packaged/artifact.tar

edgezero-provenance-validator validate \
  --work-root /work \
  --archive /work/input/artifact.tar \
  --schema /usr/local/share/edgezero/provenance.schema.json \
  --expected /work/input/expected.json \
  --output /work/validated/app-cli

edgezero-provenance-validator self-test \
  --fixtures /usr/local/share/edgezero/provenance-fixtures
```

- [x] Make production `write-expected`, `write-release-request`, `package`, and `validate` require
      canonical `--work-root /work`.
      `write-expected` accepts only the typed bounded scalars above, fixes schema version `1`, derives
      `container-ref`, and is the sole expected-identity encoder. Every command creates exactly one
      output through a create-new temporary sibling plus Linux no-replace rename and fails if the parent
      is not fresh, empty, canonical, writable, and confined. Test lexical and canonical confinement for
      every binary, expected, archive, schema, fixture, and output path; require schema and fixture paths
      to equal their baked image-owned literals. Handled failures remove the
      sibling; synthetic-root library tests model host deletion of the whole parent after
      SIGKILL/timeout. The validator never executes the app binary. Positive CLI round trips run only in
      Task 3's container, where literal `/work` exists.
- [x] Make `write-release-request` the sole release-request producer. Accept only typed gate SHA,
      protocol `1`, canonical release tag, and the literal fresh output path; test exact three-key JCS
      bytes, duplicate/missing/unknown flags, no-replace publication, and output cleanup.
- [x] Implement `self-test` as a compiled manifest of exact relative paths, fixture SHA-256 values,
      and valid/invalid outcomes. A missing, extra, or changed fixture fails.
- [x] Use synchronous Rust; do not add Tokio or change dependencies of core/adapter crates.
- [x] Run process, focused, and full crate tests; expected: pass. Commit the green CLI/capability
      tranche.
- [x] Run the focused crate tests, then the repository-required Rust and documentation checks.

```bash
cargo test --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml
cargo fmt --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml \
  --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo check --locked --workspace --all-targets --features "fastly cloudflare spin"
cargo check --locked -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
npm --prefix docs ci
npm --prefix docs run format
npm --prefix docs run lint
npm --prefix docs run build
./scripts/check_no_placeholder_pins.sh
./scripts/check_no_legacy_typed_reads.sh
cargo run -q --bin check_no_nested_app_config --features nested-app-config-check -- \
  examples/app-demo crates/edgezero-cli/src/templates
cargo test -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config
cargo test -p edgezero-adapter-fastly --all-targets --features cli
cargo test -p edgezero-cli --test generated_project_builds -- --ignored
cargo clippy -p edgezero-adapter-fastly --features cli --all-targets -- -D warnings
cargo clippy -p edgezero-adapter-fastly --no-default-features --lib -- -D warnings
cargo fmt --manifest-path examples/app-demo/Cargo.toml --all -- --check
cargo clippy --manifest-path examples/app-demo/Cargo.toml \
  --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path examples/app-demo/Cargo.toml --locked --workspace --all-targets
```

**Gate:** deterministic expected-write/package/validate golden tests and every capability fixture hash pass from a
clean checkout. The candidate PR must also pass every current format/test matrix job, including the
four wasm clippy legs and three wasm test runners; the local command list does not replace those
runner-backed gates. Task 2 copies the exact reviewed validator source, schema, and fixtures into the
gate-owned image source; the Dockerfile rebuilds the binary from that closed source rather than
copying this host build.

## 6. Task 2: Establish protected gate baseline `G`

This task creates the trust root that evaluates the later release request, owns every repository
image-context input and fixed external-source verification rule, and owns every script that can see a
release credential. Candidate code is always subject data. No image is published in this task.

**Files:**

- Create `.github/docker/build-app-cli/{gate-paths,image-context-paths}.txt`, the Dockerfile, and root
  `.dockerignore`.
- Create `.github/CODEOWNERS`.
- Modify `.github/docker/build-app-cli/check-image-pin.sh`.
- Create `.github/docker/build-app-cli/{verify-toolchain,verify-published-image}.sh`.
- Create `.github/docker/build-app-cli/{stage-build-context,assert-build-container-context}.sh`.
- Create `.github/docker/build-app-cli/{classify-build-container-change,run-build-container-gate}.sh`.
- Create `.github/docker/build-app-cli/{verify-release-prerequisites,release-approval-gate}.sh`.
- Create `.github/docker/build-app-cli/{update-image-pin-pr,check-build-container-publisher,verify-build-container-publication}.sh`.
- Create `.github/docker/build-app-cli/{write-publisher-prerequisite,verify-gate-rotation-lock}.sh`.
- Create the matching focused tests under `.github/actions/deploy-core/tests/`.
- Create `.github/workflows/{build-container-ci,publish-build-container,rotate-build-container-gate}.yml`.
- Modify `.github/actions/deploy-core/tests/run.sh` and
  `.github/workflows/deploy-action.yml`.

### 6.1 Pin-record validator

- [x] Extend `check-image-pin.test.sh` first. Cover the valid five-field record; malformed,
      duplicate, extra, and missing fields; wrong JSON types; foreign or empty repository; mutable,
      zero, uppercase, or malformed digest/source; non-integer or non-`1` protocol; malformed tag;
      and any use of the tag as a runtime pull reference.
- [x] Add independent canonical bytes and malformed cases for the exact ten-field
      `image-release-evidence.json`: duplicate/extra/missing/reordered keys, non-JCS bytes, wrong
      strings/integer, run id/attempt precision, invalid UTC grammar/calendar, login/challenge/digest/tag/S
      grammar, and every cross-file mismatch. Pair add/change/delete must be atomic.
- [x] Implement `check-image-pin.sh <path>` with Bash and `jq`. Detect duplicate top-level
      keys from streaming parse events before ordinary object construction. Accept exactly:

```json
{
  "repository": "ghcr.io/stackpop/edgezero-build-app-cli",
  "tag": "build-container-v1",
  "digest": "sha256:<64-lowercase-hex>",
  "image-source-revision": "<40-lowercase-hex>",
  "provenance-protocol": 1
}
```

      The tag matches `^build-container-v[1-9][0-9]*$` and is informational. Expose only the
      digest-qualified runtime ref, source SHA, and protocol through typed subcommands or shell-safe
      output fields.

- [x] Run the focused test and `shellcheck -S warning`. Do not create placeholder
      `image.json` or evidence record.
- [x] Implement and test the gate-owned typed release-record writer and paired validator with exactly
      this CLI:

```text
write-image-release-record.sh --image-path <new-image-file> --evidence-path <new-evidence-file> --repository ghcr.io/stackpop/edgezero-build-app-cli --release-tag <tag> --image-digest <D> --source-revision <S> --provenance-protocol 1 --approval-challenge <64-lowercase-hex> --approver-login <login> --reviewed-at <RFC3339-UTC> --run-attempt <canonical-positive-u32> --run-id <canonical-positive-u64> --screenshot-sha256 <sha256:digest>
```

      The writer accepts only those typed current publisher/approval scalars and never raw JSON. Its
      two absent output paths are distinct direct children of one canonical mode-0700 private directory
      outside every Git repository. It invokes the paired validator, atomically publishes both
      mode-0644 files without replacement, removes a partial pair on failure, and emits no stdout on
      success. `image.json` has exactly these compact bytes:

```text
{"digest":"<D>","image-source-revision":"<S>","provenance-protocol":1,"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"<tag>"}
```

      The evidence output has exactly these ten-field JCS bytes:

```text
{"approval-challenge":"<64-lowercase-hex>","approver-login":"<verified-login>","image-digest":"<D>","release-tag":"<tag>","reviewed-at":"<RFC3339-UTC>","run-attempt":"<canonical-positive-u32>","run-id":"<canonical-positive-u64>","schema-version":1,"screenshot-sha256":"sha256:<64-lowercase-hex>","source-revision":"<S>"}
```

      Tests cover exact CLI rejection,
      pre-existing/split/non-private/in-repository outputs, exact current bytes, mode, atomic
      no-replace pair publication, partial-pair cleanup, silent success, and cross-record validation.
      The updater alone copies the verified bytes into its private worktree; the writer never writes
      inside a repository. Runtime actions continue to parse only `image.json`.

### 6.2 Image and runtime verification

- [x] Immediately before the gate-image commit, re-resolve the official
      `rust:1.95.0-slim-bookworm` linux/amd64 leaf and require the reviewed digest in Section 1.
      Independently download the exact sccache asset and checksum companion and require the reviewed
      checksum. Stop for review on movement or disagreement; never commit a placeholder.
- [x] Commit the exact multi-stage Dockerfile, root `.dockerignore`, and sorted
      `image-context-paths.txt` as gate-owned inputs. The context manifest excludes the root workspace
      manifests and contains only the complete standalone validator directory, schema/fixtures,
      `.tool-versions`, Fastly versions, Dockerfile, and `.dockerignore`; every entry is also in
      `gate-paths.txt`. Run `cargo metadata --locked --manifest-path
.github/tools/edgezero-provenance-validator/Cargo.toml` inside the staged context and reject any
      workspace member or path dependency outside that validator directory. Any new effective input
      requires gate rotation.
- [x] Write `stage-build-context.test.sh` before its helper. Cover missing/extra/duplicate/unsorted
      manifest entries, symlink/hardlink/FIFO/device inputs, path escape, dirty gate checkout,
      candidate Dockerfile substitution, changed `S` copy of a manifested byte, unmanifested source,
      remote `ADD`, bind-mounted build context, broad `COPY`, and post-install replacement. The helper
      creates a fresh directory outside both checkouts and copies only regular manifested files from
      exact clean `G`, preserving paths and executable modes; the publisher passes that directory as
      the sole Docker context.
- [x] Build the validator in the Dockerfile with one
      `cargo build --locked --release --manifest-path
.github/tools/edgezero-provenance-validator/Cargo.toml` invocation. Test both the exact staged-
      context metadata command and exact Docker build. Use the reviewed Rust base digest and
      checksum-verified Fastly/sccache assets; install `wasm32-wasip1`; copy only
      the exact final binaries/assets and the complete startup-library closure. No later Dockerfile
      instruction may replace an installed tool, validator, schema, fixture, interpreter, or closure
      member. Static contract tests bind the instruction sequence and destinations.
- [x] Populate flat `/opt/edgezero/runtime-lib` with the complete reviewed x86-64 startup closure,
      excluding the interpreter. Require exact dynamic interpreter `/lib64/ld-linux-x86-64.so.2`,
      validate every library's role and filename/SONAME aliases, reserve `app-cli` and
      `ld-linux-x86-64.so.2` against flat-directory copies, and remove
      `/etc/ld.so.preload`. Dynamic app-binary launches use the container runtime's argv API with exact
      interpreter options `--inhibit-cache --glibc-hwcaps-mask '' --library-path
/opt/edgezero/runtime-lib`; static binaries run directly. Test that no shell, cache, default
      directory, hardware-capability directory, or preload can substitute a startup object.
- [x] Before freezing `G`, compile a real GNU-target Rust CLI using the selected image toolchain.
      Run package, validate, and controlled-loader smoke through their exact isolated profiles against
      the image's actual Bookworm libc/interpreter closure. Record the measured libc/loader digests
      and accepted headers; do not substitute synthetic ELF fixtures or a distro-package inspection
      for this image acceptance test. In a separate isolated loader-comparison harness, prove primary
      and dependency SONAME-collision fixtures are rejected and the interpreter alias resolves to the
      same object the parser predicts. No application execution occurs in the parser profile.
- [x] Write failing command-fixture tests for `verify-toolchain.sh`. Cover exact, prerelease,
      extra-text, missing, and malformed Rust/Fastly/sccache version output; absent
      `wasm32-wasip1`; failed minimal compile; invalid wasm magic; validator self-test failure;
      wrong uid/gid; read-only-root failure; and missing, replaced, or semantically incompatible GNU
      `/usr/bin/env -S`.
- [x] Snapshot the `self-test` container's complete create/run contract: digest-pinned linux/amd64
      image, uid/gid 1001, read-only root, all capabilities dropped, no-new-privileges, no network,
      2 GiB memory/swap, 64 pids, 10-minute wall limit, no host bind mounts, only `/work/home` and
      `/work/tmp` tmpfs, fixed `/usr/bin/env` entrypoint, exact `-S` plus sorted placeholder split
      string, exact three-variable target environment, and exact baked-fixture argv. Seed inherited
      image/Docker poison variables and prove they are absent from the target. Reject every extra
      target environment name, mount, flag, entrypoint, command prefix, non-placeholder assignment,
      value-derived argv construction, or path. Snapshot the exact container `Path` and `Args`; do not
      use accidental byte inequality between values and argv as the assertion.
- [x] Before publication, drive the verifier through real `docker create --env-file`, delete and
      verify absence of that file before `docker start --attach`, and assert exact post-`env -i`
      bytes for empty values plus spaces, quotes, backslashes, dollar signs, `#`, `=`, literal
      `${...}` text, and non-ASCII values. Prove expansion is neither recursive nor resplit, the
      placeholder list and env-file names agree exactly, and image/Docker poison is absent. Reject
      newline/NUL serialization, duplicate/bare/comment/blank/extra env-file entries, missing
      placeholders, and any argv element constructed by inserting a value.
- [x] Implement exact semantic-version parsing, installed-target inspection, and compilation of the
      committed `wasm-smoke.rs` library into tmpfs. Substring version matching is forbidden.
- [x] Write failing fixture tests for `verify-published-image.sh`. Cover accepted leaf Docker and
      OCI manifests; rejected one-entry/multi-entry indexes; missing config/layers; malformed BuildKit
      metadata; wrong OS/architecture; wrong source/revision/protocol labels; mutable-tag lookup; private
      registry response; and every toolchain/validator failure.
- [x] Implement the verifier over a supplied `repository@digest`, source SHA, and protocol. Use
      `docker buildx imagetools inspect "$REF" --raw` for media type and
      `--format '{{json .Image}}'` for image OS/architecture. Never inspect a tag to discover
      identity.
- [x] Exercise the baked validator's `write-expected` profile with only a fresh writable
      `/work/expected`, tmpfs home/temp, no repository/binary/target/Cargo/cache/token mount, and
      `--network=none`. Then run deterministic package twice and validate every golden/malformed
      archive with each operation's exact mount profile. Assert no `/work/package` convention
      exists.
- [x] Run both focused tests and shellcheck before implementing, then rerun them green.

### 6.3 Protected classifier and required workflow

- [x] Commit `gate-paths.txt` as the canonical, sorted list of every gate-owned path:
      the standalone validator manifest/lockfile, validator crate, schema and fixtures, pin scanner,
      all classifier/verifier/policy/approval/updater/publisher-checker helpers,
      context manifest/Dockerfile/`.dockerignore`, their focused tests and `run.sh` wiring,
      `scripts/{install-actionlint,install-yq,run-actionlint}.sh`, the installer tests including
      `install-yq.test.sh`, `.github/CODEOWNERS` with exact manifest expansion plus directory-wide
      `/.github/workflows/` ownership,
      `.github/zizmor.yml`, and
      all three container workflows (`build-container-ci.yml`, `publish-build-container.yml`, and
      `rotate-build-container-gate.yml`). Include exact `docs/package.json` and
      `docs/package-lock.json`. The manifest contains itself. `image-context-paths.txt` is a strict
      subset, is also canonical/sorted, and closes over every Docker build input and local Cargo
      dependency.
- [x] Freeze the literal gate manifest only after every gate-owned helper and focused test exists.
      At that point, copy the reviewed final path list into this plan as a fenced exact sorted block and
      require it to be byte-equal to `.github/docker/build-app-cli/gate-paths.txt`. Do not invent that
      final block before the file surface is complete.
      The frozen manifest is:

```text
.dockerignore
.github/CODEOWNERS
.github/actions/deploy-core/tests/assert-build-container-app-token.test.sh
.github/actions/deploy-core/tests/assert-build-container-completion.test.sh
.github/actions/deploy-core/tests/assert-build-container-context.test.sh
.github/actions/deploy-core/tests/assert-build-container-dispatch-context.test.sh
.github/actions/deploy-core/tests/build-container-workflows.test.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-build-container-publisher.test.sh
.github/actions/deploy-core/tests/check-doc-action-pins.mjs
.github/actions/deploy-core/tests/check-doc-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.test.mjs
.github/actions/deploy-core/tests/check-image-pin.test.sh
.github/actions/deploy-core/tests/classify-build-container-change.test.sh
.github/actions/deploy-core/tests/install-actionlint.test.sh
.github/actions/deploy-core/tests/install-yq.test.sh
.github/actions/deploy-core/tests/release-approval-gate.test.sh
.github/actions/deploy-core/tests/run-actionlint.test.sh
.github/actions/deploy-core/tests/run-build-container-gate.test.sh
.github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/select-build-container-range.test.sh
.github/actions/deploy-core/tests/stage-build-context.test.sh
.github/actions/deploy-core/tests/update-image-pin-pr.test.sh
.github/actions/deploy-core/tests/verify-build-container-publication.test.sh
.github/actions/deploy-core/tests/verify-gate-rotation-lock.test.sh
.github/actions/deploy-core/tests/verify-published-image.test.sh
.github/actions/deploy-core/tests/verify-release-prerequisites.test.sh
.github/actions/deploy-core/tests/verify-toolchain.test.sh
.github/actions/deploy-core/tests/write-image-release-record.test.sh
.github/actions/deploy-core/tests/write-publisher-prerequisite.test.sh
.github/actions/deploy-fastly/versions.json
.github/docker/build-app-cli/Dockerfile
.github/docker/build-app-cli/assert-build-container-app-token.sh
.github/docker/build-app-cli/assert-build-container-completion.sh
.github/docker/build-app-cli/assert-build-container-context.sh
.github/docker/build-app-cli/assert-build-container-dispatch-context.sh
.github/docker/build-app-cli/check-build-container-publisher.sh
.github/docker/build-app-cli/check-image-pin.sh
.github/docker/build-app-cli/classify-build-container-change.sh
.github/docker/build-app-cli/fixtures/gnu-smoke.rs
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-base256.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-checksum.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-duplicate.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-embedded-nul.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-end-block.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-extra-end-block.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-extra.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-gnu.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-header.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-link.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-max-size.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-octal-digit.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-octal-padding.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-order.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-padding.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-pax.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-size-limit.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-size.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-sparse.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-special.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-trailing.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/archive-traversal.tar
.github/docker/build-app-cli/fixtures/provenance/invalid/elf-malformed/app-cli
.github/docker/build-app-cli/fixtures/provenance/invalid/expected-duplicate.json
.github/docker/build-app-cli/fixtures/provenance/invalid/meta-missing-interpreter.json
.github/docker/build-app-cli/fixtures/provenance/valid/archive.tar
.github/docker/build-app-cli/fixtures/provenance/valid/dynamic-meta.json
.github/docker/build-app-cli/fixtures/provenance/valid/elf-static/app-cli
.github/docker/build-app-cli/fixtures/provenance/valid/expected.json
.github/docker/build-app-cli/fixtures/provenance/valid/static-meta.json
.github/docker/build-app-cli/fixtures/wasm-smoke.rs
.github/docker/build-app-cli/gate-paths.txt
.github/docker/build-app-cli/image-context-paths.txt
.github/docker/build-app-cli/provenance.schema.json
.github/docker/build-app-cli/release-approval-gate.sh
.github/docker/build-app-cli/run-build-container-gate.sh
.github/docker/build-app-cli/select-build-container-range.sh
.github/docker/build-app-cli/stage-build-context.sh
.github/docker/build-app-cli/update-image-pin-pr.sh
.github/docker/build-app-cli/verify-build-container-publication.sh
.github/docker/build-app-cli/verify-gate-rotation-lock.sh
.github/docker/build-app-cli/verify-published-image.sh
.github/docker/build-app-cli/verify-release-prerequisites.sh
.github/docker/build-app-cli/verify-toolchain.sh
.github/docker/build-app-cli/write-image-release-record.sh
.github/docker/build-app-cli/write-publisher-prerequisite.sh
.github/tools/edgezero-provenance-validator/Cargo.lock
.github/tools/edgezero-provenance-validator/Cargo.toml
.github/tools/edgezero-provenance-validator/src/archive.rs
.github/tools/edgezero-provenance-validator/src/command.rs
.github/tools/edgezero-provenance-validator/src/elf.rs
.github/tools/edgezero-provenance-validator/src/extract.rs
.github/tools/edgezero-provenance-validator/src/json_contract.rs
.github/tools/edgezero-provenance-validator/src/lib.rs
.github/tools/edgezero-provenance-validator/src/main.rs
.github/tools/edgezero-provenance-validator/src/orchestration.rs
.github/tools/edgezero-provenance-validator/src/self_test.rs
.github/tools/edgezero-provenance-validator/tests/cli.rs
.github/workflows/build-container-ci.yml
.github/workflows/publish-build-container.yml
.github/workflows/rotate-build-container-gate.yml
.github/zizmor.yml
.tool-versions
docs/package-lock.json
docs/package.json
scripts/install-actionlint.sh
scripts/install-yq.sh
scripts/run-actionlint.sh
```

- [x] Generate `.github/CODEOWNERS` exactly from that canonical manifest: one line per manifested path
      in manifest order, spelled `/<path> @stackpop/edgezero-build-container-gate-reviewers`, with no
      wildcard, alternate owner, unowned manifest entry, or extra non-comment rule. `CODEOWNERS` and
      `gate-paths.txt` contain and therefore protect themselves.
- [x] Write failing classifier tests for the design's closed pull-request, merge-group, and protected-
      push event-to-range table, including exact payload/context SHA and ref agreement;
      add/change/rename/delete; all-zero first-push base; shallow/missing commits; malformed or duplicate
      output; ordinary, isolated-release-request, and gate-update modes; mixed gate/non-gate changes;
      old/candidate manifest union handling; exact failed-`G'` gate-rollback restoration; every
      interrupted pointer/policy state; and every context-manifest entry. Pin classification is
      exact paired add/change/delete detection for `image.json` and `image-release-evidence.json`; a release request is relevant only when it is
      the sole changed path and has exact canonical shape and active `G`.
- [x] Implement the classifier over explicit base/head SHAs from a full subject checkout. Its only
      stdout is exactly two fixed-order lines: `mode=ordinary|gate-update|gate-rollback` and
      `relevant=true|false`; both gate modes require true. Any invalid range, missing object, gitlink,
      path ambiguity, mixed
      gate/non-gate change, or unmanifested repository image-context input fails. Gate-update mode requires the
      base's manifested tree to equal old `G`, validates candidate manifest and CODEOWNERS as inert data, and permits
      changed paths only in the union of old and candidate manifests. Gate-rollback requires disabled
      release, both pointers at old `G`, a base valid as failed `G'`, a head tree byte-equal to old
      `G`, the two-manifest union, and no non-gate/release/pin change.
- [x] Derive those classifier SHAs only through the trusted workflow's closed event selector. For
      `pull_request`, require exact base repository/ref and authenticate `M=github.sha`; read exactly
      ordered parents `[F,J]` with second parent equal to event head, require event base `A`
      ancestor-or-equal to `F`, and select `F..M`. Both scanner and classifier use that pair for state,
      changed paths, and base-manifest reads, with `{A,F,J,M}` diagnostics; no moving API/main,
      merge-base, PR-head-only, or three-dot substitute is allowed. For
      `merge_group:checks_requested`, require payload base/head SHAs, exact base/head refs,
      `head_sha==github.sha`, the `gh-readonly-queue/main/` head prefix, and base ancestry. For
      protected-main `push`, require nonzero `before`, `after==github.sha==github.workflow_sha`, exact
      main ref, and base ancestry. Reject every other event or inconsistent/missing field before the
      classifier runs. `workflow_dispatch` is a separate credential-smoke path and invokes neither
      the classifier nor documentation scanner.
- [x] Write `run-build-container-gate.test.sh` before its driver. Test separate gate/subject
      roots, full-SHA checkout assertions, candidate helper substitution, symlink escape, dirty checkout,
      missing/duplicate completion markers, ordinary relevant, gate-update, gate-rollback, and
      explicit-not-applicable branches, pin deletion, mixed-change rejection, and propagation of every verifier
      failure.
- [x] Implement the driver so all authority comes from its own canonical gate root. The subject root is
      read/build input only. It never sources, executes, or resolves a helper from the subject. A
      relevant pin invokes `verify-build-container-publication.sh`; the network-free structural
      `check-build-container-publisher.sh` runs for every candidate, including ordinary
      not-applicable candidates, so workflow-namespace credential ownership cannot be bypassed by
      classification. Gate-update/rollback additionally run the inert candidate-context checks.
- [x] Before classification in every stable job, have the trusted driver enumerate the complete tree
      at event-selected subject head `T`; select every direct `.github/workflows/*.{yml,yaml}` and every
      `action.{yml,yaml}` basename anywhere; require paths to use only `[A-Za-z0-9._/+-]`; reject
      ambiguous, symlink, gitlink, or missing
      entries; extract exact regular blobs outside both repositories; and invoke only `G`'s
      `.github/actions/deploy-core/tests/check-action-pins.sh` with the complete explicit list. Test
      omitted/nested workflows, action metadata outside `.github`, deletion, malformed YAML, floating,
      prerelease, SHA, and Docker-tag refs, candidate scanner substitution, and not-applicable changes.
      Require the scanner's exact success line to report a positive external-reference count; consume
      it only to reject a vacuous/malformed scan and never pass it to another step. Any
      enumeration/extraction/parser/policy failure stops both stable jobs.
- [x] Create workflow contract tests before YAML. They require: - organization-rule events `pull_request` and `merge_group` with
      `merge_group` limited to `checks_requested`, local
      protected-main `push`, and manual `workflow_dispatch`; - no workflow-level path filter and stable jobs `build-container-local` and
      `build-container-pin` on every PR/merge-group candidate; - workflow permissions exactly `contents: read`, `actions: read`, and `pull-requests: read`, with
      no secret/environment/mutation token in either required job; - repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA` validated as a full SHA; - separate gate and subject checkouts with persisted credentials disabled; - all classifier, driver, completion, and verifier commands resolved under the gate checkout; - exact protected workflow repository/path parsing from `github.workflow_ref` and exact
      `github.workflow_sha` assertions for required runs; - explicit not-applicable execution and an unconditional terminal completion assertion; and - fixed steps `assert-exact-g-dispatch-context` and `assert-exact-main-push-context`; generic
      protected-main push assertions for event, ref, `event.after`, current head `Q`, workflow SHA,
      active gate SHA, and both latest-attempt job conclusions; literal `runs-on: ubuntu-24.04` and an
      unconditional context-derived `github-hosted`/`Linux`/`X64` bootstrap as the first executable
      step of every job before either checkout; no bootstrap `if`, `continue-on-error`, or masked
      failure; success-gated later non-cleanup steps; and separate release-request identity when
      `Q=S`. Negative fixtures cover absent/dynamic/wrong labels, missing/late/skipped/continued
      guards, failure masking, arbitrary non-cleanup always-run steps, and required recovery/
      reconciliation steps without exact guard-success and transition-marker conditions.
- [x] Implement `build-container-ci.yml`. For organization-required runs,
      `github.workflow_ref` must identify `stackpop/edgezero` and the exact path, and
      `github.workflow_sha` must equal repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA`, whose
      value is `G`. Push runs use local workflow SHA equal to current protected head `Q` while still
      executing gate code from that variable. Every job uses the literal runner label and fixed
      first-step bootstrap above. Each stable push job invokes the trusted context helper in exactly
      one named `assert-exact-main-push-context` step. Keep the existing path-filtered
      `deploy-action.yml` separate.
- [x] Wire all focused suites into `run.sh`. The protected gate runs its own tests; it never
      executes a candidate test script.

### 6.4 Release-policy verifier and publisher

- [x] Apply the design's common trusted-helper interface first: unique explicit nonsecret flags,
      environment-only named credentials, canonical regular-file structured inputs, absent create-new
      output paths, atomic no-replace publication, silent success, sanitized stderr, and exact exit
      statuses 0/1/2 for success/contract/usage-tooling failure.
- [x] Implement only these four prerequisite-auditor command shapes from design v6.40; reject every
      cross-mode, missing, duplicate, or extra flag before reading a credential:

```text
verify-release-prerequisites.sh configuration --gate-root <G-root> --gate-sha <G> --smoke-run-id <u64> --smoke-run-attempt <u32> --expected-app-id <u64> --expected-installation-id <u64> --expected-team-id <u64> --expected-bot-id <u64> --expected-bot-login <login> --policy-token-review-json <review> --policy-token-review-png <png> --administrator-bypass-png <png> --administrator-bypass-reviewer <login> --administrator-bypass-reviewed-at <UTC> --evidence-out <new-file>
verify-release-prerequisites.sh release --gate-root <G-root> --gate-sha <G> --candidate-pr <u64> --evidence-url <exact-candidate-PR-comment-URL> --source-revision <S> --merge-group-sha <sha> --merge-group-run-id <u64> --merge-group-run-attempt <u32> --smoke-run-id <u64> --smoke-run-attempt <u32> --push-run-id <u64> --push-run-attempt <u32> --expected-app-id <u64> --expected-installation-id <u64> --expected-team-id <u64> --expected-bot-id <u64> --expected-bot-login <login> --package-auditor-login <login> --package-state <absent|public-linked> --policy-token-review-json <review> --policy-token-review-png <png> --administrator-bypass-png <png> --administrator-bypass-reviewer <login> --administrator-bypass-reviewed-at <UTC> --evidence-out <new-file> --publisher-prerequisite-out <new-file>
verify-release-prerequisites.sh rotation-review --gate-root <old-G-root> --old-gate-sha <G> --new-gate-sha <G-prime> --dispatch-sha <Q_d> --final-head-sha <Q_f> --lock-run-id <u64> --lock-run-attempt <u32> --operator-login <login> --result <activated|rolled-back> --policy-token-review-json <review> --policy-token-review-png <png> --evidence-out <new-file> --approval-comment-out <new-file>
verify-release-prerequisites.sh rotation-complete --gate-root <active-G-root> --gate-sha <final-G> --lock-run-id <u64> --lock-run-attempt <u32> --policy-token-review-json <review> --policy-token-review-png <png> --evidence-out <new-file> --publisher-prerequisite-out <new-file>
```

      Read only `EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN`, plus
      `EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN` in `release` and owner-only mode-0600 key path
      `EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE` in `configuration`/`release`. Implement and test the exact
      canonical review schemas and mode-specific outputs from the design before API interpretation.

- [x] Require both the policy-token review PNG and administrator-bypass PNG to be canonical absolute-
      path regular non-symlink files of 8 through 10,485,760 bytes, beginning with exact signature
      bytes `89 50 4e 47 0d 0a 1a 0a`. Recompute each SHA-256; require the policy image digest to equal
      the canonical review's `screenshot-sha256` and record the administrator-bypass image digest in
      canonical audit evidence. The two PNG inputs remain distinct and non-interchangeable.
- [x] Produce `evidence-out` only in the trusted auditor as a nonempty 1..1,048,576-byte UTF-8 JSON
      object serialized by that gate revision as RFC 8785 JCS with no BOM, surrounding whitespace, or
      trailing LF. Bind the SHA-256 of those exact raw bytes in the separate small prerequisite record.
      The downstream writer treats the attachment as opaque bytes: it checks only size and raw digest,
      never parses audit members or derives machine state from its prose/detail fields.

- [x] Write fake-API tests before `verify-release-prerequisites.sh`. Implement the exact
      credential-specific method/path/query allowlists from design Section 8. Reject redirects,
      unvalidated placeholders, wrong credential use, any persistent mutation route,
      hidden/missing `bypass_actors`, and pagination failure before network or state interpretation.
      Every request must use exact `Accept: application/vnd.github+json`,
      `X-GitHub-Api-Version: 2026-03-10`, and `User-Agent: edgezero-build-container-gate/1`; reject a
      missing/different header. Every curl request must also use exact
      `--disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0`; timeout is
      terminal. Require response `X-GitHub-Api-Version-Selected: 2026-03-10`; a response body has
      parsed media type exactly `application/json`, with no charset or charset `utf-8`, and contains
      exactly one complete JSON value. GET requires 200. Approved token, release, and pull-request
      POSTs require 201 plus JSON. Approved release and pull-request PATCHes require 200 plus JSON.
      Token DELETE and the publisher-prerequisite variable PATCH require 204 and an empty body, with
      no JSON media-type requirement. Require a clean detached checkout at exact `G` before reading
      any credential.
- [x] Freeze list-query bytes to exactly these ordered forms; scalar-object GETs, selected run,
      approval and pull-request GETs, exact-ref GETs, and release-id GETs have no query:

```text
/orgs/stackpop/rulesets?per_page=100&page=<page>
/repos/stackpop/edgezero/rulesets?per_page=100&page=<page>
/repos/stackpop/edgezero/environments/build-container-release/deployment-branch-policies?per_page=100&page=<page>
/repos/stackpop/edgezero/actions/runs/<run-id>/jobs?per_page=100&page=<page>
/repos/stackpop/edgezero/actions/runs/<run-id>/attempts/<run-attempt>/jobs?per_page=100&page=<page>
/repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&page=<page>
/repos/stackpop/edgezero/commits/<candidate-sha>/check-runs?check_name=<approved-check-name>&filter=latest&app_id=15368&per_page=100&page=<page>
/orgs/stackpop/packages?package_type=container&per_page=100&page=<page>
/installation/repositories?per_page=100&page=<page>
/repos/stackpop/edgezero/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=<page>
```

      The deployment-protection-rules route is a scalar no-query GET whose single response must contain
      `total_count:0` and an empty `custom_deployment_protection_rules` array. `<approved-check-name>` is
      exactly `build-container-release-preflight`,
      `build-container-local`, or `build-container-pin`, always with `filter=latest` and
      `app_id=15368` in that byte order. Start at canonical page 1, synthesize only successive pages,
      and stop at 100 pages or 10,000 unique items. Never follow a response URL. Reject duplicate keys
      or items, repeated payloads, encoding aliases, fragments, unlisted query bytes, inconsistent
      `total_count`, premature/missing/duplicate/inconsistent RFC 8288 `Link` relations, `next` after a
      short or count-complete page, missing `next` before an incomplete declared count, and 100 items
      on page 100 as truncation. Endpoint-specific one-page contracts, including the publisher's exact-
      attempt two-job query, remain exactly page 1 and reject every continuation.

- [x] Require local `EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN` to be a short-lived fine-grained PAT
      owned by the verified active `stackpop` organization-owner login and selected only for
      `stackpop/edgezero` with repository Actions/read, Checks/read, Contents/read,
      Environments/read, Pull requests/read, Variables/read, Metadata/read, Administration/write and organization
      Members/read, Administration/write. A second operator records token id, resource owner, repository
      selection, expiry, exact displayed grants, screenshot digest, and every absent write surface in
      the design's literal canonical policy-token-review JSON shape. The helper does not claim GitHub
      can report the complete PAT grant set.
- [x] Require separate `EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN` to authenticate an active
      `stackpop` owner and report exactly normalized scopes
      `{read:org,read:packages}`. Reject byte-equal audit tokens before the first request.
- [x] Verify the environment's nonempty reviewer rule, `prevent_self_review=true`, final sole
      tag deployment policy `build-container-v*`, empty custom deployment-protection-rules endpoint,
      and supplied manual administrator-bypass
      evidence. Verify repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA=G`; the exact
      organization required-workflow descriptor bound directly to gate commit `G`, with no ref or
      bypass actor, exact repository-id/main-ref conditions, and `do_not_enforce_on_create=false`;
      and repository ruleset
      `edgezero-build-container-main` with
      target `branch`, active enforcement, no bypass, exact `main` include/no excludes, exact
      pull-request review fields, and merge queue `{timeout:60, ALLGREEN, build:1, merge:1, SQUASH,
min:1, wait:0}` from the design. Every missing, extra, defaulted, or changed semantic field
      fails. Require the immutable-releases endpoint to return HTTP 200, parse `enabled` as exact
      boolean `true`, require `enforced_by_owner` to be present as a boolean, and record its value;
      tolerate additional response fields. Require exact
      image tag rulesets `edgezero-build-container-tag-creation` and
      `edgezero-build-container-tag-immutability` for `refs/tags/build-container-v*`, plus action tag
      rulesets `edgezero-action-version-tag-creation` and
      `edgezero-action-version-tag-immutability` for `refs/tags/v*`; all have exact repository source,
      tag target, active enforcement, include, and no excludes. Each creation ruleset has only the
      creation rule and sole reviewed-team `always` bypass. Each immutability ruleset has no bypass
      and only deletion plus update with `update_allows_fetch_and_merge:false`.
      Require Repository ruleset `edgezero-build-container-pin-branches` with exact repository source,
      branch target, active enforcement, `refs/heads/edgezero-build-container-pin/*` include/no
      excludes, only the dedicated App Integration `always` bypass, and exact creation/update/deletion
      rule array.
- [x] Verify organization and repository Actions permissions both report
      `sha_pinning_required:false`; add their exact GET routes to the policy-token allowlist. Run a
      hosted exact-patch-version action fixture so any stricter enterprise override fails before
      publication.
- [x] Verify the candidate PR identity, final queue `merge_group` required-workflow run from
      `G`, and exact post-merge run/job API records for `S`. The run must have event `push`, exact
      workflow path, and `head_sha=S`; each stable job must have `head_sha=S`, succeed, and contain
      exactly one successful `assert-exact-main-push-context` step. The trusted step checks ref,
      `event.after`, current main head `Q`, `github.sha`, `github.workflow_sha`, and active gate internally because those
      values are not exposed by the run REST response. Missing/duplicate/wrong-attempt assertion steps
      fail. Matching check names from candidate workflow code are not evidence.
- [x] Verify protected-environment App/installation/team variables and private-key secret metadata.
      Use the local App key to authenticate the exact dedicated App and selected-repository installation
      with only contents/write, pull-requests/write, and implicit metadata/read. First mint
      `InstallationMetadataAudit` with exact body `{"permissions":{"metadata":"read"}}`, omitting
      both repository selectors. Require only metadata-read returned permission and complete,
      duplicate-free repository enumeration with `total_count:1` and only the verified EdgeZero
      id/full name. Its wrapper allows only repository enumeration and revocation. Revoke it
      successfully, then mint distinct `EdgeZeroPublisherProbe` with exact body
      `{"repository_ids":[E],"permissions":{"contents":"write","pull_requests":"write"}}`.
      The probe's wrapper allows only the specified enumeration, EdgeZero GET, and revocation.
      Neither token can cross wrappers or be stored in Actions. Reject broadened permissions,
      narrowed audit bodies, omitted permission bodies, extra repositories, duplicate/truncated/count-
      mismatched pages, and revocation failure. Include a two-repository installation whose bounded
      probe sees only EdgeZero: the installation audit must still reject it. Only these two local
      token creations/revocations are non-GET operations; persistent mutations remain forbidden.
- [x] Resolve `EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN` through the exact public user endpoint
      and require the response login, numeric id, and `type:"Bot"` to equal the independently reviewed
      repository variables. Reject a user/login collision before trusting pin-PR authorship.
- [x] Prove package absence only through the fully paginated verified-owner listing before first push.
      After first push, require public visibility and linkage to `stackpop/edgezero`. A 404 or
      authorization failure is never absence.
- [x] Emit canonical auditor evidence with every identity, rule id/URL, workflow/run/attempt/job URL,
      permission, package state, manual-evidence digest, and timestamp, but no credential. A separately
      authenticated operator first creates one inert placeholder comment on the candidate PR and
      supplies exact URL
      `https://github.com/stackpop/edgezero/pull/<candidate-pr>#issuecomment-<positive-u64>` to release
      audit. The auditor binds it but never mutates a comment. After audit, the operator edits that same
      stable comment to attach the exact output and byte-identical review PNG; final independent review
      precedes writer invocation. In post-merge mode also emit this exact closed, ordered small JCS machine record shape as a
      separate derived output:

```text
{"evidence-sha256":"sha256:<64-lowercase-hex>","evidence-url":null,"gate-sha":"<40-lowercase-hex>","previous-value-sha256":null,"rotation-history":{"state":"bootstrap-no-rotation"},"schema-version":2,"source-pr":null,"source-revision":null}
```

      The initial inert record shown above is created manually by a separate reviewed repository-
      variable POST procedure; the PATCH-only writer never observes or creates an absent variable.
      Every record submitted to the writer has `previous-value-sha256` equal to the SHA-256 of the
      exact prior variable UTF-8 bytes. Inert records have null `source-revision`, `source-pr`, and
      `evidence-url`. Release-bound records have exact `S`, candidate PR as a canonical positive-u64
      string, and the exact matching evidence-comment URL; the tuple is all-null or all-non-null.
      Same-source package recovery may refresh evidence digest and URL but preserves source PR. `rotation-history` is
      either the bootstrap object above or exactly
      `{"created-at":"<RFC3339-UTC>","evidence-sha256":"sha256:<64-lowercase-hex>","history-sha256":"sha256:<64-lowercase-hex>","run-attempt":"<canonical-positive-u32>","run-id":"<canonical-positive-u64>","run-number":"<canonical-positive-u64>","state":"verified"}`.
      For verified history, `rotation-history.evidence-sha256` is the same value as the first rotation-
      approval line's `evidence-sha256` and hashes only the exact compact JSON object bytes on the
      second approval line, excluding its protocol prefix and every newline.
      The record contains no `required-workflow-sha`. Its trusted local-auditor evidence requires
      `gate-sha` to equal both the live active-gate variable and live required-workflow descriptor SHA;
      the writer independently checks only the live active-gate variable, while the publisher checks
      the authenticated verified-rotation receipt's descriptor SHA. Bootstrap descriptor equality
      is an explicitly trusted manual administrative assertion, not a machine-authenticated or live
      Actions read. Retain exact rotation history so
      an old same-gate `S` record is stale after rollback. The opaque evidence attachment never contains
      this record or its digest. Test evidence-post failure and reject an `S`-bound value before exact-
      `S` push evidence exists.

- [x] Write `write-publisher-prerequisite.test.sh` before its helper. Cover exact byte-equal idempotence
      after local record validation as a mutation-free no-op that occurs before predecessor-hash
      evaluation and before PATCH. Every non-idempotent mutation must match the exact prior-value digest.
      Cover gate change only to an inert, verified-rotation record, with a greater selected run number
      when prior history is verified; same-gate inert to `S`; same-`S` evidence refresh; and forward `S`
      only when the old source is a Git ancestor. Every ordinary same-gate source transition preserves
      exact rotation history. Allow same-gate source clear or inert refresh only for post-rollback
      recovery. Bootstrap history may make its first such transition only to an inert verified record,
      without comparing absent predecessor rotation fields. For verified prior history, selected run
      number must increase, run id must differ, and both receipt and full-history digests must change.
      Require every non-null source to descend from
      the current gate. Reject an absent prerequisite variable, every other source clear or inert
      refresh, source regression, incomparable source, cross-gate source carry, stale same-gate rotation
      identity after rollback, malformed JCS, unknown/reordered/duplicate fields, mismatched evidence
      digests, unexpected prior value, wrong actor/grants, redirect/header/status failure, attempted
      predecessor/PATCH evaluation on no-op, and failed post-write readback. The helper runs only from clean detached active `G`, reads
      `EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN` only from its environment, authenticates the expected
      active organization member, and accepts only a short-lived fine-grained PAT selected for
      `stackpop/edgezero` with repository Variables/write, implicit Metadata/read, organization
      Members/read, and no other displayed grant. Require the design's literal canonical writer-token-
      review JSON shape and an independent final reviewer who recomputes the outer evidence digest and,
      only for verified nonempty rotation history, the nested receipt digest.
      Treat review `token-id` as screenshot-bound fine-grained-token inventory identity, never as
      `/user.id`. Require an operator-serialized single-writer interval from the first prerequisite GET
      through final readback; document that GitHub's unconditional variable PATCH is not atomic CAS and
      that a concurrent privileged writer is an accepted operational violation. Reject promisor/partial
      clones and object alternates and disable lazy Git fetches before credential use.
- [x] Give the writer only this exact interface:

```text
write-publisher-prerequisite.sh --gate-root <G-root> --gate-sha <G> --evidence-json <opaque-audit-attachment> --publisher-prerequisite-json <canonical-derived-record> --writer-token-review-json <canonical-review> --writer-token-review-png <png>
```

      It reads only `EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN`, emits no file or stdout, and treats
      `evidence-json` as opaque nonempty bytes of at most 1,048,576 bytes, recomputing only its raw
      SHA-256. Require `writer-token-review-png` to be a canonical absolute-path regular non-symlink file
      of 8 through 10,485,760 bytes with exact PNG signature `89 50 4e 47 0d 0a 1a 0a` and digest equal
      to the canonical writer review's `screenshot-sha256`. Its credential wrapper permits only the
      design's user, membership, active-gate-variable, and prerequisite-variable GETs; exact PATCH of
      the prerequisite variable with body
      `{"name":"EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE","value":"<JSON-escaped-exact-JCS-record>"}`;
      and post-write GET. Every GET, including the prerequisite-variable GET, requires 200 plus exactly
      one complete JSON value; missing/404 prerequisite state fails. PATCH requires 204 with an empty
      body; the readback value must equal the exact requested record. The writer is PATCH-only and cannot
      POST/create or DELETE a variable, write another name, parse the opaque audit attachment, or mutate
      any other endpoint.

- [x] Add `build-container-release-preflight` to the gate-owned CI workflow only for
      a `workflow_dispatch` body with `ref:"main"` at protected snapshot `Q`, with a required
      candidate PR-number, head-repository, and full head-SHA input. Its exact candidate-bound
      `run-name` is API-visible. It uses
      `environment: {name: build-container-release, deployment: false}`, declares literal
      `runs-on: ubuntu-24.04`, runs the fixed hosted/Linux/X64 bootstrap as its first executable step,
      checks out only exact `G`, runs no candidate code, contains exactly one fixed
      `assert-exact-g-dispatch-context` step that fetches the PR with the read-only token and compares
      all three inputs, exact workflow/ref identity, boolean ref protection,
      `github.sha==github.workflow_sha==Q`, active `G` pointers, `G` ancestry, and gate/context
      presence/mode/byte equality at `Q`, and
      uses the pinned App-token action only to prove the stored key can mint the exact repository-
      scoped token. The run API must later show event `workflow_dispatch`, exact path, `head_sha=Q`,
      and that successful named assertion step. Record `{G,Q}` separately; do not require whole-
      commit equality or recompute `Q` from moving main. The operator checks the dispatch contract
      before dispatch and the independent reviewer checks the actual run snapshot before approval.
- [x] Fixture-test the bounded credential smoke: temporarily add literal deployment branch policy
      `main`, dispatch with body `ref:"main"` under that `{G,Q}` contract, approve and complete the smoke, remove
      only `main`, and restore the sole tag policy. Any wildcard, caller branch, candidate
      commit, changed active gate, manifested-byte mismatch, or invalid dispatch identity invalidates
      the smoke. Test bootstrap `Q=G` and later `Q>G` with non-gate changes; captured snapshots do not
      change when main advances. Capture
      the pre-`S` administrator-bypass PNG only after final tag-only policy is restored.
- [x] Before dispatch, snapshot variables as `{name,value,created_at,updated_at}` and secret metadata
      as `{name,created_at,updated_at}` using the local policy auditor, never the key value. Require
      every `updated_at` strictly earlier than the smoke run's `created_at` and each record's creation
      no later than its update. Recompare all four records after smoke/policy restoration and at final
      audit. Bind candidate, `{G,Q}`, run/attempt/job ids, cutoff, and snapshots in evidence. Test equal
      cutoff, malformed times, key replacement after job start but before completion, post-smoke
      replacement, delete/recreate, changed variable value, and stale-attempt substitution. Changes
      require a fresh dispatch. Record job `completed_at`, never treat run `updated_at` as completion.
- [x] Write `release-approval-gate.test.sh`. Cover canonical valid comment; missing, duplicate,
      rejected, and bypassed reviews; wrong environment/reviewer/run id/run attempt/source/tag/PNG digest;
      wrong/missing/duplicate challenge and image digest; attempted future-attempt predeclaration;
      malformed/extra/reordered JSON; invalid calendar, fractional, offset, stale, or future time;
      API/non-200/malformed response; reused earlier-attempt
      evidence; and proof that token creation and every mutation have not run on failure.
- [x] Add partial-rerun fixtures: inherited `build-attempt:"1"` with current/API/comment attempt 2
      fails before minting; missing/zero/leading-zero/overflow attempts also fail. A rebuilt attempt 2
      with a new challenge and approval passes. Use real API JSON integer fields: positive u64 `id`
      and positive u32 `run_attempt`, canonically rendered without lossy conversion for comparison
      with string context/output/comment values. Reject API strings, fractions, zero, and overflow.
      Structural tests require the build step to capture
      its own context and the approval helper to compare it unconditionally before credential minting;
      an updater-derived attempt, late check, continuation, or failure masking is rejected.
- [x] Give the approval helper only this exact interface:
      `release-approval-gate.sh --gate-root <G-root> --gate-sha <G> --run-id <u64> --run-attempt
<u32> --build-attempt <u32> --source-revision <S> --release-tag <tag> --image-digest <D>
--approval-challenge <64-lowercase-hex> --approval-out <new-file>`. It reads only `GITHUB_TOKEN`,
      emits no stdout, and atomically writes the exact ten-field approval record for the updater.
- [x] Implement the gate with only `actions:read` and `contents:read` available. It
      performs only the exact two no-redirect GETs for the current run and its non-paginated approval
      history, requires complete valid 200 responses, and requires exactly:

```text
edgezero-release-evidence-v1 {"challenge":"<64-lowercase-hex>","image-digest":"<D>","png-sha256":"sha256:<64-lowercase-hex>","release-tag":"<tag>","reviewed-at":"<RFC3339-UTC>","run-attempt":"<canonical-positive-u32>","run-id":"<canonical-positive-u64>","source-revision":"<S>"}
```

      Run it from the verified `G` checkout; it must pass before App-token creation. First require
      context-derived build output `build-attempt` to equal current `github.run_attempt` and the
      canonical decimal rendering of API integer `run_attempt`; output/context/comment attempts
      are canonical positive-u32 strings, not the API field's JSON type. Require exactly one
      protocol-prefixed record claiming the current run id/attempt, exact current challenge and `D`, and
      approved state; any second or mismatched current-attempt record fails. The API reviewer login is
      the evidence approver. Earlier attempts never satisfy the current `github.run_attempt`.
      This is the sole 15-minute publisher-authorization check. The typed writer, pin validator, and
      later publication verifier validate timestamp grammar/calendar and exact authenticated comment
      equality but never compare archived evidence with a later wall clock.

- [x] Implement and fixture-test `update-image-pin-pr.sh` in the gate. Its only interface is:

```text
update-image-pin-pr.sh --gate-root <G-root> --gate-sha <G> --repository-root <clean-full-S-repository> --source-revision <S> --release-tag <tag> --image-digest <D> --provenance-protocol 1 --approval-json <gate-produced-file> --source-pr <u64> --evidence-url <exact-PR-comment-URL> --expected-bot-id <u64> --expected-bot-login <login>
```

      It reads only `EDGEZERO_BUILD_CONTAINER_APP_TOKEN`, accepts no caller-produced image or evidence
      JSON, validates the gate-produced approval, invokes `G`'s typed pair writer, and emits no file or
      stdout. The evidence URL is exactly
      `https://github.com/stackpop/edgezero/pull/<source-pr>#issuecomment-<positive-u64>`. Let `I` be the
      current protected-base source pin. Permit first pin, `I==S`, or `I` ancestor of `S`; reject older
      or incomparable `S`. Fully enumerate matching pin PRs and require the exact authenticated App
      author id/login plus head repository `stackpop/edgezero`; an actor/repository collision fails.
      Close older proposals when superseding them, make an older run a mutation-free superseded success,
      and fail malformed, incomparable, multiple-same-source, or ambiguous state.

- [x] Freeze the updater's App-token REST allowlist to
      `GET /users/<expected-publisher-bot-login>`,
      `GET /repos/stackpop/edgezero`, the exact fully paginated all-state/main-base pull-list query,
      selected `GET /repos/stackpop/edgezero/pulls/<pull-number>`,
      `POST /repos/stackpop/edgezero/pulls`, and selected
      `PATCH /repos/stackpop/edgezero/pulls/<pull-number>`. GET/PATCH require 200 plus exactly one JSON
      value; POST requires 201 plus exactly one JSON value. The branch is
      `edgezero-build-container-pin/<S>`, title is
      `chore(actions): pin build container for <S>`, and body is
      exactly this one line with no trailing LF:

```text
edgezero-build-container-pin-v1 {"evidence-url":"<exact-PR-comment-URL>","image-digest":"<D>","release-tag":"<tag>","source-pr":"<canonical-positive-u64>","source-revision":"<S>"}
```

      Create uses exactly
      `{"base":"main","body":"<JSON-escaped-exact-body>","draft":false,"head":"edgezero-build-container-pin/<S>","title":"chore(actions): pin build container for <S>"}`.
      Close uses exactly `{"state":"closed"}`. Reopen or metadata reconciliation uses exactly
      `{"base":"main","body":"<JSON-escaped-exact-body>","state":"open","title":"chore(actions): pin build container for <S>"}`.
      No other request body, PR mutation, issue/comment endpoint, or GraphQL call is permitted.
      Require exact `<app-slug>[bot]` login grammar and a matching public user object with the expected
      numeric id and `type:"Bot"`; installation tokens never call `/user`. Retain the protected
      workflow's exact installation-ID output check before updater use and require every final PR author
      to equal this bot identity.

- [x] Limit updater Git access to `https://github.com/stackpop/edgezero.git`; carry the token only through
      a mode-0700 private askpass helper, never argv or URL. Record remote `main` and pin-branch OIDs and
      classify all PR/branch/ancestry state before mutation. Create a mode-0700 private temporary clone
      from the supplied clean full repository, always fetch named `main` without tags or submodules,
      and fetch the named pin branch only when its recorded OID is nonempty. Require every fetched OID
      to equal the recorded value. For creation, repeat the exact remote-ref lookup and require absence
      immediately before the empty-lease push. Create a private detached worktree at recorded main.
      Invoke the typed pair writer into a separate private output directory; install only the two mode-0644 files,
      stage only `.github/docker/build-app-cli/{image.json,image-release-evidence.json}`, and create at
      most one unsigned, hook-free commit with exact PR-title message and fixed authenticated-bot
      identity. Push only
      `HEAD:refs/heads/edgezero-build-container-pin/<S>` using
      `--force-with-lease=refs/heads/edgezero-build-container-pin/<S>:<recorded-oid>` (empty OID only for
      creation), `--porcelain --no-verify`, no tags/submodules, and terminal prompting disabled. Require
      immediate remote-ref readback before PR mutation, then GET every mutated or selected PR and prove
      exact final author, repository, base, head, title, body, state, merge, and commit identities.
      Created, updated, already-current, already-merged, and mutation-free superseded outcomes must be
      proved from final remote/API state; already-current state performs no mutation.
      Permit an existing pin commit parent that is an ancestor of recorded current main and rebuild on
      that current main; reject incomparable parents. Require both existing record entries to be
      mode-0644 blobs. In same-source/different-digest replacement, verify the closed old PR at the new
      post-push branch OID before creating and verifying its replacement.
- [x] Test absent branch, exact remote OID, force-with-lease race, one open PR, closed-unmerged exact PR,
      already-merged exact record, same-`S`/new-`D`, forward source, older source,
      incomparable source, newer existing PR, stale PR after newer merge, missing head repository,
      wrong author/repository collision, conditional omission of an absent target fetch, target
      creation between absence recheck and empty-lease push, pagination/API/reopen failure, and
      idempotent rerun. No test may depend on the real network.
- [x] Write `check-build-container-publisher.test.sh` and then its network-free structural checker.
      Its exact CLI is `check-build-container-publisher.sh --gate-root <canonical-G-root>
--subject-root <canonical-subject-root> --gate-sha <G> --candidate-sha <T>`. It runs only from
      clean detached `G`, reads candidate workflows and the complete workflow-name set as Git blobs at
      `T`, emits no stdout, and never executes candidate content. Require candidate
      `build-container-ci.yml`, publisher, and rotation execution graphs to equal active `G`, except
      for the one exact next graph admitted by an active preparatory `G_p`. Every
      other workflow must use only literal environment names; reject dynamic environment selectors,
      case-insensitive use of `build-container-release`, and any case-variant occurrence of
      `EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY`. It rejects
      changes to the gate-owned publisher topology, permissions, concurrency group, action versions,
      gate checkout, helper paths, output set, token ordering, package deletion/admin scope, build secret
      exposure, missing/late release-state and rotation checks, predictable/hard-coded/pre-verification
      challenge generation, any job without literal `runs-on: ubuntu-24.04`, missing or late
      hosted-Linux/X64 bootstrap assertions, conditional or continued guards, masked failures,
      unguarded non-cleanup always-run paths, or pin mutation outside the trusted updater. Apply the same exact
      concurrency/group/queue checks to the gate-rotation workflow and reject any third workflow using
      that group or either approved workflow using a different one. Parse both workflows and require
      literal runner label and context-derived hosted/Linux/X64 first-executable-step bootstrap in
      every publisher and rotation job before checkout or any later step consumes protected
      environment data or credentials, creates an installation token, invokes Docker, calls a mutation
      API, or mutates the repository. Require those shell-backed first guards to bind step-local empty
      `BASH_ENV` and `ENV`; fixtures with a missing, inherited, nonempty, different, dynamic, caller-derived, or
      late label or assertion, guard `if`, `continue-on-error`, masked failure, arbitrary non-cleanup
      always-run step, or required recovery/reconciliation step lacking exact guard-success and
      transition-marker conditions fail. Do not claim that a step runs before GitHub resolves a
      job-level protected environment.
- [x] Write `verify-build-container-publication.test.sh` before its runtime verifier. Its exact CLI is
      `verify-build-container-publication.sh --gate-root <canonical-G-root> --subject-root
<canonical-subject-root> --gate-sha <G> --candidate-sha <T> --image-json
<extracted-image-record> --evidence-json <extracted-evidence-record>`. Require separate clean,
      full repositories at exact top levels, exact `G`/`T` checkouts, exact regular extracted blobs,
      no promisor/partial-clone configuration or object alternates, distinct object stores, and lazy
      Git fetching disabled,
      canonical records, and no stdout. Read only `GITHUB_TOKEN`; permit no mutation. Poll the bound
      run immediately at most 30 times, sleeping exactly 10 seconds only between incomplete successful
      identity-bound responses. Every request uses curl connect timeout 10 seconds and total timeout
      30 seconds; timeout is terminal. Never retry an HTTP/header/media/JSON/identity failure. Require
      raw run `path` exactly `.github/workflows/publish-build-container.yml@<release-tag>`. After
      completed/success, fetch the exact-attempt jobs endpoint once with only
      `?per_page=100&page=1`; require `total_count:2`, exactly two jobs, and exactly the successful
      `build-and-verify` and `update-pin` jobs, each with exactly one successful
      `assert-exact-publisher-context` step. Fetch non-paginated approvals once; parse every
      current-run/current-attempt protocol record, require exactly one approved
      `build-container-release` record, compare every field and reviewer login with the evidence, and
      ignore earlier-attempt reviews as inert. Repeat the exact run-detail GET after approvals and
      require the same immutable identity plus current completed/successful attempt; define that final
      read as the publication linearization point. Fixture candidate workflow names, PR bodies, and check names
      are not evidence. Cover pending exhaustion, no final sleep, endpoint substitution, redirects,
      response metadata, bare and UTF-8 JSON media types, malformed/duplicate jobs and steps,
      earlier/current attempt mixtures, a rerun beginning between approval and final detail reads,
      inherited xtrace/exported token aliases, partial/promisor repositories, object alternates,
      missing tools, and complete closed curl argv for every endpoint,
      rejected/bypassed/malformed/mismatched reviews, token leakage, and pre-network record failure.
- [ ] Create gate-owned `publish-build-container.yml`. It triggers only protected
      `build-container-v*` tags and uses exact concurrency group
      `edgezero-build-container-publication` with `cancel-in-progress: false` and `queue: max`.
      Every job declares literal `runs-on: ubuntu-24.04` and begins with the inline runner bootstrap.
      Document and test one running plus at most 100 pending runs; a run rejected at capacity publishes
      no pin and must be rerun. Run it through the exact structurally validated actionlint
      compatibility wrapper.
- [x] Write `verify-gate-rotation-lock.test.sh` before its helper. Implement only
      `verify-gate-rotation-lock.sh waiting --gate-root <old-G-root> --old-gate-sha <G>
--dispatch-sha <Q_d> --run-id <u64> --run-attempt <u32> --run-actor-login <login>` and
      `verify-gate-rotation-lock.sh publisher --gate-root <G-root> --gate-sha <G> --source-revision
<S> --publisher-prerequisite-json <canonical-record>`. Both modes read only `GITHUB_TOKEN`, emit
      no file or stdout, and use only their design-specific read-only route allowlists. Cover flag
      crossover, caller-supplied derived receipt fields, incomplete history, and mutation attempts.
- [ ] Create gate-owned `rotate-build-container-gate.yml`, dispatched only from protected main at
      snapshot `Q_d` while both pointers identify old `G`. Prove the design's protected dispatch
      identity, `G` ancestry, and complete gate/context presence/mode/byte equality; non-gate main
      changes are allowed. Both jobs retain captured `{G,Q_d}` and execute only captured old-`G`
      helpers, even after activation changes the gate variable. Use the same workflow-level concurrency
      contract. Every job declares literal
      `runs-on: ubuntu-24.04` and begins with the inline runner bootstrap. Its unprivileged acquire job runs
      after older publishers; its second job waits on secret-free
      `build-container-gate-rotation-lock` with `deployment:false`, parses the exact current-run
      rotation approval/evidence, and releases only after activated or rolled-back final state at
      `Q_f`, with `Q_d` ancestry and exact final manifested bytes/pointers. Implement the spec's
      exact two-line rotation approval, canonical receipt schema, and digest bytes. The local-only
      policy auditor repeats private variable/ruleset/environment reads after final restoration;
      its receipt binds current lock run/attempt, dispatch/final heads, observed gate/descriptor
      SHAs, enabled state, audit time, and full policy-evidence digest. The independent reviewer
      verifies the attached full audit and supplies the receipt through the authenticated approval
      comment. The waiting helper trusts that review for private-policy observations and uses only
      `actions:read`/`contents:read` GETs for current run, approvals, and exact main ref; it never
      calls private-policy endpoints or receives an audit PAT. Verify API reviewer/actor separation,
      current numeric API attempt, both receipt/head bindings, final Git tree, digest, ordered
      timestamps, and 15-minute freshness. Add valid activation/rollback fixtures and reject changed
      receipts, stale audits, duplicate/rejected reviews, head/attempt mismatch, variable API calls,
      and ambient `vars` substituted for the fresh local audit. A superseded protocol review
      requires a fresh lock dispatch. Name the single unconditional success-gating verification step
      exactly `assert-exact-rotation-context`; test continuation, skipped guards, and masked failures.
      The waiting-run policy receipt is not final publisher prerequisite evidence: do not require its
      own completion during receipt generation. Only after successful lock completion may the local
      auditor emit final evidence and the independent reviewer write an inert
      `EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE` value with `source-revision:null`, the final
      gate plus authenticated descriptor receipt, and exact completed `rotation-history`. Test this ordering without a circular
      prerequisite and reject carrying any earlier `S` across rotation.
      The comment binds `Q_f`; it never requires final main to
      equal old `G` or `Q_d`. Add a
      live prerequisite fixture proving a publisher queued behind the waiting lock starts no build or
      push. A canceled/expired lock may release concurrency only after the release-state variable and
      absent tag policy keep later publishers fail-closed.
- [ ] Split the publisher into `build-and-verify` and `update-pin`.
      `build-and-verify` has no environment, permissions only contents/read, actions/read, and packages/write,
      no App key, and after anonymous verification generates 32 OS-CSPRNG bytes, exposes the 64-lowercase-
      hex `approval-challenge`, and writes exact `{challenge,S,D,tag,run-id,run-attempt}` to its job
      summary. Capture that build step's context-derived attempt as canonical positive-u32 string
      `build-attempt`; outputs are only `{S,D,protocol,tag,approval-challenge,build-attempt}`.
      `update-pin` has the protected environment, no image build, initial
      actions/read and contents/read, and executes approval/updater helpers only from a separate checkout
      of repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA`, whose value is `G`.
      `build-and-verify` separately checks out `S` only to validate the isolated release request and
      manifested-byte equality, then uses `stage-build-context.sh` to build solely from copied `G`
      inputs. After acquiring shared concurrency and before registry login/build, require release state
      exact `enabled`; parse the step-local canonical
      `vars.EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE`; and require its non-null source to equal
      `S` and its gate to equal active `G`; verified history also requires the authenticated receipt's
      gate and required-workflow descriptor SHAs to equal `G`. Bootstrap descriptor equality is an
      explicitly trusted manual administrative assertion. The single-variable writer, not the publisher,
      recomputes the outer evidence digest; the publisher never follows an attachment or caller URL.
      Add a live fixture that queues a publisher behind rotation, changes all three
      variables while it waits, and proves it observes post-concurrency values or fails before build.
      Enumerate complete unfiltered rotation history; parse exact positive integer ids, run numbers,
      and attempts plus valid `created_at`; select the unique greatest documented per-workflow
      `run_number`, never id or `updated_at`. Serialize the exact current `{run-attempt,run-id,run-number}`
      array in ascending numeric run-number order and bind its SHA-256. Require selected run detail to
      reproduce listed identity/creation/current attempt. After validating detail/jobs/approval/main,
      repeat the complete enumeration byte-identically and repeat selected detail without drift. The
      second stable snapshot is the guard linearization point; work entering FIFO concurrency later is
      checked by the next publisher rather than claimed to atomically block this one. Bind its current
      API attempt, run number, complete-history digest, and the exact
      `rotation-history` prerequisite record. Require completed/success,
      its exact-attempt successful `assert-exact-rotation-context` step, matching approved receipt,
      final active gate/descriptor, and final-head ancestry/tree equality. Check receipt freshness at
      original verification-step completion, not publication time. Only reviewed bootstrap evidence
      plus empty history allows no rotation. Test failed/canceled/timed-out/skipped/waiting rotations
      with state still enabled, a later-numbered failure after older success, numeric-id/run-number
      disagreement, successful prior attempt followed by failed rerun, a later rerun of an older run id,
      changed history between passes, selected-detail drift, a changed or deleted previously recorded run,
      truncated pages, queued new rotation before and after the defined linearization point,
      and valid later publication. Record workflow-run deletion by a repository administrator as an
      accepted privileged risk: deleting a newer unrecorded failed run can make an older successful run
      appear latest and revive its otherwise matching record. No repository workflow receives run-
      deletion authority, and the contract does not claim this deletion remains detectable. The
      automatic retention removal of any previously snapshotted run changes the digest, blocks, and
      requires a fresh greater-numbered successful rotation plus reviewed record. The
      actions/read wrapper allows only the design's exact history/run/approval/
      attempt-jobs/main-ref GET routes; add these fixed routes and fixtures to the local policy auditor
      and record producer as well, including the exact `/repos/stackpop/edgezero/git/ref/heads/main`
      positive fixture and the policy PAT's `Contents:read` grant. No registry login, Docker build,
      or mutation can precede this guard;
      structural tests reject missing actions/read, extra grants, conditional/late guards, and masks.
      A candidate Dockerfile
      or unmanifested path is never in the Docker context. Each job invokes exactly one fixed
      `assert-exact-publisher-context` step from `G` before sensitive work and verifies tag event/ref,
      `github.sha==github.workflow_sha==S`, run id/attempt/tag, active gate, and enabled release state.
- [x] Pin checkout to `actions/checkout@v7.0.1` and App-token creation to
      `actions/create-github-app-token@v3.2.0`.
      The App token is requested only after approval-gate success and only for repository `edgezero` with explicit contents/write
      and pull-requests/write. Require returned installation id to equal the protected variable and rely
      on mandatory post-step revocation.
- [x] Add a static workflow test rejecting package-delete endpoints, `delete:packages`,
      package-admin tokens, cleanup jobs, candidate helper execution, and any token before anonymous
      image verification.

### 6.5 Land and configure `G`

- [ ] Run all Task 0-2 focused tests, Rust checks, the pinned actionlint `1.7.12` compatibility
      wrapper, shellcheck, and
      `zizmor --offline` from a clean checkout. Run every current format/test CI matrix job.
- [ ] Obtain an independent security review of the exact gate-owned path manifest, workflow source,
      API allowlists, App-token ordering, and fail-closed/no-op markers.
- [ ] Merge the gate-only PR through the repository's existing protected process. Record the exact
      default-branch commit as `G`. This bootstrap is a human-reviewed trust-root operation;
      candidate-controlled checks are not evidence for it.

**Gate checkpoint:** stop before opening the source candidate.

- [ ] Set repository variable `EDGEZERO_BUILD_CONTAINER_GATE_SHA` to full SHA `G` while
      `EDGEZERO_BUILD_CONTAINER_RELEASE_STATE` is absent or otherwise not `enabled`. Configure secret-free
      environment `build-container-gate-rotation-lock` for protected main only, with required
      reviewers, self-review and administrator bypass disabled, no custom protection App, and no
      workflow reference to any environment secret or variable.
      Set repository variables `EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID`,
      `EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID`, and
      `EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN` to the independently verified dedicated App and
      bot identity; require the App id to equal the protected-environment App id and pin-branch
      ruleset Integration actor. Configure active organization ruleset
      `edgezero-build-container-required-workflow` with target
      `branch`, no bypass actors, repository-id condition exactly `[<edgezero-id>]`, ref-name include
      exactly `["refs/heads/main"]`, no ref excludes, and exactly one `workflows` rule containing
      `do_not_enforce_on_create=false` and exactly one required-workflow descriptor
      `{repository_id:<edgezero-id>,path:".github/workflows/build-container-ci.yml",sha:G}`,
      with no `ref`. Enable repository immutable releases, then require the versioned REST endpoint
      to return HTTP 200 with `enabled` exactly boolean `true` and `enforced_by_owner` present as a
      boolean; record both fields without requiring a byte-exact or one-field JSON object.
- [ ] Configure repository ruleset `edgezero-build-container-main` at target `branch`, enforcement
      `active`, no bypass actors, include exactly `refs/heads/main`, exclude none, and exactly two
      rules. Its pull-request rule allows only squash, dismisses stale reviews, requires code owners,
      requires a distinct last-push approval, two approvals, and resolved threads. Its merge queue is
      exactly `check_response_timeout_minutes:60`, `grouping_strategy:ALLGREEN`,
      `max_entries_to_build:1`, `max_entries_to_merge:1`, `merge_method:SQUASH`,
      `min_entries_to_merge:1`, and `min_entries_to_merge_wait_minutes:0`. Add fake payloads for every
      missing/wrong parameter and prove each fails. Configure both image-tag and action-version-tag
      creation-only/team-bypass and update/delete/no-bypass ruleset pairs with exact names, repository
      source, tag target, ref include/no-exclude conditions, rule arrays, and actors; protected environment; dedicated GitHub
      App; App-only canonical pin-branch ruleset; publisher App/bot repository variables; audit
      identities; and final package policy from design Section 8.
- [ ] Only after every preceding control and the required-workflow descriptor are live and verified,
      require publisher-prerequisite variable absence. Through the separate manually controlled and
      reviewed repository-variable POST procedure, create the canonical inert record with
      `previous-value-sha256:null`, `gate-sha==G`, empty-history bootstrap state, and
      `source-revision:null`; read it back exactly before enabling later PATCHes. The writer does not
      participate in bootstrap and must require its prerequisite GET to return 200. The record has no
      `required-workflow-sha` member; require the trusted local auditor and independent reviewer to
      confirm live equality among its `gate-sha`, the active-gate variable, and required-workflow
      descriptor immediately before and after the write. The writer itself checks only the active-gate
      variable and does not parse the opaque audit or query organization rulesets. Later changes use
      only the local single-variable writer; its PAT is never stored in Actions. Set
      `EDGEZERO_BUILD_CONTAINER_RELEASE_STATE` to exact `enabled` only after that final readback and all
      control readbacks succeed, then verify it once more before running configuration preflight.
- [ ] From a clean detached checkout at exact `G`, run the prerequisite verifier in configuration-only mode and preserve independently reviewed
      evidence for the exact ruleset payload, repository variable, merge queue, environment, App,
      audit-token screens, and team. Reopen or synchronize the later source PR after activation so its
      authoritative required-workflow run is not stale.

### 6.6 Prove gate rotation and recovery before release

- [ ] Exercise the required two-rotation evolution path for any protected workflow, exact
      image-context/Dockerfile contract, or hard-coded tool version/checksum change. First land and
      activate a preparatory gate `G_p` that leaves current protected workflows and image-context
      bytes unchanged while its trusted validators accept exactly the current shape and one reviewed
      next shape. Do not open a release request or create a release tag from `G_p`. Then land exact
      `G_n`, collapse validation to only the new shape, and perform a second complete rotation whose
      old gate is `G_p`. Preserve separate evidence and prerequisite transitions for both rotations;
      reject generic ranges, unbounded dual acceptance, combining preparation and payload, publishing
      between rotations, or abandoning `G_p` without the ordinary rollback protocol.
- [ ] Add fixture/integration tests for a gate-update PR. Old `G` must require the protected base tree
      to match old `G` only on manifested paths, classify exactly `mode=gate-update`, accept changes only in the union of old and
      candidate manifests, validate canonical candidate manifest and CODEOWNERS coverage as inert data,
      reject mixed release-request/pin/non-gate changes, and never execute a candidate helper.
- [ ] Exercise the activation state machine with fake configuration APIs: quiesce publication
      by dispatching at protected `Q_d` using old-`G` code and waiting until it holds publication concurrency;
      set release state `disabled:<lock-run-id>:<old-G>`, remove the sole tag deployment policy, merge
      through the one-entry queue, record `G'`, run the clean
      detached `G'` suite and exact post-merge assertion, update the repository variable and
      organization descriptor after updating the marker with `G'`, verify both plus the base manifested
      tree, restore and locally verify tag policy, set state enabled, emit a fresh policy receipt,
      and approve the lock with the exact evidence-bound comment. Only after successful lock
      verification refresh the publisher prerequisite record. Every intermediate pointer mismatch, publisher that starts
      behind the lock, or malformed state/comment fails closed.
- [ ] Repeat the full sequence after `G -> S -> B` and after later non-gate commits, including
      credential-smoke refresh at `Q != G`, activation to `G'`, final `Q_f != Q_d`, and rollback to a
      new main commit rather than back to old `G`. Reject changed gate bytes, pointer disagreement,
      wrong dispatch context, and helper reselection after the gate variable changes.
- [ ] Exercise rollback at every activation step. Release stays disabled; both pointers restore to old
      `G`; ordinary work remains blocked while base contains `G'`; and a separately reviewed old-`G`
      `mode=gate-rollback` must restore the manifested tree before release policy returns. Require
      current base to be a valid failed `G'` tree, proposed manifested bytes to equal old `G`, diff to
      stay within the two-manifest union, no release/pin/non-gate change, both pointers already old
      `G`, release disabled, queue merge, and generic exact-head push evidence. If old `G` cannot
      validate either side, require a new manual trust-root review with release disabled. Add fixtures
      for every interrupted pointer/policy state. No bypass or mixed-pointer operating mode is
      permitted. Restore presence, Git mode, and bytes across the old/new manifest union, including
      required absence of newly introduced paths; preserve unrelated release, pin, and app files.

**Gate:** do not tag or publish from `G`. The next task cannot alter a gate-owned path.

## 7. Task 3: Build, merge, and publish source revision `S`

**Source-candidate files:**

- Create only `.github/docker/build-app-cli/release-request.json`.
- No gate-owned, image-context, pin, or unrelated file changes. The Dockerfile, complete context,
  publisher, and all credential-bearing helpers already exist at `G`.

### 7.1 Qualify the isolated release request

- [ ] Create byte-canonical RFC 8785 JCS with no trailing newline. The file contains this one data
      line; the Markdown fence line break is not file content:

```text
{"gate-sha":"<G>","provenance-protocol":1,"release-tag":"build-container-v<positive-decimal>"}
```

- [ ] From a clean detached `G`, use the gate-owned staging helper to create the canonical fresh
      context, build the local image with the exact Dockerfile/arguments and revision label `G`, and
      capture local image/config identity `L` through BuildKit `--iidfile`. Run the full gate-owned
      leaf-platform, label, protocol, validator, and toolchain verifier against `L`. This bootstrap
      image is local only, is not published, and is not the future digest `D`; no preexisting pinned
      `G` image is assumed.
- [ ] Invoke only `write-release-request` by immutable local identity `L`, under the exact
      networkless/credential-free/read-only `release-request-write` profile, with full active `G` and
      the next never-moved tag. Copy its sole output into the candidate branch and remove its fresh
      output parent. Test duplicate/extra/reordered keys, whitespace/newline, wrong G/protocol/tag,
      leading-zero release number, existing output, cleanup failure, wrong/tagged local image, and any
      second changed path. No shell or `jq` is a second canonical encoder.

- [ ] Require the organization workflow at exact `G` to run both stable jobs. Verify its workflow
      repository/path/SHA and candidate head SHA. The local job must verify the candidate's complete
      gate/context tree equals `G`, stage context only from clean `G`, build with the candidate head as
      revision label, and run the trusted verifier suite. Candidate Dockerfile, helper, and context
      paths are never read.
- [ ] Require linux/amd64 leaf config, exact labels, checksum-pinned Fastly/sccache bytes, exact
      installed command versions/paths, actual wasm compile, validator self-test, deterministic
      archive bytes, every malformed fixture, controlled startup-loader behavior, and read-only/non-
      root runtime with no network/capabilities and bounded memory/pids.
- [ ] Run the exact `write-expected`, package, validate, and binary-smoke mount profiles from
      design Section 5 with fixed `/usr/bin/env`, exact sorted placeholder argv, and only each profile's
      post-`env -i` target environment. No shell or `jq` may produce expected identity. No
      `/work/package` mount may appear.
- [ ] Take the exact pre-dispatch credential metadata snapshots and verify protected `{G,Q}` dispatch
      identity/ancestry/manifest equality. Execute the bounded credential smoke: temporarily add literal
      `main` as the sole extra environment deployment policy, dispatch with body `ref:"main"` and the
      candidate PR number, exact head repository, and current full head SHA, approve and complete the
      smoke, then remove `main`. Require the exact candidate-bound `run-name`, run event
      `workflow_dispatch`, path, `head_sha=Q`, and exactly one successful
      `assert-exact-g-dispatch-context` step that resolves the PR API and compares all three inputs;
      then require final tag-only policy, exact snapshot equality, and credential updates strictly
      before run `created_at`. Record `{G,Q}` and exact run/attempt/job evidence. A new candidate
      commit, changed active gate, or credential change makes this evidence stale.

### 7.2 Qualify and merge `S`

- [ ] After policy restoration, an independent maintainer captures the administrator-bypass/final-policy
      PNG. Enter the candidate in the mandatory merge queue. Require the final `merge_group`
      execution of the organization workflow from `G` to pass. The queue payload must still have
      single-entry build/merge limits and every exact review parameter.
- [ ] Merge only through that queue. Record the resulting default-branch full SHA as `S`.
      Wait for the repository-local `build-container-ci.yml` latest-attempt push run. Its run API
      record must have event `push`, exact path, and `head_sha=S`; both stable jobs must have
      `head_sha=S`, success, and exactly one successful `assert-exact-main-push-context` step. The
      immutable step internally proves `refs/heads/main`, `event.after==github.sha==S`,
      `github.workflow_sha==S`, and active gate `G`.
- [ ] From a clean detached checkout at `G`, run the full release-prerequisite verifier after merge using the candidate PR number and exact
      `G`/`S`. Attach canonical evidence and the byte-identical PNG to the merged PR.
      A maintainer other than the verifier and screenshot reviewer recomputes the digest, reviews all
      API evidence, writes the exact `S`-bound publisher-prerequisite variable through the local
      single-variable writer, verifies readback, and only then authorizes tag creation.

**Release checkpoint 1:** no tag exists until exact-`S` push evidence and the three-person
preflight review pass.

### 7.3 Publish digest `D` and propose the pin

- [ ] A preflight-verified active member of the creation ruleset's sole bypass team
      `edgezero-build-container-releasers` creates protected tag
      `build-container-vN` at exactly `S`.
- [ ] `build-and-verify` checks out full history without persisted credentials; proves
      `HEAD==S`, clean tracked/index/untracked/submodule state immediately before and after
      release-request validation. It separately checks out clean exact `G`, proves every manifested
      byte at `S` equals `G`, and stages a fresh context containing only canonical files copied from
      `G`. Build with that context and gate-owned Dockerfile, `--platform linux/amd64`, exact
      source/protocol args,
      `--provenance=false`, `--sbom=false`, and BuildKit metadata output.
- [ ] Authenticate GHCR only through a fresh `DOCKER_CONFIG` outside build context. Pipe
      `GITHUB_TOKEN` to login; never pass it as build arg, secret mount, image environment, or
      context file. Parse `D` only from
      `containerimage.digest` in the metadata file.
- [ ] Run trusted `G` verification while authenticated, remove local credentials/reference,
      then pull and run `repository@D` with a new empty Docker config. The anonymous check must
      issue a registry request. A private package fails before `update-pin`.
- [ ] On first publication, stop at the expected private-package failure. An operator makes the GHCR
      package public, then reruns the exact `G` prerequisite verifier in package-present mode and
      attaches its public-visibility/repository-link evidence. The independent reviewer replaces and
      reads back the same-`S` publisher-prerequisite variable before rerunning the same tag. Evidence
      from the failed run attempt does not carry forward. Rerun the build and downstream job, not
      only the failed pin job; every qualifying attempt produces a fresh challenge.

**Release checkpoint 2:** after the image is public and anonymously verified, the environment approver
captures a fresh PNG and enters the exact design comment for the current
`{challenge,D,run-id,run-attempt,S,tag,png-sha256,reviewed-at}` before approving `update-pin`.

- [ ] Check out exact `G` without persisted credentials, then require `release-approval-gate.sh` to
      pass before App-token minting, including build-output/current/API/comment attempt equality.
      Then mint the exact repository-scoped App token, verify installation id, and invoke only
      `G/update-image-pin-pr.sh`.
- [ ] Generate the exact five-field image record plus canonical JCS release-evidence record through
      gate-owned typed writers. Use branch `edgezero-build-container-pin/<S>` and exact title from the
      design. Enforce protected-base and every-open-PR source ancestry. Use recorded remote OID and
      exact force-with-lease; never blind force or accept caller-provided evidence bytes.
- [ ] Use the design's exact single-line `edgezero-build-container-pin-v1` PR body containing only
      `evidence-url`, `image-digest`, `release-tag`, `source-pr`, and `source-revision`. Keep protocol,
      platform, anonymous-verification result, and approval run-attempt evidence in the canonical
      release-evidence record rather than duplicating them in the PR body. Never include credential
      material or an AI byline.
- [ ] Require pin CI from protected `G` to recompute current-base ancestry; verify both files are the
      only changed paths and agree; verify exact App bot author/id, head repository, protected branch,
      PR title, and pin-branch ruleset; then invoke `verify-build-container-publication.sh` to poll and
      verify the bound publisher run, attempt, both exact context steps, and current approval comment
      against the evidence file. Only afterward
      anonymously verify the exact digest against labels, platform, tools, validator, target,
      protocol, and package visibility. Candidate scripts, PR-body claims, and mutable tags are
      forbidden.

**Release checkpoint 3:** confirm the pin PR changes only the image/evidence pair, was authored and
pushed through the dedicated App's protected source branch, binds the successful publisher run and
approval, proposes a source not older or incomparable with the current base or another proposal, and
passed both protected jobs on its final merge-queue candidate.

## 8. Task 4: Merge and verify pin baseline `B`

- [ ] Merge the pin-only PR through the mandatory queue and record the resulting full commit as
      baseline `B`, not final action revision `P`.
- [ ] Confirm the App push caused the required organization workflow to materialize and every latest
      attempt passed. Confirm deletion or a syntactically valid but unverifiable/older/incomparable pin
      fails in dedicated test PRs.
- [ ] From a clean checkout at `B`, rerun all Task 0-3 suites:

```bash
test -z "$(git symbolic-ref -q HEAD || true)"
test "$(git rev-parse HEAD)" = "$B"
test -z "$(git status --short --untracked-files=all --ignore-submodules=none)"
test "$(awk '$1 == "rust" { print $2 }' .tool-versions)" = "$(rustc --version | awk '{ print $2 }')"
tools=$(mktemp -d)
trap 'rm -rf "$tools"' EXIT
INSTALL_DIR="$tools" scripts/install-yq.sh 4.53.3
INSTALL_DIR="$tools" ACTIONLINT_VERSION=1.7.12 scripts/install-actionlint.sh 1.7.12
cargo install zizmor --version 1.16.3 --locked --root "$tools"
export PATH="$tools:$tools/bin:$PATH"
test "$(awk '$1 == "nodejs" { print $2 }' .tool-versions)" = "$(node --version | sed 's/^v//')"
npm --prefix docs ci --ignore-scripts
cargo test --locked --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml
cargo fmt --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path .github/tools/edgezero-provenance-validator/Cargo.toml \
  --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-targets
cargo check --locked --workspace --all-targets --features "fastly cloudflare spin"
cargo check --locked -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
bash .github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.sh
scripts/run-actionlint.sh
git ls-files -z '*.sh' | xargs -0 shellcheck -S warning
zizmor --offline .github/workflows .github/actions
npm --prefix docs run format
npm --prefix docs run lint
npm --prefix docs run build
./scripts/check_no_placeholder_pins.sh
./scripts/check_no_legacy_typed_reads.sh
cargo run --locked -q --bin check_no_nested_app_config --features nested-app-config-check -- \
  examples/app-demo crates/edgezero-cli/src/templates
cargo test --locked -p edgezero-cli --features nested-app-config-check --bin check_no_nested_app_config
cargo test --locked -p edgezero-adapter-fastly --all-targets --features cli
cargo test --locked -p edgezero-cli --test generated_project_builds -- --ignored
cargo clippy --locked -p edgezero-adapter-fastly --features cli --all-targets -- -D warnings
cargo clippy --locked -p edgezero-adapter-fastly --no-default-features --lib -- -D warnings
cargo fmt --manifest-path examples/app-demo/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path examples/app-demo/Cargo.toml \
  --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path examples/app-demo/Cargo.toml --locked --workspace --all-targets
test -z "$(git status --short --untracked-files=all --ignore-submodules=none)"
```

- [ ] Require every current hosted format/test matrix job, including all wasm clippy/test legs. Local
      commands do not replace runner-backed checks.
- [ ] Pull `repository@digest` anonymously after merge and rerun the complete image verifier
      from the committed record.
- [ ] Record `{G,S,D,B,protocol,tag}`, workflow/ruleset ids, action versions plus their reviewed
      resolved commits, tool checksums, and
      evidence digests in the release record.

**Gate:** downstream plans build on `B` and never use `S` as an action ref or derive
a digest from the mutable tag.

## 9. Task 5: Runbook and downstream-plan handoff

- [ ] Document publication concurrency accurately: one running and at most 100 pending runs under
      `queue: max`; a run rejected at capacity publishes no pin and must be rerun.
- [ ] For every `update-pin` attempt, record run id, run attempt, exact approval comment, API
      reviewer login, challenge, image digest, source, tag, review time, PNG basename/digest, and final evidence attachment.
      Earlier-attempt or pre-`S` evidence is invalid. Require producer-derived `build-attempt` equality
      before minting; partial reruns that inherit an older build fail and require a full build rerun.
- [ ] Record each rotation's `{old-G,Q_d,new-G,Q_f}` and preserve image-release gate history
      separately. Credential-smoke evidence records `{G,Q}`, exact pre-dispatch metadata snapshots,
      run creation cutoff, and final unchanged metadata; neither current main nor completion time
      substitutes for these bindings.
- [ ] Document that GHCR has no enforceable per-version retention lock and repository automation has no
      package-deletion credential or endpoint. Manual administrator deletion is accepted operational
      risk; recovery is a new source/image verification/pin release.
- [ ] Document rollback as selecting an earlier reviewed exact action version containing its
      corresponding pin.
      Never regress protected-main `image.json` and never move a release tag.
- [ ] Review and approve these four sibling plans before implementing post-`B` behavior:
      `2026-08-20-build-cache-actions.md` (cache key, restore/save authorization truth table,
      sccache lifecycle/audit), `2026-08-20-build-cache-provenance.md` (typed expected producer,
      package/validate handoff and two-job topology),
      `2026-08-20-build-cache-launcher-providers.md` (container profiles, source freeze,
      nested-project Fastly `bin`/`pkg` output and cleanup, provider lifecycle), and
      `2026-08-20-build-cache-consumer-adoption.md` (consumer workflow, docs, migration, and
      final `P`/`V` qualification plus documentation revision `R`).
- [ ] Each follow-on plan must assign the design's deferred fixtures and tests before final action
      revision `P`: workspace/suffix vectors, format-independent cache tree bounds and entry count,
      exact cache action versions, cache lookup/save truth tables, sccache response-loss semantics,
      seven-day expiry as documented behavior, arbitrary app-written regular cache data under the
      disclosure acknowledgement, both fixed host cache paths and overlap cleanup, container-only
      metadata preflight, mount/environment matrices, empty Cargo-config policy, path confinement, implicit nested
      `bin`/`pkg` ownership and cleanup, exact app-env allow/deny boundaries, controlled-loader argv,
      artifact identity, and consumer recomputation.
- [ ] In the consumer plan, keep the four prepublication documents at the gated placeholder while
      candidate `H` is tested through exact version `C`. Designate `P` only after the exact-main local
      suite and immutable candidate-version hosted suite pass, then select and publish immutable `V`
      at `P`. Only after the literal-`V` smoke passes, merge documentation-only `R` adding the exact
      `{V,P}` record and replacing every placeholder with `V`; the preinstalled dual-state scanner
      then remains permanently in released mode. Every concrete third-party and final EdgeZero
      `uses:` ref is a reviewed exact patch version; no major/minor tag, branch, or commit SHA appears.

## 10. Completion review

Before declaring this plan complete, run two independent reviews:

1. **Contract review:** compare every file and test with design v6.40 Sections 2 through 10. Verify one
   expected/package/validate wire authority, protected gate and image source `G`, isolated release
   request `S`, staged gate-only Docker context, API-visible exact post-merge `S` proof, forward-only
   pin ancestry, no tag runtime pull, no placeholder image digest/checksum, no `/work/package`
   convention, fixed GNU `env -S` closed-environment launch, literal `ubuntu-24.04` job selection,
   first-step hosted-Linux/X64 assertions, and no legacy `--stage` guidance.
2. **Release-adversary review:** test candidate workflow/helper substitution, forbidden
   major/minor/branch/SHA action refs, third-party exact-tag movement as recorded risk, immutable
   EdgeZero release enforcement, private
   package state, stale/older/incomparable pin PRs, malformed BuildKit metadata, indexes, wrong
   platform/labels/versions/protocol, deleted pin, classifier/completion failure, workflow-source
   mismatch, missing bypass fields, API redirect/path/header/version confusion, approval reruns, token
   ordering, gate rotation/recovery, actionlint queue compatibility, publication queue overflow, and
   concurrent release attempts.

The container plan is complete only when protected gate `G`, source `S`, verified
digest `D`, and pin baseline `B` are recorded and all repository gates pass. The
four remaining plans may then implement caching and designate final action revision `P` only
after their complete contract suites pass.
