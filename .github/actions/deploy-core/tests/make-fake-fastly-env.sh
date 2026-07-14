#!/usr/bin/env bash
set -euo pipefail

# Installs fake `fastly` and `curl` binaries for the lifecycle smoke test, plus a
# call log the assertions read back.
#
# The fakes mirror the REAL contracts the adapter depends on, so the smoke test
# regression-tests the defects a review found:
#   * `fastly compute update` must NOT receive --comment (it has no such flag);
#     the comment goes through `fastly service-version update` BEFORE
#     `service-version stage`.
#   * `compute update` output must be a realistic success line, because the
#     version parser is fail-closed and refuses to guess.
#   * The Fastly domain API returns a SINGULAR `staging_ip` string.
#   * activate/deactivate are PUT, and staging deactivate is /deactivate/staging.
#
# The fake `fastly` is placed in the ACTION-OWNED TOOL ROOT, reporting the pinned
# version, so `install-fastly.sh` adopts it instead of downloading the real CLI
# (its idempotency check). That is what lets the staged path be exercised through
# the real deploy-fastly wrapper rather than by calling the CLI directly.
# The fake `curl` goes on PATH, which nothing reinstalls.
#
# Inputs (environment): GITHUB_WORKSPACE, GITHUB_PATH, GITHUB_ENV, RUNNER_TEMP.

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

write_fake_fastly() {
  local path="$1" version="$2"
  cat >"$path" <<SHIM
#!/usr/bin/env bash
printf 'fastly %s\n' "\$*" >>"\$EDGEZERO__TEST__FAKE_CALL_LOG"
case "\${1:-} \${2:-}" in
  "version ") echo "Fastly CLI version v$version (fake)" ;;
  "compute build") echo "Built package (fixture)" ;;
  "compute update")
    # A realistic success line: the version parser is fail-closed and will
    # refuse to stage if it cannot read a version out of this output.
    echo "SUCCESS: Updated package (service dummy-service, version 42)"
    ;;
  "compute deploy") echo "SUCCESS: Deployed package (service dummy-service, version 43)" ;;
  "service-version update") echo "Updated version comment" ;;
  "service-version stage") echo "Staged version" ;;
  *)
    case "\${1:-}" in
      version | --version) echo "Fastly CLI version v$version (fake)" ;;
      *) echo "fake fastly: unhandled: \$*" >&2 ;;
    esac
    ;;
esac
exit 0
SHIM
  chmod +x "$path"
}

write_fake_curl() {
  local path="$1"
  cat >"$path" <<'SHIM'
#!/usr/bin/env bash
# Two shapes: a Fastly API call via `--config -` (config on stdin), or a probe.
if [[ "$*" == *"--config"* ]]; then
  config=$(cat)
  url=$(printf '%s\n' "$config" | sed -nE 's/^url = "(.*)"$/\1/p')
  if printf '%s\n' "$config" | grep -q '^request = "PUT"$'; then
    printf 'PUT %s\n' "$url" >>"$EDGEZERO__TEST__FAKE_CALL_LOG"
    echo 200
    exit 0
  fi
  printf 'GET %s\n' "$url" >>"$EDGEZERO__TEST__FAKE_CALL_LOG"
  # Fastly returns a SINGULAR `staging_ip` string per domain object.
  printf '[{"name":"staging.example.com","staging_ip":"151.101.2.10"}]\n'
  exit 0
fi
printf 'PROBE %s\n' "$*" >>"$EDGEZERO__TEST__FAKE_CALL_LOG"
if [[ -n "${EDGEZERO__TEST__FORCE_UNHEALTHY:-}" ]]; then
  echo 503
else
  echo 200
fi
exit 0
SHIM
  chmod +x "$path"
}

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local runner_temp="${RUNNER_TEMP:?RUNNER_TEMP is required}"
  local action_dir
  action_dir=$(cd -- "$SCRIPT_DIR/../../deploy-fastly" && pwd)
  local path_dir="$workspace/fake-bin"
  local tool_bin="$runner_temp/edgezero-action-tools/bin"
  local log="$workspace/fake-calls.log"

  mkdir -p "$path_dir" "$tool_bin"
  : >"$log"

  # The fake must claim the pinned version, or install-fastly.sh replaces it.
  local pinned
  pinned=$(json_get "$action_dir/versions.json" fastly.version)

  write_fake_fastly "$tool_bin/fastly" "$pinned"
  write_fake_fastly "$path_dir/fastly" "$pinned"
  write_fake_curl "$path_dir/curl"

  printf '%s\n' "$path_dir" >>"${GITHUB_PATH:?GITHUB_PATH is required}"
  {
    printf 'EDGEZERO__TEST__FAKE_CALL_LOG=%s\n' "$log"
  } >>"${GITHUB_ENV:?GITHUB_ENV is required}"

  notice "fake fastly (v$pinned) installed in the tool root and on PATH; fake curl on PATH"
}

main "$@"
