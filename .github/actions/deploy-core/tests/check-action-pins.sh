#!/usr/bin/env bash
set -euo pipefail

# Verifies every `uses:` reference — across ALL repository workflows and composite
# action metadata — is pinned to a concrete ref: a released VERSION TAG (e.g.
# `@v4.3.0`) or a full commit SHA, never a mutable branch/floating ref like `@main`,
# `@develop`, or `@latest`, or an unpinned reference. A version tag is the repo's
# chosen policy (see .github/zizmor.yml); a bare branch name is rejected because it
# moves. Local (`./...`) and docker refs are exempt.
#
# The `uses` values are extracted STRUCTURALLY with yq, so no YAML spelling — a space
# before the colon, a quoted or unicode-escaped key, a `!!str`-tagged or multiline
# scalar value, a flow mapping — can smuggle an unpinned ref past the gate. A
# text-scanning regex could not see those forms.
#
# Usage: check-action-pins.sh [FILE...]   (defaults to the whole .github surface)

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../../.." && pwd)

# Require mikefarah yq v4: its expression syntax below is version-specific, and the
# alternative (kislyuk's python yq) is a different tool. Fail closed if it is absent
# so a gate that silently checked nothing can never pass.
if ! command -v yq >/dev/null 2>&1 || ! yq --version 2>&1 | grep -qiE 'mikefarah|version v?4\.'; then
  echo "::error::check-action-pins.sh requires mikefarah yq v4 for structural YAML parsing" >&2
  exit 2
fi

files=()
if [[ "$#" -gt 0 ]]; then
  files=("$@")
else
  while IFS= read -r found; do files+=("$found"); done < <(
    find "$REPO_ROOT/.github/workflows" -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) 2>/dev/null
    find "$REPO_ROOT/.github/actions" -type f \( -name 'action.yml' -o -name 'action.yaml' \) 2>/dev/null
  )
fi

# A full commit SHA, or a release version tag (v4.3.0, 1.2, v1.17.0-rc.1). Regexes
# live in variables (the bash-3.2-safe idiom for `=~`).
sha_re='^[0-9a-fA-F]{40}$'
tag_re='^v?[0-9]+(\.[0-9]+)*([-+][0-9A-Za-z.-]+)?$'

status=0
for file in "${files[@]}"; do
  [[ -f "$file" ]] || continue
  # yq errors on a non-YAML file; treat that as no refs (a real workflow parses).
  while IFS= read -r ref; do
    [[ -z "$ref" || "$ref" == "null" ]] && continue
    case "$ref" in
      ./* | docker://*) continue ;;
    esac
    if [[ "$ref" != *@* ]]; then
      echo "::error::unpinned action reference (no @ref): '$ref' in $file" >&2
      status=1
      continue
    fi
    suffix="${ref##*@}"
    if [[ "$suffix" =~ $sha_re || "$suffix" =~ $tag_re ]]; then
      :
    else
      echo "::error::action ref '@$suffix' is neither a full commit SHA nor a release version tag (mutable branch/floating refs are not allowed): '$ref' in $file" >&2
      status=1
    fi
  done < <(yq '.. | select(tag == "!!map" and has("uses")) | .uses' "$file" 2>/dev/null)
done

if [[ "$status" -eq 0 ]]; then
  echo "all action references (repository-wide) are pinned to a concrete ref"
fi
exit "$status"
