#!/usr/bin/env bash
# shellcheck disable=SC2016
{ set +x; set +a; } 2>/dev/null
set -euo pipefail

ALTERNATES_WERE_SET=false
[[ -z "${GIT_ALTERNATE_OBJECT_DIRECTORIES:-}" ]] || ALTERNATES_WERE_SET=true

export LC_ALL=C
export BASH_ENV=
export ENV=
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_NO_REPLACE_OBJECTS=1
export GIT_NO_LAZY_FETCH=1
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY
unset GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_CEILING_DIRECTORIES
unset GIT_REPLACE_REF_BASE

readonly API=https://api.github.com
readonly API_VERSION=2026-03-10
readonly WORKFLOW_PATH=.github/workflows/build-container-ci.yml
readonly ROTATION_PATH=.github/workflows/rotate-build-container-gate.yml@main
readonly ROTATION_ENVIRONMENT=build-container-gate-rotation-lock
readonly GATE_MANIFEST=.github/docker/build-app-cli/gate-paths.txt
readonly CODEOWNERS=.github/CODEOWNERS
readonly HELPER_PATH=.github/docker/build-app-cli/verify-release-prerequisites.sh
readonly U64_MAX=18446744073709551615
readonly U32_MAX=4294967295
readonly MAX_PAGES=100
readonly MAX_ITEMS=10000
readonly MAX_EVIDENCE_BYTES=1048576
readonly MAX_PNG_BYTES=10485760
readonly POLICY_PREFIX='edgezero-gate-rotation-policy-v1 '
readonly ROTATION_PREFIX='edgezero-gate-rotation-v1 '

usage() {
  printf '%s\n' \
    'usage: verify-release-prerequisites.sh configuration <exact configuration flags>' \
    'usage: verify-release-prerequisites.sh release <exact release flags>' \
    'usage: verify-release-prerequisites.sh rotation-review <exact rotation-review flags>' \
    'usage: verify-release-prerequisites.sh rotation-complete <exact rotation-complete flags>' >&2
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

for tool in git jq curl sha256sum stat od cmp awk sed grep sort uniq wc tr date openssl mktemp ln mv chmod id paste tail env rm dirname; do
  command -v "$tool" >/dev/null 2>&1 || tool_die "required tool is unavailable: $tool"
done

is_sha() {
  [[ "$1" =~ ^[0-9a-f]{40}$ && "$1" != 0000000000000000000000000000000000000000 ]]
}

is_login() {
  [[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9-]{0,38}(\[bot\])?$ ]]
}

