#!/usr/bin/env bash
set -euo pipefail

# Verifies every third-party `uses:` reference across the deploy actions and the
# deploy-action workflow is pinned to a concrete ref — a released VERSION TAG
# (e.g. `@v4.3.0`) or a full commit SHA — rather than a mutable branch/floating
# ref like `@main`, `@develop`, or `@latest`, or an unpinned reference. A version
# tag is the repo's chosen policy (see .github/zizmor.yml); a bare branch name is
# rejected because it moves. Local (`./...`) and docker refs are exempt.
#
# The `uses` key is matched in every YAML spelling GitHub accepts, not just the
# canonical block form: a space before the colon (`uses :`), a quoted key
# (`"uses":`), and flow mappings (`{ uses: … }`, `, uses: …`). A grep that only
# anchored `uses:` at line start would let those forms smuggle an unpinned ref past
# the gate.
#
# Usage: check-action-pins.sh [FILE...]   (defaults to the deploy action surface)

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
# Allow a caller (the contract tests) to check specific files instead.
if [[ "$#" -gt 0 ]]; then
  files=("$@")
fi

# Match a `uses` KEY in block, quoted, and flow-mapping forms. The key is either at
# the start of a (possibly `- `-prefixed) line, or preceded by a `{`/`,` flow
# indicator; it may be quoted; and a space may sit before the colon. A `#`-comment
# line matches none of these, so commented-out `uses` are ignored.
uses_re='(^[[:space:]]*(-[[:space:]]+)?|[{,][[:space:]]*)["'\'']?uses["'\'']?[[:space:]]*:'

status=0
while IFS= read -r line; do
  # line format: <path>:<lineno>:<content>. Strip everything up to and including the
  # `uses:` key (any spelling), then the quotes and any trailing flow punctuation.
  ref=$(printf '%s' "$line" | sed -nE 's/.*["'\'']?uses["'\'']?[[:space:]]*:[[:space:]]*//p' | tr -d '"'"'"'')
  ref="${ref%%[[:space:]]*}" # first whitespace-delimited token (drops ` }` etc.)
  ref="${ref%%,*}"           # drop a trailing flow comma
  ref="${ref%\}}"            # drop a trailing flow brace
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
# Restrict to YAML: `uses` keys only live in workflow/action files, and this also
# keeps the scan from matching prose in the deploy-core scripts (including this
# file's own examples). `--include` applies to explicit file arguments too, so the
# contract tests name their fixtures `*.yml`.
done < <(grep -rEn --include='*.yml' --include='*.yaml' "$uses_re" "${files[@]}" 2>/dev/null || true)

if [[ "$status" -eq 0 ]]; then
  echo "all third-party action references are pinned to a concrete ref"
fi
exit "$status"
