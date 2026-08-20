# Build-Cache Container Implementation Plan (sub-plan 1 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish a pinned, single-manifest `linux/amd64` build container that bakes the exact Rust toolchain + build tools, so `platform-id` for the cached-build feature is an immutable digest.

**Architecture:** A versioned in-repo Dockerfile builds an image FROM a digest-pinned base with the workspace's pinned Rust toolchain and the tools `build-app-cli` needs (`git`, `jq`, `tar`, `curl`, `ca-certificates`, a C toolchain for `build.rs`). A publish workflow builds it single-arch, pushes it to GHCR, and records its **manifest digest** in a committed `image.json`. A fail-closed `check-image-pin.sh` (wired into the existing pin gate's test harness) proves the recorded reference is pinned by a 64-hex `sha256` digest, never a mutable tag.

**Tech Stack:** Docker (BuildKit), GitHub Actions (`docker/build-push-action`), GHCR, Bash, `jq`.

**Spec:** `docs/specs/edgezero-deploy-build-caching.md` (v6.11) — §2 (container-only v1), §3.7 (image contract), §5 (digest pin in the pin gate).

## Global Constraints

- **Rust toolchain baked = `1.95.0`** (verbatim from `.tool-versions`); a build that resolves a different toolchain must fail closed downstream, so this image is the single source of truth.
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
# Single-manifest linux/amd64 build environment for build-app-cli (spec §3.7).
# Base pinned by digest; replace the digest below with a current
# rust:1.95.0-bookworm linux/amd64 manifest digest (see README in this dir).
FROM rust:1.95.0-bookworm@sha256:0000000000000000000000000000000000000000000000000000000000000000

# Tools build-app-cli and its build scripts need. Pin nothing that Cargo keys;
# this image IS the toolchain/ABI identity.
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      git jq tar curl ca-certificates; \
    rm -rf /var/lib/apt/lists/*

# Non-root, no ambient rustflags/wrapper env (spec §3.8 scrub happens at runtime too).
ENV CARGO_TERM_COLOR=never RUSTFLAGS="" CARGO_ENCODED_RUSTFLAGS=""
RUN useradd -m -u 1001 build
USER build
WORKDIR /home/build
```

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
docker run --rm --platform linux/amd64 edgezero-build-app-cli:local sh -c 'command -v git jq tar curl cc'
```
Expected: `rustc 1.95.0 (...)`, and all five tools resolve.

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
  contents: read
  packages: write
jobs:
  publish:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v7
        with:
          persist-credentials: false
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
      - name: Record and validate the pin
        run: |
          set -euo pipefail
          f=.github/docker/build-app-cli/image.json
          jq --arg t "${GITHUB_REF_NAME}" --arg d "${{ steps.push.outputs.digest }}" \
             '.tag=$t | .digest=$d' "$f" > "$f.tmp" && mv "$f.tmp" "$f"
          bash .github/docker/build-app-cli/check-image-pin.sh "$f"
          cat "$f"
```

- [ ] **Step 2: Actionlint the workflow**

Run: `actionlint .github/workflows/publish-build-container.yml`
Expected: no output.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/publish-build-container.yml
git commit -m "build-cache container: GHCR publish workflow recording the manifest digest"
```

- [ ] **Step 4: Publish (operator step, out of band)**

Tag `build-container-v1` and push it; confirm the workflow updates `image.json` with a real `sha256` digest and that `check-image-pin.sh` passes on it. Commit the updated `image.json` (the digest is the pin the rest of the feature keys on).

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

2. Cached build path (reusable workflow + `prepare`/`compile` split + rust-cache + owned save + config/source closure). 3. Provenance (JSON Schema, `validate-app-cli-provenance`, `compute-app-cli-identity`, composite outputs). 4. Consumer integration (`active-version-fastly`, per-consumer `expected-*`, the Docker launcher, recovery). Each is its own plan; sub-plan 2 consumes this container's digest as `platform-id`.