is_positive_decimal_at_most() {
  local value=$1 maximum=$2
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || return 1
  ((${#value} < ${#maximum})) && return 0
  ((${#value} == ${#maximum})) && [[ "$value" < "$maximum" || "$value" == "$maximum" ]]
}

is_nonnegative_decimal_at_most() {
  [[ "$1" == 0 ]] || is_positive_decimal_at_most "$1" "$2"
}

is_beneath() {
  [[ "$1" == "$2" || "$1" == "$2/"* ]]
}

safe_jq() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null jq "$@"
}

repo_git() {
  env -i PATH="$PATH" LC_ALL=C HOME=/dev/null TMPDIR=/tmp \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0 \
    GIT_NO_REPLACE_OBJECTS=1 GIT_NO_LAZY_FETCH=1 GIT_OPTIONAL_LOCKS=0 \
    git --no-replace-objects -c core.fsmonitor=false -c core.hooksPath=/dev/null \
      -C "$GATE_ROOT" "$@"
}

TEMP_ROOT=
temporary_files=()
AUDIT_TOKEN=
PROBE_TOKEN=
APP_JWT=
AUDIT_REVOKED=true
PROBE_REVOKED=true

remove_temporary_files() {
  local path
  for path in ${temporary_files[@]+"${temporary_files[@]}"}; do
    [[ -z "$path" ]] || rm -f -- "$path" 2>/dev/null || true
  done
  [[ -z "$TEMP_ROOT" ]] || rm -rf -- "$TEMP_ROOT" 2>/dev/null || true
}

best_effort_revoke() {
  local kind=$1 token=$2
  [[ -n "$token" && -n "$TEMP_ROOT" && -d "$TEMP_ROOT" ]] || return 0
  (
    trap - EXIT HUP INT TERM
    api_request "$kind" "$token" DELETE /installation/token 204 ignored
  ) >/dev/null 2>/dev/null || true
}

cleanup_credentials() {
  [[ "$AUDIT_REVOKED" == true ]] || best_effort_revoke installation-audit "$AUDIT_TOKEN"
  [[ "$PROBE_REVOKED" == true ]] || best_effort_revoke publisher-probe "$PROBE_TOKEN"
  APP_JWT='' AUDIT_TOKEN='' PROBE_TOKEN=''
}

cleanup_exit() {
  local status=$1
  trap - EXIT HUP INT TERM
  cleanup_credentials
  remove_temporary_files
  exit "$status"
}

cleanup_signal() {
  local status=$1
  trap - EXIT HUP INT TERM
  cleanup_credentials
  remove_temporary_files
  exit "$status"
}

trap 'cleanup_exit $?' EXIT
trap 'cleanup_signal 129' HUP
trap 'cleanup_signal 130' INT
trap 'cleanup_signal 143' TERM

new_temp() {
  local result_name=$1 label=$2 path
  path=$(mktemp "$TEMP_ROOT/.edgezero-release-$label.XXXXXX" 2>/dev/null) ||
    tool_die 'cannot create private temporary file'
  temporary_files+=("$path")
  printf -v "$result_name" '%s' "$path"
}

file_size() {
  local path=$1 size
  if size=$(env -i PATH="$PATH" LC_ALL=C stat -f '%z' -- "$path" 2>/dev/null); then :
  elif size=$(env -i PATH="$PATH" LC_ALL=C stat -c '%s' -- "$path" 2>/dev/null); then :
  else tool_die 'cannot inspect file size'
  fi
  [[ "$size" =~ ^[0-9]+$ ]] || tool_die 'file size result is malformed'
  printf '%s' "$size"
}

file_mode() {
  local path=$1 mode
  if mode=$(env -i PATH="$PATH" LC_ALL=C stat -f '%Lp' -- "$path" 2>/dev/null); then :
  elif mode=$(env -i PATH="$PATH" LC_ALL=C stat -c '%a' -- "$path" 2>/dev/null); then :
  else tool_die 'cannot inspect file mode'
  fi
  printf '%s' "$mode"
}

file_uid() {
  local path=$1 uid
  if uid=$(env -i PATH="$PATH" LC_ALL=C stat -f '%u' -- "$path" 2>/dev/null); then :
  elif uid=$(env -i PATH="$PATH" LC_ALL=C stat -c '%u' -- "$path" 2>/dev/null); then :
  else tool_die 'cannot inspect file owner'
  fi
  printf '%s' "$uid"
}

canonical_file() {
  local path=$1 label=$2 parent name canonical_parent expected
  [[ "$path" == /* && -f "$path" && ! -L "$path" ]] ||
    die "$label must be an absolute regular non-symlink file"
  parent=${path%/*}; name=${path##*/}; [[ -n "$parent" ]] || parent=/
  canonical_parent=$(cd -- "$parent" && pwd -P) || die "cannot resolve $label parent"
  [[ "$canonical_parent" == "$parent" ]] || die "$label path must already be canonical"
  if [[ "$parent" == / ]]; then expected="/$name"; else expected="$parent/$name"; fi
  [[ "$expected" == "$path" ]] || die "$label path must already be canonical"
}

validate_output_path() {
  local path=$1 label=$2 parent name canonical_parent expected mode top
  [[ "$path" == /* && ! -e "$path" && ! -L "$path" ]] || die "$label must be an absent absolute path"
  parent=${path%/*}; name=${path##*/}; [[ -n "$parent" ]] || parent=/
  [[ -d "$parent" && ! -L "$parent" ]] || die "$label parent must be a directory"
  canonical_parent=$(cd -- "$parent" && pwd -P) || die "cannot resolve $label parent"
  [[ "$canonical_parent" == "$parent" ]] || die "$label parent must already be canonical"
  if [[ "$parent" == / ]]; then expected="/$name"; else expected="$parent/$name"; fi
  [[ "$expected" == "$path" ]] || die "$label path must already be canonical"
  mode=$(file_mode "$parent")
  [[ "$mode" == 700 ]] || die "$label parent must have mode 0700"
  ! is_beneath "$path" "$GATE_ROOT" || die "$label must be outside the gate repository"
  top=$(env -i PATH="$PATH" LC_ALL=C HOME=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_CONFIG_GLOBAL=/dev/null git -C "$parent" rev-parse --show-toplevel 2>/dev/null || true)
  [[ -z "$top" ]] || die "$label must be outside every Git repository"
}

validate_raw_json_keys() {
  safe_jq -ne --stream '[inputs | select(length == 2) | (.[0] | tojson)] as $paths | ($paths | length) == ($paths | unique | length)' "$1" >/dev/null 2>&1 ||
    die "$2 contains duplicate or malformed JSON keys"
}

hash_file() {
  local output
  output=$(env -i PATH="$PATH" LC_ALL=C sha256sum "$1" 2>/dev/null) || tool_die 'cannot hash trusted bytes'
  [[ "$output" =~ ^[0-9a-f]{64}[[:space:]] ]] || tool_die 'SHA-256 output is malformed'
  printf '%s' "${output%%[[:space:]]*}"
}

hash_bytes() {
  local output
  output=$(printf '%s' "$1" | env -i PATH="$PATH" LC_ALL=C sha256sum 2>/dev/null) || tool_die 'cannot hash trusted bytes'
  [[ "$output" =~ ^[0-9a-f]{64}[[:space:]] ]] || tool_die 'SHA-256 output is malformed'
  printf '%s' "${output%%[[:space:]]*}"
}

validate_utc() {
  local value=$1 label=$2 result_name=$3 epoch round_trip
  [[ "$value" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] || die "$label is not canonical UTC"
  epoch=$(safe_jq -nr --arg value "$value" '$value | fromdateiso8601' 2>/dev/null) || die "$label is not a valid UTC instant"
  [[ "$epoch" =~ ^[0-9]+$ ]] || die "$label epoch is malformed"
  round_trip=$(safe_jq -nr --argjson epoch "$epoch" '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")' 2>/dev/null) || die "$label cannot be normalized"
  [[ "$round_trip" == "$value" ]] || die "$label is not a real calendar instant"
  printf -v "$result_name" '%s' "$epoch"
}

validate_png() {
  local path=$1 label=$2 size signature
  canonical_file "$path" "$label"
  size=$(file_size "$path")
  ((size >= 8 && size <= MAX_PNG_BYTES)) || die "$label size is outside its allowed bounds"
  signature=$(env -i PATH="$PATH" LC_ALL=C od -An -tx1 -N8 -- "$path" 2>/dev/null | tr -d '[:space:]') || tool_die "cannot inspect $label"
  [[ "$signature" == 89504e470d0a1a0a ]] || die "$label does not have the PNG signature"
}

validate_key_file() {
  local path=$1 mode uid current_uid
  canonical_file "$path" 'App private-key file'
  mode=$(file_mode "$path"); uid=$(file_uid "$path"); current_uid=$(id -u)
  [[ "$mode" == 600 && "$uid" == "$current_uid" ]] || die 'App private-key file must be owner-only mode 0600'
}

canonical_root() {
  local supplied=$1 canonical
  [[ "$supplied" == /* && -d "$supplied" && ! -L "$supplied" ]] || die 'gate root must be an absolute non-symlink directory'
  canonical=$(cd -- "$supplied" && pwd -P) || die 'cannot resolve gate root'
  [[ "$canonical" == "$supplied" ]] || die 'gate root must already be canonical'
}

require_checkout() {
  local expected=$1 top git_dir common_dir replacements shallow partial promisor sparse actual status gitlinks
  [[ "$ALTERNATES_WERE_SET" == false ]] || die 'gate checkout cannot use environment object alternates'
  [[ "$(repo_git rev-parse --is-inside-work-tree 2>/dev/null)" == true ]] || die 'gate root is not a Git worktree'
  top=$(repo_git rev-parse --show-toplevel 2>/dev/null) || die 'cannot resolve gate top level'
  [[ "$top" == "$GATE_ROOT" ]] || die 'gate root must be the exact repository top level'
  git_dir=$(repo_git rev-parse --absolute-git-dir 2>/dev/null) || die 'cannot resolve gate Git directory'
  common_dir=$(repo_git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) || die 'cannot resolve gate common directory'
  [[ ! -e "$git_dir/info/grafts" && ! -L "$git_dir/info/grafts" && ! -e "$common_dir/info/grafts" && ! -L "$common_dir/info/grafts" ]] || die 'gate checkout cannot contain grafts'
  [[ ! -e "$git_dir/objects/info/alternates" && ! -L "$git_dir/objects/info/alternates" && ! -e "$common_dir/objects/info/alternates" && ! -L "$common_dir/objects/info/alternates" ]] || die 'gate checkout cannot use object alternates'
  replacements=$(repo_git for-each-ref --format='%(refname)' refs/replace/ 2>/dev/null) || die 'cannot inspect replacement refs'
  [[ -z "$replacements" ]] || die 'gate checkout cannot contain replacement refs'
  shallow=$(repo_git rev-parse --is-shallow-repository 2>/dev/null) || die 'cannot inspect history depth'
  [[ "$shallow" == false ]] || die 'gate checkout must contain full history'
  partial=$(repo_git config --get extensions.partialclone 2>/dev/null || true)
  promisor=$(repo_git config --get-regexp '^remote\..*\.promisor$' 2>/dev/null || true)
  [[ -z "$partial$promisor" ]] || die 'gate checkout cannot use partial or promisor objects'
  sparse=$(repo_git config --bool core.sparseCheckout 2>/dev/null || true)
  [[ "$sparse" != true ]] || die 'gate checkout cannot be sparse'
  actual=$(repo_git rev-parse --verify HEAD 2>/dev/null) || die 'gate checkout HEAD is absent'
  [[ "$actual" == "$expected" ]] || die 'gate checkout HEAD differs from supplied gate SHA'
  repo_git symbolic-ref -q HEAD >/dev/null 2>&1 && die 'gate checkout must be detached'
  status=$(repo_git status --porcelain=v1 --untracked-files=all --ignore-submodules=none) || die 'cannot inspect checkout status'
  [[ -z "$status" ]] || die 'gate checkout must be clean'
  gitlinks=$(repo_git ls-tree -r "$expected" 2>/dev/null | awk '$1 == "160000" { print; exit }') || die 'cannot inspect submodule state'
  [[ -z "$gitlinks" ]] || die 'gate checkout cannot contain submodule state'
}

require_gate_helper_source() {
  local source=${BASH_SOURCE[0]} parent canonical entry
  [[ -f "$source" && ! -L "$source" ]] || die 'auditor source must be a regular non-symlink file'
  parent=$(cd -- "$(dirname -- "$source")" && pwd -P) || die 'cannot resolve auditor source directory'
  canonical="$parent/${source##*/}"
  [[ "$canonical" == "$GATE_ROOT/$HELPER_PATH" ]] || die 'auditor must execute from the verified gate path'
  entry=$(repo_git ls-tree "$GATE_SHA" -- "$HELPER_PATH") || die 'cannot inspect auditor gate entry'
  [[ "$entry" =~ ^100755[[:space:]]blob[[:space:]][0-9a-f]{40,64}$'\t'"$HELPER_PATH"$ ]] ||
    die 'auditor is not the executable gate-owned blob'
}

commit_exists() {
  [[ "$(repo_git cat-file -t "$1" 2>/dev/null)" == commit ]]
}

require_ancestor() {
  repo_git merge-base --is-ancestor "$1" "$2" 2>/dev/null || die "$3 ancestry is invalid"
}

publish_file() {
  local destination=$1 content=$2 label=$3 parent staged
  parent=${destination%/*}
  staged=$(mktemp "$parent/.edgezero-$label.XXXXXX" 2>/dev/null) || tool_die "cannot stage $label output"
  temporary_files+=("$staged")
  chmod 0600 "$staged" || tool_die "cannot protect $label output"
  printf '%s' "$content" >"$staged" || die "cannot write $label output"
  ln "$staged" "$destination" 2>/dev/null || die "$label output appeared before publication"
  rm -f -- "$staged" || die "cannot remove staged $label output"
}

MODE=${1:-}
case "$MODE" in configuration|release|rotation-review|rotation-complete) shift ;; *) usage ;; esac

GATE_ROOT='' GATE_SHA='' OLD_GATE_SHA='' NEW_GATE_SHA='' CANDIDATE_PR='' EVIDENCE_URL=''
SOURCE_REVISION='' MERGE_GROUP_SHA='' MERGE_GROUP_RUN_ID='' MERGE_GROUP_RUN_ATTEMPT=''
SMOKE_RUN_ID='' SMOKE_RUN_ATTEMPT='' PUSH_RUN_ID='' PUSH_RUN_ATTEMPT=''
EXPECTED_APP_ID='' EXPECTED_INSTALLATION_ID='' EXPECTED_TEAM_ID='' EXPECTED_BOT_ID='' EXPECTED_BOT_LOGIN=''
PACKAGE_AUDITOR_LOGIN='' PACKAGE_STATE='' DISPATCH_SHA='' FINAL_HEAD_SHA='' LOCK_RUN_ID='' LOCK_RUN_ATTEMPT=''
OPERATOR_LOGIN='' ROTATION_RESULT='' POLICY_REVIEW_JSON='' POLICY_REVIEW_PNG='' ADMIN_PNG=''
ADMIN_REVIEWER='' ADMIN_REVIEWED_AT='' EVIDENCE_OUT='' PREREQUISITE_OUT='' APPROVAL_COMMENT_OUT=''

case "$MODE" in
  configuration)
    allowed=' --gate-root --gate-sha --smoke-run-id --smoke-run-attempt --expected-app-id --expected-installation-id --expected-team-id --expected-bot-id --expected-bot-login --policy-token-review-json --policy-token-review-png --administrator-bypass-png --administrator-bypass-reviewer --administrator-bypass-reviewed-at --evidence-out '
    expected_count=15 ;;
  release)
    allowed=' --gate-root --gate-sha --candidate-pr --evidence-url --source-revision --merge-group-sha --merge-group-run-id --merge-group-run-attempt --smoke-run-id --smoke-run-attempt --push-run-id --push-run-attempt --expected-app-id --expected-installation-id --expected-team-id --expected-bot-id --expected-bot-login --package-auditor-login --package-state --policy-token-review-json --policy-token-review-png --administrator-bypass-png --administrator-bypass-reviewer --administrator-bypass-reviewed-at --evidence-out --publisher-prerequisite-out '
    expected_count=26 ;;
  rotation-review)
    allowed=' --gate-root --old-gate-sha --new-gate-sha --dispatch-sha --final-head-sha --lock-run-id --lock-run-attempt --operator-login --result --policy-token-review-json --policy-token-review-png --evidence-out --approval-comment-out '
    expected_count=13 ;;
  rotation-complete)
    allowed=' --gate-root --gate-sha --lock-run-id --lock-run-attempt --policy-token-review-json --policy-token-review-png --evidence-out --publisher-prerequisite-out '
    expected_count=8 ;;
esac

seen='|'
count=0
while (($#)); do
  (($# >= 2)) || usage
  flag=$1; value=$2; shift 2
  [[ -n "$value" ]] || usage
  case "$allowed" in *" $flag "*) ;; *) usage ;; esac
  case "$seen" in *"|$flag|"*) usage ;; esac
  seen="$seen$flag|"; count=$((count + 1))
  case "$flag" in
    --gate-root) GATE_ROOT=$value ;; --gate-sha) GATE_SHA=$value ;; --old-gate-sha) OLD_GATE_SHA=$value ;;
    --new-gate-sha) NEW_GATE_SHA=$value ;; --candidate-pr) CANDIDATE_PR=$value ;; --evidence-url) EVIDENCE_URL=$value ;;
    --source-revision) SOURCE_REVISION=$value ;; --merge-group-sha) MERGE_GROUP_SHA=$value ;;
    --merge-group-run-id) MERGE_GROUP_RUN_ID=$value ;; --merge-group-run-attempt) MERGE_GROUP_RUN_ATTEMPT=$value ;;
    --smoke-run-id) SMOKE_RUN_ID=$value ;; --smoke-run-attempt) SMOKE_RUN_ATTEMPT=$value ;;
    --push-run-id) PUSH_RUN_ID=$value ;; --push-run-attempt) PUSH_RUN_ATTEMPT=$value ;;
    --expected-app-id) EXPECTED_APP_ID=$value ;; --expected-installation-id) EXPECTED_INSTALLATION_ID=$value ;;
    --expected-team-id) EXPECTED_TEAM_ID=$value ;; --expected-bot-id) EXPECTED_BOT_ID=$value ;;
    --expected-bot-login) EXPECTED_BOT_LOGIN=$value ;; --package-auditor-login) PACKAGE_AUDITOR_LOGIN=$value ;;
    --package-state) PACKAGE_STATE=$value ;; --dispatch-sha) DISPATCH_SHA=$value ;; --final-head-sha) FINAL_HEAD_SHA=$value ;;
    --lock-run-id) LOCK_RUN_ID=$value ;; --lock-run-attempt) LOCK_RUN_ATTEMPT=$value ;;
    --operator-login) OPERATOR_LOGIN=$value ;; --result) ROTATION_RESULT=$value ;;
    --policy-token-review-json) POLICY_REVIEW_JSON=$value ;; --policy-token-review-png) POLICY_REVIEW_PNG=$value ;;
    --administrator-bypass-png) ADMIN_PNG=$value ;; --administrator-bypass-reviewer) ADMIN_REVIEWER=$value ;;
    --administrator-bypass-reviewed-at) ADMIN_REVIEWED_AT=$value ;; --evidence-out) EVIDENCE_OUT=$value ;;
    --publisher-prerequisite-out) PREREQUISITE_OUT=$value ;; --approval-comment-out) APPROVAL_COMMENT_OUT=$value ;;
  esac
done
((count == expected_count)) || usage

if [[ "$MODE" == rotation-review ]]; then GATE_SHA=$OLD_GATE_SHA; fi
is_sha "$GATE_SHA" || usage
canonical_root "$GATE_ROOT"
require_checkout "$GATE_SHA"
require_gate_helper_source

for value in "$LOCK_RUN_ID" "$MERGE_GROUP_RUN_ID" "$SMOKE_RUN_ID" "$PUSH_RUN_ID" "$CANDIDATE_PR" "$EXPECTED_APP_ID" "$EXPECTED_INSTALLATION_ID" "$EXPECTED_TEAM_ID" "$EXPECTED_BOT_ID"; do
  [[ -z "$value" ]] || is_positive_decimal_at_most "$value" "$U64_MAX" || usage
done
for value in "$LOCK_RUN_ATTEMPT" "$MERGE_GROUP_RUN_ATTEMPT" "$SMOKE_RUN_ATTEMPT" "$PUSH_RUN_ATTEMPT"; do
  [[ -z "$value" ]] || is_positive_decimal_at_most "$value" "$U32_MAX" || usage
done
for value in "$EXPECTED_BOT_LOGIN" "$PACKAGE_AUDITOR_LOGIN" "$OPERATOR_LOGIN" "$ADMIN_REVIEWER"; do
  [[ -z "$value" ]] || is_login "$value" || usage
done
for value in "$SOURCE_REVISION" "$MERGE_GROUP_SHA" "$NEW_GATE_SHA" "$DISPATCH_SHA" "$FINAL_HEAD_SHA"; do
  [[ -z "$value" ]] || is_sha "$value" || usage
done
[[ -z "$PACKAGE_STATE" || "$PACKAGE_STATE" == absent || "$PACKAGE_STATE" == public-linked ]] || usage
[[ -z "$ROTATION_RESULT" || "$ROTATION_RESULT" == activated || "$ROTATION_RESULT" == rolled-back ]] || usage
if [[ -n "$EVIDENCE_URL" ]]; then
  [[ "$EVIDENCE_URL" =~ ^https://github\.com/stackpop/edgezero/pull/([1-9][0-9]*)#issuecomment-([1-9][0-9]*)$ ]] || usage
  [[ "${BASH_REMATCH[1]}" == "$CANDIDATE_PR" ]] || usage
  is_positive_decimal_at_most "${BASH_REMATCH[2]}" "$U64_MAX" || usage
fi

for revision in "$SOURCE_REVISION" "$MERGE_GROUP_SHA" "$NEW_GATE_SHA" "$DISPATCH_SHA" "$FINAL_HEAD_SHA"; do
  [[ -z "$revision" ]] || commit_exists "$revision" || die 'required commit object is unavailable'
done
[[ -z "$SOURCE_REVISION" ]] || require_ancestor "$GATE_SHA" "$SOURCE_REVISION" 'release source'
[[ -z "$DISPATCH_SHA" || -z "$FINAL_HEAD_SHA" ]] || require_ancestor "$DISPATCH_SHA" "$FINAL_HEAD_SHA" 'rotation head'

canonical_file "$POLICY_REVIEW_JSON" 'policy-token review JSON'
canonical_file "$POLICY_REVIEW_PNG" 'policy-token review PNG'
validate_png "$POLICY_REVIEW_PNG" 'policy-token review PNG'
validate_output_path "$EVIDENCE_OUT" 'evidence output'
if [[ -n "$PREREQUISITE_OUT" ]]; then validate_output_path "$PREREQUISITE_OUT" 'publisher prerequisite output'; fi
if [[ -n "$APPROVAL_COMMENT_OUT" ]]; then validate_output_path "$APPROVAL_COMMENT_OUT" 'approval comment output'; fi
[[ -z "$PREREQUISITE_OUT" || "$PREREQUISITE_OUT" != "$EVIDENCE_OUT" ]] || die 'output paths must be distinct'
[[ -z "$APPROVAL_COMMENT_OUT" || "$APPROVAL_COMMENT_OUT" != "$EVIDENCE_OUT" ]] || die 'output paths must be distinct'

if [[ "$MODE" == configuration || "$MODE" == release ]]; then
  validate_png "$ADMIN_PNG" 'administrator-bypass PNG'
  [[ ! "$POLICY_REVIEW_PNG" -ef "$ADMIN_PNG" ]] || die 'review PNG inputs must be distinct files'
  validate_utc "$ADMIN_REVIEWED_AT" 'administrator-bypass review time' ADMIN_REVIEW_EPOCH
fi

validate_raw_json_keys "$POLICY_REVIEW_JSON" 'policy-token review'
safe_jq -e '
  type == "object"
  and keys == ["expires-at","organization-grants","repository-grants","resource-owner","reviewed-at","reviewer-login","schema-version","screenshot-sha256","selected-repositories","subject-login","token-id"]
  and ."organization-grants" == {"administration":"write","members":"read","other-displayed":"none"}
  and ."repository-grants" == {"actions":"read","administration":"write","checks":"read","contents":"read","environments":"read","metadata":"read","other-displayed":"none","pull-requests":"read","variables":"read"}
  and ."resource-owner" == "stackpop"
  and ."selected-repositories" == ["stackpop/edgezero"]
  and ."schema-version" == 1 and (."schema-version" | type) == "number"
  and all(."expires-at",."reviewed-at",."reviewer-login",."screenshot-sha256",."subject-login",."token-id"; type == "string")
' "$POLICY_REVIEW_JSON" >/dev/null 2>&1 || die 'policy-token review has the wrong shape or grants'
POLICY_EXPIRES=$(safe_jq -er '."expires-at"' "$POLICY_REVIEW_JSON")
POLICY_REVIEWED=$(safe_jq -er '."reviewed-at"' "$POLICY_REVIEW_JSON")
POLICY_REVIEWER=$(safe_jq -er '."reviewer-login"' "$POLICY_REVIEW_JSON")
POLICY_SCREENSHOT=$(safe_jq -er '."screenshot-sha256"' "$POLICY_REVIEW_JSON")
POLICY_LOGIN=$(safe_jq -er '."subject-login"' "$POLICY_REVIEW_JSON")
POLICY_TOKEN_ID=$(safe_jq -er '."token-id"' "$POLICY_REVIEW_JSON")
if ! is_login "$POLICY_LOGIN" || ! is_login "$POLICY_REVIEWER" || [[ "$POLICY_LOGIN" == "$POLICY_REVIEWER" ]]; then
  die 'policy-token review identities are invalid'
fi
is_positive_decimal_at_most "$POLICY_TOKEN_ID" "$U64_MAX" || die 'policy-token review token id is invalid'
[[ "$POLICY_SCREENSHOT" =~ ^sha256:[0-9a-f]{64}$ ]] || die 'policy-token review screenshot digest is invalid'
[[ "$POLICY_SCREENSHOT" == "sha256:$(hash_file "$POLICY_REVIEW_PNG")" ]] || die 'policy-token review PNG digest differs'
validate_utc "$POLICY_REVIEWED" 'policy-token review time' POLICY_REVIEW_EPOCH
validate_utc "$POLICY_EXPIRES" 'policy-token expiration' POLICY_EXPIRES_EPOCH
NOW_EPOCH=$(env -i PATH="$PATH" LC_ALL=C date -u +%s 2>/dev/null) || tool_die 'cannot read current time'
[[ "$NOW_EPOCH" =~ ^[0-9]+$ ]] || tool_die 'current time is malformed'
((POLICY_REVIEW_EPOCH <= NOW_EPOCH && POLICY_EXPIRES_EPOCH > NOW_EPOCH)) || die 'policy-token review is future or expired'
if [[ "$MODE" == configuration || "$MODE" == release ]]; then
  [[ "$ADMIN_REVIEWER" != "$POLICY_LOGIN" ]] || die 'administrator-bypass reviewer must differ from verifier'
  ((ADMIN_REVIEW_EPOCH <= NOW_EPOCH)) || die 'administrator-bypass review is future'
  ADMIN_DIGEST="sha256:$(hash_file "$ADMIN_PNG")"
fi

POLICY_CANONICAL=$(safe_jq -cS . "$POLICY_REVIEW_JSON" 2>/dev/null) || die 'cannot canonicalize policy-token review'
[[ "$(file_size "$POLICY_REVIEW_JSON")" == "${#POLICY_CANONICAL}" && "$POLICY_CANONICAL" == "$(<"$POLICY_REVIEW_JSON")" ]] ||
  die 'policy-token review is not exact JCS'

# Credentials are not read until every CLI, checkout, path, and reviewed-input check above succeeds.
POLICY_TOKEN=${EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN:-}
[[ -n "$POLICY_TOKEN" ]] || die 'policy-audit credential is missing'
PACKAGE_TOKEN=
APP_KEY_FILE=
if [[ "$MODE" == release ]]; then
  PACKAGE_TOKEN=${EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN:-}
  [[ -n "$PACKAGE_TOKEN" ]] || die 'package-audit credential is missing'
  [[ "$PACKAGE_TOKEN" != "$POLICY_TOKEN" ]] || die 'policy and package credentials must be distinct'
fi
if [[ "$MODE" == configuration || "$MODE" == release ]]; then
  APP_KEY_FILE=${EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE:-}
  [[ -n "$APP_KEY_FILE" ]] || die 'App private-key path is missing'
  validate_key_file "$APP_KEY_FILE"
fi
unset EDGEZERO_RELEASE_POLICY_AUDIT_TOKEN EDGEZERO_RELEASE_PACKAGE_AUDIT_TOKEN EDGEZERO_RELEASE_APP_PRIVATE_KEY_FILE

TEMP_ROOT=$(mktemp -d /tmp/.edgezero-release-audit.XXXXXX 2>/dev/null) || tool_die 'cannot create private temporary directory'
chmod 0700 "$TEMP_ROOT" || tool_die 'cannot protect private temporary directory'

valid_credential() {
  ((${#1} >= 8 && ${#1} <= 512)) && [[ "$1" =~ ^[A-Za-z0-9_.-]+$ ]]
}

valid_credential "$POLICY_TOKEN" || die 'policy-audit credential has an invalid transport form'
[[ -z "$PACKAGE_TOKEN" ]] || valid_credential "$PACKAGE_TOKEN" || die 'package-audit credential has an invalid transport form'

id_is_discovered() {
  local needle=$1 haystack=$2
  case "$haystack" in *"|$needle|"*) return 0 ;; *) return 1 ;; esac
}

ORG_RULESET_IDS='|'
REPO_RULESET_IDS='|'

allow_policy_path() {
  local method=$1 path=$2 id suffix sha check run attempt
  [[ "$method" == GET ]] || return 1
  case "$path" in
    /user|/orgs/stackpop/actions/permissions|/repos/stackpop/edgezero|/repos/stackpop/edgezero/actions/permissions|/repos/stackpop/edgezero/immutable-releases|/repos/stackpop/edgezero/environments/build-container-release|/repos/stackpop/edgezero/environments/build-container-release/deployment_protection_rules|/repos/stackpop/edgezero/git/ref/heads/main)
      return 0 ;;
    "/orgs/stackpop/memberships/$POLICY_LOGIN"|"/orgs/stackpop/teams/edgezero-build-container-releasers/memberships/$POLICY_LOGIN")
      return 0 ;;
    /orgs/stackpop/rulesets\?per_page=100\&page=*|/repos/stackpop/edgezero/rulesets\?per_page=100\&page=*|/repos/stackpop/edgezero/environments/build-container-release/deployment-branch-policies\?per_page=100\&page=*|/repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs\?event=workflow_dispatch\&per_page=100\&page=*)
      suffix=${path##*page=}; is_positive_decimal_at_most "$suffix" "$U64_MAX" ; return ;;
    /orgs/stackpop/rulesets/*)
      id=${path##*/}; is_positive_decimal_at_most "$id" "$U64_MAX" && id_is_discovered "$id" "$ORG_RULESET_IDS"; return ;;
    /repos/stackpop/edgezero/rulesets/*)
      id=${path##*/}; is_positive_decimal_at_most "$id" "$U64_MAX" && id_is_discovered "$id" "$REPO_RULESET_IDS"; return ;;
    /users/*)
      [[ "$path" == "/users/$ACTIVE_BOT_LOGIN" ]]; return ;;
    /repos/stackpop/edgezero/pulls/*)
      id=${path##*/}; is_positive_decimal_at_most "$id" "$U64_MAX" && [[ "$id" == "$ACTIVE_CANDIDATE_PR" ]]; return ;;
    /repos/stackpop/edgezero/actions/variables/*)
      case "${path##*/}" in EDGEZERO_BUILD_CONTAINER_GATE_SHA|EDGEZERO_BUILD_CONTAINER_RELEASE_STATE|EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID|EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID|EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN|EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE) return 0 ;; *) return 1 ;; esac ;;
    /repos/stackpop/edgezero/environments/build-container-release/variables/*)
      case "${path##*/}" in EDGEZERO_BUILD_CONTAINER_APP_ID|EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID|EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID) return 0 ;; *) return 1 ;; esac ;;
    /repos/stackpop/edgezero/environments/build-container-release/secrets/EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY)
      return 0 ;;
    /repos/stackpop/edgezero/commits/*/check-runs\?*)
      suffix=${path#'/repos/stackpop/edgezero/commits/'}; sha=${suffix%%/*}; suffix=${suffix#*/check-runs\?}
      is_sha "$sha" || return 1
      case "$sha" in "$ACTIVE_CANDIDATE_HEAD"|"$MERGE_GROUP_SHA") ;; *) return 1 ;; esac
      case "$suffix" in
        check_name=build-container-release-preflight\&filter=latest\&app_id=15368\&per_page=100\&page=1) check=build-container-release-preflight ;;
        check_name=build-container-local\&filter=latest\&app_id=15368\&per_page=100\&page=1) check=build-container-local ;;
        check_name=build-container-pin\&filter=latest\&app_id=15368\&per_page=100\&page=1) check=build-container-pin ;;
        *) return 1 ;;
      esac
      [[ "$check" == build-container-release-preflight && "$sha" == "$ACTIVE_CANDIDATE_HEAD" ]] ||
        [[ "$check" != build-container-release-preflight && "$sha" == "$MERGE_GROUP_SHA" ]]
      return ;;
    /repos/stackpop/edgezero/actions/runs/*)
      suffix=${path#'/repos/stackpop/edgezero/actions/runs/'}
      run=${suffix%%/*}
      is_positive_decimal_at_most "$run" "$U64_MAX" || return 1
      case "$run" in "$SMOKE_RUN_ID"|"$MERGE_GROUP_RUN_ID"|"$PUSH_RUN_ID"|"$LOCK_RUN_ID") ;; *) return 1 ;; esac
      [[ "$suffix" == "$run" ]] && return 0
      [[ "$suffix" == "$run/approvals" && "$run" == "$LOCK_RUN_ID" ]] && return 0
      if [[ "$suffix" =~ ^$run/attempts/([1-9][0-9]*)/jobs\?per_page=100\&page=([1-9][0-9]*)$ ]]; then
        attempt=${BASH_REMATCH[1]}; id=${BASH_REMATCH[2]}
        is_positive_decimal_at_most "$attempt" "$U32_MAX" && is_positive_decimal_at_most "$id" "$U64_MAX" || return 1
        case "$run:$attempt" in
          "$SMOKE_RUN_ID:$SMOKE_RUN_ATTEMPT"|"$MERGE_GROUP_RUN_ID:$MERGE_GROUP_RUN_ATTEMPT"|"$PUSH_RUN_ID:$PUSH_RUN_ATTEMPT"|"$LOCK_RUN_ID:$LOCK_RUN_ATTEMPT") return 0 ;;
        esac
      fi
      return 1 ;;
  esac
  return 1
}

allow_request() {
  local credential=$1 method=$2 path=$3 page
  case "$credential" in
    policy) allow_policy_path "$method" "$path" ;;
    package)
      [[ "$method" == GET ]] || return 1
      case "$path" in
        /user|"/orgs/stackpop/memberships/$PACKAGE_AUDITOR_LOGIN"|/orgs/stackpop/packages/container/edgezero-build-app-cli) return 0 ;;
        /orgs/stackpop/packages\?package_type=container\&per_page=100\&page=*) page=${path##*page=}; is_positive_decimal_at_most "$page" "$U64_MAX" ;;
        *) return 1 ;;
      esac ;;
    app-jwt)
      case "$method:$path" in
        GET:/app|"GET:/app/installations/$EXPECTED_INSTALLATION_ID"|"POST:/app/installations/$EXPECTED_INSTALLATION_ID/access_tokens") return 0 ;;
        *) return 1 ;;
      esac ;;
    installation-audit|publisher-probe)
      case "$method:$path" in
        DELETE:/installation/token) return 0 ;;
        GET:/installation/repositories\?per_page=100\&page=*) page=${path##*page=}; is_positive_decimal_at_most "$page" "$U64_MAX" ; return ;;
        GET:/repos/stackpop/edgezero) [[ "$credential" == publisher-probe ]] ; return ;;
        *) return 1 ;;
      esac ;;
    *) return 1 ;;
  esac
}

header_value() {
  local file=$1 wanted=$2 result_name=$3 count value
  count=$(awk -v wanted="$wanted" '
    BEGIN { IGNORECASE=1; count=0 }
    { sub(/\r$/, "") }
    index(tolower($0), tolower(wanted) ":") == 1 { count++ }
    END { print count }
  ' "$file") || tool_die 'cannot inspect response headers'
  ((count <= 1)) || die "response repeats $wanted header"
  value=$(awk -v wanted="$wanted" '
    { sub(/\r$/, "") }
    index(tolower($0), tolower(wanted) ":") == 1 {
      value=substr($0, index($0, ":") + 1); sub(/^[[:space:]]+/, "", value); sub(/[[:space:]]+$/, "", value); print value
    }
  ' "$file") || tool_die 'cannot read response header'
  printf -v "$result_name" '%s' "$value"
}

LAST_LINK=
LAST_SCOPES=
api_request() {
  local credential=$1 token=$2 method=$3 path=$4 expected_status=$5 result_name=$6 data=${7:-}
  local response_body response_headers status count selected content_type link scopes normalized_content_type
  allow_request "$credential" "$method" "$path" || die 'API request is outside its credential allowlist'
  valid_credential "$token" || die 'API credential has an invalid transport form'
  new_temp response_body api-body; new_temp response_headers api-headers
  if [[ -n "$data" ]]; then
    if ! printf '%s\n' \
      'header = "Accept: application/vnd.github+json"' \
      'header = "X-GitHub-Api-Version: 2026-03-10"' \
      'header = "User-Agent: edgezero-build-container-gate/1"' \
      "header = \"Authorization: Bearer $token\"" |
      env -i PATH="$PATH" LC_ALL=C curl --disable --silent --show-error \
        --connect-timeout 10 --max-time 30 --max-redirs 0 --request "$method" \
        --output "$response_body" --dump-header "$response_headers" --config - --data-binary "@$data" \
        "$API$path" 2>/dev/null; then
      die 'GitHub API request failed'
    fi
  else
    if ! printf '%s\n' \
      'header = "Accept: application/vnd.github+json"' \
      'header = "X-GitHub-Api-Version: 2026-03-10"' \
      'header = "User-Agent: edgezero-build-container-gate/1"' \
      "header = \"Authorization: Bearer $token\"" |
      env -i PATH="$PATH" LC_ALL=C curl --disable --silent --show-error \
        --connect-timeout 10 --max-time 30 --max-redirs 0 --request "$method" \
        --output "$response_body" --dump-header "$response_headers" --config - "$API$path" 2>/dev/null; then
      die 'GitHub API request failed'
    fi
  fi
  count=$(grep -Ec '^HTTP/[0-9.]+ [0-9]{3}([^0-9]|$)' "$response_headers" 2>/dev/null || true)
  [[ "$count" == 1 ]] || die 'response contains an ambiguous HTTP status history'
  status=$(sed -n 's/^HTTP\/[0-9.]* \([0-9][0-9][0-9]\).*$/\1/p' "$response_headers" | tr -d '\r')
  [[ "$status" == "$expected_status" ]] || die 'GitHub API response has an unexpected status'
  header_value "$response_headers" X-GitHub-Api-Version-Selected selected
  [[ "$selected" == "$API_VERSION" ]] || die 'GitHub API selected an unexpected version'
  header_value "$response_headers" Content-Type content_type
  header_value "$response_headers" Link link
  header_value "$response_headers" X-OAuth-Scopes scopes
  if [[ "$expected_status" == 204 ]]; then
    [[ ! -s "$response_body" ]] || die 'HTTP 204 response contains a body'
  else
    normalized_content_type=$(printf '%s' "$content_type" | tr '[:upper:]' '[:lower:]')
    case "$normalized_content_type" in application/json|'application/json; charset=utf-8') ;; *) die 'GitHub API response has an unexpected media type' ;; esac
    safe_jq -e -s 'length == 1' "$response_body" >/dev/null 2>&1 || die 'GitHub API response is not exactly one JSON value'
    validate_raw_json_keys "$response_body" 'GitHub API response'
  fi
  LAST_LINK=$link
  LAST_SCOPES=$scopes
  printf -v "$result_name" '%s' "$response_body"
}

policy_get() { api_request policy "$POLICY_TOKEN" GET "$1" 200 "$2"; }
package_get() {
  api_request package "$PACKAGE_TOKEN" GET "$1" 200 "$2"
  local normalized
  normalized=$(printf '%s' "$LAST_SCOPES" | tr ',' '\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | sed '/^$/d' | sort -u | paste -sd, -)
  [[ "$normalized" == read:org,read:packages ]] || die 'package token scopes are not exactly read:org and read:packages'
}

write_number_stream() {
  local body=$1 output=$2 paths numbers path_count number_count
  new_temp paths number-paths; new_temp numbers number-tokens
  safe_jq -rc --stream 'select(length == 2 and (.[1] | type) == "number") | (.[0] | @json)' "$body" >"$paths" 2>/dev/null || die 'cannot enumerate JSON number paths'
  awk '
    BEGIN { in_string=0; escaped=0 }
    {
      s=$0
      for (i=1; i<=length(s); i++) {
        c=substr(s,i,1)
        if (in_string) {
          if (escaped) { escaped=0; continue }
          if (c=="\\") { escaped=1; continue }
          if (c=="\"") in_string=0
          continue
        }
        if (c=="\"") { in_string=1; continue }
        if (c ~ /[-0-9]/) {
          token=c
          for (j=i+1; j<=length(s); j++) { q=substr(s,j,1); if (q !~ /[0-9eE+.-]/) break; token=token q }
          print token; i=j-1
        }
      }
    }
    END { if (in_string || escaped) exit 1 }
  ' "$body" >"$numbers" || die 'cannot preserve raw JSON numbers'
  path_count=$(wc -l <"$paths" | tr -d '[:space:]'); number_count=$(wc -l <"$numbers" | tr -d '[:space:]')
  [[ "$path_count" == "$number_count" ]] || die 'JSON numeric token stream is ambiguous'
  paste "$paths" "$numbers" >"$output" || tool_die 'cannot pair JSON number tokens'
}

number_at_path() {
  local body=$1 path=$2 maximum=$3 result_name=$4 allow_zero=${5:-false} stream matches matches_count raw_value
  new_temp stream number-stream; new_temp matches number-match
  write_number_stream "$body" "$stream"
  awk -F '\t' -v wanted="$path" '$1 == wanted { print $2 }' "$stream" >"$matches" || die 'cannot select JSON number'
  matches_count=$(wc -l <"$matches" | tr -d '[:space:]')
  [[ "$matches_count" == 1 ]] || die "JSON number is missing or duplicated at $path"
  raw_value=$(<"$matches")
  if [[ "$allow_zero" == true ]]; then
    is_nonnegative_decimal_at_most "$raw_value" "$maximum" || die "JSON number is not a canonical nonnegative integer at $path"
  else
    is_positive_decimal_at_most "$raw_value" "$maximum" || die "JSON number is not a canonical positive integer at $path"
  fi
  printf -v "$result_name" '%s' "$raw_value"
}

validate_link() {
  local link=$1 base=$2 page=$3 expect_next=$4 declared_total=$5 item_count=$6 result_name=$7
  local part target rel target_page seen='|' canonical_without_page expected_without_page last_page observed_last=''
  if [[ -z "$link" ]]; then
    [[ "$expect_next" == false ]] || die 'paginated response omits required next relation'
    printf -v "$result_name" '%s' ''
    return
  fi
  while [[ -n "$link" ]]; do
    part=${link%%,*}; if [[ "$link" == *,* ]]; then link=${link#*,}; else link=; fi
    part=${part#"${part%%[![:space:]]*}"}; part=${part%"${part##*[![:space:]]}"}
    [[ "$part" =~ ^\<(https://api\.github\.com[^\>]*)\>\;[[:space:]]rel=\"(next|prev|first|last)\"$ ]] || die 'Link header relation is malformed'
    target=${BASH_REMATCH[1]}; rel=${BASH_REMATCH[2]}
    case "$seen" in *"|$rel|"*) die 'Link header repeats a relation' ;; esac; seen="$seen$rel|"
    [[ "$target" =~ ^(https://api\.github\.com.*page=)([1-9][0-9]*)$ ]] || die 'Link target page is not canonical'
    target_page=${BASH_REMATCH[2]}; canonical_without_page=${BASH_REMATCH[1]}
    is_positive_decimal_at_most "$target_page" "$MAX_PAGES" || die 'Link target page exceeds the pagination bound'
    expected_without_page="https://api.github.com${base%page=*}page="
    [[ "$canonical_without_page" == "$expected_without_page" ]] || die 'Link target changes path or non-page query bytes'
    case "$rel" in
      next) ((target_page == page + 1)) || die 'Link next page is not successive' ;;
      prev) ((page > 1 && target_page == page - 1)) || die 'Link previous page is invalid' ;;
      first) [[ "$target_page" == 1 ]] || die 'Link first page is invalid' ;;
      last)
        if [[ -n "$declared_total" ]]; then last_page=$(((declared_total + 99) / 100)); ((last_page > 0)) || last_page=1; [[ "$target_page" == "$last_page" ]] || die 'Link last page is inconsistent'; fi
        observed_last=$target_page ;;
    esac
  done
  case "$seen" in *'|next|'*) [[ "$expect_next" == true ]] || die 'Link contains next after list completion' ;; *) [[ "$expect_next" == false ]] || die 'Link omits next before list completion' ;; esac
  ((item_count <= 100)) || die 'page exceeds the item bound'
  printf -v "$result_name" '%s' "$observed_last"
}

paginate() {
  local credential=$1 base=$2 key=$3 declared=$4 result_name=$5
  local page=1 total_seen=0 expected_total='' item_count expect_next response combined next_combined digest page_digests='|' ids='|' id lines total_value
  local linked_last='' page_last=''
  new_temp combined pagination-combined; printf '[]' >"$combined"
  while ((page <= MAX_PAGES)); do
    case "$credential" in
      policy) policy_get "${base}page=$page" response ;;
      package) package_get "${base}page=$page" response ;;
      installation-audit) api_request installation-audit "$AUDIT_TOKEN" GET "${base}page=$page" 200 response ;;
      publisher-probe) api_request publisher-probe "$PROBE_TOKEN" GET "${base}page=$page" 200 response ;;
      *) die 'pagination credential is invalid' ;;
    esac
    if [[ -z "$key" ]]; then
      safe_jq -e 'type == "array"' "$response" >/dev/null 2>&1 || die 'paginated response is not an array'
      item_count=$(safe_jq -r 'length' "$response")
      new_temp next_combined pagination-next
      safe_jq -c -s '.[0] + .[1]' "$combined" "$response" >"$next_combined" || die 'cannot combine paginated response'
    else
      safe_jq -e --arg key "$key" '.[$key] | type == "array"' "$response" >/dev/null 2>&1 || die 'paginated response has the wrong collection shape'
      item_count=$(safe_jq -r --arg key "$key" '.[$key] | length' "$response")
      if [[ "$declared" == true ]]; then
        safe_jq -e '.total_count | type == "number"' "$response" >/dev/null 2>&1 || die 'paginated response omits total_count'
        number_at_path "$response" '["total_count"]' "$U64_MAX" total_value true
        if [[ -z "$expected_total" ]]; then expected_total=$total_value; else [[ "$expected_total" == "$total_value" ]] || die 'paginated total_count changed'; fi
      fi
      new_temp next_combined pagination-next
      safe_jq -c -s --arg key "$key" '.[0] + .[1][$key]' "$combined" "$response" >"$next_combined" || die 'cannot combine paginated response'
    fi
    if [[ ! "$item_count" =~ ^[0-9]+$ ]] || ((item_count > 100)); then die 'paginated page size is invalid'; fi
    digest=$(hash_file "$response"); case "$page_digests" in *"|$digest|"*) die 'paginated response repeats a page payload' ;; esac; page_digests="$page_digests$digest|"
    lines=$(safe_jq -r "${key:+.\"$key\" | }.[] | if (.id | type) == \"number\" then (.id | tostring) else error(\"id\") end" "$response" 2>/dev/null) || die 'paginated item id is missing or mistyped'
    while IFS= read -r id || [[ -n "$id" ]]; do
      [[ -z "$id" ]] && continue
      is_positive_decimal_at_most "$id" "$U64_MAX" || die 'paginated item id is invalid'
      case "$ids" in *"|$id|"*) die 'paginated response repeats an item id' ;; esac; ids="$ids$id|"
    done <<<"$lines"
    mv "$next_combined" "$combined" || tool_die 'cannot advance pagination state'
    total_seen=$((total_seen + item_count)); ((total_seen <= MAX_ITEMS)) || die 'paginated result exceeds item bound'
    if [[ -n "$expected_total" ]]; then
      ((total_seen <= expected_total)) || die 'paginated result exceeds total_count'
      if ((total_seen < expected_total)); then expect_next=true; else expect_next=false; fi
    elif ((item_count == 100)); then expect_next=true
    else expect_next=false
    fi
    ((page < MAX_PAGES)) || { [[ "$expect_next" == false && "$item_count" -lt 100 ]] || die 'pagination is truncated at page 100'; }
    page_last=''
    validate_link "$LAST_LINK" "$base" "$page" "$expect_next" "$expected_total" "$item_count" page_last
    if [[ -n "$page_last" ]]; then
      if [[ -z "$linked_last" ]]; then linked_last=$page_last
      elif [[ "$linked_last" != "$page_last" ]]; then die 'Link last relation changed between pages'
      fi
    fi
    [[ "$expect_next" == true ]] || break
    page=$((page + 1))
  done
  [[ -z "$expected_total" || "$total_seen" == "$expected_total" ]] || die 'paginated result is incomplete'
  [[ -z "$linked_last" || "$linked_last" == "$page" ]] || die 'Link last relation does not identify the completed page'
  printf -v "$result_name" '%s' "$combined"
}

one_page_list() {
  local path=$1 key=$2 expected_count=$3 result_name=$4 response count
  policy_get "$path" response
  [[ -z "$LAST_LINK" ]] || die 'one-page endpoint returned a continuation'
  safe_jq -e --arg key "$key" '.[$key] | type == "array"' "$response" >/dev/null 2>&1 || die 'one-page response has the wrong shape'
  number_at_path "$response" '["total_count"]' "$U64_MAX" count
  [[ "$count" == "$expected_count" ]] || die 'one-page response total_count differs'
  [[ "$(safe_jq -r --arg key "$key" '.[$key] | length' "$response")" == "$expected_count" ]] || die 'one-page response length differs'
  printf -v "$result_name" '%s' "$response"
}

extract_variable() {
  local body=$1 name=$2 result_name=$3 extracted
  extracted=$(safe_jq -er --arg name "$name" 'select(type == "object" and .name == $name and (.value | type) == "string" and ((.value | test("[\\r\\n]")) | not)) | .value' "$body" 2>/dev/null) ||
    die "repository variable $name is invalid"
  printf -v "$result_name" '%s' "$extracted"
}

verify_named_time_record() {
  local body=$1 name=$2 expected_value=$3 result_name=$4 value created updated created_epoch updated_epoch canonical
  safe_jq -e --arg name "$name" 'type == "object" and .name == $name and (.created_at | type) == "string" and (.updated_at | type) == "string" and (.value | type) == "string"' "$body" >/dev/null 2>&1 ||
    die "environment variable $name record is invalid"
  value=$(safe_jq -er '.value' "$body"); [[ -z "$expected_value" || "$value" == "$expected_value" ]] || die "environment variable $name differs"
  created=$(safe_jq -er '.created_at' "$body"); updated=$(safe_jq -er '.updated_at' "$body")
  validate_utc "$created" "$name creation time" created_epoch; validate_utc "$updated" "$name update time" updated_epoch
  ((created_epoch <= updated_epoch)) || die "environment variable $name timestamps are inconsistent"
  canonical=$(safe_jq -cnS --arg name "$name" --arg value "$value" --arg created "$created" --arg updated "$updated" '{created_at:$created,name:$name,updated_at:$updated,value:$value}')
  printf -v "$result_name" '%s' "$canonical"
}

verify_secret_time_record() {
  local body=$1 name=$2 result_name=$3 created updated created_epoch updated_epoch canonical
  safe_jq -e --arg name "$name" 'type == "object" and .name == $name and (.created_at | type) == "string" and (.updated_at | type) == "string" and (has("value") | not)' "$body" >/dev/null 2>&1 ||
    die "environment secret $name metadata is invalid"
  created=$(safe_jq -er '.created_at' "$body"); updated=$(safe_jq -er '.updated_at' "$body")
  validate_utc "$created" "$name creation time" created_epoch; validate_utc "$updated" "$name update time" updated_epoch
  ((created_epoch <= updated_epoch)) || die "environment secret $name timestamps are inconsistent"
  canonical=$(safe_jq -cnS --arg name "$name" --arg created "$created" --arg updated "$updated" '{created_at:$created,name:$name,updated_at:$updated}')
  printf -v "$result_name" '%s' "$canonical"
}

validate_record_string() {
  local record=$1 expected_gate=$2 result_name=$3 file canonical state source source_pr evidence_url rotation previous evidence
  new_temp file prerequisite-record; printf '%s' "$record" >"$file"
  validate_raw_json_keys "$file" 'publisher prerequisite'
  safe_jq -e '
    type == "object"
    and keys == ["evidence-sha256","evidence-url","gate-sha","previous-value-sha256","rotation-history","schema-version","source-pr","source-revision"]
    and ."schema-version" == 2 and (."schema-version" | type) == "number"
    and (."evidence-sha256" | type) == "string" and (."gate-sha" | type) == "string"
    and ((."previous-value-sha256" == null) or ((."previous-value-sha256" | type) == "string"))
    and ((."source-revision" == null) or ((."source-revision" | type) == "string"))
    and ((."source-pr" == null) or ((."source-pr" | type) == "string"))
    and ((."evidence-url" == null) or ((."evidence-url" | type) == "string"))
    and ((."rotation-history" == {"state":"bootstrap-no-rotation"}) or
      ((."rotation-history" | type) == "object"
       and (."rotation-history" | keys) == ["created-at","evidence-sha256","history-sha256","run-attempt","run-id","run-number","state"]
       and ."rotation-history".state == "verified"
       and all(."rotation-history"."created-at",."rotation-history"."evidence-sha256",."rotation-history"."history-sha256",."rotation-history"."run-attempt",."rotation-history"."run-id",."rotation-history"."run-number"; type == "string")))
  ' "$file" >/dev/null 2>&1 || die 'publisher prerequisite has the wrong schema-version-2 shape'
  canonical=$(safe_jq -cS . "$file" 2>/dev/null) || die 'cannot canonicalize publisher prerequisite'
  [[ "$canonical" == "$record" ]] || die 'publisher prerequisite is not exact JCS'
  evidence=$(safe_jq -er '."evidence-sha256"' "$file"); [[ "$evidence" =~ ^sha256:[0-9a-f]{64}$ ]] || die 'publisher prerequisite evidence digest is invalid'
  gate=$(safe_jq -er '."gate-sha"' "$file"); is_sha "$gate" || die 'publisher prerequisite gate SHA is invalid'
  [[ -z "$expected_gate" || "$gate" == "$expected_gate" ]] || die 'publisher prerequisite gate SHA differs'
  previous=$(safe_jq -r 'if ."previous-value-sha256" == null then "null" else ."previous-value-sha256" end' "$file")
  [[ "$previous" == null || "$previous" =~ ^sha256:[0-9a-f]{64}$ ]] || die 'publisher prerequisite predecessor digest is invalid'
  source=$(safe_jq -r 'if ."source-revision" == null then "null" else ."source-revision" end' "$file")
  source_pr=$(safe_jq -r 'if ."source-pr" == null then "null" else ."source-pr" end' "$file")
  evidence_url=$(safe_jq -r 'if ."evidence-url" == null then "null" else ."evidence-url" end' "$file")
  if [[ "$source" == null ]]; then
    [[ "$source_pr" == null && "$evidence_url" == null ]] || die 'publisher prerequisite source tuple is partial'
  else
    is_sha "$source" || die 'publisher prerequisite source SHA is invalid'
    is_positive_decimal_at_most "$source_pr" "$U64_MAX" || die 'publisher prerequisite source PR is invalid'
    [[ "$evidence_url" =~ ^https://github\.com/stackpop/edgezero/pull/([1-9][0-9]*)#issuecomment-([1-9][0-9]*)$ ]] || die 'publisher prerequisite evidence URL is invalid'
    if [[ "${BASH_REMATCH[1]}" != "$source_pr" ]] || ! is_positive_decimal_at_most "${BASH_REMATCH[2]}" "$U64_MAX"; then
      die 'publisher prerequisite evidence URL identity differs'
    fi
    commit_exists "$source" || die 'publisher prerequisite source commit is unavailable'
    require_ancestor "$gate" "$source" 'publisher prerequisite source'
  fi
  state=$(safe_jq -er '."rotation-history".state' "$file")
  if [[ "$state" == verified ]]; then
    rotation=$(safe_jq -cS '."rotation-history"' "$file")
    created=$(safe_jq -er '."created-at"' <<<"$rotation"); validate_utc "$created" 'rotation history creation time' unused_epoch
    for pair in "run-attempt:$U32_MAX" "run-id:$U64_MAX" "run-number:$U64_MAX"; do
      name=${pair%%:*}; maximum=${pair#*:}; value=$(safe_jq -er --arg name "$name" '.[$name]' <<<"$rotation")
      is_positive_decimal_at_most "$value" "$maximum" || die "rotation history $name is invalid"
    done
    for name in evidence-sha256 history-sha256; do value=$(safe_jq -er --arg name "$name" '.[$name]' <<<"$rotation"); [[ "$value" =~ ^sha256:[0-9a-f]{64}$ ]] || die "rotation history $name is invalid"; done
  else
    rotation='{"state":"bootstrap-no-rotation"}'
  fi
  printf -v "$result_name" '%s' "$rotation"
}

validate_manifest_and_codeowners() {
  local gate=$1 main=$2 manifest entry size previous='' path line expected old_entry new_entry
  new_temp manifest gate-manifest
  repo_git show "$gate:$GATE_MANIFEST" >"$manifest" 2>/dev/null || die 'gate path manifest is absent'
  size=$(file_size "$manifest"); ((size > 0 && size <= 65536)) || die 'gate path manifest size is invalid'
  [[ "$(tail -c 1 "$manifest" | od -An -tuC | tr -d ' ')" == 10 ]] || die 'gate path manifest must end in one LF'
  while IFS= read -r path; do
    [[ -n "$path" && "$path" =~ ^[A-Za-z0-9._/+-]+$ && "$path" != /* && "$path" != -* && "$path" != *//* && "$path" != */../* ]] || die 'gate path manifest contains an invalid path'
    [[ -z "$previous" || "$previous" < "$path" ]] || die 'gate path manifest is not uniquely sorted'
    previous=$path
    entry=$(repo_git ls-tree "$gate" -- "$path") || die 'cannot inspect gate path'
    [[ "$entry" =~ ^100(644|755)[[:space:]]blob[[:space:]][0-9a-f]{40,64}$'\t'"$path"$ ]] || die 'gate path is not a regular blob'
    line="/$path @stackpop/edgezero-build-container-gate-reviewers"
    [[ "$(repo_git show "$gate:$CODEOWNERS" | grep -Fxc "$line")" == 1 ]] || die 'CODEOWNERS does not exactly cover a gate path'
    if [[ -n "$main" ]]; then
      old_entry=$(repo_git ls-tree "$gate" -- "$path"); new_entry=$(repo_git ls-tree "$main" -- "$path")
      [[ "$old_entry" == "$new_entry" ]] || die 'protected main gate path differs from active gate'
    fi
  done <"$manifest"
}

verify_policy_actor() {
  local body user_id
  policy_get /user body
  safe_jq -e --arg login "$POLICY_LOGIN" 'type == "object" and .login == $login and .type == "User" and (.id | type) == "number"' "$body" >/dev/null 2>&1 || die 'policy token identity is invalid'
  number_at_path "$body" '["id"]' "$U64_MAX" user_id
  policy_get "/orgs/stackpop/memberships/$POLICY_LOGIN" body
  safe_jq -e '.state == "active" and .role == "admin"' "$body" >/dev/null 2>&1 || die 'policy actor is not an active organization owner'
  policy_get "/orgs/stackpop/teams/edgezero-build-container-releasers/memberships/$POLICY_LOGIN" body
  safe_jq -e '.state == "active" and (.role == "member" or .role == "maintainer")' "$body" >/dev/null 2>&1 || die 'policy actor is not an active releaser'
  POLICY_USER_ID=$user_id
}

verify_rulesets() {
  local final_gate=$1 body list ids id detail detail_id name include org_count=0 main_count=0 image_create=0 image_immutable=0 action_create=0 action_immutable=0 pin_count=0
  paginate policy '/orgs/stackpop/rulesets?per_page=100&' '' false list
  ids=$(safe_jq -r '.[] | select((.id|type)=="number") | .id | tostring' "$list")
  while IFS= read -r id || [[ -n "$id" ]]; do [[ -z "$id" ]] && continue; is_positive_decimal_at_most "$id" "$U64_MAX" || die 'organization ruleset id is invalid'; case "$ORG_RULESET_IDS" in *"|$id|"*) die 'organization ruleset id repeats' ;; esac; ORG_RULESET_IDS="$ORG_RULESET_IDS$id|"; done <<<"$ids"
  while IFS= read -r id || [[ -n "$id" ]]; do
    [[ -z "$id" ]] && continue; policy_get "/orgs/stackpop/rulesets/$id" detail
    number_at_path "$detail" '["id"]' "$U64_MAX" detail_id; [[ "$detail_id" == "$id" ]] || die 'organization ruleset detail id differs'
    name=$(safe_jq -er '.name' "$detail" 2>/dev/null) || die 'organization ruleset name is invalid'
    if [[ "$name" == edgezero-build-container-required-workflow ]]; then
      safe_jq -e --arg gate "$final_gate" --argjson repo "$REPOSITORY_ID" '
        .source_type == "Organization" and .source == "stackpop" and .target == "branch" and .enforcement == "active"
        and .bypass_actors == []
        and .conditions == {"ref_name":{"exclude":[],"include":["refs/heads/main"]},"repository_id":{"repository_ids":[$repo]}}
        and .rules == [{"parameters":{"do_not_enforce_on_create":false,"workflows":[{"path":".github/workflows/build-container-ci.yml","repository_id":$repo,"sha":$gate}]},"type":"workflows"}]
      ' "$detail" >/dev/null 2>&1 || die 'required-workflow ruleset differs from the exact contract'
      REQUIRED_WORKFLOW_ID=$id; org_count=$((org_count + 1))
    fi
  done <<<"$ids"
  ((org_count == 1)) || die 'required-workflow ruleset is missing or duplicated'

  paginate policy '/repos/stackpop/edgezero/rulesets?per_page=100&' '' false list
  ids=$(safe_jq -r '.[] | select((.id|type)=="number") | .id | tostring' "$list")
  while IFS= read -r id || [[ -n "$id" ]]; do [[ -z "$id" ]] && continue; is_positive_decimal_at_most "$id" "$U64_MAX" || die 'repository ruleset id is invalid'; case "$REPO_RULESET_IDS" in *"|$id|"*) die 'repository ruleset id repeats' ;; esac; REPO_RULESET_IDS="$REPO_RULESET_IDS$id|"; done <<<"$ids"
  while IFS= read -r id || [[ -n "$id" ]]; do
    [[ -z "$id" ]] && continue; policy_get "/repos/stackpop/edgezero/rulesets/$id" detail
    number_at_path "$detail" '["id"]' "$U64_MAX" detail_id; [[ "$detail_id" == "$id" ]] || die 'repository ruleset detail id differs'
    name=$(safe_jq -er '.name' "$detail" 2>/dev/null) || die 'repository ruleset name is invalid'
    case "$name" in
      edgezero-build-container-main)
        safe_jq -e '
          .source_type == "Repository" and .source == "stackpop/edgezero" and .target == "branch" and .enforcement == "active" and .bypass_actors == []
          and .conditions == {"ref_name":{"exclude":[],"include":["refs/heads/main"]}}
          and .rules == [
            {"parameters":{"allowed_merge_methods":["squash"],"dismiss_stale_reviews_on_push":true,"require_code_owner_review":true,"require_last_push_approval":true,"required_approving_review_count":2,"required_review_thread_resolution":true},"type":"pull_request"},
            {"parameters":{"check_response_timeout_minutes":60,"grouping_strategy":"ALLGREEN","max_entries_to_build":1,"max_entries_to_merge":1,"merge_method":"SQUASH","min_entries_to_merge":1,"min_entries_to_merge_wait_minutes":0},"type":"merge_queue"}]
        ' "$detail" >/dev/null 2>&1 || die 'protected-main ruleset differs from the exact contract'; MAIN_RULESET_ID=$id; main_count=$((main_count + 1)) ;;
      edgezero-build-container-tag-creation|edgezero-action-version-tag-creation)
        if [[ "$name" == edgezero-build-container-tag-creation ]]; then include='refs/tags/build-container-v*'; image_create=$((image_create + 1)); IMAGE_CREATION_ID=$id; else include='refs/tags/v*'; action_create=$((action_create + 1)); ACTION_CREATION_ID=$id; fi
        safe_jq -e --arg include "$include" --argjson team "$ACTIVE_TEAM_ID" '.source_type == "Repository" and .source == "stackpop/edgezero" and .target == "tag" and .enforcement == "active" and .conditions == {"ref_name":{"exclude":[],"include":[$include]}} and .bypass_actors == [{"actor_id":$team,"actor_type":"Team","bypass_mode":"always"}] and .rules == [{"type":"creation"}]' "$detail" >/dev/null 2>&1 || die "$name differs from the exact contract" ;;
      edgezero-build-container-tag-immutability|edgezero-action-version-tag-immutability)
        if [[ "$name" == edgezero-build-container-tag-immutability ]]; then include='refs/tags/build-container-v*'; image_immutable=$((image_immutable + 1)); IMAGE_IMMUTABILITY_ID=$id; else include='refs/tags/v*'; action_immutable=$((action_immutable + 1)); ACTION_IMMUTABILITY_ID=$id; fi
        safe_jq -e --arg include "$include" '.source_type == "Repository" and .source == "stackpop/edgezero" and .target == "tag" and .enforcement == "active" and .conditions == {"ref_name":{"exclude":[],"include":[$include]}} and .bypass_actors == [] and .rules == [{"parameters":{"update_allows_fetch_and_merge":false},"type":"update"},{"type":"deletion"}]' "$detail" >/dev/null 2>&1 || die "$name differs from the exact contract" ;;
      edgezero-build-container-pin-branches)
        safe_jq -e --argjson app "$ACTIVE_APP_ID" '.source_type == "Repository" and .source == "stackpop/edgezero" and .target == "branch" and .enforcement == "active" and .conditions == {"ref_name":{"exclude":[],"include":["refs/heads/edgezero-build-container-pin/*"]}} and .bypass_actors == [{"actor_id":$app,"actor_type":"Integration","bypass_mode":"always"}] and .rules == [{"type":"creation"},{"parameters":{"update_allows_fetch_and_merge":false},"type":"update"},{"type":"deletion"}]' "$detail" >/dev/null 2>&1 || die 'pin-branch ruleset differs from the exact contract'; PIN_RULESET_ID=$id; pin_count=$((pin_count + 1)) ;;
    esac
  done <<<"$ids"
  ((main_count == 1 && image_create == 1 && image_immutable == 1 && action_create == 1 && action_immutable == 1 && pin_count == 1)) || die 'required repository rulesets are missing or duplicated'
}

ACTIVE_CANDIDATE_PR=$CANDIDATE_PR
ACTIVE_CANDIDATE_HEAD=
ACTIVE_BOT_LOGIN=$EXPECTED_BOT_LOGIN
ACTIVE_APP_ID=$EXPECTED_APP_ID
ACTIVE_TEAM_ID=$EXPECTED_TEAM_ID
REPOSITORY_ID=
REQUIRED_WORKFLOW_ID=
MAIN_RULESET_ID=
IMAGE_CREATION_ID=
IMAGE_IMMUTABILITY_ID=
ACTION_CREATION_ID=
ACTION_IMMUTABILITY_ID=
PIN_RULESET_ID=
CURRENT_PREREQUISITE=
CURRENT_ROTATION_HISTORY=
MAIN_SHA=
ENV_SNAPSHOTS=
IMMUTABLE_ENFORCED_BY_OWNER=

verify_common_policy() {
  local final_gate=$1 prerequisite_gate=$2 body value policies protection app_record installation_record team_record secret_record bot_id main_ref
  verify_policy_actor

  policy_get /orgs/stackpop/actions/permissions body
  safe_jq -e 'type == "object" and .sha_pinning_required == false and (.sha_pinning_required | type) == "boolean"' "$body" >/dev/null 2>&1 || die 'organization Actions SHA-pinning policy differs'
  policy_get /repos/stackpop/edgezero/actions/permissions body
  safe_jq -e 'type == "object" and .sha_pinning_required == false and (.sha_pinning_required | type) == "boolean"' "$body" >/dev/null 2>&1 || die 'repository Actions SHA-pinning policy differs'

  policy_get /repos/stackpop/edgezero body
  safe_jq -e 'type == "object" and .full_name == "stackpop/edgezero" and .owner.login == "stackpop" and .default_branch == "main" and .archived == false and (.id | type) == "number"' "$body" >/dev/null 2>&1 || die 'repository identity is invalid'
  number_at_path "$body" '["id"]' "$U64_MAX" REPOSITORY_ID

  policy_get /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_APP_ID body
  verify_named_time_record "$body" EDGEZERO_BUILD_CONTAINER_APP_ID "$EXPECTED_APP_ID" app_record
  ACTIVE_APP_ID=$(safe_jq -er '.value' "$body"); is_positive_decimal_at_most "$ACTIVE_APP_ID" "$U64_MAX" || die 'environment App id is invalid'
  policy_get /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID body
  verify_named_time_record "$body" EDGEZERO_BUILD_CONTAINER_APP_INSTALLATION_ID "$EXPECTED_INSTALLATION_ID" installation_record
  ACTIVE_INSTALLATION_ID=$(safe_jq -er '.value' "$body"); is_positive_decimal_at_most "$ACTIVE_INSTALLATION_ID" "$U64_MAX" || die 'environment installation id is invalid'
  policy_get /repos/stackpop/edgezero/environments/build-container-release/variables/EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID body
  verify_named_time_record "$body" EDGEZERO_BUILD_CONTAINER_RELEASE_TEAM_ID "$EXPECTED_TEAM_ID" team_record
  ACTIVE_TEAM_ID=$(safe_jq -er '.value' "$body"); is_positive_decimal_at_most "$ACTIVE_TEAM_ID" "$U64_MAX" || die 'environment team id is invalid'
  policy_get /repos/stackpop/edgezero/environments/build-container-release/secrets/EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY body
  verify_secret_time_record "$body" EDGEZERO_BUILD_CONTAINER_APP_PRIVATE_KEY secret_record
  ENV_SNAPSHOTS=$(safe_jq -cnS --argjson app "$app_record" --argjson installation "$installation_record" --argjson team "$team_record" --argjson secret "$secret_record" '[$app,$installation,$team,$secret]')

  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_GATE_SHA body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_GATE_SHA value
  [[ "$value" == "$final_gate" ]] || die 'active gate variable differs from final gate'
  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_RELEASE_STATE body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_RELEASE_STATE value
  [[ "$value" == enabled ]] || die 'build-container release state is not enabled'
  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_PUBLISHER_APP_ID value
  [[ "$value" == "$ACTIVE_APP_ID" ]] || die 'publisher App variable differs from environment App id'
  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_ID ACTIVE_BOT_ID
  is_positive_decimal_at_most "$ACTIVE_BOT_ID" "$U64_MAX" || die 'publisher bot id is invalid'
  [[ -z "$EXPECTED_BOT_ID" || "$ACTIVE_BOT_ID" == "$EXPECTED_BOT_ID" ]] || die 'publisher bot id differs from expected id'
  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_PUBLISHER_BOT_LOGIN ACTIVE_BOT_LOGIN
  if ! is_login "$ACTIVE_BOT_LOGIN" || [[ "$ACTIVE_BOT_LOGIN" != *'[bot]' ]]; then die 'publisher bot login is invalid'; fi
  [[ -z "$EXPECTED_BOT_LOGIN" || "$ACTIVE_BOT_LOGIN" == "$EXPECTED_BOT_LOGIN" ]] || die 'publisher bot login differs from expected login'
  policy_get /repos/stackpop/edgezero/actions/variables/EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE body
  extract_variable "$body" EDGEZERO_BUILD_CONTAINER_PUBLISHER_PREREQUISITE CURRENT_PREREQUISITE
  validate_record_string "$CURRENT_PREREQUISITE" "$prerequisite_gate" CURRENT_ROTATION_HISTORY

  verify_rulesets "$final_gate"

  policy_get /repos/stackpop/edgezero/environments/build-container-release body
  safe_jq -e --argjson team "$ACTIVE_TEAM_ID" '
    type == "object"
    and .deployment_branch_policy == {"custom_branch_policies":true,"protected_branches":false}
    and (.protection_rules | type) == "array" and (.protection_rules | length) == 1
    and .protection_rules[0].type == "required_reviewers"
    and .protection_rules[0].prevent_self_review == true
    and (.protection_rules[0].reviewers | type) == "array" and (.protection_rules[0].reviewers | length) > 0
    and any(.protection_rules[0].reviewers[]; .type == "Team" and .reviewer.id == $team)
  ' "$body" >/dev/null 2>&1 || die 'release environment reviewer policy differs'
  one_page_list '/repos/stackpop/edgezero/environments/build-container-release/deployment-branch-policies?per_page=100&page=1' branch_policies 1 policies
  safe_jq -e '.branch_policies == [{"id":31,"name":"build-container-v*","type":"tag"}] or (.branch_policies | length == 1 and .[0].name == "build-container-v*" and .[0].type == "tag" and (.[0].id | type) == "number")' "$policies" >/dev/null 2>&1 || die 'release environment tag policy differs'
  number_at_path "$policies" '["branch_policies",0,"id"]' "$U64_MAX" value
  policy_get /repos/stackpop/edgezero/environments/build-container-release/deployment_protection_rules protection
  safe_jq -e 'type == "object" and .total_count == 0 and (.total_count | type) == "number" and .custom_deployment_protection_rules == []' "$protection" >/dev/null 2>&1 || die 'release environment has a custom protection rule'
  number_at_path "$protection" '["total_count"]' "$U64_MAX" value true; [[ "$value" == 0 ]] || die 'release environment protection-rule count differs'

  policy_get /repos/stackpop/edgezero/immutable-releases body
  safe_jq -e 'type == "object" and .enabled == true and (.enabled | type) == "boolean" and (.enforced_by_owner | type) == "boolean"' "$body" >/dev/null 2>&1 || die 'immutable releases are not enabled'
  IMMUTABLE_ENFORCED_BY_OWNER=$(safe_jq -r '.enforced_by_owner' "$body")

  policy_get "/users/$ACTIVE_BOT_LOGIN" body
  safe_jq -e --arg login "$ACTIVE_BOT_LOGIN" --argjson id "$ACTIVE_BOT_ID" 'type == "object" and .login == $login and .id == $id and .type == "Bot"' "$body" >/dev/null 2>&1 || die 'publisher bot public identity differs'
  number_at_path "$body" '["id"]' "$U64_MAX" bot_id
  [[ "$bot_id" == "$ACTIVE_BOT_ID" ]] || die 'publisher bot numeric identity differs'

  policy_get /repos/stackpop/edgezero/git/ref/heads/main main_ref
  safe_jq -e 'type == "object" and .ref == "refs/heads/main" and .object.type == "commit" and (.object.sha | type) == "string"' "$main_ref" >/dev/null 2>&1 || die 'protected-main ref response is invalid'
  MAIN_SHA=$(safe_jq -er '.object.sha' "$main_ref")
  if ! is_sha "$MAIN_SHA" || ! commit_exists "$MAIN_SHA"; then die 'protected-main SHA is unavailable'; fi
  require_ancestor "$final_gate" "$MAIN_SHA" 'protected main'
  validate_manifest_and_codeowners "$final_gate" "$MAIN_SHA"
}

verify_exact_check_run() {
  local sha=$1 name=$2 expected_run=${3:-} body id app_id
  one_page_list "/repos/stackpop/edgezero/commits/$sha/check-runs?check_name=$name&filter=latest&app_id=15368&per_page=100&page=1" check_runs 1 body
  safe_jq -e --arg sha "$sha" --arg name "$name" --arg run "$expected_run" '
    .check_runs[0].name == $name and .check_runs[0].head_sha == $sha
    and .check_runs[0].status == "completed" and .check_runs[0].conclusion == "success"
    and .check_runs[0].app.id == 15368
    and ($run == "" or .check_runs[0].details_url == ("https://github.com/stackpop/edgezero/actions/runs/" + $run))
  ' "$body" >/dev/null 2>&1 || die "$name check-run identity differs"
  number_at_path "$body" '["check_runs",0,"id"]' "$U64_MAX" id
  number_at_path "$body" '["check_runs",0,"app","id"]' "$U64_MAX" app_id; [[ "$app_id" == 15368 ]] || die "$name check-run App id differs"
}

verify_run_detail() {
  local run_id=$1 attempt=$2 event=$3 head=$4 path=$5 result_name=$6 response api_id api_attempt
  policy_get "/repos/stackpop/edgezero/actions/runs/$run_id" response
  safe_jq -e --arg event "$event" --arg head "$head" --arg path "$path" '
    type == "object" and .event == $event and .head_sha == $head and .path == $path
    and .status == "completed" and .conclusion == "success"
    and (.created_at | type) == "string" and (.id | type) == "number" and (.run_attempt | type) == "number"
  ' "$response" >/dev/null 2>&1 || die 'workflow run identity or result differs'
  number_at_path "$response" '["id"]' "$U64_MAX" api_id; [[ "$api_id" == "$run_id" ]] || die 'workflow run id differs'
  number_at_path "$response" '["run_attempt"]' "$U32_MAX" api_attempt; [[ "$api_attempt" == "$attempt" ]] || die 'workflow run attempt differs'
  printf -v "$result_name" '%s' "$response"
}

verify_two_stable_jobs() {
  local run_id=$1 attempt=$2 head=$3 required_step=$4 body
  one_page_list "/repos/stackpop/edgezero/actions/runs/$run_id/attempts/$attempt/jobs?per_page=100&page=1" jobs 2 body
  safe_jq -e --arg head "$head" --arg step "$required_step" '
    ([.jobs[].name] | sort) == ["build-container-local","build-container-pin"]
    and all(.jobs[]; .head_sha == $head and .conclusion == "success")
    and ($step == "" or all(.jobs[]; ([.steps[] | select(.name == $step and .conclusion == "success")] | length) == 1))
  ' "$body" >/dev/null 2>&1 || die 'stable required jobs differ from the exact contract'
}

verify_smoke() {
  local run body title candidate_pr candidate_head created created_epoch snapshots updated completed completed_epoch
  policy_get "/repos/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID" run
  safe_jq -e '
    type == "object" and .event == "workflow_dispatch" and .path == ".github/workflows/build-container-ci.yml"
    and .status == "completed" and .conclusion == "success" and (.head_sha | type) == "string"
    and (.display_title | type) == "string" and (.created_at | type) == "string"
  ' "$run" >/dev/null 2>&1 || die 'credential-smoke run identity differs'
  number_at_path "$run" '["id"]' "$U64_MAX" id; [[ "$id" == "$SMOKE_RUN_ID" ]] || die 'credential-smoke run id differs'
  number_at_path "$run" '["run_attempt"]' "$U32_MAX" attempt; [[ "$attempt" == "$SMOKE_RUN_ATTEMPT" ]] || die 'credential-smoke attempt differs'
  SMOKE_HEAD=$(safe_jq -er '.head_sha' "$run")
  if ! is_sha "$SMOKE_HEAD" || ! commit_exists "$SMOKE_HEAD"; then die 'credential-smoke head is unavailable'; fi
  require_ancestor "$GATE_SHA" "$SMOKE_HEAD" 'credential-smoke gate'
  validate_manifest_and_codeowners "$GATE_SHA" "$SMOKE_HEAD"
  title=$(safe_jq -er '.display_title' "$run")
  [[ "$title" =~ ^build-container-release-preflight[[:space:]]pr=([1-9][0-9]*)[[:space:]]repo=stackpop/edgezero[[:space:]]sha=([0-9a-f]{40})$ ]] || die 'credential-smoke title is invalid'
  candidate_pr=${BASH_REMATCH[1]}; candidate_head=${BASH_REMATCH[2]}
  if ! is_positive_decimal_at_most "$candidate_pr" "$U64_MAX" || ! commit_exists "$candidate_head"; then die 'credential-smoke candidate identity is invalid'; fi
  [[ -z "$ACTIVE_CANDIDATE_PR" || "$candidate_pr" == "$ACTIVE_CANDIDATE_PR" ]] || die 'credential-smoke candidate PR differs'
  [[ -z "$ACTIVE_CANDIDATE_HEAD" || "$candidate_head" == "$ACTIVE_CANDIDATE_HEAD" ]] || die 'credential-smoke candidate head differs'
  ACTIVE_CANDIDATE_PR=$candidate_pr; ACTIVE_CANDIDATE_HEAD=$candidate_head
  policy_get "/repos/stackpop/edgezero/pulls/$ACTIVE_CANDIDATE_PR" body
  safe_jq -e --argjson pr "$ACTIVE_CANDIDATE_PR" --arg head "$ACTIVE_CANDIDATE_HEAD" '
    .number == $pr and .base.ref == "main" and .base.repo.full_name == "stackpop/edgezero"
    and .head.repo.full_name == "stackpop/edgezero" and .head.sha == $head
  ' "$body" >/dev/null 2>&1 || die 'credential-smoke candidate PR identity differs'
  verify_exact_check_run "$ACTIVE_CANDIDATE_HEAD" build-container-release-preflight "$SMOKE_RUN_ID"
  one_page_list "/repos/stackpop/edgezero/actions/runs/$SMOKE_RUN_ID/attempts/$SMOKE_RUN_ATTEMPT/jobs?per_page=100&page=1" jobs 1 body
  safe_jq -e --arg head "$SMOKE_HEAD" '
    .jobs[0].name == "build-container-release-preflight" and .jobs[0].head_sha == $head and .jobs[0].conclusion == "success"
    and ([.jobs[0].steps[] | select(.name == "assert-exact-g-dispatch-context" and .conclusion == "success")] | length) == 1
    and (.jobs[0].completed_at | type) == "string"
  ' "$body" >/dev/null 2>&1 || die 'credential-smoke job differs from the exact contract'
  number_at_path "$body" '["jobs",0,"id"]' "$U64_MAX" SMOKE_JOB_ID
  created=$(safe_jq -er '.created_at' "$run"); validate_utc "$created" 'credential-smoke creation time' created_epoch
  completed=$(safe_jq -er '.jobs[0].completed_at' "$body"); validate_utc "$completed" 'credential-smoke completion time' completed_epoch
  ((created_epoch <= completed_epoch)) || die 'credential-smoke timestamps are inconsistent'
  snapshots=$(safe_jq -cr '.[]' <<<"$ENV_SNAPSHOTS")
  while IFS= read -r record || [[ -n "$record" ]]; do
    [[ -z "$record" ]] && continue; updated=$(safe_jq -er '.updated_at' <<<"$record"); validate_utc "$updated" 'credential snapshot update time' updated_epoch
    ((updated_epoch < created_epoch)) || die 'credential record was not fixed before smoke creation'
  done <<<"$snapshots"
  SMOKE_CREATED=$created; SMOKE_COMPLETED=$completed
}

base64url() {
  env -i PATH="$PATH" LC_ALL=C openssl base64 -A 2>/dev/null | tr '+/' '-_' | tr -d '='
}

mint_app_jwt() {
  local header payload signing signature iat exp
  iat=$((NOW_EPOCH - 30)); exp=$((NOW_EPOCH + 540))
  header=$(printf '%s' '{"alg":"RS256","typ":"JWT"}' | base64url) || tool_die 'cannot encode App JWT header'
  payload=$(printf '{"exp":%s,"iat":%s,"iss":%s}' "$exp" "$iat" "$EXPECTED_APP_ID" | base64url) || tool_die 'cannot encode App JWT payload'
  signing="$header.$payload"
  signature=$(printf '%s' "$signing" | env -i PATH="$PATH" LC_ALL=C openssl dgst -sha256 -sign "$APP_KEY_FILE" 2>/dev/null | base64url) || die 'cannot sign App JWT'
  APP_JWT="$signing.$signature"
  valid_credential "$APP_JWT" || die 'signed App JWT is malformed'
}

revoke_token() {
  local kind=$1 token=$2 ignored=''
  api_request "$kind" "$token" DELETE /installation/token 204 ignored
  : "$ignored"
}

verify_app_probes() {
  local body request token repos expires expires_epoch app_id installation_id
  mint_app_jwt
  api_request app-jwt "$APP_JWT" GET /app 200 body
  safe_jq -e --arg slug "${ACTIVE_BOT_LOGIN%\[bot\]}" '.slug == $slug and (.id | type) == "number"' "$body" >/dev/null 2>&1 || die 'GitHub App identity differs'
  number_at_path "$body" '["id"]' "$U64_MAX" app_id; [[ "$app_id" == "$EXPECTED_APP_ID" ]] || die 'GitHub App id differs'
  api_request app-jwt "$APP_JWT" GET "/app/installations/$EXPECTED_INSTALLATION_ID" 200 body
  safe_jq -e '
    .account.login == "stackpop" and .account.type == "Organization" and .repository_selection == "selected"
    and .suspended_at == null and .permissions == {"contents":"write","metadata":"read","pull_requests":"write"}
    and (.id | type) == "number"
  ' "$body" >/dev/null 2>&1 || die 'GitHub App installation differs'
  number_at_path "$body" '["id"]' "$U64_MAX" installation_id; [[ "$installation_id" == "$EXPECTED_INSTALLATION_ID" ]] || die 'GitHub App installation id differs'

  new_temp request app-request; printf '%s' '{"permissions":{"metadata":"read"}}' >"$request"
  api_request app-jwt "$APP_JWT" POST "/app/installations/$EXPECTED_INSTALLATION_ID/access_tokens" 201 body "$request"
  safe_jq -e 'type == "object" and (.token | type) == "string" and (.token | test("^[A-Za-z0-9_.-]{8,512}$"))' "$body" >/dev/null 2>&1 || die 'metadata-audit token is malformed'
  AUDIT_TOKEN=$(safe_jq -er '.token' "$body"); AUDIT_REVOKED=false
  safe_jq -e '.permissions == {"metadata":"read"} and (.expires_at | type) == "string" and ((.expires_at | test("[\\r\\n]")) | not)' "$body" >/dev/null 2>&1 || die 'installation metadata-audit token response differs'
  expires=$(safe_jq -er '.expires_at' "$body"); validate_utc "$expires" 'metadata-audit token expiration' expires_epoch; ((expires_epoch > NOW_EPOCH)) || die 'metadata-audit token is expired'
  paginate installation-audit '/installation/repositories?per_page=100&' repositories true repos
  safe_jq -e --argjson repo "$REPOSITORY_ID" 'length == 1 and .[0].id == $repo and .[0].full_name == "stackpop/edgezero"' "$repos" >/dev/null 2>&1 || die 'installation-wide repository selection differs'
  revoke_token installation-audit "$AUDIT_TOKEN"; AUDIT_REVOKED=true

  new_temp request app-request; printf '{"repository_ids":[%s],"permissions":{"contents":"write","pull_requests":"write"}}' "$REPOSITORY_ID" >"$request"
  api_request app-jwt "$APP_JWT" POST "/app/installations/$EXPECTED_INSTALLATION_ID/access_tokens" 201 body "$request"
  safe_jq -e 'type == "object" and (.token | type) == "string" and (.token | test("^[A-Za-z0-9_.-]{8,512}$"))' "$body" >/dev/null 2>&1 || die 'publisher-probe token is malformed'
  PROBE_TOKEN=$(safe_jq -er '.token' "$body"); PROBE_REVOKED=false
  safe_jq -e '.permissions == {"contents":"write","metadata":"read","pull_requests":"write"} and (.expires_at | type) == "string" and ((.expires_at | test("[\\r\\n]")) | not)' "$body" >/dev/null 2>&1 || die 'publisher-probe token response differs'
  [[ "$PROBE_TOKEN" != "$AUDIT_TOKEN" ]] || die 'App probe tokens are not distinct'
  paginate publisher-probe '/installation/repositories?per_page=100&' repositories true repos
  safe_jq -e --argjson repo "$REPOSITORY_ID" 'length == 1 and .[0].id == $repo and .[0].full_name == "stackpop/edgezero"' "$repos" >/dev/null 2>&1 || die 'publisher-probe repository selection differs'
  api_request publisher-probe "$PROBE_TOKEN" GET /repos/stackpop/edgezero 200 body
  safe_jq -e --argjson repo "$REPOSITORY_ID" '.id == $repo and .full_name == "stackpop/edgezero"' "$body" >/dev/null 2>&1 || die 'publisher-probe repository identity differs'
  revoke_token publisher-probe "$PROBE_TOKEN"; PROBE_REVOKED=true
  APP_PROBE_EVIDENCE=$(safe_jq -cnS --arg app "$EXPECTED_APP_ID" --arg installation "$EXPECTED_INSTALLATION_ID" --arg repo "$REPOSITORY_ID" '{"app-id":$app,"installation-id":$installation,"installation-metadata-audit-revoked":true,"publisher-probe-revoked":true,"repository-id":$repo}')
  APP_JWT=; AUDIT_TOKEN=; PROBE_TOKEN=
}

verify_package() {
  local body
  package_get /user body
  safe_jq -e --arg login "$PACKAGE_AUDITOR_LOGIN" '.login == $login and .type == "User" and (.id | type) == "number"' "$body" >/dev/null 2>&1 || die 'package token identity differs'
  package_get "/orgs/stackpop/memberships/$PACKAGE_AUDITOR_LOGIN" body
  safe_jq -e '.state == "active" and .role == "admin"' "$body" >/dev/null 2>&1 || die 'package auditor is not an active organization owner'
  if [[ "$PACKAGE_STATE" == absent ]]; then
    paginate package '/orgs/stackpop/packages?package_type=container&per_page=100&' '' false body
    safe_jq -e 'all(.[]; .name != "edgezero-build-app-cli")' "$body" >/dev/null 2>&1 || die 'container package is not absent'
  else
    package_get /orgs/stackpop/packages/container/edgezero-build-app-cli body
    safe_jq -e --argjson repo "$REPOSITORY_ID" '.name == "edgezero-build-app-cli" and .package_type == "container" and .visibility == "public" and .repository.id == $repo and .repository.full_name == "stackpop/edgezero"' "$body" >/dev/null 2>&1 || die 'container package is not public and repository-linked'
  fi
}

build_policy_evidence() {
  local mode=$1 gate=$2 admin=$3 app_probes=$4 release=$5 rotation=$6
  safe_jq -cnS \
    --arg audited "$AUDITED_AT" --arg mode "$mode" --arg gate "$gate" \
    --arg policy_login "$POLICY_LOGIN" --arg policy_user_id "$POLICY_USER_ID" \
    --arg policy_token_id "$POLICY_TOKEN_ID" --arg policy_reviewed "$POLICY_REVIEWED" \
    --arg policy_expires "$POLICY_EXPIRES" --arg policy_png "$POLICY_SCREENSHOT" \
    --arg repository_id "$REPOSITORY_ID" --arg required_id "$REQUIRED_WORKFLOW_ID" \
    --arg required_sha "$gate" --arg main_id "$MAIN_RULESET_ID" --arg image_create "$IMAGE_CREATION_ID" \
    --arg image_immutable "$IMAGE_IMMUTABILITY_ID" --arg action_create "$ACTION_CREATION_ID" \
    --arg action_immutable "$ACTION_IMMUTABILITY_ID" --arg pin_id "$PIN_RULESET_ID" \
    --arg main_sha "$MAIN_SHA" --arg immutable_owner "$IMMUTABLE_ENFORCED_BY_OWNER" \
    --argjson snapshots "$ENV_SNAPSHOTS" --argjson admin "$admin" --argjson app "$app_probes" \
    --argjson release "$release" --argjson rotation "$rotation" '
      {
        "audited-at":$audited,
        "credentials":{
          "app-probes":$app,
          "environment-snapshots":$snapshots,
          "policy-auditor":{"expires-at":$policy_expires,"login":$policy_login,"reviewed-at":$policy_reviewed,"screenshot-sha256":$policy_png,"token-id":$policy_token_id,"user-id":$policy_user_id}
        },
        "environment":{"administrator-bypass":$admin,"name":"build-container-release","release-state":"enabled","tag-policy":{"name":"build-container-v*","type":"tag"}},
        "gate-sha":$gate,
        "mode":$mode,
        "policy":{
          "action-tag-rulesets":{"creation-id":$action_create,"immutability-id":$action_immutable},
          "image-tag-rulesets":{"creation-id":$image_create,"immutability-id":$image_immutable},
          "immutable-releases":{"enabled":true,"enforced-by-owner":($immutable_owner == "true")},
          "main-ref":$main_sha,
          "main-ruleset-id":$main_id,
          "pin-branch-ruleset-id":$pin_id,
          "repository-id":$repository_id,
          "required-workflow-ruleset-id":$required_id,
          "required-workflow-sha":$required_sha,
          "sha-pinning-required":false
        },
        "release":$release,
        "rotation":$rotation,
        "schema-version":1
      }
    '
}

admin_evidence() {
  if [[ "$MODE" == configuration || "$MODE" == release ]]; then
    safe_jq -cnS --arg basename "${ADMIN_PNG##*/}" --arg head "$ACTIVE_CANDIDATE_HEAD" --arg digest "$ADMIN_DIGEST" --arg reviewer "$ADMIN_REVIEWER" --arg reviewed "$ADMIN_REVIEWED_AT" '{allowed:false,basename:$basename,"candidate-head-sha":$head,"reviewed-at":$reviewed,reviewer:$reviewer,sha256:$digest,verification:"manual-ui"}'
  else
    printf 'null'
  fi
}

