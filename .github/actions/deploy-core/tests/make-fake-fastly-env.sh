#!/usr/bin/env bash
set -euo pipefail

# Installs fake `fastly` and `curl` binaries for the lifecycle smoke test, plus a
# call log the assertions read back.
#
# The fakes mirror the REAL contracts the adapter depends on, so the smoke test
# exercises the exact call shapes that matter:
#   * `fastly compute update` must NOT receive --comment (it has no such flag);
#     the comment goes through `fastly service-version update` BEFORE
#     `service-version stage`.
#   * `compute update` output must be a realistic success line, because the
#     version parser is fail-closed and refuses to guess.
#   * The Fastly domain API returns a SINGULAR `staging_ip` string.
#   * activate/deactivate are PUT, and staging deactivate is /deactivate/staging.
#
# The fake `fastly` is packaged as a tar.gz at install-fastly.sh's cache path and
# the checked-out `versions.json` is repointed at it with a matching SHA-256, so
# install-fastly.sh VERIFIES and extracts the fake through its real
# download+checksum+extract path — never adopting a planted binary. That lets the
# staged path be exercised through the real deploy-fastly wrapper while keeping
# the installer's provenance guarantee intact. The fake `curl` goes on PATH,
# which nothing reinstalls.
#
# The fake binaries write their call log to FAKE_CALL_LOG and read FORCE_UNHEALTHY.
# These are deliberately OUTSIDE the EDGEZERO__ namespace: the app CLI scrubs
# every EDGEZERO__* var before exec, and these must survive that scrub because
# the fake fastly/curl are spawned BY the app CLI and read them there.
#
# Reads (env): GITHUB_WORKSPACE, GITHUB_PATH, GITHUB_ENV, RUNNER_TEMP.
# Writes (env): FAKE_CALL_LOG (the call-log path).

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../scripts/common.sh
source "$SCRIPT_DIR/../scripts/common.sh"

write_fake_fastly() {
  local path="$1" version="$2"
  cat >"$path" <<SHIM
#!/usr/bin/env bash
printf 'fastly %s\n' "\$*" >>"\$FAKE_CALL_LOG"
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
  # An app WITH config selection: the app config store, the production selector
  # store edgezero_runtime_env (so a staged deploy relinks rather than skipping),
  # and its staging twin (the store the relink points at). config push resolves a
  # store id by name from this list, reads the current entry to diff, then upserts.
  "config-store list") echo '[{"id":"STOREID1","name":"app_config"},{"id":"ENVSEL1","name":"edgezero_runtime_env"},{"id":"STAGESEL1","name":"edgezero_runtime_env_staging_dummy-service"}]' ;;
  # A cloned draft inherits the active version's links; the staged deploy drops
  # this one and re-links the staging store under the same name.
  "resource-link list") echo '[{"id":"LINK_ENV","name":"edgezero_runtime_env"}]' ;;
  "resource-link delete") echo "SUCCESS: Deleted resource link" ;;
  "resource-link create") echo "SUCCESS: Created resource link" ;;
  "config-store-entry describe")
    # Report the key as absent so the push proceeds to a first write. The real
    # CLI distinguishes "missing" from "unparseable" — returning nothing at all
    # is a parse error, not an absent key.
    echo "Error: config store entry not found" >&2
    exit 1
    ;;
  "config-store-entry list")
    # A staged deploy MIRRORS the production selector store into the staging twin.
    # Production (ENVSEL1) carries a non-config override the twin must copy
    # verbatim; the twin (STAGESEL1) starts empty.
    case "\$*" in
      *--store-id=ENVSEL1*) echo '[{"item_key":"EDGEZERO__ADAPTER__FASTLY__LOG_LEVEL","item_value":"debug"}]' ;;
      *) echo '[]' ;;
    esac
    ;;
  "config-store-entry update") echo "SUCCESS: Updated config store entry" ;;
  "config-store-entry delete") echo "SUCCESS: Deleted config store entry" ;;
  *)
    case "\${1:-}" in
      version | --version) echo "Fastly CLI version v$version (fake)" ;;
      # An UNHANDLED command must fail: an unexpected provider call (a new command
      # the code started issuing) should break the smoke, not pass silently.
      *) echo "fake fastly: unhandled command: \$*" >&2; exit 90 ;;
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
    printf 'PUT %s\n' "$url" >>"$FAKE_CALL_LOG"
    echo 200
    exit 0
  fi
  printf 'GET %s\n' "$url" >>"$FAKE_CALL_LOG"
  # fastly_api_get appends `write-out = "\n%{http_code}"`, so the real curl emits
  # `<body>\n<status>` and the caller requires a 2xx. Mirror that: body, then a
  # trailing `\n200`, with NO trailing newline after the code.
  #
  # The service-version list. The ACTIVE version is read from a state file so the
  # smoke can model reality: it is 40 before the production deploy (rollback-target
  # capture), and a deploy (or a test step) updates it. The production-rollback
  # compare-and-swap guard requires the active version to equal the `--version`
  # being rolled back from.
  if [[ "$url" == */version ]]; then
    active=$(cat "${FAKE_ACTIVE_VERSION_FILE:-/dev/null}" 2>/dev/null || true)
    active="${active:-40}"
    printf '[{"number":1,"active":false},{"number":%s,"active":true}]\n200' "$active"
    exit 0
  fi
  # Domain lookup: Fastly returns a SINGULAR `staging_ip` string per domain.
  printf '[{"name":"staging.example.com","staging_ip":"151.101.2.10"}]\n200'
  exit 0
