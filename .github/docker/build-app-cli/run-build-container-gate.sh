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

readonly CLASSIFIER_PATH=.github/docker/build-app-cli/classify-build-container-change.sh
readonly ACTION_PIN_CHECK_PATH=.github/actions/deploy-core/tests/check-action-pins.sh
readonly STAGER_PATH=.github/docker/build-app-cli/stage-build-context.sh
readonly CONTEXT_ASSERT_PATH=.github/docker/build-app-cli/assert-build-container-context.sh
readonly IMAGE_VERIFY_PATH=.github/docker/build-app-cli/verify-published-image.sh
readonly PIN_CHECK_PATH=.github/docker/build-app-cli/check-image-pin.sh
readonly PUBLISHER_CHECK_PATH=.github/docker/build-app-cli/check-build-container-publisher.sh
readonly PUBLICATION_VERIFY_PATH=.github/docker/build-app-cli/verify-build-container-publication.sh
readonly DOCKERFILE_PATH=.github/docker/build-app-cli/Dockerfile
readonly IMAGE_MANIFEST_PATH=.github/docker/build-app-cli/image-context-paths.txt
readonly IMAGE_RECORD_PATH=.github/docker/build-app-cli/image.json
readonly EVIDENCE_RECORD_PATH=.github/docker/build-app-cli/image-release-evidence.json

usage() {
  cat >&2 <<'EOF'
usage: run-build-container-gate.sh \
  --subject-root <absolute-path> --gate-root <absolute-path> \
  --base <40-lowercase-hex> --head <40-lowercase-hex> \
  --kind <local|pin> --gate-sha <40-lowercase-hex> \
  --release-state <state> --work-root <absolute-path> \
  --completion-file <absolute-path>
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

is_digest() {
  [[ "$1" =~ ^sha256:[0-9a-f]{64}$ &&
    "$1" != sha256:0000000000000000000000000000000000000000000000000000000000000000 ]]
}

canonical_root() {
  local supplied=$1 label=$2 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] ||
    die "$label must be an absolute, non-symlink directory"
  canonical=$(cd -- "$supplied" && pwd -P) || die "cannot resolve $label"
  [[ "$canonical" == "$supplied" ]] || die "$label must already be canonical"
  printf '%s\n' "$canonical"
}