verify_release_source() {
  local body request changed canonical
  policy_get "/repos/stackpop/edgezero/pulls/$CANDIDATE_PR" body
  safe_jq -e --argjson pr "$CANDIDATE_PR" --arg head "$ACTIVE_CANDIDATE_HEAD" --arg source "$SOURCE_REVISION" '
    .number == $pr and .state == "closed" and .merged == true and .merge_commit_sha == $source
    and .head.sha == $head and .head.repo.full_name == "stackpop/edgezero"
    and .base.ref == "main" and .base.repo.full_name == "stackpop/edgezero"
  ' "$body" >/dev/null 2>&1 || die 'release candidate PR is not the exact merged source'
  require_ancestor "$SOURCE_REVISION" "$MAIN_SHA" 'release source on protected main'
  changed=$(repo_git diff --name-only "$GATE_SHA" "$SOURCE_REVISION" --) || die 'cannot inspect release-source paths'
  [[ "$changed" == .github/docker/build-app-cli/release-request.json ]] || die 'release source changes more than the isolated release request'
  new_temp request release-request
  repo_git show "$SOURCE_REVISION:.github/docker/build-app-cli/release-request.json" >"$request" 2>/dev/null || die 'release request is absent'
  validate_raw_json_keys "$request" 'release request'
  safe_jq -e --arg gate "$GATE_SHA" 'type == "object" and keys == ["gate-sha","provenance-protocol","release-tag"] and ."gate-sha" == $gate and ."provenance-protocol" == 1 and (."provenance-protocol" | type) == "number" and (."release-tag" | type) == "string" and (."release-tag" | test("^build-container-v[1-9][0-9]*$"))' "$request" >/dev/null 2>&1 || die 'release request has the wrong exact shape'
  canonical=$(safe_jq -cS . "$request")
  [[ "$(file_size "$request")" == "${#canonical}" && "$canonical" == "$(<"$request")" ]] || die 'release request is not exact JCS'
  RELEASE_TAG=$(safe_jq -er '."release-tag"' "$request")

  verify_run_detail "$MERGE_GROUP_RUN_ID" "$MERGE_GROUP_RUN_ATTEMPT" merge_group "$MERGE_GROUP_SHA" "$WORKFLOW_PATH" body
  verify_two_stable_jobs "$MERGE_GROUP_RUN_ID" "$MERGE_GROUP_RUN_ATTEMPT" "$MERGE_GROUP_SHA" ''
  verify_exact_check_run "$MERGE_GROUP_SHA" build-container-local
  verify_exact_check_run "$MERGE_GROUP_SHA" build-container-pin
  verify_run_detail "$PUSH_RUN_ID" "$PUSH_RUN_ATTEMPT" push "$SOURCE_REVISION" "$WORKFLOW_PATH" body
  verify_two_stable_jobs "$PUSH_RUN_ID" "$PUSH_RUN_ATTEMPT" "$SOURCE_REVISION" assert-exact-main-push-context
}

