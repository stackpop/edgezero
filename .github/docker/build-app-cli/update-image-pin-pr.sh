#!/usr/bin/env bash
set +x
set +a
set -euo pipefail

if [[ ${GIT_ALTERNATE_OBJECT_DIRECTORIES+x} == x ]]; then
  inherited_git_alternates=true
else
  inherited_git_alternates=false
fi
export -n EDGEZERO_BUILD_CONTAINER_APP_TOKEN 2>/dev/null || true
unset GITHUB_TOKEN GH_TOKEN TOKEN APP_TOKEN APP_TOKEN_LOCAL

export LC_ALL=C
export BASH_ENV=
export ENV=
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_NO_LAZY_FETCH=1
export GIT_NO_REPLACE_OBJECTS=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CEILING_DIRECTORIES
unset GIT_REPLACE_REF_BASE

readonly API_VERSION=2026-03-10
readonly API_BASE=https://api.github.com
readonly REPOSITORY=stackpop/edgezero
readonly REMOTE_URL=https://github.com/stackpop/edgezero.git
readonly IMAGE_REPOSITORY=ghcr.io/stackpop/edgezero-build-app-cli
readonly PIN_PREFIX=edgezero-build-container-pin/
readonly TITLE_PREFIX='chore(actions): pin build container for '
readonly BODY_PREFIX='edgezero-build-container-pin-v1 '
readonly U64_MAX=18446744073709551615

usage() {
  printf '%s\n' \
    'usage: update-image-pin-pr.sh' \
    '  --gate-root <canonical-G-root>' \
    '  --gate-sha <G>' \
    '  --repository-root <clean-full-S-repository>' \
    '  --source-revision <S>' \
    '  --release-tag <tag>' \
    '  --image-digest <D>' \
    '  --provenance-protocol 1' \
    '  --approval-json <gate-produced-file>' \
    '  --source-pr <canonical-positive-u64>' \
    '  --evidence-url <exact-PR-comment-URL>' \
    '  --expected-bot-id <canonical-positive-u64>' \
    '  --expected-bot-login <login>' >&2
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

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ ]]
}

is_digest() {
  [[ "$1" =~ ^sha256:[0-9a-f]{64}$ && "$1" != "sha256:$(printf '0%.0s' {1..64})" ]]
}

is_app_bot_login() {
  local login=$1 slug
  [[ "$login" =~ ^([a-z0-9]([a-z0-9-]{0,98}[a-z0-9])?)\[bot\]$ ]] || return 1
  slug=${BASH_REMATCH[1]}
  [[ "$slug" != *--* ]]
}

is_beneath() {
  local path=$1 root=$2
  [[ "$path" == "$root" || "$path" == "$root/"* ]]
}

isolated_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_LAZY_FETCH=1 GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null "$@"
}

repo_git() {
  local root=$1
  shift
  isolated_git -C "$root" "$@"
}

gate_git() {
  repo_git "$GATE_ROOT" "$@"
}

source_git() {
  repo_git "$REPOSITORY_ROOT" "$@"
}

clone_git() {
  repo_git "$CLONE_ROOT" "$@"
}

worktree_git() {
  repo_git "$WORKTREE_ROOT" "$@"
}

temporary_root=
cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  if [[ -n "$temporary_root" ]]; then
    rm -rf -- "$temporary_root" 2>/dev/null || true
  fi
  exit "$status"
}

signal_cleanup() {
  local status=$1
  trap - EXIT HUP INT TERM
  [[ -z "$temporary_root" ]] || rm -rf -- "$temporary_root" 2>/dev/null || true
  exit "$status"
}

trap cleanup EXIT
trap 'signal_cleanup 129' HUP
trap 'signal_cleanup 130' INT
trap 'signal_cleanup 143' TERM

require_checkout() {
  local root=$1 expected=$2 label=$3 common_result_name=$4 object_result_name=$5
  local top git_directory common_directory object_directory object_path
  local replacements partial_configuration shallow actual sparse status gitlinks config_status
  local index_flags index_entry

  [[ "$root" == /* && -d "$root" && ! -L "$root" ]] ||
    die "$label must be an absolute non-symlink directory"
  top=$(cd -- "$root" 2>/dev/null && pwd -P) || die "cannot resolve $label"
  [[ "$top" == "$root" ]] || die "$label must already be canonical"
  [[ "$(repo_git "$root" rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] ||
    die "$label is not a Git worktree"
  [[ "$(repo_git "$root" rev-parse --show-toplevel 2>/dev/null)" == "$root" ]] ||
    die "$label must be the exact repository top level"
  git_directory=$(repo_git "$root" rev-parse --absolute-git-dir 2>/dev/null) ||
    die "cannot resolve $label Git directory"
  common_directory=$(repo_git "$root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null) ||
    die "cannot resolve $label common Git directory"
  object_path=$(repo_git "$root" rev-parse --path-format=absolute --git-path objects 2>/dev/null) ||
    die "cannot resolve $label object directory"
  [[ -d "$object_path" && ! -L "$object_path" ]] || die "$label object directory is not regular"
  object_directory=$(cd -- "$object_path" 2>/dev/null && pwd -P) ||
    die "cannot canonicalize $label object directory"
  [[ "$object_directory" == "$object_path" ]] || die "$label object directory is not canonical"
  [[ ! -e "$git_directory/info/grafts" && ! -L "$git_directory/info/grafts" &&
    ! -e "$common_directory/info/grafts" && ! -L "$common_directory/info/grafts" ]] ||
    die "$label cannot contain legacy grafts"
  [[ ! -e "$object_directory/info/alternates" && ! -L "$object_directory/info/alternates" ]] ||
    die "$label cannot contain object alternates"
  replacements=$(repo_git "$root" for-each-ref --format='%(refname)' refs/replace/ 2>/dev/null) ||
    die "cannot inspect $label replacement refs"
  [[ -z "$replacements" ]] || die "$label cannot contain replacement refs"
  config_status=0
  partial_configuration=$(repo_git "$root" config --includes --name-only --get-regexp \
    '^(extensions\.partial[Cc]lone|remote\..*\.promisor|remote\..*\.partial[Cc]lone[Ff]ilter)$' \
    2>/dev/null) || config_status=$?
  ((config_status == 0 || config_status == 1)) || die "cannot inspect $label partial-clone configuration"
  [[ -z "$partial_configuration" ]] || die "$label cannot contain promisor or partial-clone configuration"
  shallow=$(repo_git "$root" rev-parse --is-shallow-repository 2>/dev/null) ||
    die "cannot inspect $label history depth"
  [[ "$shallow" == false ]] || die "$label must be a full checkout"
  actual=$(repo_git "$root" rev-parse --verify HEAD 2>/dev/null) || die "$label HEAD is absent"
  [[ "$actual" == "$expected" ]] || die "$label HEAD differs from its supplied SHA"
  if repo_git "$root" symbolic-ref -q HEAD >/dev/null 2>&1; then
    die "$label must be detached"
  fi
  sparse=$(repo_git "$root" config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die "$label cannot be sparse"
  status=$(repo_git "$root" status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    die "cannot inspect $label status"
  [[ -z "$status" ]] || die "$label must be clean"
  index_flags=$(repo_git "$root" ls-files -v) || die "cannot inspect $label index flags"
  while IFS= read -r index_entry || [[ -n "$index_entry" ]]; do
    [[ -z "$index_entry" || "$index_entry" == 'H '* ]] ||
      die "$label cannot contain assume-unchanged or skip-worktree entries"
  done <<<"$index_flags"
  gitlinks=$(repo_git "$root" ls-tree -r "$expected" 2>/dev/null | awk '$1 == "160000" { print; exit }') ||
    die "cannot inspect $label submodule state"
  [[ -z "$gitlinks" ]] || die "$label cannot contain submodule state"
  printf -v "$common_result_name" '%s' "$common_directory"
  printf -v "$object_result_name" '%s' "$object_directory"
}

require_input_file() {
  local path=$1 label=$2 parent canonical expected size
  [[ "$path" == /* && -f "$path" && ! -L "$path" ]] ||
    die "$label must be an absolute regular non-symlink file"
  parent=${path%/*}
  [[ -n "$parent" ]] || parent=/
  canonical=$(cd -- "$parent" 2>/dev/null && pwd -P) || die "cannot resolve $label parent"
  [[ "$canonical" == "$parent" ]] || die "$label path must already be canonical"
  if [[ "$parent" == / ]]; then expected="/${path##*/}"; else expected="$parent/${path##*/}"; fi
  [[ "$expected" == "$path" ]] || die "$label path must already be canonical"
  ! is_beneath "$path" "$GATE_ROOT" || die "$label must be outside the gate repository"
  ! is_beneath "$path" "$REPOSITORY_ROOT" || die "$label must be outside the source repository"
  size=$(wc -c <"$path" | tr -d '[:space:]') || die "cannot measure $label"
  [[ "$size" =~ ^[0-9]+$ ]] || die "cannot measure $label"
  ((size > 0 && size <= 4096)) || die "$label must contain 1..4096 bytes"
}

GATE_ROOT=
GATE_SHA=
REPOSITORY_ROOT=
SOURCE_REVISION=
RELEASE_TAG=
IMAGE_DIGEST=
PROVENANCE_PROTOCOL=
APPROVAL_JSON=
SOURCE_PR=
EVIDENCE_URL=
EXPECTED_BOT_ID=
EXPECTED_BOT_LOGIN=
seen_flags=' '

while (($#)); do
  (($# >= 2)) || usage
  flag=$1
  value=$2
  shift 2
  case "$flag" in
    --gate-root | --gate-sha | --repository-root | --source-revision | --release-tag | \
      --image-digest | --provenance-protocol | --approval-json | --source-pr | \
      --evidence-url | --expected-bot-id | --expected-bot-login) ;;
    *) usage ;;
  esac
  [[ -n "$value" ]] || usage
  [[ "$seen_flags" != *" $flag "* ]] || usage
  seen_flags+="$flag "
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;;
    --gate-sha) GATE_SHA=$value ;;
    --repository-root) REPOSITORY_ROOT=$value ;;
    --source-revision) SOURCE_REVISION=$value ;;
    --release-tag) RELEASE_TAG=$value ;;
    --image-digest) IMAGE_DIGEST=$value ;;
    --provenance-protocol) PROVENANCE_PROTOCOL=$value ;;
    --approval-json) APPROVAL_JSON=$value ;;
    --source-pr) SOURCE_PR=$value ;;
    --evidence-url) EVIDENCE_URL=$value ;;
    --expected-bot-id) EXPECTED_BOT_ID=$value ;;
    --expected-bot-login) EXPECTED_BOT_LOGIN=$value ;;
  esac
