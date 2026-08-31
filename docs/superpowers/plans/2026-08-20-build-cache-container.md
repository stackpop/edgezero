# Build-Cache Container Implementation Plan (plan 1 of 4)

> **Execution:** Use `superpowers:subagent-driven-development` or
> `superpowers:executing-plans`. Follow the tasks in order and stop at every release checkpoint.

**Goal:** Publish and pin a public, leaf `linux/amd64` runtime image containing the exact EdgeZero
build/deploy toolchain and the trusted provenance validator required by build caching.

**Architecture:** Source revision `S` builds the image from the repository root. The publish workflow
captures and verifies immutable digest `D`, proves anonymous access, and opens an idempotent PR adding
`image.json`. That pin plus its permanent gate forms baseline `B`. The remaining feature plans land on
top, and their final passing action revision `P` contains the unchanged `{D, S, protocol}` record.
Consumers pin all EdgeZero actions and reusable workflows to full SHA `P`.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.18.

**Tooling:** Rust, Docker BuildKit/buildx, GHCR, GitHub Actions, Bash 3.2, `jq`, `gh`, `actionlint`,
`shellcheck`, and `zizmor`.

## 1. Non-negotiable contracts

- Rust is the exact version in `.tool-versions` (`1.95.0` at plan time).
- Fastly CLI is the exact version/checksum in `.github/actions/deploy-fastly/versions.json`
  (`15.1.0` at plan time).
- sccache is exactly `0.10.0`, fetched from its release artifact and checksum-verified.
- The base image uses a real `sha256` digest. No placeholder digest or checksum is committed.
- The final image is a leaf `linux/amd64` image manifest, not an OCI index.
- The final image contains an installed `wasm32-wasip1` target, not merely a rustc target-list entry.
- The project-owned validator, schema, and capability fixtures are baked and tested before push.
- Runtime is non-root uid/gid 1001 and works with a read-only root filesystem plus explicit tmpfs.
- Every non-local external action and reusable workflow ref is a full lowercase 40-hex commit SHA.
  Docker image refs use immutable `sha256` digests. Local `./...` actions remain local refs.
- Bash scripts are Bash 3.2-compatible and `shellcheck -S warning` clean. CI helper scripts do not use
  Python. No AI bylines appear in commits or PRs.
- Publication never records a digest before the image passes authenticated verification and a clean,
  anonymous pull by digest.

## 2. Dependency order

Although this is plan 1 of the feature set, its image task cannot run first. Execute these gates:

1. Land the trusted validator contract and capability fixtures (Task 0).
2. Land the repository-wide full-SHA policy migration (Task 1).
3. Implement image pinning, the Dockerfile, publisher, local-image CI, and pin-change CI (Tasks 2-4).
4. Merge all pre-publication code and tests; record that exact full commit as source revision `S`.
5. Run the already-landed publisher at `S`, verify digest `D`, and merge its required-check pin PR to
   create baseline `B` (Tasks 4-5).
6. Execute the cached-build, provenance integration, launcher, and consumer plans on `B`; their final
   passing commit becomes action revision `P`.

Do not publish a provisional image without the validator. Do not use a placeholder `image.json` to
break the dependency cycle.

## 3. Planned file surface

Create:

- `crates/edgezero-provenance-validator/Cargo.toml`
- `crates/edgezero-provenance-validator/src/{main,json_contract,archive,elf,extract}.rs`
- `crates/edgezero-provenance-validator/tests/cli.rs`
- `.github/docker/build-app-cli/provenance.schema.json`
- `.github/docker/build-app-cli/fixtures/provenance/**`
- `.github/docker/build-app-cli/fixtures/wasm-smoke.rs`
- `.github/docker/build-app-cli/Dockerfile`
- `.dockerignore`
- `.github/docker/build-app-cli/verify-toolchain.sh`
- `.github/docker/build-app-cli/verify-published-image.sh`
- `.github/docker/build-app-cli/update-image-pin-pr.sh`
- `.github/actions/deploy-core/tests/verify-toolchain.test.sh`
- `.github/actions/deploy-core/tests/verify-published-image.test.sh`
- `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`
- `.github/actions/deploy-core/tests/check-doc-action-pins.sh`
- `.github/workflows/publish-build-container.yml`