rotation_history_snapshot() {
  local result_name=$1 list count index id attempt number created line rows records='[]' next seen_ids='|' seen_numbers='|' unused_epoch=''
  paginate policy '/repos/stackpop/edgezero/actions/workflows/rotate-build-container-gate.yml/runs?event=workflow_dispatch&per_page=100&' workflow_runs true list
  count=$(safe_jq -r 'length' "$list")
  if [[ ! "$count" =~ ^[0-9]+$ ]] || ((count == 0)); then die 'rotation history is empty'; fi
  new_temp rows rotation-rows; : >"$rows"
  index=0
  while ((index < count)); do
    number_at_path "$list" "[$index,\"id\"]" "$U64_MAX" id
    number_at_path "$list" "[$index,\"run_attempt\"]" "$U32_MAX" attempt
    number_at_path "$list" "[$index,\"run_number\"]" "$U64_MAX" number
    created=$(safe_jq -er --argjson index "$index" '.[$index].created_at | select(type == "string" and ((test("[\\r\\n]")) | not))' "$list" 2>/dev/null) || die 'rotation history creation time is missing'
    validate_utc "$created" 'rotation run creation time' unused_epoch
    ((unused_epoch <= NOW_EPOCH)) || die 'rotation run creation time is future'
    case "$seen_ids" in *"|$id|"*) die 'rotation history repeats a run id' ;; esac; seen_ids="$seen_ids$id|"
    case "$seen_numbers" in *"|$number|"*) die 'rotation history repeats a run number' ;; esac; seen_numbers="$seen_numbers$number|"
    line=$(safe_jq -cnS --arg attempt "$attempt" --arg id "$id" --arg number "$number" '{"run-attempt":$attempt,"run-id":$id,"run-number":$number}')
    printf '%02d\t%s\t%s\t%s\t%s\n' "${#number}" "$number" "$id" "$created" "$line" >>"$rows"
    index=$((index + 1))
  done
  sort -t $'\t' -k1,1n -k2,2 "$rows" >"$rows.sorted" || tool_die 'cannot order rotation history'
  temporary_files+=("$rows.sorted")
  while IFS=$'\t' read -r _ number id created line; do
    next=$(safe_jq -cnS --argjson records "$records" --argjson line "$line" '$records + [$line]')
    records=$next
    SELECTED_RUN_NUMBER=$number; SELECTED_RUN_ID=$id; SELECTED_RUN_ATTEMPT=$attempt; SELECTED_CREATED_AT=$created
  done <"$rows.sorted"
  printf -v "$result_name" '%s' "$records"
}

