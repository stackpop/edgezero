#!/usr/bin/env bash
set -euo pipefail

# After the second deploy: target/ was deleted, so if the marker is back AND equals
# the one the first build wrote, the cache RESTORE brought it back (a real hit) — the
# idempotent seed build leaves a restored marker untouched. A different value would
# mean the cache missed and the build re-stamped it.

ws="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
marker="$ws/fixture-app/target/fixture-build-marker"
expected="$ws/cache-marker-expected.txt"

[[ -f "$expected" ]] ||
  { echo "::error::expected-marker file missing — the capture step did not run" >&2; exit 1; }
[[ -f "$marker" ]] ||
  { echo "::error::target/ was not restored: $marker missing after the second deploy" >&2; exit 1; }

got=$(cat "$marker")
want=$(cat "$expected")
if [[ "$got" != "$want" ]]; then
  echo "::error::cache restore FAILED: the marker was rebuilt ('$got') instead of restored ('$want')" >&2
  exit 1
fi
echo "cache restore hit: marker '$got' came back from the cache, not a rebuild"
