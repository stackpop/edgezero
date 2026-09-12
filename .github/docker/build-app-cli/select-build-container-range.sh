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
readonly MAIN_REF=refs/heads/main
readonly QUEUE_PREFIX=refs/heads/gh-readonly-queue/main/
readonly ZERO_SHA=0000000000000000000000000000000000000000
readonly MAX_EVENT_BYTES=1048576

usage() {
  cat >&2 <<'EOF'
usage: select-build-container-range.sh \
  --event-name <pull_request|merge_group|push> \
  --github-repository <owner/repository> \
  --github-sha <40-lowercase-hex> \
  --github-workflow-sha <40-lowercase-hex> \
  --github-ref <ref> --github-ref-protected <true|false> \
  --gate-sha <40-lowercase-hex> --gate-root <absolute-path> \
  --event-json <absolute-path> --subject-root <absolute-path>
EOF
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

require_value() {
  (($# >= 2)) || usage
  [[ -n "$2" ]] || usage
}

set_once() {
  local name=$1 current=$2 value=$3
  [[ -z "$current" ]] || usage
  printf -v "$name" '%s' "$value"
}

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != "$ZERO_SHA" ]]
}

canonical_directory() {
  local supplied=$1 label=$2 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] ||
    die "$label must be an absolute, non-symlink directory"
  canonical=$(cd -- "$supplied" && pwd -P) || die "cannot resolve $label"
  [[ "$canonical" == "$supplied" ]] || die "$label must already be canonical"
  printf '%s\n' "$canonical"
}

