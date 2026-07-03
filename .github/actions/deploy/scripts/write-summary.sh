#!/usr/bin/env bash
set -euo pipefail

SUMMARY=${GITHUB_STEP_SUMMARY:-}
[[ -n "$SUMMARY" ]] || exit 0
{
  echo "## EdgeZero deploy"
  echo
  echo "| Field | Value |"
  echo "| --- | --- |"
  echo "| Adapter | ${ADAPTER:-fastly} |"
  echo "| Application directory | ${APP_REL:-unknown} |"
  echo "| Application revision | ${SOURCE_REVISION:-unknown} |"
  echo "| Manifest | ${MANIFEST_SUMMARY:-EdgeZero default discovery} |"
  echo "| EdgeZero revision | ${EDGEZERO_REVISION:-unknown} |"
  echo "| Fastly CLI | ${PROVIDER_CLI_VERSION:-unknown} |"
  echo "| Build mode | ${REQUESTED_BUILD_MODE:-auto} → ${EFFECTIVE_BUILD_MODE:-unknown} |"
  echo "| Cache | ${CACHE_ENABLED:-false} |"
  echo "| Result | ${EDGEZERO_ACTION_RESULT:-unknown} |"
} >>"$SUMMARY"
