#!/usr/bin/env bash
set -euo pipefail

# Ambiguous-publication recovery, run the way an operator would: extract the
# application CLI recorded by the already authenticated immutable release and
# ask the provider what version is live now. Emits `version=<N>` for rollback.
#
# Reads (env): FASTLY_SERVICE_ID, GITHUB_OUTPUT, RUNNER_TEMP.
# Arg 1: directory containing app-release.tar.gz.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

dir="${1:?usage: recovery-active-version.sh <application-release-dir>}"
service_id="${FASTLY_SERVICE_ID:?FASTLY_SERVICE_ID is required}"
archive="$dir/app-release.tar.gz"
[[ -f "$archive" && ! -L "$archive" ]] || fail "application release archive is missing"
assert_safe_tarball "$archive"

work=$(mktemp -d "${RUNNER_TEMP:?RUNNER_TEMP is required}/edgezero-recovery.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
tar -xOf "$archive" cli/app-cli.tar >"$work/app-cli.tar"
[[ -s "$work/app-cli.tar" ]] || fail "application release has no cli/app-cli.tar member"

cli_output="$work/cli-output"
: >"$cli_output"
GITHUB_OUTPUT="$cli_output" \
EDGEZERO__APP__CLI__ARCHIVE="$work/app-cli.tar" \
EDGEZERO__ACTION__TOOL_ROOT="$work/tools" \
  "$SCRIPT_DIR/../../deploy-core/scripts/download-app-cli.sh"
bin=$(awk -F= '$1 == "app-cli-path" { print substr($0, index($0, "=") + 1); found=1; exit } END { if (!found) exit 1 }' "$cli_output") ||
  fail "application CLI extraction did not publish app-cli-path"
[[ -x "$bin" ]] || fail "recovered application CLI is not executable"

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
