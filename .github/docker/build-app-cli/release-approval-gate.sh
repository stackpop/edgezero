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

readonly API_VERSION=2026-03-10
readonly API_ROOT=https://api.github.com/repos/stackpop/edgezero/actions/runs
readonly COMMENT_PREFIX='edgezero-release-evidence-v1 '
readonly RELEASE_ENVIRONMENT=build-container-release
readonly U64_MAX=18446744073709551615
readonly U32_MAX=4294967295

usage() {
  printf '%s\n' \
    "usage: release-approval-gate.sh \\" \
    "  --gate-root <canonical-G-root> \\" \
    "  --gate-sha <G> \\" \
    "  --run-id <canonical-positive-u64> \\" \
    "  --run-attempt <canonical-positive-u32> \\" \
    "  --build-attempt <canonical-positive-u32> \\" \
    "  --source-revision <40-lowercase-hex> \\" \
    "  --release-tag <build-container-v<positive-decimal>> \\" \
    "  --image-digest <sha256:64-lowercase-hex> \\" \
    "  --approval-challenge <64-lowercase-hex> \\" \
    '  --approval-out <new-file>' >&2
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

is_positive_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

isolated_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null "$@"
}

gate_git() {
  isolated_git -C "$GATE_ROOT" "$@"
}

temporary_files=()
remove_temporary_files() {
  local path
  for path in ${temporary_files[@]+"${temporary_files[@]}"}; do
    if [[ -n "$path" ]]; then
      rm -f -- "$path" 2>/dev/null || true
    fi
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

curl_get() {
  local label=$1 url=$2 result_name=$3 body metadata line
  local -a metadata_lines=()

  body=$(mktemp "$OUTPUT_PARENT/.edgezero-github-body.XXXXXX" 2>/dev/null) ||
    die "cannot create API response file"
  temporary_files+=("$body")
  metadata=$(mktemp "$OUTPUT_PARENT/.edgezero-github-metadata.XXXXXX" 2>/dev/null) ||
    die "cannot create API metadata file"
  temporary_files+=("$metadata")

  if ! printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    "header = \"Authorization: Bearer $TOKEN\"" |
    env -i PATH="$PATH" LC_ALL=C curl \
      --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
      --request GET --config - \
      --output "$body" \
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' \
      "$url" >"$metadata" 2>/dev/null; then
    die "$label request failed"
  fi

  while IFS= read -r line || [[ -n "$line" ]]; do
    metadata_lines+=("$line")
  done <"$metadata"
  [[ "${#metadata_lines[@]}" -eq 3 ]] || die "$label response metadata is malformed"
  [[ "${metadata_lines[0]}" == 200 ]] || die "$label response did not return HTTP 200"
  [[ "${metadata_lines[1]}" == "$API_VERSION" ]] ||
    die "$label response selected an unexpected API version"
  [[ "${metadata_lines[2]}" =~ ^[Aa][Pp][Pp][Ll][Ii][Cc][Aa][Tt][Ii][Oo][Nn]/[Jj][Ss][Oo][Nn]([[:space:]]*\;[[:space:]]*[Cc][Hh][Aa][Rr][Ss][Ee][Tt][[:space:]]*=[[:space:]]*[Uu][Tt][Ff]-8)?$ ]] ||
    die "$label response has an unsupported media type"

  printf -v "$result_name" '%s' "$body"
}

validate_protocol_json_keys() {
  local approvals_body=$1 protocol_count index raw_protocol
  protocol_count=$(jq -er -s --arg prefix "$COMMENT_PREFIX" '
    if length == 1 and (.[0] | type) == "array" then
      [.[0][]
        | select(type == "object"
            and (.comment | type) == "string"
            and (.comment | startswith($prefix)))]
      | length
    else error("invalid approval body")
    end
  ' "$approvals_body" 2>/dev/null) ||
    die "approval history response is not one valid JSON array"
  [[ "$protocol_count" =~ ^[0-9]+$ ]] || die "approval protocol count is malformed"

  index=0
  while ((index < protocol_count)); do
    raw_protocol=$(mktemp "$OUTPUT_PARENT/.edgezero-approval-comment.XXXXXX" 2>/dev/null) ||
      die "cannot create protocol comment file"
    temporary_files+=("$raw_protocol")
    jq -e -j -r -s \
      --arg prefix "$COMMENT_PREFIX" \
      --argjson index "$index" '
        [.[0][]
          | select(type == "object"
              and (.comment | type) == "string"
              and (.comment | startswith($prefix)))]
        | .[$index].comment
        | ltrimstr($prefix)
      ' "$approvals_body" >"$raw_protocol" 2>/dev/null ||
      die "cannot extract protocol comment"
    jq -ne --stream '
      [inputs | select(length == 2)] as $events
      | (($events | length) > 0)
        and all($events[]; ((.[0] | length) == 1))
        and ([$events[] | .[0][0]] as $keys
          | ($keys | length) == ($keys | unique | length))
    ' "$raw_protocol" >/dev/null 2>&1 ||
      die "approval history contains duplicate or malformed protocol JSON keys"
    index=$((index + 1))
  done
}

GATE_ROOT=
GATE_SHA=
RUN_ID=
RUN_ATTEMPT=
BUILD_ATTEMPT=
SOURCE_REVISION=
RELEASE_TAG=
IMAGE_DIGEST=
APPROVAL_CHALLENGE=
APPROVAL_OUT=
seen_flags=' '

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --gate-root | --gate-sha | --run-id | --run-attempt | --build-attempt | \
      --source-revision | --release-tag | --image-digest | --approval-challenge | \
      --approval-out) ;;
    *) usage ;;
  esac
  [[ -n "$value" ]] || usage
  [[ "$seen_flags" != *" $flag "* ]] || usage
  seen_flags+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --run-id) RUN_ID=$value ;;
    --run-attempt) RUN_ATTEMPT=$value ;;
    --build-attempt) BUILD_ATTEMPT=$value ;;
    --source-revision) SOURCE_REVISION=$value ;;
    --release-tag) RELEASE_TAG=$value ;;
    --image-digest) IMAGE_DIGEST=$value ;;
    --approval-challenge) APPROVAL_CHALLENGE=$value ;;
    --approval-out) APPROVAL_OUT=$value ;;
  esac
