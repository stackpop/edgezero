#!/usr/bin/env bash
set -euo pipefail

export LC_ALL=C
export BASH_ENV=
export ENV=
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_NO_REPLACE_OBJECTS=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CEILING_DIRECTORIES
unset GIT_REPLACE_REF_BASE

readonly EXPECTED_REPOSITORY=stackpop/edgezero
readonly EXPECTED_WORKFLOW_REF=stackpop/edgezero/.github/workflows/build-container-ci.yml@refs/heads/main
readonly GATE_MANIFEST=.github/docker/build-app-cli/gate-paths.txt
readonly IMAGE_MANIFEST=.github/docker/build-app-cli/image-context-paths.txt
readonly CODEOWNERS=.github/CODEOWNERS
readonly GATE_TEAM=@stackpop/edgezero-build-container-gate-reviewers
readonly MAX_MANIFEST_BYTES=65536

usage() {
  printf 'usage: assert-build-container-dispatch-context.sh --gate-root <absolute-path>\n' >&2
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != 0000000000000000000000000000000000000000 ]]
}

gate_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$GATE_ROOT" "$@"
}

valid_path() {
  local path=$1
  [[ -n "$path" && "$path" =~ ^[A-Za-z0-9._/+-]+$ && "$path" != /* &&
    "$path" != -* && "$path" != */ && "$path" != . && "$path" != .. &&
    "$path" != ../* && "$path" != */../* && "$path" != */.. &&
    "$path" != */./* && "$path" != */. && "$path" != *//* && "$path" != *\\* ]]
}

tree_entry() {
  local revision=$1 path=$2 entry recorded metadata mode type object
  entry=$(gate_git ls-tree "$revision" -- "$path") || die "cannot inspect manifested path: $path"
  [[ -n "$entry" && "$entry" != *$'\n'* && "$entry" == *$'\t'* ]] ||
    die "manifested path is missing or ambiguous: $path"
  recorded=${entry#*$'\t'}
  metadata=${entry%%$'\t'*}
  read -r mode type object <<<"$metadata"
  [[ "$recorded" == "$path" && "$type" == blob &&
    ("$mode" == 100644 || "$mode" == 100755) && "$object" =~ ^[0-9a-f]{40,64}$ ]] ||
    die "manifested path is not a regular Git blob: $path"
  printf '%s' "$entry"
}

extract_manifest() {
  local path=$1 output=$2 label=$3 size last previous current
  previous=
  tree_entry "$EDGEZERO_GATE_SHA" "$path" >/dev/null
  gate_git show "$EDGEZERO_GATE_SHA:$path" >"$output" || die "cannot read $label"
  size=$(wc -c <"$output" | tr -d '[:space:]') || die "cannot size $label"
  [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le "$MAX_MANIFEST_BYTES" ]] ||
    die "$label is empty or oversized"
  last=$(tail -c 1 "$output" | od -An -tx1 | tr -d '[:space:]') ||
    die "cannot inspect $label terminator"
  [[ "$last" == 0a ]] || die "$label must be LF-terminated"
  while IFS= read -r current; do
    valid_path "$current" || die "$label contains an invalid path"
    [[ -z "$previous" || "$previous" < "$current" ]] ||
      die "$label must be byte-sorted and unique"
    previous=$current
  done <"$output"
}

contains_path() {
  grep -Fqx -e "$1" "$2"
}

compare_manifested_tree() {
  local manifest=$1 path gate_entry snapshot_entry
  while IFS= read -r path; do
    gate_entry=$(tree_entry "$EDGEZERO_GATE_SHA" "$path")
    snapshot_entry=$(tree_entry "$EDGEZERO_SHA" "$path")
    [[ "$snapshot_entry" == "$gate_entry" ]] ||
      die "protected snapshot differs from active gate at $path"
  done <"$manifest"
}

[[ "$#" -eq 2 && "$1" == --gate-root && -n "$2" ]] || usage
GATE_ROOT=$2
[[ "$GATE_ROOT" == /* && -d "$GATE_ROOT" && ! -L "$GATE_ROOT" ]] ||
  die "gate root must be an absolute, non-symlink directory"
CANONICAL_ROOT=$(cd -- "$GATE_ROOT" && pwd -P) || die "cannot resolve gate root"
[[ "$CANONICAL_ROOT" == "$GATE_ROOT" ]] || die "gate root must already be canonical"

for name in GITHUB_TOKEN EDGEZERO_EVENT_NAME EDGEZERO_REPOSITORY EDGEZERO_REF \
  EDGEZERO_REF_PROTECTED EDGEZERO_SHA EDGEZERO_WORKFLOW_SHA EDGEZERO_WORKFLOW_REF \
  EDGEZERO_GATE_SHA EDGEZERO_RELEASE_STATE EDGEZERO_CANDIDATE_PR_NUMBER \
  EDGEZERO_CANDIDATE_HEAD_REPOSITORY \
  EDGEZERO_CANDIDATE_HEAD_SHA; do
  [[ -n "${!name:-}" ]] || die "required context is absent: $name"
done
[[ "$GITHUB_TOKEN" != *$'\n'* && "$GITHUB_TOKEN" != *$'\r'* &&
  "$GITHUB_TOKEN" != *'"'* && "$GITHUB_TOKEN" != *\\* ]] ||
  die "GitHub token cannot be encoded safely"
[[ "$EDGEZERO_EVENT_NAME" == workflow_dispatch ]] || die "event must be workflow_dispatch"
[[ "$EDGEZERO_REPOSITORY" == "$EXPECTED_REPOSITORY" ]] || die "repository identity differs"
[[ "$EDGEZERO_REF" == refs/heads/main && "$EDGEZERO_REF_PROTECTED" == true ]] ||
  die "dispatch must use protected main"
[[ "$EDGEZERO_WORKFLOW_REF" == "$EXPECTED_WORKFLOW_REF" ]] || die "workflow ref differs"
[[ "$EDGEZERO_RELEASE_STATE" == enabled ]] || die "release state must be enabled"
if ! is_sha "$EDGEZERO_SHA" || ! is_sha "$EDGEZERO_WORKFLOW_SHA" ||
  ! is_sha "$EDGEZERO_GATE_SHA"; then
  die "workflow and gate identities must be full nonzero SHAs"
fi
[[ "$EDGEZERO_SHA" == "$EDGEZERO_WORKFLOW_SHA" ]] || die "dispatch snapshot identities differ"
[[ "$EDGEZERO_CANDIDATE_PR_NUMBER" =~ ^[1-9][0-9]*$ ]] ||
  die "candidate pull request number is not canonical"
[[ "$EDGEZERO_CANDIDATE_HEAD_REPOSITORY" == "$EXPECTED_REPOSITORY" ]] ||
  die "candidate head repository must be exact"
is_sha "$EDGEZERO_CANDIDATE_HEAD_SHA" || die "candidate head SHA is invalid"

[[ "$(gate_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
  die "gate root is not a Git worktree"
[[ "$(gate_git rev-parse --show-toplevel 2>/dev/null)" == "$GATE_ROOT" ]] ||
  die "gate root must be the exact repository top level"
GIT_DIRECTORY=$(gate_git rev-parse --absolute-git-dir 2>/dev/null) ||
  die "cannot resolve gate Git directory"
COMMON_GIT_DIRECTORY=$(gate_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
  die "cannot resolve gate common Git directory"
[[ ! -e "$GIT_DIRECTORY/info/grafts" && ! -L "$GIT_DIRECTORY/info/grafts" &&
  ! -e "$COMMON_GIT_DIRECTORY/info/grafts" && ! -L "$COMMON_GIT_DIRECTORY/info/grafts" ]] ||
  die "gate checkout cannot contain legacy grafts"
[[ -z "$(gate_git for-each-ref --format='%(refname)' refs/replace/)" ]] ||
  die "gate checkout cannot contain replacement refs"
[[ "$(gate_git rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
  die "gate checkout must contain full history"
[[ "$(gate_git config --bool core.sparseCheckout 2>/dev/null || true)" != true ]] ||
  die "gate checkout cannot be sparse"
[[ -z "$(gate_git status --porcelain=v1 --untracked-files=all)" ]] ||
  die "gate checkout must be clean"
[[ "$(gate_git cat-file -t "$EDGEZERO_GATE_SHA" 2>/dev/null)" == commit ]] ||
  die "active gate commit is not a commit"
[[ "$(gate_git cat-file -t "$EDGEZERO_SHA" 2>/dev/null)" == commit ]] ||
  die "dispatch snapshot is not a commit"
[[ "$(gate_git rev-parse --verify HEAD 2>/dev/null)" == "$EDGEZERO_GATE_SHA" ]] ||
  die "gate checkout HEAD differs from active gate"
gate_git merge-base --is-ancestor "$EDGEZERO_GATE_SHA" "$EDGEZERO_SHA" ||
  die "active gate is not an ancestor of dispatch snapshot"
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ "$SCRIPT_DIR/assert-build-container-dispatch-context.sh" == "$GATE_ROOT/.github/docker/build-app-cli/assert-build-container-dispatch-context.sh" ]] ||
  die "dispatch helper must execute from the active gate checkout"

WORK_DIR=$(mktemp -d /tmp/edgezero-dispatch-context.XXXXXX)
trap 'rm -rf -- "$WORK_DIR"' EXIT HUP INT TERM
GATE_PATHS="$WORK_DIR/gate-paths.txt"
IMAGE_PATHS="$WORK_DIR/image-context-paths.txt"
extract_manifest "$GATE_MANIFEST" "$GATE_PATHS" "gate manifest"
extract_manifest "$IMAGE_MANIFEST" "$IMAGE_PATHS" "image-context manifest"
contains_path "$GATE_MANIFEST" "$GATE_PATHS" || die "gate manifest must contain itself"
contains_path "$IMAGE_MANIFEST" "$GATE_PATHS" || die "gate manifest omits image manifest"
contains_path "$CODEOWNERS" "$GATE_PATHS" || die "gate manifest omits CODEOWNERS"
contains_path "$IMAGE_MANIFEST" "$IMAGE_PATHS" || die "image manifest must contain itself"
while IFS= read -r path; do
  contains_path "$path" "$GATE_PATHS" || die "image path is not gate-owned: $path"
done <"$IMAGE_PATHS"
IMAGE_COUNT=$(wc -l <"$IMAGE_PATHS" | tr -d '[:space:]')
GATE_COUNT=$(wc -l <"$GATE_PATHS" | tr -d '[:space:]')
[[ "$IMAGE_COUNT" -lt "$GATE_COUNT" ]] ||
  die "image manifest must be a strict gate-manifest subset"

EXPECTED_CODEOWNERS="$WORK_DIR/CODEOWNERS"
: >"$EXPECTED_CODEOWNERS"
while IFS= read -r path; do
  printf '/%s %s\n' "$path" "$GATE_TEAM" >>"$EXPECTED_CODEOWNERS"
done <"$GATE_PATHS"
gate_git show "$EDGEZERO_GATE_SHA:$CODEOWNERS" >"$WORK_DIR/actual-CODEOWNERS" ||
  die "cannot read CODEOWNERS"
cmp -s "$EXPECTED_CODEOWNERS" "$WORK_DIR/actual-CODEOWNERS" ||
  die "CODEOWNERS does not exactly protect every gate path"
compare_manifested_tree "$GATE_PATHS"

CONFIG="$WORK_DIR/curl.config"
printf '%s\n' \
  'header = "Accept: application/vnd.github+json"' \
  'header = "X-GitHub-Api-Version: 2026-03-10"' \
  'header = "User-Agent: edgezero-build-container-gate/1"' \
  "header = \"Authorization: Bearer $GITHUB_TOKEN\"" \
  >"$CONFIG"
REPLY="$WORK_DIR/reply"
env -i PATH="$PATH" LC_ALL=C curl \
  --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
  --request GET --config - \
  --write-out $'\n%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' \
  "https://api.github.com/repos/stackpop/edgezero/pulls/$EDGEZERO_CANDIDATE_PR_NUMBER" \
  <"$CONFIG" >"$REPLY" || die "candidate pull request lookup failed"

STATUS=$(tail -n 3 "$REPLY" | sed -n '1p')
SELECTED_VERSION=$(tail -n 3 "$REPLY" | sed -n '2p')
CONTENT_TYPE=$(tail -n 3 "$REPLY" | sed -n '3p')
[[ "$STATUS" == 200 ]] || die "candidate pull request lookup returned HTTP $STATUS"
[[ "$SELECTED_VERSION" == 2026-03-10 ]] || die "GitHub selected an unexpected API version"
[[ "$CONTENT_TYPE" =~ ^[Aa][Pp][Pp][Ll][Ii][Cc][Aa][Tt][Ii][Oo][Nn]/[Jj][Ss][Oo][Nn]([[:space:]]*\;[[:space:]]*[Cc][Hh][Aa][Rr][Ss][Ee][Tt][[:space:]]*=[[:space:]]*[Uu][Tt][Ff]-8)?$ ]] ||
  die "GitHub returned an unsupported media type"
sed '$d' "$REPLY" | sed '$d' | sed '$d' >"$WORK_DIR/body"
jq -e \
  --argjson number "$EDGEZERO_CANDIDATE_PR_NUMBER" \
  --arg repository "$EDGEZERO_CANDIDATE_HEAD_REPOSITORY" \
  --arg head "$EDGEZERO_CANDIDATE_HEAD_SHA" '
    type == "object" and
    .number == $number and .state == "open" and .merged == false and
    .base.ref == "main" and .base.repo.full_name == "stackpop/edgezero" and
    .head.repo.full_name == $repository and .head.sha == $head
  ' "$WORK_DIR/body" >/dev/null 2>&1 || die "candidate pull request identity differs from inputs"
