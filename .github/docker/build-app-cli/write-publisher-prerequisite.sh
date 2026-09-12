#!/usr/bin/env bash
# shellcheck disable=SC2016
{ set +x; set +a; } 2>/dev/null
set -euo pipefail

GIT_ALTERNATES_WERE_SET=false
[[ -z "${GIT_ALTERNATE_OBJECT_DIRECTORIES:-}" ]] || GIT_ALTERNATES_WERE_SET=true

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
readonly API=https://api.github.com
readonly OWNER=stackpop
readonly REPOSITORY=stackpop/edgezero
readonly GATE_VARIABLE=EDGEZERO_BUILD_CONTAINER_GATE_SHA
readonly PREREQUISITE_VARIABLE=EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE
readonly U64_MAX=18446744073709551615
readonly U32_MAX=4294967295

usage() {
  printf '%s\n' \
    "usage: write-publisher-prerequisite.sh \\" \
    "  --gate-root <canonical-G-root> \\" \
    "  --gate-sha <G> \\" \
    "  --evidence-json <opaque-audit-attachment> \\" \
    "  --publisher-prerequisite-json <canonical-derived-record> \\" \
    "  --writer-token-review-json <canonical-review> \\" \
    '  --writer-token-review-png <png>' >&2
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

decimal_compare() {
  local left=$1 right=$2
  if ((${#left} < ${#right})) || ((${#left} == ${#right})) && [[ "$left" < "$right" ]]; then
    printf '%s' -1
  elif ((${#left} > ${#right})) || ((${#left} == ${#right})) && [[ "$left" > "$right" ]]; then
    printf '%s' 1
  else
    printf '%s' 0
  fi
}

isolated_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_NO_LAZY_FETCH=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null "$@"
}

gate_git() {
  isolated_git -C "$GATE_ROOT" "$@"
}

clean_jq() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null jq "$@"
}

clean_cmp() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null cmp "$@"
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

file_size() {
  local path=$1 size
  if size=$(env -i PATH="$PATH" LC_ALL=C stat -f '%z' -- "$path" 2>/dev/null); then
    :
  elif size=$(env -i PATH="$PATH" LC_ALL=C stat -c '%s' -- "$path" 2>/dev/null); then
    :
  else
    tool_die "cannot inspect input file size"
  fi
  [[ "$size" =~ ^[0-9]+$ ]] || tool_die "file size tool returned malformed output"
  printf '%s' "$size"
}

validate_input_file() {
  local path=$1 label=$2 minimum=${3:-0} maximum=${4:-0}
  local parent name canonical_parent expected size
  [[ "$path" == /* && -f "$path" && ! -L "$path" ]] ||
    die "$label must be an absolute regular non-symlink file"
  name=${path##*/}
  parent=${path%/*}
  [[ -n "$parent" ]] || parent=/
  canonical_parent=$(cd -- "$parent" && pwd -P) || die "cannot resolve $label parent"
  if [[ "$canonical_parent" == / ]]; then expected="/$name"; else expected="$canonical_parent/$name"; fi
  [[ "$expected" == "$path" ]] || die "$label path must already be canonical"
  if ((minimum > 0 || maximum > 0)); then
    size=$(file_size "$path")
    ((size >= minimum && size <= maximum)) || die "$label size is outside its allowed bounds"
  fi
}

validate_raw_json_keys() {
  local path=$1 label=$2
  clean_jq -ne --stream '
    [inputs | select(length == 2) | (.[0] | tojson)] as $paths
    | ($paths | length) == ($paths | unique | length)
  ' "$path" >/dev/null 2>&1 || die "$label contains duplicate or malformed JSON keys"
}

validate_utc() {
  local value=$1 label=$2 epoch round_trip
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "$label is not canonical UTC"
  epoch=$(clean_jq -nr --arg value "$value" '$value | fromdateiso8601' 2>/dev/null) ||
    die "$label is not a valid UTC instant"
  round_trip=$(clean_jq -nr --argjson epoch "$epoch" '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")' 2>/dev/null) ||
    die "$label cannot be normalized"
  [[ "$round_trip" == "$value" ]] || die "$label is not a real calendar instant"
  printf '%s' "$epoch"
}

sha256_file() {
  local output digest
  output=$(env -i PATH="$PATH" LC_ALL=C sha256sum "$1" 2>/dev/null) ||
    tool_die "cannot hash input file"
  digest=${output%% *}
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || tool_die "SHA-256 tool returned malformed output"
  printf '%s' "$digest"
}

require_exact_string() {
  local path=$1 expected=$2 label=$3 staged
  staged=$(mktemp /tmp/.edgezero-publisher-canonical.XXXXXX 2>/dev/null) ||
    die "cannot stage canonical $label"
  temporary_files+=("$staged")
  printf '%s' "$expected" >"$staged" || die "cannot stage canonical $label"
  clean_cmp -s "$path" "$staged" || die "$label is not exact JCS"
}

extract_variable_value() {
  local body=$1 name=$2 output=$3 label=$4
  clean_jq -e -j -s --arg name "$name" '
    if length == 1 and (.[0] | type) == "object"
      and .[0].name == $name and (.[0].value | type) == "string"
    then .[0].value
    else error("invalid variable response") end
  ' "$body" >"$output" 2>/dev/null || die "$label response is invalid"
}

parse_record() {
  local path=$1 label=$2 canonical
  validate_raw_json_keys "$path" "$label"
  clean_jq -e '
    type == "object"
    and keys == ["evidence-sha256","evidence-url","gate-sha","previous-value-sha256","rotation-history","schema-version","source-pr","source-revision"]
    and (."evidence-sha256" | type) == "string"
    and ((."evidence-url" == null) or ((."evidence-url" | type) == "string"))
    and (."gate-sha" | type) == "string"
    and ((."previous-value-sha256" == null) or ((."previous-value-sha256" | type) == "string"))
    and ((."source-pr" == null) or ((."source-pr" | type) == "string"))
    and ((."source-revision" == null) or ((."source-revision" | type) == "string"))
    and ."schema-version" == 2
    and ((."schema-version" | type) == "number")
    and (."rotation-history" | type) == "object"
    and (
      (."rotation-history" == {"state":"bootstrap-no-rotation"})
      or (
        (."rotation-history" | keys) == ["created-at","evidence-sha256","history-sha256","run-attempt","run-id","run-number","state"]
        and (."rotation-history"."created-at" | type) == "string"
        and (."rotation-history"."evidence-sha256" | type) == "string"
        and (."rotation-history"."history-sha256" | type) == "string"
        and (."rotation-history"."run-attempt" | type) == "string"
        and (."rotation-history"."run-id" | type) == "string"
        and (."rotation-history"."run-number" | type) == "string"
        and ."rotation-history".state == "verified"
      )
    )
  ' "$path" >/dev/null 2>&1 || die "$label has the wrong JSON shape or types"

  REC_EVIDENCE=$(clean_jq -er '."evidence-sha256"' "$path" 2>/dev/null) || die "$label evidence digest is absent"
  REC_EVIDENCE_URL=$(clean_jq -r 'if ."evidence-url" == null then "null" else ."evidence-url" end' "$path")
  REC_GATE=$(clean_jq -er '."gate-sha"' "$path" 2>/dev/null) || die "$label gate SHA is absent"
  REC_PREVIOUS=$(clean_jq -r 'if ."previous-value-sha256" == null then "null" else ."previous-value-sha256" end' "$path")
  REC_SOURCE_PR=$(clean_jq -r 'if ."source-pr" == null then "null" else ."source-pr" end' "$path")
  REC_SOURCE=$(clean_jq -r 'if ."source-revision" == null then "null" else ."source-revision" end' "$path")
  REC_STATE=$(clean_jq -er '."rotation-history".state' "$path" 2>/dev/null) || die "$label rotation state is absent"
  [[ "$REC_EVIDENCE" =~ ^sha256:[0-9a-f]{64}$ ]] || die "$label evidence digest is not canonical"
  [[ "$REC_GATE" =~ ^[0-9a-f]{40}$ ]] || die "$label gate SHA is not canonical"
  [[ "$REC_PREVIOUS" == null || "$REC_PREVIOUS" =~ ^sha256:[0-9a-f]{64}$ ]] ||
    die "$label previous-value digest is not canonical"
  [[ "$REC_SOURCE" == null || "$REC_SOURCE" =~ ^[0-9a-f]{40}$ ]] ||
    die "$label source revision is not canonical"
  if [[ "$REC_SOURCE" == null ]]; then
    [[ "$REC_SOURCE_PR" == null && "$REC_EVIDENCE_URL" == null ]] ||
      die "$label inert source tuple is partial"
  else
    [[ "$REC_SOURCE_PR" != null && "$REC_EVIDENCE_URL" != null ]] ||
      die "$label release source tuple is partial"
    is_positive_decimal_at_most "$REC_SOURCE_PR" "$U64_MAX" ||
      die "$label source PR is not a positive u64"
    [[ "$REC_EVIDENCE_URL" =~ ^https://github\.com/stackpop/edgezero/pull/([1-9][0-9]*)#issuecomment-([1-9][0-9]*)$ ]] ||
      die "$label evidence URL is not canonical"
    [[ "${BASH_REMATCH[1]}" == "$REC_SOURCE_PR" ]] ||
      die "$label evidence URL PR differs from source PR"
    is_positive_decimal_at_most "${BASH_REMATCH[2]}" "$U64_MAX" ||
      die "$label evidence comment id is not a positive u64"
  fi

  if [[ "$REC_STATE" == bootstrap-no-rotation ]]; then
    REC_CREATED=''
    REC_ROTATION_EVIDENCE=''
    REC_HISTORY_EVIDENCE=''
    REC_RUN_ATTEMPT=''
    REC_RUN_ID=''
    REC_RUN_NUMBER=''
    canonical=$(printf '{"evidence-sha256":"%s","evidence-url":%s,"gate-sha":"%s","previous-value-sha256":%s,"rotation-history":{"state":"bootstrap-no-rotation"},"schema-version":2,"source-pr":%s,"source-revision":%s}' \
      "$REC_EVIDENCE" "$([[ "$REC_EVIDENCE_URL" == null ]] && printf null || printf '"%s"' "$REC_EVIDENCE_URL")" \
      "$REC_GATE" "$([[ "$REC_PREVIOUS" == null ]] && printf null || printf '"%s"' "$REC_PREVIOUS")" \
      "$([[ "$REC_SOURCE_PR" == null ]] && printf null || printf '"%s"' "$REC_SOURCE_PR")" \
      "$([[ "$REC_SOURCE" == null ]] && printf null || printf '"%s"' "$REC_SOURCE")")
  else
    REC_CREATED=$(clean_jq -er '."rotation-history"."created-at"' "$path" 2>/dev/null) || die "$label rotation time is absent"
    REC_ROTATION_EVIDENCE=$(clean_jq -er '."rotation-history"."evidence-sha256"' "$path" 2>/dev/null) || die "$label rotation digest is absent"
    REC_HISTORY_EVIDENCE=$(clean_jq -er '."rotation-history"."history-sha256"' "$path" 2>/dev/null) || die "$label rotation history digest is absent"
    REC_RUN_ATTEMPT=$(clean_jq -er '."rotation-history"."run-attempt"' "$path" 2>/dev/null) || die "$label run attempt is absent"
    REC_RUN_ID=$(clean_jq -er '."rotation-history"."run-id"' "$path" 2>/dev/null) || die "$label run id is absent"
    REC_RUN_NUMBER=$(clean_jq -er '."rotation-history"."run-number"' "$path" 2>/dev/null) || die "$label run number is absent"
    validate_utc "$REC_CREATED" "$label rotation creation time" >/dev/null
    [[ "$REC_ROTATION_EVIDENCE" =~ ^sha256:[0-9a-f]{64}$ ]] || die "$label rotation digest is not canonical"
    [[ "$REC_HISTORY_EVIDENCE" =~ ^sha256:[0-9a-f]{64}$ ]] || die "$label rotation history digest is not canonical"
    is_positive_decimal_at_most "$REC_RUN_ATTEMPT" "$U32_MAX" || die "$label run attempt is not a positive u32"
    is_positive_decimal_at_most "$REC_RUN_ID" "$U64_MAX" || die "$label run id is not a positive u64"
    is_positive_decimal_at_most "$REC_RUN_NUMBER" "$U64_MAX" || die "$label run number is not a positive u64"
    canonical=$(printf '{"evidence-sha256":"%s","evidence-url":%s,"gate-sha":"%s","previous-value-sha256":%s,"rotation-history":{"created-at":"%s","evidence-sha256":"%s","history-sha256":"%s","run-attempt":"%s","run-id":"%s","run-number":"%s","state":"verified"},"schema-version":2,"source-pr":%s,"source-revision":%s}' \
      "$REC_EVIDENCE" "$([[ "$REC_EVIDENCE_URL" == null ]] && printf null || printf '"%s"' "$REC_EVIDENCE_URL")" \
      "$REC_GATE" "$([[ "$REC_PREVIOUS" == null ]] && printf null || printf '"%s"' "$REC_PREVIOUS")" \
      "$REC_CREATED" "$REC_ROTATION_EVIDENCE" "$REC_HISTORY_EVIDENCE" "$REC_RUN_ATTEMPT" "$REC_RUN_ID" "$REC_RUN_NUMBER" \
      "$([[ "$REC_SOURCE_PR" == null ]] && printf null || printf '"%s"' "$REC_SOURCE_PR")" \
      "$([[ "$REC_SOURCE" == null ]] && printf null || printf '"%s"' "$REC_SOURCE")")
  fi
  require_exact_string "$path" "$canonical" "$label"
}

GATE_ROOT=
GATE_SHA=
EVIDENCE_JSON=
PREREQUISITE_JSON=
REVIEW_JSON=
REVIEW_PNG=
seen_flags=' '

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --gate-root | --gate-sha | --evidence-json | --publisher-prerequisite-json | \
      --writer-token-review-json | --writer-token-review-png) ;;
    *) usage ;;
  esac
  [[ -n "$value" ]] || usage
  [[ "$seen_flags" != *" $flag "* ]] || usage
  seen_flags+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --evidence-json) EVIDENCE_JSON=$value ;;
    --publisher-prerequisite-json) PREREQUISITE_JSON=$value ;;
    --writer-token-review-json) REVIEW_JSON=$value ;;
    --writer-token-review-png) REVIEW_PNG=$value ;;
  esac
done

for required in --gate-root --gate-sha --evidence-json --publisher-prerequisite-json \
  --writer-token-review-json --writer-token-review-png; do
  [[ "$seen_flags" == *" $required "* ]] || usage
done

for tool in env git jq curl date mktemp stat rm cmp od tr sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "publisher prerequisite writer requires $tool"
done

unset WRITER_TOKEN
[[ -n "${EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN:-}" ]] || die "publisher prerequisite write token is absent"
WRITER_TOKEN=$EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN
unset EDGEZERO_PUBLISHER_PREREQUISITE_WRITE_TOKEN
readonly WRITER_TOKEN
[[ "$WRITER_TOKEN" != *$'\n'* && "$WRITER_TOKEN" != *$'\r'* && "$WRITER_TOKEN" != *'"'* && "$WRITER_TOKEN" != *\\* ]] ||
  die "publisher prerequisite write token cannot be encoded safely"

[[ "$GATE_SHA" =~ ^[0-9a-f]{40}$ ]] || die "gate SHA is not a full lowercase SHA"
[[ "$GATE_ROOT" == /* && -d "$GATE_ROOT" && ! -L "$GATE_ROOT" ]] ||
  die "gate root must be an absolute non-symlink directory"
CANONICAL_GATE_ROOT=$(cd -- "$GATE_ROOT" && pwd -P) || die "cannot resolve gate root"
[[ "$CANONICAL_GATE_ROOT" == "$GATE_ROOT" ]] || die "gate root must already be canonical"
[[ "$GIT_ALTERNATES_WERE_SET" == false ]] || die "gate checkout cannot use environment object alternates"
[[ "$(gate_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] || die "gate root is not a Git worktree"
[[ "$(gate_git rev-parse --show-toplevel 2>/dev/null)" == "$GATE_ROOT" ]] ||
  die "gate root must be the exact repository top level"
GIT_DIRECTORY=$(gate_git rev-parse --absolute-git-dir 2>/dev/null) || die "cannot resolve gate Git directory"
COMMON_GIT_DIRECTORY=$(gate_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
  die "cannot resolve gate common Git directory"
OBJECT_DIRECTORY=$(gate_git rev-parse --path-format=absolute --git-path objects 2>/dev/null) ||
  die "cannot resolve gate object directory"
if gate_git config --local --get-regexp \
  '^(extensions\.partial[Cc]lone|remote\..*\.(promisor|partial[Cc]lone[Ff]ilter))$' >/dev/null 2>&1; then
  die "gate checkout cannot use partial or promisor object storage"
fi
for alternates_file in \
  "$OBJECT_DIRECTORY/info/alternates" \
  "$GIT_DIRECTORY/objects/info/alternates" \
  "$COMMON_GIT_DIRECTORY/objects/info/alternates"; do
  [[ ! -e "$alternates_file" && ! -L "$alternates_file" ]] ||
    die "gate checkout cannot use object alternates"
done
[[ ! -e "$GIT_DIRECTORY/info/grafts" && ! -L "$GIT_DIRECTORY/info/grafts" &&
  ! -e "$COMMON_GIT_DIRECTORY/info/grafts" && ! -L "$COMMON_GIT_DIRECTORY/info/grafts" ]] ||
  die "gate checkout cannot contain legacy grafts"
REPLACEMENT_REFS=$(gate_git for-each-ref --format='%(refname)' refs/replace/) || die "cannot inspect replacement refs"
[[ -z "$REPLACEMENT_REFS" ]] || die "gate checkout cannot contain replacement refs"
[[ "$(gate_git rev-parse --is-shallow-repository 2>/dev/null)" == false ]] || die "gate checkout must contain full history"
[[ "$(gate_git config --bool core.sparseCheckout 2>/dev/null || true)" != true ]] || die "gate checkout cannot be sparse"
GATE_STATUS=$(gate_git status --porcelain=v1 --untracked-files=all --ignore-submodules=none) || die "cannot inspect gate checkout"
[[ -z "$GATE_STATUS" ]] || die "gate checkout must be clean"
[[ "$(gate_git rev-parse --verify HEAD 2>/dev/null)" == "$GATE_SHA" ]] || die "gate checkout HEAD differs from gate SHA"
if gate_git symbolic-ref -q HEAD >/dev/null 2>&1; then die "gate checkout must be detached"; fi

SCRIPT_PATH=${BASH_SOURCE[0]}
[[ "$SCRIPT_PATH" == /* ]] || die "writer must be invoked by its absolute gate path"
EXPECTED_SCRIPT="$GATE_ROOT/.github/docker/build-app-cli/write-publisher-prerequisite.sh"
[[ "$SCRIPT_PATH" == "$EXPECTED_SCRIPT" && -f "$SCRIPT_PATH" && ! -L "$SCRIPT_PATH" ]] ||
  die "writer must execute from the supplied gate revision"

validate_input_file "$EVIDENCE_JSON" "evidence JSON" 1 1048576
validate_input_file "$PREREQUISITE_JSON" "publisher prerequisite JSON"
validate_input_file "$REVIEW_JSON" "writer token review JSON"
validate_input_file "$REVIEW_PNG" "writer token review PNG" 8 10485760

PNG_MAGIC=$(env -i PATH="$PATH" LC_ALL=C od -An -tx1 -N8 -- "$REVIEW_PNG" 2>/dev/null | tr -d '[:space:]') ||
  tool_die "cannot inspect writer token review PNG"
[[ "$PNG_MAGIC" == 89504e470d0a1a0a ]] || die "writer token review PNG signature is invalid"

validate_raw_json_keys "$REVIEW_JSON" "writer token review"
clean_jq -e '
  type == "object"
  and keys == ["expires-at","organization-grants","repository-grants","resource-owner","reviewed-at","reviewer-login","schema-version","screenshot-sha256","selected-repositories","subject-login","token-id"]
  and ."organization-grants" == {"members":"read","other-displayed":"none"}
  and ."repository-grants" == {"metadata":"read","other-displayed":"none","variables":"write"}
  and ."resource-owner" == "stackpop"
  and ."selected-repositories" == ["stackpop/edgezero"]
  and ."schema-version" == 1 and (."schema-version" | type) == "number"
  and (."expires-at" | type) == "string"
  and (."reviewed-at" | type) == "string"
  and (."reviewer-login" | type) == "string"
  and (."screenshot-sha256" | type) == "string"
  and (."subject-login" | type) == "string"
  and (."token-id" | type) == "string"
' "$REVIEW_JSON" >/dev/null 2>&1 || die "writer token review has the wrong JSON shape or grants"

REVIEW_EXPIRES=$(clean_jq -er '."expires-at"' "$REVIEW_JSON")
REVIEW_REVIEWED=$(clean_jq -er '."reviewed-at"' "$REVIEW_JSON")
REVIEW_REVIEWER=$(clean_jq -er '."reviewer-login"' "$REVIEW_JSON")
REVIEW_SCREENSHOT=$(clean_jq -er '."screenshot-sha256"' "$REVIEW_JSON")
REVIEW_SUBJECT=$(clean_jq -er '."subject-login"' "$REVIEW_JSON")
REVIEW_TOKEN_ID=$(clean_jq -er '."token-id"' "$REVIEW_JSON")
for login in "$REVIEW_REVIEWER" "$REVIEW_SUBJECT"; do
  [[ "$login" =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,37}[A-Za-z0-9])?$ && "$login" != *--* ]] ||
    die "writer token review contains an invalid login"
done
[[ "$REVIEW_REVIEWER" != "$REVIEW_SUBJECT" ]] || die "writer token reviewer must differ from subject"
[[ "$REVIEW_SCREENSHOT" =~ ^sha256:[0-9a-f]{64}$ ]] || die "writer token screenshot digest is not canonical"
is_positive_decimal_at_most "$REVIEW_TOKEN_ID" "$U64_MAX" || die "writer token id is not a positive u64"
REVIEW_CANONICAL=$(printf '{"expires-at":"%s","organization-grants":{"members":"read","other-displayed":"none"},"repository-grants":{"metadata":"read","other-displayed":"none","variables":"write"},"resource-owner":"stackpop","reviewed-at":"%s","reviewer-login":"%s","schema-version":1,"screenshot-sha256":"%s","selected-repositories":["stackpop/edgezero"],"subject-login":"%s","token-id":"%s"}' \
  "$REVIEW_EXPIRES" "$REVIEW_REVIEWED" "$REVIEW_REVIEWER" "$REVIEW_SCREENSHOT" "$REVIEW_SUBJECT" "$REVIEW_TOKEN_ID")
require_exact_string "$REVIEW_JSON" "$REVIEW_CANONICAL" "writer token review"

REVIEWED_EPOCH=$(validate_utc "$REVIEW_REVIEWED" "writer token review time")
EXPIRES_EPOCH=$(validate_utc "$REVIEW_EXPIRES" "writer token expiration")
NOW_EPOCH=$(env -i PATH="$PATH" LC_ALL=C date -u +%s 2>/dev/null) || tool_die "cannot read current UTC time"
[[ "$NOW_EPOCH" =~ ^[0-9]+$ ]] || tool_die "UTC time source returned malformed output"
((REVIEWED_EPOCH <= NOW_EPOCH)) || die "writer token review is in the future"
((EXPIRES_EPOCH > NOW_EPOCH)) || die "writer token is expired"
[[ "sha256:$(sha256_file "$REVIEW_PNG")" == "$REVIEW_SCREENSHOT" ]] || die "writer token screenshot digest differs"

parse_record "$PREREQUISITE_JSON" "publisher prerequisite record"
NEW_EVIDENCE=$REC_EVIDENCE
NEW_GATE=$REC_GATE
NEW_PREVIOUS=$REC_PREVIOUS
NEW_SOURCE_PR=$REC_SOURCE_PR
NEW_SOURCE=$REC_SOURCE
NEW_STATE=$REC_STATE
NEW_CREATED=$REC_CREATED
NEW_ROTATION_EVIDENCE=$REC_ROTATION_EVIDENCE
NEW_HISTORY_EVIDENCE=$REC_HISTORY_EVIDENCE
NEW_RUN_ATTEMPT=$REC_RUN_ATTEMPT
NEW_RUN_ID=$REC_RUN_ID
NEW_RUN_NUMBER=$REC_RUN_NUMBER
[[ "$NEW_GATE" == "$GATE_SHA" ]] || die "publisher prerequisite gate differs from supplied gate"
[[ "sha256:$(sha256_file "$EVIDENCE_JSON")" == "$NEW_EVIDENCE" ]] || die "opaque evidence digest differs"
if [[ "$NEW_SOURCE" != null ]]; then
  gate_git cat-file -e "$NEW_SOURCE^{commit}" 2>/dev/null || die "source revision is not a commit"
  gate_git merge-base --is-ancestor "$GATE_SHA" "$NEW_SOURCE" 2>/dev/null || die "source revision does not descend from gate"
fi

curl_get() {
  local label=$1 url=$2 result_name=$3 body metadata line
  local -a lines=()
  body=$(mktemp /tmp/.edgezero-publisher-body.XXXXXX 2>/dev/null) || die "cannot create API response file"
  temporary_files+=("$body")
  metadata=$(mktemp /tmp/.edgezero-publisher-metadata.XXXXXX 2>/dev/null) || die "cannot create API metadata file"
  temporary_files+=("$metadata")
  if ! printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    "header = \"Authorization: Bearer $WRITER_TOKEN\"" |
    env -i PATH="$PATH" LC_ALL=C curl \
      --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
      --request GET --config - --output "$body" \
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' \
      "$url" >"$metadata" 2>/dev/null; then
    die "$label request failed"
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do lines+=("$line"); done <"$metadata"
  [[ "${#lines[@]}" -eq 3 && "${lines[0]}" == 200 ]] || die "$label response did not return HTTP 200"
  [[ "${lines[1]}" == "$API_VERSION" ]] || die "$label selected an unexpected API version"
  [[ "${lines[2]}" =~ ^[Aa][Pp][Pp][Ll][Ii][Cc][Aa][Tt][Ii][Oo][Nn]/[Jj][Ss][Oo][Nn]([[:space:]]*\;[[:space:]]*[Cc][Hh][Aa][Rr][Ss][Ee][Tt][[:space:]]*=[[:space:]]*[Uu][Tt][Ff]-8)?$ ]] ||
    die "$label response has an unsupported media type"
  clean_jq -e -s 'length == 1' "$body" >/dev/null 2>&1 || die "$label response is not one complete JSON value"
  printf -v "$result_name" '%s' "$body"
}

curl_patch() {
  local request=$1 body metadata line
  local -a lines=()
  body=$(mktemp /tmp/.edgezero-publisher-body.XXXXXX 2>/dev/null) || die "cannot create PATCH response file"
  temporary_files+=("$body")
  metadata=$(mktemp /tmp/.edgezero-publisher-metadata.XXXXXX 2>/dev/null) || die "cannot create PATCH metadata file"
  temporary_files+=("$metadata")
  if ! printf '%s\n' \
    'header = "Accept: application/vnd.github+json"' \
    'header = "X-GitHub-Api-Version: 2026-03-10"' \
    'header = "User-Agent: edgezero-build-container-gate/1"' \
    'header = "Content-Type: application/json"' \
    "header = \"Authorization: Bearer $WRITER_TOKEN\"" |
    env -i PATH="$PATH" LC_ALL=C curl \
      --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
      --request PATCH --config - --data-binary "@$request" --output "$body" \
      --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}' \
      "$API/repos/$REPOSITORY/actions/variables/$PREREQUISITE_VARIABLE" >"$metadata" 2>/dev/null; then
    die "publisher prerequisite PATCH failed"
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do lines+=("$line"); done <"$metadata"
  [[ ("${#lines[@]}" -eq 2 || "${#lines[@]}" -eq 3) && "${lines[0]}" == 204 ]] ||
    die "publisher prerequisite PATCH did not return HTTP 204"
  [[ "${lines[1]}" == "$API_VERSION" ]] || die "publisher prerequisite PATCH selected an unexpected API version"
  [[ ! -s "$body" ]] || die "publisher prerequisite PATCH returned a nonempty body"
}

USER_BODY=
curl_get "authenticated user" "$API/user" USER_BODY
USER_VALUES=$(clean_jq -er -s '
  if length == 1 and (.[0] | type) == "object"
    and (.[0].login | type) == "string" and (.[0].id | type) == "number"
  then [.[0].login, (.[0].id | tostring)] | @tsv
  else error("invalid user") end
' "$USER_BODY" 2>/dev/null) || die "authenticated user response is invalid"
IFS=$'\t' read -r USER_LOGIN USER_ID USER_EXTRA <<<"$USER_VALUES"
[[ -z "${USER_EXTRA:-}" && "$USER_LOGIN" == "$REVIEW_SUBJECT" ]] || die "authenticated user login differs from review"
is_positive_decimal_at_most "$USER_ID" "$U64_MAX" || die "authenticated user id is not a positive u64"

MEMBERSHIP_BODY=
curl_get "organization membership" "$API/orgs/$OWNER/memberships/$REVIEW_SUBJECT" MEMBERSHIP_BODY
clean_jq -e -s --arg login "$REVIEW_SUBJECT" '
  length == 1 and (.[0] | type) == "object"
  and .[0].state == "active" and .[0].user.login == $login
' "$MEMBERSHIP_BODY" >/dev/null 2>&1 || die "organization membership is not active for reviewed subject"

GATE_BODY=
curl_get "active gate variable" "$API/repos/$REPOSITORY/actions/variables/$GATE_VARIABLE" GATE_BODY
clean_jq -e -s --arg name "$GATE_VARIABLE" --arg value "$GATE_SHA" '
  length == 1 and (.[0] | type) == "object"
  and .[0].name == $name and .[0].value == $value
' "$GATE_BODY" >/dev/null 2>&1 || die "active gate variable differs from supplied gate"

CURRENT_BODY=
curl_get "current publisher prerequisite" "$API/repos/$REPOSITORY/actions/variables/$PREREQUISITE_VARIABLE" CURRENT_BODY
CURRENT_FILE=$(mktemp /tmp/.edgezero-publisher-current.XXXXXX 2>/dev/null) || die "cannot create current state file"
temporary_files+=("$CURRENT_FILE")
extract_variable_value "$CURRENT_BODY" "$PREREQUISITE_VARIABLE" "$CURRENT_FILE" \
  "current publisher prerequisite"
parse_record "$CURRENT_FILE" "current publisher prerequisite"
OLD_GATE=$REC_GATE
OLD_SOURCE_PR=$REC_SOURCE_PR
OLD_SOURCE=$REC_SOURCE
OLD_STATE=$REC_STATE
OLD_CREATED=$REC_CREATED
OLD_ROTATION_EVIDENCE=$REC_ROTATION_EVIDENCE
OLD_HISTORY_EVIDENCE=$REC_HISTORY_EVIDENCE
OLD_RUN_ATTEMPT=$REC_RUN_ATTEMPT
OLD_RUN_ID=$REC_RUN_ID
OLD_RUN_NUMBER=$REC_RUN_NUMBER

gate_git cat-file -e "$OLD_GATE^{commit}" 2>/dev/null || die "current record gate is not a commit"
if [[ "$OLD_SOURCE" != null ]]; then
  gate_git cat-file -e "$OLD_SOURCE^{commit}" 2>/dev/null || die "current source revision is not a commit"
  gate_git merge-base --is-ancestor "$OLD_GATE" "$OLD_SOURCE" 2>/dev/null || die "current source does not descend from its gate"
fi

if clean_cmp -s "$PREREQUISITE_JSON" "$CURRENT_FILE"; then
  exit 0
fi

[[ "$NEW_PREVIOUS" == "sha256:$(sha256_file "$CURRENT_FILE")" ]] || die "previous-value digest differs from current bytes"

HISTORY_EQUAL=false
if [[ "$OLD_STATE" == bootstrap-no-rotation && "$NEW_STATE" == bootstrap-no-rotation ]]; then
  HISTORY_EQUAL=true
elif [[ "$OLD_STATE" == verified && "$NEW_STATE" == verified &&
  "$OLD_CREATED" == "$NEW_CREATED" &&
  "$OLD_ROTATION_EVIDENCE" == "$NEW_ROTATION_EVIDENCE" &&
  "$OLD_HISTORY_EVIDENCE" == "$NEW_HISTORY_EVIDENCE" &&
  "$OLD_RUN_ATTEMPT" == "$NEW_RUN_ATTEMPT" &&
  "$OLD_RUN_ID" == "$NEW_RUN_ID" &&
  "$OLD_RUN_NUMBER" == "$NEW_RUN_NUMBER" ]]; then
  HISTORY_EQUAL=true
fi

ROTATION_ADVANCED=false
if [[ "$OLD_STATE" == bootstrap-no-rotation ]]; then
  if [[ "$NEW_STATE" == verified ]]; then ROTATION_ADVANCED=true; fi
elif [[ "$NEW_STATE" == bootstrap-no-rotation ]]; then
  die "bootstrap rotation history cannot replace verified history"
elif [[ "$HISTORY_EQUAL" != true ]]; then
  RUN_NUMBER_ORDER=$(decimal_compare "$OLD_RUN_NUMBER" "$NEW_RUN_NUMBER")
  [[ "$RUN_NUMBER_ORDER" == -1 ]] || die "selected rotation run number did not advance"
  [[ "$NEW_RUN_ID" != "$OLD_RUN_ID" ]] || die "forward rotation reused its run id"
  [[ "$NEW_ROTATION_EVIDENCE" != "$OLD_ROTATION_EVIDENCE" ]] || die "forward rotation reused its receipt digest"
  [[ "$NEW_HISTORY_EVIDENCE" != "$OLD_HISTORY_EVIDENCE" ]] || die "forward rotation reused its history digest"
  ROTATION_ADVANCED=true
fi

if [[ "$OLD_GATE" != "$NEW_GATE" ]]; then
  [[ "$NEW_SOURCE" == null && "$NEW_STATE" == verified && "$ROTATION_ADVANCED" == true ]] ||
    die "gate change requires inert forward verified rotation state"
elif [[ "$OLD_SOURCE" == null && "$NEW_SOURCE" == null ]]; then
  [[ "$NEW_STATE" == verified && "$ROTATION_ADVANCED" == true ]] ||
    die "inert refresh requires a forward verified rotation"
elif [[ "$OLD_SOURCE" != null && "$NEW_SOURCE" == null ]]; then
  [[ "$NEW_STATE" == verified && "$ROTATION_ADVANCED" == true ]] ||
    die "same-gate source clear requires a forward verified rotation"
elif [[ "$OLD_SOURCE" == null && "$NEW_SOURCE" != null ]]; then
  [[ "$HISTORY_EQUAL" == true ]] || die "source binding cannot change rotation history"
else
  [[ "$HISTORY_EQUAL" == true ]] || die "source transition cannot change rotation history"
  if [[ "$OLD_SOURCE" == "$NEW_SOURCE" ]]; then
    [[ "$OLD_SOURCE_PR" == "$NEW_SOURCE_PR" ]] || die "same-source transition cannot change source PR"
  fi
  if [[ "$OLD_SOURCE" != "$NEW_SOURCE" ]]; then
    gate_git merge-base --is-ancestor "$OLD_SOURCE" "$NEW_SOURCE" 2>/dev/null ||
      die "source transition is not forward"
  fi
fi

PATCH_FILE=$(mktemp /tmp/.edgezero-publisher-request.XXXXXX 2>/dev/null) || die "cannot create PATCH request"
temporary_files+=("$PATCH_FILE")
clean_jq -cjn --arg name "$PREREQUISITE_VARIABLE" --rawfile value "$PREREQUISITE_JSON" \
  '{name:$name,value:$value}' >"$PATCH_FILE" || die "cannot construct PATCH request"
curl_patch "$PATCH_FILE"

READBACK_BODY=
curl_get "publisher prerequisite readback" "$API/repos/$REPOSITORY/actions/variables/$PREREQUISITE_VARIABLE" READBACK_BODY
READBACK_FILE=$(mktemp /tmp/.edgezero-publisher-readback.XXXXXX 2>/dev/null) || die "cannot create readback state file"
temporary_files+=("$READBACK_FILE")
extract_variable_value "$READBACK_BODY" "$PREREQUISITE_VARIABLE" "$READBACK_FILE" \
  "publisher prerequisite readback"
clean_cmp -s "$READBACK_FILE" "$PREREQUISITE_JSON" ||
  die "publisher prerequisite readback differs from requested bytes"