parse_rotation_approval() {
  local approvals=$1 completed_at=$2 run_actor=$3 comment reviewer api_reviewed first second first_json second_json canonical
  local first_file second_file
  local run_created_epoch audited_epoch reviewed_epoch completed_epoch evidence_digest policy_digest
  safe_jq -e --arg prefix "$ROTATION_PREFIX" --arg environment "$ROTATION_ENVIRONMENT" '
    type == "array"
    and ([.[] | select((.comment | type) == "string" and (.comment | startswith($prefix)))]) as $records
    | ($records | length) == 1
      and ($records[0].user | type) == "object"
      and ($records[0].user.login | type) == "string"
      and (($records[0].user.login | test("[\\r\\n]")) | not)
      and ($records[0].reviewed_at | type) == "string"
      and (($records[0].reviewed_at | test("[\\r\\n]")) | not)
      and (($records[0].comment | contains("\r")) | not)
      and ($records[0].comment | split("\n") | length) == 2
      and ($records[0].comment | endswith("\n") | not)
      and $records[0].state == "approved"
      and $records[0].environment_name == $environment
  ' "$approvals" >/dev/null 2>&1 || die 'rotation approval record is missing, duplicated, or malformed'
  comment=$(safe_jq -er --arg prefix "$ROTATION_PREFIX" '.[] | select(.comment | startswith($prefix)) | .comment' "$approvals")
  reviewer=$(safe_jq -er --arg prefix "$ROTATION_PREFIX" '.[] | select(.comment | startswith($prefix)) | .user.login' "$approvals")
  api_reviewed=$(safe_jq -er --arg prefix "$ROTATION_PREFIX" '.[] | select(.comment | startswith($prefix)) | .reviewed_at' "$approvals")
  if ! is_login "$reviewer" || [[ "$reviewer" == "$run_actor" || "$reviewer" == "$POLICY_LOGIN" ]]; then
    die 'rotation reviewer identity is not independent'
  fi
  [[ "$comment" != *$'\n'$'\n'* && "$(printf '%s' "$comment" | grep -c '^')" == 2 ]] || die 'rotation approval must contain exactly two lines'
  first=${comment%%$'\n'*}; second=${comment#*$'\n'}
  [[ "$first" == "$ROTATION_PREFIX"* && "$second" == "$POLICY_PREFIX"* ]] || die 'rotation approval prefixes are invalid'
  first_json=${first#"$ROTATION_PREFIX"}; second_json=${second#"$POLICY_PREFIX"}
  new_temp first_file rotation-receipt; new_temp second_file rotation-policy
  printf '%s' "$first_json" >"$first_file"; printf '%s' "$second_json" >"$second_file"
  validate_raw_json_keys "$first_file" 'rotation receipt'; validate_raw_json_keys "$second_file" 'rotation policy receipt'
  canonical=$(safe_jq -cS . "$first_file"); [[ "$canonical" == "$first_json" ]] || die 'rotation receipt is not exact JCS'
  canonical=$(safe_jq -cS . "$second_file"); [[ "$canonical" == "$second_json" ]] || die 'rotation policy receipt is not exact JCS'
  safe_jq -e '
    type == "object" and keys == ["evidence-sha256","head-sha","lock-run-id","new-gate-sha","old-gate-sha","result","reviewed-at"]
    and all(."evidence-sha256",."head-sha",."lock-run-id",."new-gate-sha",."old-gate-sha",.result,."reviewed-at"; type == "string")
  ' "$first_file" >/dev/null 2>&1 || die 'rotation receipt shape is invalid'
  safe_jq -e '
    type == "object" and keys == ["audited-at","dispatch-sha","gate-sha","head-sha","lock-run-attempt","lock-run-id","policy-sha256","release-state","required-workflow-sha"]
    and all(."audited-at",."dispatch-sha",."gate-sha",."head-sha",."lock-run-attempt",."lock-run-id",."policy-sha256",."release-state",."required-workflow-sha"; type == "string")
  ' "$second_file" >/dev/null 2>&1 || die 'rotation policy receipt shape is invalid'
  evidence_digest=$(safe_jq -er '."evidence-sha256"' "$first_file")
  [[ "$evidence_digest" == "sha256:$(hash_bytes "$second_json")" ]] || die 'rotation nested evidence digest differs'
  policy_digest=$(safe_jq -er '."policy-sha256"' "$second_file"); [[ "$policy_digest" =~ ^sha256:[0-9a-f]{64}$ ]] || die 'rotation policy evidence digest is invalid'
  RECEIPT_HEAD=$(safe_jq -er '."head-sha"' "$first_file"); RECEIPT_OLD_GATE=$(safe_jq -er '."old-gate-sha"' "$first_file"); RECEIPT_NEW_GATE=$(safe_jq -er '."new-gate-sha"' "$first_file"); RECEIPT_RESULT=$(safe_jq -er '.result' "$first_file")
  RECEIPT_REVIEWED_AT=$(safe_jq -er '."reviewed-at"' "$first_file"); RECEIPT_EVIDENCE_DIGEST=$evidence_digest
  RECEIPT_DISPATCH=$(safe_jq -er '."dispatch-sha"' "$second_file"); RECEIPT_GATE=$(safe_jq -er '."gate-sha"' "$second_file"); RECEIPT_REQUIRED_GATE=$(safe_jq -er '."required-workflow-sha"' "$second_file"); RECEIPT_AUDITED_AT=$(safe_jq -er '."audited-at"' "$second_file")
  [[ "$(safe_jq -er '."head-sha"' "$second_file")" == "$RECEIPT_HEAD" ]] || die 'rotation receipt head values differ'
  [[ "$(safe_jq -er '."lock-run-id"' "$first_file")" == "$LOCK_RUN_ID" && "$(safe_jq -er '."lock-run-id"' "$second_file")" == "$LOCK_RUN_ID" ]] || die 'rotation receipt run id differs'
  [[ "$(safe_jq -er '."lock-run-attempt"' "$second_file")" == "$LOCK_RUN_ATTEMPT" ]] || die 'rotation receipt attempt differs'
  [[ "$(safe_jq -er '."release-state"' "$second_file")" == enabled ]] || die 'rotation receipt release state differs'
  [[ "$api_reviewed" == "$RECEIPT_REVIEWED_AT" ]] || die 'rotation approval timestamp differs from comment'
  validate_utc "$SELECTED_CREATED_AT" 'rotation creation time' run_created_epoch
  validate_utc "$RECEIPT_AUDITED_AT" 'rotation audit time' audited_epoch
  validate_utc "$RECEIPT_REVIEWED_AT" 'rotation review time' reviewed_epoch
  validate_utc "$completed_at" 'rotation completion time' completed_epoch
  ((run_created_epoch <= audited_epoch && audited_epoch <= reviewed_epoch && reviewed_epoch <= completed_epoch && completed_epoch - audited_epoch <= 900 && completed_epoch - reviewed_epoch <= 900)) || die 'rotation receipt time ordering or freshness differs'
  [[ "$RECEIPT_RESULT" == activated || "$RECEIPT_RESULT" == rolled-back ]] || die 'rotation receipt result is invalid'
  for value in "$RECEIPT_HEAD" "$RECEIPT_OLD_GATE" "$RECEIPT_NEW_GATE" "$RECEIPT_DISPATCH" "$RECEIPT_GATE" "$RECEIPT_REQUIRED_GATE"; do is_sha "$value" || die 'rotation receipt SHA is invalid'; done
}

verify_rotation_complete() {
  local first_history second_history first_detail second_detail jobs approvals run_actor completed_at selected_id selected_attempt selected_number selected_created selected_head
  local id attempt number first_identity second_identity
  rotation_history_snapshot first_history
  selected_id=$SELECTED_RUN_ID; selected_attempt=$SELECTED_RUN_ATTEMPT; selected_number=$SELECTED_RUN_NUMBER; selected_created=$SELECTED_CREATED_AT
  [[ "$selected_id" == "$LOCK_RUN_ID" ]] || die 'selected rotation is not the requested latest run'
  [[ "$selected_attempt" == "$LOCK_RUN_ATTEMPT" ]] || die 'selected rotation attempt differs'
  policy_get "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID" first_detail
  safe_jq -e --arg head "$(safe_jq -er '.head_sha' "$first_detail")" --arg path "$ROTATION_PATH" '.event == "workflow_dispatch" and .path == $path and .status == "completed" and .conclusion == "success" and (.actor.login | type) == "string" and (.run_number | type) == "number" and (.run_attempt | type) == "number"' "$first_detail" >/dev/null 2>&1 || die 'selected rotation run is not completed successfully'
  number_at_path "$first_detail" '["id"]' "$U64_MAX" id; [[ "$id" == "$LOCK_RUN_ID" ]] || die 'selected rotation detail id differs'
  number_at_path "$first_detail" '["run_attempt"]' "$U32_MAX" attempt; [[ "$attempt" == "$LOCK_RUN_ATTEMPT" ]] || die 'selected rotation detail attempt differs'
  [[ "$attempt" == "$selected_attempt" ]] || die 'selected rotation history attempt differs from detail'
  number_at_path "$first_detail" '["run_number"]' "$U64_MAX" number; [[ "$number" == "$selected_number" ]] || die 'selected rotation detail number differs'
  [[ "$(safe_jq -er '.created_at' "$first_detail")" == "$selected_created" ]] || die 'selected rotation detail creation time differs'
  selected_head=$(safe_jq -er '.head_sha' "$first_detail"); is_sha "$selected_head" || die 'selected rotation head SHA is invalid'
  run_actor=$(safe_jq -er '.actor.login' "$first_detail"); is_login "$run_actor" || die 'rotation run actor is invalid'
  one_page_list "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/attempts/$LOCK_RUN_ATTEMPT/jobs?per_page=100&page=1" jobs 2 jobs
  safe_jq -e --arg head "$selected_head" '
    ([.jobs[].name] | sort) == ["acquire-rotation-lock","wait-for-rotation-review"]
    and all(.jobs[]; .conclusion == "success" and .head_sha == $head)
    and ([.jobs[] | select(.name == "wait-for-rotation-review") | .steps[] | select(.name == "assert-exact-rotation-context" and .conclusion == "success")] | length) == 1
  ' "$jobs" >/dev/null 2>&1 || die 'rotation jobs differ from the exact contract'
  completed_at=$(safe_jq -er '.jobs[] | select(.name == "wait-for-rotation-review") | .completed_at' "$jobs")
  policy_get "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID/approvals" approvals
  [[ -z "$LAST_LINK" ]] || die 'rotation approvals endpoint returned continuation'
  parse_rotation_approval "$approvals" "$completed_at" "$run_actor"
  [[ "$RECEIPT_DISPATCH" == "$selected_head" ]] || die 'rotation dispatch SHA differs from run'
  [[ "$RECEIPT_HEAD" == "$MAIN_SHA" && "$RECEIPT_GATE" == "$GATE_SHA" && "$RECEIPT_REQUIRED_GATE" == "$GATE_SHA" ]] || die 'rotation final pointers differ from active policy'
  require_ancestor "$RECEIPT_DISPATCH" "$RECEIPT_HEAD" 'rotation receipt head'
  validate_manifest_and_codeowners "$GATE_SHA" "$RECEIPT_HEAD"

  rotation_history_snapshot second_history
  [[ "$first_history" == "$second_history" && "$SELECTED_RUN_ID" == "$selected_id" && "$SELECTED_RUN_ATTEMPT" == "$selected_attempt" && "$SELECTED_RUN_NUMBER" == "$selected_number" && "$SELECTED_CREATED_AT" == "$selected_created" ]] || die 'rotation history changed during audit'
  policy_get "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID" second_detail
  first_identity=$(safe_jq -cS '{conclusion,created_at,event,head_sha,id,path,run_attempt,run_number,status}' "$first_detail")
  second_identity=$(safe_jq -cS '{conclusion,created_at,event,head_sha,id,path,run_attempt,run_number,status}' "$second_detail")
  [[ "$first_identity" == "$second_identity" ]] || die 'selected rotation detail changed during audit'
  ROTATION_HISTORY_DIGEST="sha256:$(hash_bytes "$second_history")"
  ROTATION_CREATED_AT=$SELECTED_CREATED_AT
}

AUDITED_AT=$(safe_jq -nr --argjson epoch "$NOW_EPOCH" '$epoch | strftime("%Y-%m-%dT%H:%M:%SZ")') || tool_die 'cannot format audit time'
ADMIN_JSON=null
APP_PROBE_EVIDENCE=null
RELEASE_EVIDENCE=null
ROTATION_EVIDENCE=null

case "$MODE" in
  configuration)
    verify_common_policy "$GATE_SHA" "$GATE_SHA"
    verify_smoke
    ADMIN_JSON=$(admin_evidence)
    verify_app_probes
    RELEASE_EVIDENCE=$(safe_jq -cnS --arg pr "$ACTIVE_CANDIDATE_PR" --arg head "$ACTIVE_CANDIDATE_HEAD" --arg smoke "$SMOKE_RUN_ID" --arg attempt "$SMOKE_RUN_ATTEMPT" --arg job "$SMOKE_JOB_ID" --arg created "$SMOKE_CREATED" --arg completed "$SMOKE_COMPLETED" '{"candidate-head":$head,"candidate-pr":$pr,"credential-smoke":{"completed-at":$completed,"created-at":$created,"job-id":$job,"run-attempt":$attempt,"run-id":$smoke,"run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$smoke)}}')
    EVIDENCE=$(build_policy_evidence configuration "$GATE_SHA" "$ADMIN_JSON" "$APP_PROBE_EVIDENCE" "$RELEASE_EVIDENCE" null)
    ;;
  release)
    verify_common_policy "$GATE_SHA" "$GATE_SHA"
    verify_smoke
    ADMIN_JSON=$(admin_evidence)
    verify_app_probes
    verify_release_source
    verify_package
    RELEASE_EVIDENCE=$(safe_jq -cnS --arg pr "$CANDIDATE_PR" --arg url "$EVIDENCE_URL" --arg source "$SOURCE_REVISION" --arg tag "$RELEASE_TAG" --arg merge_sha "$MERGE_GROUP_SHA" --arg merge_id "$MERGE_GROUP_RUN_ID" --arg merge_attempt "$MERGE_GROUP_RUN_ATTEMPT" --arg smoke_id "$SMOKE_RUN_ID" --arg smoke_attempt "$SMOKE_RUN_ATTEMPT" --arg smoke_job "$SMOKE_JOB_ID" --arg push_id "$PUSH_RUN_ID" --arg push_attempt "$PUSH_RUN_ATTEMPT" --arg package "$PACKAGE_STATE" --arg package_login "$PACKAGE_AUDITOR_LOGIN" '{"candidate-pr":$pr,"evidence-url":$url,"merge-group":{"run-attempt":$merge_attempt,"run-id":$merge_id,"run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$merge_id),sha:$merge_sha},"package":{"auditor-login":$package_login,state:$package},"push":{"run-attempt":$push_attempt,"run-id":$push_id,"run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$push_id)},"release-tag":$tag,"smoke":{"job-id":$smoke_job,"run-attempt":$smoke_attempt,"run-id":$smoke_id,"run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$smoke_id)},"source-revision":$source}')
    EVIDENCE=$(build_policy_evidence release "$GATE_SHA" "$ADMIN_JSON" "$APP_PROBE_EVIDENCE" "$RELEASE_EVIDENCE" null)
    ;;
  rotation-review)
    if [[ "$ROTATION_RESULT" == activated ]]; then FINAL_GATE=$NEW_GATE_SHA; else FINAL_GATE=$OLD_GATE_SHA; fi
    verify_common_policy "$FINAL_GATE" "$OLD_GATE_SHA"
    [[ "$MAIN_SHA" == "$FINAL_HEAD_SHA" ]] || die 'rotation final head differs from protected main'
    policy_get "/repos/stackpop/edgezero/actions/runs/$LOCK_RUN_ID" LOCK_DETAIL
    safe_jq -e --arg dispatch "$DISPATCH_SHA" --arg path "$ROTATION_PATH" --arg actor "$OPERATOR_LOGIN" '
      .event == "workflow_dispatch" and .path == $path and .head_sha == $dispatch
      and .status == "in_progress" and .conclusion == null and (.actor.login | type) == "string"
    ' "$LOCK_DETAIL" >/dev/null 2>&1 || die 'waiting rotation run identity differs'
    number_at_path "$LOCK_DETAIL" '["id"]' "$U64_MAX" id; [[ "$id" == "$LOCK_RUN_ID" ]] || die 'waiting rotation run id differs'
    number_at_path "$LOCK_DETAIL" '["run_attempt"]' "$U32_MAX" attempt; [[ "$attempt" == "$LOCK_RUN_ATTEMPT" ]] || die 'waiting rotation attempt differs'
    require_ancestor "$DISPATCH_SHA" "$FINAL_HEAD_SHA" 'rotation final head'
    validate_manifest_and_codeowners "$FINAL_GATE" "$FINAL_HEAD_SHA"
    ROTATION_EVIDENCE=$(safe_jq -cnS --arg old "$OLD_GATE_SHA" --arg new "$NEW_GATE_SHA" --arg dispatch "$DISPATCH_SHA" --arg head "$FINAL_HEAD_SHA" --arg id "$LOCK_RUN_ID" --arg attempt "$LOCK_RUN_ATTEMPT" --arg operator "$OPERATOR_LOGIN" --arg actor "$(safe_jq -er '.actor.login' "$LOCK_DETAIL")" --arg result "$ROTATION_RESULT" '{"dispatch-sha":$dispatch,"final-head-sha":$head,"lock-run-actor":$actor,"lock-run-attempt":$attempt,"lock-run-id":$id,"lock-run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$id),"new-gate-sha":$new,"old-gate-sha":$old,"operator-login":$operator,result:$result}')
    EVIDENCE=$(build_policy_evidence rotation-review "$FINAL_GATE" null null null "$ROTATION_EVIDENCE")
    ;;
  rotation-complete)
    verify_common_policy "$GATE_SHA" ''
    verify_rotation_complete
    ROTATION_EVIDENCE=$(safe_jq -cnS --arg head "$RECEIPT_HEAD" --arg id "$LOCK_RUN_ID" --arg attempt "$LOCK_RUN_ATTEMPT" --arg number "$SELECTED_RUN_NUMBER" --arg history "$ROTATION_HISTORY_DIGEST" --arg evidence "$RECEIPT_EVIDENCE_DIGEST" --arg created "$ROTATION_CREATED_AT" --arg result "$RECEIPT_RESULT" '{"completed-lock":{"created-at":$created,"evidence-sha256":$evidence,"history-sha256":$history,"run-attempt":$attempt,"run-id":$id,"run-number":$number,"run-url":("https://github.com/stackpop/edgezero/actions/runs/"+$id)},"final-head-sha":$head,result:$result}')
    EVIDENCE=$(build_policy_evidence rotation-complete "$GATE_SHA" null null null "$ROTATION_EVIDENCE")
    ;;
