#!/usr/bin/env bash
{ set +x; } 2>/dev/null
set +a
set -euo pipefail

export LC_ALL=C
export BASH_ENV=
export ENV=
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_NO_REPLACE_OBJECTS=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_COMMON_DIR GIT_CEILING_DIRECTORIES
unset GIT_REPLACE_REF_BASE

readonly API_ROOT=https://api.github.com/repos/stackpop/edgezero/actions/runs
readonly API_VERSION=2026-03-10
readonly CONTENT_TYPE='application/json; charset=utf-8'
readonly COMMENT_PREFIX='edgezero-release-evidence-v1 '
readonly ENVIRONMENT=build-container-release
readonly IMAGE_RECORD_PATH=.github/docker/build-app-cli/image.json
readonly EVIDENCE_RECORD_PATH=.github/docker/build-app-cli/image-release-evidence.json
readonly PIN_CHECK_PATH=.github/docker/build-app-cli/check-image-pin.sh
readonly U64_MAX=18446744073709551615
readonly U32_MAX=4294967295

usage() {
  printf '%s\n' \
    "usage: verify-build-container-publication.sh \\" \
    "  --gate-root <canonical-G-root> \\" \
    "  --subject-root <canonical-subject-root> \\" \
    "  --gate-sha <G> \\" \
    "  --candidate-sha <T> \\" \
    "  --image-json <extracted-image-record> \\" \
    '  --evidence-json <extracted-evidence-record>' >&2
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
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != 0000000000000000000000000000000000000000 ]]
}

is_positive_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

