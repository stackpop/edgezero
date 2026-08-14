#!/usr/bin/env bash
set -euo pipefail

# Runs the application CLI's deploy through the provider-env credential boundary
# and emits the resulting Fastly version.
#
# Credentials are handed to run-app-cli.sh as DATA (a JSON object), not as FASTLY_*
# aliases on this step. run-app-cli.sh clears every declared alias — including any
# inherited FASTLY_ENDPOINT / FASTLY_TOKEN — exports only these typed values, and
# then scrubs its own private variables (including this JSON) before exec'ing the
# CLI. Building the JSON here, from step `env:`, is also what keeps the secret out
# of an interpolated `run:` block.
#
# Reads (env):
#   EDGEZERO__FASTLY__API_TOKEN           required  typed Fastly API token
#   EDGEZERO__FASTLY__SERVICE_ID          required  typed Fastly service id
#   (plus the run-app-cli.sh Reads contract, which this delegates to)
# Writes (outputs):
#   mutation-attempted                    true, emitted before the CLI runs (reconcile signal)
#   fastly-version                        the deployed/staged Fastly version

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../deploy-core/scripts/common.sh
source "$SCRIPT_DIR/../../deploy-core/scripts/common.sh"

main() {
  local token="${EDGEZERO__FASTLY__API_TOKEN:-}"
  local service_id="${EDGEZERO__FASTLY__SERVICE_ID:-}"

  require_input fastly-api-token "$token"
  require_input_matching fastly-service-id "$service_id" '^[A-Za-z0-9_-]+$'
  require_cmd jq

  EDGEZERO__PROVIDER__ENV=$(jq -n --arg t "$token" --arg s "$service_id" \
    '{FASTLY_API_TOKEN: $t, FASTLY_SERVICE_ID: $s}')
  export EDGEZERO__PROVIDER__ENV

  new_private_log
  # run-app-cli.sh publishes `mutation-attempted=true` itself, immediately before
  # it invokes the CLI — so a setup failure never falsely signals, and the signal
  # lands in GITHUB_OUTPUT before the mutation starts (best-effort durable across a
  # cancel/timeout; a hard runner loss can still drop it). This wrapper only threads
  # the resulting version out.
  local rc=0
  "$SCRIPT_DIR/../../deploy-core/scripts/run-app-cli.sh" deploy 2>&1 | tee "$LIFECYCLE_LOG" || rc=$?
  if [[ "$rc" -ne 0 ]]; then
    fail_with "$rc" "deploy failed (CLI exit $rc or setup error before invocation)"
  fi

  # The deploy must emit EXACTLY ONE canonical `version=<digits>` line. Mirror the
  # rollback-target capture (capture-previous.sh) rather than taking the LAST line:
  # `fastly-version` is threaded straight into healthcheck (`--version`) and rollback
  # (`--version`), so a non-conforming app CLI that printed a spurious `version=`
  # line before the authoritative one could otherwise thread a version that was never
  # deployed. A missing, duplicated, or malformed line fails CLOSED.
  local version_lines version_count version
  version_lines=$(grep -E '^version=' "$LIFECYCLE_LOG" || true)
  version_count=$(printf '%s' "$version_lines" | grep -c . || true)
  if [[ "$version_count" -eq 0 ]]; then
    fail "deploy reported success but emitted no canonical 'version=<digits>' line, so there is no version to thread into healthcheck or rollback"
  fi
  if [[ "$version_count" -gt 1 ]]; then
    fail "deploy emitted $version_count 'version=' lines; exactly one is required. Refusing to guess which version was deployed. Lines: $(printf '%s' "$version_lines" | tr '\n' ' ')"
  fi
  if [[ ! "$version_lines" =~ ^version=([0-9]+)$ ]]; then
    fail "deploy emitted a malformed version line '$version_lines'; expected 'version=<N>'. Refusing to thread an unparseable version into healthcheck or rollback"
  fi
  version="${BASH_REMATCH[1]}"

  append_output fastly-version "$version"
}

main "$@"
