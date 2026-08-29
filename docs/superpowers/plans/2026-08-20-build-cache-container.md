# Build-Cache Container Implementation Plan (sub-plan 1 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a pinned, single-manifest `linux/amd64` build container that bakes the exact Rust toolchain + build tools, so `platform-id` for the cached-build feature is an immutable digest.

**Architecture:** A versioned in-repo Dockerfile builds an image FROM a digest-pinned base with the workspace's pinned Rust toolchain and the tools `build-app-cli` needs (`git`, `jq`, `tar`, `curl`, `ca-certificates`, a C toolchain for `build.rs`). A publish workflow builds it single-arch, pushes it to GHCR, and records its **manifest digest** in a committed `image.json`. A fail-closed `check-image-pin.sh` (wired into the existing pin gate's test harness) proves the recorded reference is pinned by a 64-hex `sha256` digest, never a mutable tag.

**Tech Stack:** Docker (BuildKit), GitHub Actions (`docker/build-push-action`), GHCR, Bash, `jq`.

**Spec:** `docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md` (v6.17, sccache pivot) — §2 (build-only single-producer, hosted-only v1), §3.1 (sccache cache mechanism), §3.6 (image contract: baked Rust + `wasm32-wasip1` + **sccache** + Fastly CLI + baked provenance validator, read-only/non-root), §5 (digest pin, atomic same-SHA rollout).

## Global Constraints