Created by the release PR, not source revision `S`:

- `.github/docker/build-app-cli/image.json`

Modify:

- workspace `Cargo.toml` / `Cargo.lock`
- `.github/docker/build-app-cli/check-image-pin.sh`
- `.github/actions/deploy-core/tests/check-image-pin.test.sh`
- `.github/actions/deploy-core/tests/check-action-pins.sh`
- `.github/actions/deploy-core/tests/run.sh`
- `.github/workflows/deploy-action.yml`
- every existing `.github` workflow/composite containing a non-local external `uses:` ref
- the four deploy/adoption documents containing consumer `uses:` examples

## 4. Task 0: Land the validator capability contract

This task is implemented as part of this plan because no separate prerequisite plan exists. It is a
hard dependency of Task 3 and must merge into source revision `S`.

**Files:**

- Create `crates/edgezero-provenance-validator/Cargo.toml` and
  `src/{main,json_contract,archive,elf,extract}.rs`.
- Put module unit tests beside their implementation under `src/`; create only the true process-level
  integration test `crates/edgezero-provenance-validator/tests/cli.rs`.
- Create `.github/docker/build-app-cli/provenance.schema.json`.
- Create `.github/docker/build-app-cli/fixtures/provenance/{valid,invalid}/**`.
- Modify workspace `Cargo.toml` and `Cargo.lock`.

### 4.1 JSON/schema tranche

- [ ] Add the exact JSON Schema and valid/invalid metadata fixtures. Write colocated failing tests for
  RFC 8785 canonical bytes, duplicate-key rejection before object construction, exact field/type/
  bounds checks, unknown fields, caller/platform identity mismatch, and schema-version mismatch.
- [ ] Run `cargo test -p edgezero-provenance-validator json_contract::tests`; expected: non-zero with
  the new assertions failing for unimplemented behavior.
- [ ] Implement only `json_contract.rs`; rerun the same command, then the full crate test; expected:
  both pass. Commit the green JSON/schema tranche.

### 4.2 Archive/extraction tranche

- [ ] Add a byte-for-byte golden ustar archive plus malformed PAX/GNU, duplicate, extra, traversal,
  link, special-file, bad-header, bad-order, bad-size, and trailing-data fixtures.
- [ ] Write colocated archive/extraction tests, then run
  `cargo test -p edgezero-provenance-validator archive::tests`; expected: non-zero for unimplemented
  strict parsing/extraction.
- [ ] Implement `archive.rs` and `extract.rs` without invoking system `tar`. Require exact normalized
  headers and exactly one confined regular output file. Rerun focused and full crate tests; expected:
  pass. Commit the green archive/extraction tranche.

### 4.3 ELF/loadability tranche

- [ ] Add controlled valid/wrong-architecture/unresolved-interpreter/unresolved-library ELF
  fixtures. Write failing tests for machine, interpreter/null, sorted direct `DT_NEEDED`, digest, size,
  and immutable-image dependency resolution.
- [ ] Run `cargo test -p edgezero-provenance-validator elf::tests`; expected: non-zero for
  unimplemented inspection/loadability behavior.
- [ ] Implement `elf.rs`; rerun focused and full crate tests; expected: pass. Commit the green ELF
  tranche.

### 4.4 CLI/capability tranche

- [ ] Write failing `tests/cli.rs` process tests that combine the three modules and verify clean failure
  leaves the output directory empty. Run `cargo test -p edgezero-provenance-validator --test cli`;
  expected: non-zero until the CLI is wired. Implement this stable credential-free interface:

```text
edgezero-provenance-validator validate \
  --archive /work/input/artifact.tar \
  --schema /usr/local/share/edgezero/provenance.schema.json \
  --expected /work/input/expected.json \
  --output /work/validated/app-cli

edgezero-provenance-validator self-test \
  --fixtures /usr/local/share/edgezero/provenance-fixtures
```

- [ ] Make `validate` create exactly one regular output file and fail if the output parent is not
  empty, canonical, writable, and confined. The validator never executes the extracted binary.