is_beneath() {
  [[ "$1" == "$2" || "$1" == "$2/"* ]]
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

require_checkout() {
  local root=$1 expected=$2 label=$3 result_name=$4
  local actual status sparse top git_directory common_directory
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
    die "$label must be a full checkout"
  actual=$(repo_git "$root" rev-parse --verify HEAD 2>/dev/null) || die "$label HEAD is absent"
  [[ "$actual" == "$expected" ]] || die "$label HEAD differs from its supplied full SHA"
  status=$(repo_git "$root" status --porcelain=v1 --untracked-files=all) ||
    die "cannot inspect $label status"
  [[ -z "$status" ]] || die "$label must be clean"
  sparse=$(repo_git "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "$label cannot be sparse"
  printf -v "$result_name" '%s' "$common_directory"
}

require_gate_helper() {
  local relative=$1 path="$GATE_ROOT/$1"
  [[ -f "$path" && ! -L "$path" && -x "$path" ]] ||
    die "gate helper is missing, linked, or not executable: $relative"
  printf '%s\n' "$path"
}

tree_blob() {
  local revision=$1 path=$2 destination=$3 entry metadata mode type recorded
  entry=$(subject_git ls-tree "$revision" -- "$path") ||
    die "cannot inspect subject path: $path"
  [[ -n "$entry" && "$entry" != *$'\n'* && "$entry" == *$'\t'* ]] ||
    die "subject path is missing or ambiguous: $path"
  metadata=${entry%%$'\t'*}
  recorded=${entry#*$'\t'}
  read -r mode type _ <<<"$metadata"
  [[ "$recorded" == "$path" && "$type" == blob &&
    ("$mode" == 100644 || "$mode" == 100755) ]] ||
    die "subject path is not a regular tracked file: $path"
  mkdir -p -- "$(dirname -- "$destination")"
  subject_git show "$revision:$path" >"$destination" ||
    die "cannot extract subject path: $path"
  if [[ "$mode" == 100755 ]]; then chmod 0755 "$destination"; else chmod 0644 "$destination"; fi
}

extract_subject_context() {
  local destination=$1 manifest="$RUN_DIR/subject-image-context-paths.txt" path
  tree_blob "$HEAD" "$IMAGE_MANIFEST_PATH" "$manifest"
  mkdir -- "$destination"
  while IFS= read -r path; do
    [[ -n "$path" && "$path" != /* && "$path" != -* && "$path" != */ &&
      "$path" != *//* && "$path" != *\\* && "$path" != */../* && "$path" != ../* ]] ||
      die "candidate image manifest contains an invalid path"
    tree_blob "$HEAD" "$path" "$destination/$path"
  done <"$manifest"
}

is_action_reference_path() {
  local path=$1 relative
  case "$path" in
    .github/workflows/*.yml | .github/workflows/*.yaml)
      relative=${path#.github/workflows/}
      [[ "$relative" != */* ]]
      ;;
    action.yml | action.yaml | */action.yml | */action.yaml) return 0 ;;
    *) return 1 ;;
  esac
}

scan_subject_action_references() {
  local checker=$1 inventory="$RUN_DIR/subject-tree-paths" destination path result count=0
  local -a inputs=()
  subject_git ls-tree -r -z --name-only "$HEAD" >"$inventory" ||
    die "cannot enumerate subject action-reference files"
  while IFS= read -r -d '' path; do
    is_action_reference_path "$path" || continue
    [[ "$path" =~ ^[A-Za-z0-9._/+-]+$ && "$path" != /* && "$path" != -* &&
      "$path" != */ && "$path" != */../* && "$path" != ../* &&
      "$path" != */./* && "$path" != *//* ]] ||
      die "subject action-reference path is not canonical: $path"
    destination="$RUN_DIR/action-reference-inputs/$path"
    tree_blob "$HEAD" "$path" "$destination"
    inputs[count]=$destination
    count=$((count + 1))
  done <"$inventory"
  ((count > 0)) || die "subject contains no action-reference files"
  result=$(bash "$checker" ${inputs[@]+"${inputs[@]}"}) ||
    die "subject action-reference policy failed"
  [[ "$result" =~ ^action\ reference\ policy\ passed\ \([1-9][0-9]*\ external\ references\)$ ]] ||
    die "subject action-reference scan was empty or malformed"
}

publish_completion() {
  local mode=$1 branch=$2 temporary="$RUN_DIR/completion"
  printf 'kind=%s\nmode=%s\nbranch=%s\n' "$KIND" "$mode" "$branch" >"$temporary"
  chmod 0600 "$temporary"
  ln -- "$temporary" "$COMPLETION_FILE" 2>/dev/null || die "completion file already exists"
  rm -f -- "$temporary"
}

SUBJECT_ROOT=
GATE_ROOT=
BASE=
HEAD=
KIND=
GATE_SHA=
RELEASE_STATE=
WORK_ROOT=
COMPLETION_FILE=

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
    --work-root)
      require_value "$@"
      set_once WORK_ROOT "$WORK_ROOT" "$2"
      shift 2
      ;;
    --completion-file)
      require_value "$@"
      set_once COMPLETION_FILE "$COMPLETION_FILE" "$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

[[ -n "$SUBJECT_ROOT" && -n "$GATE_ROOT" && -n "$BASE" && -n "$HEAD" &&
  -n "$KIND" && -n "$GATE_SHA" && -n "$RELEASE_STATE" && -n "$WORK_ROOT" &&
  -n "$COMPLETION_FILE" ]] || usage
[[ "$KIND" == local || "$KIND" == pin ]] || usage
if ! is_sha "$BASE" || ! is_sha "$HEAD" || ! is_sha "$GATE_SHA"; then
  usage
fi

SUBJECT_ROOT=$(canonical_root "$SUBJECT_ROOT" "subject root")
GATE_ROOT=$(canonical_root "$GATE_ROOT" "gate root")
WORK_ROOT=$(canonical_root "$WORK_ROOT" "work root")
[[ "$SUBJECT_ROOT" != "$GATE_ROOT" ]] || die "gate and subject roots must differ"
if is_beneath "$WORK_ROOT" "$SUBJECT_ROOT" || is_beneath "$WORK_ROOT" "$GATE_ROOT"; then
  die "work root must be outside both checkouts"
fi
[[ "$COMPLETION_FILE" == "$WORK_ROOT/"* &&
  "$(dirname -- "$COMPLETION_FILE")" == "$WORK_ROOT" ]] ||
  die "completion file must be a direct child of the work root"
[[ ! -e "$COMPLETION_FILE" && ! -L "$COMPLETION_FILE" ]] ||
  die "completion file already exists"

require_checkout "$GATE_ROOT" "$GATE_SHA" "gate checkout" GATE_COMMON_DIRECTORY
require_checkout "$SUBJECT_ROOT" "$HEAD" "subject checkout" SUBJECT_COMMON_DIRECTORY
[[ "$GATE_COMMON_DIRECTORY" != "$SUBJECT_COMMON_DIRECTORY" ]] ||
  die "gate and subject roots must use separate Git repositories"
subject_git cat-file -e "$BASE^{commit}" 2>/dev/null || die "base commit is missing"
subject_git merge-base --is-ancestor "$BASE" "$HEAD" ||
  die "base must be an ancestor of head"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ "$SCRIPT_DIR/run-build-container-gate.sh" == "$GATE_ROOT/.github/docker/build-app-cli/run-build-container-gate.sh" ]] ||
  die "driver must execute from the canonical gate checkout"

CLASSIFIER=$(require_gate_helper "$CLASSIFIER_PATH")
ACTION_PIN_CHECK=$(require_gate_helper "$ACTION_PIN_CHECK_PATH")
RUN_DIR=$(mktemp -d "$WORK_ROOT/gate-run.XXXXXX")
trap 'rm -rf -- "$RUN_DIR"' EXIT HUP INT TERM
scan_subject_action_references "$ACTION_PIN_CHECK"
CLASSIFICATION="$RUN_DIR/classification"
bash "$CLASSIFIER" \
  --subject-root "$SUBJECT_ROOT" \
  --gate-root "$GATE_ROOT" \
  --base "$BASE" \
  --head "$HEAD" \
  --kind "$KIND" \
  --gate-sha "$GATE_SHA" \
  --release-state "$RELEASE_STATE" \
  >"$CLASSIFICATION"

if cmp -s <(printf 'mode=ordinary\nrelevant=true\n') "$CLASSIFICATION"; then
  mode=ordinary
  relevant=true
elif cmp -s <(printf 'mode=ordinary\nrelevant=false\n') "$CLASSIFICATION"; then
  mode=ordinary
  relevant=false
elif cmp -s <(printf 'mode=gate-update\nrelevant=true\n') "$CLASSIFICATION"; then
  mode=gate-update
  relevant=true
elif cmp -s <(printf 'mode=gate-rollback\nrelevant=true\n') "$CLASSIFICATION"; then
  mode=gate-rollback
  relevant=true
else
  die "classifier output is missing, duplicated, malformed, or contradictory"
fi

PUBLISHER_CHECK=$(require_gate_helper "$PUBLISHER_CHECK_PATH")
bash "$PUBLISHER_CHECK" \
  --gate-root "$GATE_ROOT" \
  --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$GATE_SHA" \
  --candidate-sha "$HEAD"

if [[ "$mode" == gate-update || "$mode" == gate-rollback ]]; then
  CONTEXT_ASSERT=$(require_gate_helper "$CONTEXT_ASSERT_PATH")
  SUBJECT_CONTEXT="$RUN_DIR/subject-context"
  extract_subject_context "$SUBJECT_CONTEXT"
  bash "$CONTEXT_ASSERT" --context "$SUBJECT_CONTEXT"
  publish_completion "$mode" "$mode"
  exit 0
fi

if [[ "$relevant" == false ]]; then
  publish_completion ordinary not-applicable
  exit 0
fi

if [[ "$KIND" == local ]]; then
  STAGER=$(require_gate_helper "$STAGER_PATH")
  CONTEXT_ASSERT=$(require_gate_helper "$CONTEXT_ASSERT_PATH")
  IMAGE_VERIFY=$(require_gate_helper "$IMAGE_VERIFY_PATH")
  BUILD_CONTEXT="$RUN_DIR/context"
  IID_FILE="$RUN_DIR/image.iid"
  bash "$STAGER" \
    --gate-root "$GATE_ROOT" \
    --source-root "$SUBJECT_ROOT" \
    --gate-sha "$GATE_SHA" \
    --source-sha "$HEAD" \
    --output "$BUILD_CONTEXT"
  bash "$CONTEXT_ASSERT" --context "$BUILD_CONTEXT"

  DOCKER_BIN=$(command -v docker) || die "docker is required"
  DOCKER_BIN=$(cd -- "$(dirname -- "$DOCKER_BIN")" && pwd -P)/$(basename -- "$DOCKER_BIN")
  [[ -f "$DOCKER_BIN" && ! -L "$DOCKER_BIN" && -x "$DOCKER_BIN" ]] ||
    die "docker must be a regular executable"
  if is_beneath "$DOCKER_BIN" "$SUBJECT_ROOT" || is_beneath "$DOCKER_BIN" "$GATE_ROOT"; then
    die "docker cannot resolve from either checkout"
  fi
  "$DOCKER_BIN" build \
    --platform linux/amd64 \
    --provenance=false \
    --build-arg "IMAGE_SOURCE_REVISION=$HEAD" \
    --iidfile "$IID_FILE" \
    -f "$BUILD_CONTEXT/$DOCKERFILE_PATH" \
    "$BUILD_CONTEXT"
  [[ -f "$IID_FILE" && ! -L "$IID_FILE" ]] || die "Docker did not create a regular iidfile"
  image_id=$(sed -n '1p' "$IID_FILE")
  [[ "$(wc -l <"$IID_FILE" | tr -d '[:space:]')" -eq 1 ]] ||
    die "Docker iidfile must contain exactly one line"
  is_digest "$image_id" || die "Docker iidfile does not contain an immutable image ID"
  bash "$IMAGE_VERIFY" --local-image-id "$image_id" --source-sha "$HEAD" --protocol 1
  publish_completion ordinary relevant
  exit 0
fi

PIN_CHECK=$(require_gate_helper "$PIN_CHECK_PATH")
PUBLICATION_VERIFY=$(require_gate_helper "$PUBLICATION_VERIFY_PATH")
IMAGE_VERIFY=$(require_gate_helper "$IMAGE_VERIFY_PATH")
IMAGE_RECORD="$RUN_DIR/image.json"
EVIDENCE_RECORD="$RUN_DIR/image-release-evidence.json"
tree_blob "$HEAD" "$IMAGE_RECORD_PATH" "$IMAGE_RECORD"
tree_blob "$HEAD" "$EVIDENCE_RECORD_PATH" "$EVIDENCE_RECORD"
bash "$PIN_CHECK" validate-pair "$IMAGE_RECORD" "$EVIDENCE_RECORD"
bash "$PUBLICATION_VERIFY" \
  --gate-root "$GATE_ROOT" \
  --subject-root "$SUBJECT_ROOT" \
  --gate-sha "$GATE_SHA" \
  --candidate-sha "$HEAD" \
  --image-json "$IMAGE_RECORD" \
  --evidence-json "$EVIDENCE_RECORD"
runtime_ref=$(bash "$PIN_CHECK" runtime-ref "$IMAGE_RECORD") || die "cannot read runtime image ref"
source_revision=$(bash "$PIN_CHECK" source-revision "$IMAGE_RECORD") ||
  die "cannot read image source revision"
protocol=$(bash "$PIN_CHECK" provenance-protocol "$IMAGE_RECORD") ||
  die "cannot read image protocol"
[[ "$runtime_ref" =~ ^ghcr\.io/stackpop/edgezero-build-app-cli@sha256:[0-9a-f]{64}$ ]] ||
  die "pin checker returned an invalid runtime ref"
is_sha "$source_revision" || die "pin checker returned an invalid source revision"
[[ "$protocol" == 1 ]] || die "pin checker returned an invalid protocol"
bash "$IMAGE_VERIFY" --ref "$runtime_ref" --source-sha "$source_revision" --protocol "$protocol"
publish_completion ordinary relevant
