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

# Capture the CLI's output AND its exit status: a bare `out=$(…)` under `set -e`
# would abort with an untraced error on a non-zero exit, never reaching the
# diagnostic below.
if ! out=$("$bin" active-version --adapter fastly --service-id "$service_id" 2>&1); then
  echo "::error::active-version failed while recovering the live version:" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi

# Require EXACTLY ONE canonical `version=<digits>` line. The recovery target must be
# a real live version, so — unlike active-version's first-deploy contract, where an
# empty `version=` is valid — an empty, duplicated, or malformed line fails closed.
version_lines=$(grep -E '^version=' <<<"$out" || true)
version_count=$(printf '%s' "$version_lines" | grep -c . || true)
if [[ "$version_count" -ne 1 || ! "$version_lines" =~ ^version=([0-9]+)$ ]]; then
  echo "::error::active-version recovered no unambiguous live version from: $out" >&2
  exit 1
fi
version="${BASH_REMATCH[1]}"

echo "recovered live version: $version"
printf 'version=%s\n' "$version" >>"${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