esac

EVIDENCE_SIZE=${#EVIDENCE}
((EVIDENCE_SIZE > 0 && EVIDENCE_SIZE <= MAX_EVIDENCE_BYTES)) || die 'canonical evidence size is outside its allowed bounds'
safe_jq -e 'type == "object"' <<<"$EVIDENCE" >/dev/null 2>&1 || die 'canonical evidence is not a JSON object'
EVIDENCE_DIGEST="sha256:$(hash_bytes "$EVIDENCE")"

if [[ "$MODE" == release ]]; then
  PREVIOUS_DIGEST="sha256:$(hash_bytes "$CURRENT_PREREQUISITE")"
  PREREQUISITE=$(safe_jq -cnS --arg evidence "$EVIDENCE_DIGEST" --arg url "$EVIDENCE_URL" --arg gate "$GATE_SHA" --arg previous "$PREVIOUS_DIGEST" --argjson rotation "$CURRENT_ROTATION_HISTORY" --arg pr "$CANDIDATE_PR" --arg source "$SOURCE_REVISION" '{"evidence-sha256":$evidence,"evidence-url":$url,"gate-sha":$gate,"previous-value-sha256":$previous,"rotation-history":$rotation,"schema-version":2,"source-pr":$pr,"source-revision":$source}')
elif [[ "$MODE" == rotation-complete ]]; then
  PREVIOUS_DIGEST="sha256:$(hash_bytes "$CURRENT_PREREQUISITE")"
  VERIFIED_HISTORY=$(safe_jq -cnS --arg created "$ROTATION_CREATED_AT" --arg evidence "$RECEIPT_EVIDENCE_DIGEST" --arg history "$ROTATION_HISTORY_DIGEST" --arg attempt "$LOCK_RUN_ATTEMPT" --arg id "$LOCK_RUN_ID" --arg number "$SELECTED_RUN_NUMBER" '{"created-at":$created,"evidence-sha256":$evidence,"history-sha256":$history,"run-attempt":$attempt,"run-id":$id,"run-number":$number,state:"verified"}')
  PREREQUISITE=$(safe_jq -cnS --arg evidence "$EVIDENCE_DIGEST" --arg gate "$GATE_SHA" --arg previous "$PREVIOUS_DIGEST" --argjson rotation "$VERIFIED_HISTORY" '{"evidence-sha256":$evidence,"evidence-url":null,"gate-sha":$gate,"previous-value-sha256":$previous,"rotation-history":$rotation,"schema-version":2,"source-pr":null,"source-revision":null}')
elif [[ "$MODE" == rotation-review ]]; then
  POLICY_JSON=$(safe_jq -cnS --arg audited "$AUDITED_AT" --arg dispatch "$DISPATCH_SHA" --arg gate "$FINAL_GATE" --arg head "$FINAL_HEAD_SHA" --arg attempt "$LOCK_RUN_ATTEMPT" --arg id "$LOCK_RUN_ID" --arg digest "$EVIDENCE_DIGEST" '{"audited-at":$audited,"dispatch-sha":$dispatch,"gate-sha":$gate,"head-sha":$head,"lock-run-attempt":$attempt,"lock-run-id":$id,"policy-sha256":$digest,"release-state":"enabled","required-workflow-sha":$gate}')
  ROTATION_JSON=$(safe_jq -cnS --arg evidence "sha256:$(hash_bytes "$POLICY_JSON")" --arg head "$FINAL_HEAD_SHA" --arg id "$LOCK_RUN_ID" --arg new "$NEW_GATE_SHA" --arg old "$OLD_GATE_SHA" --arg result "$ROTATION_RESULT" --arg reviewed "$AUDITED_AT" '{"evidence-sha256":$evidence,"head-sha":$head,"lock-run-id":$id,"new-gate-sha":$new,"old-gate-sha":$old,result:$result,"reviewed-at":$reviewed}')
  APPROVAL_COMMENT="$ROTATION_PREFIX$ROTATION_JSON"$'\n'"$POLICY_PREFIX$POLICY_JSON"
fi

publish_file "$EVIDENCE_OUT" "$EVIDENCE" evidence
if [[ "$MODE" == release || "$MODE" == rotation-complete ]]; then
  if ! publish_file "$PREREQUISITE_OUT" "$PREREQUISITE" prerequisite; then rm -f -- "$EVIDENCE_OUT"; exit 1; fi
elif [[ "$MODE" == rotation-review ]]; then
  if ! publish_file "$APPROVAL_COMMENT_OUT" "$APPROVAL_COMMENT" approval-comment; then rm -f -- "$EVIDENCE_OUT"; exit 1; fi
fi
