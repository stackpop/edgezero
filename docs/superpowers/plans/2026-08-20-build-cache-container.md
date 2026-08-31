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

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` v6.19.

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
- Every non-local external action and reusable workflow ref is a full lowercase 40-hex commit SHA.
  Docker image refs use immutable `sha256` digests. Local `./...` actions remain local refs.
- Bash scripts are Bash 3.2-compatible and `shellcheck -S warning` clean. CI helper scripts do not use
  Python. No AI bylines appear in commits or PRs.
- Publication never records a digest before the image passes authenticated verification and a clean,
  anonymous pull by digest.

## 2. Dependency order

Although this is plan 1 of the feature set, its image task cannot run first. Execute these gates:

1. Complete and commit the repository-wide full-SHA and zizmor policy migration on the unmerged
   source-candidate branch (Task 0).
2. Complete and commit the trusted protocol-owner validator and capability fixtures on that same
   branch (Task 1).
3. Implement image pinning, the Dockerfile, publisher, local-image CI, and pin-change CI on that same
   branch (Tasks 2-4).
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
- `crates/edgezero-provenance-validator/src/{lib,main,json_contract,archive,elf,extract}.rs`
- `crates/edgezero-provenance-validator/tests/cli.rs`
- `.github/docker/build-app-cli/provenance.schema.json`
- `.github/docker/build-app-cli/fixtures/provenance/**`
- `.github/docker/build-app-cli/fixtures/wasm-smoke.rs`
- `.github/docker/build-app-cli/Dockerfile`
- `.dockerignore`
- `.github/docker/build-app-cli/verify-toolchain.sh`
- `.github/docker/build-app-cli/verify-published-image.sh`
- `.github/docker/build-app-cli/verify-release-prerequisites.sh`
- `.github/docker/build-app-cli/update-image-pin-pr.sh`
- `.github/docker/build-app-cli/classify-build-container-change.sh`
- `.github/actions/deploy-core/tests/verify-toolchain.test.sh`
- `.github/actions/deploy-core/tests/verify-published-image.test.sh`
- `.github/actions/deploy-core/tests/verify-release-prerequisites.test.sh`
- `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`
- `.github/actions/deploy-core/tests/classify-build-container-change.test.sh`
- `.github/actions/deploy-core/tests/check-doc-action-pins.sh`
- `.github/workflows/build-container-ci.yml`
- `.github/workflows/publish-build-container.yml`

Created by the release PR, not source revision `S`:

- `.github/docker/build-app-cli/image.json`

Modify:

- workspace `Cargo.toml` / `Cargo.lock`
- `.github/docker/build-app-cli/check-image-pin.sh`
- `.github/actions/deploy-core/tests/check-image-pin.test.sh`
- `.github/actions/deploy-core/tests/check-action-pins.sh`
- `.github/actions/deploy-core/tests/run.sh`
- `.github/zizmor.yml`
- `.github/workflows/deploy-action.yml`
- every existing `.github` workflow/composite containing a non-local external `uses:` ref
- the four deploy/adoption documents containing consumer `uses:` examples

## 4. Task 0: Enforce full-SHA external references repository-wide

The current pin gate and zizmor policy accept version tags. That contradicts v6.19 and must be
migrated before adding the write-privileged publisher.

**Files:**

- Modify `.github/actions/deploy-core/tests/check-action-pins.sh` and its tests in `run.sh`.
- Create `.github/actions/deploy-core/tests/check-doc-action-pins.sh`.
- Modify `.github/zizmor.yml`.
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
- [ ] Reject empty and null `uses` scalars and count only parsed non-local external refs for the
      non-vacuity assertion. Encode each structurally extracted scalar so a multiline value cannot split
      into multiple shell records.
- [ ] Require Docker action refs to match an immutable lowercase
      `docker://<name>@sha256:<64-lowercase-hex>` form; tags, uppercase hex, short digests, and other
      algorithms fail unless a separately reviewed digest algorithm is added to the policy.
- [ ] Update documentation examples to use a named `<FULL_EDGEZERO_COMMIT_SHA>` placeholder where the
      consumer must substitute release `P`; examples for third-party actions use real reviewed SHAs.
- [ ] Add `check-doc-action-pins.sh` to extract `uses:` lines from fenced YAML in the four named docs.
      It allows the exact EdgeZero placeholder only in documentation, requires full SHAs for concrete
      third-party refs, and rejects version/branch refs. Add positive/negative cases to `run.sh`.
- [ ] Replace the global zizmor `ref-pin` relaxation with `hash-pin`. Update contradictory prose in
      all four named documents, not only their fenced YAML examples.
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

## 5. Task 1: Implement the protocol-owner validator on the source candidate

This task owns protocol-1 encoding and validation. No shell, `jq`, system `tar`, or general-purpose
archive crate may become a second wire implementation. It is a hard dependency of Task 3 and must
merge into source revision `S`.

**Files:**

- Create `crates/edgezero-provenance-validator/Cargo.toml` and
  `src/{lib,main,json_contract,archive,elf,extract}.rs`.
- Put module unit tests beside their implementation under `src/`; create only the true process-level
  integration test `crates/edgezero-provenance-validator/tests/cli.rs`.
- Create `.github/docker/build-app-cli/provenance.schema.json`.
- Create `.github/docker/build-app-cli/fixtures/provenance/{valid,invalid}/**`.
- Modify workspace `Cargo.toml` and `Cargo.lock`.

### 5.1 JSON/schema tranche

- [ ] Add one Draft 2020-12 schema and exact valid/invalid fixtures for both `expected.json` and
      `app-cli-meta.json` from design Section 6.2. Write colocated failing tests for RFC 8785 bytes,
      recursive duplicate-key rejection before object construction, every exact field/type/bound,
      unknown and missing fields, noncanonical decimal/hash/name values, schema/protocol mismatch,
      `container-ref` derivation, and complete caller/platform identity mismatch.
- [ ] Test a closed typed canonical encoder. Protocol 1 contains only bounded strings, positive
      integers, null, fixed objects, and the `needed` array; no generic floating-point value is accepted.
- [ ] Run `cargo test -p edgezero-provenance-validator json_contract::tests`; expected: non-zero for
      unimplemented behavior.
- [ ] Implement only `json_contract.rs`; rerun the focused and full crate tests; expected: pass.
      Commit the green JSON/schema tranche.

### 5.2 Archive/extraction tranche

- [ ] Add a byte-for-byte golden archive from design Section 6.3 plus malformed base-256/octal,
      checksum, embedded-NUL, PAX/GNU, sparse, duplicate, extra, traversal, link, special-file, header,
      order, size, padding, end-block, overflow, and trailing-data fixtures.
- [ ] Write failing encoder, parser, and extraction tests. Assert two repeated encodes are identical,
      all payload padding is zero, exactly two end blocks precede EOF, and failure leaves the fresh output
      parent empty.
- [ ] Run `cargo test -p edgezero-provenance-validator archive::tests`; expected: non-zero for
      unimplemented protocol behavior.
- [ ] Implement `archive.rs` and `extract.rs` directly over bounded `Read + Seek`/`Write`; do not
      invoke system `tar`, add a tar crate, or load the allowed 512 MiB binary wholesale. Create outputs
      atomically and require the final regular file to have mode 0755 and link count one. Rerun focused
      and full crate tests; expected: pass. Commit the green archive/extraction tranche.

### 5.3 ELF/loadability tranche

- [ ] Add controlled static/dynamic valid, wrong class/endian/type/architecture/interpreter,
      malformed/duplicate `PT_DYNAMIC`, missing/nonzero-after `DT_NULL`, conflicting string-table tags,
      unmapped/overlapping string ranges, malformed string/interpreter termination, RPATH/RUNPATH,
      AUDIT/DEPAUDIT/CONFIG/AUXILIARY/FILTER/POSFLAG rejection, valid bounded SONAME, empty/oversized/
      slash-containing/duplicate SONAME rejection, NODEFLIB/LOADFLTR and unknown-flag rejection, every
      in-range and just-outside case for the closed numeric tag allowlist, exact
      `DT_FLAGS=0x0000001e` and `DT_FLAGS_1=0x5eff976f` mask boundaries, unknown standard/GNU/OS/processor
      tag rejection, duplicate rejection for every singleton tag, slash-containing dependency, missing
      direct/transitive library, ambiguous resolution, dangling or escaping candidates,
      mixed-architecture, duplicate-needed, interpreter dependency, and cycle fixtures for the
      conservative loader profile in design Section 6.4.
- [ ] Write failing tests for machine, interpreter/null, byte-sorted duplicate-preserving direct
      `DT_NEEDED`, digest, size, six-root candidate enumeration, same-device/inode symlink and hardlink
      aliases, distinct-file ambiguity, interpreter parsing, and recursive dependency resolution against
      a synthetic image root.
- [ ] Run `cargo test -p edgezero-provenance-validator elf::tests`; expected: non-zero for
      unimplemented inspection/loadability behavior.
- [ ] Implement `elf.rs` with bounded ranged reads and checked offsets. Do not invoke `ldd`, the
      loader, or the artifact. Rerun focused and full crate tests; expected: pass. Commit the green ELF
      tranche.

### 5.4 CLI/capability tranche

- [ ] Write failing library integration tests using a private synthetic-root harness for deterministic
      package/validate round trips, identity mismatch, atomic cleanup, and host-deletion recovery. This
      harness calls library entry points and is not a CLI option or production bypass. Write host process
      tests proving `package` and `validate` reject every `--work-root` that does not canonicalize to
      literal `/work`, plus process tests for self-test fixture integrity. Run
      `cargo test -p edgezero-provenance-validator --test cli`; expected: non-zero until wired. Implement:

```text
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

- [ ] Make the production `package` and `validate` CLI require canonical `--work-root /work`, create
      exactly one output through a create-new temporary sibling plus Linux no-replace rename, and fail if
      the parent is not fresh, empty, canonical, writable, and confined. Handled failures remove the
      sibling; synthetic-root library tests model host deletion of the whole parent after
      SIGKILL/timeout. The validator never executes the app binary. Positive CLI round trips run only in
      Task 3's container, where literal `/work` exists.
- [ ] Implement `self-test` as a compiled manifest of exact relative paths, fixture SHA-256 values,
      and valid/invalid outcomes. A missing, extra, or changed fixture fails.
- [ ] Use synchronous Rust; do not add Tokio or change dependencies of core/adapter crates.
- [ ] Run process, focused, and full crate tests; expected: pass. Commit the green CLI/capability
      tranche.
- [ ] Run the focused crate tests, then the repository-required Rust and documentation checks.

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

**Gate:** deterministic package/validate golden tests and every capability fixture hash pass from a
clean checkout. The candidate PR must also pass every current format/test matrix job, including the
four wasm clippy legs and three wasm test runners; the local command list does not replace those
runner-backed gates. Task 3 copies this exact built binary, schema, and fixtures into the image.

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

- [ ] Before editing, re-resolve the official `rust:1.95.0-slim-bookworm` `linux/amd64` leaf manifest
      and compare it with the reviewed digest in Section 1. Stop for review if the tag moved; never
      silently replace the reviewed base. Download the selected sccache asset and its upstream checksum
      companion independently, hash the payload, and require the reviewed checksum in Section 1. Record
      provenance in comments. Never commit `000...` or `REPLACE_ME`.
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
- [ ] Use the reviewed Rust leaf digest in every `FROM`. Install the exact Rust toolchain,
      `wasm32-wasip1`, checksum-verified Fastly CLI, the selected static-musl sccache client, `git`, `jq`,
      `tar`, `curl`, CA certificates, and a C toolchain. Remove package/download caches.
- [ ] Accept required build args `IMAGE_SOURCE_REVISION` and `PROVENANCE_PROTOCOL`. Fail the build
      unless they are a lowercase full SHA and exactly `1`.
- [ ] Override inherited OCI metadata with exact labels
      `org.opencontainers.image.source=https://github.com/stackpop/edgezero`,
      `org.opencontainers.image.revision=$IMAGE_SOURCE_REVISION`, and
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
- [ ] Write parser and command-fixture tests in `verify-toolchain.test.sh` for exact, prerelease,
      extra-text, missing-line, malformed output, absent target, and invalid wasm magic. Run
      `bash .github/actions/deploy-core/tests/verify-toolchain.test.sh`; expected: non-zero before the
      helper exists.
- [ ] Put the assertions in `verify-toolchain.sh`, rerun the focused test, and require zero failures
      before copying it into the image.
- [ ] Run the baked validator `self-test`; run deterministic `package` twice over the controlled ELF
      fixture and compare bytes; then run the golden archive and every malformed fixture through the
      baked `validate` command, with fresh mounts rooted at literal `/work`. Prove the production CLI
      accepts `/work`, rejects an alternate root, and that the image's glibc layout satisfies the fixed
      loader profile.
- [ ] Do not add a validator basename-mismatch case: the fixed `/work/input/app-cli` mount cannot
      expose the original Cargo output basename. Record this as a mandatory host-action test in the
      downstream provenance-integration plan, where the basename is checked before mounting.
- [ ] Verify image config is linux/amd64, `User` is 1001, and all three OCI labels equal the exact
      EdgeZero source, source revision, and protocol values.
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
- Create `.github/docker/build-app-cli/verify-release-prerequisites.sh`.
- Create `.github/docker/build-app-cli/update-image-pin-pr.sh`.
- Create `.github/docker/build-app-cli/classify-build-container-change.sh`.
- Create `.github/actions/deploy-core/tests/verify-published-image.test.sh`.
- Create `.github/actions/deploy-core/tests/verify-release-prerequisites.test.sh`.
- Create `.github/actions/deploy-core/tests/update-image-pin-pr.test.sh`.
- Create `.github/actions/deploy-core/tests/classify-build-container-change.test.sh`.
- Create `.github/workflows/build-container-ci.yml`.
- Create `.github/workflows/publish-build-container.yml`.
- Modify `.github/actions/deploy-core/tests/run.sh` and `.github/workflows/deploy-action.yml`.

### 8.1 Testable verification helper

- [ ] Write fixture-driven failing tests for leaf manifest media types, required config/layers,
      rejection of one-entry and multi-entry indexes, `.Image` os/architecture, all three image labels, exact
      tool versions, installed target, validator self-test, and malformed BuildKit metadata.
- [ ] Run `bash .github/actions/deploy-core/tests/verify-published-image.test.sh`; expected: non-zero
      before the helper exists.
- [ ] Implement a helper that takes `repository`, `digest`, `source SHA`, and protocol. It verifies the
      immutable digest only and never rereads a mutable tag to discover identity.
- [ ] Use `docker buildx imagetools inspect "$REF" --raw` to require a leaf manifest. Use
      `docker buildx imagetools inspect "$REF" --format '{{json .Image}}'` and inspect `.os` and
      `.architecture` directly; do not use nonexistent `.Image.Platform`.
- [ ] Inspect image config labels and run the same exact-version, installed-target/minimal-compile,
      validator-capability, and read-only/non-root tests as Task 3.

### 8.2 Pre-`S` publisher and required CI

- [ ] Write failing fake-`gh`/`openssl` tests, then implement `verify-release-prerequisites.sh`. It takes
      the repository and candidate PR, expected numeric App and installation IDs, App private-key path,
      expected package state, and evidence output path. It reads a repository-administrator token and a
      separate package-audit token from `EDGEZERO_RELEASE_REPOSITORY_ADMIN_TOKEN` and
      `EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN`, respectively. The latter is a classic PAT belonging to an
      active `stackpop` organization owner; require the normalized `X-OAuth-Scopes` set to equal exactly
      `{read:org,read:packages}` and use that same verified token for all package requests. Reject
      byte-equal token values before any API request without logging them. Neither token is stored in
      GitHub Actions. The helper never accepts a PR-write token and never mutates settings, packages, or
      comments.
- [ ] Require environment `build-container-release` to have at least one required reviewer,
      `prevent_self_review=true`, administrator bypass disabled, custom deployment policies enabled, and
      exactly one deployment policy: tag `build-container-v*`. The documented environment REST response
      does not expose administrator bypass; do not invent an API assertion. Instead require a PNG settings
      capture, independent reviewer login, and RFC 3339 review time as preflight inputs. Reject a non-PNG,
      reviewer equal to the verifier, future review time, or recorded candidate head SHA unequal to the
      current PR head. Record that SHA, `allowed:false`, `verification:"manual-ui"`, reviewer, review time,
      literal basename, and `sha256:<64-lowercase-hex>` under `environment.administrator-bypass` in
      canonical evidence. Require an active tag ruleset matching that pattern with creation, update, and
      deletion restrictions. Require exactly one bypass actor: team
      `edgezero-build-container-releasers`, whose ID equals environment variable
      `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID`, with bypass mode `always`; require the verifier actor
      to be an active team member. Require an active default-branch ruleset requiring
      `build-container-local` and `build-container-pin`. Enumerate the candidate's successful check runs,
      require both names to come from one App with slug `github-actions`, and require each ruleset
      status-check entry's non-null `integration_id` to equal that App ID. Record the ID and check-run URLs
      in evidence; a same-name status from any other source fails.
- [ ] Require protected-environment variables `EDGEZERO_BUILD_CONTAINER_APP_ID`,
      `EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID`, and
      `EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID` to equal the reviewed App, installation, and sole
      bypass-team IDs, and secret metadata to contain `EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY`.
      Generate a short-lived App JWT with `openssl`;
      verify the authenticated App and active installation identity; require account `stackpop`, selected
      repositories, exactly `contents:write`, `pull_requests:write`, and implicit `metadata:read`, and an
      installation repository list containing only `stackpop/edgezero`. Mint a test installation token
      restricted to that repository ID with explicit contents/pull-request write permissions, verify its
      returned scope and repository read, and revoke it in a trap before exit. Never print JWTs, tokens, or
      private-key material.
- [ ] Before first push, allow an absent package only after the verified active organization owner's
      package-audit token produces a successful fully paginated organization container-package listing
      with no exact package-name match; a listing from another identity, GET 404, or authorization error
      never establishes absence. Afterward require the package API record to be public and linked to
      `stackpop/edgezero`. Emit canonical JSON containing repository/PR, package-audit login, owner role,
      granted non-secret scopes, environment protection and policy IDs/URLs, ruleset and sole bypass-team
      IDs/URLs, App and installation IDs, exact installation/token scopes, required checks and integration
      ID, package identity/visibility/repository link, verifier actor/team membership, and timestamp, but
      no credential values. Record its SHA-256.
      A separately authenticated operator posts the evidence file, digest, and byte-identical
      administrator-bypass PNG to the candidate PR; failure to post blocks `S`. API failure, incomplete
      pagination, ambiguity, an extra bypass actor,
      repository, or write permission, failed token revocation, or a missing control fails closed.
- [ ] In `build-container-ci.yml`, add non-required job `build-container-release-preflight`. It runs
      only for a same-repository `pull_request` carrying maintainer-applied label
      `build-container-release-candidate`, references environment
      `{name: build-container-release, deployment: false}`, performs no checkout, and invokes only the
      pinned token action plus fixed inline API assertions. Use the stored
      App ID/private key, repository `edgezero`, explicit `permission-contents: write` and
      `permission-pull-requests: write`, and default token revocation. Require the action's
      `installation-id` output to equal stored variable `EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID`,
      prove the token reads only `stackpop/edgezero`, and expose no credential-derived output. The
      environment reviewer inspects the workflow diff before approval.
- [ ] Because the final environment is tag-only, document and fixture-test the bounded smoke sequence:
      an administrator temporarily adds one custom branch deployment policy equal to literal
      `refs/pull/<candidate-pr>/merge`; applies the label; obtains environment approval and a green smoke;
      then removes only that branch policy. The final preflight requires the sole `build-container-v*` tag
      policy again. It resolves the successful workflow run and requires its PR number, head repository
      `stackpop/edgezero`, and head SHA to equal the current candidate values, and requires all App-variable
      and private-key-secret metadata `updated_at` timestamps to be no later than that run's completion.
      A new commit or credential update invalidates the evidence and requires a new smoke. Any wildcard,
      source-branch, or fork policy fails.
- [ ] Extend the preflight helper to require that job's latest candidate check run to be successful and
      sourced from the same GitHub Actions integration ID as the two required jobs, and to resolve to that
      exact workflow run. This is the pre-`S` proof that the actual protected-environment secret, not only
      the operator's local key, mints the publisher's exact scoped token.
- [ ] Run `bash .github/actions/deploy-core/tests/verify-release-prerequisites.test.sh` before
      implementation; expected: non-zero. Rerun after implementation and require zero failures plus
      `shellcheck -S warning`.
- [ ] Implement the publisher before designating `S`. Trigger only protected `build-container-v*`
      tags. Before tagging, verify the protected `build-container-release` environment, tag ruleset,
      dedicated GitHub App installation and credentials, package/repository permissions, and branch
      ruleset entries for `build-container-local` and `build-container-pin`. Record operator evidence;
      missing prerequisites stop release execution.
- [ ] Serialize the entire workflow under repository-global concurrency group
      `edgezero-build-container-publication` with `cancel-in-progress: false`; different tags must not
      race the one pin record.
- [ ] Split the publisher into `build-and-verify` and `update-pin`. `build-and-verify` has no
      `environment`, uses job permissions `contents: read` and `packages: write`, and exports only
      non-secret `{S,D,protocol,tag}` outputs after every authenticated and anonymous check passes.
      Authenticate to `ghcr.io` only by
      piping `${{ secrets.GITHUB_TOKEN }}` to `docker login` in a fresh
      `$RUNNER_TEMP/publish-docker-config`; never pass it as a build arg, secret mount, environment inside
      the build, or context file. Remove that config before anonymous verification.
- [ ] Make `update-pin` depend on successful `build-and-verify`, set
      `environment: build-container-release` on that job, grant its `GITHUB_TOKEN` only `contents: read`,
      and perform no image build. It checks out with persisted credentials disabled, consumes only the
      four non-secret outputs, verifies their syntax and relationship to the tag event, then mints and
      uses the App token for pin branch/PR mutation. The environment private key is unavailable to the
      build job.
- [ ] Before every `update-pin` environment approval, including same-tag reruns, wait for
      `build-and-verify` to pass. The environment approver then captures a fresh PNG of the disabled
      administrator-bypass control and records its digest, login, review time, workflow run ID, exact `S`,
      and release tag. Require that same login to approve `update-pin` within 15 minutes. Attach the record
      and byte-identical PNG to release evidence. A missed window, bypassed approval, run-ID mismatch, or
      known policy change invalidates the run and requires a fresh workflow run, capture, and approval.
- [ ] Mint the branch/PR token only after anonymous verification with
      `actions/create-github-app-token@bcd2ba49218906704ab6c1aa796996da409d3eb1 # v3`, using protected
      environment variable `EDGEZERO_BUILD_CONTAINER_APP_ID` and secret
      `EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY`, owner `stackpop`, repository `edgezero`,
      `permission-contents: write`, and `permission-pull-requests: write`; do not inherit installation-wide
      permissions. Require its `installation-id` output to equal protected-environment variable
      `EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID` before use. The App installation itself is limited to
      that repository and those two write permissions plus implicit metadata read. `GITHUB_TOKEN` is
      forbidden for branch/PR mutation because its push does not trigger push workflows and its
      automation-created PR checks require manual approval. Checkout uses
      `actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7` with persisted credentials
      disabled.
- [ ] Mint the GitHub App token only in `update-pin`, after `build-and-verify` completed, so neither its
      private key nor installation token is available while repository-root context is assembled or
      app-owned Rust code is built.
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
- [ ] Add a static release-workflow test rejecting package-deletion API endpoints, `delete:packages`,
      package-admin tokens, or cleanup jobs. GHCR has no enforceable per-version retention lock; manual
      administrator deletion remains an explicit operational risk rather than a fake automated gate.
- [ ] Generate the exact five-field `image.json`, run `check-image-pin.sh`, and use a branch derived
      from both `S` and `D`.
- [ ] Implement and fixture-test the branch/PR state machine. Fetch an existing remote branch and
      record its exact OID; update it only with
      `--force-with-lease=refs/heads/<branch>:<recorded-oid>`. Create an absent branch without force.
      Update one open matching PR. Reopen the sole closed-unmerged matching PR after recreating/updating
      its exact source/digest branch; a missing head repository or failed reopen fails for operator review.
      Treat an already-merged exact `{S,D}` record as idempotent success. If the same `S` produces a new
      `D`, close/supersede any older open pin PR before opening the new digest PR. Multiple or ambiguous
      states fail closed.
- [ ] Put this state machine in `update-image-pin-pr.sh`. Its tests inject fake `git` and `gh` through
      `PATH`, record every argv/stdin mutation, and cover absent branch, matching remote OID, lease race,
      one open PR, closed-unmerged PR, already-merged exact record, same-`S`/new-`D` supersession, multiple
      matches, missing closed-PR head, reopen/API failure, and rerun idempotency. Run the focused test red
      before implementation and green afterward, then run shellcheck.
- [ ] Include `S`, `D`, protocol, verified platform, and anonymous-pull result in the PR body. Never
      include an AI byline.
- [ ] Write failing tests for `classify-build-container-change.sh`. Cover pull-request, merge-group,
      and push base/head ranges, rename/add/change/delete, an all-zero first-push base, shallow/missing
      commits, empty/duplicate/invalid output, and the exact local-image path set: `.tool-versions`,
      root `rust-toolchain`/`rust-toolchain.toml`, `.cargo/**`, root `Cargo.toml`/`Cargo.lock`,
      `crates/edgezero-provenance-validator/**`, `.github/actions/deploy-fastly/versions.json`,
      `.dockerignore`, `.github/docker/build-app-cli/**`, the six focused helper test files, `run.sh`,
      `.github/workflows/build-container-ci.yml`, and `.github/workflows/publish-build-container.yml`.
      Pin classification is exact add/change/delete detection for
      `.github/docker/build-app-cli/image.json`.
- [ ] Run `bash .github/actions/deploy-core/tests/classify-build-container-change.test.sh`; expected:
      non-zero before the helper exists.
- [ ] Implement the classifier fail closed over a full checkout and explicit base/head SHAs. It emits
      only a typed `relevant=true|false` output. Do not use a third-party path-filter action.
- [ ] Create `.github/workflows/build-container-ci.yml` with unfiltered `pull_request` types `opened`,
      `synchronize`, `reopened`, and `labeled`, plus `merge_group` and `push` to `main` triggers. It always
      materializes stable jobs `build-container-local` and `build-container-pin`; do not put workflow-level
      `paths` or job-level skip conditions on them.
- [ ] Make each required job independently check out full history without persisted credentials and
      run the classifier. `build-container-local` builds from root and runs all Task 3 smokes when
      relevant, otherwise it runs an explicit successful not-applicable step. `build-container-pin`
      requires `image.json`, runs `check-image-pin.sh`, creates a fresh anonymous Docker config, and runs
      complete `verify-published-image.sh` for relevant add/change/delete events; otherwise it explicitly
      succeeds as not applicable. Each job has an unconditional terminal assertion that classification
      was exactly one valid line and exactly one execution branch wrote its completion marker. A
      classifier/build/no-op failure fails that required job rather than skipping it.
- [ ] Keep the existing path-filtered `.github/workflows/deploy-action.yml` separate. The unfiltered
      `build-container-local` job itself runs all focused helper suites, shellchecks
      `.github/docker/build-app-cli/*.sh`, and applies actionlint plus `zizmor --offline` to both new
      workflows whenever a helper/workflow input changes. Modify deploy-action static checks to run both
      pin scanners and retain broad repository coverage, but do not rely on its path filter for the new
      helper surface.
- [ ] Wire all focused helper suites into `run.sh`. Add workflow contract tests for the unfiltered
      triggers, exact job names, independent classification, explicit no-op steps, local-image path set,
      pin deletion failure, and the same-repository/label/environment/no-checkout/scoped-token contract of
      `build-container-release-preflight` so topology drift is visible.

### 8.3 Merge the source candidate as `S`, then execute publication

- [ ] Run all Task 0-4 local and CI tests on the candidate PR, including both always-materialized
      container jobs. Complete the external prerequisite check from Section 8.2 after those check names
      exist, apply `build-container-release-candidate`, obtain the independent environment approval and
      successful credential-smoke check, and complete the preflight evidence before merge.

**Release checkpoint 1:** stop. A maintainer who is neither the preflight verifier nor the recorded
administrator-bypass reviewer reviews the canonical prerequisite evidence and both required jobs,
recomputes the attached PNG digest, and confirms it visibly shows administrator bypass disabled before
authorizing merge. Any candidate commit or environment-policy change invalidates the manual evidence.

- [ ] Merge validator, Dockerfile, `.dockerignore`, helpers, publisher, and required CI jobs; record
      the resulting full default-branch commit as `S`.

**Release checkpoint 2:** stop. Confirm the recorded default-branch commit and protected tag target
are exactly `S` before creating the tag.

- [ ] Using a credential for the preflight-verified active member of sole bypass team
      `edgezero-build-container-releasers`, create the protected release tag at exactly `S`. The
      publisher must verify the tag resolves to that commit and perform the build/verification logic
      already reviewed at `S`.
- [ ] On first publication, a private GHCR package intentionally stops before pin PR creation. An
      operator makes the package public, confirms its API record links `stackpop/edgezero`, and reruns
      the same workflow/tag. Do not merge a pin first.

**Release checkpoint 3:** stop after the first private-package failure. Resume the same tag only after
public visibility and repository linkage are independently reviewed.

- [ ] Require the GitHub-App-created pin PR's local shape and remote anonymous image verification jobs
      to pass before review or merge.

**Release checkpoint 4:** stop before merging the pin PR. Confirm its only content is the exact
five-field `image.json` for verified `{S,D,protocol}` and both required container checks passed.

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
- [ ] From a clean checkout at baseline `B`, rerun every Task 0 gate and every command in the Task 1
      Section 5.4 matrix, then run the complete deploy-core/helper and workflow-static suites:

```bash
bash .github/actions/deploy-core/tests/run.sh
.github/actions/deploy-core/tests/check-action-pins.sh
.github/actions/deploy-core/tests/check-doc-action-pins.sh
actionlint
zizmor --offline .github/workflows .github/actions
```

- [ ] Require the baseline `B` commit to pass every current format/test CI matrix job, including all
      four wasm clippy legs and three wasm test runners. Local commands do not substitute for these
      runner-backed checks.

- [ ] Pull `repository@digest` anonymously again after merge and rerun image verification by the
      committed record.

**Gate:** downstream plans build on baseline `B`; they do not reference source revision `S` as an
action ref or recompute a tag digest. Their final integration plan designates full SHA `P` only after
all feature contracts pass.

## 10. Task 6: Release and package-persistence runbook

- [ ] Re-verify the publisher tag pattern, protected environment, GitHub App installation, required
      container checks, and release-review requirement established before `S`; fail if they drifted.
- [ ] For every `update-pin` attempt, including reruns, repeat the administrator-bypass UI capture,
      digest check, and same-reviewer environment approval from Task 4. Record the exact `S`, release tag,
      workflow run ID, reviewer, review time, PNG basename, and SHA-256 in release evidence. Do not treat
      either the pre-`S` capture or another workflow attempt's capture as current.
- [ ] Confirm GHCR package visibility is public and its API record links `stackpop/edgezero` before the
      pin PR can be generated.
- [ ] Document that GHCR provides no enforceable per-version retention lock, repository automation has
      no package-deletion path, and manual administrator deletion can break existing pinned consumers.
      The recovery is an emergency rebuild, full verification, and new pin release; do not claim the old
      digest remains available.
- [ ] Document rollback as reverting to an earlier reviewed `image.json` digest/protocol and pinning
      consumers to the corresponding earlier action SHA. Never move a tag to simulate rollback.
- [ ] Document the release record: image source `S`, digest `D`, pin baseline `B`, final action pin
      `P`, image tag (informational), checksums, and exact third-party action SHAs.
- [ ] Update the parent spec, implementation plan, adoption guide, and public guide in the downstream
      integration plan. Consumer examples must use one full `P` for all EdgeZero references.

## 11. Completion review

Before declaring this plan complete, run two independent reviews:

1. **Contract review:** compare every file and test with design v6.19 Sections 3, 5, 6.2 through 6.6,
   8, 9, and 10. Verify there is one package/validate wire authority, no same-SHA claim, no platform
   identity output, no tag runtime pull, no placeholder, and no legacy `--stage` guidance.
2. **Release-adversary review:** test mutable tags, private package state, stale/idempotent PR branches,
   malformed BuildKit metadata, index manifests, wrong platform/labels/versions/protocol, deleted
   image pin, unrelated PR no-op checks, classifier failure, publication reruns, and concurrent release
   attempts.

The container plan is complete only when source `S`, verified digest `D`, and pin baseline `B` are
recorded and all repository gates pass. The remaining plans may then implement cached compilation and
eventually designate final action revision `P`.