done

for required in --gate-root --gate-sha --run-id --run-attempt --build-attempt \
  --source-revision --release-tag --image-digest --approval-challenge --approval-out; do
  [[ "$seen_flags" == *" $required "* ]] || usage
done

[[ "$GATE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "gate SHA is not a full lowercase SHA"
is_positive_decimal_at_most "$RUN_ID" "$U64_MAX" || die "run id is not a canonical positive u64"
is_positive_decimal_at_most "$RUN_ATTEMPT" "$U32_MAX" ||
  die "run attempt is not a canonical positive u32"
is_positive_decimal_at_most "$BUILD_ATTEMPT" "$U32_MAX" ||
  die "build attempt is not a canonical positive u32"
[[ "$BUILD_ATTEMPT" == "$RUN_ATTEMPT" ]] ||
  die "build attempt differs from the current run attempt"
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] ||
  die "source revision is not a full lowercase SHA"
[[ "$RELEASE_TAG" =~ ^build-container-v[1-9][0-9]*$ ]] ||
  die "release tag is not canonical"
[[ "$IMAGE_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || die "image digest is not canonical"
[[ "$APPROVAL_CHALLENGE" =~ ^[0-9a-f]{64}$ ]] ||
  die "approval challenge is not canonical"

for tool in env git jq curl date mktemp stat chmod ln rm; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "release approval gate requires $tool"
done

[[ "$GATE_ROOT" == /* && -d "$GATE_ROOT" && ! -L "$GATE_ROOT" ]] ||
  die "gate root must be an absolute, non-symlink directory"
CANONICAL_GATE_ROOT=$(cd -- "$GATE_ROOT" && pwd -P) || die "cannot resolve gate root"
[[ "$CANONICAL_GATE_ROOT" == "$GATE_ROOT" ]] || die "gate root must already be canonical"
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
REPLACEMENT_REFS=$(gate_git for-each-ref --format='%(refname)' refs/replace/) ||
  die "cannot inspect gate replacement refs"
[[ -z "$REPLACEMENT_REFS" ]] ||
  die "gate checkout cannot contain replacement refs"
[[ "$(gate_git rev-parse --is-shallow-repository 2>/dev/null)" == false ]] ||
  die "gate checkout must contain full history"
[[ "$(gate_git config --bool core.sparseCheckout 2>/dev/null || true)" != true ]] ||
  die "gate checkout cannot be sparse"
GATE_STATUS=$(gate_git status --porcelain=v1 --untracked-files=all) ||
  die "cannot inspect gate checkout status"
[[ -z "$GATE_STATUS" ]] ||
  die "gate checkout must be clean"
[[ "$(gate_git rev-parse --verify HEAD 2>/dev/null)" == "$GATE_SHA" ]] ||
  die "gate checkout HEAD differs from the supplied gate SHA"
if gate_git symbolic-ref -q HEAD >/dev/null 2>&1; then
  die "gate checkout must be detached"
fi

[[ "$APPROVAL_OUT" == /* && "$APPROVAL_OUT" != */ ]] ||
  die "approval output must be an absolute file path"
OUTPUT_NAME=${APPROVAL_OUT##*/}
OUTPUT_PARENT=${APPROVAL_OUT%/*}
[[ -n "$OUTPUT_PARENT" ]] || OUTPUT_PARENT=/
[[ -n "$OUTPUT_NAME" && "$OUTPUT_NAME" != . && "$OUTPUT_NAME" != .. ]] ||
  die "approval output name is invalid"
[[ -d "$OUTPUT_PARENT" && ! -L "$OUTPUT_PARENT" ]] ||
  die "approval output parent must be a non-symlink directory"
CANONICAL_OUTPUT_PARENT=$(cd -- "$OUTPUT_PARENT" && pwd -P) ||
  die "cannot resolve approval output parent"
[[ "$CANONICAL_OUTPUT_PARENT" == "$OUTPUT_PARENT" ]] ||
  die "approval output parent must already be canonical"
if [[ "$OUTPUT_PARENT" == / ]]; then
  EXPECTED_OUTPUT_PATH="/$OUTPUT_NAME"
else
  EXPECTED_OUTPUT_PATH="$OUTPUT_PARENT/$OUTPUT_NAME"
fi
[[ "$EXPECTED_OUTPUT_PATH" == "$APPROVAL_OUT" ]] ||
  die "approval output must be directly below its parent"
[[ ! -e "$APPROVAL_OUT" && ! -L "$APPROVAL_OUT" ]] || die "approval output already exists"

if OUTPUT_MODE=$(env -i PATH="$PATH" LC_ALL=C stat -f '%OLp' -- "$OUTPUT_PARENT" 2>/dev/null); then
  :
elif OUTPUT_MODE=$(env -i PATH="$PATH" LC_ALL=C stat -c '%a' -- "$OUTPUT_PARENT" 2>/dev/null); then
  :
else
  tool_die "cannot inspect approval output directory permissions"
fi
[[ "$OUTPUT_MODE" == 700 ]] || die "approval output parent must have mode 0700"

for repository_test in --is-inside-work-tree --is-inside-git-dir --is-bare-repository; do
  [[ "$(isolated_git -C "$OUTPUT_PARENT" rev-parse "$repository_test" 2>/dev/null || true)" != true ]] ||
    die "approval output parent must be outside every Git repository"
done

# Credential access deliberately follows all local trust, tooling, and output checks.
[[ -n "${GITHUB_TOKEN:-}" ]] || die "GitHub token is absent"
TOKEN=$GITHUB_TOKEN
unset GITHUB_TOKEN
[[ "$TOKEN" != *$'\n'* && "$TOKEN" != *$'\r'* && "$TOKEN" != *'"'* && "$TOKEN" != *\\* ]] ||
  die "GitHub token cannot be encoded safely"

RUN_BODY=
curl_get "current run" "$API_ROOT/$RUN_ID" RUN_BODY
RUN_VALUES=$(jq -er -s '
  if length != 1 or (.[0] | type) != "object" then error("invalid run body")
  else .[0] as $run
    | if (($run.id | type) == "number")
        and (($run.run_attempt | type) == "number")
        and ($run.id == ($run.id | floor))
        and ($run.run_attempt == ($run.run_attempt | floor))
      then [($run.id | tostring), ($run.run_attempt | tostring)] | @tsv
      else error("invalid run identifiers")
      end
  end
' "$RUN_BODY" 2>/dev/null) || die "current run response is not one valid JSON object"
IFS=$'\t' read -r API_RUN_ID API_RUN_ATTEMPT API_EXTRA <<<"$RUN_VALUES"
[[ -z "${API_EXTRA:-}" ]] || die "current run response identifiers are malformed"
is_positive_decimal_at_most "$API_RUN_ID" "$U64_MAX" ||
  die "current run API id is not a positive u64 integer"
is_positive_decimal_at_most "$API_RUN_ATTEMPT" "$U32_MAX" ||
  die "current run API attempt is not a positive u32 integer"
[[ "$API_RUN_ID" == "$RUN_ID" && "$API_RUN_ATTEMPT" == "$RUN_ATTEMPT" ]] ||
  die "current run API identifiers differ from the supplied context"

APPROVALS_BODY=
curl_get "approval history" "$API_ROOT/$RUN_ID/approvals" APPROVALS_BODY
validate_protocol_json_keys "$APPROVALS_BODY"
EVIDENCE=$(jq -er -s \
  --arg prefix "$COMMENT_PREFIX" \
  --arg environment "$RELEASE_ENVIRONMENT" \
  --arg challenge "$APPROVAL_CHALLENGE" \
  --arg digest "$IMAGE_DIGEST" \
  --arg tag "$RELEASE_TAG" \
  --arg attempt "$RUN_ATTEMPT" \
  --arg run_id "$RUN_ID" \
  --arg source "$SOURCE_REVISION" '
  def decimal_at_most($maximum):
    type == "string"
    and test("^[1-9][0-9]*$")
    and ((length < ($maximum | length))
      or (length == ($maximum | length) and . <= $maximum));
  def valid_protocol:
    type == "object"
    and keys == ["challenge","image-digest","png-sha256","release-tag","reviewed-at","run-attempt","run-id","source-revision"]
    and all(.[]; type == "string")
    and (.challenge | test("^[0-9a-f]{64}$"))
    and (."image-digest" | test("^sha256:[0-9a-f]{64}$"))
    and (."png-sha256" | test("^sha256:[0-9a-f]{64}$"))
    and (."release-tag" | test("^build-container-v[1-9][0-9]*$"))
    and (."reviewed-at" | test("^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$"))
    and (."run-attempt" | decimal_at_most("4294967295"))
    and (."run-id" | decimal_at_most("18446744073709551615"))
    and (."source-revision" | test("^[0-9a-f]{40}$"));
  def valid_login:
    type == "string"
    and test("^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$")
    and (contains("--") | not);
  if length != 1 or (.[0] | type) != "array" then error("invalid approval body")
  else
    [.[0][]
      | select(type == "object"
          and (.comment | type) == "string"
          and (.comment | startswith($prefix)))
      | . as $review
      | ($review.comment | ltrimstr($prefix) | fromjson) as $protocol
      | {review:$review, protocol:$protocol}
    ] as $records
    | if all($records[];
        ((.protocol | type) == "object")
        and (.protocol."run-attempt" | decimal_at_most("4294967295")))
      then $records
      else error("unclassifiable protocol record")
      end
    | if all(.[]; ((.protocol."run-attempt" | tonumber) <= ($attempt | tonumber))) then .
      else error("future protocol record")
      end
    | [.[] | select(.protocol."run-attempt" == $attempt)] as $current
    | if ($current | length) != 1 then error("non-unique current record")
      elif ($current[0].protocol | valid_protocol) then $current[0]
      else error("malformed current record")
      end
    | . as $current
    | if $current.protocol."run-id" == $run_id
        and $current.protocol.challenge == $challenge
        and $current.protocol."image-digest" == $digest
        and $current.protocol."release-tag" == $tag
        and $current.protocol."source-revision" == $source
        and $current.review.state == "approved"
        and (($current.review.environments | type) == "array")
        and ($current.review.environments | length) == 1
        and (($current.review.environments[0] | type) == "object")
        and $current.review.environments[0].name == $environment
        and ($current.review.user.login | valid_login)
        and $current.review.comment == ($prefix + ({
          "challenge":$current.protocol.challenge,
          "image-digest":$current.protocol."image-digest",
          "png-sha256":$current.protocol."png-sha256",
          "release-tag":$current.protocol."release-tag",
          "reviewed-at":$current.protocol."reviewed-at",
          "run-attempt":$current.protocol."run-attempt",
          "run-id":$current.protocol."run-id",
          "source-revision":$current.protocol."source-revision"
        } | tojson))
      then ({
        "approval-challenge":$current.protocol.challenge,
        "approver-login":$current.review.user.login,
        "image-digest":$current.protocol."image-digest",
        "release-tag":$current.protocol."release-tag",
        "reviewed-at":$current.protocol."reviewed-at",
        "run-attempt":$current.protocol."run-attempt",
        "run-id":$current.protocol."run-id",
        "schema-version":1,
        "screenshot-sha256":$current.protocol."png-sha256",
        "source-revision":$current.protocol."source-revision"
      } | tojson)
      else error("current approval does not match")
      end
  end
' "$APPROVALS_BODY" 2>/dev/null) || die "approval history does not contain one valid current approval"

REVIEWED_AT=$(jq -er '."reviewed-at"' <<<"$EVIDENCE" 2>/dev/null) ||
  die "approved review time is absent"
REVIEWED_EPOCH=$(jq -nr --arg value "$REVIEWED_AT" '$value | fromdateiso8601' 2>/dev/null) ||
  die "approved review time is not a valid UTC instant"
ROUND_TRIP=$(jq -nr --argjson epoch "$REVIEWED_EPOCH" \
  '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")' 2>/dev/null) ||
  die "approved review time cannot be normalized"
[[ "$ROUND_TRIP" == "$REVIEWED_AT" ]] || die "approved review time is not a real calendar instant"
NOW_EPOCH=$(env -i PATH="$PATH" LC_ALL=C date -u +%s 2>/dev/null) ||
  tool_die "cannot read the current UTC time"
[[ "$REVIEWED_EPOCH" =~ ^[0-9]+$ && "$NOW_EPOCH" =~ ^[0-9]+$ ]] ||
  tool_die "UTC time source returned a malformed epoch"
((REVIEWED_EPOCH <= NOW_EPOCH)) || die "approved review time is in the future"
((NOW_EPOCH - REVIEWED_EPOCH <= 900)) || die "approved review is stale"

OUTPUT_TMP=$(mktemp "$OUTPUT_PARENT/.edgezero-release-approval.XXXXXX" 2>/dev/null) ||
  die "cannot create approval output"
temporary_files+=("$OUTPUT_TMP")
printf '%s' "$EVIDENCE" >"$OUTPUT_TMP" || die "cannot write approval output"
chmod 0600 "$OUTPUT_TMP" || die "cannot secure approval output"
ln -- "$OUTPUT_TMP" "$APPROVAL_OUT" 2>/dev/null || die "approval output appeared before publication"
[[ -f "$APPROVAL_OUT" && ! -L "$APPROVAL_OUT" ]] || die "published approval output is not regular"