canonical_file() {
  local supplied=$1 label=$2 directory canonical size
  [[ "$supplied" == /* && -f "$supplied" && ! -L "$supplied" ]] ||
    die "$label must be an absolute, regular non-symlink file"
  directory=$(dirname -- "$supplied")
  canonical=$(cd -- "$directory" && pwd -P)/$(basename -- "$supplied") ||
    die "cannot resolve $label"
  [[ "$canonical" == "$supplied" ]] || die "$label must already be canonical"
  size=$(wc -c <"$supplied" | tr -d '[:space:]') || die "cannot size $label"
  [[ "$size" =~ ^[0-9]+$ && "$size" -gt 0 && "$size" -le "$MAX_EVENT_BYTES" ]] ||
    die "$label is empty or exceeds its size limit"
  printf '%s\n' "$canonical"
}

subject_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$SUBJECT_ROOT" "$@"
}

gate_git() {
  SUBJECT_ROOT="$GATE_ROOT" subject_git "$@"
}

require_commit() {
  local revision=$1 label=$2 type
  is_sha "$revision" || die "$label must be a nonzero full lowercase SHA"
  type=$(subject_git cat-file -t "$revision" 2>/dev/null) || die "$label commit is missing"
  [[ "$type" == commit ]] || die "$label does not name a commit object"
}

require_ancestor() {
  subject_git merge-base --is-ancestor "$1" "$2" ||
    die "$3 must be an ancestor of or equal to $4"
}

json_string() {
  local filter=$1 label=$2 value
  jq -e "$filter | type == \"string\" and (test(\"[\\u0000-\\u001f\\u007f]\") | not)" \
    "$EVENT_JSON" >/dev/null 2>&1 || die "$label is missing, malformed, or contains controls"
  value=$(jq -er "$filter" "$EVENT_JSON" 2>/dev/null) ||
    die "$label is missing or is not a string"
  printf '%s\n' "$value"
}

json_positive_integer() {
  local filter=$1 label=$2 value
  value=$(jq -er "$filter | select(type == \"number\" and . >= 1 and . == floor) | tostring" \
    "$EVENT_JSON" 2>/dev/null) || die "$label is not a positive integer"
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || die "$label is not canonical"
  printf '%s\n' "$value"
}

emit_range() {
  printf 'base=%s\nhead=%s\n' "$1" "$2"
}

EVENT_NAME=
GITHUB_REPOSITORY_VALUE=
GITHUB_SHA_VALUE=
GITHUB_WORKFLOW_SHA_VALUE=
GITHUB_REF_VALUE=
GITHUB_REF_PROTECTED_VALUE=
GATE_SHA=
GATE_ROOT=
EVENT_JSON=
SUBJECT_ROOT=

while (($#)); do
  case "$1" in
    --event-name)
      require_value "$@"
      set_once EVENT_NAME "$EVENT_NAME" "$2"
      shift 2
      ;;
    --github-repository)
      require_value "$@"
      set_once GITHUB_REPOSITORY_VALUE "$GITHUB_REPOSITORY_VALUE" "$2"
      shift 2
      ;;
    --github-sha)
      require_value "$@"
      set_once GITHUB_SHA_VALUE "$GITHUB_SHA_VALUE" "$2"
      shift 2
      ;;
    --github-workflow-sha)
      require_value "$@"
      set_once GITHUB_WORKFLOW_SHA_VALUE "$GITHUB_WORKFLOW_SHA_VALUE" "$2"
      shift 2
      ;;
    --github-ref)
      require_value "$@"
      set_once GITHUB_REF_VALUE "$GITHUB_REF_VALUE" "$2"
      shift 2
      ;;
    --github-ref-protected)
      require_value "$@"
      set_once GITHUB_REF_PROTECTED_VALUE "$GITHUB_REF_PROTECTED_VALUE" "$2"
      shift 2
      ;;
    --gate-sha)
      require_value "$@"
      set_once GATE_SHA "$GATE_SHA" "$2"
      shift 2
      ;;
    --gate-root)
      require_value "$@"
      set_once GATE_ROOT "$GATE_ROOT" "$2"
      shift 2
      ;;
    --event-json)
      require_value "$@"
      set_once EVENT_JSON "$EVENT_JSON" "$2"
      shift 2
      ;;
    --subject-root)
      require_value "$@"
      set_once SUBJECT_ROOT "$SUBJECT_ROOT" "$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "$EVENT_NAME" && -n "$GITHUB_REPOSITORY_VALUE" && -n "$GITHUB_SHA_VALUE" &&
  -n "$GITHUB_WORKFLOW_SHA_VALUE" && -n "$GITHUB_REF_VALUE" &&
  -n "$GITHUB_REF_PROTECTED_VALUE" && -n "$GATE_SHA" && -n "$EVENT_JSON" &&
  -n "$GATE_ROOT" && -n "$SUBJECT_ROOT" ]] || usage
[[ "$GITHUB_REPOSITORY_VALUE" == "$EXPECTED_REPOSITORY" ]] ||
  die "workflow repository must be $EXPECTED_REPOSITORY"
[[ "$GITHUB_REF_PROTECTED_VALUE" == true || "$GITHUB_REF_PROTECTED_VALUE" == false ]] ||
  die "github ref protection must be an exact boolean"
if ! is_sha "$GITHUB_SHA_VALUE" || ! is_sha "$GITHUB_WORKFLOW_SHA_VALUE" || ! is_sha "$GATE_SHA"; then
  usage
fi

SUBJECT_ROOT=$(canonical_directory "$SUBJECT_ROOT" "subject root")
GATE_ROOT=$(canonical_directory "$GATE_ROOT" "gate root")
[[ "$SUBJECT_ROOT" != "$GATE_ROOT" ]] || die "subject and gate roots must differ"
EVENT_JSON=$(canonical_file "$EVENT_JSON" "event JSON")
jq -e 'type == "object"' "$EVENT_JSON" >/dev/null 2>&1 || die "event JSON must be one object"

[[ "$(subject_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
  die "subject root is not a Git worktree"
[[ "$(subject_git rev-parse --show-toplevel 2>/dev/null)" == "$SUBJECT_ROOT" ]] ||
  die "subject root must be the exact repository top level"
[[ "$(subject_git rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
  die "subject checkout must contain full history"
[[ "$(subject_git config --bool core.sparseCheckout 2>/dev/null || true)" != true ]] ||
  die "subject checkout cannot be sparse"
[[ -z "$(subject_git status --porcelain=v1 --untracked-files=all)" ]] ||
  die "subject checkout must be clean"
[[ -z "$(subject_git for-each-ref --format='%(refname)' refs/replace/)" ]] ||
  die "subject checkout cannot contain replacement refs"
GIT_DIRECTORY=$(subject_git rev-parse --absolute-git-dir 2>/dev/null) ||
  die "cannot resolve subject Git directory"
SUBJECT_COMMON_DIRECTORY=$(subject_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
  die "cannot resolve subject common Git directory"
[[ ! -e "$GIT_DIRECTORY/info/grafts" && ! -L "$GIT_DIRECTORY/info/grafts" &&
  ! -e "$SUBJECT_COMMON_DIRECTORY/info/grafts" && ! -L "$SUBJECT_COMMON_DIRECTORY/info/grafts" ]] ||
  die "subject checkout cannot contain legacy grafts"

require_commit "$GITHUB_SHA_VALUE" "github SHA"
require_commit "$GATE_SHA" "active gate SHA"
[[ "$(subject_git rev-parse --verify HEAD 2>/dev/null)" == "$GITHUB_SHA_VALUE" ]] ||
  die "subject HEAD differs from github SHA"

[[ "$(gate_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
  die "gate root is not a Git worktree"
[[ "$(gate_git rev-parse --show-toplevel 2>/dev/null)" == "$GATE_ROOT" ]] ||
  die "gate root must be the exact repository top level"
[[ "$(gate_git rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
  die "gate checkout must contain full history"
[[ "$(gate_git config --bool core.sparseCheckout 2>/dev/null || true)" != true ]] ||
  die "gate checkout cannot be sparse"
[[ -z "$(gate_git status --porcelain=v1 --untracked-files=all)" ]] ||
  die "gate checkout must be clean"
[[ -z "$(gate_git for-each-ref --format='%(refname)' refs/replace/)" ]] ||
  die "gate checkout cannot contain replacement refs"
GATE_GIT_DIRECTORY=$(gate_git rev-parse --absolute-git-dir 2>/dev/null) ||
  die "cannot resolve gate Git directory"
GATE_COMMON_DIRECTORY=$(gate_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
  die "cannot resolve gate common Git directory"
[[ "$GATE_COMMON_DIRECTORY" != "$SUBJECT_COMMON_DIRECTORY" ]] ||
  die "gate and subject roots must use separate Git repositories"
[[ ! -e "$GATE_GIT_DIRECTORY/info/grafts" && ! -L "$GATE_GIT_DIRECTORY/info/grafts" &&
  ! -e "$GATE_COMMON_DIRECTORY/info/grafts" && ! -L "$GATE_COMMON_DIRECTORY/info/grafts" ]] ||
  die "gate checkout cannot contain legacy grafts"
[[ "$(gate_git cat-file -t "$GATE_SHA" 2>/dev/null)" == commit ]] ||
  die "active gate is not a commit in gate checkout"
[[ "$(gate_git rev-parse --verify HEAD 2>/dev/null)" == "$GATE_SHA" ]] ||
  die "gate checkout HEAD differs from active gate SHA"

case "$EVENT_NAME" in
  pull_request)
    [[ "$GITHUB_WORKFLOW_SHA_VALUE" == "$GATE_SHA" ]] ||
      die "required pull-request workflow must execute at active gate SHA"
    PR_NUMBER=$(json_positive_integer '.number' 'pull request number')
    BASE_REPOSITORY=$(json_string '.pull_request.base.repo.full_name' 'pull request base repository')
    BASE_REF=$(json_string '.pull_request.base.ref' 'pull request base ref')
    A=$(json_string '.pull_request.base.sha' 'pull request payload base SHA')
    J=$(json_string '.pull_request.head.sha' 'pull request payload head SHA')
    [[ "$BASE_REPOSITORY" == "$EXPECTED_REPOSITORY" ]] ||
      die "pull request base repository is not trusted"
    [[ "$BASE_REF" == main ]] || die "pull request base ref must be main"
    [[ "$GITHUB_REF_VALUE" == "refs/pull/$PR_NUMBER/merge" ]] ||
      die "pull request context ref does not match its number"
    require_commit "$A" "pull request payload base"
    require_commit "$J" "pull request payload head"
    read -r -a PARENTS <<<"$(subject_git rev-list --parents -n 1 "$GITHUB_SHA_VALUE")"
    [[ "${#PARENTS[@]}" -eq 3 && "${PARENTS[0]}" == "$GITHUB_SHA_VALUE" ]] ||
      die "pull request candidate must have exactly two ordered parents"
    F=${PARENTS[1]}
    require_commit "$F" "pull request synthetic first parent"
    [[ "${PARENTS[2]}" == "$J" ]] ||
      die "pull request synthetic second parent differs from payload head"
    require_ancestor "$A" "$F" "pull request payload base" "synthetic first parent"
    emit_range "$F" "$GITHUB_SHA_VALUE"
    ;;
  merge_group)
    [[ "$GITHUB_WORKFLOW_SHA_VALUE" == "$GATE_SHA" ]] ||
      die "required merge-group workflow must execute at active gate SHA"
    ACTION=$(json_string '.action' 'merge group action')
    BASE=$(json_string '.merge_group.base_sha' 'merge group base SHA')
    HEAD=$(json_string '.merge_group.head_sha' 'merge group head SHA')
    BASE_REF=$(json_string '.merge_group.base_ref' 'merge group base ref')
    HEAD_REF=$(json_string '.merge_group.head_ref' 'merge group head ref')
    [[ "$ACTION" == checks_requested ]] || die "merge group action must be checks_requested"
    [[ "$BASE_REF" == "$MAIN_REF" ]] || die "merge group base ref must be protected main"
    [[ "$HEAD_REF" == "$QUEUE_PREFIX"* && "$HEAD_REF" != "$QUEUE_PREFIX" ]] ||
      die "merge group head ref is outside the protected-main queue"
    [[ "$HEAD_REF" == "$GITHUB_REF_VALUE" ]] ||
      die "merge group payload and context head refs differ"
    [[ "$HEAD" == "$GITHUB_SHA_VALUE" ]] || die "merge group payload and context SHAs differ"
    require_commit "$BASE" "merge group base"
    require_commit "$HEAD" "merge group head"
    require_ancestor "$BASE" "$HEAD" "merge group base" "merge group head"
    emit_range "$BASE" "$HEAD"
    ;;
  push)
    [[ "$GITHUB_REF_PROTECTED_VALUE" == true ]] || die "push ref must be protected"
    [[ "$GITHUB_REF_VALUE" == "$MAIN_REF" ]] || die "push ref must be protected main"
    [[ "$GITHUB_WORKFLOW_SHA_VALUE" == "$GITHUB_SHA_VALUE" ]] ||
      die "push workflow revision must equal github SHA"
    PAYLOAD_REPOSITORY=$(json_string '.repository.full_name' 'push repository')
    BASE=$(json_string '.before' 'push before SHA')
    HEAD=$(json_string '.after' 'push after SHA')
    [[ "$PAYLOAD_REPOSITORY" == "$EXPECTED_REPOSITORY" ]] ||
      die "push payload repository is not trusted"
    [[ "$BASE" != "$ZERO_SHA" ]] || die "first push has no authenticated base"
    [[ "$HEAD" == "$GITHUB_SHA_VALUE" ]] || die "push payload after differs from github SHA"
    require_commit "$BASE" "push base"
    require_commit "$HEAD" "push head"
    require_ancestor "$BASE" "$HEAD" "push base" "push head"
    emit_range "$BASE" "$HEAD"
    ;;
  *) die "event is not a build-container range-consumer event" ;;
esac
