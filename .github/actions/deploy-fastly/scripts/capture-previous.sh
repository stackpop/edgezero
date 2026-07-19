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
#   FASTLY_API_TOKEN                       required  provider token (Fastly's own convention)
# Writes (outputs):
#   previous-version                       the active version before this deploy (may be empty)

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  local cli_bin service_id
  cli_bin=$(resolve_app_cli)
  service_id="${EDGEZERO__FASTLY__SERVICE_ID:?EDGEZERO__FASTLY__SERVICE_ID is required}"
  require_input fastly-api-token "${FASTLY_API_TOKEN:-}"
  require_cmd "$cli_bin"

  new_private_log
  # Fail CLOSED on an operational failure: the CLI exits 0 for "no active version"
  # (first deploy) and non-zero only for a real failure. Capture the CLI's exit
  # (pipefail makes it the pipeline status; tee exits 0).
  local rc=0
  "$cli_bin" active-version --adapter fastly --service-id "$service_id" \
    2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "could not determine the active version (CLI exit $rc); refusing to deploy without a captured rollback target. A first-ever deploy (no active version) exits 0 with an empty target — a non-zero exit means an API/auth/parse failure."
  fi

  local previous
  previous=$(read_numeric_line version "$LIFECYCLE_LOG")
  append_output previous-version "$previous"
  if [[ -n "$previous" ]]; then
    notice "captured production rollback target: previous-version=$previous"
  else
    notice "no active version yet (first deploy?); previous-version is empty"
  fi
}

main "$@"
