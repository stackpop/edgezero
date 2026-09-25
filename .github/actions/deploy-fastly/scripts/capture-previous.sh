#!/usr/bin/env bash
set -euo pipefail

# Captures the version that is active BEFORE this deploy — the production rollback
# target — and emits it as `previous-version`.
#
# Fastly's version list exposes no field to tell a previously-live version from a
# staged one (`staging`/`deployed` are documented "Unused"; `locked` only means
# "not editable"), so the rollback target CANNOT be inferred after a deploy
# supersedes it. It has to be read here, first.
#
# Runs only for a production deploy: a staged deploy never activates, so there is
# nothing to roll back TO (staging rollback deactivates the staged version). When
# the service has no active version yet (a first-ever deploy), there is no
# previous version and the output is left empty — the caller simply has no
# production rollback target, which is correct.
#
# The CLI distinguishes "confirmed no active version" (exit 0, empty version)
# from an operational failure (non-zero exit: API/auth error, unparseable list).
# A non-zero exit is NOT tolerated: proceeding to deploy without knowing the
# rollback target would leave production with no way back. Only the genuine
# first-deploy case yields an empty target.
#
# `active-version` is manifest-independent (a pure Fastly-API call keyed on the
# service id), so this runs the CLI from wherever the step is — no app-directory
# resolution needed.
#
# Reads (env):
#   EDGEZERO__APP__CLI__PATH / _BIN        required  the app CLI (via resolve_app_cli)
#   EDGEZERO__FASTLY__SERVICE_ID           required  the Fastly service id
#   EDGEZERO__FASTLY__API_TOKEN           required  action-private Fastly API token
# Writes (outputs):
#   previous-version                       the active version before this deploy (may be empty)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

main() {
  local service_id cli_bin
  cli_bin=$(resolve_app_cli)
  cli_bin=$(command -v "$cli_bin") || fail "application CLI is unavailable"
  export EDGEZERO__APP__CLI__PATH="$cli_bin"
  service_id="${EDGEZERO__FASTLY__SERVICE_ID:?EDGEZERO__FASTLY__SERVICE_ID is required}"
  require_fastly_service_id "$service_id"
  local token="${EDGEZERO__FASTLY__API_TOKEN:-}"
  require_input fastly-api-token "$token"
  require_cmd jq

  local workspace="${EDGEZERO__ACTION__WORKSPACE:-$(dirname -- "$cli_bin")}"
  mkdir -p "$workspace"
  export EDGEZERO__ACTION__WORKSPACE="$workspace"
  local args_file="$workspace/active-version-argv.nul"
  local clear_file="$workspace/fastly-provider-clear.nul"
  printf '%s\0' active-version --adapter fastly --service-id "$service_id" >"$args_file"
  write_fastly_provider_clear_file "$clear_file"
  EDGEZERO__PROVIDER__ENV=$(jq -n --arg token "$token" '{FASTLY_API_TOKEN:$token}')
  export EDGEZERO__PROVIDER__ENV
  export EDGEZERO__PROVIDER__ENV_CLEAR_FILE="$clear_file"
  export EDGEZERO__APP__CLI__ARGS_FILE="$args_file"
  export EDGEZERO__APP__CLI__MUTATES=false

  new_private_log
  # Fail CLOSED on an operational failure: the CLI exits 0 for "no active version"
  # (first deploy) and non-zero only for a real failure. Capture the CLI's exit
  # (pipefail makes it the pipeline status; tee exits 0).
  local rc=0
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" \
    2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "could not determine the active version (CLI exit $rc); refusing to deploy without a captured rollback target. A first-ever deploy (no active version) exits 0 with an empty target — a non-zero exit means an API/auth/parse failure."
  fi

  # A successful exit is NOT enough: the CLI must emit EXACTLY one of the
  # `version=` contract shapes — `version=<digits>` for an active version, or an
  # empty `version=` for a confirmed first deploy. Anything else fails CLOSED:
  #   * NO `version=` line at all (a CLI that exited 0 but printed nothing
  #     parseable — e.g. one that never called `init_cli_logger`); or
  #   * a MALFORMED value like `version=12abc`, which the anchored numeric parse
  #     would otherwise silently drop to empty and mistake for a first deploy.
  # EXACTLY ONE `version=` line must be present. Taking only the last would let a
  # malformed line hide behind a later well-formed one (e.g. `version=12abc`
  # followed by `version=`, which would read as a first deploy), so a conflicting
  # or duplicated contract line is refused outright.
  local version_lines version_count version_line
  version_lines=$(grep -E '^version=' "$LIFECYCLE_LOG" || true)
  version_count=$(printf '%s' "$version_lines" | grep -c . || true)
  if [[ "$version_count" -eq 0 ]]; then
    fail_with 1 "active-version exited 0 but emitted no \`version=\` line; refusing to deploy without a confirmed rollback target. Ensure the app CLI dispatches \`active-version\` to \`edgezero_cli::run_active_version\` and initialises its logger (\`edgezero_cli::init_cli_logger()\`) so machine-readable lines are printed unprefixed."
  fi
  if [[ "$version_count" -gt 1 ]]; then
    fail_with 1 "active-version emitted $version_count \`version=\` lines; exactly one is required. Refusing to guess which is the rollback target. Lines: $(printf '%s' "$version_lines" | tr '\n' ' ')"
  fi
  version_line="$version_lines"
  if [[ ! "$version_line" =~ ^version=([0-9]+)?$ ]]; then
    fail_with 1 "active-version emitted a malformed rollback target '$version_line'; expected \`version=<N>\` or an empty \`version=\`. Refusing to deploy with an unparseable version."
  fi

  local previous="${BASH_REMATCH[1]:-}"
  append_output previous-version "$previous"
  if [[ -n "$previous" ]]; then
    notice "captured production rollback target: previous-version=$previous"
  else
    notice "no active version yet (first deploy?); previous-version is empty"
  fi
}

main "$@"
