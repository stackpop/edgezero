#!/usr/bin/env bash
# Unit tests for the build-container digest-pin validator (spec §3.6/§5). The
# validator must accept a sha256-digest-pinned image.json and FAIL CLOSED on a
# tag, a missing digest, or malformed JSON.
set -euo pipefail

DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CHECK="$DIR/../../../docker/build-app-cli/check-image-pin.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
ok() {
  printf '  \033[32mok\033[0m   %s\n' "$1"
  pass=$((pass + 1))
}
no() {
  printf '  \033[31mFAIL\033[0m %s\n' "$1"
  fail=$((fail + 1))
}
run() { bash "$CHECK" "$1" >/dev/null 2>&1; }

echo "== build container image.json digest-pin validator =="

printf '{"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"v1","digest":"sha256:%064d"}\n' 0 >"$WORK/ok.json"
if run "$WORK/ok.json"; then ok "a digest-pinned reference passes"; else no "a digest-pinned reference passes"; fi

printf '{"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"v1","digest":"v1"}\n' >"$WORK/tag.json"
if run "$WORK/tag.json"; then no "a non-digest (tag) reference is rejected"; else ok "a non-digest (tag) reference is rejected"; fi

printf '{"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"v1","digest":"sha256:deadbeef"}\n' >"$WORK/short.json"
if run "$WORK/short.json"; then no "a short/invalid digest is rejected"; else ok "a short/invalid digest is rejected"; fi

printf '{"repository":"ghcr.io/stackpop/edgezero-build-app-cli","tag":"v1"}\n' >"$WORK/nodigest.json"
if run "$WORK/nodigest.json"; then no "a missing digest is rejected"; else ok "a missing digest is rejected"; fi

printf '{"tag":"v1","digest":"sha256:%064d"}\n' 0 >"$WORK/norepo.json"
if run "$WORK/norepo.json"; then no "a missing repository is rejected"; else ok "a missing repository is rejected"; fi

printf '{"repository":"ghcr.io/attacker/edgezero-build-app-cli","tag":"v1","digest":"sha256:%064d"}\n' 0 >"$WORK/foreign.json"
if run "$WORK/foreign.json"; then no "a foreign repository is rejected"; else ok "a foreign repository is rejected"; fi

printf '{"repository":123,"tag":1,"digest":"sha256:%064d"}\n' 0 >"$WORK/numeric.json"
if run "$WORK/numeric.json"; then no "numeric (non-string) repository/tag is rejected"; else ok "numeric (non-string) repository/tag is rejected"; fi

printf 'not json\n' >"$WORK/bad.json"
if run "$WORK/bad.json"; then no "malformed JSON fails closed"; else ok "malformed JSON fails closed"; fi

printf 'Passed: %d  Failed: %d\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