- **Rust toolchain baked = `1.95.0`** (verbatim from `.tool-versions`); a build that resolves a different toolchain must fail closed downstream, so this image is the single source of truth.
- **Full build+deploy runtime baked** (spec §3.6): `1.95.0` + `wasm32-wasip1` + a pinned **`sccache`** (the cache mechanism, spec §3.1) + the pinned **Fastly CLI `15.1.0`** (`.tool-versions`) + `git jq tar curl cc` — the container is the deploy runtime, not only the CLI-compile runtime.
- **Baked provenance validator** (spec §3.7): the image also bakes a single pinned, **project-owned validator binary** (a small Rust tool built from this repo at the same SHA — not a network-fetched helper) that performs JCS canonicalization, duplicate-key detection, JSON-Schema-2020-12 validation, strict `ustar` parsing, and ELF inspection (`jq`/`tar` cannot). Its capabilities are smoke-tested **before the digest is published** (a downstream sub-plan wires the validator itself; this plan reserves its place in the image and the publish smoke).
- **Runtime posture:** consumed **read-only root filesystem, non-root user**, explicit writable mounts only (spec §3.7).
- **Single-manifest `linux/amd64` only** — no multi-arch index (an index digest can select another architecture).
- **No Python in CI tooling** — Bash + `jq` only.
- **Pin policy (risk-tiered, at or above the repo's `check-action-pins.sh` gate):** **images** are pinned by `sha256` digest (the base image's digest in the `FROM`, and the published image's digest recorded in `image.json`). **Actions in this write-privileged publish workflow are pinned to a full 40-hex commit SHA** — GitHub identifies a full commit SHA as the only immutable action reference, and this workflow holds `contents: write` + `packages: write` + `pull-requests: write`, a supply-chain-sensitive privilege class where a re-tagged major version is an unacceptable risk. (Elsewhere in the repo, low-privilege read-only actions follow the standing major-tag convention the gate accepts; **whether to migrate those existing references to SHAs is a separate, repo-wide decision** — see the review note — not made by this container plan.)
- **No AI bylines** in commits or PR bodies.
- **Bash 3.2-compatible** scripts (macOS dev parity); scripts are `shellcheck -S warning` clean.

## File Structure

- `.github/docker/build-app-cli/Dockerfile` — the image definition (one responsibility: the build environment).
- `.github/docker/build-app-cli/image.json` — the published image's canonical reference + digest (the pin record).
- `.github/docker/build-app-cli/check-image-pin.sh` — fail-closed validator of `image.json`.
- `.github/actions/deploy-core/tests/check-image-pin.test.sh` — unit tests for the validator (colocated with the existing action test harness).
- `.github/workflows/publish-build-container.yml` — build + push + digest capture (runs on a `build-container-v*` tag).
- `.github/actions/deploy-core/tests/run.sh` — modified to invoke the new validator suite.

---

### Task 1: Fail-closed `image.json` validator (pure TDD)

**Files:**
- Create: `.github/docker/build-app-cli/check-image-pin.sh`
- Test: `.github/actions/deploy-core/tests/check-image-pin.test.sh`

**Interfaces:**
- Consumes: nothing (leaf).
- Produces: `check-image-pin.sh <path-to-image.json>` — exit `0` iff the JSON has string-typed `repository`/`tag`/`digest`, `repository` **equals the canonical `ghcr.io/stackpop/edgezero-build-app-cli`** (a foreign repository can never become `platform-id`), and `digest` matches `^sha256:[0-9a-f]{64}$`; prints `::error::` and exits `1` otherwise. Reused by the pin gate and the publish workflow. (`image.json` is a committed, PR-reviewed 3-field pin record; its rigor is this type+repo+digest gate. The JCS/JSON-Schema/duplicate-key **provenance** machinery is for *produced* artifacts — `app-cli-meta.json`, spec §3.7 — and belongs to sub-plan 3, not this committed record.)

- [ ] **Step 1: Write the failing test**

```bash
#!/usr/bin/env bash
# .github/actions/deploy-core/tests/check-image-pin.test.sh
set -euo pipefail
DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CHECK="$DIR/../../../docker/build-app-cli/check-image-pin.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0 fail=0
ok(){ printf '  ok   %s\n' "$1"; pass=$((pass+1)); }
no(){ printf '  FAIL %s\n' "$1"; fail=$((fail+1)); }
run(){ bash "$CHECK" "$1" >/dev/null 2>&1; }

R="ghcr.io/stackpop/edgezero-build-app-cli"
printf '{"repository":"%s","tag":"v1","digest":"sha256:%064d"}\n' "$R" 0 >"$WORK/ok.json"
run "$WORK/ok.json" && ok "a digest-pinned reference passes" || no "a digest-pinned reference passes"

printf '{"repository":"%s","tag":"v1","digest":"v1"}\n' "$R" >"$WORK/tag.json"
run "$WORK/tag.json" && no "a non-digest (tag) reference is rejected" || ok "a non-digest (tag) reference is rejected"

printf '{"repository":"%s","tag":"v1"}\n' "$R" >"$WORK/nodigest.json"
run "$WORK/nodigest.json" && no "a missing digest is rejected" || ok "a missing digest is rejected"

printf '{"repository":"ghcr.io/attacker/edgezero-build-app-cli","tag":"v1","digest":"sha256:%064d"}\n' 0 >"$WORK/foreign.json"
run "$WORK/foreign.json" && no "a foreign repository is rejected" || ok "a foreign repository is rejected"

printf 'not json\n' >"$WORK/bad.json"
run "$WORK/bad.json" && no "malformed JSON fails closed" || ok "malformed JSON fails closed"

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
```

- [ ] **Step 2: Run it to verify it fails**

Run: `bash .github/actions/deploy-core/tests/check-image-pin.test.sh`
Expected: FAIL (the `check-image-pin.sh` file does not exist yet).

- [ ] **Step 3: Write the minimal implementation**

```bash
#!/usr/bin/env bash
# .github/docker/build-app-cli/check-image-pin.sh
# Fail-closed: the build container reference must be pinned by a sha256 digest,
# never a mutable tag (spec §3.7/§5). Requires mikefarah yq/jq-free: uses jq.
set -euo pipefail

EXPECTED_REPO="ghcr.io/stackpop/edgezero-build-app-cli"
file="${1:?usage: check-image-pin.sh <image.json>}"
if ! command -v jq >/dev/null 2>&1; then
  echo "::error::check-image-pin.sh requires jq" >&2
  exit 2
fi
if ! json=$(jq -e . "$file" 2>/dev/null); then
  echo "::error::$file is not valid JSON — refusing to pass an unreadable image pin" >&2
  exit 1
fi
# String TYPES (jq -r would coerce a numeric value to a string).
if [[ "$(jq -r '.repository|type' <<<"$json")" != string ||
  "$(jq -r '.tag|type' <<<"$json")" != string ||
  "$(jq -r '.digest|type' <<<"$json")" != string ]]; then
  echo "::error::$file 'repository', 'tag', 'digest' must be JSON strings" >&2
  exit 1
fi
repo=$(jq -r '.repository' <<<"$json"); tag=$(jq -r '.tag' <<<"$json"); digest=$(jq -r '.digest' <<<"$json")
if [[ -z "$repo" || -z "$tag" ]]; then
  echo "::error::$file must set non-empty 'repository' and 'tag'" >&2
  exit 1
fi
# The repository must be the canonical EdgeZero build container, not merely non-empty.
if [[ "$repo" != "$EXPECTED_REPO" ]]; then
  echo "::error::$file 'repository' must be '$EXPECTED_REPO', not '$repo'" >&2
  exit 1
fi
if [[ ! "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "::error::$file 'digest' must be a sha256 manifest digest (sha256:<64-hex>), not a tag: '$digest'" >&2
  exit 1
fi
echo "build container reference is pinned: $repo@$digest"
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `chmod +x .github/docker/build-app-cli/check-image-pin.sh && bash .github/actions/deploy-core/tests/check-image-pin.test.sh`
Expected: `Passed: N  Failed: 0` (the committed test carries the full case set — string-type, foreign-repo, tag, short/missing digest, missing repository, malformed JSON).

- [ ] **Step 5: Shellcheck**

Run: `shellcheck -S warning .github/docker/build-app-cli/check-image-pin.sh`
Expected: no output (clean).

- [ ] **Step 6: Commit**

```bash
git add .github/docker/build-app-cli/check-image-pin.sh .github/actions/deploy-core/tests/check-image-pin.test.sh
git commit -m "build-cache container: fail-closed image.json digest-pin validator"
```

---

### Task 2: The pinned Dockerfile

**Files:**
- Create: `.github/docker/build-app-cli/Dockerfile`
- Create: `.github/docker/build-app-cli/image.json` (placeholder digest until Task 3 publishes)

**Interfaces:**
- Consumes: the Global Constraints (Rust `1.95.0`, single-arch amd64).
- Produces: an image whose `rustc --version` is `1.95.0` and which has `git jq tar curl cc` on `PATH`; consumed by Task 3's publish and by sub-plans 2–4 as `platform-id`.

- [ ] **Step 1: Write the Dockerfile**

```dockerfile
# .github/docker/build-app-cli/Dockerfile
# Single-manifest linux/amd64 FULL build+deploy runtime (spec §3.7): the pinned
# Rust toolchain, wasm32-wasip1, the pinned Fastly CLI, and build tools. This
# image IS the toolchain/ABI identity; it runs read-only/non-root at runtime.
# Base pinned by digest; replace the digest below with a current
# rust:1.95.0-bookworm linux/amd64 manifest digest (see README in this dir).
FROM rust:1.95.0-bookworm@sha256:0000000000000000000000000000000000000000000000000000000000000000

# Pinned downloads (spec §3.6): fastly 15.1.0 (versions.json) and a pinned sccache.
# Each ARG carries the exact release URL + sha256 (fill the sccache values from the
# chosen sccache release; the fastly values are versions.json's).
ARG FASTLY_URL="https://github.com/fastly/cli/releases/download/v15.1.0/fastly_v15.1.0_linux-amd64.tar.gz"
ARG FASTLY_SHA256="3ba3d8a739b7a88d0a612825a9755d735efb87a9b02ea67e53a11b96d178d500"
ARG SCCACHE_VERSION="0.10.0"
ARG SCCACHE_URL="https://github.com/mozilla/sccache/releases/download/v0.10.0/sccache-v0.10.0-x86_64-unknown-linux-musl.tar.gz"
ARG SCCACHE_SHA256="REPLACE_WITH_RELEASE_SHA256"

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      git jq tar curl ca-certificates build-essential; \
    rm -rf /var/lib/apt/lists/*; \
    rustup target add wasm32-wasip1; \
    curl -fsSL -o /tmp/fastly.tar.gz "$FASTLY_URL"; \
    echo "${FASTLY_SHA256}  /tmp/fastly.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/fastly.tar.gz -C /usr/local/bin fastly; \
    curl -fsSL -o /tmp/sccache.tar.gz "$SCCACHE_URL"; \
    echo "${SCCACHE_SHA256}  /tmp/sccache.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/sccache.tar.gz --strip-components=1 -C /usr/local/bin "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache"; \
    chmod +x /usr/local/bin/sccache; \
    rm /tmp/fastly.tar.gz /tmp/sccache.tar.gz; \
    fastly version; sccache --version

# No ambient rustflags/wrapper env (spec §3.8 also scrubs at runtime); non-root.
ENV CARGO_TERM_COLOR=never RUSTFLAGS="" CARGO_ENCODED_RUSTFLAGS=""
RUN useradd -m -u 1001 build
USER build
WORKDIR /home/build
```

> The Fastly CLI download is checksum-verified against `versions.json`'s pinned
> `sha256` (above). The publish workflow (Task 3) builds on a hosted runner and
> **makes the GHCR package public** (GHCR packages are private on first publish); the
> image is consumed **read-only/non-root** with explicit writable mounts (spec §3.7).

- [ ] **Step 2: Write the placeholder pin record**

```json
{
  "repository": "ghcr.io/stackpop/edgezero-build-app-cli",
  "tag": "build-container-v1",
  "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
}
```

(The placeholder digest is intentional; Task 3's publish workflow overwrites it with the real one, and `check-image-pin.sh` still passes on shape. The pin-gate wiring in Task 4 additionally forbids the all-zero placeholder in a release.)

- [ ] **Step 3: Verify the image builds and bakes the toolchain (local integration check)**

Run (requires Docker + a real base digest substituted into the `FROM`):
```bash
docker build --platform linux/amd64 -t edgezero-build-app-cli:local .github/docker/build-app-cli
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local rustc --version
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local rustc --print target-list | grep -x wasm32-wasip1
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local fastly version
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local sccache --version
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local sh -c 'command -v git jq tar curl cc'
# read-only/non-root smoke (spec §3.7): a read-only rootfs run still works with a tmpfs.
docker run --rm --read-only --tmpfs /tmp --user 1001 --platform linux/amd64 edgezero-build-app-cli:local rustc --version
```
Expected: `rustc 1.95.0 (...)`, `wasm32-wasip1` present, `fastly` reports 15.1.0, all five tools resolve, and the read-only/non-root run succeeds.

- [ ] **Step 4: Commit**

```bash
git add .github/docker/build-app-cli/Dockerfile .github/docker/build-app-cli/image.json
git commit -m "build-cache container: pinned single-arch Dockerfile + image pin record"
```

---

### Task 3: Publish workflow (build, push, record digest)

**Files:**
- Create: `.github/workflows/publish-build-container.yml`

**Interfaces:**
- Consumes: `.github/docker/build-app-cli/Dockerfile`, `check-image-pin.sh`.
- Produces: a GHCR image `ghcr.io/stackpop/edgezero-build-app-cli` whose **manifest digest** is written back to `image.json` on the release tag; consumed by sub-plans 2–4.

- [ ] **Step 1: Write the workflow**

```yaml
# .github/workflows/publish-build-container.yml
name: Publish build container
on:
  push:
    tags: ["build-container-v*"]
permissions:
  contents: write # push the pin branch
  packages: write # push the image to GHCR
  pull-requests: write # open the image.json PR
jobs:
  publish:
    runs-on: ubuntu-24.04
    steps:
      # SHA-PINNED (not @v7): this job is write-privileged (contents/packages/PRs),
      # so every action is pinned to a full 40-hex commit SHA — the only immutable
      # action reference. Replace <full-40-hex> with the pinned actions/checkout
      # release SHA (recorded in a comment as its version, e.g. # v4.3.0).
      - uses: actions/checkout@<full-40-hex-commit-sha> # vX.Y.Z
        # Trusted publish job (no app code runs here); keep the token so the
        # pin-record PR branch can be pushed.
        with:
          persist-credentials: true
      - name: Log in to GHCR
        run: echo "${{ secrets.GITHUB_TOKEN }}" | docker login ghcr.io -u "${{ github.actor }}" --password-stdin
      - name: Build and push (single-arch amd64)
        id: push
        run: |
          set -euo pipefail
          REPO="ghcr.io/stackpop/edgezero-build-app-cli"
          TAG="${GITHUB_REF_NAME}"
          docker buildx build --platform linux/amd64 \
            --provenance=false --sbom=false \
            --tag "$REPO:$TAG" --push .github/docker/build-app-cli
          DIGEST=$(docker buildx imagetools inspect "$REPO:$TAG" --format '{{json .Manifest.Digest}}' | tr -d '"')
          echo "digest=$DIGEST" >> "$GITHUB_OUTPUT"
      - name: Verify the pushed image BY DIGEST before recording it
        env:
          REPO: ghcr.io/stackpop/edgezero-build-app-cli
          DIGEST: ${{ steps.push.outputs.digest }}
        run: |
          set -euo pipefail
          REF="$REPO@$DIGEST"
          # Require a LEAF image manifest, not an index — reject ANY manifest list,
          # including a one-entry OCI index (a count `<= 1` would wrongly accept it,
          # and an index digest can be repointed to select a different image). The
          # digest must resolve to an image manifest (has .config + .layers, no
          # .manifests), whose platform is linux/amd64.
          mt=$(docker buildx imagetools inspect "$REF" --raw | jq -r '.mediaType // ""')
          case "$mt" in
            *"image.index"*|*"manifest.list"*)
              echo "::error::$REF is an index/manifest-list ($mt), not a leaf image manifest"; exit 1 ;;
          esac
          docker buildx imagetools inspect "$REF" --raw \
            | jq -e '(.config != null) and (.layers != null) and (.manifests == null)' >/dev/null \
            || { echo "::error::$REF is not a leaf image manifest (config+layers, no manifests)"; exit 1; }
          plat=$(docker buildx imagetools inspect "$REF" --format '{{json .Image.Platform}}')
          echo "$plat" | jq -e '.os=="linux" and .architecture=="amd64"' >/dev/null \
            || { echo "::error::$REF is not linux/amd64 ($plat)"; exit 1; }
          # Runtime smoke, pulled with the AUTHENTICATED session (a GHCR package is
          # PRIVATE on first publish, so an anonymous pull here would deadlock the very
          # first release). The anonymous-pull check is the operator's post-make-public
          # step below, once the package visibility is public.
          docker run --rm --platform linux/amd64 "$REF" rustc --version | grep -F '1.95.0'
          docker run --rm --platform linux/amd64 "$REF" sh -c 'rustc --print target-list | grep -qx wasm32-wasip1'
          docker run --rm --platform linux/amd64 "$REF" fastly version
          docker run --rm --platform linux/amd64 "$REF" sccache --version
          docker run --rm --read-only --tmpfs /tmp --user 1001 --platform linux/amd64 "$REF" rustc --version
      - name: Open a reviewable image.json PR (not an in-place commit)
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          DIGEST: ${{ steps.push.outputs.digest }}
        run: |
          set -euo pipefail
          f=.github/docker/build-app-cli/image.json
          jq --arg t "${GITHUB_REF_NAME}" --arg d "${DIGEST}" '.tag=$t | .digest=$d' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
          bash .github/docker/build-app-cli/check-image-pin.sh "$f"
          br="build-container-pin-${GITHUB_REF_NAME}"
          git switch -c "$br"
          git add "$f"
          git -c user.name=edgezero-ci -c user.email=ci@stackpop \
            commit -m "build container: pin ${GITHUB_REF_NAME} = ${DIGEST}"
          git push -u origin "$br"
          gh pr create --fill --base main --head "$br" \
            --title "Pin build container ${GITHUB_REF_NAME}" \
            --body "Digest verified by the publish workflow (single-manifest + authenticated runtime smoke). Anonymous-pull verification is the operator's post-make-public step."
```

The publish thus **pushes → inspects by digest → verifies single-manifest + the runtime smoke (authenticated) → then opens a reviewable `image.json` PR** — the pin the rest of the feature keys on is never recorded until it has been proven against the actual pushed digest. The **anonymous** pull is verified separately, after the operator makes the package public (below), avoiding a first-publish deadlock.

- [ ] **Step 2: Actionlint the workflow**

Run: `actionlint .github/workflows/publish-build-container.yml` (after substituting the real
`actions/checkout` release SHA for the `<full-40-hex-commit-sha>` placeholder, as with the
Dockerfile's base-image digest).
Expected: no output.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/publish-build-container.yml
git commit -m "build-cache container: GHCR publish workflow recording the manifest digest"
```

- [ ] **Step 4: Publish (operator step, out of band)**

Tag `build-container-v1` and push it. The workflow pushes the image, **verifies it by digest** (leaf image manifest, linux/amd64 + an **authenticated** runtime smoke — the package is private on first publish), and **opens a PR** updating `image.json` to the real `sha256` digest. Ordering matters: **review and merge the PR FIRST** — only then does the committed `image.json` carry the real digest — **then make the GHCR package public and verify the anonymous pull reading the merged `image.json`** (verifying before merge would read the still-placeholder digest). The digest is the pin the rest of the feature keys on, and it is only recorded after passing verification against the actual pushed image.

**One-time GHCR visibility + retention (operator):** GHCR packages are **private on first publish** and there is no clean REST endpoint to flip a container package public, so set the package `edgezero-build-app-cli` to **public** in its GHCR package settings (or set the org's default package visibility) so consumers can **anonymously** pull by digest (spec §3.7), and enable a retention policy that never prunes a digest referenced by a committed `image.json`. Verify anonymous access:
```bash
docker logout ghcr.io
docker pull "ghcr.io/stackpop/edgezero-build-app-cli@$(jq -r .digest .github/docker/build-app-cli/image.json)"
```
Expected: the pull succeeds without credentials.

---

### Task 4: Wire the digest pin into the pin gate

**Files:**
- Modify: `.github/actions/deploy-core/tests/run.sh` (add the validator suite)
- Modify: `.github/actions/deploy-core/tests/check-image-pin.test.sh` (add a reject-placeholder case)

**Interfaces:**
- Consumes: `check-image-pin.sh`, `image.json`.
- Produces: a CI gate that fails if the build container is not digest-pinned (or is the all-zero placeholder), alongside the existing action-pin gate.

- [ ] **Step 1: Add the failing placeholder-rejection test**

Append to `check-image-pin.test.sh` (before the summary), a case asserting the real repo `image.json` is not the all-zero placeholder:

```bash
REAL="$DIR/../../../docker/build-app-cli/image.json"
zero="sha256:$(printf '%064d' 0)"
if [ "$(jq -r '.digest' "$REAL")" = "$zero" ]; then
  no "committed image.json is still the all-zero placeholder"
else
  ok "committed image.json carries a real digest"
fi
```

- [ ] **Step 2: Run it to verify it fails**

Run: `bash .github/actions/deploy-core/tests/check-image-pin.test.sh`
Expected: FAIL on "committed image.json carries a real digest" until Task 3's publish lands a real digest.

- [ ] **Step 3: Invoke the suite from the contract runner**

Add to `.github/actions/deploy-core/tests/run.sh` (near the other suite invocations):

```bash
bash "$(dirname -- "${BASH_SOURCE[0]}")/check-image-pin.test.sh"
```

- [ ] **Step 4: Run the full suite**

Run: `bash .github/actions/deploy-core/tests/run.sh`
Expected: the image-pin cases run and (after Task 3) pass.

- [ ] **Step 5: Commit**

```bash
git add .github/actions/deploy-core/tests/run.sh .github/actions/deploy-core/tests/check-image-pin.test.sh
git commit -m "build-cache container: gate the build-container digest pin in the contract suite"
```

---

## Self-Review

- **Spec coverage (container scope only):** §3.7 image contract → Tasks 2/3; digest = `platform-id` → Tasks 2/3; single-manifest amd64 → Task 3 (`--platform linux/amd64`, single-arch); baked toolchain `1.95.0` → Task 2 + verify; digest pinned/checked (§5) → Tasks 1/4. The *use* of the container (reusable workflow, launcher, provenance) is sub-plans 2–4, out of scope here.
- **Placeholder scan:** the only intentional placeholder is the all-zero digest, which Task 3 overwrites and Task 4 forbids in a release — flagged, not silent.
- **Type consistency:** `check-image-pin.sh <image.json>` contract is used identically in Tasks 1, 3, 4; the `image.json` keys (`repository`/`tag`/`digest`) match across Tasks 1–4.

## Downstream sub-plans (not written yet)

2. Cached build path (reusable workflow + `prepare`/`compile` split + **an action-owned `sccache` disk cache**: fresh `CARGO_TARGET_DIR` + owned `actions/cache` restore/save over `SCCACHE_DIR` under a bounded rolling generation key + the constructed minimal env + config/source closure, spec §3.1–§3.4/§3.8). 3. Provenance (JCS canonical JSON + JSON Schema + procedural validation, `validate-app-cli-provenance`, `compute-app-cli-identity`, `ExpectedIdentity`). 4. Consumer integration (`active-version-fastly`, per-consumer `ExpectedIdentity` inputs, the Docker launcher, production-only recovery). Each is its own plan; sub-plan 2 consumes this container's digest as `platform-id`.
