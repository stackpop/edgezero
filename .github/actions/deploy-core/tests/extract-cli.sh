#!/usr/bin/env bash
set -euo pipefail

# Extracts the build-cli artifact tarball so the lifecycle test can drive the
# app CLI directly (the wrappers do this themselves via download-cli.sh).
#
# Inputs (environment): GITHUB_WORKSPACE, CLI_BIN.

main() {
  local workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
  local cli_bin="${CLI_BIN:?CLI_BIN is required}"
  local dest="$workspace/cli-bin"
  local tarball

  tarball=$(find "$workspace/cli-dl" -maxdepth 1 -name '*.tar' -print -quit)
  if [[ -z "$tarball" ]]; then
    echo "::error::no CLI tarball found under $workspace/cli-dl" >&2
    return 1
  fi

  mkdir -p "$dest"
  tar -xf "$tarball" -C "$dest"
  chmod +x "$dest/$cli_bin"
  echo "extracted $cli_bin from $tarball"
}

main "$@"
