#!/usr/bin/env bash
set -euo pipefail

# Verifies every `uses:` reference — across ALL repository workflows and composite
# action metadata — is pinned to a CONCRETE ref: a released VERSION TAG (a major tag
# `@v4`, or a fuller `@v4.3.0`) or a full commit SHA, never a mutable branch/floating
# ref like `@main`, `@develop`, or `@latest`, or an unpinned reference.
#
# This does NOT assert immutability. A version tag — a major tag such as `@v4`
# especially — is repointed by the action's publisher on every release, so it can
# move under you (the tj-actions/changed-files compromise is exactly this). The gate
# enforces the repo's version-tag policy (see .github/zizmor.yml) and a concrete,
# reviewable ref; it rejects refs that move on their own (branches) but not a
# publisher re-tag. Pin to a full commit SHA where cryptographic immutability
# matters. Local (`./...`) refs are exempt; a `docker://` ref must itself be pinned —
# by an `@sha256:` digest or a version tag, never a floating `:latest`/bare image.
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
if ! command -v yq >/dev/null 2>&1 || ! yq --version 2>&1 | grep -qE 'mikefarah/yq.*version v?4\.'; then
  echo "::error::check-action-pins.sh requires mikefarah yq v4 for structural YAML parsing" >&2
  exit 2
fi

# Extract ONLY genuine action references: workflow job-level `uses` (reusable
# workflow calls), workflow step `uses`, and composite-action `runs.steps[].uses`.
# A blanket "any map with a `uses` key" would also reject unrelated fields such as
# `jobs.<job>.env.uses`.
uses_query='[(.jobs[]? | .uses), (.jobs[]? | .steps[]? | .uses), (.runs.steps[]? | .uses)] | .[] | select(. != null)'

files=()
if [[ "$#" -gt 0 ]]; then
  files=("$@")
else
  while IFS= read -r found; do files+=("$found"); done < <(
    # GitHub only reads workflows from .github/workflows (no nesting), so maxdepth 1
    # is correct there. Composite/local actions, however, can live ANYWHERE in the
    # repo (e.g. tools/deploy/action.yml), so scan action.yml repo-wide — pruning
    # build/vendor/VCS trees — rather than only under .github/actions.
    find "$REPO_ROOT/.github/workflows" -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) 2>/dev/null
    find "$REPO_ROOT" \
      \( -path '*/.git' -o -name target -o -name node_modules \) -prune -o \
      -type f \( -name 'action.yml' -o -name 'action.yaml' \) -print 2>/dev/null
  )
fi

# A full commit SHA, or a release version tag. The tag allows an optional semver
# prerelease AND build-metadata suffix together (v1.2.3-rc.1+build.5), not just one.
# Regexes live in variables (the bash-3.2-safe idiom for `=~`).
sha_re='^[0-9a-fA-F]{40}$'
tag_re='^v?[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'

status=0
refs_seen=0
for file in "${files[@]}"; do
  [[ -f "$file" ]] || continue
  # FAIL CLOSED on a parse/tool failure: if yq cannot read the file, a `2>/dev/null`
  # process substitution would yield no refs and the gate would silently pass a file
  # it never checked. Capture the output and the exit status instead.
  if ! uses_list=$(yq "$uses_query" "$file" 2>/dev/null); then
    echo "::error::could not parse '$file' as YAML — refusing to pass a file the pin gate cannot read" >&2
    status=1
    continue
  fi
  while IFS= read -r ref; do
    [[ -z "$ref" || "$ref" == "null" ]] && continue
    refs_seen=$((refs_seen + 1))
    case "$ref" in
      ./*) continue ;;
      docker://*)
        # A docker ref is pinned by an `@<algo>:<digest>` (immutable) or a version
        # tag; a bare image or a floating `:latest` is rejected like a branch ref.
        docker_ref="${ref#docker://}"
        if [[ "$docker_ref" == *@*:* ]]; then continue; fi
        docker_tag="${docker_ref##*:}"
        if [[ "$docker_ref" == *:* && "$docker_tag" != *"/"* && "$docker_tag" =~ $tag_re ]]; then
          continue
        fi
        echo "::error::docker action ref must be pinned by an @<algo>:<digest> or a version tag (floating ':latest'/bare images are not allowed): '$ref' in $file" >&2
        status=1
        continue
        ;;
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
  done <<<"$uses_list"
done

# A default (whole-repo) scan that finds ZERO action references is not a pass: the
# repo's own workflows and composite actions always carry `uses:` refs, so an empty
# result means the parser saw nothing — a substituted/broken `yq` would make the gate
# green while checking nothing. Fail closed. (An explicit FILE... run may legitimately
# target a file with no refs, so only guard the default scan.)
if [[ "$#" -eq 0 && "$refs_seen" -eq 0 ]]; then
  echo "::error::pin gate parsed 0 action references across the repository — expected many; refusing to pass (is yq working?)" >&2
  status=1
fi

if [[ "$status" -eq 0 ]]; then
  echo "all action references (repository-wide) are pinned to a concrete ref"
fi
exit "$status"
