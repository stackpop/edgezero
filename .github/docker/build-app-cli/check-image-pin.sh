#!/usr/bin/env bash
# Fail-closed: the build container reference must be pinned by a sha256 manifest
# digest, never a mutable tag (spec docs/superpowers/specs/edgezero-deploy-build-caching.md
# §3.6/§5). image.json records the canonical repository, tag, and pinned digest;
# the rest of the build-caching feature keys `platform-id` on that digest, so a
# non-digest or malformed pin must never pass.
#
# Usage: check-image-pin.sh <path-to-image.json>
set -euo pipefail

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

repo=$(jq -r '.repository // empty' <<<"$json")
tag=$(jq -r '.tag // empty' <<<"$json")
digest=$(jq -r '.digest // empty' <<<"$json")

if [[ -z "$repo" || -z "$tag" ]]; then
  echo "::error::$file must set a non-empty string 'repository' and 'tag'" >&2
  exit 1
fi

# A sha256 manifest digest, never a tag.
if [[ ! "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "::error::$file 'digest' must be a sha256 manifest digest (sha256:<64-hex>), not a tag: '$digest'" >&2
  exit 1
fi

echo "build container reference is pinned: $repo@$digest"