decimal_greater_than() {
  local left=$1 right=$2
  ((${#left} > ${#right})) && return 0
  ((${#left} == ${#right})) && [[ "$left" > "$right" ]]
}

is_beneath() {
  [[ "$1" == "$2" || "$1" == "$2/"* ]]
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
    GIT_NO_REPLACE_OBJECTS=1 GIT_NO_LAZY_FETCH=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$root" "$@"
}

gate_git() {
  repo_git "$GATE_ROOT" "$@"
}

subject_git() {
  repo_git "$SUBJECT_ROOT" "$@"
}

require_checkout() {
  local root=$1 expected=$2 label=$3 common_result_name=$4 object_result_name=$5
  local top git_directory common_directory object_directory
  local canonical_common canonical_object config_status
  local replacements shallow actual status sparse gitlinks

  [[ "$(repo_git "$root" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
    die "$label is not a Git worktree"
  top=$(repo_git "$root" rev-parse --show-toplevel 2>/dev/null) ||
    die "cannot resolve $label top level"
  [[ "$top" == "$root" ]] || die "$label must be the exact repository top level"
  git_directory=$(repo_git "$root" rev-parse --absolute-git-dir 2>/dev/null) ||
    die "cannot resolve $label Git directory"
  common_directory=$(repo_git "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    die "cannot resolve $label common Git directory"
  object_directory=$(repo_git "$root" rev-parse --path-format=absolute --git-path objects 2>/dev/null) ||
    die "cannot resolve $label object directory"
  [[ -d "$common_directory" && -d "$object_directory" ]] ||
    die "$label Git directories are unavailable"
  canonical_common=$(cd -- "$common_directory" && pwd -P) ||
    die "cannot canonicalize $label common Git directory"
  canonical_object=$(cd -- "$object_directory" && pwd -P) ||
    die "cannot canonicalize $label object directory"
  if repo_git "$root" config --name-only --get-regexp \
    '^(extensions\.partial[Cc]lone|remote\..*\.(promisor|partial[Cc]lone[Ff]ilter))$' \
    >/dev/null 2>&1; then
    die "$label cannot use partial-clone or promisor configuration"
  else
    config_status=$?
    [[ "$config_status" -eq 1 ]] || die "cannot inspect $label partial-clone configuration"
  fi
  [[ ! -e "$canonical_object/info/alternates" && ! -L "$canonical_object/info/alternates" ]] ||
    die "$label cannot use an alternate object store"
  [[ ! -e "$git_directory/info/grafts" && ! -L "$git_directory/info/grafts" &&
    ! -e "$canonical_common/info/grafts" && ! -L "$canonical_common/info/grafts" ]] ||
    die "$label cannot contain legacy grafts"
  replacements=$(repo_git "$root" for-each-ref --format='%(refname)' refs/replace/ 2>/dev/null) ||
    die "cannot inspect $label replacement refs"
  [[ -z "$replacements" ]] || die "$label cannot contain replacement refs"
  shallow=$(repo_git "$root" rev-parse --is-shallow-repository 2>/dev/null) ||
    die "cannot inspect $label history depth"
  [[ "$shallow" == false ]] || die "$label must be a full checkout"
  actual=$(repo_git "$root" rev-parse --verify HEAD 2>/dev/null) || die "$label HEAD is absent"
  [[ "$actual" == "$expected" ]] || die "$label HEAD differs from its supplied full SHA"
  if repo_git "$root" symbolic-ref -q HEAD >/dev/null 2>&1; then
    die "$label must be detached"
  fi
  sparse=$(repo_git "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "$label cannot be sparse"
  status=$(repo_git "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    die "cannot inspect $label status"
  [[ -z "$status" ]] || die "$label must be clean"
  gitlinks=$(repo_git "$root" ls-tree -r "$expected" 2>/dev/null | awk '$1 == "160000" { print; exit }') ||
    die "cannot inspect $label submodule state"
  [[ -z "$gitlinks" ]] || die "$label cannot contain submodule state"
  printf -v "$common_result_name" '%s' "$canonical_common"
  printf -v "$object_result_name" '%s' "$canonical_object"
}

require_input_file() {
  local path=$1 label=$2 parent canonical_parent expected
  [[ "$path" == /* && -f "$path" && ! -L "$path" ]] ||
    die "$label must be an absolute regular non-symlink file"
  parent=${path%/*}
  [[ -n "$parent" ]] || parent=/
  canonical_parent=$(cd -- "$parent" && pwd -P) || die "cannot resolve $label parent"
  [[ "$canonical_parent" == "$parent" ]] || die "$label path must already be canonical"
  if [[ "$parent" == / ]]; then expected="/${path##*/}"; else expected="$parent/${path##*/}"; fi
  [[ "$expected" == "$path" ]] || die "$label path must already be canonical"
  ! is_beneath "$path" "$GATE_ROOT" || die "$label must be outside the gate repository"
  ! is_beneath "$path" "$SUBJECT_ROOT" || die "$label must be outside the subject repository"
}

require_exact_subject_blob() {
  local input=$1 relative=$2 label=$3 entry metadata mode type recorded
  entry=$(subject_git ls-tree "$CANDIDATE_SHA" -- "$relative" 2>/dev/null) ||
    die "cannot inspect candidate $label blob"
  [[ -n "$entry" && "$entry" != *$'\n'* && "$entry" == *$'\t'* ]] ||
    die "candidate $label blob is missing or ambiguous"
  metadata=${entry%%$'\t'*}
  recorded=${entry#*$'\t'}
  read -r mode type _ <<<"$metadata"
  [[ "$recorded" == "$relative" && "$type" == blob && "$mode" == 100644 ]] ||
    die "candidate $label path is not a regular Git blob"
  cmp -s "$input" <(subject_git show "$CANDIDATE_SHA:$relative" 2>/dev/null) ||
    die "$label bytes differ from the exact candidate blob"
}

extract_top_level_unsigned() {
  local path=$1 wanted=$2
  awk -v wanted="$wanted" '
    BEGIN { RS = "\034"; count = 0 }
    {
      s = $0
      depth = 0
      n = length(s)
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "{") { depth++; continue }
        if (c == "}" || c == "]") { depth--; continue }
        if (c == "[") { depth++; continue }
        if (c != "\"") continue

        start = i + 1
        escaped = 0
        for (j = start; j <= n; j++) {
          q = substr(s, j, 1)
          if (escaped) { escaped = 0; continue }
          if (q == "\\") { escaped = 1; continue }
          if (q == "\"") break
        }
        if (j > n) exit 1
        if (depth != 1) { i = j; continue }
        key = substr(s, start, j - start)
        k = j + 1
        while (k <= n && substr(s, k, 1) ~ /[ \t\r\n]/) k++
        if (substr(s, k, 1) != ":") { i = j; continue }
        k++
        while (k <= n && substr(s, k, 1) ~ /[ \t\r\n]/) k++
        if (key == wanted) {
          value_start = k
          while (k <= n && substr(s, k, 1) ~ /[0-9]/) k++
          if (k == value_start) exit 1
          value = substr(s, value_start, k - value_start)
          while (k <= n && substr(s, k, 1) ~ /[ \t\r\n]/) k++
          if (substr(s, k, 1) != "," && substr(s, k, 1) != "}") exit 1
          count++
          found = value
        }
        i = j
      }
    }
    END { if (count == 1) print found; else exit 1 }
  ' "$path"
}

temporary_files=()
remove_temporary_files() {
  local path
  for path in ${temporary_files[@]+"${temporary_files[@]}"}; do
    [[ -z "$path" ]] || rm -f -- "$path" 2>/dev/null || true
  done
}

cleanup_exit() {
  local status=$?
  trap - EXIT HUP INT TERM
  remove_temporary_files
  exit "$status"
}

cleanup_signal() {
  local status=$1
  trap - EXIT HUP INT TERM
  remove_temporary_files
  exit "$status"
}

trap cleanup_exit EXIT
trap 'cleanup_signal 129' HUP
trap 'cleanup_signal 130' INT
trap 'cleanup_signal 143' TERM

new_temporary_file() {
  local result_name=$1 label=$2 path
  path=$(mktemp "$TEMP_ROOT/.edgezero-publication-$label.XXXXXX" 2>/dev/null) ||
    die "cannot create temporary response file"
  temporary_files+=("$path")
  printf -v "$result_name" '%s' "$path"
}

curl_get() {
  local label=$1 url=$2 result_name=$3 link_name=$4 body metadata line link
  local -a metadata_lines=()
  new_temporary_file body body
  new_temporary_file metadata metadata

  if ! printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    "header = \"Authorization: Bearer $publication_github_token\"" |
    env -i PATH="$PATH" LC_ALL=C curl \
      --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
      --request GET --config - --output "$body" \
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\nlink=%header{link}' \
      "$url" >"$metadata" 2>/dev/null; then
    die "$label request failed"
  fi

  while IFS= read -r line || [[ -n "$line" ]]; do
    metadata_lines+=("$line")
  done <"$metadata"
  [[ "${#metadata_lines[@]}" -eq 4 ]] || die "$label response metadata is malformed"
  [[ "${metadata_lines[0]}" == 200 ]] || die "$label response did not return HTTP 200"
  [[ "${metadata_lines[1]}" == "$API_VERSION" ]] ||
    die "$label response selected an unexpected API version"
  case "${metadata_lines[2]}" in
    application/json | "$CONTENT_TYPE") ;;
    *) die "$label response has an unexpected content type" ;;
  esac
  [[ "${metadata_lines[3]}" == link=* ]] || die "$label Link metadata is malformed"
  link=${metadata_lines[3]#link=}
  printf -v "$result_name" '%s' "$body"
  printf -v "$link_name" '%s' "$link"
}

validate_review_time() {
  local value=$1 epoch round_trip
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    return 1
  epoch=$(jq -nr --arg value "$value" '$value | fromdateiso8601' 2>/dev/null) || return 1
  round_trip=$(jq -nr --argjson epoch "$epoch" \
    '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")' 2>/dev/null) || return 1
  [[ "$round_trip" == "$value" ]]
}

validate_run_response() {
  local body=$1 raw_id raw_attempt values event workflow_path head_sha head_branch status conclusion extra
  jq -e -s 'length == 1 and (.[0] | type) == "object"' "$body" >/dev/null 2>&1 ||
    die "run response is not exactly one JSON object"
  raw_id=$(extract_top_level_unsigned "$body" id 2>/dev/null) ||
    die "run response id is not one exact JSON integer"
  raw_attempt=$(extract_top_level_unsigned "$body" run_attempt 2>/dev/null) ||
    die "run response attempt is not one exact JSON integer"
  is_positive_decimal_at_most "$raw_id" "$U64_MAX" || die "run response id is out of range"
  is_positive_decimal_at_most "$raw_attempt" "$U32_MAX" || die "run response attempt is out of range"
  [[ "$raw_id" == "$RUN_ID" && "$raw_attempt" == "$RUN_ATTEMPT" ]] ||
    die "run response identifiers differ from the evidence"

  values=$(jq -er -s '
    .[0] as $run
    | if (($run.event | type) == "string")
        and (($run.path | type) == "string")
        and (($run.head_sha | type) == "string")
        and (($run.head_branch | type) == "string")
        and (($run.status | type) == "string")
        and (($run.conclusion == null) or (($run.conclusion | type) == "string"))
      then [$run.event, $run.path, $run.head_sha, $run.head_branch,
        $run.status, ($run.conclusion // "")] | @tsv
      else error("invalid run fields")
      end
  ' "$body" 2>/dev/null) || die "run response fields are malformed"
  IFS=$'\t' read -r event workflow_path head_sha head_branch status conclusion extra <<<"$values"
  [[ -z "${extra:-}" ]] || die "run response fields are malformed"
  [[ "$event" == push ]] || die "publication run event is not push"
  [[ "$workflow_path" == ".github/workflows/publish-build-container.yml@$RELEASE_TAG" ]] ||
    die "publication run path is not exact"
  [[ "$head_sha" == "$SOURCE_REVISION" ]] || die "publication run source revision differs"
  [[ "$head_branch" == "$RELEASE_TAG" ]] || die "publication run release tag differs"

  if [[ "$status" == completed ]]; then
    [[ "$conclusion" == success ]] || die "publication run did not succeed"
    return 0
  fi
  case "$status" in queued | in_progress | waiting | requested | pending) ;; *)
    die "publication run has an unknown status" ;;
  esac
  [[ -z "$conclusion" ]] || die "incomplete publication run has a conclusion"
  return 10
}

validate_jobs_response() {
  local body=$1 raw_total_count
  raw_total_count=$(extract_top_level_unsigned "$body" total_count 2>/dev/null) ||
    die "publisher jobs total count is not one exact JSON integer"
  [[ "$raw_total_count" == 2 ]] || die "publisher jobs total count is not exactly two"
  jq -e -s --arg source "$SOURCE_REVISION" --arg attempt "$RUN_ATTEMPT" '
    def valid_context_step:
      (.steps | type) == "array"
      and ([.steps[]
        | select(type == "object" and .name == "assert-exact-publisher-context")] | length) == 1
      and ([.steps[]
        | select(type == "object" and .name == "assert-exact-publisher-context")][0].conclusion
          == "success");
    length == 1
    and (.[0] | type) == "object"
    and (.[0].total_count | type) == "number"
    and .[0].total_count == 2
    and .[0].total_count == (.[0].total_count | floor)
    and (.[0].jobs | type) == "array"
    and (.[0].jobs | length) == 2
    and ([.[0].jobs[].name] | sort) == ["build-and-verify", "update-pin"]
    and all(.[0].jobs[];
      (type == "object")
      and .status == "completed"
      and .conclusion == "success"
      and .head_sha == $source
      and ((has("run_attempt") | not)
        or ((.run_attempt | type) == "number"
          and .run_attempt == (.run_attempt | floor)
          and (.run_attempt | tostring) == $attempt))
      and valid_context_step)
  ' "$body" >/dev/null 2>&1 || die "publisher jobs response is not exact"
}

validate_protocol_keys() {
  local path=$1
  jq -ne --stream '
    [inputs | select(length == 2)] as $events
    | (($events | length) == 8)
      and all($events[]; ((.[0] | length) == 1))
      and ([$events[] | .[0][0]] as $keys
        | ($keys | length) == ($keys | unique | length)
        and ($keys | sort) == (["challenge", "image-digest", "png-sha256",
          "release-tag", "reviewed-at", "run-attempt", "run-id", "source-revision"] | sort))
  ' "$path" >/dev/null 2>&1
}

validate_approvals_response() {
  local body=$1 reviews review_file comment protocol_file values
  local challenge digest screenshot tag reviewed_at attempt run_id source extra canonical
  local current_count=0
  jq -e -s 'length == 1 and (.[0] | type) == "array"' "$body" >/dev/null 2>&1 ||
    die "approvals response is not exactly one JSON array"
  new_temporary_file reviews reviews
  jq -j -s --arg prefix "$COMMENT_PREFIX" '
    .[0][]
    | select(type == "object"
        and (.comment | type) == "string"
        and (.comment | startswith($prefix)))
    | @json, "\u0000"
  ' "$body" >"$reviews" 2>/dev/null || die "cannot enumerate approval comments"

  while IFS= read -r -d '' review_file; do
    new_temporary_file protocol_file protocol
    comment=$(jq -er '.comment' <<<"$review_file" 2>/dev/null) ||
      die "cannot extract a protocol approval comment"
    printf '%s' "${comment#"$COMMENT_PREFIX"}" >"$protocol_file"
    validate_protocol_keys "$protocol_file" ||
      die "approval history contains duplicate or malformed protocol keys"
    values=$(jq -er '
      if type == "object"
          and all(.[]; type == "string")
        then [.challenge, ."image-digest", ."png-sha256", ."release-tag",
          ."reviewed-at", ."run-attempt", ."run-id", ."source-revision"] | @tsv
        else error("invalid protocol fields")
        end
    ' "$protocol_file" 2>/dev/null) || die "approval protocol fields are malformed"
    IFS=$'\t' read -r challenge digest screenshot tag reviewed_at attempt run_id source extra <<<"$values"
    [[ -z "${extra:-}" ]] || die "approval protocol fields are malformed"
    [[ "$challenge" =~ ^[0-9a-f]{64}$ ]] || die "approval challenge is malformed"
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || die "approval image digest is malformed"
    [[ "$screenshot" =~ ^sha256:[0-9a-f]{64}$ ]] || die "approval screenshot digest is malformed"
    [[ "$tag" =~ ^build-container-v[1-9][0-9]*$ ]] || die "approval release tag is malformed"
    validate_review_time "$reviewed_at" || die "approval review time is malformed"
    is_positive_decimal_at_most "$attempt" "$U32_MAX" || die "approval attempt is malformed"
    is_positive_decimal_at_most "$run_id" "$U64_MAX" || die "approval run id is malformed"
    is_sha "$source" || die "approval source revision is malformed"
    [[ "$run_id" == "$RUN_ID" ]] || die "approval protocol belongs to another run"
    decimal_greater_than "$attempt" "$RUN_ATTEMPT" && die "approval protocol claims a future attempt"
    canonical="$COMMENT_PREFIX{\"challenge\":\"$challenge\",\"image-digest\":\"$digest\",\"png-sha256\":\"$screenshot\",\"release-tag\":\"$tag\",\"reviewed-at\":\"$reviewed_at\",\"run-attempt\":\"$attempt\",\"run-id\":\"$run_id\",\"source-revision\":\"$source\"}"
    [[ "$comment" == "$canonical" ]] || die "approval protocol comment is not canonical"

    if [[ "$attempt" == "$RUN_ATTEMPT" ]]; then
      current_count=$((current_count + 1))
      [[ "$comment" == "$EXPECTED_COMMENT" ]] ||
        die "current approval protocol differs from the release evidence"
      jq -e --arg state approved --arg environment "$ENVIRONMENT" \
        --arg login "$APPROVER_LOGIN" --arg comment "$EXPECTED_COMMENT" '
          .state == $state
          and .comment == $comment
          and (.environments | type) == "array"
          and (.environments | length) == 1
          and (.environments[0] | type) == "object"
          and .environments[0].name == $environment
          and (.user | type) == "object"
          and .user.login == $login
        ' <<<"$review_file" >/dev/null 2>&1 ||
        die "current approval review metadata differs from the release evidence"
    fi
  done <"$reviews"
  [[ "$current_count" -eq 1 ]] || die "approval history lacks one unique current approval"
}

GATE_ROOT=
SUBJECT_ROOT=
GATE_SHA=
CANDIDATE_SHA=
IMAGE_JSON=
EVIDENCE_JSON=
seen_flags=' '

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --gate-root | --subject-root | --gate-sha | --candidate-sha | --image-json | --evidence-json) ;;
    *) usage ;;
  esac
  [[ -n "$value" ]] || usage
  [[ "$seen_flags" != *" $flag "* ]] || usage
  seen_flags+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --subject-root) SUBJECT_ROOT=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --candidate-sha) CANDIDATE_SHA=$value ;;
    --image-json) IMAGE_JSON=$value ;;
    --evidence-json) EVIDENCE_JSON=$value ;;
  esac
done

for required in --gate-root --subject-root --gate-sha --candidate-sha --image-json --evidence-json; do
  [[ "$seen_flags" == *" $required "* ]] || usage
done

is_sha "$GATE_SHA" || die "gate SHA is not a full nonzero lowercase SHA"
is_sha "$CANDIDATE_SHA" || die "candidate SHA is not a full nonzero lowercase SHA"
for tool in env git jq curl sleep mktemp cmp awk rm dirname bash; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "publication verifier requires $tool"
done

[[ -z "${GIT_ALTERNATE_OBJECT_DIRECTORIES+x}" ]] ||
  die "publication verifier cannot use environment-provided Git object alternates"
unset GIT_ALTERNATE_OBJECT_DIRECTORIES

[[ -n "${GITHUB_TOKEN:-}" ]] || die "GitHub token is absent"
unset publication_github_token
publication_github_token=$GITHUB_TOKEN
readonly publication_github_token
unset GITHUB_TOKEN
[[ "$publication_github_token" != *$'\n'* && "$publication_github_token" != *$'\r'* &&
  "$publication_github_token" != *'"'* && "$publication_github_token" != *\\* ]] ||
  die "GitHub token cannot be encoded safely"

GATE_ROOT=$(canonical_root "$GATE_ROOT" "gate root")
SUBJECT_ROOT=$(canonical_root "$SUBJECT_ROOT" "subject root")
[[ "$GATE_ROOT" != "$SUBJECT_ROOT" ]] || die "gate and subject roots must differ"
require_checkout "$GATE_ROOT" "$GATE_SHA" "gate checkout" \
  GATE_COMMON_DIRECTORY GATE_OBJECT_DIRECTORY
require_checkout "$SUBJECT_ROOT" "$CANDIDATE_SHA" "subject checkout" \
  SUBJECT_COMMON_DIRECTORY SUBJECT_OBJECT_DIRECTORY
[[ "$GATE_COMMON_DIRECTORY" != "$SUBJECT_COMMON_DIRECTORY" ]] ||
  die "gate and subject roots must use separate Git repositories"
[[ "$GATE_OBJECT_DIRECTORY" != "$SUBJECT_OBJECT_DIRECTORY" ]] ||
  die "gate and subject roots must use separate Git object stores"

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
[[ "$SCRIPT_DIR/verify-build-container-publication.sh" == "$GATE_ROOT/.github/docker/build-app-cli/verify-build-container-publication.sh" ]] ||
  die "publication verifier must execute from the canonical gate checkout"
PIN_CHECK="$GATE_ROOT/$PIN_CHECK_PATH"
[[ -f "$PIN_CHECK" && ! -L "$PIN_CHECK" && -x "$PIN_CHECK" ]] ||
  die "gate pin checker is missing, linked, or not executable"

require_input_file "$IMAGE_JSON" "image record"
require_input_file "$EVIDENCE_JSON" "release evidence"
require_exact_subject_blob "$IMAGE_JSON" "$IMAGE_RECORD_PATH" "image record"
require_exact_subject_blob "$EVIDENCE_JSON" "$EVIDENCE_RECORD_PATH" "release evidence"

PAIR_OUTPUT=$(cd -- "$GATE_ROOT" && bash "$PIN_CHECK" validate-pair "$IMAGE_JSON" "$EVIDENCE_JSON" 2>/dev/null) ||
  die "gate pin checker rejected the release record pair"
[[ "$PAIR_OUTPUT" == 'image release record pair is valid' ]] ||
  die "gate pin checker returned unexpected output"

IMAGE_VALUES=$(jq -er '[.tag, .digest, ."image-source-revision"] | @tsv' "$IMAGE_JSON" 2>/dev/null) ||
  die "cannot parse validated image values"
IFS=$'\t' read -r RELEASE_TAG IMAGE_DIGEST SOURCE_REVISION IMAGE_EXTRA <<<"$IMAGE_VALUES"
[[ -z "${IMAGE_EXTRA:-}" ]] || die "validated image values are malformed"
EVIDENCE_VALUES=$(jq -er '[."approval-challenge", ."approver-login", ."reviewed-at",
  ."run-attempt", ."run-id", ."screenshot-sha256"] | @tsv' "$EVIDENCE_JSON" 2>/dev/null) ||
  die "cannot parse validated evidence values"
IFS=$'\t' read -r APPROVAL_CHALLENGE APPROVER_LOGIN REVIEWED_AT RUN_ATTEMPT RUN_ID \
  SCREENSHOT_SHA256 EVIDENCE_EXTRA <<<"$EVIDENCE_VALUES"
[[ -z "${EVIDENCE_EXTRA:-}" ]] || die "validated evidence values are malformed"
EXPECTED_COMMENT="$COMMENT_PREFIX{\"challenge\":\"$APPROVAL_CHALLENGE\",\"image-digest\":\"$IMAGE_DIGEST\",\"png-sha256\":\"$SCREENSHOT_SHA256\",\"release-tag\":\"$RELEASE_TAG\",\"reviewed-at\":\"$REVIEWED_AT\",\"run-attempt\":\"$RUN_ATTEMPT\",\"run-id\":\"$RUN_ID\",\"source-revision\":\"$SOURCE_REVISION\"}"

TEMP_ROOT=${IMAGE_JSON%/*}
[[ -n "$TEMP_ROOT" ]] || TEMP_ROOT=/

RUN_BODY=
RUN_LINK=
run_complete=false
for poll in {1..30}; do
  curl_get "publication run" "$API_ROOT/$RUN_ID" RUN_BODY RUN_LINK
  [[ -z "$RUN_LINK" ]] || die "publication run response has an unexpected Link header"
  if validate_run_response "$RUN_BODY"; then
    run_complete=true
    break
  else
    status=$?
    [[ "$status" -eq 10 ]] || exit "$status"
  fi
  ((poll < 30)) || die "publication run did not complete after thirty polls"
  env -i PATH="$PATH" LC_ALL=C sleep 10 2>/dev/null || tool_die "cannot sleep between run polls"
done
[[ "$run_complete" == true ]] || die "publication run did not complete"

JOBS_BODY=
JOBS_LINK=
curl_get "publisher jobs" \
  "$API_ROOT/$RUN_ID/attempts/$RUN_ATTEMPT/jobs?per_page=100&page=1" JOBS_BODY JOBS_LINK
[[ -z "$JOBS_LINK" ]] || die "publisher jobs response requires pagination"
validate_jobs_response "$JOBS_BODY"

APPROVALS_BODY=
APPROVALS_LINK=
curl_get "approval history" "$API_ROOT/$RUN_ID/approvals" APPROVALS_BODY APPROVALS_LINK
[[ -z "$APPROVALS_LINK" ]] || die "approval history response has an unexpected Link header"
validate_approvals_response "$APPROVALS_BODY"

FINAL_RUN_BODY=
FINAL_RUN_LINK=
curl_get "final publication run" "$API_ROOT/$RUN_ID" FINAL_RUN_BODY FINAL_RUN_LINK
[[ -z "$FINAL_RUN_LINK" ]] || die "final publication run response has an unexpected Link header"
if validate_run_response "$FINAL_RUN_BODY"; then
  :
else
  status=$?
  [[ "$status" -ne 10 ]] || die "publication run changed before final verification"
  exit "$status"
fi
