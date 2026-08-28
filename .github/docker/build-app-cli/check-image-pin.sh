#!/usr/bin/env bash
# Fail-closed: the build container reference must be the canonical EdgeZero GHCR
# repository, pinned by a sha256 manifest digest, never a mutable tag (spec
# docs/superpowers/specs/2026-08-20-edgezero-deploy-build-caching-design.md
# §3.6/§5). image.json records the canonical repository, tag, and pinned digest;
# the rest of the build-caching feature keys `platform-id` on that digest, so a
# non-digest, malformed, or foreign-repository pin must never pass.
#
# Usage: check-image-pin.sh <path-to-image.json>
set -euo pipefail

# The one repository the build-caching feature trusts; a pin naming any other
# repository is rejected so a foreign image can never become `platform-id`.
EXPECTED_REPO="ghcr.io/stackpop/edgezero-build-app-cli"

file="${1:?usage: check-image-pin.sh <image.json>}"

if ! command -v jq >/dev/null 2>&1; then
  echo "::error::check-image-pin.sh requires jq" >&2
  exit 2
fi

# FAIL CLOSED on unreadable JSON: a file jq cannot parse must be rejected, never
# silently passed.
if ! json=$(jq -e . "$file" 2>/dev/null); then
  echo "::error::$file is not valid JSON — refusing to pass an unreadable image pin" >&2
  exit 1
fi

# Require string TYPES: `jq -r` would coerce a numeric repository/tag/digest to a
# string, so a `"repository": 123` would otherwise slip through. Check the JSON type.
if [[ "$(jq -r '.repository | type' <<<"$json")" != "string" ||
  "$(jq -r '.tag | type' <<<"$json")" != "string" ||
  "$(jq -r '.digest | type' <<<"$json")" != "string" ]]; then
  echo "::error::$file 'repository', 'tag', and 'digest' must all be JSON strings" >&2
  exit 1
fi

repo=$(jq -r '.repository' <<<"$json")
tag=$(jq -r '.tag' <<<"$json")
digest=$(jq -r '.digest' <<<"$json")

if [[ -z "$repo" || -z "$tag" ]]; then
  echo "::error::$file must set a non-empty 'repository' and 'tag'" >&2
  exit 1
fi

# The repository must be the canonical EdgeZero build container, not merely
# non-empty: `platform-id` is trusted, so a foreign repository must never pass.
if [[ "$repo" != "$EXPECTED_REPO" ]]; then
  echo "::error::$file 'repository' must be '$EXPECTED_REPO', not '$repo'" >&2
  exit 1
fi

# A sha256 manifest digest, never a tag.
if [[ ! "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "::error::$file 'digest' must be a sha256 manifest digest (sha256:<64-hex>), not a tag: '$digest'" >&2
  exit 1
fi

echo "build container reference is pinned: $repo@$digest"