- [ ] Implement `self-test` as a fixed manifest of expected valid and invalid fixture outcomes plus
  fixture SHA-256 values; a missing, extra, or changed fixture fails.
- [ ] Use synchronous Rust; do not add Tokio, and do not change dependencies of core/adapter crates.
- [ ] Run `cargo test -p edgezero-provenance-validator --test cli`, then the full focused crate suite;
  expected: pass. Commit the green CLI/capability tranche.
- [ ] Run the focused crate tests, then the repository-required Rust checks.

```bash
cargo test -p edgezero-provenance-validator
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
npm --prefix docs ci
npm --prefix docs run format
npm --prefix docs run lint
npm --prefix docs run build
```

**Gate:** all capability tests and fixture hashes pass from a clean checkout. Task 3 must copy this
exact built binary, schema, and fixtures into the image.

## 5. Task 1: Enforce full-SHA external references repository-wide

The current pin gate accepts version tags. That contradicts v6.18 and must be migrated before adding
the write-privileged publisher.

**Files:**

- Modify `.github/actions/deploy-core/tests/check-action-pins.sh` and its tests in `run.sh`.
- Create `.github/actions/deploy-core/tests/check-doc-action-pins.sh`.
- Modify external refs in `.github/workflows/{codeql,deploy-action,deploy-docs,fastly-installer-check,format,test}.yml`.
- Modify external refs in `.github/actions/{build-app-cli,config-push-fastly,deploy-fastly,healthcheck-fastly,rollback-fastly}/action.yml`.
- Modify examples in `docs/specs/edgezero-deploy-github-action.md`,
  `docs/specs/edgezero-deploy-action-implementation-plan.md`,
  `docs/specs/edgezero-deploy-adoption-guide.md`, and `docs/guide/deploy-github-actions.md`.

- [ ] Write failing pin-gate tests proving `@v1`, `@v1.2.3`, branches, abbreviated SHAs, malformed
  SHAs, and empty refs fail; full lowercase 40-hex SHAs pass; local actions and digest-pinned Docker
  actions remain valid. Generate invalid YAML fixtures under the test's temporary directory; do not
  commit them into a surface scanned by the production gate.
- [ ] Resolve each existing version to a reviewed upstream commit SHA. Preserve the human-readable
  release in an adjacent comment, for example `# v6.0.1`.
- [ ] Change the structural YAML scanner to require full 40-hex SHAs for every non-local external
  action and reusable workflow. Its default scan is exactly workflow `*.yml`/`*.yaml` files directly
  under `.github/workflows`, plus every repository-wide `action.yml`/`action.yaml`, pruning `.git`,
  `target`, and `node_modules`. Shell source and arbitrary YAML test data are not inputs. Do not add a
  low-privilege exception.
- [ ] Require Docker action refs to match an immutable lowercase
  `docker://<name>@sha256:<64-lowercase-hex>` form; tags, uppercase hex, short digests, and other
  algorithms fail unless a separately reviewed digest algorithm is added to the policy.
- [ ] Update documentation examples to use a named `<FULL_EDGEZERO_COMMIT_SHA>` placeholder where the
  consumer must substitute release `P`; examples for third-party actions use real reviewed SHAs.
- [ ] Add `check-doc-action-pins.sh` to extract `uses:` lines from fenced YAML in the four named docs.
  It allows the exact EdgeZero placeholder only in documentation, requires full SHAs for concrete
  third-party refs, and rejects version/branch refs. Add positive/negative cases to `run.sh`.
- [ ] Scan that exact default surface, including reusable-workflow job-level `uses`, and require at
  least one parsed external ref so a broken parser cannot pass vacuously.
- [ ] Run the pin suite, actionlint, and zizmor.

```bash
bash .github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.sh
actionlint
zizmor --offline .github/workflows .github/actions
```

**Gate:** both structural scanners pass their exact surfaces and report non-zero parsed-reference
counts; no broad `rg` gate scans intentional invalid test strings.

## 6. Task 2: Implement the exact `image.json` validator

`image.json` has five fields and is created only after publication succeeds.

