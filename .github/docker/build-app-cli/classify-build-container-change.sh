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

readonly GATE_MANIFEST=.github/docker/build-app-cli/gate-paths.txt
readonly IMAGE_MANIFEST=.github/docker/build-app-cli/image-context-paths.txt
readonly CODEOWNERS=.github/CODEOWNERS
readonly RELEASE_REQUEST=.github/docker/build-app-cli/release-request.json
readonly IMAGE_RECORD=.github/docker/build-app-cli/image.json
readonly EVIDENCE_RECORD=.github/docker/build-app-cli/image-release-evidence.json
readonly GATE_TEAM=@stackpop/edgezero-build-container-gate-reviewers
readonly MAX_MANIFEST_BYTES=65536
readonly MAX_RELEASE_REQUEST_BYTES=4096

usage() {
  cat >&2 <<'EOF'
usage: classify-build-container-change.sh \
  --subject-root <absolute-path> --gate-root <absolute-path> \
  --base <40-lowercase-hex> --head <40-lowercase-hex> \
  --kind <local|pin> --gate-sha <40-lowercase-hex> \
  --release-state <enabled|disabled-state>
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
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != 0000000000000000000000000000000000000000 ]]
}

is_positive_u64() {
  local value=$1 maximum=18446744073709551615
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  # Equal-length canonical decimals are intentionally compared lexically.
  # shellcheck disable=SC2071
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

canonical_root() {
  local supplied=$1 label=$2 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] ||
    die "$label must be an absolute, non-symlink directory"
  canonical=$(cd -- "$supplied" && pwd -P) || die "cannot resolve $label"
  [[ "$canonical" == "$supplied" ]] || die "$label must already be canonical"
  printf '%s\n' "$canonical"
}

repo_git() {
  local root=$1
  shift
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$root" "$@"
}

subject_git() {
  repo_git "$SUBJECT_ROOT" "$@"
}

gate_git() {
  repo_git "$GATE_ROOT" "$@"
}

