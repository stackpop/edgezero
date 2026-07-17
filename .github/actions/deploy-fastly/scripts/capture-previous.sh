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
  # A first-ever deploy has no active version; that is not a failure, just an
  # empty rollback target. So a non-zero exit here is tolerated and yields "".
  "$cli_bin" active-version --adapter fastly --service-id "$service_id" \
    2>&1 | tee "$LIFECYCLE_LOG" || true

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
