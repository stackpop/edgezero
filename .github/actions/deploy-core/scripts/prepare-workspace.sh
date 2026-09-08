#!/usr/bin/env bash
set -euo pipefail

# Creates a UNIQUE per-invocation workspace root under RUNNER_TEMP and publishes it
# as the `root` step output. Two concurrent invocations of an action in one job
# (e.g. `background: true`) must not share a fixed temp path, or one could
# overwrite the other's CLI, tools, or service flags and then run them with the
# wrong token — so every temp path hangs off this per-run root, and the action's
# cleanup step removes it.
#
# Reads (env):
#   RUNNER_TEMP                           optional  workspace root parent (default: /tmp)
#   GITHUB_OUTPUT                         required  step output file
# Writes (outputs):
#   root                                  the mktemp'd workspace directory

root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/edgezero.XXXXXX")
# Validate SEPARATELY, never as `echo "root=$(mktemp …)"`: bash reports echo's
# success even when the command substitution fails, which would publish an empty
# root — and every path below it (/state, /tools, /cli-download, …) would resolve
# at the filesystem root, escaping RUNNER_TEMP.
[[ -d "$root" ]] || {
  echo "::error::could not create the action workspace under RUNNER_TEMP" >&2
  exit 1
}
printf 'root=%s\n' "$root" >>"${GITHUB_OUTPUT:?GITHUB_OUTPUT is required}"