require_clean_full_repository() {
  local root=$1 label=$2 result_name=$3
  local status sparse top git_directory common_directory
  [[ "$(repo_git "$root" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
    die "$label is not a Git worktree"
  top=$(repo_git "$root" rev-parse --show-toplevel 2>/dev/null) ||
    die "cannot resolve $label top level"
  [[ "$top" == "$root" ]] || die "$label must be the exact repository top level"
  git_directory=$(repo_git "$root" rev-parse --absolute-git-dir 2>/dev/null) ||
    die "cannot resolve $label Git directory"
  common_directory=$(repo_git "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    die "cannot resolve $label common Git directory"
  [[ ! -e "$git_directory/info/grafts" && ! -L "$git_directory/info/grafts" &&
    ! -e "$common_directory/info/grafts" && ! -L "$common_directory/info/grafts" ]] ||
    die "$label cannot contain legacy grafts"
  [[ -z "$(repo_git "$root" for-each-ref --format='%(refname)' refs/replace/)" ]] ||
    die "$label cannot contain replacement refs"
  [[ "$(repo_git "$root" rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
    die "$label must be a full, non-shallow checkout"
  sparse=$(repo_git "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "$label cannot be sparse"
  status=$(repo_git "$root" status --porcelain=v1 --untracked-files=all) ||
    die "cannot inspect $label status"
  [[ -z "$status" ]] || die "$label must be clean"
  printf -v "$result_name" '%s' "$common_directory"
}

require_commit() {
  local root=$1 revision=$2 label=$3 type
  type=$(repo_git "$root" cat-file -t "$revision" 2>/dev/null) || die "$label commit is missing"
  [[ "$type" == commit ]] || die "$label must identify one exact commit"
}

valid_path() {
  local path=$1
  [[ -n "$path" && "$path" != /* && "$path" != -* && "$path" != */ &&
    "$path" != . && "$path" != .. && "$path" != ../* && "$path" != */../* &&
    "$path" != */.. && "$path" != */./* && "$path" != */. && "$path" != *//* &&
    "$path" != *\\* ]] || return 1
  ! LC_ALL=C grep -q '[[:cntrl:]]' <<<"$path"
}

manifest_path() {
  [[ "$1" =~ ^[A-Za-z0-9._/+-]+$ ]] && valid_path "$1"
}

tree_entry() {
  local root=$1 revision=$2 path=$3 entry recorded_path
  entry=$(repo_git "$root" ls-tree "$revision" -- "$path") ||
    die "cannot inspect $path at $revision"
  if [[ -n "$entry" ]]; then
    [[ "$entry" != *$'\n'* && "$entry" == *$'\t'* ]] ||
      die "ambiguous tree entry for $path at $revision"
    recorded_path=${entry#*$'\t'}
    [[ "$recorded_path" == "$path" ]] || die "tree entry path mismatch for $path"
  fi
  printf '%s' "$entry"
}

require_regular_tree_file() {
  local root=$1 revision=$2 path=$3 label=$4 entry metadata mode type object
  entry=$(tree_entry "$root" "$revision" "$path")
  [[ -n "$entry" ]] || die "$label is missing: $path"
  metadata=${entry%%$'\t'*}
  read -r mode type object <<<"$metadata"
  [[ "$type" == blob && ("$mode" == 100644 || "$mode" == 100755) &&
    "$object" =~ ^[0-9a-f]{40,64}$ ]] || die "$label must be a regular Git blob: $path"
}

extract_tree_file() {
  local root=$1 revision=$2 path=$3 destination=$4 label=$5
  require_regular_tree_file "$root" "$revision" "$path" "$label"
  repo_git "$root" show "$revision:$path" >"$destination" || die "cannot read $label"
}

require_canonical_path_file() {
  local file=$1 label=$2 size last path previous=
  size=$(wc -c <"$file" | tr -d '[:space:]') || die "cannot measure $label"
  [[ "$size" =~ ^[0-9]+$ ]] || die "cannot measure $label"
  ((size > 0 && size <= MAX_MANIFEST_BYTES)) || die "$label has an invalid size"
  last=$(tail -c 1 "$file" | od -An -tx1 | tr -d '[:space:]') ||
    die "cannot inspect $label terminator"
  [[ "$last" == 0a ]] || die "$label must end in exactly one LF-delimited record"
  while IFS= read -r path; do
    manifest_path "$path" || die "$label contains an invalid path"
    [[ -z "$previous" || "$previous" < "$path" ]] ||
      die "$label must be byte-sorted with unique paths"
    previous=$path
  done <"$file"
}

contains_path() {
  grep -Fqx -e "$1" "$2"
}

validate_codeowners() {
  local root=$1 revision=$2 manifest=$3 prefix=$4 expected actual path
  expected="$WORK_DIR/codeowners-$prefix.expected"
  actual="$WORK_DIR/codeowners-$prefix.actual"
  : >"$expected"
  while IFS= read -r path; do
    printf '/%s %s\n' "$path" "$GATE_TEAM" >>"$expected"
  done <"$manifest"
  extract_tree_file "$root" "$revision" "$CODEOWNERS" "$actual" "CODEOWNERS"
  cmp -s "$expected" "$actual" ||
    die "CODEOWNERS must assign every manifested path exactly to $GATE_TEAM"
}

validate_manifest_at() {
  local root=$1 revision=$2 prefix=$3 output_manifest=$4 output_image=$5
  local path gate_count image_count
  extract_tree_file "$root" "$revision" "$GATE_MANIFEST" "$output_manifest" \
    "gate path manifest"
  require_canonical_path_file "$output_manifest" "gate path manifest"
  contains_path "$GATE_MANIFEST" "$output_manifest" || die "gate manifest must contain itself"
  contains_path "$CODEOWNERS" "$output_manifest" || die "gate manifest must contain CODEOWNERS"
  contains_path "$IMAGE_MANIFEST" "$output_manifest" ||
    die "gate manifest must contain the image-context manifest"
  ! contains_path "$RELEASE_REQUEST" "$output_manifest" ||
    die "release request cannot be gate-owned"
  ! contains_path "$IMAGE_RECORD" "$output_manifest" || die "image record cannot be gate-owned"
  ! contains_path "$EVIDENCE_RECORD" "$output_manifest" ||
    die "image release evidence cannot be gate-owned"

  while IFS= read -r path; do
    require_regular_tree_file "$root" "$revision" "$path" "manifested gate path"
  done <"$output_manifest"

  extract_tree_file "$root" "$revision" "$IMAGE_MANIFEST" "$output_image" \
    "image-context manifest"
  require_canonical_path_file "$output_image" "image-context manifest"
  contains_path "$IMAGE_MANIFEST" "$output_image" ||
    die "image-context manifest must contain itself"
  while IFS= read -r path; do
    contains_path "$path" "$output_manifest" ||
      die "every image-context path must also be gate-owned: $path"
    require_regular_tree_file "$root" "$revision" "$path" "manifested image-context path"
  done <"$output_image"
  gate_count=$(wc -l <"$output_manifest" | tr -d '[:space:]')
  image_count=$(wc -l <"$output_image" | tr -d '[:space:]')
  ((image_count < gate_count)) || die "image-context manifest must be a strict gate-manifest subset"
  validate_codeowners "$root" "$revision" "$output_manifest" "$prefix"
}

manifest_tree_equal() {
  local left_root=$1 left_revision=$2 right_root=$3 right_revision=$4 manifest=$5 path
  local left_entry right_entry
  while IFS= read -r path; do
    left_entry=$(tree_entry "$left_root" "$left_revision" "$path")
    right_entry=$(tree_entry "$right_root" "$right_revision" "$path")
    [[ "$left_entry" == "$right_entry" ]] || return 1
  done <"$manifest"
}

make_union() {
  LC_ALL=C sort -u "$1" "$2" >"$3"
}

collect_changes() {
  local from=$1 to=$2 destination=$3 raw status path
  raw="$destination.raw"
  : >"$destination"
  subject_git diff --name-status --no-renames -z "$from" "$to" >"$raw" ||
    die "cannot inspect candidate range"
  exec 3<"$raw"
  while IFS= read -r -d '' status <&3; do
    IFS= read -r -d '' path <&3 || die "candidate range has truncated path data"
    [[ "$status" == A || "$status" == M || "$status" == D ]] ||
      die "candidate range contains unsupported status: $status"
    valid_path "$path" || die "candidate range contains an ambiguous path"
    printf '%s\t%s\n' "$status" "$path" >>"$destination"
  done
  exec 3<&-
}

change_count() {
  wc -l <"$1" | tr -d '[:space:]'
}

changed_status() {
  local wanted=$1 changes=$2 status path
  while IFS=$'\t' read -r status path; do
    if [[ "$path" == "$wanted" ]]; then
      printf '%s\n' "$status"
      return 0
    fi
  done <"$changes"
  return 1
}

require_changed_entries_not_gitlinks() {
  local changes=$1 status path entry metadata mode type object revision
  while IFS=$'\t' read -r status path; do
    for revision in "$BASE" "$HEAD"; do
      entry=$(tree_entry "$SUBJECT_ROOT" "$revision" "$path")
      [[ -z "$entry" ]] && continue
      metadata=${entry%%$'\t'*}
      read -r mode type object <<<"$metadata"
      [[ "$mode" != 160000 && "$type" != commit ]] ||
        die "candidate range contains a gitlink: $path"
    done
  done <"$changes"
}

require_changes_within_manifest() {
  local changes=$1 manifest=$2 status path seen=0
  while IFS=$'\t' read -r status path; do
    contains_path "$path" "$manifest" || return 1
    seen=1
  done <"$changes"
  ((seen == 1))
}

validate_release_request() {
  local file=$1 size events keys json canonical gate protocol tag
  size=$(wc -c <"$file" | tr -d '[:space:]') || die "cannot measure release request"
  [[ "$size" =~ ^[0-9]+$ ]] || die "cannot measure release request"
  ((size > 0 && size <= MAX_RELEASE_REQUEST_BYTES)) || die "release request has invalid size"
  events=$(jq -c --stream . "$file" 2>/dev/null) || die "release request is not valid JSON"
  keys=$(jq -ces '
      if any(.[]; length == 2 and (.[0] | length) != 1) then error("nested") else
        [.[] | select(length == 2) | .[0][0]] as $keys
        | if (($keys | length) == 3 and ($keys | unique | length) == 3
              and ($keys | sort) == ["gate-sha","provenance-protocol","release-tag"])
          then $keys else error("keys") end
      end
    ' <<<"$events" 2>/dev/null) || die "release request has wrong or duplicate fields"
  [[ -n "$keys" ]] || die "release request has no fields"
  json=$(jq -ces 'if length == 1 and (.[0] | type) == "object" then .[0] else error("object") end' \
    "$file" 2>/dev/null) || die "release request must be one object"
  jq -e '
      (."gate-sha" | type) == "string"
      and (."provenance-protocol" | type) == "number"
      and (."release-tag" | type) == "string"
    ' <<<"$json" >/dev/null || die "release request fields have wrong types"
  gate=$(jq -r '."gate-sha"' <<<"$json")
  protocol=$(jq -r '."provenance-protocol"' <<<"$json")
  tag=$(jq -r '."release-tag"' <<<"$json")
  [[ "$gate" == "$GATE_SHA" && "$protocol" == 1 &&
    "$tag" =~ ^build-container-v[1-9][0-9]*$ ]] || die "release request identity is invalid"
  canonical=$(jq -cn --arg gate "$gate" --arg tag "$tag" \
    '{"gate-sha":$gate,"provenance-protocol":1,"release-tag":$tag}')
  cmp -s <(printf '%s' "$canonical") "$file" || die "release request bytes are not canonical"
}

emit() {
  printf 'mode=%s\nrelevant=%s\n' "$1" "$2"
}

SUBJECT_ROOT=
GATE_ROOT=
BASE=
HEAD=
KIND=
GATE_SHA=
RELEASE_STATE=

while (($#)); do
  case "$1" in
    --subject-root)
      require_value "$@"
      set_once SUBJECT_ROOT "$SUBJECT_ROOT" "$2"
      shift 2
      ;;
    --gate-root)
      require_value "$@"
      set_once GATE_ROOT "$GATE_ROOT" "$2"
      shift 2
      ;;
    --base)
      require_value "$@"
      set_once BASE "$BASE" "$2"
      shift 2
      ;;
    --head)
      require_value "$@"
      set_once HEAD "$HEAD" "$2"
      shift 2
      ;;
    --kind)
      require_value "$@"
      set_once KIND "$KIND" "$2"
      shift 2
      ;;
    --gate-sha)
      require_value "$@"
      set_once GATE_SHA "$GATE_SHA" "$2"
      shift 2
      ;;
    --release-state)
      require_value "$@"
      set_once RELEASE_STATE "$RELEASE_STATE" "$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "$SUBJECT_ROOT" && -n "$GATE_ROOT" && -n "$BASE" && -n "$HEAD" &&
  -n "$KIND" && -n "$GATE_SHA" && -n "$RELEASE_STATE" ]] || usage
[[ "$KIND" == local || "$KIND" == pin ]] || usage
if ! is_sha "$BASE" || ! is_sha "$HEAD" || ! is_sha "$GATE_SHA"; then
  usage
fi

SUBJECT_ROOT=$(canonical_root "$SUBJECT_ROOT" "subject root")
GATE_ROOT=$(canonical_root "$GATE_ROOT" "gate root")
[[ "$SUBJECT_ROOT" != "$GATE_ROOT" ]] || die "gate and subject roots must be distinct"
require_clean_full_repository "$SUBJECT_ROOT" "subject checkout" SUBJECT_COMMON_DIRECTORY
require_clean_full_repository "$GATE_ROOT" "gate checkout" GATE_COMMON_DIRECTORY
[[ "$SUBJECT_COMMON_DIRECTORY" != "$GATE_COMMON_DIRECTORY" ]] ||
  die "gate and subject roots must use separate Git repositories"
require_commit "$SUBJECT_ROOT" "$BASE" "base"
require_commit "$SUBJECT_ROOT" "$HEAD" "head"
require_commit "$SUBJECT_ROOT" "$GATE_SHA" "gate"
require_commit "$GATE_ROOT" "$GATE_SHA" "gate checkout"
[[ "$(gate_git rev-parse HEAD)" == "$GATE_SHA" ]] ||
  die "gate checkout HEAD differs from active gate SHA"
subject_git merge-base --is-ancestor "$BASE" "$HEAD" ||
  die "base must be an ancestor of head"

WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-classify.XXXXXX")
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM
OLD_MANIFEST="$WORK_DIR/old-gate-paths.txt"
OLD_IMAGE_MANIFEST="$WORK_DIR/old-image-context-paths.txt"
HEAD_MANIFEST="$WORK_DIR/head-gate-paths.txt"
HEAD_IMAGE_MANIFEST="$WORK_DIR/head-image-context-paths.txt"
UNION_MANIFEST="$WORK_DIR/union-gate-paths.txt"
CHANGES="$WORK_DIR/changes.txt"
BASE_MANIFEST="$WORK_DIR/base-gate-paths.txt"
BASE_IMAGE_MANIFEST="$WORK_DIR/base-image-context-paths.txt"

validate_manifest_at "$GATE_ROOT" "$GATE_SHA" old "$OLD_MANIFEST" "$OLD_IMAGE_MANIFEST"
validate_manifest_at "$SUBJECT_ROOT" "$HEAD" head "$HEAD_MANIFEST" "$HEAD_IMAGE_MANIFEST"
if [[ "$RELEASE_STATE" =~ ^disabled:[1-9][0-9]*:[0-9a-f]{40}:[0-9a-f]{40}$ ]]; then
  HEAD_OLD_UNION="$WORK_DIR/head-old-union.txt"
  validate_manifest_at "$SUBJECT_ROOT" "$BASE" base "$BASE_MANIFEST" "$BASE_IMAGE_MANIFEST"
  make_union "$OLD_MANIFEST" "$HEAD_MANIFEST" "$HEAD_OLD_UNION"
  make_union "$HEAD_OLD_UNION" "$BASE_MANIFEST" "$UNION_MANIFEST"
else
  make_union "$OLD_MANIFEST" "$HEAD_MANIFEST" "$UNION_MANIFEST"
fi
collect_changes "$BASE" "$HEAD" "$CHANGES"
require_changed_entries_not_gitlinks "$CHANGES"

gate_changed=false
non_gate_changed=false
while IFS=$'\t' read -r status path; do
  if contains_path "$path" "$UNION_MANIFEST"; then
    gate_changed=true
  else
    non_gate_changed=true
  fi
done <"$CHANGES"

if [[ "$gate_changed" == true ]]; then
  [[ "$non_gate_changed" == false ]] || die "gate and non-gate changes cannot be mixed"
  require_changes_within_manifest "$CHANGES" "$UNION_MANIFEST" ||
    die "gate changes escaped the old/candidate manifest union"
  if manifest_tree_equal "$SUBJECT_ROOT" "$BASE" "$GATE_ROOT" "$GATE_SHA" "$OLD_MANIFEST"; then
    [[ "$RELEASE_STATE" == enabled ]] || die "gate update requires enabled release state"
    emit gate-update true
    exit 0
  fi

  [[ "$RELEASE_STATE" =~ ^disabled:([1-9][0-9]*):([0-9a-f]{40}):([0-9a-f]{40})$ ]] ||
    die "gate rollback requires the exact disabled release state"
  LOCK_RUN_ID=${BASH_REMATCH[1]}
  DISABLED_OLD_GATE=${BASH_REMATCH[2]}
  FAILED_GATE=${BASH_REMATCH[3]}
  is_positive_u64 "$LOCK_RUN_ID" || die "gate rollback lock run id is invalid"
  [[ "$DISABLED_OLD_GATE" == "$GATE_SHA" && "$FAILED_GATE" == "$BASE" ]] ||
    die "gate rollback state does not bind the active and failed gates"
  subject_git merge-base --is-ancestor "$GATE_SHA" "$BASE" ||
    die "failed gate does not descend from active gate"

  FAILED_UNION="$WORK_DIR/failed-union.txt"
  FAILED_RANGE="$WORK_DIR/failed-range.txt"
  make_union "$OLD_MANIFEST" "$BASE_MANIFEST" "$FAILED_UNION"
  collect_changes "$GATE_SHA" "$BASE" "$FAILED_RANGE"
  require_changed_entries_not_gitlinks "$FAILED_RANGE"
  require_changes_within_manifest "$FAILED_RANGE" "$FAILED_UNION" ||
    die "failed gate was not a canonical gate-only update"
  manifest_tree_equal "$SUBJECT_ROOT" "$HEAD" "$GATE_ROOT" "$GATE_SHA" "$FAILED_UNION" ||
    die "rollback head does not restore the active gate tree exactly"
  emit gate-rollback true
  exit 0
fi

[[ "$RELEASE_STATE" == enabled ]] || die "ordinary changes require enabled release state"
manifest_tree_equal "$SUBJECT_ROOT" "$BASE" "$GATE_ROOT" "$GATE_SHA" "$OLD_MANIFEST" ||
  die "protected base gate bytes differ from active gate"
manifest_tree_equal "$SUBJECT_ROOT" "$HEAD" "$GATE_ROOT" "$GATE_SHA" "$OLD_MANIFEST" ||
  die "candidate gate bytes differ from active gate"

release_status=$(changed_status "$RELEASE_REQUEST" "$CHANGES" || true)
image_status=$(changed_status "$IMAGE_RECORD" "$CHANGES" || true)
evidence_status=$(changed_status "$EVIDENCE_RECORD" "$CHANGES" || true)

if [[ -n "$release_status" ]]; then
  [[ "$(change_count "$CHANGES")" -eq 1 && "$release_status" == A ]] ||
    die "release request must be the sole newly added path"
  RELEASE_FILE="$WORK_DIR/release-request.json"
  extract_tree_file "$SUBJECT_ROOT" "$HEAD" "$RELEASE_REQUEST" "$RELEASE_FILE" "release request"
  validate_release_request "$RELEASE_FILE"
  if [[ "$KIND" == local ]]; then emit ordinary true; else emit ordinary false; fi
  exit 0
fi

if [[ -n "$image_status" || -n "$evidence_status" ]]; then
  [[ -n "$image_status" && -n "$evidence_status" && "$(change_count "$CHANGES")" -eq 2 ]] ||
    die "image pin records must change as an exact pair and no other path"
  [[ "$image_status" == "$evidence_status" &&
    ("$image_status" == A || "$image_status" == M) ]] ||
    die "image pin records must be added or changed together"
  require_regular_tree_file "$SUBJECT_ROOT" "$HEAD" "$IMAGE_RECORD" "image record"
  require_regular_tree_file "$SUBJECT_ROOT" "$HEAD" "$EVIDENCE_RECORD" "release evidence"
  if [[ "$KIND" == pin ]]; then emit ordinary true; else emit ordinary false; fi
  exit 0
fi

emit ordinary false
