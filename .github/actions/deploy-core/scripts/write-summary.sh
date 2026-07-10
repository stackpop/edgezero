#!/usr/bin/env bash
set -euo pipefail

# Writes a non-sensitive GitHub step summary. Never emits credentials, full
# environments, or raw argument arrays.

SUMMARY=${GITHUB_STEP_SUMMARY:-}
[[ -n "$SUMMARY" ]] || exit 0
{
  echo "## EdgeZero deploy"
  echo
  echo "| Field | Value |"
  echo "| ----- | ----- |"
  echo "| Adapter | ${SUMMARY_ADAPTER:-unknown} |"
  echo "| Application directory | ${SUMMARY_WORKING_DIRECTORY:-unknown} |"
  echo "| Source revision | ${SUMMARY_SOURCE_REVISION:-unknown} |"
  echo "| Manifest | ${SUMMARY_MANIFEST:-EdgeZero default discovery} |"
  echo "| Rust toolchain | ${SUMMARY_RUST_TOOLCHAIN:-unknown} |"
  echo "| Target | ${SUMMARY_TARGET:-unknown} |"
  echo "| CLI version | ${SUMMARY_CLI_VERSION:-unknown} |"
  echo "| Effective build mode | ${SUMMARY_EFFECTIVE_BUILD_MODE:-unknown} |"
  echo "| Cache | ${SUMMARY_CACHE:-false} |"
  echo "| Result | ${SUMMARY_RESULT:-unknown} |"
} >>"$SUMMARY"