**Files:**

- Modify `.github/docker/build-app-cli/check-image-pin.sh`.
- Modify `.github/actions/deploy-core/tests/check-image-pin.test.sh`.

- [ ] Write failing tests for the valid five-field record and rejection of malformed JSON, duplicate
  or extra/missing fields, non-string string fields, foreign/empty repository, mutable/zero/uppercase
  digest, malformed/zero/uppercase source revision, non-integer protocol, protocol other than `1`,
  an empty/malformed release tag, and tag use as the runtime reference.
- [ ] Implement `check-image-pin.sh <path>` using Bash and `jq`. Detect duplicate top-level keys from
  `jq --stream` events before normal object parsing; ordinary `jq` object parsing alone loses duplicate
  keys. It accepts exactly:

```json
{
  "repository": "ghcr.io/stackpop/edgezero-build-app-cli",
  "tag": "build-container-v1",
  "digest": "sha256:<64-lowercase-hex>",
  "image-source-revision": "<40-lowercase-hex>",
  "provenance-protocol": 1
}
```

  `tag` must match `^build-container-v[1-9][0-9]*$`; it remains informational.

- [ ] Output only the canonical runtime ref, source revision, and protocol through explicit
  subcommands or shell-safe output fields. Never use `tag` for a pull.
- [ ] Run unit tests and shellcheck. Do not create a placeholder `image.json`.

```bash
bash .github/actions/deploy-core/tests/check-image-pin.test.sh
shellcheck -S warning .github/docker/build-app-cli/check-image-pin.sh
```

## 7. Task 3: Build the pinned image from repository root

**Files:**

- Create `.github/docker/build-app-cli/Dockerfile`.
- Create `.dockerignore`, `.github/docker/build-app-cli/verify-toolchain.sh`, and
  `.github/docker/build-app-cli/fixtures/wasm-smoke.rs`.
- Extend validator/image tests under `.github/actions/deploy-core/tests/`.

- [ ] Before editing, resolve the amd64 digest for the exact Rust base image and the upstream sccache
  v0.10.0 release checksum. Record provenance in comments. Never commit `000...` or `REPLACE_ME`.
- [ ] Use a multi-stage Dockerfile. The builder stage copies the repository and runs:

```bash
cargo build --locked --release -p edgezero-provenance-validator
```

- [ ] Copy only the validator binary, schema, and capability fixtures from the builder into the final
  runtime. BuildKit context is repository root; the Dockerfile remains under
  `.github/docker/build-app-cli/`.
- [ ] Add a root `.dockerignore` excluding `.git`, `.claude`, every `target/`, `node_modules/`, local
  editor/temp/env files, and other non-source detritus while retaining the workspace, `.github`
  schema/fixtures, lockfile, and Dockerfile. CI also requires a clean checkout, so `.dockerignore` is
  defense in depth rather than permission to build untracked source.
- [ ] Install the exact Rust toolchain, `wasm32-wasip1`, checksum-verified Fastly CLI and sccache,
  `git`, `jq`, `tar`, `curl`, CA certificates, and a C toolchain. Remove package/download caches.
- [ ] Accept required build args `IMAGE_SOURCE_REVISION` and `PROVENANCE_PROTOCOL`. Fail the build
  unless they are a lowercase full SHA and exactly `1`.
- [ ] Add OCI labels `org.opencontainers.image.revision=$IMAGE_SOURCE_REVISION` and
  `org.edgezero.provenance-protocol=$PROVENANCE_PROTOCOL`.
- [ ] Create uid/gid 1001, set it as final `USER`, and avoid writable data under the image root.
- [ ] Build locally from root:

```bash
docker build --platform linux/amd64 \
  --build-arg IMAGE_SOURCE_REVISION="$(git rev-parse HEAD)" \
  --build-arg PROVENANCE_PROTOCOL=1 \
  -f .github/docker/build-app-cli/Dockerfile \
  -t edgezero-build-app-cli:local .
```

