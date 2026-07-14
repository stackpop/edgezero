#!/usr/bin/env bash
set -euo pipefail

# Writes a non-sensitive GitHub step summary. Never emits credentials, full
# environments, or raw argument arrays.
#
# Reads (env):
#   EDGEZERO__SUMMARY__ADAPTER            optional  adapter name
#   EDGEZERO__SUMMARY__WORKING_DIRECTORY  optional  app directory (relative, for display)
#   EDGEZERO__SUMMARY__SOURCE_REVISION    optional  deployed Git revision
#   EDGEZERO__SUMMARY__MANIFEST           optional  manifest path or default-discovery note
#   EDGEZERO__SUMMARY__RUST_TOOLCHAIN     optional  resolved toolchain
#   EDGEZERO__SUMMARY__TARGET             optional  build target
#   EDGEZERO__SUMMARY__APP_CLI_VERSION    optional  app CLI version
#   EDGEZERO__SUMMARY__EFFECTIVE_BUILD_MODE optional  build mode after resolution
#   EDGEZERO__SUMMARY__CACHE              optional  whether caching was enabled
#   EDGEZERO__SUMMARY__RESULT             optional  final step result
#   GITHUB_STEP_SUMMARY                   optional  summary sink (no-op when unset)
# Writes (step summary):
#   a Markdown table of the non-sensitive facts above.

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
