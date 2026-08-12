#!/usr/bin/env bash
set -euo pipefail

# Negative cache case: a deploy with build-mode: never and cache: true must NOT
# restore the target/ cache (restore is gated on build-mode: always). target/ was
# deleted before this deploy and build-mode: never does no build, so if the marker
# is back, restore ran when it should not have — reintroducing the gap this guards.

ws="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
marker="$ws/fixture-app/target/fixture-build-marker"

if [[ -f "$marker" ]]; then
  echo "::error::cache was restored under build-mode: never (marker '$(cat "$marker")' came back) — restore is not gated on build-mode: always" >&2
  exit 1
fi
echo "build-mode: never + cache: true restored nothing, as required"