- [ ] Parse each tool's documented version line and compare the normalized semantic version for exact
  equality; substring matching is forbidden. Assert target installation with
  `rustup target list --installed`, then compile the committed `wasm-smoke.rs` as a library for
  `wasm32-wasip1` into writable tmpfs and assert the output starts with wasm magic `00 61 73 6d`.
- [ ] Put those assertions in `verify-toolchain.sh` and unit-test its parsers with exact, prerelease,
  extra-text, missing-line, and malformed output fixtures before copying it into the image.
- [ ] Run the baked validator `self-test`; then run one valid and each malformed fixture through the
  baked `validate` command.
- [ ] Verify image config is linux/amd64, `User` is 1001, and OCI labels equal the build args.
- [ ] Verify a read-only/non-root smoke with `--network=none`, `--cap-drop=ALL`,
  `--security-opt=no-new-privileges`, bounded memory/pids, and only `/tmp` as tmpfs.

```bash
docker run --rm --platform linux/amd64 --read-only --network=none --cap-drop=ALL \
  --security-opt=no-new-privileges --memory=512m --pids-limit=128 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec --user 1001:1001 \
  edgezero-build-app-cli:local verify-toolchain.sh \
  --rust 1.95.0 --fastly 15.1.0 --sccache 0.10.0 \
  --target wasm32-wasip1 \
  --fixture /usr/local/share/edgezero/wasm-smoke.rs
docker run --rm --read-only --network=none --cap-drop=ALL \
  --security-opt=no-new-privileges --memory=512m --pids-limit=128 \
  --tmpfs /tmp:rw,nosuid,nodev,noexec --user 1001:1001 \
  edgezero-build-app-cli:local \
  edgezero-provenance-validator self-test \
  --fixtures /usr/local/share/edgezero/provenance-fixtures
```

**Gate:** no image is pushed until every command above passes with the exact source SHA and protocol.

## 8. Task 4: Publish, verify, and open an idempotent pin PR

**Files:**

- Create `.github/docker/build-app-cli/verify-published-image.sh`.
- Create `.github/docker/build-app-cli/update-image-pin-pr.sh`.
- Create `.github/actions/deploy-core/tests/verify-published-image.test.sh`.
- Create `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`.
- Create `.github/workflows/publish-build-container.yml`.
- Modify `.github/actions/deploy-core/tests/run.sh` and `.github/workflows/deploy-action.yml`.

### 8.1 Testable verification helper

- [ ] Write fixture-driven failing tests for leaf manifest media types, required config/layers,
  rejection of one-entry and multi-entry indexes, `.Image` os/architecture, both image labels, exact
  tool versions, installed target, validator self-test, and malformed BuildKit metadata.
- [ ] Implement a helper that takes `repository`, `digest`, `source SHA`, and protocol. It verifies the
  immutable digest only and never rereads a mutable tag to discover identity.
- [ ] Use `docker buildx imagetools inspect "$REF" --raw` to require a leaf manifest. Use
  `docker buildx imagetools inspect "$REF" --format '{{json .Image}}'` and inspect `.os` and
  `.architecture` directly; do not use nonexistent `.Image.Platform`.
- [ ] Inspect image config labels and run the same exact-version, installed-target/minimal-compile,
  validator-capability, and read-only/non-root tests as Task 3.

### 8.2 Pre-`S` publisher and required CI

- [ ] Implement the publisher before designating `S`. Trigger only protected `build-container-v*`
  tags and configure the protected `build-container-release` environment and repository tag ruleset.
- [ ] Serialize the entire workflow under repository-global concurrency group
  `edgezero-build-container-publication` with `cancel-in-progress: false`; different tags must not
  race the one pin record.
- [ ] Use job permissions `contents: read` and `packages: write`. Mint a short-lived token from a
  dedicated GitHub App, stored in the protected environment and scoped only to branch contents and
  pull requests, for the pin branch/PR. `GITHUB_TOKEN` is forbidden for this operation because its
  push does not trigger push workflows and its automation-created PR checks require manual approval;
  it cannot guarantee the automatic required-check path. Pin the token-minting and checkout actions
  to reviewed full SHAs.
- [ ] Mint the GitHub App token only after build, digest verification, and anonymous verification have
  completed, so neither its private key nor installation token exists while repository-root context is
  assembled or app-owned Rust code is built.
