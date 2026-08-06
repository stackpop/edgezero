#!/usr/bin/env bash
set -euo pipefail

# Verifies every third-party `uses:` reference across the deploy actions and the
# deploy-action workflow is pinned to a concrete ref — a released VERSION TAG
# (e.g. `@v4.3.0`) or a full commit SHA — rather than a mutable branch/floating
# ref like `@main`, `@develop`, or `@latest`, or an unpinned reference. A version
# tag is the repo's chosen policy (see .github/zizmor.yml); a bare branch name is
# rejected because it moves. Local (`./...`) and docker refs are exempt.

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)

files=(
  "$REPO_ROOT/.github/workflows/deploy-action.yml"
  "$REPO_ROOT/.github/workflows/fastly-installer-check.yml"
  "$REPO_ROOT/.github/actions/build-app-cli/action.yml"
  "$REPO_ROOT/.github/actions/deploy-core"
  "$REPO_ROOT/.github/actions/deploy-fastly/action.yml"
  "$REPO_ROOT/.github/actions/healthcheck-fastly/action.yml"
  "$REPO_ROOT/.github/actions/rollback-fastly/action.yml"
  "$REPO_ROOT/.github/actions/config-push-fastly/action.yml"
)

status=0
while IFS= read -r line; do
  # line format: <path>:<lineno>:<content>
  ref=$(printf '%s' "$line" | sed -nE 's/.*uses[[:space:]]*:[[:space:]]*//p' | tr -d '"'"'"'')
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
  # Accept ONLY a full commit SHA or a release version tag. Anything else — a
  # branch (`develop`), a moving alias (`latest`), or `main`/`master`/`HEAD` — is a
  # mutable ref and rejected. Regexes live in variables (the bash-3.2-safe idiom for
  # `=~`).
  sha_re='^[0-9a-fA-F]{40}$'
  tag_re='^v?[0-9]+(\.[0-9]+)*([-+][0-9A-Za-z.-]+)?$'
  if [[ "$suffix" =~ $sha_re || "$suffix" =~ $tag_re ]]; then
    :
  else
    echo "::error::action ref '@$suffix' is neither a full commit SHA nor a release version tag (mutable branch/floating refs are not allowed): $line" >&2
    status=1
  fi
# `uses[[:space:]]*:` — YAML (and actionlint) accept `uses : x`, so a space before
# the colon must not slip an unpinned ref past this gate.
done < <(grep -rEn '^[[:space:]]*(-[[:space:]]+)?uses[[:space:]]*:' "${files[@]}" 2>/dev/null || true)

if [[ "$status" -eq 0 ]]; then
  echo "all third-party action references are pinned to a concrete ref"
fi
exit "$status"
