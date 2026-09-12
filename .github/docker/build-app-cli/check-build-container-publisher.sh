#!/usr/bin/env bash
set +x
set +a
set -euo pipefail

export LC_ALL=C
export BASH_ENV=
export ENV=
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CEILING_DIRECTORIES

readonly CI=.github/workflows/build-container-ci.yml
readonly PUBLISH=.github/workflows/publish-build-container.yml
readonly ROTATE=.github/workflows/rotate-build-container-gate.yml
readonly GROUP=edgezero-build-container-publication
readonly ZERO_SHA=0000000000000000000000000000000000000000

usage() {
  printf '%s\n' \
    "usage: check-build-container-publisher.sh \\" \
    "  --gate-root <canonical-G-root> \\" \
    "  --subject-root <canonical-subject-root> \\" \
    "  --gate-sha <G> \\" \
    '  --candidate-sha <T>' >&2
  exit 2
}

die() {
  printf '::error::%s\n' "$*" >&2
  exit 1
}

tool_die() {
  printf '::error::%s\n' "$*" >&2
  exit 2
}

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != "$ZERO_SHA" ]]
}

repo_git() {
  local root=$1
  shift
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_NO_LAZY_FETCH=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$root" "$@"
}

canonical_root() {
  local supplied=$1 label=$2 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] ||
    die "$label must be an absolute non-symlink directory"
  canonical=$(cd -- "$supplied" && pwd -P) || die "cannot resolve $label"
  [[ "$canonical" == "$supplied" ]] || die "$label must already be canonical"
  printf '%s' "$canonical"
}