- [ ] Checkout with `persist-credentials: false` and full history. Resolve
  `S=$(git rev-parse "${GITHUB_SHA}^{commit}")`, validate it as 40 lowercase hex, fetch the protected
  default branch, and require `S` to be its ancestor.
- [ ] Immediately before BuildKit receives root context, require `HEAD == S`, no tracked/index
  changes, no untracked files, and clean initialized submodules. Re-run the same assertions after
  extracting metadata. No credential may exist in Git config or a file under the context.
- [ ] Build with repository-root context, explicit `-f`, `--platform linux/amd64`, exact source/protocol
  args, `--provenance=false`, `--sbom=false`, and `--metadata-file`:

```bash
docker buildx build --platform linux/amd64 \
  --build-arg "IMAGE_SOURCE_REVISION=$S" \
  --build-arg PROVENANCE_PROTOCOL=1 \
  --provenance=false --sbom=false \
  --metadata-file "$RUNNER_TEMP/build-metadata.json" \
  -f .github/docker/build-app-cli/Dockerfile \
  --tag "$REPOSITORY:$GITHUB_REF_NAME" --push .
D=$(jq -er '."containerimage.digest"' "$RUNNER_TEMP/build-metadata.json")
```

- [ ] Validate `D` immediately and pass it to `verify-published-image.sh`. Never derive `D` by
  inspecting the mutable tag.
- [ ] After authenticated verification, remove the local image reference, use a fresh empty
  `DOCKER_CONFIG`, and pull/run `REPOSITORY@D` without credentials. The anonymous check must make a
  registry request and fail if the package is private.
- [ ] On first publication, a private GHCR package intentionally stops before pin PR creation. An
  operator makes the package public and reruns the same workflow/tag. Do not merge a pin first.
- [ ] Generate the exact five-field `image.json`, run `check-image-pin.sh`, and use a branch derived
  from both `S` and `D`.
- [ ] Implement and fixture-test the branch/PR state machine. Fetch an existing remote branch and
  record its exact OID; update it only with
  `--force-with-lease=refs/heads/<branch>:<recorded-oid>`. Create an absent branch without force.
  Update one open matching PR. Reopen a closed-unmerged matching PR or fail for operator review.
  Treat an already-merged exact `{S,D}` record as idempotent success. If the same `S` produces a new
  `D`, close/supersede any older open pin PR before opening the new digest PR. Multiple or ambiguous
  states fail closed.
- [ ] Put this state machine in `update-image-pin-pr.sh`. Its tests inject fake `git` and `gh` through
  `PATH`, record every argv/stdin mutation, and cover absent branch, matching remote OID, lease race,
  one open PR, closed-unmerged PR, already-merged exact record, same-`S`/new-`D` supersession, multiple
  matches, API failure, and rerun idempotency. Run the focused test red before implementation and green
  afterward, then run shellcheck.
- [ ] Include `S`, `D`, protocol, verified platform, and anonymous-pull result in the PR body. Never
  include an AI byline.
- [ ] Before `S`, extend `.github/workflows/deploy-action.yml` with a required local-image job that
  builds from root and runs all Task 3 smokes. Its PR/push trigger set is exactly `.tool-versions`,
  root `Cargo.toml`/`Cargo.lock`, `crates/edgezero-provenance-validator/**`,
  `.github/actions/deploy-fastly/versions.json`, `.dockerignore`,
  `.github/docker/build-app-cli/**`,
  `.github/actions/deploy-core/tests/check-image-pin.test.sh`,
  `.github/actions/deploy-core/tests/verify-toolchain.test.sh`,
  `.github/actions/deploy-core/tests/verify-published-image.test.sh`,
  `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`,
  `.github/actions/deploy-core/tests/run.sh`,
  `.github/workflows/publish-build-container.yml`, and `.github/workflows/deploy-action.yml`.
- [ ] Before `S`, add a required pin-change job for every add/change/delete of `image.json`. It must
  require the file to exist, run `check-image-pin.sh`, use a clean anonymous Docker config, and run the
  complete `verify-published-image.sh` against the committed digest. This job is the pre-merge gate
  for every future pin, not a one-time release checklist.
