#!/usr/bin/env bash
set -euo pipefail

# Between the two deploys of the cache smoke: capture the marker the credential-free
# seed build wrote into target/ (the cache path), then DELETE target/ so the second
# deploy's marker can only come back from a cache RESTORE — never from what is left
# on disk in the same job.

ws="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
marker="$ws/fixture-app/target/fixture-build-marker"

[[ -f "$marker" ]] ||
  { echo "::error::the credential-free seed build did not populate the cache: $marker missing" >&2; exit 1; }

cp "$marker" "$ws/cache-marker-expected.txt"
echo "seeded marker: $(cat "$marker")"

rm -rf "$ws/fixture-app/target"
echo "deleted target/ — the next deploy must restore it from the cache"