require_repository() {
  local root=$1 revision=$2 label=$3 common_name=$4 objects_name=$5
  local top git_dir common shallow sparse status partial alternates objects physical
  [[ "$(repo_git "$root" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
    die "$label is not a Git worktree"
  top=$(repo_git "$root" rev-parse --show-toplevel 2>/dev/null) || die "cannot resolve $label top"
  [[ "$top" == "$root" ]] || die "$label must be the exact repository top level"
  git_dir=$(repo_git "$root" rev-parse --absolute-git-dir 2>/dev/null) || die "cannot resolve $label Git directory"
  common=$(repo_git "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    die "cannot resolve $label common directory"
  objects=$(repo_git "$root" rev-parse --path-format=absolute --git-path objects 2>/dev/null) ||
    die "cannot resolve $label object directory"
  [[ "$common" == /* && -d "$common" && ! -L "$common" ]] ||
    die "$label common directory must be an absolute non-symlink directory"
  physical=$(cd -- "$common" && pwd -P) || die "cannot canonicalize $label common directory"
  [[ "$physical" == "$common" ]] || die "$label common directory must already be canonical"
  common=$physical
  [[ "$objects" == /* && -d "$objects" && ! -L "$objects" ]] ||
    die "$label object directory must be an absolute non-symlink directory"
  physical=$(cd -- "$objects" && pwd -P) || die "cannot canonicalize $label object directory"
  [[ "$physical" == "$objects" ]] || die "$label object directory must already be canonical"
  objects=$physical
  [[ ! -e "$git_dir/info/grafts" && ! -L "$git_dir/info/grafts" &&
    ! -e "$common/info/grafts" && ! -L "$common/info/grafts" ]] || die "$label has grafts"
  [[ -z "$(repo_git "$root" for-each-ref --format='%(refname)' refs/replace/ 2>/dev/null)" ]] ||
    die "$label has replacement refs"
  shallow=$(repo_git "$root" rev-parse --is-shallow-repository 2>/dev/null) ||
    die "cannot inspect $label history"
  [[ "$shallow" == false ]] || die "$label must be full"
  sparse=$(repo_git "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "$label cannot be sparse"
  partial=$(repo_git "$root" config --local --get-regexp \
    '^(extensions\.partialClone|remote\..*\.promisor|remote\..*\.partialclonefilter)$' 2>/dev/null || true)
  [[ -z "$partial" ]] || die "$label cannot be partial or promisor"
  alternates="$objects/info/alternates"
  [[ ! -e "$alternates" && ! -L "$alternates" ]] || die "$label cannot use object alternates"
  [[ "$(repo_git "$root" rev-parse --verify HEAD 2>/dev/null)" == "$revision" ]] ||
    die "$label HEAD differs from its supplied revision"
  if repo_git "$root" symbolic-ref -q HEAD >/dev/null 2>&1; then
    die "$label must be detached"
  fi
  status=$(repo_git "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    die "cannot inspect $label status"
  [[ -z "$status" ]] || die "$label must be clean"
  printf -v "$common_name" '%s' "$common"
  printf -v "$objects_name" '%s' "$objects"
}

GATE_ROOT=
SUBJECT_ROOT=
GATE_SHA=
CANDIDATE_SHA=
seen=' '
while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --gate-root | --subject-root | --gate-sha | --candidate-sha) ;;
    *) usage ;;
  esac
  [[ -n "$value" && "$seen" != *" $flag "* ]] || usage
  seen+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --subject-root) SUBJECT_ROOT=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --candidate-sha) CANDIDATE_SHA=$value ;;
  esac
done
for required in --gate-root --subject-root --gate-sha --candidate-sha; do
  [[ "$seen" == *" $required "* ]] || usage
done

is_sha "$GATE_SHA" || die "gate SHA is not a full nonzero lowercase SHA"
is_sha "$CANDIDATE_SHA" || die "candidate SHA is not a full nonzero lowercase SHA"
for tool in env git jq yq mktemp rm cp chmod awk grep tr; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "publisher checker requires $tool"
done
[[ "$(yq --version 2>&1)" == 'yq (https://github.com/mikefarah/yq/) version v4.53.3' ]] ||
  tool_die "publisher checker requires mikefarah yq v4.53.3"

GATE_ROOT=$(canonical_root "$GATE_ROOT" 'gate root')
SUBJECT_ROOT=$(canonical_root "$SUBJECT_ROOT" 'subject root')
[[ "$GATE_ROOT" != "$SUBJECT_ROOT" ]] || die "gate and subject roots must differ"
require_repository "$GATE_ROOT" "$GATE_SHA" gate GATE_COMMON GATE_OBJECTS
require_repository "$SUBJECT_ROOT" "$CANDIDATE_SHA" subject SUBJECT_COMMON SUBJECT_OBJECTS
[[ "$GATE_COMMON" != "$SUBJECT_COMMON" && "$GATE_OBJECTS" != "$SUBJECT_OBJECTS" ]] ||
  die "gate and subject repositories must have separate object storage"

SCRIPT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)/check-build-container-publisher.sh
[[ "$SCRIPT" == "$GATE_ROOT/.github/docker/build-app-cli/check-build-container-publisher.sh" ]] ||
  die "publisher checker must execute from the canonical gate checkout"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/edgezero-publisher-check.XXXXXX") ||
  tool_die "cannot create publisher checker workspace"
cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  rm -rf -- "$WORK"
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

WORKFLOW_LIST="$WORK/workflows"
repo_git "$SUBJECT_ROOT" ls-tree -r --name-only "$CANDIDATE_SHA" -- .github/workflows >"$WORKFLOW_LIST" ||
  die "cannot enumerate candidate workflows"

CI_FILE=
PUBLISH_FILE=
ROTATE_FILE=
workflow_count=0
while IFS= read -r path; do
  [[ "$path" =~ ^\.github/workflows/[A-Za-z0-9._+-]+\.(yml|yaml)$ ]] ||
    die "candidate workflow path is not canonical"
  entry=$(repo_git "$SUBJECT_ROOT" ls-tree "$CANDIDATE_SHA" -- "$path") ||
    die "cannot inspect candidate workflow"
  [[ "$entry" != *$'\n'* && "$entry" == 100644\ blob\ *$'\t'"$path" ]] ||
    die "candidate workflow is not one mode-0644 Git blob"
  output="$WORK/workflow-$workflow_count.yml"
  repo_git "$SUBJECT_ROOT" show "$CANDIDATE_SHA:$path" >"$output" ||
    die "cannot extract candidate workflow"
  parsed="$WORK/workflow-$workflow_count.json"
  yq -o=json -I=0 \
    '{"document": ., "aliases": [... | select(kind == "alias")], "duplicates": [.. | select(kind == "map") | to_entries | group_by(.key) | .[] | select(length > 1)]}' \
    "$output" >"$parsed" 2>/dev/null || die "candidate workflow is not valid YAML"
  jq -e '.aliases == [] and .duplicates == [] and (.document | type) == "object"' "$parsed" >/dev/null ||
    die "candidate workflow has aliases, duplicate keys, or wrong shape"
  if [[ "$path" == "$CI" ]]; then CI_FILE=$parsed; fi
  if [[ "$path" == "$PUBLISH" ]]; then PUBLISH_FILE=$parsed; fi
  if [[ "$path" == "$ROTATE" ]]; then ROTATE_FILE=$parsed; fi
  groups="$WORK/workflow-$workflow_count.groups"
  jq -e '[
    (.document.concurrency.group? // empty),
    (.document.jobs[]?.concurrency.group? // empty)
  ] | all(.[]; type == "string")' "$parsed" >/dev/null ||
    die "candidate workflow concurrency group is malformed"
  jq -r '[
    (.document.concurrency.group? // empty),
    (.document.jobs[]?.concurrency.group? // empty)
  ][]' "$parsed" >"$groups"
  if [[ "$path" != "$CI" && "$path" != "$PUBLISH" && "$path" != "$ROTATE" ]]; then
    jq -e '
      def environment_name:
        if type == "string" then .
        elif type == "object" and (.name | type) == "string" then .name
        else null
        end;
      ([.document.jobs[]? | select(has("environment")) | .environment |
        environment_name as $name |
        select($name == null or ($name | contains("${{")) or
          ($name | ascii_downcase) == "build-container-release")] | length) == 0 and
      ([.document | .. | strings |
        select(ascii_downcase | contains("edgezero_build_container_app_private_key"))] | length) == 0
    ' "$parsed" >/dev/null ||
      die "a third workflow can select the release environment or publisher private key"
    while IFS= read -r group; do
      if [[ "$group" == *"\${{"* ]]; then
        [[ "$group" == "\${{ github.workflow }}-\${{ github.ref }}" ]] ||
          die "a third workflow has an unprovable dynamic concurrency group"
      elif [[ "$(printf '%s' "$group" | tr '[:upper:]' '[:lower:]')" == "$GROUP" ]]; then
        die "a third workflow claims the publication concurrency group"
      fi
    done <"$groups"
  fi
  workflow_count=$((workflow_count + 1))
done <"$WORKFLOW_LIST"
[[ -n "$CI_FILE" && -n "$PUBLISH_FILE" && -n "$ROTATE_FILE" ]] ||
  die "gate, publisher, or rotation workflow is absent"

parse_gate_workflow() {
  local path=$1 name=$2 destination_name=$3 entry output parsed
  entry=$(repo_git "$GATE_ROOT" ls-tree "$GATE_SHA" -- "$path") ||
    die "cannot inspect gate $name workflow"
  [[ "$entry" != *$'\n'* && "$entry" == 100644\ blob\ *$'\t'"$path" ]] ||
    die "gate $name workflow is not one mode-0644 Git blob"
  output="$WORK/gate-$name.yml"
  repo_git "$GATE_ROOT" show "$GATE_SHA:$path" >"$output" ||
    die "cannot extract gate $name workflow"
  parsed="$WORK/gate-$name.json"
  yq -o=json -I=0 \
    '{"document": ., "aliases": [... | select(kind == "alias")], "duplicates": [.. | select(kind == "map") | to_entries | group_by(.key) | .[] | select(length > 1)]}' \
    "$output" >"$parsed" 2>/dev/null || die "gate $name workflow is not valid YAML"
  jq -e '.aliases == [] and .duplicates == [] and (.document | type) == "object"' "$parsed" >/dev/null ||
    die "gate $name workflow has aliases, duplicate keys, or wrong shape"
  printf -v "$destination_name" '%s' "$parsed"
}

GATE_CI_FILE=
GATE_PUBLISH_FILE=
GATE_ROTATE_FILE=
parse_gate_workflow "$CI" gate GATE_CI_FILE
parse_gate_workflow "$PUBLISH" publisher GATE_PUBLISH_FILE
parse_gate_workflow "$ROTATE" rotation GATE_ROTATE_FILE
jq -e --slurpfile gate "$GATE_CI_FILE" '.document == $gate[0].document' "$CI_FILE" >/dev/null ||
  die "gate workflow execution graph differs from the active gate"
jq -e --slurpfile gate "$GATE_PUBLISH_FILE" '.document == $gate[0].document' "$PUBLISH_FILE" >/dev/null ||
  die "publisher workflow execution graph differs from the active gate"
jq -e --slurpfile gate "$GATE_ROTATE_FILE" '.document == $gate[0].document' "$ROTATE_FILE" >/dev/null ||
  die "rotation workflow execution graph differs from the active gate"

# shellcheck disable=SC2016 # Match literal GitHub expression strings.
common_jq='
  def no_masks:
    [.. | objects | select(has("continue-on-error"))] | length == 0;
  def exact_versions:
    [.. | objects | .uses? // empty]
    | all(type == "string" and
      (startswith("./") or test("^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*@v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$")));
  def first_guard:
    .[0].name == "assert-hosted-runner-context"
    and .[0].shell == "bash"
    and (. [0] | has("if") | not)
    and (. [0] | has("continue-on-error") | not)
    and .[0].env.BASH_ENV == "" and .[0].env.ENV == ""
    and .[0].env.EDGEZERO_RUNNER_ENVIRONMENT == "${{ runner.environment }}"
    and .[0].env.EDGEZERO_RUNNER_OS == "${{ runner.os }}"
    and .[0].env.EDGEZERO_RUNNER_ARCH == "${{ runner.arch }}"
    and .[0].env.EDGEZERO_REPOSITORY == "${{ github.repository }}"
    and (. [0].run | startswith("set -euo pipefail\n")
      and contains("[[ \"$EDGEZERO_RUNNER_ENVIRONMENT\" == github-hosted ]]")
      and contains("[[ \"$EDGEZERO_RUNNER_OS\" == Linux ]]")
      and contains("[[ \"$EDGEZERO_RUNNER_ARCH\" == X64 ]]")
      and contains("[[ \"$EDGEZERO_REPOSITORY\" == stackpop/edgezero ]]"));
  def checkout($name; $ref; $path):
    [ .[] | select(.name == $name) ] == [{
      name:$name, if:"${{ success() }}", uses:"actions/checkout@v7.0.1",
      with:{repository:"stackpop/edgezero",ref:$ref,path:$path,"persist-credentials":false,"fetch-depth":0}
    }];
'

jq -e "$common_jq"'
  .document as $w |
  ($w | no_masks) and ($w | exact_versions) and
  $w.name == "Publish build container" and
  ($w.on | keys) == ["push"] and $w.on.push == {tags:["build-container-v*"]} and
  $w.permissions == {} and
  $w.concurrency == {group:"edgezero-build-container-publication","cancel-in-progress":false,queue:"max"} and
  ($w.jobs | keys | sort) == ["build-and-verify","update-pin"] and
  ($w.jobs["build-and-verify"] as $build |
    $build["runs-on"] == "ubuntu-24.04" and
    $build.permissions == {actions:"read",contents:"read",packages:"write"} and
    ($build | has("environment") | not) and
    ($build | tostring | contains("secrets.") | not) and
    ($build.outputs | keys | sort) == ["approval-challenge","build-attempt","image-digest","provenance-protocol","release-tag","source-revision"] and
    ($build.steps | first_guard) and
    ($build.steps | checkout("checkout-active-gate"; "${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}"; ".edgezero-gate")) and
    ($build.steps | checkout("checkout-release-source"; "${{ github.sha }}"; ".edgezero-source")) and
    ($build.steps | map(.name)) == ["assert-hosted-runner-context","checkout-active-gate","checkout-release-source","assert-exact-publisher-context","verify-rotation-prerequisite","validate-release-request","stage-trusted-build-context","build-publish-and-verify","generate-approval-challenge"] and
    ([ $build.steps[] | select(.name == "assert-exact-publisher-context" and .if == "${{ success() }}") ] | length) == 1 and
    ($build.steps[4].run | contains(".edgezero-gate/.github/docker/build-app-cli/verify-gate-rotation-lock.sh")) and
    ($build.steps[5].env.EDGEZERO_RELEASE_TAG == "${{ github.ref_name }}") and
    ($build.steps[5].run |
      contains(".edgezero-gate/.github/docker/build-app-cli/classify-build-container-change.sh") and
      contains("--kind local") and contains("mode=ordinary") and contains("relevant=true") and
      contains("release-request.json") and contains("EDGEZERO_RELEASE_TAG")) and
    ($build.steps[6].run | contains(".edgezero-gate/.github/docker/build-app-cli/stage-build-context.sh")) and
    ($build.steps[7].env.BASH_ENV == "" and $build.steps[7].env.ENV == "" and
      $build.steps[7].env.EDGEZERO_REGISTRY_TOKEN == "${{ github.token }}" and
      $build.steps[7].env.EDGEZERO_REGISTRY_ACTOR == "${{ github.actor }}") and
    ($build.steps[7].run | contains("docker login ghcr.io") and contains("unset EDGEZERO_REGISTRY_TOKEN") and
      contains("trap cleanup_registry EXIT") and contains("--provenance=false") and contains("--sbom=false") and
      contains("--file") and contains("verify-published-image.sh") and contains("docker logout ghcr.io") and
      contains("rm -rf -- \"$DOCKER_CONFIG\"")) and
    (($build.steps[8].run | contains("/dev/urandom")) and
      ($build.steps[8].run | contains("od -An -N32 -tx1")) and
      (($build.steps[8].run | contains("verify-published-image.sh")) | not)) and
    ($build.steps[1:] | all(.if == "${{ success() }}"))) and
  ($w.jobs["update-pin"] as $pin |
    $pin["runs-on"] == "ubuntu-24.04" and $pin.needs == "build-and-verify" and
    $pin.environment == {name:"build-container-release",deployment:false} and
    $pin.permissions == {actions:"read",contents:"read"} and
    ($pin.steps | first_guard) and
    ($pin.steps | checkout("checkout-active-gate"; "${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}"; ".edgezero-gate")) and
    ($pin.steps | checkout("checkout-release-source"; "${{ needs.build-and-verify.outputs.source-revision }}"; ".edgezero-source")) and
    ($pin.steps | map(.name)) == ["assert-hosted-runner-context","checkout-active-gate","checkout-release-source","assert-exact-publisher-context","verify-rotation-prerequisite","verify-release-approval","mint-publisher-token","assert-publisher-token","update-pin-branch"] and
    ([ $pin.steps[] | select(.name == "assert-exact-publisher-context" and .if == "${{ success() }}") ] | length) == 1 and
    $pin.steps[6].uses == "actions/create-github-app-token@v3.2.0" and
    $pin.steps[6].with["permission-contents"] == "write" and
    $pin.steps[6].with["permission-pull-requests"] == "write" and
    ($pin.steps[5].run | contains(".edgezero-gate/.github/docker/build-app-cli/release-approval-gate.sh")) and
    ($pin.steps[7].run | contains(".edgezero-gate/.github/docker/build-app-cli/assert-build-container-app-token.sh")) and
    $pin.steps[8].env.EDGEZERO_APPROVAL_JSON == "${{ steps.approval.outputs.approval-json }}" and
    ($pin.steps[8].run | contains(".edgezero-gate/.github/docker/build-app-cli/update-image-pin-pr.sh") and
      contains("--approval-json \"$EDGEZERO_APPROVAL_JSON\"") and
      (contains("${{ steps.approval.outputs.approval-json }}") | not)) and
    ($pin.steps[1:] | all(.if == "${{ success() }}"))) and
  ([ $w | tostring | scan("delete:packages|delete-packages|packages:delete|/packages/.*/versions/.+DELETE|package-admin") ] | length) == 0
' "$PUBLISH_FILE" >/dev/null || die "publisher workflow violates its structural contract"

jq -e "$common_jq"'
  .document as $w |
  ($w | no_masks) and ($w | exact_versions) and
  $w.name == "Rotate build container gate" and
  ($w.on | keys) == ["workflow_dispatch"] and $w.on.workflow_dispatch == null and
  $w.permissions == {} and
  $w.concurrency == {group:"edgezero-build-container-publication","cancel-in-progress":false,queue:"max"} and
  ($w.jobs | keys | sort) == ["acquire","wait"] and
  all($w.jobs[]; .["runs-on"] == "ubuntu-24.04" and (.steps | first_guard)) and
  ($w.jobs.acquire as $acquire |
    $acquire.permissions == {actions:"read",contents:"read"} and
    ($acquire | has("environment") | not) and
    ($acquire | tostring | contains("secrets.") | not) and
    ($acquire.outputs | keys | sort) == ["dispatch-sha","old-gate-sha","run-actor-login"] and
    ($acquire.steps | map(.name)) == ["assert-hosted-runner-context","checkout-captured-old-gate","capture-protected-dispatch"] and
    ($acquire.steps | checkout("checkout-captured-old-gate"; "${{ vars.EDGEZERO_BUILD_CONTAINER_GATE_SHA }}"; ".edgezero-gate")) and
    ($acquire.steps[2].env | has("EDGEZERO_PUBLISHER_PREREQUISITE") | not) and
    ($acquire.steps[2].run | contains("gate-paths.txt") and contains("ls-tree") and contains("gate_entry") and contains("dispatch_entry")) and
    ($acquire.steps[1:] | all(.if == "${{ success() }}"))) and
  ($w.jobs.wait as $wait |
    $wait.needs == "acquire" and $wait.permissions == {actions:"read",contents:"read"} and
    $wait.environment == {name:"build-container-gate-rotation-lock",deployment:false} and
    ($wait.steps | map(.name)) == ["assert-hosted-runner-context","checkout-captured-old-gate","assert-exact-rotation-context"] and
    ($wait.steps | checkout("checkout-captured-old-gate"; "${{ needs.acquire.outputs.old-gate-sha }}"; ".edgezero-gate")) and
    ($wait.steps[2] | has("if") | not) and
    ($wait.steps[2] | has("continue-on-error") | not) and
    $wait.steps[2].env == {
      GITHUB_TOKEN:"${{ github.token }}",
      EDGEZERO_OLD_GATE_SHA:"${{ needs.acquire.outputs.old-gate-sha }}",
      EDGEZERO_DISPATCH_SHA:"${{ needs.acquire.outputs.dispatch-sha }}",
      EDGEZERO_RUN_ID:"${{ github.run_id }}",
      EDGEZERO_RUN_ATTEMPT:"${{ github.run_attempt }}",
      EDGEZERO_RUN_ACTOR_LOGIN:"${{ needs.acquire.outputs.run-actor-login }}"
    } and
    ($wait.steps[2].run | startswith("set -euo pipefail\n") and contains(".edgezero-gate/.github/docker/build-app-cli/verify-gate-rotation-lock.sh") and contains("waiting")))
' "$ROTATE_FILE" >/dev/null || die "rotation workflow violates its structural contract"