done

for required in --gate-root --gate-sha --repository-root --source-revision --release-tag \
  --image-digest --provenance-protocol --approval-json --source-pr --evidence-url \
  --expected-bot-id --expected-bot-login; do
  [[ "$seen_flags" == *" $required "* ]] || usage
done

is_sha "$GATE_SHA" || die "gate SHA is not a full lowercase SHA"
is_sha "$SOURCE_REVISION" || die "source revision is not a full lowercase SHA"
[[ "$RELEASE_TAG" =~ ^build-container-v[1-9][0-9]*$ ]] || die "release tag is not canonical"
is_digest "$IMAGE_DIGEST" || die "image digest is not canonical"
[[ "$PROVENANCE_PROTOCOL" == 1 ]] || die "provenance protocol must be the exact integer spelling 1"
is_positive_decimal_at_most "$SOURCE_PR" "$U64_MAX" || die "source PR is not a canonical positive u64"
is_positive_decimal_at_most "$EXPECTED_BOT_ID" "$U64_MAX" ||
  die "expected bot id is not a canonical positive u64"
is_app_bot_login "$EXPECTED_BOT_LOGIN" || die "expected bot login is not canonical"
EXPECTED_BOT_LOGIN_URL="${EXPECTED_BOT_LOGIN%\[bot\]}%5Bbot%5D"
readonly EXPECTED_BOT_LOGIN_URL
if [[ "$EVIDENCE_URL" =~ ^https://github\.com/stackpop/edgezero/pull/([1-9][0-9]*)#issuecomment-([1-9][0-9]*)$ ]]; then
  EVIDENCE_SOURCE_PR=${BASH_REMATCH[1]}
  EVIDENCE_COMMENT_ID=${BASH_REMATCH[2]}
else
  die "evidence URL is not canonical"
fi
is_positive_decimal_at_most "$EVIDENCE_SOURCE_PR" "$U64_MAX" || die "evidence URL source PR is invalid"
is_positive_decimal_at_most "$EVIDENCE_COMMENT_ID" "$U64_MAX" || die "evidence URL comment id is invalid"
[[ "$EVIDENCE_SOURCE_PR" == "$SOURCE_PR" ]] || die "evidence URL source PR differs"
[[ "$inherited_git_alternates" == false ]] || die "environment-provided Git object alternates are forbidden"

for tool in bash env git jq curl mktemp stat chmod rm cmp install wc tr sed awk sort uniq mkdir ln \
  cat cp dirname basename; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "image pin updater requires $tool"
done

require_checkout "$GATE_ROOT" "$GATE_SHA" "gate checkout" GATE_COMMON_DIRECTORY GATE_OBJECT_DIRECTORY
require_checkout "$REPOSITORY_ROOT" "$SOURCE_REVISION" "source checkout" \
  SOURCE_COMMON_DIRECTORY SOURCE_OBJECT_DIRECTORY
[[ "$GATE_ROOT" != "$REPOSITORY_ROOT" && "$GATE_COMMON_DIRECTORY" != "$SOURCE_COMMON_DIRECTORY" &&
  "$GATE_OBJECT_DIRECTORY" != "$SOURCE_OBJECT_DIRECTORY" ]] ||
  die "gate and source checkouts must use separate repositories"
source_git cat-file -e "$GATE_SHA^{commit}" 2>/dev/null || die "source checkout does not contain gate commit"
source_git merge-base --is-ancestor "$GATE_SHA" "$SOURCE_REVISION" 2>/dev/null ||
  die "source revision does not descend from the gate"

SCRIPT_PATH=${BASH_SOURCE[0]}
[[ "$SCRIPT_PATH" == /* ]] || die "updater must be invoked by its absolute gate path"
EXPECTED_SCRIPT="$GATE_ROOT/.github/docker/build-app-cli/update-image-pin-pr.sh"
[[ "$SCRIPT_PATH" == "$EXPECTED_SCRIPT" && -f "$SCRIPT_PATH" && ! -L "$SCRIPT_PATH" ]] ||
  die "updater must execute from the supplied gate revision"
CHECK="$GATE_ROOT/.github/docker/build-app-cli/check-image-pin.sh"
WRITER="$GATE_ROOT/.github/docker/build-app-cli/write-image-release-record.sh"
for helper in "$CHECK" "$WRITER"; do
  [[ -f "$helper" && ! -L "$helper" ]] || die "required gate helper is absent"
done
require_input_file "$APPROVAL_JSON" "approval JSON"

temporary_root=$(mktemp -d /tmp/edgezero-image-pin.XXXXXX 2>/dev/null) ||
  die "cannot create updater temporary directory"
temporary_root=$(cd -- "$temporary_root" && pwd -P) || die "cannot resolve updater temporary directory"
chmod 0700 "$temporary_root" || die "cannot secure updater temporary directory"
CLONE_ROOT="$temporary_root/clone"
WORKTREE_ROOT="$temporary_root/worktree"
RECORD_ROOT="$temporary_root/records"
API_WORK="$temporary_root/api"
mkdir -m 0700 "$RECORD_ROOT" "$API_WORK" || die "cannot create private updater directories"

# Validate the caller's gate-produced approval with the gate's protocol owner before credentials.
VALIDATION_IMAGE="$temporary_root/approval-image.json"
jq -cnS --arg repository "$IMAGE_REPOSITORY" --arg tag "$RELEASE_TAG" \
  --arg digest "$IMAGE_DIGEST" --arg source "$SOURCE_REVISION" \
  --arg protocol "$PROVENANCE_PROTOCOL" \
  '{digest:$digest,"image-source-revision":$source,"provenance-protocol":($protocol|tonumber),repository:$repository,tag:$tag}' \
  >"$VALIDATION_IMAGE" 2>/dev/null || die "cannot construct approval validation record"
chmod 0600 "$VALIDATION_IMAGE" || die "cannot secure approval validation record"
bash "$CHECK" validate-pair "$VALIDATION_IMAGE" "$APPROVAL_JSON" >/dev/null 2>&1 ||
  die "approval JSON is not the exact matching gate record"

APPROVAL_CHALLENGE=$(jq -er '."approval-challenge"' "$APPROVAL_JSON") || die "approval challenge is absent"
APPROVER_LOGIN=$(jq -er '."approver-login"' "$APPROVAL_JSON") || die "approval login is absent"
REVIEWED_AT=$(jq -er '."reviewed-at"' "$APPROVAL_JSON") || die "approval review time is absent"
RUN_ATTEMPT=$(jq -er '."run-attempt"' "$APPROVAL_JSON") || die "approval run attempt is absent"
RUN_ID=$(jq -er '."run-id"' "$APPROVAL_JSON") || die "approval run id is absent"
SCREENSHOT_SHA256=$(jq -er '."screenshot-sha256"' "$APPROVAL_JSON") || die "approval screenshot digest is absent"
IMAGE_OUTPUT="$RECORD_ROOT/image.json"
EVIDENCE_OUTPUT="$RECORD_ROOT/image-release-evidence.json"
bash "$WRITER" \
  --image-path "$IMAGE_OUTPUT" \
  --evidence-path "$EVIDENCE_OUTPUT" \
  --repository "$IMAGE_REPOSITORY" \
  --release-tag "$RELEASE_TAG" \
  --image-digest "$IMAGE_DIGEST" \
  --source-revision "$SOURCE_REVISION" \
  --provenance-protocol "$PROVENANCE_PROTOCOL" \
  --approval-challenge "$APPROVAL_CHALLENGE" \
  --approver-login "$APPROVER_LOGIN" \
  --reviewed-at "$REVIEWED_AT" \
  --run-attempt "$RUN_ATTEMPT" \
  --run-id "$RUN_ID" \
  --screenshot-sha256 "$SCREENSHOT_SHA256" >/dev/null ||
  die "typed image release record writer failed"
cmp -s "$EVIDENCE_OUTPUT" "$APPROVAL_JSON" || die "typed evidence differs from gate approval"

# Read the de-exported credential only after every CLI and precredential validation has succeeded.
APP_TOKEN_LOCAL=${EDGEZERO_BUILD_CONTAINER_APP_TOKEN:-}
unset EDGEZERO_BUILD_CONTAINER_APP_TOKEN
readonly APP_TOKEN_LOCAL
[[ -n "$APP_TOKEN_LOCAL" ]] || die "build-container App token is absent"
[[ "$APP_TOKEN_LOCAL" != *$'\n'* && "$APP_TOKEN_LOCAL" != *$'\r'* &&
  "$APP_TOKEN_LOCAL" != *'"'* && "$APP_TOKEN_LOCAL" != *\\* ]] ||
  die "build-container App token is malformed"

TOKEN_FILE="$temporary_root/token"
ASKPASS="$temporary_root/askpass"
printf '%s' "$APP_TOKEN_LOCAL" >"$TOKEN_FILE" || die "cannot create private Git credential"
chmod 0600 "$TOKEN_FILE" || die "cannot secure private Git credential"
cat >"$ASKPASS" <<EOF
#!/usr/bin/env bash
case "\${1:-}" in
  *Username*) printf '%s\\n' x-access-token ;;
  *Password*) cat '$TOKEN_FILE' ;;
  *) exit 1 ;;
esac
EOF
chmod 0700 "$ASKPASS" || die "cannot secure Git askpass helper"

authenticated_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_LAZY_FETCH=1 GIT_NO_REPLACE_OBJECTS=1 GIT_OPTIONAL_LOCKS=0 GIT_ASKPASS="$ASKPASS" \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null "$@"
}

api_request() {
  local method=$1 url=$2 request_body=$3 result_name=$4 expected_status body metadata line
  local list_prefix pull_prefix route_value route_allowed=false
  local -a lines=()
  case "$method" in GET) expected_status=200 ;; POST) expected_status=201 ;; PATCH) expected_status=200 ;; *) die "internal REST method is not allowlisted" ;; esac
  list_prefix="$API_BASE/repos/$REPOSITORY/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page="
  pull_prefix="$API_BASE/repos/$REPOSITORY/pulls/"
  case "$method $url" in
    "GET $API_BASE/users/$EXPECTED_BOT_LOGIN_URL" | "GET $API_BASE/repos/$REPOSITORY" | \
      "POST $API_BASE/repos/$REPOSITORY/pulls") route_allowed=true ;;
  esac
  if [[ "$method" == GET && "$url" == "$list_prefix"* ]]; then
    route_value=${url#"$list_prefix"}
    if [[ "$route_value" =~ ^([1-9]|[1-9][0-9]|100)$ ]]; then route_allowed=true; fi
  elif [[ ("$method" == GET || "$method" == PATCH) && "$url" == "$pull_prefix"* ]]; then
    route_value=${url#"$pull_prefix"}
    if is_positive_decimal_at_most "$route_value" "$U64_MAX"; then route_allowed=true; fi
  fi
  [[ "$route_allowed" == true ]] || die "internal REST route is not allowlisted"
  [[ "$url" != *$'\n'* && "$url" != *$'\r'* && "$url" != *'#'* ]] || die "internal REST URL is malformed"
  body=$(mktemp "$API_WORK/body.XXXXXX" 2>/dev/null) || die "cannot create API body file"
  metadata=$(mktemp "$API_WORK/metadata.XXXXXX" 2>/dev/null) || die "cannot create API metadata file"
  if [[ "$method" == GET ]]; then
    if ! printf '%s\n' \
      'header = "Accept: application/vnd.github+json"' \
      'header = "X-GitHub-Api-Version: 2026-03-10"' \
      'header = "User-Agent: edgezero-build-container-gate/1"' \
      "header = \"Authorization: Bearer $APP_TOKEN_LOCAL\"" |
      env -i PATH="$PATH" LC_ALL=C curl \
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
        --request "$method" --config - --output "$body" \
        --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\n%header{link}' \
        "$url" >"$metadata" 2>/dev/null; then
      die "GitHub REST request failed"
    fi
  else
    [[ -f "$request_body" && ! -L "$request_body" ]] || die "internal REST request body is absent"
    if ! printf '%s\n' \
      'header = "Accept: application/vnd.github+json"' \
      'header = "X-GitHub-Api-Version: 2026-03-10"' \
      'header = "User-Agent: edgezero-build-container-gate/1"' \
      "header = \"Authorization: Bearer $APP_TOKEN_LOCAL\"" \
      'header = "Content-Type: application/json"' |
      env -i PATH="$PATH" LC_ALL=C curl \
        --disable --silent --show-error --connect-timeout 10 --max-time 30 --max-redirs 0 \
        --request "$method" --config - --data-binary "@$request_body" --output "$body" \
        --write-out '%{http_code}\n%header{x-github-api-version-selected}\n%header{content-type}\n%header{link}' \
        "$url" >"$metadata" 2>/dev/null; then
      die "GitHub REST request failed"
    fi
  fi
  while IFS= read -r line || [[ -n "$line" ]]; do lines+=("$line"); done <"$metadata"
  [[ "${#lines[@]}" -eq 3 || "${#lines[@]}" -eq 4 ]] ||
    die "GitHub REST response metadata is malformed"
  [[ "${lines[0]}" == "$expected_status" ]] || die "GitHub REST response has an unexpected status"
  [[ "${lines[1]}" == "$API_VERSION" ]] || die "GitHub REST response selected an unexpected API version"
  [[ "${lines[2]}" =~ ^[Aa][Pp][Pp][Ll][Ii][Cc][Aa][Tt][Ii][Oo][Nn]/[Jj][Ss][Oo][Nn]([[:space:]]*\;[[:space:]]*[Cc][Hh][Aa][Rr][Ss][Ee][Tt][[:space:]]*=[[:space:]]*[Uu][Tt][Ff]-8)?$ ]] ||
    die "GitHub REST response has an unsupported content type"
  jq -e -s 'length == 1' "$body" >/dev/null 2>&1 || die "GitHub REST response is not exactly one JSON value"
  API_LINK=${lines[3]:-}
  printf -v "$result_name" '%s' "$body"
}

USER_BODY=
api_request GET "$API_BASE/users/$EXPECTED_BOT_LOGIN_URL" '' USER_BODY
AUTHENTICATED=$(jq -er '
  if type == "object" and (.id|type) == "number" and .id >= 1 and .id == (.id|floor)
    and (.login|type) == "string" and .type == "Bot"
  then [(.id|tostring),.login] | @tsv else error("identity") end
' "$USER_BODY" 2>/dev/null) || die "authenticated App bot identity is malformed"
IFS=$'\t' read -r AUTHENTICATED_BOT_ID AUTHENTICATED_BOT_LOGIN AUTHENTICATED_EXTRA <<<"$AUTHENTICATED"
[[ -z "${AUTHENTICATED_EXTRA:-}" && "$AUTHENTICATED_BOT_ID" == "$EXPECTED_BOT_ID" &&
  "$AUTHENTICATED_BOT_LOGIN" == "$EXPECTED_BOT_LOGIN" ]] || die "authenticated App bot identity differs"

REPOSITORY_BODY=
api_request GET "$API_BASE/repos/$REPOSITORY" '' REPOSITORY_BODY
jq -e '
  type == "object" and (.id|type) == "number" and .id >= 1 and .id == (.id|floor)
  and .full_name == "stackpop/edgezero" and .private == false and .visibility == "public"
  and .default_branch == "main" and .owner.login == "stackpop"
' "$REPOSITORY_BODY" >/dev/null 2>&1 || die "authenticated repository identity differs"

BRANCH="${PIN_PREFIX}${SOURCE_REVISION}"
BRANCH_REF="refs/heads/$BRANCH"
REMOTE_REFS="$temporary_root/remote-refs"
authenticated_git ls-remote --refs "$REMOTE_URL" refs/heads/main "$BRANCH_REF" >"$REMOTE_REFS" 2>/dev/null ||
  die "cannot record remote refs"
MAIN_OID=
TARGET_OID=
while IFS=$'\t' read -r oid ref extra || [[ -n "${oid:-}" ]]; do
  [[ -n "$oid" && -z "${extra:-}" && $(is_sha "$oid"; printf '%s' "$?") == 0 ]] ||
    die "remote ref response is malformed"
  case "$ref" in
    refs/heads/main) [[ -z "$MAIN_OID" ]] || die "remote main ref is duplicated"; MAIN_OID=$oid ;;
    "$BRANCH_REF") [[ -z "$TARGET_OID" ]] || die "remote target ref is duplicated"; TARGET_OID=$oid ;;
    *) die "remote ref response contains an unrequested ref" ;;
  esac
done <"$REMOTE_REFS"
[[ -n "$MAIN_OID" ]] || die "remote main ref is absent"

isolated_git clone --no-hardlinks --no-local --no-checkout --no-tags --no-recurse-submodules \
  "$REPOSITORY_ROOT" "$CLONE_ROOT" >/dev/null 2>&1 || die "cannot create private source clone"
chmod 0700 "$CLONE_ROOT" || die "cannot secure private source clone"
clone_git remote set-url origin "$REMOTE_URL" || die "cannot set fixed origin URL"
authenticated_git -C "$CLONE_ROOT" fetch --force --no-tags --no-recurse-submodules \
  origin refs/heads/main:refs/remotes/origin/main >/dev/null 2>&1 || die "cannot fetch remote main"
[[ "$(clone_git rev-parse --verify refs/remotes/origin/main 2>/dev/null)" == "$MAIN_OID" ]] ||
  die "fetched main differs from its recorded OID"
if [[ -n "$TARGET_OID" ]]; then
  authenticated_git -C "$CLONE_ROOT" fetch --force --no-tags --no-recurse-submodules \
    origin "$BRANCH_REF:refs/remotes/origin/pin-target" >/dev/null 2>&1 || die "cannot fetch remote target"
  [[ "$(clone_git rev-parse --verify refs/remotes/origin/pin-target 2>/dev/null)" == "$TARGET_OID" ]] ||
    die "fetched target differs from its recorded OID"
fi
clone_git worktree add --detach "$WORKTREE_ROOT" "$MAIN_OID" >/dev/null 2>&1 ||
  die "cannot create detached main worktree"
chmod 0700 "$WORKTREE_ROOT" || die "cannot secure detached main worktree"

IMAGE_PATH=.github/docker/build-app-cli/image.json
EVIDENCE_PATH=.github/docker/build-app-cli/image-release-evidence.json
MAIN_IMAGE="$WORKTREE_ROOT/$IMAGE_PATH"
MAIN_EVIDENCE="$WORKTREE_ROOT/$EVIDENCE_PATH"

require_regular_blob_entry() {
  local commit=$1 path=$2 label=$3 entry metadata actual_path extra mode type oid metadata_extra
  entry=$(clone_git ls-tree "$commit" -- "$path" 2>/dev/null) || die "cannot inspect $label tree entry"
  [[ -n "$entry" && "$entry" != *$'\n'* ]] || die "$label tree entry is absent or duplicated"
  IFS=$'\t' read -r metadata actual_path extra <<<"$entry"
  IFS=' ' read -r mode type oid metadata_extra <<<"$metadata"
  [[ -z "${extra:-}" && -z "${metadata_extra:-}" && "$mode" == 100644 && "$type" == blob &&
    "$actual_path" == "$path" ]] || die "$label must be a regular mode-100644 blob"
  is_sha "$oid" || die "$label blob OID is malformed"
}

BASE_SOURCE=
BASE_DIGEST=
if [[ -e "$MAIN_IMAGE" || -L "$MAIN_IMAGE" || -e "$MAIN_EVIDENCE" || -L "$MAIN_EVIDENCE" ]]; then
  [[ -f "$MAIN_IMAGE" && ! -L "$MAIN_IMAGE" && -f "$MAIN_EVIDENCE" && ! -L "$MAIN_EVIDENCE" ]] ||
    die "remote main pin pair is incomplete or non-regular"
  require_regular_blob_entry "$MAIN_OID" "$IMAGE_PATH" "remote main image record"
  require_regular_blob_entry "$MAIN_OID" "$EVIDENCE_PATH" "remote main evidence record"
  bash "$CHECK" validate-pair "$MAIN_IMAGE" "$MAIN_EVIDENCE" >/dev/null 2>&1 ||
    die "remote main pin pair is invalid"
  BASE_SOURCE=$(bash "$CHECK" source-revision "$MAIN_IMAGE" 2>/dev/null) ||
    die "cannot read remote main source pin"
  BASE_DIGEST=$(jq -er '.digest' "$MAIN_IMAGE") || die "cannot read remote main digest"
  if [[ "$BASE_SOURCE" != "$SOURCE_REVISION" ]]; then
    clone_git cat-file -e "$BASE_SOURCE^{commit}" 2>/dev/null || die "remote main source pin commit is absent"
    if clone_git merge-base --is-ancestor "$BASE_SOURCE" "$SOURCE_REVISION" 2>/dev/null; then
      :
    elif clone_git merge-base --is-ancestor "$SOURCE_REVISION" "$BASE_SOURCE" 2>/dev/null; then
      die "source revision regresses the protected base pin"
    else
      die "source revision is incomparable with the protected base pin"
    fi
  fi
fi

validate_link_header() {
  local header=$1 page=$2 count=$3 entry url rel target_page expected seen=' '
  local -a entries=()
  [[ -z "$header" ]] && return 0
  IFS=, read -r -a entries <<<"$header"
  for entry in "${entries[@]}"; do
    entry=$(printf '%s' "$entry" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
    if [[ "$entry" =~ ^\<([^\>]*)\>\;[[:space:]]rel=\"(next|prev|first|last)\"$ ]]; then
      url=${BASH_REMATCH[1]}
      rel=${BASH_REMATCH[2]}
    else
      die "pull pagination Link header is malformed"
    fi
    [[ "$seen" != *" $rel "* ]] || die "pull pagination Link relation is duplicated"
    seen+="$rel "
    [[ "$url" =~ ^https://api\.github\.com/repos/stackpop/edgezero/pulls\?state=all\&base=main\&sort=created\&direction=asc\&per_page=100\&page=([1-9][0-9]*)$ ]] ||
      die "pull pagination Link URL varies from the exact query"
    target_page=${BASH_REMATCH[1]}
    is_positive_decimal_at_most "$target_page" 100 ||
      die "pull pagination Link page is outside the bounded inventory"
    case "$rel" in
      next) expected=$((page + 1)); [[ "$count" -eq 100 && "$target_page" -eq "$expected" ]] || die "pull pagination next relation is impossible" ;;
      prev) expected=$((page - 1)); [[ "$page" -gt 1 && "$target_page" -eq "$expected" ]] || die "pull pagination prev relation is impossible" ;;
      first) [[ "$target_page" -eq 1 ]] || die "pull pagination first relation is impossible" ;;
      last) printf '%s\t%s\n' "$page" "$target_page" >>"$LAST_LINKS" ;;
    esac
  done
  if [[ "$seen" == *' next '* ]]; then LINK_HAS_NEXT=true; else LINK_HAS_NEXT=false; fi
}

PULL_IDS="$temporary_root/pull-ids"
MATCHING_PULLS="$temporary_root/matching-pulls"
LAST_LINKS="$temporary_root/last-links"
: >"$PULL_IDS"
: >"$MATCHING_PULLS"
: >"$LAST_LINKS"
page=1
while :; do
  LIST_BODY=
  LIST_URL="$API_BASE/repos/$REPOSITORY/pulls?state=all&base=main&sort=created&direction=asc&per_page=100&page=$page"
  api_request GET "$LIST_URL" '' LIST_BODY
  jq -e 'type == "array" and length <= 100 and all(.[];
    type == "object" and (.id|type) == "number" and .id >= 1 and .id == (.id|floor)
    and (.number|type) == "number" and .number >= 1 and .number == (.number|floor)
    and (.title|type) == "string" and (.head|type) == "object" and (.head.ref|type) == "string")' \
    "$LIST_BODY" >/dev/null 2>&1 || die "pull list page is malformed"
  count=$(jq -r 'length' "$LIST_BODY")
  jq -r '.[] | "id:" + (.id|tostring), "number:" + (.number|tostring)' "$LIST_BODY" >>"$PULL_IDS"
  duplicate=$(sort "$PULL_IDS" | uniq -d | sed -n '1p')
  [[ -z "$duplicate" ]] || die "pull pagination contains duplicate identities"
  if ((page > 1)); then
    prior=1
    while ((prior < page)); do
      cmp -s "$LIST_BODY" "$API_WORK/list-page-$prior" && die "pull pagination repeats a page payload"
      prior=$((prior + 1))
    done
  fi
  cp "$LIST_BODY" "$API_WORK/list-page-$page"
  jq -r --arg branch "$PIN_PREFIX" --arg title "$TITLE_PREFIX" '
    .[] | select((.head.ref|startswith($branch)) or (.title|startswith($title))) | .number
  ' "$LIST_BODY" >>"$MATCHING_PULLS"
  LINK_HAS_NEXT=false
  validate_link_header "$API_LINK" "$page" "$count"
  if ((count < 100)); then
    [[ "$LINK_HAS_NEXT" == false ]] || die "short pull page advertises a next relation"
    final_page=$page
    break
  fi
  ((page < 100)) || die "pull pagination is truncated at 10,000 items"
  page=$((page + 1))
done
while IFS=$'\t' read -r _ linked_last || [[ -n "${linked_last:-}" ]]; do
  [[ -z "${linked_last:-}" || "$linked_last" -eq "$final_page" ]] || die "pull pagination last relation is incorrect"
done <"$LAST_LINKS"

relation_to_source() {
  local proposal=$1 result_name=$2 computed
  clone_git cat-file -e "$proposal^{commit}" 2>/dev/null || die "pin proposal source commit is absent"
  if [[ "$proposal" == "$SOURCE_REVISION" ]]; then
    computed=equal
  elif clone_git merge-base --is-ancestor "$proposal" "$SOURCE_REVISION" 2>/dev/null; then
    computed=older
  elif clone_git merge-base --is-ancestor "$SOURCE_REVISION" "$proposal" 2>/dev/null; then
    computed=newer
  else
    die "pin proposal source is incomparable"
  fi
  printf -v "$result_name" '%s' "$computed"
}

validate_pull() {
  local path=$1 expected_number=$2 result_prefix=$3
  local number source suffix canonical body digest tag source_pr evidence relation head merge_sha merged state
  jq -e '
    type == "object" and (.id|type) == "number" and .id >= 1 and .id == (.id|floor)
    and (.number|type) == "number" and .number >= 1 and .number == (.number|floor)
    and (.state == "open" or .state == "closed") and (.merged|type) == "boolean"
    and ((.merged_at == null) or (.merged_at|type) == "string")
    and ((.merge_commit_sha == null) or (.merge_commit_sha|type) == "string")
    and (.user|type) == "object" and (.user.id|type) == "number" and (.user.login|type) == "string"
    and .user.type == "Bot" and (.title|type) == "string" and (.body|type) == "string"
    and (.base|type) == "object" and .base.ref == "main" and .base.repo.full_name == "stackpop/edgezero"
    and (.head|type) == "object" and (.head.ref|type) == "string" and (.head.sha|type) == "string"
    and (.head.repo|type) == "object" and (.head.repo.full_name|type) == "string"
  ' "$path" >/dev/null 2>&1 || die "selected pin pull response is malformed"
  number=$(jq -er '.number|tostring' "$path")
  [[ "$number" == "$expected_number" ]] || die "selected pin pull number differs"
  [[ "$(jq -er '.user.id|tostring' "$path")" == "$EXPECTED_BOT_ID" &&
    "$(jq -er '.user.login' "$path")" == "$EXPECTED_BOT_LOGIN" ]] ||
    die "pin pull is owned by a different actor"
  [[ "$(jq -er '.head.repo.full_name' "$path")" == "$REPOSITORY" ]] ||
    die "pin pull belongs to a different head repository"
  branch=$(jq -er '.head.ref' "$path")
  head=$(jq -er '.head.sha' "$path")
  is_sha "$head" || die "pin pull head commit is not a full SHA"
  [[ "$branch" == "$PIN_PREFIX"* ]] || die "pin pull branch is not canonical"
  source=${branch#"$PIN_PREFIX"}
  is_sha "$source" || die "pin pull source suffix is not a full SHA"
  [[ "$(jq -er '.title' "$path")" == "$TITLE_PREFIX$source" ]] || die "pin pull title collides with its branch"
  body=$(jq -er '.body' "$path")
  [[ "$body" != *$'\n'* && "$body" == "$BODY_PREFIX"* ]] || die "pin pull body is not one protocol line"
  suffix=${body#"$BODY_PREFIX"}
  printf '%s' "$suffix" >"$API_WORK/pull-protocol"
  jq -e '
    type == "object" and keys == ["evidence-url","image-digest","release-tag","source-pr","source-revision"]
    and all(.[]; type == "string")
  ' "$API_WORK/pull-protocol" >/dev/null 2>&1 || die "pin pull body JSON is malformed"
  digest=$(jq -er '."image-digest"' "$API_WORK/pull-protocol")
  tag=$(jq -er '."release-tag"' "$API_WORK/pull-protocol")
  source_pr=$(jq -er '."source-pr"' "$API_WORK/pull-protocol")
  evidence=$(jq -er '."evidence-url"' "$API_WORK/pull-protocol")
  [[ "$(jq -er '."source-revision"' "$API_WORK/pull-protocol")" == "$source" ]] ||
    die "pin pull body source differs from its branch"
  is_digest "$digest" || die "pin pull digest is invalid"
  [[ "$tag" =~ ^build-container-v[1-9][0-9]*$ ]] || die "pin pull release tag is invalid"
  is_positive_decimal_at_most "$source_pr" "$U64_MAX" || die "pin pull source PR is invalid"
  [[ "$evidence" =~ ^https://github\.com/stackpop/edgezero/pull/$source_pr#issuecomment-([1-9][0-9]*)$ ]] ||
    die "pin pull evidence URL is invalid"
  canonical=$(jq -cnS --arg evidence "$evidence" --arg digest "$digest" --arg tag "$tag" \
    --arg source_pr "$source_pr" --arg source "$source" \
    '{"evidence-url":$evidence,"image-digest":$digest,"release-tag":$tag,"source-pr":$source_pr,"source-revision":$source}')
  [[ "$suffix" == "$canonical" ]] || die "pin pull body is not exact JCS"
  state=$(jq -er '.state' "$path")
  merged=$(jq -er '.merged|tostring' "$path")
  merge_sha=$(jq -er 'if .merge_commit_sha == null then "" else .merge_commit_sha end' "$path")
  if [[ "$merged" == true ]]; then
    [[ "$state" == closed && $(jq -er '.merged_at|type' "$path") == string ]] ||
      die "merged pin pull has inconsistent merge state"
    is_sha "$merge_sha" || die "merged pin pull merge commit is not a full SHA"
    clone_git cat-file -e "$merge_sha^{commit}" 2>/dev/null || die "merged pin pull commit is absent"
    clone_git merge-base --is-ancestor "$merge_sha" "$MAIN_OID" 2>/dev/null ||
      die "merged pin pull commit is not on remote main"
  else
    [[ "$(jq -er '.merged_at == null' "$path")" == true ]] ||
      die "unmerged pin pull has a merged timestamp"
    [[ -z "$merge_sha" ]] || is_sha "$merge_sha" || die "unmerged pin pull merge commit is malformed"
  fi
  relation_to_source "$source" relation
  printf -v "${result_prefix}_NUMBER" '%s' "$number"
  printf -v "${result_prefix}_SOURCE" '%s' "$source"
  printf -v "${result_prefix}_DIGEST" '%s' "$digest"
  printf -v "${result_prefix}_TAG" '%s' "$tag"
  printf -v "${result_prefix}_STATE" '%s' "$state"
  printf -v "${result_prefix}_MERGED" '%s' "$merged"
  printf -v "${result_prefix}_HEAD" '%s' "$head"
  printf -v "${result_prefix}_MERGE" '%s' "$merge_sha"
  printf -v "${result_prefix}_RELATION" '%s' "$relation"
  printf -v "${result_prefix}_BODY" '%s' "$body"
}

snapshot_pull() {
  local prefix=$1 field value value_name
  for field in NUMBER STATE DIGEST HEAD BODY MERGED MERGE; do
    value_name="${prefix}_${field}"
    value=${!value_name}
    printf -v "${prefix}_EXPECTED_${field}" '%s' "$value"
  done
}

set_expected_pull_field() {
  local prefix=$1 field=$2 value=$3
  printf -v "${prefix}_EXPECTED_${field}" '%s' "$value"
}

verify_final_pull() {
  local number=$1 expected_state=$2 expected_digest=$3 expected_head=$4 expected_body=$5
  local expected_merged=$6 expected_merge=$7 final prefix
  api_request GET "$API_BASE/repos/$REPOSITORY/pulls/$number" '' final
  prefix=FINAL
  validate_pull "$final" "$number" "$prefix"
  [[ "$FINAL_STATE" == "$expected_state" && "$FINAL_DIGEST" == "$expected_digest" &&
    "$FINAL_HEAD" == "$expected_head" && "$FINAL_BODY" == "$expected_body" &&
    "$FINAL_MERGED" == "$expected_merged" && "$FINAL_MERGE" == "$expected_merge" ]] ||
    die "final pull state differs from the exact expected state"
}

verify_expected_pull() {
  local prefix=$1 field value value_name
  local number state digest head body merged merge
  for field in NUMBER STATE DIGEST HEAD BODY MERGED MERGE; do
    value_name="${prefix}_EXPECTED_${field}"
    value=${!value_name}
    case "$field" in
      NUMBER) number=$value ;;
      STATE) state=$value ;;
      DIGEST) digest=$value ;;
      HEAD) head=$value ;;
      BODY) body=$value ;;
      MERGED) merged=$value ;;
      MERGE) merge=$value ;;
    esac
  done
  verify_final_pull "$number" "$state" "$digest" "$head" "$body" "$merged" "$merge"
}

verify_selected_pulls() {
  local prefix
  while IFS= read -r prefix || [[ -n "$prefix" ]]; do
    [[ -n "$prefix" ]] || continue
    verify_expected_pull "$prefix"
  done <"$SELECTED_PULLS"
}

verify_remote_main_state() {
  local path="$temporary_root/final-main-ref" oid ref extra found=
  authenticated_git ls-remote --refs "$REMOTE_URL" refs/heads/main >"$path" 2>/dev/null ||
    die "cannot read final remote main"
  while IFS=$'\t' read -r oid ref extra || [[ -n "${oid:-}" ]]; do
    [[ -z "$found" && -n "$oid" && -z "${extra:-}" && "$ref" == refs/heads/main ]] ||
      die "final remote main response is malformed"
    is_sha "$oid" || die "final remote main OID is malformed"
    found=$oid
  done <"$path"
  [[ "$found" == "$MAIN_OID" ]] || die "final remote main differs from the proved state"
}

DESIRED_TITLE="$TITLE_PREFIX$SOURCE_REVISION"
DESIRED_BODY="$BODY_PREFIX$(jq -cnS --arg evidence "$EVIDENCE_URL" --arg digest "$IMAGE_DIGEST" \
  --arg tag "$RELEASE_TAG" --arg source_pr "$SOURCE_PR" --arg source "$SOURCE_REVISION" \
  '{"evidence-url":$evidence,"image-digest":$digest,"release-tag":$tag,"source-pr":$source_pr,"source-revision":$source}')"

SAME_TOTAL=0
NEWER_COUNT=0
OLDER_OPEN="$temporary_root/older-open"
: >"$OLDER_OPEN"
SAME_PULLS="$temporary_root/same-pulls"
: >"$SAME_PULLS"
SELECTED_PULLS="$temporary_root/selected-pulls"
: >"$SELECTED_PULLS"
index=0
pull_relation=
pull_state=
pull_merged=
while IFS= read -r pull_number || [[ -n "$pull_number" ]]; do
  [[ -n "$pull_number" ]] || continue
  is_positive_decimal_at_most "$pull_number" "$U64_MAX" || die "matching pull number is invalid"
  PULL_BODY=
  api_request GET "$API_BASE/repos/$REPOSITORY/pulls/$pull_number" '' PULL_BODY
  index=$((index + 1))
  prefix="PULL_$index"
  validate_pull "$PULL_BODY" "$pull_number" "$prefix"
  snapshot_pull "$prefix"
  printf '%s\n' "$prefix" >>"$SELECTED_PULLS"
  eval "pull_relation=\${${prefix}_RELATION}"
  eval "pull_state=\${${prefix}_STATE}"
  eval "pull_merged=\${${prefix}_MERGED}"
  case "$pull_relation" in
    equal)
      SAME_TOTAL=$((SAME_TOTAL + 1))
      printf '%s\n' "$prefix" >>"$SAME_PULLS"
      ;;
    older)
      if [[ "$pull_state" == open && "$pull_merged" == false ]]; then
        printf '%s\t%s\n' "$pull_number" "$prefix" >>"$OLDER_OPEN"
      fi
      ;;
    newer)
      [[ "$pull_state" == open && "$pull_merged" == false ]] || die "non-open newer proposal is ambiguous"
      NEWER_COUNT=$((NEWER_COUNT + 1))
      ;;
  esac
done <"$MATCHING_PULLS"

SAME_COUNT=0
same_historical_prefix=
while IFS= read -r same_prefix || [[ -n "$same_prefix" ]]; do
  [[ -n "$same_prefix" ]] || continue
  same_candidate_state=
  same_candidate_merged=
  same_candidate_digest=
  same_candidate_body=
  eval "same_candidate_state=\${${same_prefix}_STATE}"
  eval "same_candidate_merged=\${${same_prefix}_MERGED}"
  eval "same_candidate_digest=\${${same_prefix}_DIGEST}"
  eval "same_candidate_body=\${${same_prefix}_BODY}"
  if [[ "$same_candidate_state" == open ||
    ("$same_candidate_digest" == "$IMAGE_DIGEST" && "$same_candidate_body" == "$DESIRED_BODY") ]]; then
    SAME_COUNT=$((SAME_COUNT + 1))
    SAME_PREFIX=$same_prefix
  elif [[ "$same_candidate_merged" == false ]]; then
    same_historical_prefix=$same_prefix
  fi
done <"$SAME_PULLS"
((SAME_COUNT <= 1)) || die "multiple current pin pulls claim the same source"
if ((SAME_COUNT == 0 && SAME_TOTAL == 1)) && [[ -n "$same_historical_prefix" ]]; then
  SAME_COUNT=1
  SAME_PREFIX=$same_historical_prefix
fi
((NEWER_COUNT <= 1)) || die "multiple newer pin proposals are ambiguous"
((SAME_COUNT == 0 || NEWER_COUNT == 0)) || die "same-source and newer proposals are ambiguous"

if ((NEWER_COUNT == 1)); then
  verify_selected_pulls
  verify_remote_main_state
  exit 0
fi

TARGET_CURRENT=false
if [[ -n "$TARGET_OID" ]]; then
  TARGET_IMAGE="$API_WORK/target-image"
  TARGET_EVIDENCE="$API_WORK/target-evidence"
  require_regular_blob_entry "$TARGET_OID" "$IMAGE_PATH" "target branch image record"
  require_regular_blob_entry "$TARGET_OID" "$EVIDENCE_PATH" "target branch evidence record"
  clone_git show "$TARGET_OID:$IMAGE_PATH" >"$TARGET_IMAGE" 2>/dev/null || die "target branch image record is absent"
  clone_git show "$TARGET_OID:$EVIDENCE_PATH" >"$TARGET_EVIDENCE" 2>/dev/null || die "target branch evidence record is absent"
  bash "$CHECK" validate-pair "$TARGET_IMAGE" "$TARGET_EVIDENCE" >/dev/null 2>&1 ||
    die "target branch record pair is invalid"
  TARGET_SOURCE=$(bash "$CHECK" source-revision "$TARGET_IMAGE" 2>/dev/null) ||
    die "cannot read target branch source pin"
  [[ "$TARGET_SOURCE" == "$SOURCE_REVISION" ]] ||
    die "target branch record source differs from its branch suffix"
  TARGET_PARENT=$(clone_git show -s --format=%P "$TARGET_OID") || die "cannot inspect target branch parent"
  TARGET_IDENTITY=$(clone_git show -s --format='%s%n%an%n%ae%n%cn%n%ce' "$TARGET_OID") ||
    die "cannot inspect target branch commit identity"
  TARGET_PATHS=$(clone_git diff-tree --no-commit-id --name-only -r "$TARGET_OID" | sort) ||
    die "cannot inspect target branch changed paths"
  EXPECTED_TARGET_PATHS=$(printf '%s\n%s' "$EVIDENCE_PATH" "$IMAGE_PATH" | sort)
  TARGET_SIGNATURE=$(clone_git cat-file commit "$TARGET_OID" | sed -n '/^gpgsig\(-sha256\)\{0,1\} /p') ||
    die "cannot inspect target branch signature"
  is_sha "$TARGET_PARENT" || die "target branch commit must have exactly one parent"
  clone_git cat-file -e "$TARGET_PARENT^{commit}" 2>/dev/null || die "target branch parent commit is absent"
  if [[ "$TARGET_PARENT" != "$MAIN_OID" ]]; then
    clone_git merge-base --is-ancestor "$TARGET_PARENT" "$MAIN_OID" 2>/dev/null ||
      die "target branch parent is not an ancestor of recorded main"
  fi
  [[ "$TARGET_IDENTITY" == "$DESIRED_TITLE"$'\n'"$EXPECTED_BOT_LOGIN"$'\n'"$EXPECTED_BOT_ID+$EXPECTED_BOT_LOGIN@users.noreply.github.com"$'\n'"$EXPECTED_BOT_LOGIN"$'\n'"$EXPECTED_BOT_ID+$EXPECTED_BOT_LOGIN@users.noreply.github.com" ]] ||
    die "target branch commit author or committer identity differs"
  [[ "$TARGET_PATHS" == "$EXPECTED_TARGET_PATHS" ]] || die "target branch changed paths differ from the exact pin pair"
  [[ -z "$TARGET_SIGNATURE" ]] || die "target branch commit is signed"
  if [[ "$TARGET_PARENT" == "$MAIN_OID" ]] && cmp -s "$TARGET_IMAGE" "$IMAGE_OUTPUT" &&
    cmp -s "$TARGET_EVIDENCE" "$EVIDENCE_OUTPUT"; then
    TARGET_CURRENT=true
  fi
fi

ACTION=create
same_number=
same_digest=
same_tag=
same_state=
same_merged=
same_head=
same_body=
if ((SAME_COUNT == 1)); then
  eval "same_number=\${${SAME_PREFIX}_NUMBER}"
  eval "same_digest=\${${SAME_PREFIX}_DIGEST}"
  eval "same_tag=\${${SAME_PREFIX}_TAG}"
  eval "same_state=\${${SAME_PREFIX}_STATE}"
  eval "same_merged=\${${SAME_PREFIX}_MERGED}"
  eval "same_head=\${${SAME_PREFIX}_HEAD}"
  eval "same_body=\${${SAME_PREFIX}_BODY}"
  [[ "$same_head" == "$TARGET_OID" || "$same_merged" == true ]] ||
    die "same-source pull head differs from the recorded target branch"
  if [[ "$same_merged" == true ]]; then
    [[ "$same_state" == closed && "$BASE_SOURCE" == "$SOURCE_REVISION" &&
      "$BASE_DIGEST" == "$IMAGE_DIGEST" && "$same_digest" == "$IMAGE_DIGEST" &&
      "$same_tag" == "$RELEASE_TAG" && "$same_body" == "$DESIRED_BODY" &&
      $(cmp -s "$MAIN_IMAGE" "$IMAGE_OUTPUT"; printf '%s' "$?") == 0 &&
      $(cmp -s "$MAIN_EVIDENCE" "$EVIDENCE_OUTPUT"; printf '%s' "$?") == 0 ]] ||
      die "merged same-source pull is not the exact current pin"
    verify_selected_pulls
    verify_remote_main_state
    exit 0
  fi
  if [[ "$same_state" == open ]]; then
    if [[ "$same_digest" != "$IMAGE_DIGEST" ]]; then
      ACTION=replace
    elif [[ "$same_tag" == "$RELEASE_TAG" && "$same_body" == "$DESIRED_BODY" ]]; then
      ACTION=verify
    else
      ACTION=reconcile
    fi
  else
    if [[ "$same_digest" == "$IMAGE_DIGEST" ]]; then
      ACTION=reopen
    else
      ACTION=replace_closed
    fi
  fi
fi

if [[ "$TARGET_CURRENT" != true ]]; then
  install -m 0644 "$IMAGE_OUTPUT" "$WORKTREE_ROOT/$IMAGE_PATH" || die "cannot install image record"
  install -m 0644 "$EVIDENCE_OUTPUT" "$WORKTREE_ROOT/$EVIDENCE_PATH" || die "cannot install evidence record"
  worktree_git add -- "$IMAGE_PATH" "$EVIDENCE_PATH" || die "cannot stage pin records"
  STAGED=$(worktree_git diff --cached --name-only --diff-filter=ACMRTUXB) || die "cannot inspect staged pin paths"
  EXPECTED_STAGED=$(printf '%s\n%s' "$EVIDENCE_PATH" "$IMAGE_PATH" | sort)
  [[ "$(printf '%s\n' "$STAGED" | sort)" == "$EXPECTED_STAGED" ]] || die "staged changes are not exactly the pin pair"
  STATUS=$(worktree_git status --porcelain=v1 --untracked-files=all --ignore-submodules=none) ||
    die "cannot inspect pin worktree status"
  while IFS= read -r status_line || [[ -n "$status_line" ]]; do
    [[ -z "$status_line" ]] && continue
    status_path=${status_line:3}
    [[ "$status_path" == "$IMAGE_PATH" || "$status_path" == "$EVIDENCE_PATH" ]] ||
      die "pin worktree contains an unexpected change"
  done <<<"$STATUS"
  worktree_git -c user.name="$EXPECTED_BOT_LOGIN" \
    -c user.email="$EXPECTED_BOT_ID+$EXPECTED_BOT_LOGIN@users.noreply.github.com" \
    -c commit.gpgsign=false -c core.hooksPath=/dev/null \
    commit --no-gpg-sign --no-verify -m "$DESIRED_TITLE" -- "$IMAGE_PATH" "$EVIDENCE_PATH" \
    >/dev/null 2>&1 || die "cannot create pin commit"
  NEW_OID=$(worktree_git rev-parse --verify HEAD) || die "cannot resolve pin commit"
  is_sha "$NEW_OID" || die "pin commit OID is malformed"
  [[ "$(worktree_git show -s --format=%P "$NEW_OID")" == "$MAIN_OID" ]] ||
    die "pin commit does not have exactly recorded main as parent"
  [[ -z "$(worktree_git status --porcelain=v1 --untracked-files=all --ignore-submodules=none)" ]] ||
    die "pin worktree is not clean after commit"
  if [[ -z "$TARGET_OID" ]]; then
    RECHECK="$temporary_root/target-recheck"
    authenticated_git ls-remote --refs "$REMOTE_URL" "$BRANCH_REF" >"$RECHECK" 2>/dev/null ||
      die "cannot recheck absent target branch"
    [[ ! -s "$RECHECK" ]] || die "target branch appeared before creation push"
  fi
  authenticated_git -C "$WORKTREE_ROOT" push --porcelain --no-verify \
    "--force-with-lease=$BRANCH_REF:$TARGET_OID" origin "HEAD:$BRANCH_REF" >/dev/null 2>&1 ||
    die "leased pin branch push failed"
  READBACK="$temporary_root/target-readback"
  authenticated_git ls-remote --refs "$REMOTE_URL" "$BRANCH_REF" >"$READBACK" 2>/dev/null ||
    die "cannot read back pushed target branch"
  [[ "$(awk -F '\t' -v ref="$BRANCH_REF" '$2 == ref {print $1}' "$READBACK")" == "$NEW_OID" &&
    $(wc -l <"$READBACK" | tr -d '[:space:]') == 1 ]] || die "pushed target branch readback differs"
  TARGET_OID=$NEW_OID
fi

while IFS= read -r same_prefix || [[ -n "$same_prefix" ]]; do
  [[ -n "$same_prefix" ]] || continue
  set_expected_pull_field "$same_prefix" HEAD "$TARGET_OID"
done <"$SAME_PULLS"

request_file() {
  local name bytes result_name path
  name=$1
  bytes=$2
  result_name=$3
  path="$API_WORK/$name"
  [[ "$bytes" != *$'\n'* && "$bytes" != *$'\r'* ]] || die "internal request body is multiline"
  printf '%s' "$bytes" >"$path" || die "cannot write REST request body"
  chmod 0600 "$path" || die "cannot secure REST request body"
  printf -v "$result_name" '%s' "$path"
}

require_response_pull_number() {
  local path=$1 expected=$2 actual
  actual=$(jq -er '.number | tostring' "$path" 2>/dev/null) || die "mutation response pull number is absent"
  [[ "$actual" == "$expected" ]] || die "mutation response pull number differs"
}

verify_final_remote_state() {
  local path="$temporary_root/final-remote-refs" oid ref extra final_main='' final_target=''
  authenticated_git ls-remote --refs "$REMOTE_URL" refs/heads/main "$BRANCH_REF" >"$path" 2>/dev/null ||
    die "cannot read final remote refs"
  while IFS=$'\t' read -r oid ref extra || [[ -n "${oid:-}" ]]; do
    [[ -n "$oid" && -z "${extra:-}" ]] || die "final remote ref response is malformed"
    is_sha "$oid" || die "final remote ref OID is malformed"
    case "$ref" in
      refs/heads/main) [[ -z "$final_main" ]] || die "final remote main ref is duplicated"; final_main=$oid ;;
      "$BRANCH_REF") [[ -z "$final_target" ]] || die "final remote target ref is duplicated"; final_target=$oid ;;
      *) die "final remote ref response contains an unrequested ref" ;;
    esac
  done <"$path"
  [[ "$final_main" == "$MAIN_OID" && "$final_target" == "$TARGET_OID" ]] ||
    die "final remote refs differ from the proved state"
}

while IFS=$'\t' read -r old_number old_prefix || [[ -n "${old_number:-}" ]]; do
  [[ -n "$old_number" ]] || continue
  CLOSE_REQUEST=
  request_file "close-$old_number.json" '{"state":"closed"}' CLOSE_REQUEST
  MUTATION_BODY=
  api_request PATCH "$API_BASE/repos/$REPOSITORY/pulls/$old_number" "$CLOSE_REQUEST" MUTATION_BODY
  require_response_pull_number "$MUTATION_BODY" "$old_number"
  set_expected_pull_field "$old_prefix" STATE closed
  verify_expected_pull "$old_prefix"
done <"$OLDER_OPEN"

if [[ "$ACTION" == replace ]]; then
  CLOSE_REQUEST=
  request_file "close-$same_number.json" '{"state":"closed"}' CLOSE_REQUEST
  MUTATION_BODY=
  api_request PATCH "$API_BASE/repos/$REPOSITORY/pulls/$same_number" "$CLOSE_REQUEST" MUTATION_BODY
  require_response_pull_number "$MUTATION_BODY" "$same_number"
  set_expected_pull_field "$SAME_PREFIX" STATE closed
  verify_expected_pull "$SAME_PREFIX"
elif [[ "$ACTION" == replace_closed ]]; then
  verify_expected_pull "$SAME_PREFIX"
fi

if [[ "$ACTION" == verify ]]; then
  verify_expected_pull "$SAME_PREFIX"
elif [[ "$ACTION" == reopen || "$ACTION" == reconcile ]]; then
  REOPEN_JSON=$(jq -cnS --arg base main --arg body "$DESIRED_BODY" --arg state open --arg title "$DESIRED_TITLE" \
    '{base:$base,body:$body,state:$state,title:$title}')
  REOPEN_REQUEST=
  request_file reopen.json "$REOPEN_JSON" REOPEN_REQUEST
  MUTATION_BODY=
  api_request PATCH "$API_BASE/repos/$REPOSITORY/pulls/$same_number" "$REOPEN_REQUEST" MUTATION_BODY
  require_response_pull_number "$MUTATION_BODY" "$same_number"
  set_expected_pull_field "$SAME_PREFIX" STATE open
  set_expected_pull_field "$SAME_PREFIX" DIGEST "$IMAGE_DIGEST"
  set_expected_pull_field "$SAME_PREFIX" BODY "$DESIRED_BODY"
  verify_expected_pull "$SAME_PREFIX"
else
  CREATE_JSON=$(jq -cnS --arg base main --arg body "$DESIRED_BODY" --arg head "$BRANCH" --arg title "$DESIRED_TITLE" \
    '{base:$base,body:$body,draft:false,head:$head,title:$title}')
  CREATE_REQUEST=
  request_file create.json "$CREATE_JSON" CREATE_REQUEST
  CREATED_BODY=
  api_request POST "$API_BASE/repos/$REPOSITORY/pulls" "$CREATE_REQUEST" CREATED_BODY
  CREATED_NUMBER=$(jq -er '.number|tostring' "$CREATED_BODY" 2>/dev/null) || die "created pull number is absent"
  is_positive_decimal_at_most "$CREATED_NUMBER" "$U64_MAX" || die "created pull number is invalid"
  validate_pull "$CREATED_BODY" "$CREATED_NUMBER" CREATED
  [[ "$CREATED_STATE" == open && "$CREATED_DIGEST" == "$IMAGE_DIGEST" &&
    "$CREATED_HEAD" == "$TARGET_OID" && "$CREATED_BODY" == "$DESIRED_BODY" &&
    "$CREATED_MERGED" == false ]] || die "created pull differs from the exact requested state"
  verify_final_pull "$CREATED_NUMBER" open "$IMAGE_DIGEST" "$TARGET_OID" "$DESIRED_BODY" false "$CREATED_MERGE"
fi

verify_selected_pulls
verify_final_remote_state
