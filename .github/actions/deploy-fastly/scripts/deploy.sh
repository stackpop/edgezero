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
#   package-digest                        verified package SHA-256 reported by the CLI

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../../fastly-common/scripts/common.sh
source "$SCRIPT_DIR/../../fastly-common/scripts/common.sh"

PARSED_VALUE=""
parse_contract_value() {
  local key="$1" pattern="$2" all_lines malformed values distinct
  PARSED_VALUE=""
  all_lines=$(grep -E "^${key}=" "$LIFECYCLE_LOG" || true)
  [[ -n "$all_lines" ]] || return 1
  malformed=$(printf '%s\n' "$all_lines" | grep -vE "^${key}=${pattern}$" || true)
  [[ -z "$malformed" ]] || return 2
  values=$(printf '%s\n' "$all_lines" | sed "s/^${key}=//" | sort -u)
  distinct=$(printf '%s\n' "$values" | grep -c . || true)
  [[ "$distinct" == 1 ]] || return 3
  PARSED_VALUE="$values"
}

main() {
  local token="${EDGEZERO__FASTLY__API_TOKEN:-}"
  local service_id="${EDGEZERO__FASTLY__SERVICE_ID:-}"
  local expected_package_digest="${EDGEZERO__APP__RELEASE__PACKAGE_DIGEST:-}"

  require_input fastly-api-token "$token"
  require_fastly_service_id "$service_id"
  [[ "$expected_package_digest" =~ ^[0-9a-f]{64}$ ]] ||
    fail "the verified application release package digest is missing or invalid"
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

  local version="" version_status=0 package_digest="" package_status=0
  parse_contract_value version '[0-9]+' || version_status=$?
  [[ "$version_status" -ne 0 ]] || version="$PARSED_VALUE"
  parse_contract_value package-sha256 '[0-9a-f]{64}' || package_status=$?
  [[ "$package_status" -ne 0 ]] || package_digest="$PARSED_VALUE"

  if [[ "$package_status" -eq 0 && -n "$expected_package_digest" && "$package_digest" != "$expected_package_digest" ]]; then
    package_status=4
  fi
  if [[ "$rc" -ne 0 ]]; then
    # Recovery outputs are best-effort on an already-failed provider command.
    # A broken GitHub output channel must not replace the original provider status.
    set +e
    [[ "$version_status" -ne 0 ]] || append_output fastly-version "$version"
    [[ "$package_status" -ne 0 ]] || append_output package-digest "$package_digest"
    set -e
    fail_with "$rc" "deploy failed (CLI exit $rc or setup error before invocation)"
  fi

  # Publish every independently valid recovery value before checking the other
  # post-invocation contract. A package-output defect must not hide a version
  # that may already identify a mutated provider draft, and vice versa.
  [[ "$version_status" -ne 0 ]] || append_output fastly-version "$version"
  [[ "$package_status" -ne 0 ]] || append_output package-digest "$package_digest"
  [[ "$version_status" -eq 0 ]] || fail "deploy reported success without one unambiguous canonical 'version=<digits>' line"
  [[ "$package_status" -eq 0 ]] || fail "deploy reported success without the verified canonical package digest"
}

main "$@"