- [ ] Wire all helper unit suites into `run.sh`; assert the explicit trigger set above in contract
  tests so existing-path omissions regress visibly; make actionlint, shellcheck, and
  `zizmor --offline` cover the publisher and helpers.

### 8.3 Land `S`, then execute publication

- [ ] Run all Task 0-4 local and CI tests, merge validator, Dockerfile, `.dockerignore`, helpers,
  publisher, and required CI jobs, then record the resulting full default-branch commit as `S`.
- [ ] Create the protected release tag at exactly `S`. The publisher must verify the tag resolves to
  that commit and perform the build/verification logic already reviewed at `S`.
- [ ] On first publication, a private GHCR package intentionally stops before pin PR creation. An
  operator makes the package public and reruns the same workflow/tag. Do not merge a pin first.
- [ ] Require the GitHub-App-created pin PR's local shape and remote anonymous image verification jobs
  to pass before review or merge.

**Gate:** the pin PR cannot exist unless the exact digest passed all checks including anonymous pull.

## 9. Task 5: Merge and verify pin baseline `B`

**Files:**

- Add `.github/docker/build-app-cli/image.json` through the publisher PR.
- No post-merge gate wiring: all required checks were part of source `S`.

- [ ] Review the generated record and confirm its source revision is the published `S`, digest is the
  verified `D`, and protocol is `1`.
- [ ] Confirm the GitHub App push triggered all required pin-change workflows and that every check
  passed. Merge the pin-only PR and record the merge/full commit SHA as baseline `B`, not final action
  revision `P`.
- [ ] Confirm a deletion or syntactically valid but unverifiable replacement of `image.json` fails the
  required pin-change job in a test PR.
- [ ] Run the full repository verification suite from a clean checkout at baseline `B`:

```bash
bash .github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.sh
actionlint
zizmor --offline .github/workflows .github/actions
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
cargo check --workspace --all-targets --features "fastly cloudflare spin"
cargo check -p edgezero-adapter-spin --target wasm32-wasip2 --features spin
npm --prefix docs ci
npm --prefix docs run format
npm --prefix docs run lint
npm --prefix docs run build
```

- [ ] Pull `repository@digest` anonymously again after merge and rerun image verification by the
  committed record.

**Gate:** downstream plans build on baseline `B`; they do not reference source revision `S` as an
action ref or recompute a tag digest. Their final integration plan designates full SHA `P` only after
all feature contracts pass.

## 10. Task 6: Release and retention runbook

- [ ] Protect the publisher tag pattern and environment; require review for release execution.
- [ ] Confirm GHCR package visibility is public before the pin PR can be generated.
- [ ] Configure retention so no digest referenced by any supported `image.json` is deleted.
- [ ] Document rollback as reverting to an earlier reviewed `image.json` digest/protocol and pinning
  consumers to the corresponding earlier action SHA. Never move a tag to simulate rollback.
- [ ] Document the release record: image source `S`, digest `D`, pin baseline `B`, final action pin
  `P`, image tag (informational), checksums, and exact third-party action SHAs.
- [ ] Update the parent spec, implementation plan, adoption guide, and public guide in the downstream
  integration plan. Consumer examples must use one full `P` for all EdgeZero references.

## 11. Completion review

Before declaring this plan complete, run two independent reviews:

1. **Contract review:** compare every file and test with design v6.18 Sections 3, 5, 6.3, 8, 9, and
   10. Verify there is no same-SHA claim, no platform identity output, no tag runtime pull, no
   placeholder, and no legacy `--stage` guidance.
2. **Release-adversary review:** test mutable tags, private package state, stale/idempotent PR branches,
   malformed BuildKit metadata, index manifests, wrong platform/labels/versions/protocol, deleted
   image pin, publication reruns, and concurrent release attempts.

The container plan is complete only when source `S`, verified digest `D`, and pin baseline `B` are
recorded and all repository gates pass. The remaining plans may then implement cached compilation and
eventually designate final action revision `P`.
