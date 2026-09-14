#!/usr/bin/env bash
# spin_version_guard.sh — shared Spin CLI floor check.
#
# `spin-sdk` ~7 imports `wasi:http/types@0.3.0`, which Spin 4.0.x does
# not provide. The failure is late and opaque: the component builds,
# the wasip2 contract tests pass under wasmtime, and only `spin up`
# reports
#
#   Error: component imports instance `wasi:http/types@0.3.0`, but a
#   matching implementation was not found in the linker
#
# Every smoke test that runs `spin up` needs this check, so it lives
# here rather than being copied per script.
#
# Usage:
#   . "$(dirname "${BASH_SOURCE[0]}")/spin_version_guard.sh"
#   spin_meets_floor || exit 1        # hard requirement
#   spin_meets_floor || continue      # skip one row of a loop
#
# Prints the reason to stderr when the check fails. Returns 0 when the
# CLI is present and new enough, 1 otherwise.

# Minimum Spin CLI, as MAJOR*100 + MINOR so 4.0.9 (400) sorts below
# 4.1.0 (401) and 4.10 / 10.1 compare correctly.
SPIN_MIN_ENCODED=401
SPIN_MIN_DISPLAY="4.1"
SPIN_INSTALL_URL="https://spinframework.dev/install"

spin_meets_floor() {
  if ! command -v spin >/dev/null 2>&1; then
    echo "Spin CLI is required (>= ${SPIN_MIN_DISPLAY}). Install from ${SPIN_INSTALL_URL}" >&2
    return 1
  fi

  local version encoded
  version=$(spin --version 2>/dev/null | awk '{print $2}' | head -1)
  # MAJOR.MINOR only; a pre-release suffix on the patch is ignored.
  encoded=$(printf '%s' "$version" | awk -F'.' '{printf "%d%02d", $1, $2}')

  if [ -n "$encoded" ] && [ "$encoded" -lt "$SPIN_MIN_ENCODED" ]; then
    echo "Spin CLI ${version} is too old: spin-sdk 7 needs >= ${SPIN_MIN_DISPLAY} for wasi:http/types@0.3.0." >&2
    echo "Install a newer CLI from ${SPIN_INSTALL_URL} and re-run." >&2
    return 1
  fi
  return 0
}