fi
printf 'PROBE %s\n' "$*" >>"$FAKE_CALL_LOG"
if [[ -n "${FORCE_UNHEALTHY:-}" ]]; then
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
  # install-fastly.sh extracts the provider CLI from a checksum-verified archive
  # into `<tool root>/provider-bin`, caching the archive under `downloads/`.
  # Deliver the fake THROUGH that verified path (not by planting a binary), so the
  # smoke exercises the real download+verify+extract and never relies on a bypass.
  local downloads="$runner_temp/edgezero-action-tools/downloads"
  local log="$workspace/fake-calls.log"

  mkdir -p "$path_dir" "$downloads"
  : >"$log"

  local pinned
  pinned=$(json_get "$action_dir/versions.json" fastly.version)

  # Regression guard against the installer trust-by-version-text bypass: plant a
  # DIFFERENT binary at the extract target that REPORTS the pinned version but
  # errors on every real command. install-fastly.sh must overwrite it from the
  # verified archive; if it ever went back to adopting a pre-seeded binary by its
  # version text, THIS one would survive and the staged deploy's first real
  # `fastly` call would exit non-zero — failing the smoke.
  local provider_bin="$runner_temp/edgezero-action-tools/provider-bin"
  mkdir -p "$provider_bin"
  # `$1`/`$2` here are the PLANTED script's args, not this script's — intentionally
  # literal (single-quoted printf format).
  # shellcheck disable=SC2016
  printf '#!/bin/sh\ncase "$1" in\n  version|--version) echo "Fastly CLI version v%s (planted, should have been overwritten)";;\n  *) echo "install-fastly adopted a planted binary instead of extracting the verified archive" >&2; exit 97;;\nesac\n' "$pinned" >"$provider_bin/fastly"
  chmod +x "$provider_bin/fastly"

  # Package a fake `fastly` as the archive install-fastly.sh expects, pre-placed
  # at its cache path so no network fetch happens.
  local stage archive sha
  stage=$(mktemp -d)
  write_fake_fastly "$stage/fastly" "$pinned"
  archive="$downloads/fastly-$pinned-linux-amd64.tar.gz"
  tar -C "$stage" -czf "$archive" fastly
  sha=$(sha256_file "$archive")

  # Repoint the CHECKED-OUT versions.json (what the local action reads) at the
  # fake archive with its real checksum, so install-fastly.sh verifies and
  # extracts the fake. The version stays pinned, so the `.tool-versions`
  # agreement check still holds. This modifies only the job's checkout, never a
  # committed file — production reads the real, pinned versions.json.
  local patched
  patched=$(mktemp)
  jq --arg url "file://$archive" --arg sha "$sha" \
    '.fastly.linux_amd64.url = $url | .fastly.linux_amd64.sha256 = $sha' \
    "$action_dir/versions.json" >"$patched"
  mv "$patched" "$action_dir/versions.json"

  write_fake_curl "$path_dir/curl"

  # The active version the fake Fastly API reports, in a file so a deploy or a
  # test step can update it (see the production-rollback guard). Starts at 40 —
  # the version rollback-target capture sees BEFORE the first production deploy.
  local active_state="$workspace/fake-active-version"
  printf '40\n' >"$active_state"

  printf '%s\n' "$path_dir" >>"${GITHUB_PATH:?GITHUB_PATH is required}"
  {
    printf 'FAKE_CALL_LOG=%s\n' "$log"
    printf 'FAKE_ACTIVE_VERSION_FILE=%s\n' "$active_state"
  } >>"${GITHUB_ENV:?GITHUB_ENV is required}"

  notice "fake fastly (v$pinned) packaged as a checksum-verified archive at $archive; fake curl on PATH"
}

main "$@"
