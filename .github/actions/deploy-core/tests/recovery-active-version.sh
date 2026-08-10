#!/usr/bin/env bash
set -euo pipefail

# Lost-version recovery, run the way an operator would: extract the CLI from the
# downloaded build-app-cli artifact and ask the provider what version is live NOW
# (the one the failed deploy activated). Emits `version=<N>` as a step output for
# the rollback that follows.
#
# Reads (env): FASTLY_SERVICE_ID, GITHUB_OUTPUT. Arg 1: the artifact download dir.

dir="${1:?usage: recovery-active-version.sh <artifact-dir>}"
service_id="${FASTLY_SERVICE_ID:?FASTLY_SERVICE_ID is required}"

# The transient provider failure that lost the deploy's version has passed: clear the
# fake API-break sentinel so active-version can read the live version.
[[ -n "${FAKE_API_BREAK_FILE:-}" ]] && rm -f "$FAKE_API_BREAK_FILE"

tar -C "$dir" -xf "$dir"/*.tar
bin="$dir/$(jq -r '."app-cli-bin"' "$dir/app-cli-meta.json")"
[[ -x "$bin" ]] || { echo "::error::recovered CLI binary not found or not executable: $bin" >&2; exit 1; }

out=$("$bin" active-version --adapter fastly --service-id "$service_id")
version=$(sed -n 's/^version=//p' <<<"$out")
[[ -n "$version" ]] ||
  { echo "::error::active-version recovered no live version from: $out" >&2; exit 1; }

echo "recovered live version: $version"
printf 'version=%s\n' "$version" >>"${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
