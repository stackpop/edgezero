#!/usr/bin/env bash
set -euo pipefail

# Writes a non-sensitive GitHub step summary. Never emits credentials, full
# environments, or raw argument arrays. All values arrive via SUMMARY_* env.

main() {
  local summary_file="${GITHUB_STEP_SUMMARY:-}"
  [[ -n "$summary_file" ]] || return 0
  {
    echo "## EdgeZero deploy"
    echo
    echo "| Field | Value |"
    echo "| ----- | ----- |"
    echo "| Adapter | ${EDGEZERO__SUMMARY__ADAPTER:-unknown} |"
    echo "| Application directory | ${EDGEZERO__SUMMARY__WORKING_DIRECTORY:-unknown} |"
    echo "| Source revision | ${EDGEZERO__SUMMARY__SOURCE_REVISION:-unknown} |"
    echo "| Manifest | ${EDGEZERO__SUMMARY__MANIFEST:-EdgeZero default discovery} |"
    echo "| Rust toolchain | ${EDGEZERO__SUMMARY__RUST_TOOLCHAIN:-unknown} |"
    echo "| Target | ${EDGEZERO__SUMMARY__TARGET:-unknown} |"
    echo "| CLI version | ${EDGEZERO__SUMMARY__APP_CLI_VERSION:-unknown} |"
    echo "| Effective build mode | ${EDGEZERO__SUMMARY__EFFECTIVE_BUILD_MODE:-unknown} |"
    echo "| Cache | ${EDGEZERO__SUMMARY__CACHE:-false} |"
    echo "| Result | ${EDGEZERO__SUMMARY__RESULT:-unknown} |"
  } >>"$summary_file"
}

main "$@"
