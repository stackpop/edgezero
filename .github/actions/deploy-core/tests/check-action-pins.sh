#!/usr/bin/env bash
set -euo pipefail

# Verifies every third-party `uses:` reference across the deploy actions and the
# deploy-action workflow is pinned to a concrete ref (a readable released tag or
# a full commit SHA) rather than a floating branch like @main or an unpinned
# reference. Local (`./...`) and docker refs are exempt.

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)

files=(
  "$REPO_ROOT/.github/workflows/deploy-action.yml"
  "$REPO_ROOT/.github/actions/build-cli/action.yml"
  "$REPO_ROOT/.github/actions/deploy-core"
  "$REPO_ROOT/.github/actions/deploy-fastly/action.yml"
  "$REPO_ROOT/.github/actions/healthcheck-fastly/action.yml"
  "$REPO_ROOT/.github/actions/rollback-fastly/action.yml"
)

status=0
while IFS= read -r line; do
  # line format: <path>:<lineno>:<content>
  ref=$(printf '%s' "$line" | sed -nE 's/.*uses:[[:space:]]*//p' | tr -d '"'"'"'')
  [[ -z "$ref" ]] && continue
  case "$ref" in
    ./* | docker://*) continue ;;
  esac
  if [[ ! "$ref" == *@* ]]; then
    echo "::error::unpinned action reference (no @ref): $line" >&2
    status=1
    continue
  fi
  suffix="${ref##*@}"
  case "$suffix" in
    main | master | HEAD)
      echo "::error::action pinned to a floating ref '@$suffix': $line" >&2
      status=1
      ;;
  esac
done < <(grep -rEn '^[[:space:]]*(-[[:space:]]+)?uses:' "${files[@]}" 2>/dev/null || true)

[[ "$status" -eq 0 ]] && echo "all third-party action references are pinned to a concrete ref"
exit "$status"
