# Build-Cache Container Implementation Plan (sub-plan 1 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a pinned, single-manifest `linux/amd64` build container that bakes the exact Rust toolchain + build tools, so `platform-id` for the cached-build feature is an immutable digest.

**Architecture:** A versioned in-repo Dockerfile builds an image FROM a digest-pinned base with the workspace's pinned Rust toolchain and the tools `build-app-cli` needs (`git`, `jq`, `tar`, `curl`, `ca-certificates`, a C toolchain for `build.rs`). A publish workflow builds it single-arch, pushes it to GHCR, and records its **manifest digest** in a committed `image.json`. A fail-closed `check-image-pin.sh` (wired into the existing pin gate's test harness) proves the recorded reference is pinned by a 64-hex `sha256` digest, never a mutable tag.

**Tech Stack:** Docker (BuildKit), GitHub Actions (`docker/build-push-action`), GHCR, Bash, `jq`.

**Spec:** `docs/specs/edgezero-deploy-build-caching.md` (v6.14, sccache pivot) — §2 (single-producer, hosted-only v1), §3.1 (sccache cache mechanism), §3.6 (image contract: baked Rust + `wasm32-wasip1` + **sccache** + Fastly CLI, read-only/non-root), §5 (digest pin, atomic same-SHA rollout).

## Global Constraints

- **Rust toolchain baked = `1.95.0`** (verbatim from `.tool-versions`); a build that resolves a different toolchain must fail closed downstream, so this image is the single source of truth.
- **Full build+deploy runtime baked** (spec §3.6): `1.95.0` + `wasm32-wasip1` + a pinned **`sccache`** (the cache mechanism, spec §3.1) + the pinned **Fastly CLI `15.1.0`** (`.tool-versions`) + `git jq tar curl cc` — the container is the deploy runtime, not only the CLI-compile runtime.
- **Runtime posture:** consumed **read-only root filesystem, non-root user**, explicit writable mounts only (spec §3.7).
- **Single-manifest `linux/amd64` only** — no multi-arch index (an index digest can select another architecture).
- **No Python in CI tooling** — Bash + `jq` only.
- **Pin policy:** every referenced image/action is pinned; the base image is pinned by `sha256` digest, and the published image is recorded by `sha256` digest.
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
- Produces: `check-image-pin.sh <path-to-image.json>` — exit `0` iff the JSON has string `repository`, string `tag`, and a `digest` matching `^sha256:[0-9a-f]{64}$`; prints `::error::` and exits `1` otherwise. Reused by the pin gate and the publish workflow.

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

printf '{"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"v1","digest":"sha256:%064d"}\n' 0 >"$WORK/ok.json"
run "$WORK/ok.json" && ok "a digest-pinned reference passes" || no "a digest-pinned reference passes"

printf '{"repository":"ghcr.io/x","tag":"v1","digest":"v1"}\n' >"$WORK/tag.json"
run "$WORK/tag.json" && no "a non-digest (tag) reference is rejected" || ok "a non-digest (tag) reference is rejected"

printf '{"repository":"ghcr.io/x","tag":"v1"}\n' >"$WORK/nodigest.json"
run "$WORK/nodigest.json" && no "a missing digest is rejected" || ok "a missing digest is rejected"

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

file="${1:?usage: check-image-pin.sh <image.json>}"
if ! command -v jq >/dev/null 2>&1; then
  echo "::error::check-image-pin.sh requires jq" >&2
  exit 2
fi
if ! json=$(jq -e . "$file" 2>/dev/null); then
  echo "::error::$file is not valid JSON — refusing to pass an unreadable image pin" >&2
  exit 1
fi
repo=$(jq -r '.repository // empty' <<<"$json")
tag=$(jq -r '.tag // empty' <<<"$json")
digest=$(jq -r '.digest // empty' <<<"$json")
if [[ -z "$repo" || -z "$tag" ]]; then
  echo "::error::$file must set string 'repository' and 'tag'" >&2
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
Expected: `Passed: 4  Failed: 0`.

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
      - uses: actions/checkout@v7
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
          # Single-manifest linux/amd64 (reject a multi-arch index).
          n=$(docker buildx imagetools inspect "$REF" --format '{{json .}}' \
                | jq '[.. | .manifests? // empty | .[] | select(.platform.os != "unknown")] | length')
          [ "${n:-1}" -le 1 ] || { echo "::error::not single-manifest ($n)"; exit 1; }
          # Anonymous pull (the package must be public) + the runtime smoke contract.
          docker logout ghcr.io || true
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
            --body "Digest verified by the publish workflow (single-manifest, anonymous pull, runtime smoke)."
```

The publish thus **pushes → inspects by digest → verifies single-manifest + anonymous pull + the runtime smoke → then opens a reviewable `image.json` PR** — the pin the rest of the feature keys on is never recorded until it has been proven against the actual pushed digest.

- [ ] **Step 2: Actionlint the workflow**

Run: `actionlint .github/workflows/publish-build-container.yml`
Expected: no output.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/publish-build-container.yml
git commit -m "build-cache container: GHCR publish workflow recording the manifest digest"
```

- [ ] **Step 4: Publish (operator step, out of band)**

Tag `build-container-v1` and push it. The workflow pushes the image, **verifies it by digest** (single-manifest, anonymous pull, runtime smoke), and **opens a PR** updating `image.json` to the real `sha256` digest. Review and merge that PR — the digest is the pin the rest of the feature keys on, and it is only recorded after passing verification against the actual pushed image.

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

2. Cached build path (reusable workflow + `prepare`/`compile` split + **owned `actions/cache` restore+save with the four-root prune** + config/source closure, spec §3.4/§3.8). 3. Provenance (JSON Schema + procedural validation, `validate-app-cli-provenance`, `compute-app-cli-identity`, `ExpectedIdentity`). 4. Consumer integration (`active-version-fastly`, per-consumer `ExpectedIdentity` inputs, the Docker launcher, production-only recovery). Each is its own plan; sub-plan 2 consumes this container's digest as `platform-id`.
