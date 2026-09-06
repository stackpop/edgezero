#!/usr/bin/env bash
set -euo pipefail

# Public action/workflow refs use exact stable vMAJOR.MINOR.PATCH tags. Docker
# actions use lowercase sha256 digests. Tag movement is an accepted release risk.
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
if ! command -v jq >/dev/null 2>&1; then
  echo "::error::check-action-pins.sh requires jq" >&2
  exit 2
fi

# Extract ONLY genuine action references: workflow job-level `uses` (reusable
# workflow calls), workflow step `uses`, and composite-action `runs.steps[].uses`.
# A blanket "any map with a `uses` key" would also reject unrelated fields such as
# `jobs.<job>.env.uses`.
# has() preserves explicit nulls. JSON keeps multiline scalars in one record.
uses_query='[(.jobs[]? | select(has("uses")) | .uses), (.jobs[]? | .steps[]? | select(has("uses")) | .uses), (.runs.steps[]? | select(has("uses")) | .uses)]'

files=()
if [[ "$#" -gt 0 ]]; then
  files=("$@")
else
  inventory=$(mktemp)
  trap 'rm -f "$inventory"' EXIT
  # Workflows are direct children; local actions may live anywhere in the repo.
  find "$REPO_ROOT/.github/workflows" -maxdepth 1 -type f \( -name '*.yml' -o -name '*.yaml' \) -print0 >"$inventory"
  find "$REPO_ROOT" \
    \( -path '*/.git' -o -name target -o -name node_modules \) -prune -o \
    -type f \( -name 'action.yml' -o -name 'action.yaml' \) -print0 >>"$inventory"
  while IFS= read -r -d '' found; do files+=("$found"); done <"$inventory"
fi

policy='
  def valid:
    if type != "string" then false
    elif test("[\\s\\x00-\\x1f\\x7f]") then false
    elif startswith("./") then length > 2
    elif startswith("docker://") then
      test("^docker://[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$")
    else
      test("^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*@v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$")
    end;
  [.[] | select(valid | not)]'

status=0
refs_seen=0
# Bash 3.2 treats an empty array as unset under nounset.
for file in ${files[@]+"${files[@]}"}; do
  if [[ ! -f "$file" ]]; then
    echo "::error::missing action metadata or workflow: $file" >&2
    status=1
    continue
  fi
  # FAIL CLOSED on a parse/tool failure: if yq cannot read the file, a `2>/dev/null`
  # process substitution would yield no refs and the gate would silently pass a file
  # it never checked. Capture the output and the exit status instead.
  if ! uses_list=$(yq -o=json -I=0 "$uses_query" "$file" | jq -cse 'if length == 1 and (.[0] | type == "array") then .[0] else error("expected one YAML document") end'); then
    echo "::error::could not parse '$file' as YAML — refusing to pass a file the pin gate cannot read" >&2
    status=1
    continue
  fi
  invalid=$(jq -c "$policy" <<<"$uses_list")
  if [[ "$invalid" != '[]' ]]; then
    echo "::error::expected an exact stable version tag, local action, or Docker sha256 digest in $file: $invalid" >&2
    status=1
  fi
  count=$(jq '[.[] | select(type == "string") | select(startswith("./") | not)] | length' <<<"$uses_list")
  refs_seen=$((refs_seen + count))
done

# A default (whole-repo) scan that finds ZERO action references is not a pass: the
# repo's own workflows and composite actions always carry `uses:` refs, so an empty
# result means the parser saw nothing — a substituted/broken `yq` would make the gate
# green while checking nothing. Fail closed. (An explicit FILE... run may legitimately
# target a file with no refs, so only guard the default scan.)
if [[ "$#" -eq 0 && "$refs_seen" -eq 0 ]]; then
  echo "::error::pin gate parsed 0 external action references; refusing a vacuous repository scan" >&2
  status=1
fi

if [[ "$status" -eq 0 ]]; then
  echo "action reference policy passed ($refs_seen external references)"
fi
exit "$status"
