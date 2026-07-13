#!/usr/bin/env bash
set -euo pipefail

# Installs fake `fastly` and `curl` binaries on PATH for the lifecycle smoke
# test, plus a call log the assertions read back.
#
# These fakes mirror the REAL contracts the adapter depends on, so the smoke
# test regression-tests the bugs a review found:
#   * `fastly compute update` must NOT receive --comment (it does not support it);
#     the comment must be applied via `fastly service-version update` BEFORE
#     `service-version stage`.
#   * `compute update` output must be a realistic success line, because the
#     version parser is now fail-closed (it refuses to guess).
#   * The Fastly domain API returns a SINGULAR `staging_ip` string.
#   * activate/deactivate are PUT, and staging deactivate is /deactivate/staging.
#
# Inputs (environment): GITHUB_WORKSPACE, GITHUB_PATH.

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local bin_dir="$workspace/fake-bin"
  local log="$workspace/fake-calls.log"
  local state="$workspace/fake-state"

  mkdir -p "$bin_dir" "$state"
  : >"$log"

  cat >"$bin_dir/fastly" <<'SH'
#!/usr/bin/env bash
printf 'fastly %s\n' "$*" >>"$FAKE_CALL_LOG"
case "${1:-} ${2:-}" in
  "compute build")
    echo "Built package (fixture)"
    ;;
  "compute update")
    # Realistic success line — the version parser is fail-closed and will
    # refuse to stage if it cannot parse a version from this output.
    echo "SUCCESS: Updated package (service dummy-service, version 42)"
    ;;
  "compute deploy")
    echo "SUCCESS: Deployed package (service dummy-service, version 43)"
    ;;
  "service-version update") echo "Updated version comment" ;;
  "service-version stage") echo "Staged version" ;;
  *) echo "fake fastly: unhandled: $*" >&2 ;;
esac
exit 0
SH
  chmod +x "$bin_dir/fastly"

  cat >"$bin_dir/curl" <<'SH'
#!/usr/bin/env bash
# Two shapes: an API call via `--config -` (config on stdin), or a health probe.
if [[ "$*" == *"--config"* ]]; then
  config=$(cat)
  url=$(printf '%s\n' "$config" | sed -nE 's/^url = "(.*)"$/\1/p')
  if printf '%s\n' "$config" | grep -q '^request = "PUT"$'; then
    printf 'PUT %s\n' "$url" >>"$FAKE_CALL_LOG"
    echo 200
    exit 0
  fi
  printf 'GET %s\n' "$url" >>"$FAKE_CALL_LOG"
  # Fastly returns a SINGULAR `staging_ip` string per domain object.
  printf '[{"name":"staging.example.com","staging_ip":"151.101.2.10"}]\n'
  exit 0
fi
printf 'PROBE %s\n' "$*" >>"$FAKE_CALL_LOG"
if [[ -n "${FORCE_UNHEALTHY:-}" || -f "$FAKE_STATE_DIR/unhealthy" ]]; then
  echo 503
else
  echo 200
fi
exit 0
SH
  chmod +x "$bin_dir/curl"

  # Prepend the fakes so they shadow the real tools for subsequent steps.
  printf '%s\n' "$bin_dir" >>"${GITHUB_PATH:?GITHUB_PATH is required}"
  {
    printf 'FAKE_CALL_LOG=%s\n' "$log"
    printf 'FAKE_STATE_DIR=%s\n' "$state"
  } >>"${GITHUB_ENV:?GITHUB_ENV is required}"

  echo "fake fastly + curl installed at $bin_dir"
}

main "$@"
